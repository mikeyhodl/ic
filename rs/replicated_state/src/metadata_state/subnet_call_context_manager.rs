use ic_btc_replica_types::{GetSuccessorsRequestInitial, SendTransactionRequest};
use ic_logger::{info, ReplicaLogger};
use ic_management_canister_types_private::{
    EcdsaKeyId, MasterPublicKeyId, SchnorrKeyId, VetKdKeyId,
};
use ic_protobuf::{
    proxy::{try_from_option_field, ProxyDecodeError},
    registry::subnet::v1 as pb_subnet,
    state::queues::v1 as pb_queues,
    state::system_metadata::v1 as pb_metadata,
    types::v1 as pb_types,
};
use ic_types::{
    canister_http::CanisterHttpRequestContext,
    consensus::idkg::{common::PreSignature, PreSigId},
    crypto::{
        canister_threshold_sig::{
            idkg::IDkgTranscript, EcdsaPreSignatureQuadruple, SchnorrPreSignatureTranscript,
        },
        threshold_sig::ni_dkg::{id::ni_dkg_target_id, NiDkgId, NiDkgTargetId},
    },
    messages::{CallbackId, CanisterCall, Request, StopCanisterCallId},
    node_id_into_protobuf, node_id_try_from_option, CanisterId, ExecutionRound, Height, NodeId,
    RegistryVersion, Time,
};
use phantom_newtype::Id;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    convert::{From, TryFrom},
    sync::Arc,
};

/// ECDSA message hash size in bytes.
const MESSAGE_HASH_SIZE: usize = 32;

/// Threshold algorithm pseudo-random ID size in bytes.
const PSEUDO_RANDOM_ID_SIZE: usize = 32;

/// Threshold algorithm nonce size in bytes.
const NONCE_SIZE: usize = 32;

pub enum SubnetCallContext {
    SetupInitialDKG(SetupInitialDkgContext),
    CanisterHttpRequest(CanisterHttpRequestContext),
    ReshareChainKey(ReshareChainKeyContext),
    BitcoinGetSuccessors(BitcoinGetSuccessorsContext),
    BitcoinSendTransactionInternal(BitcoinSendTransactionInternalContext),
    SignWithThreshold(SignWithThresholdContext),
}

impl SubnetCallContext {
    pub fn get_request(&self) -> &Request {
        match &self {
            SubnetCallContext::SetupInitialDKG(context) => &context.request,
            SubnetCallContext::CanisterHttpRequest(context) => &context.request,
            SubnetCallContext::ReshareChainKey(context) => &context.request,
            SubnetCallContext::BitcoinGetSuccessors(context) => &context.request,
            SubnetCallContext::BitcoinSendTransactionInternal(context) => &context.request,
            SubnetCallContext::SignWithThreshold(context) => &context.request,
        }
    }

    pub fn get_time(&self) -> Time {
        match &self {
            SubnetCallContext::SetupInitialDKG(context) => context.time,
            SubnetCallContext::CanisterHttpRequest(context) => context.time,
            SubnetCallContext::ReshareChainKey(context) => context.time,
            SubnetCallContext::BitcoinGetSuccessors(context) => context.time,
            SubnetCallContext::BitcoinSendTransactionInternal(context) => context.time,
            SubnetCallContext::SignWithThreshold(context) => context.batch_time,
        }
    }
}

pub struct InstallCodeCallIdTag;
pub type InstallCodeCallId = Id<InstallCodeCallIdTag, u64>;

/// Collection of install code call messages whose execution is paused at the
/// end of the round.
///
/// During a subnet split, these messages will be automatically rejected if
/// the targeted canister has moved to a new subnet.
#[derive(Clone, Eq, PartialEq, Debug, Default)]
struct InstallCodeCallManager {
    next_call_id: u64,
    install_code_calls: BTreeMap<InstallCodeCallId, InstallCodeCall>,
}

impl InstallCodeCallManager {
    fn push_call(&mut self, call: InstallCodeCall) -> InstallCodeCallId {
        let call_id = InstallCodeCallId::new(self.next_call_id);
        self.next_call_id += 1;
        self.install_code_calls.insert(call_id, call);

        call_id
    }

    fn remove_call(&mut self, call_id: InstallCodeCallId) -> Option<InstallCodeCall> {
        self.install_code_calls.remove(&call_id)
    }

    /// Removes and returns all `InstallCodeCalls` not targeted to local canisters.
    ///
    /// Used for rejecting all calls targeting migrated canisters after a subnet
    /// split.
    fn remove_non_local_calls(
        &mut self,
        is_local_canister: impl Fn(CanisterId) -> bool,
    ) -> Vec<InstallCodeCall> {
        let mut removed = Vec::new();
        self.install_code_calls.retain(|_call_id, call| {
            if is_local_canister(call.effective_canister_id) {
                true
            } else {
                removed.push(call.clone());
                false
            }
        });
        removed
    }
}

/// Collection of stop canister messages whose execution is paused at the
/// end of the round.
///
/// During a subnet split, these messages will be automatically rejected if
/// the target canister has moved to a new subnet.
#[derive(Clone, Eq, PartialEq, Debug, Default)]
struct StopCanisterCallManager {
    next_call_id: u64,
    stop_canister_calls: BTreeMap<StopCanisterCallId, StopCanisterCall>,
}

impl StopCanisterCallManager {
    fn push_call(&mut self, call: StopCanisterCall) -> StopCanisterCallId {
        let call_id = StopCanisterCallId::new(self.next_call_id);
        self.next_call_id += 1;
        self.stop_canister_calls.insert(call_id, call);

        call_id
    }

    fn remove_call(&mut self, call_id: StopCanisterCallId) -> Option<StopCanisterCall> {
        self.stop_canister_calls.remove(&call_id)
    }

    fn get_time(&self, call_id: &StopCanisterCallId) -> Option<Time> {
        self.stop_canister_calls.get(call_id).map(|x| x.time)
    }

    /// Removes and returns all `StopCanisterCalls` not targeted to local canisters.
    ///
    /// Used for rejecting all calls targeting migrated canisters after a subnet
    /// split.
    fn remove_non_local_calls(
        &mut self,
        is_local_canister: impl Fn(CanisterId) -> bool,
    ) -> Vec<StopCanisterCall> {
        let mut removed = Vec::new();
        self.stop_canister_calls.retain(|_call_id, call| {
            if is_local_canister(call.effective_canister_id) {
                true
            } else {
                removed.push(call.clone());
                false
            }
        });
        removed
    }
}

/// It is responsible for keeping track of all subnet messages that
/// do not require work to be done by another IC layer and
/// cannot finalize the execution in a single round.
#[derive(Clone, Eq, PartialEq, Debug, Default)]
struct CanisterManagementCalls {
    install_code_call_manager: InstallCodeCallManager,
    stop_canister_call_manager: StopCanisterCallManager,
}

impl CanisterManagementCalls {
    fn push_install_code_call(&mut self, call: InstallCodeCall) -> InstallCodeCallId {
        self.install_code_call_manager.push_call(call)
    }

    fn push_stop_canister_call(&mut self, call: StopCanisterCall) -> StopCanisterCallId {
        self.stop_canister_call_manager.push_call(call)
    }

    fn remove_install_code_call(&mut self, call_id: InstallCodeCallId) -> Option<InstallCodeCall> {
        self.install_code_call_manager.remove_call(call_id)
    }

    fn remove_stop_canister_call(
        &mut self,
        call_id: StopCanisterCallId,
    ) -> Option<StopCanisterCall> {
        self.stop_canister_call_manager.remove_call(call_id)
    }

    fn get_time_for_stop_canister_call(&self, call_id: &StopCanisterCallId) -> Option<Time> {
        self.stop_canister_call_manager.get_time(call_id)
    }

    pub fn install_code_calls_len(&self) -> usize {
        self.install_code_call_manager.install_code_calls.len()
    }

    pub fn stop_canister_calls_len(&self) -> usize {
        self.stop_canister_call_manager.stop_canister_calls.len()
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct PreSignatureStash {
    pub key_transcript: Arc<IDkgTranscript>,
    pub pre_signatures: BTreeMap<PreSigId, PreSignature>,
}

#[derive(Clone, Eq, PartialEq, Debug, Default)]
pub struct SubnetCallContextManager {
    /// Should increase monotonically. This property is used to determine if a request
    /// corresponds to a future state.
    next_callback_id: u64,
    pub setup_initial_dkg_contexts: BTreeMap<CallbackId, SetupInitialDkgContext>,
    pub sign_with_threshold_contexts: BTreeMap<CallbackId, SignWithThresholdContext>,
    pub canister_http_request_contexts: BTreeMap<CallbackId, CanisterHttpRequestContext>,
    pub reshare_chain_key_contexts: BTreeMap<CallbackId, ReshareChainKeyContext>,
    pub bitcoin_get_successors_contexts: BTreeMap<CallbackId, BitcoinGetSuccessorsContext>,
    pub bitcoin_send_transaction_internal_contexts:
        BTreeMap<CallbackId, BitcoinSendTransactionInternalContext>,
    canister_management_calls: CanisterManagementCalls,
    pub raw_rand_contexts: VecDeque<RawRandContext>,
    pub pre_signature_stashes: BTreeMap<MasterPublicKeyId, PreSignatureStash>,
}

impl SubnetCallContextManager {
    pub fn next_callback_id(&self) -> CallbackId {
        CallbackId::from(self.next_callback_id)
    }

    pub fn push_context(&mut self, context: SubnetCallContext) -> CallbackId {
        let callback_id = CallbackId::new(self.next_callback_id);
        self.next_callback_id += 1;

        match context {
            SubnetCallContext::SetupInitialDKG(context) => {
                self.setup_initial_dkg_contexts.insert(callback_id, context);
            }
            SubnetCallContext::SignWithThreshold(context) => {
                self.sign_with_threshold_contexts
                    .insert(callback_id, context);
            }
            SubnetCallContext::CanisterHttpRequest(context) => {
                self.canister_http_request_contexts
                    .insert(callback_id, context);
            }
            SubnetCallContext::ReshareChainKey(context) => {
                self.reshare_chain_key_contexts.insert(callback_id, context);
            }
            SubnetCallContext::BitcoinGetSuccessors(context) => {
                self.bitcoin_get_successors_contexts
                    .insert(callback_id, context);
            }
            SubnetCallContext::BitcoinSendTransactionInternal(context) => {
                self.bitcoin_send_transaction_internal_contexts
                    .insert(callback_id, context);
            }
        };

        callback_id
    }

    pub fn retrieve_context(
        &mut self,
        callback_id: CallbackId,
        logger: &ReplicaLogger,
    ) -> Option<SubnetCallContext> {
        self.setup_initial_dkg_contexts
            .remove(&callback_id)
            .map(|context| {
                info!(
                    logger,
                    "Received the response for SetupInitialDKG request for target {:?}",
                    context.target_id
                );
                SubnetCallContext::SetupInitialDKG(context)
            })
            .or_else(|| {
                self.sign_with_threshold_contexts
                    .remove(&callback_id)
                    .map(|context| {
                        info!(
                            logger,
                            "Received the response for SignWithThreshold request with id {:?} from {:?}",
                            context.pseudo_random_id,
                            context.request.sender
                        );
                        SubnetCallContext::SignWithThreshold(context)
                    })
            })
            .or_else(|| {
                self.reshare_chain_key_contexts
                    .remove(&callback_id)
                    .map(|context| {
                        info!(
                            logger,
                            "Received the response for ReshareChainKey request with key_id {:?} and callback id {:?} from {:?}",
                            context.key_id,
                            context.request.sender_reply_callback,
                            context.request.sender
                        );
                        SubnetCallContext::ReshareChainKey(context)
                    })
            })
            .or_else(|| {
                self.canister_http_request_contexts
                    .remove(&callback_id)
                    .map(|context| {
                        info!(
                            logger,
                            "Received the response for HttpRequest with callback id {:?} from {:?}",
                            context.request.sender_reply_callback,
                            context.request.sender
                        );
                        SubnetCallContext::CanisterHttpRequest(context)
                    })
            })
            .or_else(|| {
                self.bitcoin_get_successors_contexts
                    .remove(&callback_id)
                    .map(|context| {
                        info!(
                            logger,
                            "Received the response for BitcoinGetSuccessors with callback id {:?} from {:?}",
                            context.request.sender_reply_callback,
                            context.request.sender
                        );
                        SubnetCallContext::BitcoinGetSuccessors(context)
                    })
            })
            .or_else(|| {
                self.bitcoin_send_transaction_internal_contexts
                    .remove(&callback_id)
                    .map(|context| {
                        info!(
                            logger,
                            "Received the response for BitcoinSendTransactionInternal with callback id {:?} from {:?}",
                            context.request.sender_reply_callback,
                            context.request.sender
                        );
                        SubnetCallContext::BitcoinSendTransactionInternal(context)
                    })
            })
    }

    pub fn push_install_code_call(&mut self, call: InstallCodeCall) -> InstallCodeCallId {
        self.canister_management_calls.push_install_code_call(call)
    }

    pub fn remove_install_code_call(
        &mut self,
        call_id: InstallCodeCallId,
    ) -> Option<InstallCodeCall> {
        self.canister_management_calls
            .remove_install_code_call(call_id)
    }

    pub fn remove_non_local_install_code_calls(
        &mut self,
        is_local_canister: impl Fn(CanisterId) -> bool,
    ) -> Vec<InstallCodeCall> {
        self.canister_management_calls
            .install_code_call_manager
            .remove_non_local_calls(is_local_canister)
    }

    pub fn install_code_calls_len(&self) -> usize {
        self.canister_management_calls.install_code_calls_len()
    }

    pub fn push_stop_canister_call(&mut self, call: StopCanisterCall) -> StopCanisterCallId {
        self.canister_management_calls.push_stop_canister_call(call)
    }

    pub fn remove_stop_canister_call(
        &mut self,
        call_id: StopCanisterCallId,
    ) -> Option<StopCanisterCall> {
        self.canister_management_calls
            .remove_stop_canister_call(call_id)
    }

    pub fn get_time_for_stop_canister_call(&self, call_id: &StopCanisterCallId) -> Option<Time> {
        self.canister_management_calls
            .get_time_for_stop_canister_call(call_id)
    }

    pub fn remove_non_local_stop_canister_calls(
        &mut self,
        is_local_canister: impl Fn(CanisterId) -> bool,
    ) -> Vec<StopCanisterCall> {
        self.canister_management_calls
            .stop_canister_call_manager
            .remove_non_local_calls(is_local_canister)
    }

    pub fn stop_canister_calls_len(&self) -> usize {
        self.canister_management_calls.stop_canister_calls_len()
    }

    pub fn push_raw_rand_request(
        &mut self,
        request: Request,
        execution_round_id: ExecutionRound,
        time: Time,
    ) {
        self.raw_rand_contexts.push_back(RawRandContext {
            request,
            execution_round_id,
            time,
        });
    }

    pub fn remove_non_local_raw_rand_calls(
        &mut self,
        is_local_canister: impl Fn(CanisterId) -> bool,
    ) -> Vec<RawRandContext> {
        let mut removed = Vec::new();
        self.raw_rand_contexts.retain(|context| {
            if is_local_canister(context.request.sender()) {
                true
            } else {
                removed.push(context.clone());
                false
            }
        });
        removed
    }

    /// Returns the number of `sign_with_threshold_contexts` per key id.
    pub fn sign_with_threshold_contexts_count(&self, key_id: &MasterPublicKeyId) -> usize {
        self.sign_with_threshold_contexts
            .iter()
            .filter(|(_, context)| match (key_id, &context.args) {
                (MasterPublicKeyId::Ecdsa(ecdsa_key_id), ThresholdArguments::Ecdsa(args)) => {
                    args.key_id == *ecdsa_key_id
                }
                (MasterPublicKeyId::Schnorr(schnorr_key_id), ThresholdArguments::Schnorr(args)) => {
                    args.key_id == *schnorr_key_id
                }
                (MasterPublicKeyId::VetKd(vetkd_key_id), ThresholdArguments::VetKd(args)) => {
                    args.key_id == *vetkd_key_id
                }
                _ => false,
            })
            .count()
    }

    pub fn sign_with_ecdsa_contexts(&self) -> BTreeMap<CallbackId, SignWithThresholdContext> {
        self.sign_with_threshold_contexts
            .iter()
            .filter(|(_, context)| context.is_ecdsa())
            .map(|(cid, context)| (*cid, context.clone()))
            .collect()
    }

    pub fn sign_with_schnorr_contexts(&self) -> BTreeMap<CallbackId, SignWithThresholdContext> {
        self.sign_with_threshold_contexts
            .iter()
            .filter(|(_, context)| context.is_schnorr())
            .map(|(cid, context)| (*cid, context.clone()))
            .collect()
    }

    pub fn vetkd_derive_key_contexts(&self) -> BTreeMap<CallbackId, SignWithThresholdContext> {
        self.sign_with_threshold_contexts
            .iter()
            .filter(|(_, context)| context.is_vetkd())
            .map(|(cid, context)| (*cid, context.clone()))
            .collect()
    }
}

impl From<&SubnetCallContextManager> for pb_metadata::SubnetCallContextManager {
    fn from(item: &SubnetCallContextManager) -> Self {
        Self {
            next_callback_id: item.next_callback_id,
            setup_initial_dkg_contexts: item
                .setup_initial_dkg_contexts
                .iter()
                .map(
                    |(callback_id, context)| pb_metadata::SetupInitialDkgContextTree {
                        callback_id: callback_id.get(),
                        context: Some(context.into()),
                    },
                )
                .collect(),
            sign_with_threshold_contexts: item
                .sign_with_threshold_contexts
                .iter()
                .map(
                    |(callback_id, context)| pb_metadata::SignWithThresholdContextTree {
                        callback_id: callback_id.get(),
                        context: Some(context.into()),
                    },
                )
                .collect(),
            pre_signature_stashes: item
                .pre_signature_stashes
                .iter()
                .map(
                    |(key_id, pre_signature_stash)| pb_metadata::PreSignatureStashTree {
                        key_id: Some(key_id.into()),
                        key_transcript: Some(pre_signature_stash.key_transcript.as_ref().into()),
                        pre_signatures: pre_signature_stash
                            .pre_signatures
                            .iter()
                            .map(|(id, pre_sig)| pb_metadata::PreSignatureIdPair {
                                pre_sig_id: id.0,
                                pre_signature: Some(pre_sig.into()),
                            })
                            .collect(),
                    },
                )
                .collect(),
            canister_http_request_contexts: item
                .canister_http_request_contexts
                .iter()
                .map(
                    |(callback_id, context)| pb_metadata::CanisterHttpRequestContextTree {
                        callback_id: callback_id.get(),
                        context: Some(context.into()),
                    },
                )
                .collect(),
            bitcoin_get_successors_contexts: item
                .bitcoin_get_successors_contexts
                .iter()
                .map(
                    |(callback_id, context)| pb_metadata::BitcoinGetSuccessorsContextTree {
                        callback_id: callback_id.get(),
                        context: Some(context.into()),
                    },
                )
                .collect(),
            bitcoin_send_transaction_internal_contexts: item
                .bitcoin_send_transaction_internal_contexts
                .iter()
                .map(|(callback_id, context)| {
                    pb_metadata::BitcoinSendTransactionInternalContextTree {
                        callback_id: callback_id.get(),
                        context: Some(context.into()),
                    }
                })
                .collect(),
            install_code_calls: item
                .canister_management_calls
                .install_code_call_manager
                .install_code_calls
                .iter()
                .map(|(call_id, call)| pb_metadata::InstallCodeCallTree {
                    call_id: call_id.get(),
                    call: Some(call.into()),
                })
                .collect(),
            install_code_requests: vec![],
            next_install_code_call_id: item
                .canister_management_calls
                .install_code_call_manager
                .next_call_id,

            stop_canister_calls: item
                .canister_management_calls
                .stop_canister_call_manager
                .stop_canister_calls
                .iter()
                .map(|(call_id, call)| pb_metadata::StopCanisterCallTree {
                    call_id: call_id.get(),
                    call: Some(call.into()),
                })
                .collect(),
            next_stop_canister_call_id: item
                .canister_management_calls
                .stop_canister_call_manager
                .next_call_id,
            raw_rand_contexts: item
                .raw_rand_contexts
                .iter()
                .map(|context| context.into())
                .collect(),
            reshare_chain_key_contexts: item
                .reshare_chain_key_contexts
                .iter()
                .map(
                    |(callback_id, context)| pb_metadata::ReshareChainKeyContextTree {
                        callback_id: callback_id.get(),
                        context: Some(context.into()),
                    },
                )
                .collect(),
        }
    }
}

impl TryFrom<(Time, pb_metadata::SubnetCallContextManager)> for SubnetCallContextManager {
    type Error = ProxyDecodeError;
    fn try_from(
        (time, item): (Time, pb_metadata::SubnetCallContextManager),
    ) -> Result<Self, Self::Error> {
        let mut setup_initial_dkg_contexts = BTreeMap::<CallbackId, SetupInitialDkgContext>::new();
        for entry in item.setup_initial_dkg_contexts {
            let pb_context =
                try_from_option_field(entry.context, "SystemMetadata::SetupInitialDkgContext")?;
            let context = SetupInitialDkgContext::try_from((time, pb_context))?;
            setup_initial_dkg_contexts.insert(CallbackId::new(entry.callback_id), context);
        }

        let mut sign_with_threshold_contexts =
            BTreeMap::<CallbackId, SignWithThresholdContext>::new();
        for entry in item.sign_with_threshold_contexts {
            let context: SignWithThresholdContext =
                try_from_option_field(entry.context, "SystemMetadata::SignWithThresholdContext")?;
            sign_with_threshold_contexts.insert(CallbackId::new(entry.callback_id), context);
        }

        let mut pre_signature_stashes = BTreeMap::<MasterPublicKeyId, PreSignatureStash>::new();
        for entry in item.pre_signature_stashes {
            let key_id: MasterPublicKeyId = try_from_option_field(
                entry.key_id,
                "SystemMetadata::PreSignatureStash::MasterPublicKeyId",
            )?;
            let key_transcript: IDkgTranscript = try_from_option_field(
                entry.key_transcript.as_ref(),
                "SystemMetadata::PreSignatureStash::IDkgTranscript",
            )?;
            let mut pre_signatures = BTreeMap::new();
            for pre_signature in entry.pre_signatures {
                pre_signatures.insert(
                    PreSigId(pre_signature.pre_sig_id),
                    try_from_option_field(
                        pre_signature.pre_signature.as_ref(),
                        "SystemMetadata::PreSignatureStash::PreSignature",
                    )?,
                );
            }
            pre_signature_stashes.insert(
                key_id,
                PreSignatureStash {
                    key_transcript: Arc::new(key_transcript),
                    pre_signatures,
                },
            );
        }

        let mut canister_http_request_contexts =
            BTreeMap::<CallbackId, CanisterHttpRequestContext>::new();
        for entry in item.canister_http_request_contexts {
            let context: CanisterHttpRequestContext =
                try_from_option_field(entry.context, "SystemMetadata::CanisterHttpRequestContext")?;
            canister_http_request_contexts.insert(CallbackId::new(entry.callback_id), context);
        }

        let mut reshare_chain_key_contexts = BTreeMap::<CallbackId, ReshareChainKeyContext>::new();
        for entry in item.reshare_chain_key_contexts {
            let pb_context =
                try_from_option_field(entry.context, "SystemMetadata::ReshareChainKeyContext")?;
            let context = ReshareChainKeyContext::try_from((time, pb_context))?;
            reshare_chain_key_contexts.insert(CallbackId::new(entry.callback_id), context);
        }

        let mut bitcoin_get_successors_contexts =
            BTreeMap::<CallbackId, BitcoinGetSuccessorsContext>::new();
        for entry in item.bitcoin_get_successors_contexts {
            let pb_context = try_from_option_field(
                entry.context,
                "SystemMetadata::BitcoinGetSuccessorsContext",
            )?;
            let context = BitcoinGetSuccessorsContext::try_from((time, pb_context))?;
            bitcoin_get_successors_contexts.insert(CallbackId::new(entry.callback_id), context);
        }

        let mut bitcoin_send_transaction_internal_contexts =
            BTreeMap::<CallbackId, BitcoinSendTransactionInternalContext>::new();
        for entry in item.bitcoin_send_transaction_internal_contexts {
            let pb_context = try_from_option_field(
                entry.context,
                "SystemMetadata::BitcoinSendTransactionInternalContext",
            )?;
            let context = BitcoinSendTransactionInternalContext::try_from((time, pb_context))?;
            bitcoin_send_transaction_internal_contexts
                .insert(CallbackId::new(entry.callback_id), context);
        }

        let mut install_code_calls = BTreeMap::<InstallCodeCallId, InstallCodeCall>::new();
        // TODO(EXC-1454): Remove when `install_code_requests` field is not needed.
        for entry in item.install_code_requests {
            let pb_request = entry.request.ok_or(ProxyDecodeError::MissingField(
                "InstallCodeRequest::request",
            ))?;
            let call = InstallCodeCall::try_from((time, pb_request))?;
            install_code_calls.insert(InstallCodeCallId::new(entry.request_id), call);
        }
        for entry in item.install_code_calls {
            let pb_call = entry.call.ok_or(ProxyDecodeError::MissingField(
                "SystemMetadata::InstallCodeCall",
            ))?;
            let call = InstallCodeCall::try_from((time, pb_call))?;
            install_code_calls.insert(InstallCodeCallId::new(entry.call_id), call);
        }
        let install_code_call_manager: InstallCodeCallManager = InstallCodeCallManager {
            next_call_id: item.next_install_code_call_id,
            install_code_calls,
        };

        let mut stop_canister_calls = BTreeMap::<StopCanisterCallId, StopCanisterCall>::new();
        for entry in item.stop_canister_calls {
            let pb_call = try_from_option_field(entry.call, "SystemMetadata::StopCanisterCall")?;
            let call = StopCanisterCall::try_from((time, pb_call))?;
            stop_canister_calls.insert(StopCanisterCallId::new(entry.call_id), call);
        }
        let stop_canister_call_manager = StopCanisterCallManager {
            next_call_id: item.next_stop_canister_call_id,
            stop_canister_calls,
        };
        let mut raw_rand_contexts = VecDeque::<RawRandContext>::new();
        for pb_context in item.raw_rand_contexts {
            let context = RawRandContext::try_from((time, pb_context))?;
            raw_rand_contexts.push_back(context);
        }

        Ok(Self {
            next_callback_id: item.next_callback_id,
            setup_initial_dkg_contexts,
            sign_with_threshold_contexts,
            canister_http_request_contexts,
            bitcoin_get_successors_contexts,
            bitcoin_send_transaction_internal_contexts,
            canister_management_calls: CanisterManagementCalls {
                install_code_call_manager,
                stop_canister_call_manager,
            },
            raw_rand_contexts,
            reshare_chain_key_contexts,
            pre_signature_stashes,
        })
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct SetupInitialDkgContext {
    pub request: Request,
    pub nodes_in_target_subnet: BTreeSet<NodeId>,
    pub target_id: NiDkgTargetId,
    pub registry_version: RegistryVersion,
    pub time: Time,
}

impl From<&SetupInitialDkgContext> for pb_metadata::SetupInitialDkgContext {
    fn from(context: &SetupInitialDkgContext) -> Self {
        Self {
            request: Some((&context.request).into()),
            nodes_in_subnet: context
                .nodes_in_target_subnet
                .iter()
                .map(|node_id| node_id_into_protobuf(*node_id))
                .collect(),
            target_id: context.target_id.to_vec(),
            registry_version: context.registry_version.get(),
            time: Some(pb_metadata::Time {
                time_nanos: context.time.as_nanos_since_unix_epoch(),
            }),
        }
    }
}

impl TryFrom<(Time, pb_metadata::SetupInitialDkgContext)> for SetupInitialDkgContext {
    type Error = ProxyDecodeError;
    fn try_from(
        (time, context): (Time, pb_metadata::SetupInitialDkgContext),
    ) -> Result<Self, Self::Error> {
        let mut nodes_in_target_subnet = BTreeSet::<NodeId>::new();
        for node_id in context.nodes_in_subnet {
            nodes_in_target_subnet.insert(node_id_try_from_option(Some(node_id))?);
        }
        Ok(SetupInitialDkgContext {
            request: try_from_option_field(context.request, "SetupInitialDkgContext::request")?,
            nodes_in_target_subnet,
            target_id: match ni_dkg_target_id(context.target_id.as_slice()) {
                Ok(target_id) => target_id,
                Err(_) => return Err(Self::Error::Other("target_id is not 32 bytes.".to_string())),
            },
            registry_version: RegistryVersion::from(context.registry_version),
            time: context
                .time
                .map_or(time, |t| Time::from_nanos_since_unix_epoch(t.time_nanos)),
        })
    }
}

/// Tries to convert a vector of bytes into an array of N bytes.
fn try_into_array<const N: usize>(bytes: Vec<u8>, name: &str) -> Result<[u8; N], ProxyDecodeError> {
    if bytes.len() != N {
        return Err(ProxyDecodeError::Other(format!(
            "{} is not {} bytes.",
            name, N
        )));
    }
    let mut id = [0; N];
    id.copy_from_slice(&bytes);
    Ok(id)
}

fn try_into_array_message_hash(
    bytes: Vec<u8>,
) -> Result<[u8; MESSAGE_HASH_SIZE], ProxyDecodeError> {
    try_into_array::<MESSAGE_HASH_SIZE>(bytes, "message_hash")
}

fn try_into_array_pseudo_random_id(
    bytes: Vec<u8>,
) -> Result<[u8; PSEUDO_RANDOM_ID_SIZE], ProxyDecodeError> {
    try_into_array::<PSEUDO_RANDOM_ID_SIZE>(bytes, "pseudo_random_id")
}

fn try_into_array_nonce(bytes: Vec<u8>) -> Result<[u8; NONCE_SIZE], ProxyDecodeError> {
    try_into_array::<NONCE_SIZE>(bytes, "nonce")
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct EcdsaMatchedPreSignature {
    pub id: PreSigId,
    pub height: Height,
    pub pre_signature: Arc<EcdsaPreSignatureQuadruple>,
    pub key_transcript: Arc<IDkgTranscript>,
}

impl From<&EcdsaMatchedPreSignature> for pb_types::EcdsaMatchedPreSignature {
    fn from(value: &EcdsaMatchedPreSignature) -> Self {
        Self {
            pre_signature_id: value.id.0,
            height: value.height.get(),
            pre_signature: Some(pb_types::EcdsaPreSignatureQuadruple::from(
                value.pre_signature.as_ref(),
            )),
            key_transcript: Some(pb_subnet::IDkgTranscript::from(
                value.key_transcript.as_ref(),
            )),
        }
    }
}

impl TryFrom<pb_types::EcdsaMatchedPreSignature> for EcdsaMatchedPreSignature {
    type Error = ProxyDecodeError;
    fn try_from(proto: pb_types::EcdsaMatchedPreSignature) -> Result<Self, Self::Error> {
        Ok(EcdsaMatchedPreSignature {
            id: PreSigId(proto.pre_signature_id),
            height: Height::from(proto.height),
            pre_signature: Arc::new(try_from_option_field(
                proto.pre_signature.as_ref(),
                "EcdsaMatchedPreSignature::pre_signature",
            )?),
            key_transcript: Arc::new(try_from_option_field(
                proto.key_transcript.as_ref(),
                "EcdsaMatchedPreSignature::key_transcript",
            )?),
        })
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct EcdsaArguments {
    pub key_id: EcdsaKeyId,
    pub message_hash: [u8; MESSAGE_HASH_SIZE],
    pub pre_signature: Option<EcdsaMatchedPreSignature>,
}

impl From<&EcdsaArguments> for pb_metadata::EcdsaArguments {
    fn from(args: &EcdsaArguments) -> Self {
        Self {
            key_id: Some((&args.key_id).into()),
            message_hash: args.message_hash.to_vec(),
            pre_signature: args
                .pre_signature
                .as_ref()
                .map(pb_types::EcdsaMatchedPreSignature::from),
        }
    }
}

impl TryFrom<pb_metadata::EcdsaArguments> for EcdsaArguments {
    type Error = ProxyDecodeError;
    fn try_from(context: pb_metadata::EcdsaArguments) -> Result<Self, Self::Error> {
        Ok(EcdsaArguments {
            key_id: try_from_option_field(context.key_id, "EcdsaArguments::key_id")?,
            message_hash: try_into_array_message_hash(context.message_hash)?,
            pre_signature: context
                .pre_signature
                .map(EcdsaMatchedPreSignature::try_from)
                .transpose()?,
        })
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct SchnorrMatchedPreSignature {
    pub id: PreSigId,
    pub height: Height,
    pub pre_signature: Arc<SchnorrPreSignatureTranscript>,
    pub key_transcript: Arc<IDkgTranscript>,
}

impl From<&SchnorrMatchedPreSignature> for pb_types::SchnorrMatchedPreSignature {
    fn from(value: &SchnorrMatchedPreSignature) -> Self {
        Self {
            pre_signature_id: value.id.0,
            height: value.height.get(),
            pre_signature: Some(pb_types::SchnorrPreSignatureTranscript::from(
                value.pre_signature.as_ref(),
            )),
            key_transcript: Some(pb_subnet::IDkgTranscript::from(
                value.key_transcript.as_ref(),
            )),
        }
    }
}

impl TryFrom<pb_types::SchnorrMatchedPreSignature> for SchnorrMatchedPreSignature {
    type Error = ProxyDecodeError;
    fn try_from(proto: pb_types::SchnorrMatchedPreSignature) -> Result<Self, Self::Error> {
        Ok(SchnorrMatchedPreSignature {
            id: PreSigId(proto.pre_signature_id),
            height: Height::from(proto.height),
            pre_signature: Arc::new(try_from_option_field(
                proto.pre_signature.as_ref(),
                "SchnorrMatchedPreSignature::pre_signature",
            )?),
            key_transcript: Arc::new(try_from_option_field(
                proto.key_transcript.as_ref(),
                "SchnorrMatchedPreSignature::key_transcript",
            )?),
        })
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct SchnorrArguments {
    pub key_id: SchnorrKeyId,
    pub message: Arc<Vec<u8>>,
    pub taproot_tree_root: Option<Arc<Vec<u8>>>,
    pub pre_signature: Option<SchnorrMatchedPreSignature>,
}

impl From<&SchnorrArguments> for pb_metadata::SchnorrArguments {
    fn from(args: &SchnorrArguments) -> Self {
        Self {
            key_id: Some((&args.key_id).into()),
            message: args.message.to_vec(),
            taproot_tree_root: args.taproot_tree_root.as_ref().map(|v| v.to_vec()),
            pre_signature: args
                .pre_signature
                .as_ref()
                .map(pb_types::SchnorrMatchedPreSignature::from),
        }
    }
}

impl TryFrom<pb_metadata::SchnorrArguments> for SchnorrArguments {
    type Error = ProxyDecodeError;
    fn try_from(context: pb_metadata::SchnorrArguments) -> Result<Self, Self::Error> {
        Ok(SchnorrArguments {
            key_id: try_from_option_field(context.key_id, "SchnorrArguments::key_id")?,
            message: Arc::new(context.message),
            taproot_tree_root: context.taproot_tree_root.map(Arc::new),
            pre_signature: context
                .pre_signature
                .map(SchnorrMatchedPreSignature::try_from)
                .transpose()?,
        })
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct VetKdArguments {
    pub key_id: VetKdKeyId,
    pub input: Arc<Vec<u8>>,
    pub transport_public_key: Vec<u8>,
    pub ni_dkg_id: NiDkgId,
    pub height: Height,
}

impl From<&VetKdArguments> for pb_metadata::VetKdArguments {
    fn from(args: &VetKdArguments) -> Self {
        Self {
            key_id: Some((&args.key_id).into()),
            input: args.input.to_vec(),
            transport_public_key: args.transport_public_key.to_vec(),
            ni_dkg_id: Some((args.ni_dkg_id.clone()).into()),
            height: args.height.get(),
        }
    }
}

impl TryFrom<pb_metadata::VetKdArguments> for VetKdArguments {
    type Error = ProxyDecodeError;
    fn try_from(context: pb_metadata::VetKdArguments) -> Result<Self, Self::Error> {
        Ok(VetKdArguments {
            key_id: try_from_option_field(context.key_id, "VetKdArguments::key_id")?,
            input: Arc::new(context.input),
            transport_public_key: context.transport_public_key,
            ni_dkg_id: try_from_option_field(context.ni_dkg_id, "VetKdArguments::ni_dkg_id")?,
            height: Height::from(context.height),
        })
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum ThresholdArguments {
    Ecdsa(EcdsaArguments),
    Schnorr(SchnorrArguments),
    VetKd(VetKdArguments),
}

impl ThresholdArguments {
    /// Returns the generic key id.
    pub fn key_id(&self) -> MasterPublicKeyId {
        match self {
            ThresholdArguments::Ecdsa(args) => MasterPublicKeyId::Ecdsa(args.key_id.clone()),
            ThresholdArguments::Schnorr(args) => MasterPublicKeyId::Schnorr(args.key_id.clone()),
            ThresholdArguments::VetKd(args) => MasterPublicKeyId::VetKd(args.key_id.clone()),
        }
    }
}

impl From<&ThresholdArguments> for pb_metadata::ThresholdArguments {
    fn from(context: &ThresholdArguments) -> Self {
        let threshold_scheme = match context {
            ThresholdArguments::Ecdsa(args) => {
                pb_metadata::threshold_arguments::ThresholdScheme::Ecdsa(args.into())
            }
            ThresholdArguments::Schnorr(args) => {
                pb_metadata::threshold_arguments::ThresholdScheme::Schnorr(args.into())
            }
            ThresholdArguments::VetKd(args) => {
                pb_metadata::threshold_arguments::ThresholdScheme::Vetkd(args.into())
            }
        };
        Self {
            threshold_scheme: Some(threshold_scheme),
        }
    }
}

impl TryFrom<pb_metadata::ThresholdArguments> for ThresholdArguments {
    type Error = ProxyDecodeError;
    fn try_from(context: pb_metadata::ThresholdArguments) -> Result<Self, Self::Error> {
        let threshold_scheme = try_from_option_field(
            context.threshold_scheme,
            "ThresholdArguments::threshold_scheme",
        )?;
        match threshold_scheme {
            pb_metadata::threshold_arguments::ThresholdScheme::Ecdsa(args) => {
                Ok(ThresholdArguments::Ecdsa(EcdsaArguments::try_from(args)?))
            }
            pb_metadata::threshold_arguments::ThresholdScheme::Schnorr(args) => Ok(
                ThresholdArguments::Schnorr(SchnorrArguments::try_from(args)?),
            ),
            pb_metadata::threshold_arguments::ThresholdScheme::Vetkd(args) => {
                Ok(ThresholdArguments::VetKd(VetKdArguments::try_from(args)?))
            }
        }
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct IDkgSignWithThresholdContext<'a>(&'a SignWithThresholdContext);

impl<'a> TryFrom<&'a SignWithThresholdContext> for IDkgSignWithThresholdContext<'a> {
    type Error = ();

    fn try_from(val: &'a SignWithThresholdContext) -> Result<Self, Self::Error> {
        if !val.is_idkg() {
            Err(())
        } else {
            Ok(Self(val))
        }
    }
}

impl<'a> From<IDkgSignWithThresholdContext<'a>> for &'a SignWithThresholdContext {
    fn from(val: IDkgSignWithThresholdContext<'a>) -> Self {
        val.0
    }
}

impl IDkgSignWithThresholdContext<'_> {
    pub fn inner(&self) -> &SignWithThresholdContext {
        self.0
    }
}

impl std::ops::Deref for IDkgSignWithThresholdContext<'_> {
    type Target = SignWithThresholdContext;

    fn deref(&self) -> &<Self as std::ops::Deref>::Target {
        self.inner()
    }
}

impl std::borrow::Borrow<SignWithThresholdContext> for IDkgSignWithThresholdContext<'_> {
    fn borrow(&self) -> &SignWithThresholdContext {
        self.inner()
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct SignWithThresholdContext {
    pub request: Request,
    pub args: ThresholdArguments,
    pub derivation_path: Arc<Vec<Vec<u8>>>,
    pub pseudo_random_id: [u8; PSEUDO_RANDOM_ID_SIZE],
    pub batch_time: Time,
    pub matched_pre_signature: Option<(PreSigId, Height)>,
    pub nonce: Option<[u8; NONCE_SIZE]>,
}

impl SignWithThresholdContext {
    /// Returns the key id of the master public key.
    pub fn key_id(&self) -> MasterPublicKeyId {
        match &self.args {
            ThresholdArguments::Ecdsa(args) => MasterPublicKeyId::Ecdsa(args.key_id.clone()),
            ThresholdArguments::Schnorr(args) => MasterPublicKeyId::Schnorr(args.key_id.clone()),
            ThresholdArguments::VetKd(args) => MasterPublicKeyId::VetKd(args.key_id.clone()),
        }
    }

    pub fn requires_pre_signature(&self) -> bool {
        match &self.args {
            ThresholdArguments::Ecdsa(args) => {
                self.matched_pre_signature.is_none() && args.pre_signature.is_none()
            }
            ThresholdArguments::Schnorr(args) => {
                self.matched_pre_signature.is_none() && args.pre_signature.is_none()
            }
            ThresholdArguments::VetKd(_) => false,
        }
    }

    /// Returns true if arguments are for ECDSA.
    pub fn is_ecdsa(&self) -> bool {
        matches!(&self.args, ThresholdArguments::Ecdsa(_))
    }

    /// Returns true if arguments are for Schnorr.
    pub fn is_schnorr(&self) -> bool {
        matches!(&self.args, ThresholdArguments::Schnorr(_))
    }

    /// Returns true if arguments are for VetKd.
    pub fn is_vetkd(&self) -> bool {
        matches!(&self.args, ThresholdArguments::VetKd(_))
    }

    /// Returns true if arguments are for a context handled by IDKG.
    pub fn is_idkg(&self) -> bool {
        match &self.args {
            ThresholdArguments::Ecdsa(_) | ThresholdArguments::Schnorr(_) => true,
            ThresholdArguments::VetKd(_) => false,
        }
    }

    /// Returns ECDSA arguments.
    /// Panics if arguments are not for ECDSA.
    /// Should only be called if `is_ecdsa` returns true.
    pub fn ecdsa_args(&self) -> &EcdsaArguments {
        match &self.args {
            ThresholdArguments::Ecdsa(args) => args,
            _ => panic!("ECDSA arguments not found."),
        }
    }

    /// Returns Schnorr arguments.
    /// Panics if arguments are not for Schnorr
    /// Should only be called if `is_schnorr` returns true.
    pub fn schnorr_args(&self) -> &SchnorrArguments {
        match &self.args {
            ThresholdArguments::Schnorr(args) => args,
            _ => panic!("Schnorr arguments not found."),
        }
    }

    /// Returns VetKd arguments.
    /// Panics if arguments are not for VetKd
    /// Should only be called if `is_vetkd` returns true.
    pub fn vetkd_args(&self) -> &VetKdArguments {
        match &self.args {
            ThresholdArguments::VetKd(args) => args,
            _ => panic!("VetKd arguments not found."),
        }
    }

    /// Return all IDkgTranscripts included in this context
    pub fn iter_idkg_transcripts(&self) -> impl Iterator<Item = &IDkgTranscript> {
        let refs = match &self.args {
            ThresholdArguments::Ecdsa(args) => args
                .pre_signature
                .as_ref()
                .map(|pre_sig| {
                    vec![
                        pre_sig.pre_signature.kappa_unmasked(),
                        pre_sig.pre_signature.lambda_masked(),
                        pre_sig.pre_signature.kappa_times_lambda(),
                        pre_sig.pre_signature.key_times_lambda(),
                        &pre_sig.key_transcript,
                    ]
                })
                .unwrap_or_default(),
            ThresholdArguments::Schnorr(args) => args
                .pre_signature
                .as_ref()
                .map(|pre_sig| {
                    vec![
                        pre_sig.pre_signature.blinder_unmasked(),
                        &pre_sig.key_transcript,
                    ]
                })
                .unwrap_or_default(),
            ThresholdArguments::VetKd(_) => vec![],
        };
        refs.into_iter()
    }
}

impl From<&SignWithThresholdContext> for pb_metadata::SignWithThresholdContext {
    fn from(context: &SignWithThresholdContext) -> Self {
        Self {
            request: Some((&context.request).into()),
            args: Some((&context.args).into()),
            derivation_path_vec: context.derivation_path.to_vec(),
            pseudo_random_id: context.pseudo_random_id.to_vec(),
            batch_time: context.batch_time.as_nanos_since_unix_epoch(),
            pre_signature_id: context.matched_pre_signature.as_ref().map(|q| q.0.id()),
            height: context.matched_pre_signature.as_ref().map(|q| q.1.get()),
            nonce: context.nonce.map(|n| n.to_vec()),
        }
    }
}

impl TryFrom<pb_metadata::SignWithThresholdContext> for SignWithThresholdContext {
    type Error = ProxyDecodeError;
    fn try_from(context: pb_metadata::SignWithThresholdContext) -> Result<Self, Self::Error> {
        let request: Request =
            try_from_option_field(context.request, "SignWithThresholdContext::request")?;
        let args: ThresholdArguments =
            try_from_option_field(context.args, "SignWithThresholdContext::args")?;
        Ok(SignWithThresholdContext {
            request,
            args,
            derivation_path: Arc::new(context.derivation_path_vec),
            pseudo_random_id: try_into_array_pseudo_random_id(context.pseudo_random_id)?,
            batch_time: Time::from_nanos_since_unix_epoch(context.batch_time),
            matched_pre_signature: context
                .pre_signature_id
                .map(PreSigId)
                .zip(context.height)
                .map(|(q, h)| (q, Height::from(h))),
            nonce: context.nonce.map(try_into_array_nonce).transpose()?,
        })
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct ReshareChainKeyContext {
    pub request: Request,
    pub key_id: MasterPublicKeyId,
    pub nodes: BTreeSet<NodeId>,
    pub registry_version: RegistryVersion,
    pub time: Time,
    pub target_id: NiDkgTargetId,
}

impl From<&ReshareChainKeyContext> for pb_metadata::ReshareChainKeyContext {
    fn from(context: &ReshareChainKeyContext) -> Self {
        Self {
            request: Some(pb_queues::Request::from(&context.request)),
            key_id: Some(pb_types::MasterPublicKeyId::from(&context.key_id)),
            nodes: context
                .nodes
                .iter()
                .map(|node_id| node_id_into_protobuf(*node_id))
                .collect(),
            registry_version: context.registry_version.get(),
            time: Some(pb_metadata::Time {
                time_nanos: context.time.as_nanos_since_unix_epoch(),
            }),
            target_id: context.target_id.to_vec(),
        }
    }
}

impl TryFrom<(Time, pb_metadata::ReshareChainKeyContext)> for ReshareChainKeyContext {
    type Error = ProxyDecodeError;
    fn try_from(
        (time, context): (Time, pb_metadata::ReshareChainKeyContext),
    ) -> Result<Self, Self::Error> {
        let key_id: MasterPublicKeyId =
            try_from_option_field(context.key_id, "ReshareChainKeyContext::key_id")?;

        Ok(Self {
            request: try_from_option_field(context.request, "ReshareChainKeyContext::request")?,
            key_id,
            nodes: context
                .nodes
                .into_iter()
                .map(|node_id| node_id_try_from_option(Some(node_id)))
                .collect::<Result<_, _>>()?,
            registry_version: RegistryVersion::from(context.registry_version),
            time: context
                .time
                .map_or(time, |t| Time::from_nanos_since_unix_epoch(t.time_nanos)),
            target_id: {
                match ni_dkg_target_id(context.target_id.as_slice()) {
                    Ok(target_id) => target_id,
                    Err(_) => {
                        return Err(Self::Error::Other("target_id is not 32 bytes.".to_string()))
                    }
                }
            },
        })
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct BitcoinGetSuccessorsContext {
    pub request: Request,
    pub payload: GetSuccessorsRequestInitial,
    pub time: Time,
}

impl From<&BitcoinGetSuccessorsContext> for pb_metadata::BitcoinGetSuccessorsContext {
    fn from(context: &BitcoinGetSuccessorsContext) -> Self {
        Self {
            request: Some((&context.request).into()),
            payload: Some((&context.payload).into()),
            time: Some(pb_metadata::Time {
                time_nanos: context.time.as_nanos_since_unix_epoch(),
            }),
        }
    }
}

impl TryFrom<(Time, pb_metadata::BitcoinGetSuccessorsContext)> for BitcoinGetSuccessorsContext {
    type Error = ProxyDecodeError;
    fn try_from(
        (time, context): (Time, pb_metadata::BitcoinGetSuccessorsContext),
    ) -> Result<Self, Self::Error> {
        let request: Request =
            try_from_option_field(context.request, "BitcoinGetSuccessorsContext::request")?;
        let payload: GetSuccessorsRequestInitial =
            try_from_option_field(context.payload, "BitcoinGetSuccessorsContext::payload")?;
        Ok(BitcoinGetSuccessorsContext {
            request,
            payload,
            time: context
                .time
                .map_or(time, |t| Time::from_nanos_since_unix_epoch(t.time_nanos)),
        })
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct BitcoinSendTransactionInternalContext {
    pub request: Request,
    pub payload: SendTransactionRequest,
    pub time: Time,
}

impl From<&BitcoinSendTransactionInternalContext>
    for pb_metadata::BitcoinSendTransactionInternalContext
{
    fn from(context: &BitcoinSendTransactionInternalContext) -> Self {
        Self {
            request: Some((&context.request).into()),
            payload: Some((&context.payload).into()),
            time: Some(pb_metadata::Time {
                time_nanos: context.time.as_nanos_since_unix_epoch(),
            }),
        }
    }
}

impl TryFrom<(Time, pb_metadata::BitcoinSendTransactionInternalContext)>
    for BitcoinSendTransactionInternalContext
{
    type Error = ProxyDecodeError;
    fn try_from(
        (time, context): (Time, pb_metadata::BitcoinSendTransactionInternalContext),
    ) -> Result<Self, Self::Error> {
        let request: Request =
            try_from_option_field(context.request, "BitcoinGetSuccessorsContext::request")?;
        let payload: SendTransactionRequest =
            try_from_option_field(context.payload, "BitcoinGetSuccessorsContext::payload")?;
        Ok(BitcoinSendTransactionInternalContext {
            request,
            payload,
            time: context
                .time
                .map_or(time, |t| Time::from_nanos_since_unix_epoch(t.time_nanos)),
        })
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct InstallCodeCall {
    pub call: CanisterCall,
    pub time: Time,
    pub effective_canister_id: CanisterId,
}

impl From<&InstallCodeCall> for pb_metadata::InstallCodeCall {
    fn from(install_code_call: &InstallCodeCall) -> Self {
        use pb_metadata::install_code_call::CanisterCall as PbCanisterCall;
        let call = match &install_code_call.call {
            CanisterCall::Request(request) => PbCanisterCall::Request(request.as_ref().into()),
            CanisterCall::Ingress(ingress) => PbCanisterCall::Ingress(ingress.as_ref().into()),
        };
        Self {
            canister_call: Some(call),
            effective_canister_id: Some((install_code_call.effective_canister_id).into()),
            time: Some(pb_metadata::Time {
                time_nanos: install_code_call.time.as_nanos_since_unix_epoch(),
            }),
        }
    }
}

impl TryFrom<(Time, pb_metadata::InstallCodeRequest)> for InstallCodeCall {
    type Error = ProxyDecodeError;
    fn try_from(
        (time, install_code_request): (Time, pb_metadata::InstallCodeRequest),
    ) -> Result<Self, Self::Error> {
        let pb_call = install_code_request
            .request
            .ok_or(ProxyDecodeError::MissingField(
                "InstallCodeRequest::request",
            ))?;
        let effective_canister_id: CanisterId = try_from_option_field(
            install_code_request.effective_canister_id,
            "InstallCodeRequest::effective_canister_id",
        )?;
        Ok(InstallCodeCall {
            call: CanisterCall::Request(Arc::new(pb_call.try_into()?)),
            effective_canister_id,
            time: install_code_request
                .time
                .map_or(time, |t| Time::from_nanos_since_unix_epoch(t.time_nanos)),
        })
    }
}

impl TryFrom<(Time, pb_metadata::InstallCodeCall)> for InstallCodeCall {
    type Error = ProxyDecodeError;
    fn try_from(
        (time, install_code_call): (Time, pb_metadata::InstallCodeCall),
    ) -> Result<Self, Self::Error> {
        use pb_metadata::install_code_call::CanisterCall as PbCanisterCall;
        let pb_call = install_code_call
            .canister_call
            .ok_or(ProxyDecodeError::MissingField(
                "InstallCodeCall::canister_call",
            ))?;

        let call = match pb_call {
            PbCanisterCall::Request(request) => {
                CanisterCall::Request(Arc::new(request.try_into()?))
            }
            PbCanisterCall::Ingress(ingress) => {
                CanisterCall::Ingress(Arc::new(ingress.try_into()?))
            }
        };

        let effective_canister_id: CanisterId = try_from_option_field(
            install_code_call.effective_canister_id,
            "InstallCodeCall::effective_canister_id",
        )?;
        Ok(InstallCodeCall {
            call,
            effective_canister_id,
            time: install_code_call
                .time
                .map_or(time, |t| Time::from_nanos_since_unix_epoch(t.time_nanos)),
        })
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct StopCanisterCall {
    pub call: CanisterCall,
    pub effective_canister_id: CanisterId,
    pub time: Time,
}

impl From<&StopCanisterCall> for pb_metadata::StopCanisterCall {
    fn from(stop_canister_call: &StopCanisterCall) -> Self {
        use pb_metadata::stop_canister_call::CanisterCall as PbCanisterCall;
        let call = match &stop_canister_call.call {
            CanisterCall::Request(request) => PbCanisterCall::Request(request.as_ref().into()),
            CanisterCall::Ingress(ingress) => PbCanisterCall::Ingress(ingress.as_ref().into()),
        };
        Self {
            canister_call: Some(call),
            effective_canister_id: Some((stop_canister_call.effective_canister_id).into()),
            time: Some(pb_metadata::Time {
                time_nanos: stop_canister_call.time.as_nanos_since_unix_epoch(),
            }),
        }
    }
}

impl TryFrom<(Time, pb_metadata::StopCanisterCall)> for StopCanisterCall {
    type Error = ProxyDecodeError;
    fn try_from(
        (time, stop_canister_call): (Time, pb_metadata::StopCanisterCall),
    ) -> Result<Self, Self::Error> {
        use pb_metadata::stop_canister_call::CanisterCall as PbCanisterCall;
        let pb_call = stop_canister_call
            .canister_call
            .ok_or(ProxyDecodeError::MissingField(
                "StopCanisterCall::canister_call",
            ))?;

        let call = match pb_call {
            PbCanisterCall::Request(request) => {
                CanisterCall::Request(Arc::new(request.try_into()?))
            }
            PbCanisterCall::Ingress(ingress) => {
                CanisterCall::Ingress(Arc::new(ingress.try_into()?))
            }
        };
        let effective_canister_id = try_from_option_field(
            stop_canister_call.effective_canister_id,
            "StopCanisterCall::effective_canister_id",
        )?;
        Ok(StopCanisterCall {
            call,
            effective_canister_id,
            time: stop_canister_call
                .time
                .map_or(time, |t| Time::from_nanos_since_unix_epoch(t.time_nanos)),
        })
    }
}

/// Struct for tracking the required information needed for creating a response.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct RawRandContext {
    pub request: Request,
    pub time: Time,
    pub execution_round_id: ExecutionRound,
}

impl From<&RawRandContext> for pb_metadata::RawRandContext {
    fn from(context: &RawRandContext) -> Self {
        Self {
            request: Some((&context.request).into()),
            execution_round_id: context.execution_round_id.get(),
            time: Some(pb_metadata::Time {
                time_nanos: context.time.as_nanos_since_unix_epoch(),
            }),
        }
    }
}

impl TryFrom<(Time, pb_metadata::RawRandContext)> for RawRandContext {
    type Error = ProxyDecodeError;
    fn try_from((time, context): (Time, pb_metadata::RawRandContext)) -> Result<Self, Self::Error> {
        let request: Request = try_from_option_field(context.request, "RawRandContext::request")?;

        Ok(RawRandContext {
            request,
            execution_round_id: ExecutionRound::new(context.execution_round_id),
            time: context
                .time
                .map_or(time, |t| Time::from_nanos_since_unix_epoch(t.time_nanos)),
        })
    }
}

mod testing {
    use super::*;

    /// Early warning system / stumbling block forcing the authors of changes adding
    /// or removing replicated state fields to think about and/or ask the Message
    /// Routing team to think about any repercussions to the subnet splitting logic.
    ///
    /// If you do find yourself having to make changes to this function, it is quite
    /// possible that you have not broken anything. But there is a non-zero chance
    /// for changes to the structure of the replicated state to also require changes
    /// to the subnet splitting logic or risk breaking it. Which is why this brute
    /// force check exists.
    ///
    /// See `ReplicatedState::split()` and `ReplicatedState::after_split()` for more
    /// context.
    #[allow(dead_code)]
    fn subnet_splitting_change_guard_do_not_modify_without_reading_doc_comment() {
        //
        // DO NOT MODIFY WITHOUT READING DOC COMMENT!
        //
        let canister_management_calls = CanisterManagementCalls {
            install_code_call_manager: Default::default(),
            stop_canister_call_manager: Default::default(),
        };
        //
        // DO NOT MODIFY WITHOUT READING DOC COMMENT!
        //
        let _subnet_call_context_manager = SubnetCallContextManager {
            next_callback_id: 0,
            setup_initial_dkg_contexts: Default::default(),
            sign_with_threshold_contexts: Default::default(),
            canister_http_request_contexts: Default::default(),
            reshare_chain_key_contexts: Default::default(),
            bitcoin_get_successors_contexts: Default::default(),
            bitcoin_send_transaction_internal_contexts: Default::default(),
            canister_management_calls,
            raw_rand_contexts: Default::default(),
            pre_signature_stashes: Default::default(),
        };
    }
}
