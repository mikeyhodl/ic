use super::*;
use crate::{
    governance::{Governance, MockEnvironment},
    pb::v1::{
        neuron::{DissolveState, Followees},
        Governance as GovernanceProto, KnownNeuronData,
    },
    storage::{reset_stable_memory, Signed32},
};
use assert_matches::assert_matches;
use ic_nervous_system_common::{cmc::MockCMC, ledger::MockIcpLedger};
use ic_nervous_system_governance::index::{
    neuron_following::NeuronFollowingIndex, neuron_principal::NeuronPrincipalIndex,
};
use ic_nns_common::pb::v1::NeuronId;
use lazy_static::lazy_static;
use maplit::{btreemap, hashmap, hashset};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn add_remove_neuron() {
    // Step 1: prepare indexes and neurons.
    let mut indexes = new_heap_based();
    let neuron = Neuron {
        id: Some(NeuronId { id: 1 }),
        account: [1u8; 32].to_vec(),
        controller: Some(PrincipalId::new_user_test_id(1)),
        hot_keys: vec![
            PrincipalId::new_user_test_id(2),
            PrincipalId::new_user_test_id(3),
        ],
        followees: hashmap! {
            1 => Followees{
                followees: vec![
                    NeuronId { id: 2 },
                    NeuronId { id: 3 },
                    NeuronId { id: 4 },
                ],
            },
        },
        known_neuron_data: Some(KnownNeuronData {
            name: "known neuron data".to_string(),
            description: None,
        }),
        ..Default::default()
    };
    let account_id = AccountIdentifier::new(GOVERNANCE_CANISTER_ID.get(), neuron.subaccount().ok());

    // Step 2: reading indexes return empty before adding neuron to them.
    assert_eq!(
        indexes
            .subaccount()
            .get_neuron_id_by_subaccount(&neuron.subaccount().unwrap()),
        None
    );
    assert_eq!(
        indexes
            .principal()
            .get_neuron_ids(PrincipalId::new_user_test_id(1)),
        hashset! {}
    );
    assert_eq!(
        indexes
            .following()
            .get_followers_by_followee_and_category(&2, Signed32::from(1)),
        Vec::<u64>::default()
    );
    assert_eq!(indexes.known_neuron().list_known_neuron_ids(), vec![]);
    assert_eq!(
        indexes
            .account_id()
            .get_neuron_id_by_account_id(&account_id),
        None
    );

    // Step 3: adding a neuron.
    assert_eq!(indexes.add_neuron(&neuron), Ok(()));

    // Step 4: verify that reading indexes now return the added neuron.
    assert_eq!(
        indexes
            .subaccount()
            .get_neuron_id_by_subaccount(&neuron.subaccount().unwrap()),
        Some(NeuronId { id: 1 })
    );
    for principal_num in 1..=3 {
        assert_eq!(
            indexes
                .principal()
                .get_neuron_ids(PrincipalId::new_user_test_id(principal_num)),
            hashset! { 1 }
        );
    }
    for followee_id in 2..=4 {
        assert_eq!(
            indexes
                .following()
                .get_followers_by_followee_and_category(&followee_id, Signed32::from(1)),
            vec![1]
        );
    }
    assert_eq!(
        indexes.known_neuron().list_known_neuron_ids(),
        vec![NeuronId { id: 1 }]
    );
    assert_eq!(
        indexes
            .account_id()
            .get_neuron_id_by_account_id(&account_id),
        Some(NeuronId { id: 1 })
    );

    // Step 5: remove the neuron.
    assert_eq!(indexes.remove_neuron(&neuron), Ok(()));

    // Step 6: verify that indexes are again empty.
    assert_eq!(
        indexes
            .subaccount()
            .get_neuron_id_by_subaccount(&neuron.subaccount().unwrap()),
        None
    );
    assert_eq!(
        indexes
            .principal()
            .get_neuron_ids(PrincipalId::new_user_test_id(1)),
        hashset! {}
    );
    assert_eq!(
        indexes
            .following()
            .get_followers_by_followee_and_category(&2, Signed32::from(1)),
        Vec::<u64>::default()
    );
    assert_eq!(indexes.known_neuron().list_known_neuron_ids(), vec![]);
    assert_eq!(
        indexes
            .account_id()
            .get_neuron_id_by_account_id(&account_id),
        None
    );
}

lazy_static! {
    // Use MODEL_NEURON for tests where the exact member values are not needed for understanding the
    // test.
    static ref MODEL_NEURON: Neuron = Neuron {
        id: Some(NeuronId { id: 1 }),
        account: [1u8; 32].to_vec(),
        controller: Some(PrincipalId::new_user_test_id(1)),
        hot_keys: vec![
            PrincipalId::new_user_test_id(2),
            PrincipalId::new_user_test_id(3),
        ],
        followees: hashmap! {
            1 => Followees{
                followees: vec![
                    NeuronId { id: 2 },
                    NeuronId { id: 3 },
                    NeuronId { id: 4 },
                ],
            },
        },
        known_neuron_data: Some(KnownNeuronData {
            name: "known neuron data".to_string(),
            description: None,
        }),

        ..Default::default()
    };
}

#[test]
fn update_neuron_id_fails() {
    let mut indexes = new_heap_based();
    let neuron = Neuron {
        id: Some(NeuronId { id: 1 }),
        ..MODEL_NEURON.clone()
    };
    let neuron_with_different_id = Neuron {
        id: Some(NeuronId { id: 2 }),
        ..MODEL_NEURON.clone()
    };

    assert_matches!(indexes.update_neuron(&neuron, &neuron_with_different_id),
        Err(NeuronStoreError::NeuronIdModified { old_neuron_id, new_neuron_id })
        if old_neuron_id.id == 1 && new_neuron_id.id == 2);
}

#[test]
fn update_neuron_account_fails() {
    let mut indexes = new_heap_based();
    let neuron = Neuron {
        account: [1u8; 32].to_vec(),
        ..MODEL_NEURON.clone()
    };
    let neuron_with_different_account = Neuron {
        account: [2u8; 32].to_vec(),
        ..MODEL_NEURON.clone()
    };

    assert_matches!(
        indexes.update_neuron(&neuron, &neuron_with_different_account), 
        Err(NeuronStoreError::SubaccountModified { old_subaccount, new_subaccount })
        if old_subaccount.0 == [1u8; 32] && new_subaccount.0 == [2u8; 32]);
}

#[test]
fn update_neuron_replace_controller() {
    let mut indexes = new_heap_based();
    let old_neuron = Neuron {
        controller: Some(PrincipalId::new_user_test_id(1)),
        hot_keys: vec![],
        ..MODEL_NEURON.clone()
    };
    assert_eq!(indexes.add_neuron(&old_neuron), Ok(()));
    let new_neuron = Neuron {
        controller: Some(PrincipalId::new_user_test_id(2)),
        hot_keys: vec![],
        ..MODEL_NEURON.clone()
    };
    let neuron_id = MODEL_NEURON.id.unwrap().id;
    // Before updating, the neuron can be looked up by the old controller but cannot be by the new
    // one.
    assert_eq!(
        indexes
            .principal()
            .get_neuron_ids(old_neuron.controller.unwrap()),
        hashset! {neuron_id}
    );
    assert_eq!(
        indexes
            .principal()
            .get_neuron_ids(new_neuron.controller.unwrap()),
        hashset! {}
    );

    assert_eq!(indexes.update_neuron(&old_neuron, &new_neuron), Ok(()));

    // After updating the neuron cannot be looked up by the old controller, but the can be by the
    // new one.
    assert_eq!(
        indexes
            .principal()
            .get_neuron_ids(old_neuron.controller.unwrap()),
        hashset! {}
    );
    assert_eq!(
        indexes
            .principal()
            .get_neuron_ids(new_neuron.controller.unwrap()),
        hashset! {neuron_id}
    );
}

#[test]
fn update_neuron_add_hot_key() {
    let mut indexes = new_heap_based();
    let old_neuron = Neuron {
        controller: Some(PrincipalId::new_user_test_id(100)),
        hot_keys: vec![
            PrincipalId::new_user_test_id(101),
            PrincipalId::new_user_test_id(102),
        ],
        ..MODEL_NEURON.clone()
    };
    assert_eq!(indexes.add_neuron(&old_neuron), Ok(()));
    let new_neuron = Neuron {
        controller: Some(PrincipalId::new_user_test_id(100)),
        hot_keys: vec![
            PrincipalId::new_user_test_id(101),
            PrincipalId::new_user_test_id(102),
            PrincipalId::new_user_test_id(103),
        ],
        ..MODEL_NEURON.clone()
    };
    let neuron_id = MODEL_NEURON.id.unwrap().id;
    // Before updating, the neuron can be looked up by 101, 102 but not 103
    for i in 101..=102 {
        assert_eq!(
            indexes
                .principal()
                .get_neuron_ids(PrincipalId::new_user_test_id(i)),
            hashset! {neuron_id}
        );
    }
    assert_eq!(
        indexes
            .principal()
            .get_neuron_ids(PrincipalId::new_user_test_id(103)),
        hashset! {}
    );

    assert_eq!(indexes.update_neuron(&old_neuron, &new_neuron), Ok(()));

    // After updating the neuron cannot be looked up by 101..=103
    for i in 101..=103 {
        assert_eq!(
            indexes
                .principal()
                .get_neuron_ids(PrincipalId::new_user_test_id(i)),
            hashset! {neuron_id}
        );
    }
}

#[test]
fn update_neuron_remove_hot_key() {
    let mut indexes = new_heap_based();
    let old_neuron = Neuron {
        controller: Some(PrincipalId::new_user_test_id(100)),
        hot_keys: vec![
            PrincipalId::new_user_test_id(101),
            PrincipalId::new_user_test_id(102),
        ],
        ..MODEL_NEURON.clone()
    };
    assert_eq!(indexes.add_neuron(&old_neuron), Ok(()));
    let new_neuron = Neuron {
        controller: Some(PrincipalId::new_user_test_id(100)),
        hot_keys: vec![PrincipalId::new_user_test_id(102)],
        ..MODEL_NEURON.clone()
    };
    let neuron_id = MODEL_NEURON.id.unwrap().id;
    // Before updating, the neuron can be looked up by 101, 102
    for i in 101..=102 {
        assert_eq!(
            indexes
                .principal()
                .get_neuron_ids(PrincipalId::new_user_test_id(i)),
            hashset! {neuron_id}
        );
    }

    assert_eq!(indexes.update_neuron(&old_neuron, &new_neuron), Ok(()));

    // After updating the neuron only be looked up by 102
    assert_eq!(
        indexes
            .principal()
            .get_neuron_ids(PrincipalId::new_user_test_id(102)),
        hashset! {neuron_id}
    );
    assert_eq!(
        indexes
            .principal()
            .get_neuron_ids(PrincipalId::new_user_test_id(101)),
        hashset! {}
    );
}

#[test]
fn update_neuron_remove_controller_as_hot_key() {
    // Unfortunately, we currently allow controller to be added as hot key, we need to make sure
    // that when we remove the hot key (that is the same as the controller), the neuron can still be
    // looked up by the controller through the index.
    let mut indexes = new_heap_based();
    let old_neuron = Neuron {
        controller: Some(PrincipalId::new_user_test_id(100)),
        hot_keys: vec![
            PrincipalId::new_user_test_id(100),
            PrincipalId::new_user_test_id(101),
        ],
        ..MODEL_NEURON.clone()
    };
    assert_eq!(indexes.add_neuron(&old_neuron), Ok(()));
    let new_neuron = Neuron {
        controller: Some(PrincipalId::new_user_test_id(100)),
        hot_keys: vec![PrincipalId::new_user_test_id(101)],
        ..MODEL_NEURON.clone()
    };
    let neuron_id = MODEL_NEURON.id.unwrap().id;
    // Before updating, the neuron can be looked up by 100, 101
    for i in 100..=101 {
        assert_eq!(
            indexes
                .principal()
                .get_neuron_ids(PrincipalId::new_user_test_id(i)),
            hashset! {neuron_id}
        );
    }

    assert_eq!(indexes.update_neuron(&old_neuron, &new_neuron), Ok(()));

    // After updating can still be looked up by both
    for i in 100..=101 {
        assert_eq!(
            indexes
                .principal()
                .get_neuron_ids(PrincipalId::new_user_test_id(i)),
            hashset! {neuron_id}
        );
    }
}

#[test]
fn update_neuron_set_followees() {
    let mut indexes = new_heap_based();
    let old_neuron = Neuron {
        followees: hashmap! {
            1 => Followees{
                followees: vec![
                    NeuronId { id: 2 },
                    NeuronId { id: 3 },
                    NeuronId { id: 4 },
                ],
            },
            2 => Followees{
                followees: vec![
                    NeuronId { id: 5 },
                    NeuronId { id: 6 },
                ],
            },
        },
        ..MODEL_NEURON.clone()
    };
    assert_eq!(indexes.add_neuron(&old_neuron), Ok(()));
    let new_neuron = Neuron {
        followees: hashmap! {
            1 => Followees{
                followees: vec![
                    NeuronId { id: 2 },
                    NeuronId { id: 3 },
                    NeuronId { id: 4 },
                ],
            },
            2 => Followees{
                followees: vec![
                    NeuronId { id: 7 },
                    NeuronId { id: 8 },
                ],
            },
        },
        ..MODEL_NEURON.clone()
    };
    let neuron_id = MODEL_NEURON.id.unwrap().id;
    // Before updating, the neuron can be looked up by 2, 3, 4 for topic 1.
    for i in 2..=4 {
        assert_eq!(
            indexes
                .following()
                .get_followers_by_followee_and_category(&i, Signed32::from(1)),
            vec![neuron_id],
        );
    }
    // Before updating, the neuron can be looked up by 5, 6 for topic 2.
    for i in 5..=6 {
        assert_eq!(
            indexes
                .following()
                .get_followers_by_followee_and_category(&i, Signed32::from(2)),
            vec![neuron_id],
        );
    }

    assert_eq!(indexes.update_neuron(&old_neuron, &new_neuron), Ok(()));

    // After updating, the neuron can still be looked up by 2, 3, 4 for topic 1.
    for i in 2..=4 {
        assert_eq!(
            indexes
                .following()
                .get_followers_by_followee_and_category(&i, Signed32::from(1)),
            vec![neuron_id],
        );
    }
    // After updating, the neuron can no longer be looked up by 5, 6 for topic 2.
    for i in 5..=6 {
        assert_eq!(
            indexes
                .following()
                .get_followers_by_followee_and_category(&i, Signed32::from(2)),
            Vec::<u64>::new(),
        );
    }
    // After updating, the neuron can be looked up by 7, 8 for topic 2.
    for i in 7..=8 {
        assert_eq!(
            indexes
                .following()
                .get_followers_by_followee_and_category(&i, Signed32::from(2)),
            vec![neuron_id],
        );
    }
}

#[test]
fn update_neuron_add_known_neuron() {
    let mut indexes = new_heap_based();
    let old_neuron = Neuron {
        known_neuron_data: None,
        ..MODEL_NEURON.clone()
    };
    assert_eq!(indexes.add_neuron(&old_neuron), Ok(()));
    let new_neuron = Neuron {
        known_neuron_data: Some(KnownNeuronData {
            name: "known neuron data".to_string(),
            description: None,
        }),
        ..MODEL_NEURON.clone()
    };
    let neuron_id = MODEL_NEURON.id.unwrap();
    // No known neurons before update.
    assert_eq!(indexes.known_neuron().list_known_neuron_ids(), vec![]);

    assert_eq!(indexes.update_neuron(&old_neuron, &new_neuron), Ok(()));

    // Known neuron can be looked up after update.
    assert_eq!(
        indexes.known_neuron().list_known_neuron_ids(),
        vec![neuron_id]
    );
}

#[test]
fn update_neuron_remove_known_neuron() {
    let mut indexes = new_heap_based();
    let old_neuron = Neuron {
        known_neuron_data: Some(KnownNeuronData {
            name: "known neuron data".to_string(),
            description: None,
        }),
        ..MODEL_NEURON.clone()
    };
    assert_eq!(indexes.add_neuron(&old_neuron), Ok(()));
    let new_neuron = Neuron {
        known_neuron_data: None,
        ..MODEL_NEURON.clone()
    };
    let neuron_id = MODEL_NEURON.id.unwrap();
    // Known neuron can be looked up before update.
    assert_eq!(
        indexes.known_neuron().list_known_neuron_ids(),
        vec![neuron_id]
    );

    assert_eq!(indexes.update_neuron(&old_neuron, &new_neuron), Ok(()));

    // Known neuron can no longer be looked up after update.
    assert_eq!(indexes.known_neuron().list_known_neuron_ids(), vec![]);
}

#[test]
fn update_neuron_update_known_neuron_name() {
    let mut indexes = new_heap_based();
    let old_neuron = Neuron {
        known_neuron_data: Some(KnownNeuronData {
            name: "known neuron data".to_string(),
            description: None,
        }),
        ..MODEL_NEURON.clone()
    };
    assert_eq!(indexes.add_neuron(&old_neuron), Ok(()));
    let new_neuron = Neuron {
        known_neuron_data: Some(KnownNeuronData {
            name: "different known neuron data".to_string(),
            description: None,
        }),
        ..MODEL_NEURON.clone()
    };
    let neuron_id = MODEL_NEURON.id.unwrap();
    assert!(indexes
        .known_neuron()
        .contains_known_neuron_name("known neuron data"),);

    assert_eq!(indexes.update_neuron(&old_neuron, &new_neuron), Ok(()));

    assert!(!indexes
        .known_neuron()
        .contains_known_neuron_name("known neuron data"),);
    assert!(indexes
        .known_neuron()
        .contains_known_neuron_name("different known neuron data"),);
}

fn create_mock_environment(now_timestamp_seconds: Option<u64>) -> MockEnvironment {
    let mut environment = MockEnvironment::new();
    let now_timestamp_seconds = now_timestamp_seconds.unwrap_or(now_seconds());

    environment.expect_now().return_const(now_timestamp_seconds);
    environment
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn simple_neuron(id: u64) -> Neuron {
    // Make sure different neurons have different accounts.
    let mut account = vec![0; 32];
    for (destination, data) in account.iter_mut().zip(id.to_le_bytes().iter().cycle()) {
        *destination = *data;
    }

    Neuron {
        id: Some(NeuronId { id }),
        account,
        controller: Some(PrincipalId::new_user_test_id(id)),
        ..Default::default()
    }
}

// TODO(NNS1-2784) - Remove test after 1-time upgrade
/// Test that restoring the Governance canister after an upgrade with an empty NeuronAccountIdIndex will
/// trigger the index to be built with existing neurons. To do this, mainnet like state must be simulated.
/// Initializing or adding a neuron to Governance's data layer will automatically add a neuron to the index,
/// so in this test, a special test only API to reset the index will be called to simulate mainnet like state.
#[test]
fn test_neuron_store_populates_index_on_restore() {
    // Step 1: set up 1 active neuron and 1 inactive neuron.
    let active_neuron = Neuron {
        cached_neuron_stake_e8s: 1,
        ..simple_neuron(1)
    };
    let inactive_neuron = Neuron {
        cached_neuron_stake_e8s: 0,
        dissolve_state: Some(DissolveState::WhenDissolvedTimestampSeconds(1)),
        ..simple_neuron(2)
    };
    let mut gov = Governance::new(
        GovernanceProto::default(),
        Box::new(create_mock_environment(None)),
        Box::<MockIcpLedger>::default(),
        Box::<MockCMC>::default(),
    );

    // Step 2: Add neurons to governance and simulate mainnet like state
    gov.neuron_store.add_neuron(active_neuron);
    gov.neuron_store.add_neuron(inactive_neuron);

    // The stable indexes should have been updated to include the neurons
    assert_eq!(gov.neuron_store.stable_indexes_lens().account_id, 2);
    assert_eq!(gov.neuron_store.len(), 2);

    // Simulate mainnet like state to have an empty index with existing neurons.
    gov.neuron_store.reset_account_id_index();
    assert_eq!(gov.neuron_store.stable_indexes_lens().account_id, 0);
    assert_eq!(gov.neuron_store.len(), 2);

    // Step 3: Test that restoring the governance canister will trigger the stable index to be
    // rebuilt with neurons.
    let mut heap_proto = gov.take_heap_proto();
    let gov = Governance::new_restored(
        heap_proto,
        Box::new(create_mock_environment(None)),
        Box::<MockIcpLedger>::default(),
        Box::<MockCMC>::default(),
    );

    assert_eq!(gov.neuron_store.stable_indexes_lens().account_id, 2);
    assert_eq!(gov.neuron_store.len(), 2);
}
