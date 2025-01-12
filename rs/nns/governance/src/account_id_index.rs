use crate::pb::v1::{governance_error::ErrorType, GovernanceError};
use ic_nns_common::pb::v1::NeuronId;
use ic_stable_structures::{Memory, StableBTreeMap};
use icp_ledger::AccountIdentifier;

/// An Index mapping an AccountIdentifier on the ICP Ledger to a NeuronId. The
/// This AccountIdentifier is the ICP Ledger Account that "backs" a Neuron's
/// stake.  
pub struct NeuronAccountIdIndex<M: Memory> {
    account_id_to_id: StableBTreeMap<[u8; 28], u64, M>,
}

impl<M: Memory> NeuronAccountIdIndex<M> {
    pub fn new(memory: M) -> Self {
        Self {
            account_id_to_id: StableBTreeMap::init(memory),
        }
    }

    pub fn num_entries(&self) -> usize {
        self.account_id_to_id.len() as usize
    }

    pub fn add_neuron_account_id(
        &mut self,
        neuron_id: NeuronId,
        account_id: AccountIdentifier,
    ) -> Result<(), GovernanceError> {
        let previous_neuron_id = self.account_id_to_id.insert(account_id.hash, neuron_id.id);
        match previous_neuron_id {
            None => Ok(()),
            Some(previous_neuron_id) => {
                self.account_id_to_id
                    .insert(account_id.hash, previous_neuron_id);
                Err(GovernanceError::new_with_message(
                    ErrorType::PreconditionFailed,
                    format!(
                        "AccountIdentifier {:?} already exists in the index",
                        account_id
                    ),
                ))
            }
        }
    }

    pub fn remove_neuron_account_id(
        &mut self,
        neuron_id: NeuronId,
        account_identifier: AccountIdentifier,
    ) -> Result<(), GovernanceError> {
        let previous_neuron_id = self.account_id_to_id.remove(&account_identifier.hash);

        match previous_neuron_id {
            Some(previous_neuron_id) => {
                if previous_neuron_id == neuron_id.id {
                    Ok(())
                } else {
                    self.account_id_to_id
                        .insert(account_identifier.hash, previous_neuron_id);
                    Err(GovernanceError::new_with_message(
                        ErrorType::PreconditionFailed,
                        format!(
                            "AccountIdentifier ({}) exists in the index with a different neuron id {}",
                            account_identifier, previous_neuron_id
                        )
                    ))
                }
            }
            None => Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                format!(
                    "AccountIdentifier ({}) already absent in the index",
                    account_identifier
                ),
            )),
        }
    }

    /// Finds the neuron id by subaccount if it exists.
    pub fn get_neuron_id_by_account_id(&self, account_id: &AccountIdentifier) -> Option<NeuronId> {
        self.account_id_to_id
            .get(&account_id.hash)
            .map(|id| NeuronId { id })
    }

    /// This method is used in testing to reset the AccountId index to properly test the upgrade path.
    /// The `.clear()` method does not work given a mutable reference, so instead, iterate the map,
    /// collect all keys, and remove all keys.
    // TODO(NNS1-2784) - Remove test after 1-time upgrade
    #[cfg(not(target_arch = "wasm32"))]
    pub fn reset(&mut self) {
        self.account_id_to_id
            .range(..)
            .collect::<Vec<_>>()
            .iter()
            .for_each(|(key, _)| {
                self.account_id_to_id.remove(key);
            });
    }
}

#[cfg(test)]
mod tests;
