use crate::{
    as_num_instructions,
    canister_manager::{
        uninstall_canister, AddCanisterChangeToHistory, CanisterManager, CanisterManagerError,
        CanisterMgrConfig, DtsInstallCodeResult, InstallCodeContext, StopCanisterResult,
        WasmSource, MAX_SLICE_SIZE_BYTES,
    },
    canister_settings::CanisterSettings,
    execution_environment::{as_round_instructions, CompilationCostHandling, RoundCounters},
    hypervisor::Hypervisor,
    types::{IngressResponse, Response},
    IngressHistoryWriterImpl, RoundLimits,
};
use assert_matches::assert_matches;
use candid::{CandidType, Decode, Encode};
use ic_base_types::{EnvironmentVariables, NumSeconds, PrincipalId};
use ic_config::{
    execution_environment::{
        Config, CANISTER_GUARANTEED_CALLBACK_QUOTA, DEFAULT_WASM_MEMORY_LIMIT,
        MAX_ENVIRONMENT_VARIABLES, MAX_ENVIRONMENT_VARIABLE_NAME_LENGTH,
        MAX_ENVIRONMENT_VARIABLE_VALUE_LENGTH, MAX_NUMBER_OF_SNAPSHOTS_PER_CANISTER,
        SUBNET_CALLBACK_SOFT_LIMIT,
    },
    flag_status::FlagStatus,
    subnet_config::SchedulerConfig,
};
use ic_cycles_account_manager::{CyclesAccountManager, ResourceSaturation};
use ic_embedders::{
    wasm_utils::instrumentation::{instruction_to_cost, WasmMemoryType},
    wasmtime_embedder::system_api::{ExecutionParameters, InstructionLimits},
};
use ic_error_types::{ErrorCode, UserError};
use ic_interfaces::execution_environment::{ExecutionMode, HypervisorError, SubnetAvailableMemory};
use ic_limits::SMALL_APP_SUBNET_MAX_SIZE;
use ic_logger::replica_logger::no_op_logger;
use ic_management_canister_types_private::{
    CanisterChange, CanisterChangeDetails, CanisterChangeOrigin, CanisterIdRecord,
    CanisterInstallMode, CanisterInstallModeV2, CanisterSettingsArgsBuilder,
    CanisterSnapshotResponse, CanisterStatusResultV2, CanisterStatusType, CanisterUpgradeOptions,
    ChunkHash, ClearChunkStoreArgs, CreateCanisterArgs, EmptyBlob, EnvironmentVariable,
    InstallCodeArgsV2, LoadCanisterSnapshotArgs, Method, NodeMetricsHistoryArgs,
    NodeMetricsHistoryResponse, OnLowWasmMemoryHookStatus, Payload, RenameCanisterArgs,
    RenameToArgs, StoredChunksArgs, StoredChunksReply, SubnetInfoArgs, SubnetInfoResponse,
    TakeCanisterSnapshotArgs, UpdateSettingsArgs, UploadChunkArgs, UploadChunkReply,
    WasmMemoryPersistence, IC_00,
};
use ic_metrics::MetricsRegistry;
use ic_registry_provisional_whitelist::ProvisionalWhitelist;
use ic_registry_routing_table::{CanisterIdRange, RoutingTable, CANISTER_IDS_PER_SUBNET};
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::{
    canister_state::system_state::{
        wasm_chunk_store::{self, ChunkValidationResult},
        CyclesUseCase,
    },
    metadata_state::subnet_call_context_manager::InstallCodeCallId,
    page_map::TestPageAllocatorFileDescriptorImpl,
    testing::{CanisterQueuesTesting, SystemStateTesting},
    CallContextManager, CallOrigin, CanisterState, CanisterStatus, ReplicatedState,
};
use ic_state_machine_tests::{
    two_subnets_simple, StateMachine, StateMachineBuilder, StateMachineConfig,
};
use ic_test_utilities::{
    cycles_account_manager::CyclesAccountManagerBuilder,
    state_manager::FakeStateManager,
    universal_canister::{call_args, wasm, UNIVERSAL_CANISTER_WASM},
};
use ic_test_utilities_execution_environment::{
    assert_delta, cycles_reserved_for_app_and_verified_app_subnets, get_reply,
    get_routing_table_with_specified_ids_allocation_range, wasm_compilation_cost, wat_canister,
    wat_compilation_cost, wat_fn, ExecutionTest, ExecutionTestBuilder,
};
use ic_test_utilities_state::{
    get_running_canister, get_stopped_canister, get_stopped_canister_with_controller,
    get_stopping_canister, CallContextBuilder, CanisterStateBuilder, ReplicatedStateBuilder,
};
use ic_test_utilities_types::{
    ids::{canister_test_id, message_test_id, subnet_test_id, user_test_id},
    messages::{IngressBuilder, RequestBuilder},
};
use ic_types::{
    batch::CanisterCyclesCostSchedule,
    ingress::{IngressState, IngressStatus, WasmResult},
    messages::{CallbackId, CanisterCall, StopCanisterCallId, StopCanisterContext, NO_DEADLINE},
    nominal_cycles::NominalCycles,
    time::UNIX_EPOCH,
    CanisterId, CanisterTimer, ComputeAllocation, Cycles, MemoryAllocation, NumBytes,
    NumInstructions, SubnetId, UserId,
};
use ic_universal_canister::PayloadBuilder;
use ic_wasm_types::CanisterModule;
use lazy_static::lazy_static;
use maplit::{btreemap, btreeset};
use more_asserts::{assert_ge, assert_gt, assert_le, assert_lt};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    convert::TryFrom,
    mem::size_of,
    path::Path,
    sync::Arc,
};

use super::InstallCodeResult;
use prometheus::IntCounter;

const CANISTER_FREEZE_BALANCE_RESERVE: Cycles = Cycles::new(5_000_000_000_000);
const MAX_NUM_INSTRUCTIONS: NumInstructions = NumInstructions::new(5_000_000_000);
const DEFAULT_PROVISIONAL_BALANCE: Cycles = Cycles::new(100_000_000_000_000);
const MEMORY_CAPACITY: NumBytes = NumBytes::new(8 * 1024 * 1024 * 1024); // 8GiB
const MAX_CONTROLLERS: usize = 10;
const WASM_PAGE_SIZE_IN_BYTES: u64 = 64 * 1024; // 64KiB
const MAX_NUMBER_OF_CANISTERS: u64 = 0;
// The simplest valid Wasm binary: "(module)"
const MINIMAL_WASM: [u8; 8] = [
    0, 97, 115, 109, // \0ASM - magic
    1, 0, 0, 0, //  0x01 - version
];

const SUBNET_MEMORY_CAPACITY: i64 = i64::MAX / 2;

// Ensure the slice, with extra room for Candid encoding, fits within 2 MiB.
#[test]
fn test_slice() {
    let slice = vec![42; MAX_SLICE_SIZE_BYTES as usize];
    #[derive(Deserialize, CandidType)]
    struct S {
        #[serde(with = "serde_bytes")]
        x: Vec<u8>,
    }
    let x = S { x: slice };
    let encoded = Encode!(&x).unwrap();
    assert_le!(encoded.len(), 2 * 1024 * 1024);
}

lazy_static! {
    static ref MAX_SUBNET_AVAILABLE_MEMORY: SubnetAvailableMemory = SubnetAvailableMemory::new(
        SUBNET_MEMORY_CAPACITY,
        SUBNET_MEMORY_CAPACITY,
        SUBNET_MEMORY_CAPACITY
    );
    static ref INITIAL_CYCLES: Cycles =
        CANISTER_FREEZE_BALANCE_RESERVE + Cycles::new(5_000_000_000_000);
    static ref EXECUTION_PARAMETERS: ExecutionParameters = ExecutionParameters {
        instruction_limits: InstructionLimits::new(
            FlagStatus::Disabled,
            MAX_NUM_INSTRUCTIONS,
            MAX_NUM_INSTRUCTIONS
        ),
        wasm_memory_limit: None,
        memory_allocation: MemoryAllocation::default(),
        canister_guaranteed_callback_quota: CANISTER_GUARANTEED_CALLBACK_QUOTA as u64,
        compute_allocation: ComputeAllocation::default(),
        subnet_type: SubnetType::Application,
        execution_mode: ExecutionMode::Replicated,
        subnet_memory_saturation: ResourceSaturation::default(),
    };
    static ref DROP_MEMORY_GROW_CONST_COST: u64 =
        instruction_to_cost(&wasmparser::Operator::Drop, WasmMemoryType::Wasm32)
            + instruction_to_cost(
                &wasmparser::Operator::MemoryGrow { mem: 0 },
                WasmMemoryType::Wasm32
            )
            + instruction_to_cost(
                &wasmparser::Operator::I32Const { value: 0 },
                WasmMemoryType::Wasm32
            );
    static ref UNREACHABLE_COST: u64 =
        instruction_to_cost(&wasmparser::Operator::Unreachable, WasmMemoryType::Wasm32);
}

fn canister_change_origin_from_canister(sender: &CanisterId) -> CanisterChangeOrigin {
    CanisterChangeOrigin::from_canister(sender.get(), None)
}

fn canister_change_origin_from_principal(sender: &PrincipalId) -> CanisterChangeOrigin {
    if sender.as_slice().last() == Some(&0x01) {
        CanisterChangeOrigin::from_canister(*sender, None)
    } else {
        CanisterChangeOrigin::from_user(*sender)
    }
}

fn no_op_counter() -> IntCounter {
    IntCounter::new("no_op", "no_op").unwrap()
}

pub struct InstallCodeContextBuilder {
    ctx: InstallCodeContext,
}

impl InstallCodeContextBuilder {
    pub fn sender(mut self, sender: PrincipalId) -> Self {
        self.ctx.origin = canister_change_origin_from_principal(&sender);
        self
    }

    pub fn canister_id(mut self, canister_id: CanisterId) -> Self {
        self.ctx.canister_id = canister_id;
        self
    }

    #[allow(dead_code)]
    pub fn arg(mut self, arg: Vec<u8>) -> Self {
        self.ctx.arg = arg;
        self
    }

    pub fn mode(mut self, mode: CanisterInstallModeV2) -> Self {
        self.ctx.mode = mode;
        self
    }

    pub fn build(&self) -> InstallCodeContext {
        self.ctx.clone()
    }
}

impl Default for InstallCodeContextBuilder {
    fn default() -> Self {
        Self {
            ctx: InstallCodeContext {
                origin: canister_change_origin_from_principal(&PrincipalId::new_user_test_id(0)),
                canister_id: canister_test_id(0),
                wasm_source: WasmSource::CanisterModule(CanisterModule::new(
                    wat::parse_str(EMPTY_WAT).unwrap(),
                )),
                arg: vec![],
                mode: CanisterInstallModeV2::Install,
            },
        }
    }
}

struct CanisterManagerBuilder {
    cycles_account_manager: CyclesAccountManager,
    subnet_id: SubnetId,
    rate_limiting_of_instructions: FlagStatus,
}

impl CanisterManagerBuilder {
    fn with_subnet_id(mut self, subnet_id: SubnetId) -> Self {
        self.subnet_id = subnet_id;
        self
    }

    fn with_cycles_account_manager(mut self, cycles_account_manager: CyclesAccountManager) -> Self {
        self.cycles_account_manager = cycles_account_manager;
        self
    }

    fn build(self) -> CanisterManager {
        let subnet_type = SubnetType::Application;
        let metrics_registry = MetricsRegistry::new();
        let state_reader = Arc::new(FakeStateManager::new());
        let (completed_execution_messages_tx, _) = tokio::sync::mpsc::channel(1);
        let ingress_history_writer = Arc::new(IngressHistoryWriterImpl::new(
            Config::default(),
            no_op_logger(),
            &metrics_registry,
            completed_execution_messages_tx,
            state_reader,
        ));
        let cycles_account_manager = Arc::new(self.cycles_account_manager);
        let hypervisor = Hypervisor::new(
            Config::default(),
            &metrics_registry,
            self.subnet_id,
            no_op_logger(),
            Arc::clone(&cycles_account_manager),
            SchedulerConfig::application_subnet().dirty_page_overhead,
            Arc::new(TestPageAllocatorFileDescriptorImpl::new()),
            Arc::new(FakeStateManager::new()),
            Path::new("/tmp"),
        );
        let hypervisor = Arc::new(hypervisor);
        CanisterManager::new(
            hypervisor,
            no_op_logger(),
            canister_manager_config(
                self.subnet_id,
                subnet_type,
                self.rate_limiting_of_instructions,
            ),
            cycles_account_manager,
            ingress_history_writer,
            Arc::new(TestPageAllocatorFileDescriptorImpl),
            FlagStatus::Disabled,
        )
    }
}

impl Default for CanisterManagerBuilder {
    fn default() -> Self {
        Self {
            cycles_account_manager: CyclesAccountManagerBuilder::new().build(),
            subnet_id: subnet_test_id(1),
            rate_limiting_of_instructions: FlagStatus::Disabled,
        }
    }
}

fn canister_manager_config(
    subnet_id: SubnetId,
    subnet_type: SubnetType,
    rate_limiting_of_instructions: FlagStatus,
) -> CanisterMgrConfig {
    CanisterMgrConfig::new(
        MEMORY_CAPACITY,
        DEFAULT_PROVISIONAL_BALANCE,
        NumSeconds::from(100_000),
        subnet_id,
        subnet_type,
        MAX_CONTROLLERS,
        // Compute capacity for 2-core scheduler is 100%
        // TODO(RUN-319): the capacity should be defined based on actual `scheduler_cores`
        100,
        rate_limiting_of_instructions,
        100,
        FlagStatus::Enabled,
        // 10 MiB should be enough for all the tests.
        NumBytes::from(10 * 1024 * 1024),
        SchedulerConfig::application_subnet().upload_wasm_chunk_instructions,
        ic_config::embedders::Config::default().wasm_max_size,
        SchedulerConfig::application_subnet().canister_snapshot_baseline_instructions,
        SchedulerConfig::application_subnet().canister_snapshot_data_baseline_instructions,
        DEFAULT_WASM_MEMORY_LIMIT,
        MAX_NUMBER_OF_SNAPSHOTS_PER_CANISTER,
        MAX_ENVIRONMENT_VARIABLES,
        MAX_ENVIRONMENT_VARIABLE_NAME_LENGTH,
        MAX_ENVIRONMENT_VARIABLE_VALUE_LENGTH,
    )
}

fn initial_state(subnet_id: SubnetId, use_specified_ids_routing_table: bool) -> ReplicatedState {
    let mut state = ReplicatedState::new(subnet_id, SubnetType::Application);

    state.metadata.network_topology.routing_table = if use_specified_ids_routing_table {
        let routing_table =
            get_routing_table_with_specified_ids_allocation_range(subnet_id).unwrap();
        Arc::new(routing_table)
    } else {
        Arc::new(
            RoutingTable::try_from(btreemap! {
                CanisterIdRange{ start: CanisterId::from(0), end: CanisterId::from(CANISTER_IDS_PER_SUBNET - 1) } => subnet_id,
            })
            .unwrap(),
        )
    };

    state.metadata.network_topology.nns_subnet_id = subnet_id;
    state.metadata.init_allocation_ranges_if_empty().unwrap();
    state
}

fn install_code(
    canister_manager: &CanisterManager,
    context: InstallCodeContext,
    state: &mut ReplicatedState,
    round_limits: &mut RoundLimits,
) -> (
    NumInstructions,
    Result<InstallCodeResult, CanisterManagerError>,
    Option<CanisterState>,
) {
    let instruction_limit = NumInstructions::new(round_limits.instructions.get() as u64);
    let mut execution_parameters = ExecutionParameters {
        instruction_limits: InstructionLimits::new(
            FlagStatus::Disabled,
            instruction_limit,
            instruction_limit,
        ),
        ..EXECUTION_PARAMETERS.clone()
    };

    let args = InstallCodeArgsV2::new(
        context.mode,
        context.canister_id,
        context.wasm_source.unwrap_as_slice_for_testing().into(),
        context.arg.clone(),
    );
    let ingress = IngressBuilder::new()
        .source(UserId::from(context.sender()))
        .receiver(CanisterId::ic_00())
        .method_name(Method::InstallCode)
        .method_payload(args.encode())
        .build();
    let no_op_counter: IntCounter = IntCounter::new("no_op", "no_op").unwrap();

    let round_counters = RoundCounters {
        execution_refund_error: &no_op_counter,
        state_changes_error: &no_op_counter,
        invalid_system_call_error: &no_op_counter,
        charging_from_balance_error: &no_op_counter,
        unexpected_response_error: &no_op_counter,
        response_cycles_refund_error: &no_op_counter,
        invalid_canister_state_error: &no_op_counter,
        ingress_with_cycles_error: &no_op_counter,
    };

    let time = state.time();
    let network_topology = state.metadata.network_topology.clone();

    let old_canister = state.take_canister_state(&context.canister_id).unwrap();
    execution_parameters.compute_allocation = old_canister.scheduler_state.compute_allocation;
    execution_parameters.memory_allocation = old_canister.memory_allocation();

    let dts_result = canister_manager.install_code_dts(
        context,
        CanisterCall::Ingress(Arc::new(ingress)),
        InstallCodeCallId::new(0),
        None,
        old_canister,
        time,
        "NOT_USED".into(),
        &network_topology,
        execution_parameters,
        round_limits,
        CompilationCostHandling::CountFullAmount,
        round_counters,
        SMALL_APP_SUBNET_MAX_SIZE,
        CanisterCyclesCostSchedule::Normal,
        Config::default().dirty_page_logging,
    );
    // Canister manager tests do not trigger DTS executions.
    let (result, instructions_used, canister) = match dts_result {
        DtsInstallCodeResult::Finished {
            mut canister,
            call_id: _,
            message: _,
            instructions_used,
            result,
        } => {
            canister.update_on_low_wasm_memory_hook_condition();
            (result, instructions_used, Some(canister))
        }
        DtsInstallCodeResult::Paused {
            canister: _,
            paused_execution,
            ingress_status: _,
        } => {
            unreachable!(
                "Unexpected paused execution in canister manager tests: {:?}",
                paused_execution
            );
        }
    };

    let instructions_left = instruction_limit - instructions_used.min(instruction_limit);
    (instructions_left, result, canister)
}

fn with_setup<F>(f: F)
where
    F: FnOnce(CanisterManager, ReplicatedState, SubnetId),
{
    let subnet_id = subnet_test_id(1);
    let canister_manager = CanisterManagerBuilder::default()
        .with_subnet_id(subnet_id)
        .build();
    f(canister_manager, initial_state(subnet_id, false), subnet_id)
}

#[test]
fn upgrade_non_existing_canister_fails() {
    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = canister_test_id(0);
    let err = test
        .upgrade_canister(canister_id, UNIVERSAL_CANISTER_WASM.to_vec())
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::CanisterNotFound);
    assert!(err
        .description()
        .contains(&format!("Canister {} not found", canister_id)));
}

#[test]
fn upgrade_canister_with_no_wasm_fails() {
    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = test.create_canister(*INITIAL_CYCLES);
    let err = test
        .upgrade_canister(canister_id, UNIVERSAL_CANISTER_WASM.to_vec())
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::CanisterWasmModuleNotFound);
    assert!(err
        .description()
        .contains("the canister contains no Wasm module"));
}

#[test]
fn install_canister_fails_if_memory_capacity_exceeded() {
    let initial_cycles = Cycles::new(1_000_000_000_000_000);
    let mb = 1 << 20;
    let memory_capacity = 1000 * mb;
    // canister1 is created with `memory_used` memory allocation;
    // => SubnetAvailableMemory decreases by `memory_used`
    // after canister1 is created and then SubnetAvailableMemory is equal to
    // `memory_capacity - (memory_used)`; we want this quantity to be `10 * mb`
    // and derive the value of `memory_used` from there.
    let memory_used = memory_capacity - 10 * mb;

    let wat = r#"
        (module
            (func (export "canister_init")
                (drop (memory.grow (i32.const 160)))
            )
            (memory 0)
        )"#;

    let mut test = ExecutionTestBuilder::new()
        .with_subnet_execution_memory(memory_capacity as i64)
        .with_subnet_memory_reservation(0)
        .build();

    let wasm = wat::parse_str(wat).unwrap();

    let _canister1 = test.create_canister_with_allocation(initial_cycles, None, Some(memory_used));
    let canister2 = test.create_canister(initial_cycles);

    // Try installing canister2, should fail due to insufficient memory capacity on the subnet.
    let err = test.install_canister(canister2, wasm).unwrap_err();
    err.assert_contains(
        ErrorCode::SubnetOversubscribed,
        "Canister requested 10.00 MiB of memory but only 10.00 MiB are available in the subnet.",
    );
    assert_eq!(
        test.canister_state(canister2).system_state.balance(),
        initial_cycles - test.canister_execution_cost(canister2)
    );
}

#[test]
fn install_code_preserves_messages() {
    let mut test = ExecutionTestBuilder::new().with_manual_execution().build();
    let canister_id = test.create_canister(*INITIAL_CYCLES);

    // Induct some messages without executing them (since manual execution mode
    // is set).
    let num_messages = 10;
    for _ in 0..num_messages {
        test.ingress_raw(canister_id, "foo", vec![]);
    }

    // Install the canister.
    test.install_code_v2(InstallCodeArgsV2::new(
        CanisterInstallModeV2::Install,
        canister_id,
        UNIVERSAL_CANISTER_WASM.to_vec(),
        vec![],
    ))
    .unwrap();

    // Ingress messages should still be in the canister's input queue.
    assert_eq!(
        test.canister_state(canister_id)
            .system_state
            .queues()
            .ingress_queue_size() as u64,
        num_messages
    );
}

#[test]
fn can_create_canister() {
    let mut test = ExecutionTestBuilder::new().build();

    let expected_generated_id1 = CanisterId::from(0);
    let expected_generated_id2 = CanisterId::from(1);

    let canister_id1 = test.create_canister(*INITIAL_CYCLES);
    assert_eq!(canister_id1, expected_generated_id1);

    let canister_id2 = test.create_canister(*INITIAL_CYCLES);
    assert_eq!(canister_id2, expected_generated_id2);

    assert_eq!(test.state().canister_states.len(), 2);
}

#[test]
fn create_canister_fails_if_not_enough_cycles_are_sent_with_the_request() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test
        .universal_canister_with_cycles(*INITIAL_CYCLES)
        .unwrap();

    let create_canister_args = CreateCanisterArgs {
        settings: None,
        sender_canister_version: None,
    }
    .encode();
    let payload = wasm()
        .call_simple(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(create_canister_args)
                .on_reject(wasm().reject_message().reject()),
        )
        .build();
    let result = test.ingress(canister_id, "update", payload).unwrap();

    match result {
        WasmResult::Reply(_) => panic!("expected reject"),
        WasmResult::Reject(msg) => {
            assert!(
                msg.contains("but only 0 cycles were received with the create_canister request")
            );
        }
    }
    assert_eq!(test.state().canister_states.len(), 1);
}

#[test]
fn can_create_canister_with_extra_cycles() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test
        .universal_canister_with_cycles(*INITIAL_CYCLES)
        .unwrap();

    let create_canister_args = CreateCanisterArgs {
        settings: None,
        sender_canister_version: None,
    }
    .encode();
    let cycles = Cycles::from(1_000_000_000_200u64);
    let payload = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(create_canister_args)
                .on_reply(wasm().message_payload().append_and_reply()),
            cycles,
        )
        .build();
    let result = test.ingress(canister_id, "update", payload);
    let _ = get_reply(result);
    assert_eq!(test.state().canister_states.len(), 2);
}

#[test]
fn cannot_install_non_empty_canister() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);
    test.install_code_v2(InstallCodeArgsV2::new(
        CanisterInstallModeV2::Install,
        canister_id,
        UNIVERSAL_CANISTER_WASM.to_vec(),
        vec![],
    ))
    .unwrap();

    let err = test
        .install_code_v2(InstallCodeArgsV2::new(
            CanisterInstallModeV2::Install,
            canister_id,
            UNIVERSAL_CANISTER_WASM.to_vec(),
            vec![],
        ))
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::CanisterNonEmpty);
    assert!(err.description().contains(&format!(
        "Canister {} cannot be installed because the canister is not empty",
        canister_id
    )));
}

#[test]
fn install_code_with_wrong_controller_fails() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test
        .universal_canister_with_cycles(*INITIAL_CYCLES)
        .unwrap();

    // Switch user id so the request comes from a non-controller.
    test.set_user_id(user_test_id(42));

    let err = test
        .install_code_v2(InstallCodeArgsV2::new(
            CanisterInstallModeV2::Upgrade(None),
            canister_id,
            UNIVERSAL_CANISTER_WASM.to_vec(),
            vec![],
        ))
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::CanisterInvalidController);
    assert!(err.description().contains(&format!(
        "Only the controllers of the canister {} can control it",
        canister_id
    )));
}

#[test]
fn create_canister_sets_correct_allocations() {
    let mut test = ExecutionTestBuilder::new().build();

    let compute_allocation = 50;
    let memory_allocation = 1024 * 1024 * 1024;
    let canister_id = test
        .create_canister_with_settings(
            Cycles::from(u64::MAX),
            CanisterSettingsArgsBuilder::new()
                .with_compute_allocation(compute_allocation)
                .with_memory_allocation(memory_allocation)
                .build(),
        )
        .unwrap();

    let canister_state = test.canister_state(canister_id);
    assert_eq!(
        canister_state.compute_allocation().as_percent(),
        compute_allocation
    );
    assert_eq!(
        canister_state.memory_allocation().bytes().get(),
        memory_allocation
    );
}

#[test]
fn create_canister_updates_consumed_cycles_metric_correctly() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test
        .universal_canister_with_cycles(*INITIAL_CYCLES * 2u64)
        .unwrap();

    let create_canister_args = CreateCanisterArgs {
        settings: None,
        sender_canister_version: None,
    }
    .encode();
    let payload = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(create_canister_args)
                .on_reply(wasm().message_payload().append_and_reply()),
            *INITIAL_CYCLES,
        )
        .build();

    test.ingress(canister_id, "update", payload).unwrap();

    let cycles_account_manager = Arc::new(CyclesAccountManagerBuilder::new().build());
    let creation_fee = cycles_account_manager.canister_creation_fee(
        SMALL_APP_SUBNET_MAX_SIZE,
        CanisterCyclesCostSchedule::Normal,
    );
    // There's only 2 canisters on the subnet, so the one created from the first one
    // with have the test id corresponding to `1`.
    let canister = test.canister_state(canister_test_id(1));
    assert_eq!(
        canister.system_state.canister_metrics.consumed_cycles.get(),
        creation_fee.get()
    );
    assert_eq!(
        canister
            .system_state
            .canister_metrics
            .get_consumed_cycles_by_use_cases()
            .get(&CyclesUseCase::CanisterCreation)
            .unwrap()
            .get(),
        creation_fee.get()
    );
    assert_eq!(
        canister.system_state.balance(),
        *INITIAL_CYCLES - creation_fee
    );
}

#[test]
fn create_canister_free() {
    let cost_schedule = CanisterCyclesCostSchedule::Free;
    let mut test = ExecutionTestBuilder::new()
        .with_cost_schedule(cost_schedule)
        .build();

    let canister_id = test
        .universal_canister_with_cycles(*INITIAL_CYCLES * 2u64)
        .unwrap();

    let create_canister_args = CreateCanisterArgs {
        settings: None,
        sender_canister_version: None,
    }
    .encode();
    let payload = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(create_canister_args)
                .on_reply(wasm().message_payload().append_and_reply()),
            *INITIAL_CYCLES,
        )
        .build();

    test.ingress(canister_id, "update", payload).unwrap();

    let cycles_account_manager = Arc::new(CyclesAccountManagerBuilder::new().build());
    let creation_fee =
        cycles_account_manager.canister_creation_fee(SMALL_APP_SUBNET_MAX_SIZE, cost_schedule);
    assert_eq!(creation_fee, Cycles::new(0));
    // There's only 2 canisters on the subnet, so the one created from the first one
    // with have the test id corresponding to `1`.
    let canister = test.canister_state(canister_test_id(1));
    assert_eq!(
        canister.system_state.canister_metrics.consumed_cycles.get(),
        0
    );
    assert_eq!(canister.system_state.balance(), *INITIAL_CYCLES);
}

#[test]
fn provisional_create_canister_has_no_creation_fee() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    let canister = test.canister_state(canister_id);
    assert_eq!(
        canister.system_state.canister_metrics.consumed_cycles,
        NominalCycles::default(),
    );
    assert_eq!(
        canister
            .system_state
            .canister_metrics
            .get_consumed_cycles_by_use_cases()
            .get(&CyclesUseCase::CanisterCreation),
        None
    );
    assert_eq!(canister.system_state.balance(), *INITIAL_CYCLES);
}

#[test]
fn reinstall_on_empty_canister_succeeds() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    let result = test.install_code_v2(InstallCodeArgsV2::new(
        CanisterInstallModeV2::Reinstall,
        canister_id,
        UNIVERSAL_CANISTER_WASM.to_vec(),
        vec![],
    ));

    let _ = get_reply(result);

    // Canister should still be in the replicated state.
    assert!(test.state().canister_state(&canister_id).is_some());
}

const COUNTER_WAT: &str = r#"
    (module
        (import "ic0" "msg_reply" (func $msg_reply))
        (import "ic0" "msg_reply_data_append"
            (func $msg_reply_data_append (param i32 i32)))
        (func $inc
            ;; Increment a counter.
            (i32.store
                (i32.const 0)
                (i32.add (i32.load (i32.const 0)) (i32.const 1))))
        (func $read
            (call $msg_reply_data_append
                (i32.const 0) ;; the counter from heap[0]
                (i32.const 4)) ;; length
            (call $msg_reply))
        (func $canister_init
            ;; Increment the counter by 41 in canister_init.
            (i32.store
                (i32.const 0)
                (i32.add (i32.load (i32.const 0)) (i32.const 41))))
        (start $inc)    ;; Increments counter by 1 in canister_start
        (memory $memory 1)
        (export "canister_query read" (func $read))
        (export "canister_init" (func $canister_init))
    )"#;

const EMPTY_WAT: &str = r#"(module (memory $memory 1 1000))"#;

#[test]
fn reinstall_calls_canister_start_and_canister_init() {
    let mut test = ExecutionTestBuilder::new().build();

    // install wasm module with no exported functions
    let id = test
        .canister_from_cycles_and_wat(*INITIAL_CYCLES, EMPTY_WAT)
        .unwrap();

    let wasm = wat::parse_str(COUNTER_WAT).unwrap();
    test.reinstall_canister(id, wasm).unwrap();
    // If canister_start and canister_init were called, then the counter
    // should be initialized to 42.
    let reply = test.ingress(id, "read", vec![]);
    assert_eq!(reply, Ok(WasmResult::Reply(vec![42, 0, 0, 0])));
}

#[test]
fn install_calls_canister_start_and_canister_init() {
    let mut test = ExecutionTestBuilder::new().build();

    let id = test
        .canister_from_cycles_and_wat(*INITIAL_CYCLES, COUNTER_WAT)
        .unwrap();

    // If canister_start and canister_init were called, then the counter
    // should be initialized to 42.
    let reply = test.ingress(id, "read", vec![]);
    assert_eq!(reply, Ok(WasmResult::Reply(vec![42, 0, 0, 0])));
}

#[test]
fn install_puts_canister_back_after_invalid_wasm() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    // Use an invalid wasm code (import memory from an invalid module).
    let wasm = wat::parse_str(r#"(module (import "foo" "memory" (memory (;0;) 529)))"#).unwrap();
    let err = test
        .install_code_v2(InstallCodeArgsV2::new(
            CanisterInstallModeV2::Install,
            canister_id,
            wasm.to_vec(),
            vec![],
        ))
        .unwrap_err();

    assert_eq!(err.code(), ErrorCode::CanisterInvalidWasm);
    assert!(err
        .description()
        .contains("Canister's Wasm module is not valid"));
    // Canister should still be in the replicated state.
    assert!(test.state().canister_state(&canister_id).is_some());
}

#[test]
fn reinstall_clears_stable_memory() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);
    test.install_code_v2(InstallCodeArgsV2::new(
        CanisterInstallModeV2::Install,
        canister_id,
        UNIVERSAL_CANISTER_WASM.to_vec(),
        vec![],
    ))
    .unwrap();

    let data = [1, 2, 3, 5, 8, 13];
    let payload = wasm()
        .stable_grow(1)
        .stable_write(42, &data)
        .reply()
        .build();
    test.ingress(canister_id, "update", payload).unwrap();

    assert_eq!(
        test.execution_state(canister_id).stable_memory.size.get(),
        1
    );

    // Reinstall the canister, should clear its stable memory.
    test.install_code_v2(InstallCodeArgsV2::new(
        CanisterInstallModeV2::Reinstall,
        canister_id,
        UNIVERSAL_CANISTER_WASM.to_vec(),
        vec![],
    ))
    .unwrap();

    assert_eq!(
        test.execution_state(canister_id).stable_memory.size.get(),
        0
    );
}

#[test]
fn stop_a_running_canister() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    // When created, a canister is initially running.
    assert_eq!(
        test.canister_state(canister_id).status(),
        CanisterStatusType::Running
    );

    let message_id = test.stop_canister(canister_id);

    // Canister should now have the "stopping" status with empty call contexts.
    assert_eq!(
        test.canister_state(canister_id).system_state.get_status(),
        &CanisterStatus::Stopping {
            stop_contexts: vec![StopCanisterContext::Ingress {
                sender: test.user_id(),
                message_id,
                call_id: Some(StopCanisterCallId::new(0)),
            }],
            call_context_manager: CallContextManager::default(),
        }
    );

    // It should also be ready to stop.
    assert!(test
        .canister_state(canister_id)
        .system_state
        .ready_to_stop());
}

#[test]
fn stop_a_stopped_canister() {
    with_setup(|canister_manager, mut state, _| {
        let sender = user_test_id(1);
        let canister_id = canister_test_id(0);
        let canister = get_stopped_canister(canister_id);
        state.put_canister_state(canister);

        // Canister should be stopped.
        assert_eq!(
            state.canister_state(&canister_id).unwrap().status(),
            CanisterStatusType::Stopped
        );

        let stop_context = StopCanisterContext::Ingress {
            sender,
            message_id: message_test_id(0),
            call_id: Some(StopCanisterCallId::new(0)),
        };
        assert_eq!(
            canister_manager.stop_canister(canister_id, stop_context, &mut state),
            StopCanisterResult::AlreadyStopped {
                cycles_to_return: Cycles::zero()
            }
        );

        // Canister should still be stopped.
        assert_eq!(
            state.canister_state(&canister_id).unwrap().status(),
            CanisterStatusType::Stopped
        );
    });
}

#[test]
fn stop_a_stopped_canister_from_another_canister() {
    with_setup(|canister_manager, mut state, _| {
        let controller = canister_test_id(1);
        let canister_id = canister_test_id(0);
        let canister = get_stopped_canister_with_controller(canister_id, controller.get());
        state.put_canister_state(canister);

        // Canister should be stopped.
        assert_eq!(
            state.canister_state(&canister_id).unwrap().status(),
            CanisterStatusType::Stopped
        );

        let cycles = 20u128;
        let stop_context = StopCanisterContext::Canister {
            sender: controller,
            reply_callback: CallbackId::from(0),
            call_id: Some(StopCanisterCallId::new(0)),
            cycles: Cycles::from(cycles),
            deadline: NO_DEADLINE,
        };
        assert_eq!(
            canister_manager.stop_canister(canister_id, stop_context, &mut state),
            StopCanisterResult::AlreadyStopped {
                cycles_to_return: Cycles::from(cycles)
            }
        );

        // Canister should still be stopped.
        assert_eq!(
            state.canister_state(&canister_id).unwrap().status(),
            CanisterStatusType::Stopped
        );
    });
}

#[test]
fn stop_a_canister_with_incorrect_controller() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    // When created, a canister is initially running.
    assert_eq!(
        test.canister_state(canister_id).status(),
        CanisterStatusType::Running
    );

    // Switch the user so the stop request comes from a non-controller.
    test.set_user_id(user_test_id(42));

    let err = test
        .subnet_message(
            Method::StopCanister,
            CanisterIdRecord::from(canister_id).encode(),
        )
        .unwrap_err();

    assert_eq!(err.code(), ErrorCode::CanisterInvalidController);
    assert!(err.description().contains(&format!(
        "Only the controllers of the canister {} can control it",
        canister_id
    )));
    // Canister should still be running.
    assert_eq!(
        test.canister_state(canister_id).status(),
        CanisterStatusType::Running
    );
}

#[test]
fn stop_a_non_existing_canister() {
    let mut test = ExecutionTestBuilder::new().build();

    let non_existing_canister_id = canister_test_id(1);
    let err = test
        .subnet_message(
            Method::StopCanister,
            CanisterIdRecord::from(non_existing_canister_id).encode(),
        )
        .unwrap_err();

    assert_eq!(err.code(), ErrorCode::CanisterNotFound);
    assert!(err
        .description()
        .contains(&format!("Canister {} not found", non_existing_canister_id)));
}

#[test]
fn start_a_canister_with_incorrect_controller() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    // Switch the user so the start request comes from a non-controller.
    test.set_user_id(user_test_id(42));

    let err = test
        .subnet_message(
            Method::StartCanister,
            CanisterIdRecord::from(canister_id).encode(),
        )
        .unwrap_err();

    assert_eq!(err.code(), ErrorCode::CanisterInvalidController);
    assert!(err.description().contains(&format!(
        "Only the controllers of the canister {} can control it",
        canister_id
    )));
}

#[test]
fn starting_an_already_running_canister_keeps_it_running() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);
    // When created, a canister is initially running.
    assert_eq!(
        test.canister_state(canister_id).status(),
        CanisterStatusType::Running
    );

    // Start the canister. Since it's already running, the canister should
    // remain running.
    test.subnet_message(
        Method::StartCanister,
        CanisterIdRecord::from(canister_id).encode(),
    )
    .unwrap();
    assert_eq!(
        test.canister_state(canister_id).status(),
        CanisterStatusType::Running
    );
}

#[test]
fn start_a_stopped_canister_succeeds() {
    with_setup(|canister_manager, mut state, _| {
        let sender = user_test_id(1).get();
        let canister_id = canister_test_id(0);
        let canister = get_stopped_canister(canister_id);
        state.put_canister_state(canister);

        // Canister should be stopped.
        assert_eq!(
            state.canister_state(&canister_id).unwrap().status(),
            CanisterStatusType::Stopped
        );

        // Start the canister.
        let canister = state.canister_state_mut(&canister_id).unwrap();
        canister_manager.start_canister(sender, canister).unwrap();

        // Canister should now be running.
        assert_eq!(
            state.canister_state(&canister_id).unwrap().status(),
            CanisterStatusType::Running
        );
    });
}

#[test]
fn start_a_stopping_canister_with_no_stop_contexts() {
    with_setup(|canister_manager, mut state, _| {
        let sender = user_test_id(1).get();
        let canister_id = canister_test_id(0);
        let canister = get_stopping_canister(canister_id);

        state.put_canister_state(canister);

        let canister = state.canister_state_mut(&canister_id).unwrap();
        assert_eq!(
            canister_manager.start_canister(sender, canister),
            Ok(Vec::new())
        );
    });
}

#[test]
fn start_a_stopping_canister_with_stop_contexts() {
    with_setup(|canister_manager, mut state, _| {
        let sender = user_test_id(1).get();
        let canister_id = canister_test_id(0);
        let mut canister = get_stopping_canister(canister_id);
        let stop_context = StopCanisterContext::Ingress {
            sender: user_test_id(1),
            message_id: message_test_id(0),
            call_id: Some(StopCanisterCallId::new(0)),
        };
        canister.system_state.add_stop_context(stop_context.clone());

        state.put_canister_state(canister);

        let canister = state.canister_state_mut(&canister_id).unwrap();
        assert_eq!(
            canister_manager.start_canister(sender, canister),
            Ok(vec![stop_context])
        );
    });
}

#[test]
fn get_canister_status_with_incorrect_controller() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    // Switch the user so the canister_status request comes from a non-controller.
    test.set_user_id(user_test_id(42));

    let err = test.canister_status(canister_id).unwrap_err();

    assert_eq!(err.code(), ErrorCode::CanisterInvalidController);
    assert!(err.description().contains(&format!(
        "Only the controllers of the canister {} can control it",
        canister_id
    )));
}

#[test]
fn get_canister_status_of_running_canister() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);
    let result = test.canister_status(canister_id);

    let reply = get_reply(result);
    let status = Decode!(&reply, CanisterStatusResultV2).unwrap();
    assert_eq!(status.status(), CanisterStatusType::Running);
}

#[test]
fn get_canister_status_of_self() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test
        .universal_canister_with_cycles(*INITIAL_CYCLES)
        .unwrap();

    let payload = wasm()
        .call_simple(
            CanisterId::ic_00(),
            Method::CanisterStatus,
            call_args()
                .other_side(CanisterIdRecord::from(canister_id).encode())
                .on_reply(wasm().message_payload().append_and_reply()),
        )
        .build();

    let result = test.ingress(canister_id, "update", payload);

    let reply = get_reply(result);
    let status = Decode!(&reply, CanisterStatusResultV2).unwrap();
    assert_eq!(status.status(), CanisterStatusType::Running);
}

#[test]
fn get_canister_status_of_stopped_canister() {
    with_setup(|canister_manager, mut state, _| {
        let sender = user_test_id(1).get();
        let canister_id = canister_test_id(0);
        let canister = get_stopped_canister(canister_id);
        state.put_canister_state(canister);

        let canister = state.canister_state_mut(&canister_id).unwrap();
        let status = canister_manager
            .get_canister_status(
                sender,
                canister,
                SMALL_APP_SUBNET_MAX_SIZE,
                CanisterCyclesCostSchedule::Normal,
            )
            .unwrap()
            .status();
        assert_eq!(status, CanisterStatusType::Stopped);
    });
}

#[test]
fn get_canister_status_of_stopping_canister() {
    with_setup(|canister_manager, mut state, _| {
        let sender = user_test_id(1).get();
        let canister_id = canister_test_id(0);
        let canister = get_stopping_canister(canister_id);
        state.put_canister_state(canister);

        let canister = state.canister_state_mut(&canister_id).unwrap();
        let status = canister_manager
            .get_canister_status(
                sender,
                canister,
                SMALL_APP_SUBNET_MAX_SIZE,
                CanisterCyclesCostSchedule::Normal,
            )
            .unwrap()
            .status();
        assert_eq!(status, CanisterStatusType::Stopping);
    });
}

#[test]
fn canister_status_with_environment_variables() {
    let mut test = ExecutionTestBuilder::new()
        .with_environment_variables_flag(FlagStatus::Enabled)
        .build();
    let environment_variables = btreemap![
        "TEST_VAR".to_string() => "test_value".to_string(),
        "TEST_VAR2".to_string() => "test_value2".to_string(),
    ];

    let expected_env_vars = environment_variables
        .into_iter()
        .map(|(name, value)| EnvironmentVariable { name, value })
        .collect::<Vec<_>>();
    let settings = CanisterSettingsArgsBuilder::new()
        .with_environment_variables(expected_env_vars.clone())
        .build();
    let canister_id = test
        .create_canister_with_settings(*INITIAL_CYCLES, settings)
        .unwrap();
    let status = test.canister_status(canister_id);
    let reply = get_reply(status);
    let status = Decode!(&reply, CanisterStatusResultV2).unwrap();
    assert_eq!(
        status.settings().environment_variables(),
        &expected_env_vars
    );
}

#[test]
fn set_controller_with_incorrect_controller() {
    let mut test = ExecutionTestBuilder::new().build();

    let controller = test.user_id();
    let canister_id = test.create_canister(*INITIAL_CYCLES);

    // Switch the user to attempt to set controllers with a non-controller.
    test.set_user_id(user_test_id(42));

    let err = test
        .set_controller(canister_id, user_test_id(1).get())
        .unwrap_err();

    assert_eq!(err.code(), ErrorCode::CanisterInvalidController);
    assert!(err.description().contains(&format!(
        "Only the controllers of the canister {} can control it",
        canister_id
    )));
    // List of controllers should not have changed.
    assert_eq!(
        test.canister_state(canister_id).controllers(),
        &btreeset! {controller.get()}
    );
}

#[test]
fn set_controller_with_correct_controller() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    let new_controller = user_test_id(10).get();
    test.set_controller(canister_id, new_controller).unwrap();

    // List of controllers should have been updated to the new_controller.
    assert_eq!(
        test.canister_state(canister_id).controllers(),
        &btreeset! {new_controller}
    );
}

#[test]
fn delete_non_existing_canister_fails() {
    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = canister_test_id(0);

    let err = test.delete_canister(canister_id).unwrap_err();
    assert_eq!(err.code(), ErrorCode::CanisterNotFound);
    assert!(err
        .description()
        .contains(&format!("Canister {} not found", canister_id)));
}

#[test]
fn delete_canister_with_incorrect_controller_fails() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    // Switch the user to attempt to delete the canister with a non-controller.
    test.set_user_id(user_test_id(42));

    let err = test.delete_canister(canister_id).unwrap_err();

    assert_eq!(err.code(), ErrorCode::CanisterInvalidController);
    assert!(err.description().contains(&format!(
        "Only the controllers of the canister {} can control it",
        canister_id
    )));
    // Canister should still be there.
    assert!(test.state().canister_state(&canister_id).is_some());
}

#[test]
fn delete_running_canister_fails() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    let err = test.delete_canister(canister_id).unwrap_err();

    assert_eq!(err.code(), ErrorCode::CanisterNotStopped);
    assert!(err.description().contains(&format!(
        "Canister {} must be stopped before it is deleted",
        canister_id
    ),));
    // Canister should still be there.
    assert!(test.state().canister_state(&canister_id).is_some());
}

#[test]
fn delete_stopping_canister_fails() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    let _ = test.stop_canister(canister_id);
    assert_eq!(
        test.canister_state(canister_id).status(),
        CanisterStatusType::Stopping
    );

    let err = test.delete_canister(canister_id).unwrap_err();

    assert_eq!(err.code(), ErrorCode::CanisterNotStopped);
    assert!(err.description().contains(&format!(
        "Canister {} must be stopped before it is deleted",
        canister_id
    ),));
    // Canister should still be there.
    assert!(test.state().canister_state(&canister_id).is_some());
}

#[test]
fn delete_stopped_canister_succeeds() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    let _ = test.stop_canister(canister_id);
    test.process_stopping_canisters();
    assert_eq!(
        test.canister_state(canister_id).status(),
        CanisterStatusType::Stopped
    );

    test.delete_canister(canister_id).unwrap();
    // Canister should no longer be there.
    assert!(test.state().canister_state(&canister_id).is_none());
}

#[test]
fn delete_canister_consumed_cycles_observed() {
    let mut test = ExecutionTestBuilder::new().build();

    let initial_cycles = Cycles::new(5_000_000_000_000);
    let canister_id = test.create_canister(initial_cycles);

    // Stop and delete the canister.
    test.stop_canister(canister_id);
    test.process_stopping_canisters();
    assert_eq!(
        test.canister_state(canister_id).status(),
        CanisterStatusType::Stopped
    );
    test.delete_canister(canister_id).unwrap();

    // Canister should no longer be there.
    assert!(test.state().canister_state(&canister_id).is_none());
    // Check that the consumed cycles have been updated correctly.
    assert_eq!(
        *test
            .state()
            .metadata
            .subnet_metrics
            .get_consumed_cycles_by_use_case()
            .get(&CyclesUseCase::DeletedCanisters)
            .unwrap(),
        NominalCycles::from(initial_cycles)
    );
}

#[test]
fn deposit_cycles_succeeds_with_enough_cycles() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);
    let deposit_canister = test.universal_canister().unwrap();

    let cycles_balance_before = test.canister_state(canister_id).system_state.balance();

    let cycles_to_deposit = Cycles::new(100);
    let deposit_cycles_args = CanisterIdRecord::from(canister_id).encode();
    let payload = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::DepositCycles,
            call_args()
                .other_side(deposit_cycles_args)
                .on_reply(wasm().message_payload().append_and_reply()),
            cycles_to_deposit,
        )
        .build();

    test.ingress(deposit_canister, "update", payload).unwrap();

    assert_eq!(
        test.canister_state(canister_id).system_state.balance(),
        cycles_balance_before + cycles_to_deposit
    );
}

#[test]
fn create_canister_with_cycles_sender_in_whitelist() {
    let subnet_id = subnet_test_id(1);
    let subnet_type = SubnetType::Application;
    let cycles_account_manager = CyclesAccountManagerBuilder::new()
        .with_subnet_type(subnet_type)
        .build();

    let canister_manager = CanisterManagerBuilder::default()
        .with_subnet_id(subnet_id)
        .with_cycles_account_manager(cycles_account_manager)
        .build();

    let mut state = initial_state(subnet_id, false);
    let mut round_limits = RoundLimits {
        instructions: as_round_instructions(EXECUTION_PARAMETERS.instruction_limits.message()),
        subnet_available_memory: (*MAX_SUBNET_AVAILABLE_MEMORY),
        subnet_available_callbacks: SUBNET_CALLBACK_SOFT_LIMIT as i64,
        compute_allocation_used: state.total_compute_allocation(),
    };
    let sender = canister_test_id(1).get();
    let canister_id = canister_manager
        .create_canister_with_cycles(
            canister_change_origin_from_principal(&sender),
            Some(123),
            CanisterSettings::default(),
            None,
            &mut state,
            &ProvisionalWhitelist::Set(btreeset! { canister_test_id(1).get() }),
            MAX_NUMBER_OF_CANISTERS,
            &mut round_limits,
            ResourceSaturation::default(),
            SMALL_APP_SUBNET_MAX_SIZE,
            &no_op_counter(),
        )
        .unwrap();

    let canister = state.take_canister_state(&canister_id).unwrap();

    // Verify cycles are set as expected.
    assert_eq!(canister.system_state.balance(), Cycles::new(123));
}

fn create_canister_with_specified_id(
    specified_id: PrincipalId,
) -> (Result<CanisterId, CanisterManagerError>, ReplicatedState) {
    let subnet_id = subnet_test_id(1);
    let canister_manager = CanisterManagerBuilder::default()
        .with_subnet_id(subnet_id)
        .build();

    let mut state = initial_state(subnet_id, true);
    let mut round_limits = RoundLimits {
        instructions: as_round_instructions(EXECUTION_PARAMETERS.instruction_limits.message()),
        subnet_available_memory: (*MAX_SUBNET_AVAILABLE_MEMORY),
        subnet_available_callbacks: SUBNET_CALLBACK_SOFT_LIMIT as i64,
        compute_allocation_used: state.total_compute_allocation(),
    };

    let creator = canister_test_id(1).get();

    let creation_result = canister_manager.create_canister_with_cycles(
        canister_change_origin_from_principal(&creator),
        Some(123),
        CanisterSettings::default(),
        Some(specified_id),
        &mut state,
        &ProvisionalWhitelist::Set(btreeset! { canister_test_id(1).get() }),
        MAX_NUMBER_OF_CANISTERS,
        &mut round_limits,
        ResourceSaturation::default(),
        SMALL_APP_SUBNET_MAX_SIZE,
        &no_op_counter(),
    );

    (creation_result, state)
}

#[test]
fn create_canister_with_valid_specified_id_creator_in_whitelist() {
    let specified_id = CanisterId::from(u64::MAX / 4).get();

    let (creation_result, mut state) = create_canister_with_specified_id(specified_id);

    let canister_id = creation_result.unwrap();

    let canister = state.take_canister_state(&canister_id).unwrap();

    // Verify canister ID is set as expected.
    assert_eq!(canister.canister_id().get(), specified_id);
}

#[test]
fn create_canister_with_invalid_specified_id_creator_in_whitelist() {
    let specified_id = CanisterId::from(u64::MAX / 4 * 3).get();

    let creation_result = create_canister_with_specified_id(specified_id).0;

    assert_matches!(
        creation_result,
        Err(CanisterManagerError::CanisterNotHostedBySubnet { .. })
    );
}

#[test]
fn can_get_canister_balance() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    let result = test.canister_status(canister_id);

    let reply = get_reply(result);
    let status = Decode!(&reply, CanisterStatusResultV2).unwrap();
    assert_eq!(status.cycles(), INITIAL_CYCLES.get());
}

#[test]
fn add_cycles_sender_in_whitelist() {
    let subnet_id = subnet_test_id(1);
    let subnet_type = SubnetType::Application;
    let cycles_account_manager = CyclesAccountManagerBuilder::new()
        .with_subnet_type(subnet_type)
        .build();

    let canister_manager = CanisterManagerBuilder::default()
        .with_subnet_id(subnet_id)
        .with_cycles_account_manager(cycles_account_manager)
        .build();

    let canister_id = canister_test_id(0);
    let canister = get_running_canister(canister_id);
    let sender = canister_test_id(1).get();

    let mut state = initial_state(subnet_id, false);
    let initial_cycles = canister.system_state.balance();
    state.put_canister_state(canister);

    let canister = state.canister_state_mut(&canister_id).unwrap();
    canister_manager
        .add_cycles(
            sender,
            Some(123),
            canister,
            &ProvisionalWhitelist::Set(btreeset! { canister_test_id(1).get() }),
        )
        .unwrap();

    // Verify cycles are set as expected.
    let canister = state.take_canister_state(&canister_id).unwrap();
    assert_eq!(
        canister.system_state.balance(),
        initial_cycles + Cycles::new(123),
    );
}

#[test]
fn add_cycles_sender_not_in_whitelist() {
    with_setup(|canister_manager, mut state, _| {
        let canister_id = canister_test_id(0);
        let canister = get_running_canister(canister_id);
        let sender = canister_test_id(1).get();

        state.put_canister_state(canister);

        // By default, the `CanisterManager`'s whitelist is set to `None`.
        // A call to `add_cycles` should fail.
        let canister = state.canister_state_mut(&canister_id).unwrap();
        assert_eq!(
            canister_manager.add_cycles(
                sender,
                Some(123),
                canister,
                &ProvisionalWhitelist::Set(BTreeSet::new()),
            ),
            Err(CanisterManagerError::SenderNotInWhitelist(sender))
        );
    });
}

#[test]
fn upgrading_canister_fails_if_memory_capacity_exceeded() {
    let initial_cycles = Cycles::new(1_000_000_000_000_000);
    let mb = 1 << 20;
    let memory_capacity = 1000 * mb;
    let memory_used = memory_capacity - 10 * mb;

    let wat = r#"
        (module
            (import "ic0" "stable64_grow" (func $stable64_grow (param i64) (result i64)))
            (func (export "canister_pre_upgrade")
                (drop (call $stable64_grow (i64.const 80)))
            )
            (func (export "canister_post_upgrade")
                (drop (call $stable64_grow (i64.const 80)))
            )
            (memory 0)
        )"#;

    let mut test = ExecutionTestBuilder::new()
        .with_subnet_execution_memory(memory_capacity as i64)
        .with_subnet_memory_reservation(0)
        .build();

    let wasm = wat::parse_str(wat).unwrap();

    let _canister1 = test.create_canister_with_allocation(initial_cycles, None, Some(memory_used));
    let canister2 = test.create_canister(initial_cycles);

    test.install_canister(canister2, wasm.clone()).unwrap();

    let cycles_before = test.canister_state(canister2).system_state.balance();
    let execution_cost_before = test.canister_execution_cost(canister2);

    // Try upgrading the canister, should fail because there is not enough memory capacity
    // on the subnet.
    test.upgrade_canister(canister2, wasm)
        .unwrap_err()
        .assert_contains(
            ErrorCode::SubnetOversubscribed,
            "Canister requested 10.00 MiB of memory but only 10.00 MiB are available \
            in the subnet.",
        );

    assert_eq!(
        test.canister_state(canister2).system_state.balance(),
        cycles_before - (test.canister_execution_cost(canister2) - execution_cost_before)
    );
}

#[test]
fn installing_a_canister_with_not_enough_cycles_fails() {
    let mut test = ExecutionTestBuilder::new().build();

    // Give the new canister a relatively small number of cycles so it doesn't have
    // enough to be installed.
    let canister_id = test.create_canister(Cycles::new(100));

    let err = test
        .install_code_v2(InstallCodeArgsV2::new(
            CanisterInstallModeV2::Install,
            canister_id,
            UNIVERSAL_CANISTER_WASM.to_vec(),
            vec![],
        ))
        .unwrap_err();

    assert_eq!(err.code(), ErrorCode::CanisterOutOfCycles);
    assert!(err.description().contains(&format!(
        "Canister installation failed with `Canister {} is out of cycles",
        canister_id
    )));
}

#[test]
fn uninstall_canister_doesnt_respond_to_responded_call_contexts() {
    assert_eq!(
        uninstall_canister(
            &no_op_logger(),
            &mut CanisterStateBuilder::new()
                .with_call_context(CallContextBuilder::new().with_responded(true).build())
                .build(),
            UNIX_EPOCH,
            AddCanisterChangeToHistory::No,
            Arc::new(TestPageAllocatorFileDescriptorImpl),
        ),
        Vec::new()
    );
}

#[test]
fn uninstall_canister_responds_to_unresponded_call_contexts() {
    assert_eq!(
        uninstall_canister(
            &no_op_logger(),
            &mut CanisterStateBuilder::new()
                .with_canister_id(canister_test_id(789))
                .with_call_context(
                    CallContextBuilder::new()
                        .with_call_origin(CallOrigin::Ingress(
                            user_test_id(123),
                            message_test_id(456)
                        ))
                        .with_responded(false)
                        .build()
                )
                .build(),
            UNIX_EPOCH,
            AddCanisterChangeToHistory::No,
            Arc::new(TestPageAllocatorFileDescriptorImpl),
        )[0],
        Response::Ingress(IngressResponse {
            message_id: message_test_id(456),
            status: IngressStatus::Known {
                receiver: canister_test_id(789).get(),
                user_id: user_test_id(123),
                time: UNIX_EPOCH,
                state: IngressState::Failed(UserError::new(
                    ErrorCode::CanisterRejectedMessage,
                    "Canister has been uninstalled.",
                )),
            }
        })
    );
}

#[test]
fn failed_upgrade_hooks_consume_instructions() {
    fn run(
        initial_wasm: Vec<u8>,
        upgrade_wasm: Vec<u8>,
        fails_before_compiling_upgrade_wasm: bool,
    ) {
        let subnet_id = subnet_test_id(1);
        let subnet_type = SubnetType::Application;
        let cycles_account_manager = CyclesAccountManagerBuilder::new()
            .with_subnet_type(subnet_type)
            .build();

        let canister_manager = CanisterManagerBuilder::default()
            .with_subnet_id(subnet_id)
            .with_cycles_account_manager(cycles_account_manager)
            .build();

        let mut state = initial_state(subnet_id, false);
        let mut round_limits = RoundLimits {
            instructions: as_round_instructions(EXECUTION_PARAMETERS.instruction_limits.message()),
            subnet_available_memory: (*MAX_SUBNET_AVAILABLE_MEMORY),
            subnet_available_callbacks: SUBNET_CALLBACK_SOFT_LIMIT as i64,
            compute_allocation_used: state.total_compute_allocation(),
        };
        let sender = canister_test_id(100).get();
        let canister_id = canister_manager
            .create_canister(
                canister_change_origin_from_principal(&sender),
                subnet_id,
                *INITIAL_CYCLES,
                CanisterSettings::default(),
                MAX_NUMBER_OF_CANISTERS,
                &mut state,
                SMALL_APP_SUBNET_MAX_SIZE,
                &mut round_limits,
                ResourceSaturation::default(),
                &no_op_counter(),
            )
            .0
            .unwrap();

        let res = install_code(
            &canister_manager,
            InstallCodeContext {
                origin: canister_change_origin_from_principal(&sender),
                canister_id,
                wasm_source: WasmSource::CanisterModule(CanisterModule::new(initial_wasm)),
                arg: vec![],
                mode: CanisterInstallModeV2::Install,
            },
            &mut state,
            &mut round_limits,
        );
        assert!(res.1.is_ok());
        state.put_canister_state(res.2.unwrap());

        // reset instruction limit to investigate costs of just the following install
        let mut round_limits = RoundLimits {
            instructions: as_round_instructions(EXECUTION_PARAMETERS.instruction_limits.message()),
            subnet_available_memory: (*MAX_SUBNET_AVAILABLE_MEMORY),
            subnet_available_callbacks: SUBNET_CALLBACK_SOFT_LIMIT as i64,
            compute_allocation_used: state.total_compute_allocation(),
        };
        let compilation_cost = wasm_compilation_cost(&upgrade_wasm);
        let (instructions_left, result, _) = install_code(
            &canister_manager,
            InstallCodeContext {
                origin: canister_change_origin_from_principal(&sender),
                canister_id,
                wasm_source: WasmSource::CanisterModule(CanisterModule::new(upgrade_wasm)),
                arg: vec![],
                mode: CanisterInstallModeV2::Upgrade(None),
            },
            &mut state,
            &mut round_limits,
        );
        // Function + unreachable.
        let expected = NumInstructions::from(2)
            + if fails_before_compiling_upgrade_wasm {
                NumInstructions::new(0)
            } else {
                compilation_cost
            };
        assert_eq!(
            MAX_NUM_INSTRUCTIONS - instructions_left,
            expected,
            "initial instructions {} left {} diff {} expected {}",
            MAX_NUM_INSTRUCTIONS,
            instructions_left,
            MAX_NUM_INSTRUCTIONS - instructions_left,
            expected
        );
        assert_matches!(result, Err(CanisterManagerError::Hypervisor(_, _)));
    }

    let initial_wasm = r#"
    (module
        (func $canister_pre_upgrade
          unreachable
        )
        (memory $memory 1)
        (export "canister_pre_upgrade" (func $canister_pre_upgrade))
    )"#;
    let initial_wasm = wat::parse_str(initial_wasm).unwrap();
    let upgrade_wasm = r#"
    (module
        (memory $memory 1)
    )"#;
    let upgrade_wasm = wat::parse_str(upgrade_wasm).unwrap();
    run(initial_wasm, upgrade_wasm, true);

    let initial_wasm = r#"
    (module
        (memory $memory 1)
    )"#;
    let initial_wasm = wat::parse_str(initial_wasm).unwrap();
    let upgrade_wasm = r#"
    (module
        (func $canister_post_upgrade
          unreachable
        )
        (memory $memory 1)
        (export "canister_post_upgrade" (func $canister_post_upgrade))
    )"#;
    let upgrade_wasm = wat::parse_str(upgrade_wasm).unwrap();
    run(initial_wasm, upgrade_wasm, false);

    let initial_wasm = r#"
    (module
        (memory $memory 1)
    )"#;
    let initial_wasm = wat::parse_str(initial_wasm).unwrap();
    let upgrade_wasm = r#"
    (module
        (func $start
          unreachable
        )
        (memory $memory 1)
        (start $start)
    )"#;
    let upgrade_wasm = wat::parse_str(upgrade_wasm).unwrap();
    run(initial_wasm, upgrade_wasm, false);
}

#[test]
fn failed_install_hooks_consume_instructions() {
    fn run(wasm: Vec<u8>) {
        let subnet_id = subnet_test_id(1);
        let subnet_type = SubnetType::Application;
        let cycles_account_manager = CyclesAccountManagerBuilder::new()
            .with_subnet_type(subnet_type)
            .build();

        let canister_manager = CanisterManagerBuilder::default()
            .with_subnet_id(subnet_id)
            .with_cycles_account_manager(cycles_account_manager)
            .build();

        let mut state = initial_state(subnet_id, false);
        let mut round_limits = RoundLimits {
            instructions: as_round_instructions(EXECUTION_PARAMETERS.instruction_limits.message()),
            subnet_available_memory: (*MAX_SUBNET_AVAILABLE_MEMORY),
            subnet_available_callbacks: SUBNET_CALLBACK_SOFT_LIMIT as i64,
            compute_allocation_used: state.total_compute_allocation(),
        };
        let sender = canister_test_id(100).get();
        let canister_id = canister_manager
            .create_canister(
                canister_change_origin_from_principal(&sender),
                subnet_id,
                *INITIAL_CYCLES,
                CanisterSettings::default(),
                MAX_NUMBER_OF_CANISTERS,
                &mut state,
                SMALL_APP_SUBNET_MAX_SIZE,
                &mut round_limits,
                ResourceSaturation::default(),
                &no_op_counter(),
            )
            .0
            .unwrap();

        let compilation_cost = wasm_compilation_cost(&wasm);
        let (instructions_left, result, _) = install_code(
            &canister_manager,
            InstallCodeContext {
                origin: canister_change_origin_from_principal(&sender),
                canister_id,
                wasm_source: WasmSource::CanisterModule(CanisterModule::new(wasm)),
                arg: vec![],
                mode: CanisterInstallModeV2::Install,
            },
            &mut state,
            &mut round_limits,
        );
        assert_matches!(result, Err(CanisterManagerError::Hypervisor(_, _)));
        assert_eq!(
            MAX_NUM_INSTRUCTIONS - instructions_left,
            // Func + unreachable.
            NumInstructions::from(2) + compilation_cost,
            "initial instructions {} left {} diff {} expected {}",
            MAX_NUM_INSTRUCTIONS,
            instructions_left,
            MAX_NUM_INSTRUCTIONS - instructions_left,
            NumInstructions::from(1) + compilation_cost,
        );
    }

    let wasm = r#"
    (module
        (func $start
          unreachable
        )
        (memory $memory 1)
        (start $start)
    )"#;
    let wasm = wat::parse_str(wasm).unwrap();
    run(wasm);
    let wasm = r#"
    (module
        (func $canister_init
          unreachable
        )
        (memory $memory 1)
        (export "canister_init" (func $canister_init))
    )"#;
    let wasm = wat::parse_str(wasm).unwrap();
    run(wasm);
}

#[test]
fn install_code_respects_instruction_limit() {
    let subnet_id = subnet_test_id(1);
    let subnet_type = SubnetType::Application;
    let cycles_account_manager = CyclesAccountManagerBuilder::new()
        .with_subnet_type(subnet_type)
        .build();

    let canister_manager = CanisterManagerBuilder::default()
        .with_subnet_id(subnet_id)
        .with_cycles_account_manager(cycles_account_manager)
        .build();

    let mut state = initial_state(subnet_id, false);
    let mut round_limits = RoundLimits {
        instructions: as_round_instructions(EXECUTION_PARAMETERS.instruction_limits.message()),
        subnet_available_memory: (*MAX_SUBNET_AVAILABLE_MEMORY),
        subnet_available_callbacks: SUBNET_CALLBACK_SOFT_LIMIT as i64,
        compute_allocation_used: state.total_compute_allocation(),
    };
    let sender = canister_test_id(100).get();
    let canister_id = canister_manager
        .create_canister(
            canister_change_origin_from_principal(&sender),
            subnet_id,
            *INITIAL_CYCLES,
            CanisterSettings::default(),
            MAX_NUMBER_OF_CANISTERS,
            &mut state,
            SMALL_APP_SUBNET_MAX_SIZE,
            &mut round_limits,
            ResourceSaturation::default(),
            &no_op_counter(),
        )
        .0
        .unwrap();

    let wasm = r#"
    (module
        (func $start
          (i32.const 0)
          drop
        )
        (func $canister_init
          (i32.const 0)
          drop
        )
        (func $canister_pre_upgrade
          (i32.const 0)
          drop
        )
        (func $canister_post_upgrade
          (i32.const 0)
          drop
        )
        (memory $memory 1)
        (start $start)
        (export "canister_init" (func $canister_init))
        (export "canister_pre_upgrade" (func $canister_pre_upgrade))
        (export "canister_post_upgrade" (func $canister_post_upgrade))
    )"#;
    let compilation_cost = wat_compilation_cost(wasm);
    let wasm = wat::parse_str(wasm).unwrap();

    let instructions_limit = NumInstructions::from(3) + compilation_cost;

    // Too few instructions result in failed installation.
    let mut round_limits = RoundLimits {
        instructions: as_round_instructions(instructions_limit),
        subnet_available_memory: (*MAX_SUBNET_AVAILABLE_MEMORY),
        subnet_available_callbacks: SUBNET_CALLBACK_SOFT_LIMIT as i64,
        compute_allocation_used: state.total_compute_allocation(),
    };
    let (instructions_left, result, canister) = install_code(
        &canister_manager,
        InstallCodeContext {
            origin: canister_change_origin_from_principal(&sender),
            canister_id,
            wasm_source: WasmSource::CanisterModule(CanisterModule::new(wasm.clone())),
            arg: vec![],
            mode: CanisterInstallModeV2::Install,
        },
        &mut state,
        &mut round_limits,
    );
    state.put_canister_state(canister.unwrap());
    assert_matches!(
        result,
        Err(CanisterManagerError::Hypervisor(
            _,
            HypervisorError::InstructionLimitExceeded(instructions_limit_in_error)
        ))
        if instructions_limit == instructions_limit_in_error
    );
    assert_eq!(instructions_left, NumInstructions::from(0));

    // Enough instructions result in successful installation.
    let mut round_limits = RoundLimits {
        instructions: as_round_instructions(NumInstructions::from(6) + compilation_cost),
        // Function is 1 instruction.
        subnet_available_memory: (*MAX_SUBNET_AVAILABLE_MEMORY),
        subnet_available_callbacks: SUBNET_CALLBACK_SOFT_LIMIT as i64,
        compute_allocation_used: state.total_compute_allocation(),
    };
    let (instructions_left, result, canister) = install_code(
        &canister_manager,
        InstallCodeContext {
            origin: canister_change_origin_from_principal(&sender),
            canister_id,
            wasm_source: WasmSource::CanisterModule(CanisterModule::new(wasm.clone())),
            arg: vec![],
            mode: CanisterInstallModeV2::Install,
        },
        &mut state,
        &mut round_limits,
    );
    assert!(result.is_ok());
    assert_eq!(instructions_left, NumInstructions::from(0));
    state.put_canister_state(canister.unwrap());

    let instructions_limit = NumInstructions::from(5);

    // Too few instructions result in failed upgrade.
    let mut round_limits = RoundLimits {
        instructions: as_round_instructions(instructions_limit),
        subnet_available_memory: (*MAX_SUBNET_AVAILABLE_MEMORY),
        subnet_available_callbacks: SUBNET_CALLBACK_SOFT_LIMIT as i64,
        compute_allocation_used: state.total_compute_allocation(),
    };
    let (instructions_left, result, canister) = install_code(
        &canister_manager,
        InstallCodeContext {
            origin: canister_change_origin_from_principal(&sender),
            canister_id,
            wasm_source: WasmSource::CanisterModule(CanisterModule::new(wasm.clone())),
            arg: vec![],
            mode: CanisterInstallModeV2::Upgrade(None),
        },
        &mut state,
        &mut round_limits,
    );
    state.put_canister_state(canister.unwrap());
    assert_matches!(
        result,
        Err(CanisterManagerError::Hypervisor(
            _,
            HypervisorError::InstructionLimitExceeded(instructions_limit_in_error)
        ))
        if instructions_limit == instructions_limit_in_error
    );
    assert_eq!(instructions_left, NumInstructions::from(0));

    // Enough instructions result in successful upgrade.
    let mut round_limits = RoundLimits {
        instructions: as_round_instructions(NumInstructions::from(10) + compilation_cost),
        subnet_available_memory: (*MAX_SUBNET_AVAILABLE_MEMORY),
        subnet_available_callbacks: SUBNET_CALLBACK_SOFT_LIMIT as i64,
        compute_allocation_used: state.total_compute_allocation(),
    };
    let (instructions_left, result, _) = install_code(
        &canister_manager,
        InstallCodeContext {
            origin: canister_change_origin_from_principal(&sender),
            canister_id,
            wasm_source: WasmSource::CanisterModule(CanisterModule::new(wasm)),
            arg: vec![],
            mode: CanisterInstallModeV2::Upgrade(None),
        },
        &mut state,
        &mut round_limits,
    );
    assert!(result.is_ok());
    assert_eq!(instructions_left, NumInstructions::from(1));
}

#[test]
fn install_code_preserves_system_state_and_scheduler_state() {
    let canister_manager = CanisterManagerBuilder::default()
        .with_cycles_account_manager(
            CyclesAccountManagerBuilder::new()
                // Make it free so we don't have to worry about cycles when
                // making assertions.
                .with_update_message_execution_fee(Cycles::zero())
                .with_ten_update_instructions_execution_fee(Cycles::zero())
                .with_ten_update_instructions_execution_fee_wasm64(Cycles::zero())
                .build(),
        )
        .build();

    let controller = canister_test_id(123);
    let canister_id = canister_test_id(456);

    // Create a canister with various attributes to later ensure they are preserved.
    let certified_data = vec![42];
    let mut original_canister = CanisterStateBuilder::new()
        .with_canister_id(canister_id)
        .with_status(CanisterStatusType::Running)
        .with_controller(controller)
        .with_certified_data(certified_data.clone())
        .with_call_context(CallContextBuilder::new().build())
        .with_input(
            RequestBuilder::default()
                .receiver(canister_id)
                .build()
                .into(),
        )
        .build();

    let mut state = ReplicatedStateBuilder::new()
        .with_canister(original_canister.clone())
        .build();
    let mut round_limits = RoundLimits {
        instructions: as_round_instructions(EXECUTION_PARAMETERS.instruction_limits.message()),
        subnet_available_memory: (*MAX_SUBNET_AVAILABLE_MEMORY),
        subnet_available_callbacks: SUBNET_CALLBACK_SOFT_LIMIT as i64,
        compute_allocation_used: state.total_compute_allocation(),
    };

    // 1. INSTALL
    let install_code_context = InstallCodeContextBuilder::default()
        .mode(CanisterInstallModeV2::Install)
        .sender(controller.into())
        .canister_id(canister_id)
        .build();
    let compilation_cost = wasm_compilation_cost(
        install_code_context
            .wasm_source
            .unwrap_as_slice_for_testing(),
    );

    let ctxt = InstallCodeContextBuilder::default()
        .mode(CanisterInstallModeV2::Install)
        .sender(controller.into())
        .canister_id(canister_id)
        .build();
    let module_hash = ctxt.wasm_source.module_hash();
    let (instructions_left, res, canister) =
        install_code(&canister_manager, ctxt, &mut state, &mut round_limits);
    state.put_canister_state(canister.unwrap());

    // Installation is free, since there is no `(start)` or `canister_init` to run.
    assert_eq!(instructions_left, MAX_NUM_INSTRUCTIONS - compilation_cost);

    // No heap delta.
    assert_eq!(res.unwrap().heap_delta, NumBytes::from(0));

    // Verify the system state is preserved except for certified data, global timer,
    // canister version, and canister history.
    let new_state = state
        .canister_state(&canister_id)
        .unwrap()
        .system_state
        .clone();
    original_canister.system_state.certified_data = Vec::new();
    original_canister.system_state.global_timer = CanisterTimer::Inactive;
    original_canister.system_state.canister_version += 1;
    original_canister.system_state.add_canister_change(
        state.time(),
        canister_change_origin_from_canister(&controller),
        CanisterChangeDetails::code_deployment(CanisterInstallMode::Install, module_hash),
    );
    assert_eq!(new_state, original_canister.system_state);

    // Verify the scheduler state is preserved.
    assert_eq!(
        state.canister_state(&canister_id).unwrap().scheduler_state,
        original_canister.scheduler_state
    );

    // 2. REINSTALL

    let instructions_before_reinstall = as_num_instructions(round_limits.instructions);
    let ctxt = InstallCodeContextBuilder::default()
        .mode(CanisterInstallModeV2::Reinstall)
        .sender(controller.into())
        .canister_id(canister_id)
        .build();
    let module_hash = ctxt.wasm_source.module_hash();
    let (instructions_left, res, canister) =
        install_code(&canister_manager, ctxt, &mut state, &mut round_limits);
    state.put_canister_state(canister.unwrap());

    // Installation is free, since there is no `(start)` or `canister_init` to run.
    assert_eq!(
        instructions_left,
        instructions_before_reinstall - compilation_cost
    );

    // No heap delta.
    assert_eq!(res.unwrap().heap_delta, NumBytes::from(0));

    // Verify the system state is preserved except for certified data, global timer,
    // canister version, and canister history.
    let new_state = state
        .canister_state(&canister_id)
        .unwrap()
        .system_state
        .clone();
    original_canister.system_state.certified_data = Vec::new();
    original_canister.system_state.global_timer = CanisterTimer::Inactive;
    original_canister.system_state.canister_version += 1;
    original_canister.system_state.add_canister_change(
        state.time(),
        canister_change_origin_from_canister(&controller),
        CanisterChangeDetails::code_deployment(CanisterInstallMode::Reinstall, module_hash),
    );
    assert_eq!(new_state, original_canister.system_state);

    // Verify the scheduler state is preserved.
    assert_eq!(
        state.canister_state(&canister_id).unwrap().scheduler_state,
        original_canister.scheduler_state
    );

    // 3. UPGRADE
    // reset certified_data cleared by install and reinstall in the previous steps
    original_canister
        .system_state
        .certified_data
        .clone_from(&certified_data);
    state
        .canister_state_mut(&canister_id)
        .unwrap()
        .system_state
        .certified_data = certified_data;
    let instructions_before_upgrade = as_num_instructions(round_limits.instructions);
    let ctxt = InstallCodeContextBuilder::default()
        .mode(CanisterInstallModeV2::Upgrade(None))
        .sender(controller.into())
        .canister_id(canister_id)
        .build();

    let (instructions_left, res, canister) =
        install_code(&canister_manager, ctxt, &mut state, &mut round_limits);
    state.put_canister_state(canister.unwrap());

    // Installation is free, since there is no `canister_pre/post_upgrade`
    assert_eq!(
        instructions_left,
        instructions_before_upgrade - compilation_cost
    );

    // No heap delta.
    assert_eq!(res.unwrap().heap_delta, NumBytes::from(0));

    // Verify the system state is preserved except for global timer,
    // canister version, and canister history.
    let new_state = state
        .canister_state(&canister_id)
        .unwrap()
        .system_state
        .clone();
    original_canister.system_state.global_timer = CanisterTimer::Inactive;
    original_canister.system_state.canister_version += 1;
    original_canister.system_state.add_canister_change(
        state.time(),
        canister_change_origin_from_canister(&controller),
        CanisterChangeDetails::code_deployment(CanisterInstallMode::Upgrade, module_hash),
    );
    assert_eq!(new_state, original_canister.system_state);

    // Verify the scheduler state is preserved.
    assert_eq!(
        state.canister_state(&canister_id).unwrap().scheduler_state,
        original_canister.scheduler_state
    );
}

#[test]
fn lower_memory_allocation_than_usage_fails() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    test.install_code_v2(InstallCodeArgsV2::new(
        CanisterInstallModeV2::Install,
        canister_id,
        UNIVERSAL_CANISTER_WASM.to_vec(),
        vec![],
    ))
    .unwrap();

    let memory_allocation = 2;
    let err = test
        .canister_update_allocations_settings(canister_id, None, Some(memory_allocation))
        .unwrap_err();

    assert_eq!(err.code(), ErrorCode::InsufficientMemoryAllocation);
    assert!(err.description().contains(&format!(
        "Canister was given {} B memory allocation but at least",
        memory_allocation,
    )));
}

#[test]
fn test_install_when_updating_memory_allocation_via_canister_settings() {
    let mut test = ExecutionTestBuilder::new().build();

    let memory_allocation = 200;
    let canister_id = test
        .create_canister_with_allocation(*INITIAL_CYCLES, None, Some(memory_allocation))
        .unwrap();

    // The memory allocation is too low, `install_code` should fail.
    let err = test
        .install_code_v2(InstallCodeArgsV2::new(
            CanisterInstallModeV2::Install,
            canister_id,
            UNIVERSAL_CANISTER_WASM.to_vec(),
            vec![],
        ))
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InsufficientMemoryAllocation);
    assert!(err.description().contains(&format!(
        "Canister was given {} B memory allocation but at least",
        memory_allocation
    )));

    // Update memory allocation to a big enough value, `install_code` should succeed.
    let canister_id = test
        .create_canister_with_allocation(*INITIAL_CYCLES, None, Some(MEMORY_CAPACITY.get() / 2))
        .unwrap();
    test.install_code_v2(InstallCodeArgsV2::new(
        CanisterInstallModeV2::Install,
        canister_id,
        UNIVERSAL_CANISTER_WASM.to_vec(),
        vec![],
    ))
    .unwrap();
}

#[test]
fn test_upgrade_when_updating_memory_allocation_via_canister_settings() {
    let mut test = ExecutionTestBuilder::new().build();

    // canister history memory usage at the beginning of attempted upgrade
    let canister_history_memory = 2 * size_of::<CanisterChange>() + size_of::<PrincipalId>();
    let memory_allocation = WASM_PAGE_SIZE_IN_BYTES + 100 + canister_history_memory as u64;
    let canister_id = test
        .create_canister_with_allocation(*INITIAL_CYCLES, None, Some(memory_allocation))
        .unwrap();

    let wat = r#"
        (module
            (memory $memory 1)
        )"#;
    let wasm = wat::parse_str(wat).unwrap();
    test.install_code_v2(InstallCodeArgsV2::new(
        CanisterInstallModeV2::Install,
        canister_id,
        wasm.to_vec(),
        vec![],
    ))
    .unwrap();

    // Try to upgrade to a wasm module that has bigger memory requirements. It
    // should fail...
    let wat = r#"
        (module
            (memory $memory 2)
        )"#;
    let wasm = wat::parse_str(wat).unwrap();
    let err = test
        .install_code_v2(InstallCodeArgsV2::new(
            CanisterInstallModeV2::Upgrade(None),
            canister_id,
            wasm.to_vec(),
            vec![],
        ))
        .unwrap_err();

    assert_eq!(err.code(), ErrorCode::InsufficientMemoryAllocation);
    assert!(err
        .description()
        .contains("Canister was given 64.44 KiB memory allocation but at least"));

    // canister history memory usage at the beginning of update_settings
    let canister_history_memory = 2 * size_of::<CanisterChange>() + size_of::<PrincipalId>();
    // Update memory allocation to a big enough value via canister settings. The
    // upgrade should succeed.
    let memory_allocation = WASM_PAGE_SIZE_IN_BYTES * 2 + 100 + canister_history_memory as u64;
    test.canister_update_allocations_settings(canister_id, None, Some(memory_allocation))
        .unwrap();

    test.install_code_v2(InstallCodeArgsV2::new(
        CanisterInstallModeV2::Upgrade(None),
        canister_id,
        wasm.to_vec(),
        vec![],
    ))
    .unwrap();
}

#[test]
#[cfg(not(all(target_arch = "aarch64", target_vendor = "apple")))]
fn uninstall_code_can_be_invoked_by_governance_canister() {
    use crate::util::GOVERNANCE_CANISTER_ID;

    let canister_manager = CanisterManagerBuilder::default().build();
    let mut state = ReplicatedStateBuilder::new()
        .with_canister(
            CanisterStateBuilder::new()
                .with_canister_id(canister_test_id(0))
                // Give the canister a random wasm so that it
                // has an execution state.
                .with_wasm(vec![1, 2, 3])
                .build(),
        )
        .build();

    // Insert data to the chunk store to verify it is cleared on uninstall.
    let store = &mut state
        .canister_state_mut(&canister_test_id(0))
        .unwrap()
        .system_state
        .wasm_chunk_store;
    let chunk = [0x41, 200].to_vec();
    let result = store.can_insert_chunk(canister_manager.config.wasm_chunk_store_max_size, chunk);
    let validated_chunk = match result {
        ChunkValidationResult::Insert(validated_chunk) => validated_chunk,
        res => panic!("Unexpected chunk validation result: {:?}", res),
    };
    store.insert_chunk(validated_chunk);

    assert!(state
        .canister_state(&canister_test_id(0))
        .unwrap()
        .execution_state
        .is_some());

    assert_eq!(
        state
            .canister_state(&canister_test_id(0))
            .unwrap()
            .system_state
            .wasm_chunk_store
            .memory_usage(),
        NumBytes::from(1024 * 1024)
    );

    let no_op_counter: IntCounter = IntCounter::new("no_op", "no_op").unwrap();
    canister_manager
        .uninstall_code(
            canister_change_origin_from_canister(&GOVERNANCE_CANISTER_ID),
            canister_test_id(0),
            &mut state,
            &no_op_counter,
        )
        .unwrap();

    // The execution state of the canister should be removed.
    assert_eq!(
        state
            .canister_state(&canister_test_id(0))
            .unwrap()
            .execution_state,
        None
    );

    assert_eq!(
        state
            .canister_state(&canister_test_id(0))
            .unwrap()
            .system_state
            .wasm_chunk_store
            .memory_usage(),
        NumBytes::from(0)
    )
}

#[test]
fn test_install_when_setting_memory_allocation_to_zero() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    let memory_allocation = 0;
    test.canister_update_allocations_settings(canister_id, None, Some(memory_allocation))
        .unwrap();
    test.install_code_v2(InstallCodeArgsV2::new(
        CanisterInstallModeV2::Install,
        canister_id,
        UNIVERSAL_CANISTER_WASM.to_vec(),
        vec![],
    ))
    .unwrap();
}

#[test]
fn test_upgrade_when_setting_memory_allocation_to_zero() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(*INITIAL_CYCLES);

    test.install_code_v2(InstallCodeArgsV2::new(
        CanisterInstallModeV2::Install,
        canister_id,
        UNIVERSAL_CANISTER_WASM.to_vec(),
        vec![],
    ))
    .unwrap();

    let memory_allocation = 0;
    test.canister_update_allocations_settings(canister_id, None, Some(memory_allocation))
        .unwrap();
    test.install_code_v2(InstallCodeArgsV2::new(
        CanisterInstallModeV2::Upgrade(None),
        canister_id,
        UNIVERSAL_CANISTER_WASM.to_vec(),
        vec![],
    ))
    .unwrap();
}

#[test]
fn max_number_of_canisters_is_respected_when_creating_canisters() {
    let max_number_of_canisters = 3;
    let mut test = ExecutionTestBuilder::new()
        .with_max_number_of_canisters(max_number_of_canisters)
        .build();

    // Create 3 canisters with `max_number_of_canisters = 3`, should succeed.
    test.create_canister_with_allocation(*INITIAL_CYCLES, None, None)
        .unwrap();
    test.create_canister_with_allocation(*INITIAL_CYCLES, None, None)
        .unwrap();
    test.create_canister_with_allocation(*INITIAL_CYCLES, None, None)
        .unwrap();

    // Creating a fourth canister with 3 already created and
    // `max_number_of_canisters = 3` should fail.
    let err = test
        .create_canister_with_allocation(*INITIAL_CYCLES, None, None)
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::MaxNumberOfCanistersReached);
    assert!(err.description().contains(&format!(
        "has reached the allowed canister limit of {} canisters",
        max_number_of_canisters,
    )));
}

/// This canister exports a query that returns its canister version.
const CANISTER_VERSION: &str = r#"
    (module
        (import "ic0" "msg_reply" (func $msg_reply))
        (import "ic0" "msg_reply_data_append"
            (func $msg_reply_data_append (param i32 i32)))
        (import "ic0" "canister_version"
            (func $canister_version (result i64)))
        (func $version
            (i64.store (i32.const 0) (call $canister_version))
            (call $msg_reply_data_append
                (i32.const 0) ;; the counter from heap[0]
                (i32.const 1)) ;; length (assume the i32 actually fits in one byte)
            (call $msg_reply))
        (func $canister_init)
        (memory $memory 1)
        (export "canister_query version" (func $version))
        (export "canister_init" (func $canister_init))
    )"#;

/// With sandboxing, we are caching some information about a canister's state
/// (including the canister version) with the sandboxed process. This test verifies
/// that the canister sees the proper change when the canister version is updated.
#[test]
fn canister_version_changes_are_visible() {
    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = test.canister_from_wat(CANISTER_VERSION).unwrap();
    let result = test.ingress(canister_id, "version", vec![]);
    let reply = get_reply(result);
    assert_eq!(reply, vec![1]);

    // Change controllers to bump canister version.
    let new_controller = PrincipalId::try_from(&[1, 2, 3][..]).unwrap();
    assert_ne!(new_controller, test.user_id().get());
    test.set_controller(canister_id, new_controller).unwrap();

    let result = test.ingress(canister_id, "version", vec![]);
    let reply = get_reply(result);
    assert_eq!(reply, vec![3]);
}

// This test confirms that we can always create as many canisters as possible if
// no explicit limit is set.
#[test]
fn creating_canisters_always_works_if_limit_is_set_to_zero() {
    let own_subnet = subnet_test_id(1);
    let caller = canister_test_id(1);
    let mut test = ExecutionTestBuilder::new()
        .with_own_subnet_id(own_subnet)
        .with_caller(own_subnet, caller)
        .build();
    for _ in 0..1_000 {
        test.inject_call_to_ic00(
            Method::CreateCanister,
            EmptyBlob.encode(),
            test.canister_creation_fee(),
        );
        test.execute_all();
    }
    assert_eq!(test.state().num_canisters() as u64, 1_000);
}

#[test]
fn test_upgrade_preserves_stable_memory() {
    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = test.universal_canister().unwrap();
    let data = [1, 2, 3, 5, 8, 13];
    let update = wasm()
        .stable_grow(1)
        .stable_write(42, &data)
        .reply()
        .build();
    let result = test.ingress(canister_id, "update", update);
    let reply = get_reply(result);
    assert_eq!(reply, vec![] as Vec<u8>);
    test.upgrade_canister(canister_id, UNIVERSAL_CANISTER_WASM.to_vec())
        .unwrap();
    let query = wasm()
        .stable_read(42, data.len() as u32)
        .append_and_reply()
        .build();
    let result = test.ingress(canister_id, "query", query);
    let reply = get_reply(result);
    assert_eq!(reply, data);
}

#[test]
fn test_enhanced_orthogonal_persistence_upgrade_preserves_main_memory() {
    let mut test = ExecutionTestBuilder::new().build();

    let version1_wat = r#"
        (module
            (func $start
                call $initialize
                call $check
            )
            (func $initialize
                global.get 0
                i32.const 1234
                i32.store
                global.get 1
                i32.const 5678
                i32.store
            )
            (func $check_word (param i32) (param i32)
                block
                    local.get 0
                    i32.load
                    local.get 1
                    i32.eq
                    br_if 0
                    unreachable
                end
            )
            (func $check
                global.get 0
                i32.const 1234
                call $check_word
                global.get 1
                i32.const 5678
                call $check_word
            )
            (start $start)
            (memory 160)
            (global (mut i32) (i32.const 8500000))
            (global (mut i32) (i32.const 9000000))
            (@custom "icp:private enhanced-orthogonal-persistence" "")
        )
        "#;
    let version1_wasm = wat::parse_str(version1_wat).unwrap();
    let canister_id = test.create_canister(Cycles::new(1_000_000_000_000_000));
    test.install_canister(canister_id, version1_wasm).unwrap();

    let version2_wat = r#"
        (module
            (func $check_word (param i32) (param i32)
                block
                    local.get 0
                    i32.load
                    local.get 1
                    i32.eq
                    br_if 0
                    unreachable
                end
            )
            (func $check
                global.get 0
                i32.const 1234
                call $check_word
                global.get 1
                i32.const 5678
                call $check_word
            )
            (start $check)
            (memory 160)
            (global (mut i32) (i32.const 8500000))
            (global (mut i32) (i32.const 9000000))
            (@custom "icp:private enhanced-orthogonal-persistence" "")
        )
        "#;

    let version2_wasm = wat::parse_str(version2_wat).unwrap();
    test.upgrade_canister_v2(
        canister_id,
        version2_wasm,
        CanisterUpgradeOptions {
            skip_pre_upgrade: None,
            wasm_memory_persistence: Some(WasmMemoryPersistence::Keep),
        },
    )
    .unwrap();
}

#[test]
fn fails_with_missing_main_memory_option_for_enhanced_orthogonal_persistence() {
    let mut test = ExecutionTestBuilder::new().build();

    let version1_wat = r#"
        (module
            (memory 1)
            (@custom "icp:private enhanced-orthogonal-persistence" "")
        )
        "#;
    let version1_wasm = wat::parse_str(version1_wat).unwrap();
    let canister_id = test.create_canister(Cycles::new(1_000_000_000_000_000));
    test.install_canister(canister_id, version1_wasm).unwrap();

    let version2_wat = r#"
        (module
            (memory 1)
        )
        "#;

    let version2_wasm = wat::parse_str(version2_wat).unwrap();
    let error = test
        .upgrade_canister_v2(
            canister_id,
            version2_wasm,
            CanisterUpgradeOptions {
                skip_pre_upgrade: None,
                wasm_memory_persistence: None,
            },
        )
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::CanisterContractViolation);
    assert_eq!(error.description(), "Missing upgrade option: Enhanced orthogonal persistence requires the `wasm_memory_persistence` upgrade option.");
}

#[test]
fn fails_with_missing_upgrade_option_for_enhanced_orthogonal_persistence() {
    let mut test = ExecutionTestBuilder::new().build();

    let version1_wat = r#"
        (module
            (memory 1)
            (@custom "icp:private enhanced-orthogonal-persistence" "")
        )
        "#;
    let version1_wasm = wat::parse_str(version1_wat).unwrap();
    let canister_id = test.create_canister(Cycles::new(1_000_000_000_000_000));
    test.install_canister(canister_id, version1_wasm).unwrap();

    let version2_wat = r#"
        (module
            (memory 1)
        )
        "#;

    let version2_wasm = wat::parse_str(version2_wat).unwrap();
    let error = test
        .upgrade_canister(canister_id, version2_wasm)
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::CanisterContractViolation);
    assert_eq!(error.description(), "Missing upgrade option: Enhanced orthogonal persistence requires the `wasm_memory_persistence` upgrade option.");
}

#[test]
fn fails_when_keeping_main_memory_without_enhanced_orthogonal_persistence() {
    let mut test = ExecutionTestBuilder::new().build();

    let classical_persistence = r#"
    (module
        (memory 1)
    )
    "#;
    let orthogonal_persistence = r#"
    (module
        (memory 1)
        (@custom "icp:private enhanced-orthogonal-persistence" "")
    )
    "#;

    for (version1_wat, version2_wat) in [
        (classical_persistence, classical_persistence),
        (orthogonal_persistence, classical_persistence),
    ] {
        let version1_wasm = wat::parse_str(version1_wat).unwrap();
        let canister_id = test.create_canister(Cycles::new(1_000_000_000_000_000));
        test.install_canister(canister_id, version1_wasm).unwrap();

        let version2_wasm = wat::parse_str(version2_wat).unwrap();
        let error = test
            .upgrade_canister_v2(
                canister_id,
                version2_wasm,
                CanisterUpgradeOptions {
                    skip_pre_upgrade: None,
                    wasm_memory_persistence: Some(WasmMemoryPersistence::Keep),
                },
            )
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::CanisterContractViolation);
        assert_eq!(error.description(), "Invalid upgrade option: The `wasm_memory_persistence: opt Keep` upgrade option requires that the new canister module supports enhanced orthogonal persistence.");
    }
}

#[test]
fn test_upgrade_to_enhanced_orthogonal_persistence() {
    let mut test = ExecutionTestBuilder::new().build();

    let version1_wat = r#"
    (module
        (memory 1)
    )
    "#;
    let version1_wasm = wat::parse_str(version1_wat).unwrap();
    let canister_id = test.create_canister(Cycles::new(1_000_000_000_000_000));
    test.install_canister(canister_id, version1_wasm).unwrap();

    let version2_wat = r#"
    (module
        (memory 1)
        (@custom "icp:private enhanced-orthogonal-persistence" "")
    )
    "#;
    let version2_wasm = wat::parse_str(version2_wat).unwrap();
    test.upgrade_canister_v2(
        canister_id,
        version2_wasm,
        CanisterUpgradeOptions {
            skip_pre_upgrade: None,
            wasm_memory_persistence: Some(WasmMemoryPersistence::Keep),
        },
    )
    .unwrap();
}

#[test]
fn test_invalid_wasm_with_enhanced_orthogonal_persistence() {
    let mut test = ExecutionTestBuilder::new().build();

    let valid_version1_wat = r#"
    (module
        (memory 1)
    )
    "#;
    let version1_wasm = wat::parse_str(valid_version1_wat).unwrap();
    let canister_id = test.create_canister(Cycles::new(1_000_000_000_000_000));
    test.install_canister(canister_id, version1_wasm).unwrap();

    let invalid_version2_wat = r#"
    (module
        (func $check
            i32.const 1
        )
        (start $check)
        (memory 1)
        (@custom "icp:private enhanced-orthogonal-persistence" "")
    )
    "#;
    let version2_wasm = wat::parse_str(invalid_version2_wat).unwrap();
    let error = test
        .upgrade_canister_v2(
            canister_id,
            version2_wasm,
            CanisterUpgradeOptions {
                skip_pre_upgrade: None,
                wasm_memory_persistence: Some(WasmMemoryPersistence::Keep),
            },
        )
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::CanisterInvalidWasm);
}

fn create_canisters(test: &mut ExecutionTest, canisters: usize) {
    for _ in 1..=canisters {
        test.canister_from_binary(MINIMAL_WASM.to_vec()).unwrap();
    }
}

#[test]
pub fn test_can_create_10_canisters() {
    let mut test = ExecutionTestBuilder::new().build();
    create_canisters(&mut test, 10);
}

// The following tests are expensive to run, so enable them explicitly:
// perf stat cargo t test_can_create_125_canisters -- --include-ignored --nocapture
// Test results: https://docs.google.com/spreadsheets/d/14tBO0vg508tW_r4t4_btH4iQdia9BMIJeV8IAuWc_sg
#[test]
#[ignore]
pub fn test_can_create_125_canisters() {
    let mut test = ExecutionTestBuilder::new().build();
    create_canisters(&mut test, 125);
}

#[test]
#[ignore]
pub fn test_can_create_250_canisters() {
    let mut test = ExecutionTestBuilder::new().build();
    create_canisters(&mut test, 250);
}

#[test]
#[ignore]
pub fn test_can_create_500_canisters() {
    let mut test = ExecutionTestBuilder::new().build();
    create_canisters(&mut test, 500);
}

#[test]
#[ignore]
pub fn test_can_create_1000_canisters() {
    let mut test = ExecutionTestBuilder::new().build();
    create_canisters(&mut test, 1000);
}

#[test]
#[ignore]
pub fn test_can_create_5000_canisters() {
    let mut test = ExecutionTestBuilder::new().build();
    create_canisters(&mut test, 5000);
}

#[test]
#[ignore]
pub fn test_can_create_10000_canisters() {
    let mut test = ExecutionTestBuilder::new()
        .with_max_number_of_canisters(10_000)
        .build();
    create_canisters(&mut test, 10_000);
}

#[test]
fn test_install_code_rate_limiting() {
    let mut test = ExecutionTestBuilder::new()
        .with_rate_limiting_of_instructions()
        .build();
    let canister_id = test.universal_canister().unwrap();
    let binary = UNIVERSAL_CANISTER_WASM.to_vec();
    let err = test
        .upgrade_canister(canister_id, binary.clone())
        .unwrap_err();
    assert_eq!(ErrorCode::CanisterInstallCodeRateLimited, err.code());
    let err = test.upgrade_canister(canister_id, binary).unwrap_err();
    assert_eq!(ErrorCode::CanisterInstallCodeRateLimited, err.code());
}

#[test]
fn test_install_code_rate_limiting_disabled() {
    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = test.universal_canister().unwrap();
    let binary = UNIVERSAL_CANISTER_WASM.to_vec();
    test.upgrade_canister(canister_id, binary.clone()).unwrap();
    test.upgrade_canister(canister_id, binary).unwrap();
}

#[test]
fn unfreezing_of_frozen_canister() {
    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = test
        .universal_canister_with_cycles(Cycles::new(1_000_000_000_000))
        .unwrap();

    // Set the freezing threshold high to freeze the canister.
    let payload = UpdateSettingsArgs {
        canister_id: canister_id.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_freezing_threshold(1_000_000_000_000)
            .build(),
        sender_canister_version: None,
    }
    .encode();
    let balance_before = test.canister_state(canister_id).system_state.balance();
    let result = test.subnet_message(Method::UpdateSettings, payload);
    let balance_after = test.canister_state(canister_id).system_state.balance();
    // If the freezing threshold doesn't change, then the canister is not charged.
    assert_eq!(balance_before, balance_after);
    get_reply(result);

    // Sending an ingress message fails due to the cycles balance.
    let err = test
        .ingress(canister_id, "update", wasm().reply().build())
        .unwrap_err();
    assert_eq!(ErrorCode::CanisterOutOfCycles, err.code());

    // Unfreeze the canister.
    let payload = UpdateSettingsArgs {
        canister_id: canister_id.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_freezing_threshold(1)
            .build(),
        sender_canister_version: None,
    }
    .encode();
    let ingress_bytes =
        NumBytes::from((Method::UpdateSettings.to_string().len() + payload.len()) as u64);
    let balance_before = test.canister_state(canister_id).system_state.balance();
    test.subnet_message(Method::UpdateSettings, payload)
        .unwrap();
    let balance_after = test.canister_state(canister_id).system_state.balance();
    assert_eq!(
        balance_before - balance_after,
        test.cycles_account_manager()
            .ingress_induction_cost_from_bytes(
                ingress_bytes,
                test.subnet_size(),
                CanisterCyclesCostSchedule::Normal,
            )
    );
    // Now the canister works again.
    let result = test.ingress(canister_id, "update", wasm().reply().build());
    get_reply(result);
}

#[test]
fn frozen_canister_reveal_top_up() {
    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = test
        .universal_canister_with_cycles(Cycles::new(1_000_000_000_000))
        .unwrap();

    // Set the freezing threshold high to freeze the canister.
    let payload = UpdateSettingsArgs {
        canister_id: canister_id.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_freezing_threshold(1_000_000_000_000)
            .build(),
        sender_canister_version: None,
    }
    .encode();
    test.subnet_message(Method::UpdateSettings, payload)
        .unwrap();

    // Sending an ingress message to a frozen canister fails with a verbose error message.
    let err = test
        .ingress(canister_id, "update", wasm().reply().build())
        .unwrap_err();
    assert_eq!(ErrorCode::CanisterOutOfCycles, err.code());
    assert!(err.description().starts_with(&format!(
        "Canister {} is out of cycles: please top up the canister with at least",
        canister_id
    )));

    // Blackhole the canister.
    test.canister_update_controller(canister_id, vec![])
        .unwrap();

    // Sending an ingress message to a frozen canister fails without revealing
    // top up balance to non-controllers.
    let err = test
        .ingress(canister_id, "update", wasm().reply().build())
        .unwrap_err();
    assert_eq!(ErrorCode::CanisterOutOfCycles, err.code());
    assert_eq!(
        err.description(),
        format!("Canister {} is out of cycles", canister_id)
    );
}

#[test]
fn create_canister_fails_if_memory_capacity_exceeded() {
    let mut test = ExecutionTestBuilder::new()
        .with_subnet_execution_memory(MEMORY_CAPACITY.get() as i64)
        .with_subnet_memory_reservation(0)
        .build();
    let uc = test.universal_canister().unwrap();

    test.canister_state_mut(uc)
        .system_state
        .set_balance(Cycles::new(1_000_000_000_000_000_000));

    let settings = CanisterSettingsArgsBuilder::new()
        .with_freezing_threshold(1)
        .with_memory_allocation(MEMORY_CAPACITY.get() / 2)
        .build();
    let args = CreateCanisterArgs {
        settings: Some(settings),
        sender_canister_version: None,
    };
    let create_canister = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(args.encode())
                .on_reject(wasm().reject_message().reject()),
            test.canister_creation_fee() + Cycles::new(1_000_000_000),
        )
        .build();
    let result = test.ingress(uc, "update", create_canister);
    let reply = get_reply(result);
    Decode!(reply.as_slice(), CanisterIdRecord).unwrap();

    // There should be not enough memory for CAPACITY/2 because universal
    // canister already consumed some
    let settings = CanisterSettingsArgsBuilder::new()
        .with_freezing_threshold(1)
        .with_memory_allocation(MEMORY_CAPACITY.get() / 2)
        .build();
    let args = CreateCanisterArgs {
        settings: Some(settings),
        sender_canister_version: None,
    };
    let create_canister = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(args.encode())
                .on_reject(wasm().reject_message().reject()),
            test.canister_creation_fee() + Cycles::new(1_000_000_000),
        )
        .build();
    let result = test.ingress(uc, "update", create_canister).unwrap();
    assert_matches!(result, WasmResult::Reject(_));
}

#[test]
fn create_canister_makes_subnet_oversubscribed() {
    let mut test = ExecutionTestBuilder::new()
        .with_allocatable_compute_capacity_in_percent(100)
        .build();
    let uc = test.universal_canister().unwrap();

    test.canister_state_mut(uc)
        .system_state
        .set_balance(Cycles::new(u128::MAX));

    let settings = CanisterSettingsArgsBuilder::new()
        .with_freezing_threshold(1)
        .with_compute_allocation(50)
        .build();
    let args = CreateCanisterArgs {
        settings: Some(settings),
        sender_canister_version: None,
    };
    let create_canister = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(args.encode())
                .on_reject(wasm().reject_message().reject()),
            test.canister_creation_fee() + Cycles::new(1_000_000_000),
        )
        .build();
    let result = test.ingress(uc, "update", create_canister);
    let reply = get_reply(result);
    Decode!(reply.as_slice(), CanisterIdRecord).unwrap();

    let settings = CanisterSettingsArgsBuilder::new()
        .with_freezing_threshold(1)
        .with_compute_allocation(25)
        .build();
    let args = CreateCanisterArgs {
        settings: Some(settings),
        sender_canister_version: None,
    };
    let create_canister = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(args.encode())
                .on_reject(wasm().reject_message().reject()),
            test.canister_creation_fee() + Cycles::new(1_000_000_000),
        )
        .build();
    let result = test.ingress(uc, "update", create_canister);
    let reply = get_reply(result);
    Decode!(reply.as_slice(), CanisterIdRecord).unwrap();

    // Create a canister with compute allocation.
    let settings = CanisterSettingsArgsBuilder::new()
        .with_freezing_threshold(1)
        .with_compute_allocation(30)
        .build();
    let args = CreateCanisterArgs {
        settings: Some(settings),
        sender_canister_version: None,
    };
    let create_canister = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(args.encode())
                .on_reject(wasm().reject_message().reject()),
            test.canister_creation_fee() + Cycles::new(1_000_000_000),
        )
        .build();
    let result = test.ingress(uc, "update", create_canister).unwrap();
    match result {
        WasmResult::Reject(msg) => {
            assert!(msg.contains("compute allocation"))
        }
        _ => panic!("Expected WasmResult::Reject"),
    }
}

#[test]
fn update_settings_makes_subnet_oversubscribed() {
    // By default the scheduler has 2 cores
    let mut test = ExecutionTestBuilder::new()
        .with_allocatable_compute_capacity_in_percent(100)
        .with_subnet_execution_memory(100 * 1024 * 1024) // 100 MiB
        .with_subnet_memory_reservation(0)
        .build();
    let c1 = test.create_canister(Cycles::new(1_000_000_000_000_000));
    let c2 = test.create_canister(Cycles::new(1_000_000_000_000_000));
    let c3 = test.create_canister(Cycles::new(1_000_000_000_000_000));

    // Updating the compute allocation.
    let args = UpdateSettingsArgs {
        canister_id: c1.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_freezing_threshold(1)
            .with_compute_allocation(50)
            .build(),
        sender_canister_version: None,
    };
    test.subnet_message(Method::UpdateSettings, args.encode())
        .unwrap();

    let args = UpdateSettingsArgs {
        canister_id: c2.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_freezing_threshold(1)
            .with_compute_allocation(25)
            .build(),
        sender_canister_version: None,
    };
    test.subnet_message(Method::UpdateSettings, args.encode())
        .unwrap();

    // Go over the compute capacity.
    let args = UpdateSettingsArgs {
        canister_id: c3.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_freezing_threshold(1)
            .with_compute_allocation(30)
            .build(),
        sender_canister_version: None,
    };
    let err = test
        .subnet_message(Method::UpdateSettings, args.encode())
        .unwrap_err();
    assert_eq!(ErrorCode::SubnetOversubscribed, err.code());

    // Updating the memory allocation.
    let args = UpdateSettingsArgs {
        canister_id: c1.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_freezing_threshold(1)
            .with_memory_allocation(10 * 1024 * 1024)
            .build(),
        sender_canister_version: None,
    };
    test.subnet_message(Method::UpdateSettings, args.encode())
        .unwrap();

    let args = UpdateSettingsArgs {
        canister_id: c2.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_freezing_threshold(1)
            .with_memory_allocation(30 * 1024 * 1024)
            .build(),
        sender_canister_version: None,
    };
    test.subnet_message(Method::UpdateSettings, args.encode())
        .unwrap();

    // Go over the memory capacity.
    let args = UpdateSettingsArgs {
        canister_id: c3.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_freezing_threshold(1)
            .with_memory_allocation(65 * 1024 * 1024)
            .build(),
        sender_canister_version: None,
    };
    let err = test
        .subnet_message(Method::UpdateSettings, args.encode())
        .unwrap_err();
    assert_eq!(ErrorCode::SubnetOversubscribed, err.code());
}

#[test]
fn create_canister_when_compute_capacity_is_oversubscribed() {
    let mut test = ExecutionTestBuilder::new()
        .with_allocatable_compute_capacity_in_percent(0)
        .build();
    let uc = test.universal_canister().unwrap();

    // Manually set the compute allocation higher to emulate the state after
    // replica upgrade that decreased compute capacity.
    test.canister_state_mut(uc)
        .scheduler_state
        .compute_allocation = ComputeAllocation::try_from(60).unwrap();
    test.canister_state_mut(uc)
        .system_state
        .set_balance(Cycles::new(2_000_000_000_000_000));

    // Create a canister with default settings.
    let args = CreateCanisterArgs::default();
    let create_canister = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args().other_side(args.encode()),
            test.canister_creation_fee(),
        )
        .build();

    let result = test.ingress(uc, "update", create_canister);
    let reply = get_reply(result);
    Decode!(reply.as_slice(), CanisterIdRecord).unwrap();

    // Create a canister with zero compute allocation.
    let settings = CanisterSettingsArgsBuilder::new()
        .with_compute_allocation(0)
        .build();
    let args = CreateCanisterArgs {
        settings: Some(settings),
        sender_canister_version: None,
    };
    let create_canister = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(args.encode())
                .on_reject(wasm().reject_message().reject()),
            test.canister_creation_fee(),
        )
        .build();
    let result = test.ingress(uc, "update", create_canister);
    let reply = get_reply(result);
    Decode!(reply.as_slice(), CanisterIdRecord).unwrap();

    // Create a canister with compute allocation.
    let settings = CanisterSettingsArgsBuilder::new()
        .with_compute_allocation(10)
        .build();
    let args = CreateCanisterArgs {
        settings: Some(settings),
        sender_canister_version: None,
    };
    let create_canister = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(args.encode())
                .on_reject(wasm().reject_message().reject()),
            test.canister_creation_fee(),
        )
        .build();
    test.ingress(uc, "update", create_canister)
        .unwrap()
        .assert_contains_reject(
            "Canister requested a compute allocation of 10% which \
            cannot be satisfied because the Subnet's remaining \
            compute capacity is 0%.",
        );
}

#[test]
fn update_settings_when_compute_capacity_is_oversubscribed() {
    let mut test = ExecutionTestBuilder::new()
        .with_allocatable_compute_capacity_in_percent(0)
        .build();
    let canister_id = test.create_canister(Cycles::new(1_000_000_000_000_000));

    // Manually set the compute allocation higher to emulate the state after
    // replica upgrade that decreased compute capacity.
    test.canister_state_mut(canister_id)
        .scheduler_state
        .compute_allocation = ComputeAllocation::try_from(60).unwrap();

    // Updating the compute allocation to a higher value fails.
    let args = UpdateSettingsArgs {
        canister_id: canister_id.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_compute_allocation(61)
            .build(),
        sender_canister_version: None,
    };
    test.subnet_message(Method::UpdateSettings, args.encode())
        .unwrap_err()
        .assert_contains(
            ErrorCode::SubnetOversubscribed,
            "Canister requested a compute allocation of 61% \
        which cannot be satisfied because the Subnet's \
        remaining compute capacity is 60%.",
        );

    // Updating the compute allocation to the same value succeeds.
    let args = UpdateSettingsArgs {
        canister_id: canister_id.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_compute_allocation(60)
            .build(),
        sender_canister_version: None,
    };
    test.subnet_message(Method::UpdateSettings, args.encode())
        .unwrap();
    assert_eq!(
        ComputeAllocation::try_from(60).unwrap(),
        test.canister_state(canister_id)
            .scheduler_state
            .compute_allocation
    );

    // Updating the compute allocation to a lower value succeeds.
    let args = UpdateSettingsArgs {
        canister_id: canister_id.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_compute_allocation(59)
            .build(),
        sender_canister_version: None,
    };
    test.subnet_message(Method::UpdateSettings, args.encode())
        .unwrap();
    assert_eq!(
        ComputeAllocation::try_from(59).unwrap(),
        test.canister_state(canister_id)
            .scheduler_state
            .compute_allocation
    );
}

#[test]
fn cycles_correct_if_upgrade_succeeds() {
    let mut test = ExecutionTestBuilder::new().build();

    let wat = r#"
        (module
            (func $start
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (func (export "canister_init")
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (func (export "canister_pre_upgrade")
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (func (export "canister_post_upgrade")
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (start $start)
            (memory 0)
        )"#;
    let wasm = wat::parse_str(wat).unwrap();

    let initial_cycles = Cycles::new(1_000_000_000_000_000);
    let id = test.create_canister(initial_cycles);

    test.install_canister(id, wasm.clone()).unwrap();
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        initial_cycles - test.canister_execution_cost(id),
    );

    assert_delta!(
        test.canister_execution_cost(id),
        test.cycles_account_manager().execution_cost(
            NumInstructions::from(5 * *DROP_MEMORY_GROW_CONST_COST) + wasm_compilation_cost(&wasm),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
            test.canister_wasm_execution_mode(id),
        ),
        Cycles::new(10)
    );

    let cycles_before = test.canister_state(id).system_state.balance();
    let execution_cost_before = test.canister_execution_cost(id);
    // Clear `expected_compiled_wasms` so that the full execution cost is applied
    test.state_mut().metadata.expected_compiled_wasms.clear();
    test.upgrade_canister(id, wasm.clone()).unwrap();
    let execution_cost = test.canister_execution_cost(id) - execution_cost_before;
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        cycles_before - execution_cost,
    );
    assert_delta!(
        execution_cost,
        test.cycles_account_manager().execution_cost(
            NumInstructions::from(11 * *DROP_MEMORY_GROW_CONST_COST) + wasm_compilation_cost(&wasm),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
            test.canister_wasm_execution_mode(id),
        ),
        Cycles::new(10)
    );
}

#[test]
fn cycles_correct_if_upgrade_fails_at_validation() {
    let mut test = ExecutionTestBuilder::new()
        .with_rate_limiting_of_instructions()
        .build();

    let wat = r#"
        (module
            (func (export "canister_pre_upgrade")
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (func (export "canister_post_upgrade")
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (memory 0)
        )"#;
    let wasm = wat::parse_str(wat).unwrap();

    let initial_cycles = Cycles::new(1_000_000_000_000_000);
    let id = test.create_canister(initial_cycles);

    test.install_canister(id, wasm.clone()).unwrap();
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        initial_cycles - test.canister_execution_cost(id),
    );
    assert_eq!(
        test.canister_execution_cost(id),
        test.cycles_account_manager().execution_cost(
            wasm_compilation_cost(&wasm),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
            test.canister_wasm_execution_mode(id),
        )
    );

    // Set a large value for `install_code_debit` so the installation fails due
    // to rate limiting.
    test.canister_state_mut(id)
        .scheduler_state
        .install_code_debit = NumInstructions::from(u64::MAX);

    let cycles_before = test.canister_state(id).system_state.balance();
    let execution_cost_before = test.canister_execution_cost(id);
    test.upgrade_canister(id, wasm).unwrap_err();
    let execution_cost = test.canister_execution_cost(id) - execution_cost_before;
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        cycles_before - execution_cost,
    );
    assert_eq!(
        execution_cost,
        test.cycles_account_manager().execution_cost(
            NumInstructions::from(0),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
            test.canister_wasm_execution_mode(id),
        )
    );
}

#[test]
fn cycles_correct_if_upgrade_fails_at_start() {
    let mut test = ExecutionTestBuilder::new().build();

    let wat1 = r#"
        (module
            (func (export "canister_pre_upgrade")
                (drop (memory.grow (i32.const 1)))
            )
            (memory 0)
        )"#;
    let wat2 = r#"
        (module
            (func $start
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (unreachable)
            )
            (func (export "canister_post_upgrade")
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (start $start)
            (memory 0)
        )"#;
    let wasm1 = wat::parse_str(wat1).unwrap();
    let wasm2 = wat::parse_str(wat2).unwrap();

    let initial_cycles = Cycles::new(1_000_000_000_000_000);
    let id = test.create_canister(initial_cycles);

    test.install_canister(id, wasm1).unwrap();
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        initial_cycles - test.canister_execution_cost(id),
    );

    let cycles_before = test.canister_state(id).system_state.balance();
    let execution_cost_before = test.canister_execution_cost(id);
    test.upgrade_canister(id, wasm2.clone()).unwrap_err();
    let execution_cost = test.canister_execution_cost(id) - execution_cost_before;
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        cycles_before - execution_cost,
    );
    assert_delta!(
        execution_cost,
        test.cycles_account_manager().execution_cost(
            NumInstructions::from(3 * *DROP_MEMORY_GROW_CONST_COST + *UNREACHABLE_COST)
                + wasm_compilation_cost(&wasm2),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
            test.canister_wasm_execution_mode(id),
        ),
        Cycles::new(10)
    );
}

#[test]
fn cycles_correct_if_upgrade_fails_at_pre_upgrade() {
    let mut test = ExecutionTestBuilder::new().build();

    let wat = r#"
        (module
            (func (export "canister_pre_upgrade")
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (unreachable)
            )
            (func (export "canister_post_upgrade")
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (memory 0)
        )"#;
    let wasm = wat::parse_str(wat).unwrap();

    let initial_cycles = Cycles::new(1_000_000_000_000_000);
    let id = test.create_canister(initial_cycles);

    test.install_canister(id, wasm.clone()).unwrap();
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        initial_cycles - test.canister_execution_cost(id),
    );
    assert_eq!(
        test.canister_execution_cost(id),
        test.cycles_account_manager().execution_cost(
            wasm_compilation_cost(&wasm),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
            test.canister_wasm_execution_mode(id),
        )
    );

    let cycles_before = test.canister_state(id).system_state.balance();
    let execution_cost_before = test.canister_execution_cost(id);
    test.upgrade_canister(id, wasm).unwrap_err();
    let execution_cost = test.canister_execution_cost(id) - execution_cost_before;
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        cycles_before - execution_cost,
    );
    assert_delta!(
        execution_cost,
        test.cycles_account_manager().execution_cost(
            NumInstructions::from(3 * *DROP_MEMORY_GROW_CONST_COST + *UNREACHABLE_COST),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
            test.canister_wasm_execution_mode(id),
        ),
        Cycles::new(10)
    );
}

#[test]
fn cycles_correct_if_upgrade_fails_at_post_upgrade() {
    let mut test = ExecutionTestBuilder::new().build();

    let wat1 = r#"
        (module
            (func (export "canister_pre_upgrade")
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (memory 0)
        )"#;
    let wat2 = r#"
        (module
            (func $start
                (drop (memory.grow (i32.const 1)))
            )
            (func (export "canister_post_upgrade")
                (unreachable)
            )
            (start $start)
            (memory 0)
        )"#;
    let wasm1 = wat::parse_str(wat1).unwrap();
    let wasm2 = wat::parse_str(wat2).unwrap();

    let initial_cycles = Cycles::new(1_000_000_000_000_000);
    let id = test.create_canister(initial_cycles);

    test.install_canister(id, wasm1).unwrap();
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        initial_cycles - test.canister_execution_cost(id),
    );

    let cycles_before = test.canister_state(id).system_state.balance();
    let execution_cost_before = test.canister_execution_cost(id);
    test.upgrade_canister(id, wasm2.clone()).unwrap_err();
    let execution_cost = test.canister_execution_cost(id) - execution_cost_before;
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        cycles_before - execution_cost,
    );
    assert_delta!(
        execution_cost,
        test.cycles_account_manager().execution_cost(
            NumInstructions::from(3 * *DROP_MEMORY_GROW_CONST_COST + *UNREACHABLE_COST)
                + wasm_compilation_cost(&wasm2),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
            test.canister_wasm_execution_mode(id),
        ),
        Cycles::new(10)
    );
}

#[test]
fn cycles_correct_if_install_succeeds() {
    let mut test = ExecutionTestBuilder::new().build();

    let wat = r#"
        (module
            (func $start
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (func (export "canister_init")
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (start $start)
            (memory 0)
        )"#;
    let wasm = wat::parse_str(wat).unwrap();

    let initial_cycles = Cycles::new(1_000_000_000_000_000);
    let id = test.create_canister(initial_cycles);

    test.install_canister(id, wasm.clone()).unwrap();
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        initial_cycles - test.canister_execution_cost(id),
    );
    assert_delta!(
        test.canister_execution_cost(id),
        test.cycles_account_manager().execution_cost(
            NumInstructions::from(6 * *DROP_MEMORY_GROW_CONST_COST) + wasm_compilation_cost(&wasm),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
            test.canister_wasm_execution_mode(id),
        ),
        Cycles::new(4)
    );
}

#[test]
fn cycles_correct_if_install_fails_at_validation() {
    let mut test = ExecutionTestBuilder::new()
        .with_rate_limiting_of_instructions()
        .build();

    let wat = r#"
        (module
            (func $start
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (func (export "canister_init")
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (start $start)
            (memory 0)
        )"#;
    let wasm = wat::parse_str(wat).unwrap();

    let initial_cycles = Cycles::new(1_000_000_000_000_000);
    let id = test.create_canister(initial_cycles);

    // Set a large value for `install_code_debit` so the installation fails due
    // to rate limiting.
    test.canister_state_mut(id)
        .scheduler_state
        .install_code_debit = NumInstructions::from(u64::MAX);

    test.install_canister(id, wasm.clone()).unwrap_err();
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        initial_cycles - test.canister_execution_cost(id),
    );
    assert_eq!(
        test.canister_execution_cost(id),
        test.cycles_account_manager().execution_cost(
            NumInstructions::from(0),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
            test.canister_wasm_execution_mode(id),
        )
    );
}

#[test]
fn cycles_correct_if_install_fails_at_start() {
    let mut test = ExecutionTestBuilder::new().build();

    let wat = r#"
        (module
            (func $start
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (unreachable)
            )
            (func (export "canister_post_upgrade")
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
            )
            (start $start)
            (memory 0)
        )"#;
    let wasm = wat::parse_str(wat).unwrap();

    let initial_cycles = Cycles::new(1_000_000_000_000_000);
    let id = test.create_canister(initial_cycles);

    test.install_canister(id, wasm.clone()).unwrap_err();
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        initial_cycles - test.canister_execution_cost(id),
    );

    assert_delta!(
        test.canister_execution_cost(id),
        test.cycles_account_manager().execution_cost(
            NumInstructions::from(3 * *DROP_MEMORY_GROW_CONST_COST) + wasm_compilation_cost(&wasm),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
            test.canister_wasm_execution_mode(id),
        ),
        Cycles::new(10)
    );
}

#[test]
fn cycles_correct_if_install_fails_at_init() {
    let mut test = ExecutionTestBuilder::new().build();

    let wat = r#"
        (module
            (func $start
                (drop (memory.grow (i32.const 1)))
            )
            (func (export "canister_init")
                (drop (memory.grow (i32.const 1)))
                (drop (memory.grow (i32.const 1)))
                (unreachable)
            )
            (start $start)
            (memory 0)
        )"#;
    let wasm = wat::parse_str(wat).unwrap();

    let initial_cycles = Cycles::new(1_000_000_000_000_000);
    let id = test.create_canister(initial_cycles);

    test.install_canister(id, wasm.clone()).unwrap_err();
    assert_eq!(
        test.canister_state(id).system_state.balance(),
        initial_cycles - test.canister_execution_cost(id),
    );
    assert_delta!(
        test.canister_execution_cost(id),
        test.cycles_account_manager().execution_cost(
            NumInstructions::from(3 * *DROP_MEMORY_GROW_CONST_COST + *UNREACHABLE_COST)
                + wasm_compilation_cost(&wasm),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
            test.canister_wasm_execution_mode(id),
        ),
        Cycles::new(10)
    );
}

#[test]
fn delete_canister_with_non_empty_input_queue_fails() {
    let mut test = ExecutionTestBuilder::new().with_manual_execution().build();

    let canister_id = test.universal_canister().unwrap();

    // Send an ingress to the canister but do not execute it.
    let _ = test.ingress_raw(canister_id, "update", vec![]);

    // Stop the canister to avoid this condition.
    test.stop_canister(canister_id);
    test.process_stopping_canisters();

    // Attempting to delete a canister with a non-empty input queue should fail.
    let err = test.delete_canister(canister_id).unwrap_err();
    assert_eq!(err.code(), ErrorCode::CanisterQueueNotEmpty);
    assert!(err.description().contains(&format!(
        "Canister {} has messages in its queues and cannot be deleted now",
        canister_id,
    )));
}

#[test]
fn update_settings_checks_freezing_threshold_for_memory_allocation() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(Cycles::new(1_000_000_000_000));

    let err = test
        .canister_update_allocations_settings(canister_id, None, Some(10 * 1024 * 1024 * 1024))
        .unwrap_err();

    assert!(
        err.description()
            .contains("Cannot increase memory allocation to 10.00 GiB due to insufficient cycles."),
        "{}",
        err.description(),
    );
    assert_eq!(err.code(), ErrorCode::InsufficientCyclesInMemoryAllocation);
}

#[test]
fn update_settings_checks_freezing_threshold_for_compute_allocation() {
    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(Cycles::new(1_000_000_000_000));

    let err = test
        .canister_update_allocations_settings(canister_id, Some(50), None)
        .unwrap_err();

    assert!(
        err.description()
            .contains("Cannot increase compute allocation to 50% due to insufficient cycles."),
        "{}",
        err.description(),
    );
    assert_eq!(err.code(), ErrorCode::InsufficientCyclesInComputeAllocation);
}

#[test]
fn create_canister_checks_freezing_threshold_for_memory_allocation() {
    let mut test = ExecutionTestBuilder::new().build();

    let err = test
        .create_canister_with_allocation(
            Cycles::new(1_000_000_000_000),
            None,
            Some(10 * 1024 * 1024 * 1024),
        )
        .unwrap_err();

    assert!(
        err.description()
            .contains("Cannot increase memory allocation to 10.00 GiB due to insufficient cycles."),
        "{}",
        err.description(),
    );
    assert_eq!(err.code(), ErrorCode::InsufficientCyclesInMemoryAllocation);
}

#[test]
fn create_canister_checks_freezing_threshold_for_compute_allocation() {
    let mut test = ExecutionTestBuilder::new().build();

    let err = test
        .create_canister_with_allocation(Cycles::new(1_000_000_000_000), Some(50), None)
        .unwrap_err();

    assert!(
        err.description()
            .contains("Cannot increase compute allocation to 50% due to insufficient cycles."),
        "{}",
        err.description(),
    );
    assert_eq!(err.code(), ErrorCode::InsufficientCyclesInComputeAllocation);
}

#[test]
fn system_subnet_does_not_check_for_freezing_threshold_on_allocation_changes() {
    let mut test = ExecutionTestBuilder::new()
        .with_subnet_type(SubnetType::System)
        .build();

    let canister_id = test
        .create_canister_with_allocation(Cycles::new(1_000_000_000_000), Some(50), None)
        .unwrap();
    test.canister_update_allocations_settings(canister_id, Some(0), Some(0))
        .unwrap();

    let canister_id = test
        .create_canister_with_allocation(
            Cycles::new(1_000_000_000_000),
            None,
            Some(10 * 1024 * 1024 * 1024),
        )
        .unwrap();
    test.canister_update_allocations_settings(canister_id, Some(0), Some(0))
        .unwrap();

    let canister_id = test.create_canister(Cycles::new(1_000_000_000_000));
    test.canister_update_allocations_settings(canister_id, Some(50), None)
        .unwrap();
    test.canister_update_allocations_settings(canister_id, Some(0), Some(0))
        .unwrap();

    let canister_id = test.create_canister(Cycles::new(1_000_000_000_000));
    test.canister_update_allocations_settings(canister_id, None, Some(10 * 1024 * 1024 * 1024))
        .unwrap();
    test.canister_update_allocations_settings(canister_id, Some(0), Some(0))
        .unwrap();
}

#[test]
fn install_does_not_reserve_cycles_when_memory_allocation_is_set() {
    cycles_reserved_for_app_and_verified_app_subnets(|subnet_type| {
        const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
        const CAPACITY: u64 = 20_000_000_000;
        const THRESHOLD: u64 = CAPACITY / 2;
        const USAGE: u64 = CAPACITY - THRESHOLD;

        let mut test = ExecutionTestBuilder::new()
            .with_subnet_type(subnet_type)
            .with_subnet_execution_memory(CAPACITY as i64)
            .with_subnet_memory_reservation(0)
            .with_subnet_memory_threshold(THRESHOLD as i64)
            .build();

        test.create_canister_with_allocation(CYCLES, None, Some(THRESHOLD))
            .unwrap();

        let subnet_memory_usage =
            CAPACITY - test.subnet_available_memory().get_execution_memory() as u64;

        let canister_id = test
            .create_canister_with_settings(
                CYCLES,
                CanisterSettingsArgsBuilder::new()
                    .with_memory_allocation(USAGE)
                    .with_reserved_cycles_limit(CYCLES.get())
                    .build(),
            )
            .unwrap();

        let memory_usage_after = NumBytes::from(USAGE);
        let reserved_cycles = test
            .canister_state(canister_id)
            .system_state
            .reserved_balance();
        assert_gt!(reserved_cycles, Cycles::zero());
        assert_eq!(
            reserved_cycles,
            test.cycles_account_manager().storage_reservation_cycles(
                memory_usage_after,
                &ResourceSaturation::new(subnet_memory_usage, THRESHOLD, CAPACITY),
                test.subnet_size(),
                CanisterCyclesCostSchedule::Normal,
            )
        );

        let wat = r#"
            (module
                (import "ic0" "stable64_grow" (func $stable64_grow (param i64) (result i64)))
                (func (export "canister_post_upgrade")
                    (if (i64.eq (call $stable64_grow (i64.const 150000)) (i64.const -1))
                      (then (unreachable))
                    )
                )
                (memory 0)
            )"#;

        let wasm_binary = wat::parse_str(wat).unwrap();

        let balance_before = test.canister_state(canister_id).system_state.balance();
        test.install_canister(canister_id, wasm_binary).unwrap();
        let balance_after = test.canister_state(canister_id).system_state.balance();

        let new_reserved_cycles = test
            .canister_state(canister_id)
            .system_state
            .reserved_balance();

        // The reserved balance shouldn't change because the canister has already
        // reserved memory allocation during its creation.
        assert_eq!(reserved_cycles, new_reserved_cycles);
        assert_eq!(
            balance_after,
            balance_before - test.canister_execution_cost(canister_id)
        );
    });
}

#[test]
fn install_reserves_cycles_on_memory_grow() {
    cycles_reserved_for_app_and_verified_app_subnets(|subnet_type| {
        const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
        const CAPACITY: u64 = 20_000_000_000;
        const THRESHOLD: u64 = CAPACITY / 2;

        let mut test = ExecutionTestBuilder::new()
            .with_subnet_type(subnet_type)
            .with_subnet_execution_memory(CAPACITY as i64)
            .with_subnet_memory_reservation(0)
            .with_subnet_memory_threshold(THRESHOLD as i64)
            .build();

        test.create_canister_with_allocation(CYCLES, None, Some(THRESHOLD))
            .unwrap();

        let canister_id = test
            .create_canister_with_settings(
                CYCLES,
                CanisterSettingsArgsBuilder::new()
                    .with_reserved_cycles_limit(CYCLES.get())
                    .build(),
            )
            .unwrap();

        let wat = r#"
            (module
                (import "ic0" "stable64_grow" (func $stable64_grow (param i64) (result i64)))
                (func (export "canister_init")
                    (if (i64.eq (call $stable64_grow (i64.const 150000)) (i64.const -1))
                      (then (unreachable))
                    )
                )
                (memory 0)
            )"#;

        let wasm_binary = wat::parse_str(wat).unwrap();

        let subnet_memory_usage =
            CAPACITY - test.subnet_available_memory().get_execution_memory() as u64;
        let memory_usage_before = test.canister_state(canister_id).execution_memory_usage();
        let balance_before = test.canister_state(canister_id).system_state.balance();
        test.install_canister(canister_id, wasm_binary).unwrap();
        let balance_after = test.canister_state(canister_id).system_state.balance();
        let memory_usage_after = test.canister_state(canister_id).execution_memory_usage();

        let reserved_cycles = test
            .canister_state(canister_id)
            .system_state
            .reserved_balance();

        assert_gt!(reserved_cycles, Cycles::zero());
        assert_eq!(
            reserved_cycles,
            test.cycles_account_manager().storage_reservation_cycles(
                memory_usage_after - memory_usage_before,
                &ResourceSaturation::new(subnet_memory_usage, THRESHOLD, CAPACITY),
                test.subnet_size(),
                CanisterCyclesCostSchedule::Normal,
            )
        );

        assert_ge!(
            balance_before - balance_after,
            reserved_cycles,
            "Unexpected balance change: {} >= {}",
            balance_before - balance_after,
            reserved_cycles,
        );
    });
}

#[test]
fn upgrade_reserves_cycles_on_memory_grow() {
    cycles_reserved_for_app_and_verified_app_subnets(|subnet_type| {
        const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
        const CAPACITY: u64 = 20_000_000_000;
        const THRESHOLD: u64 = CAPACITY / 2;

        let mut test = ExecutionTestBuilder::new()
            .with_subnet_type(subnet_type)
            .with_subnet_execution_memory(CAPACITY as i64)
            .with_subnet_memory_reservation(0)
            .with_subnet_memory_threshold(THRESHOLD as i64)
            .build();

        test.create_canister_with_allocation(CYCLES, None, Some(THRESHOLD))
            .unwrap();

        let canister_id = test
            .create_canister_with_settings(
                CYCLES,
                CanisterSettingsArgsBuilder::new()
                    .with_reserved_cycles_limit(CYCLES.get())
                    .build(),
            )
            .unwrap();

        let wat = r#"
        (module
            (import "ic0" "stable64_grow" (func $stable64_grow (param i64) (result i64)))
            (func (export "canister_post_upgrade")
                (if (i64.eq (call $stable64_grow (i64.const 150000)) (i64.const -1))
                  (then (unreachable))
                )
            )
            (memory 0)
        )"#;

        let wasm_binary = wat::parse_str(wat).unwrap();

        test.install_canister(canister_id, wasm_binary.clone())
            .unwrap();

        let subnet_memory_usage =
            CAPACITY - test.subnet_available_memory().get_execution_memory() as u64;
        let memory_usage_before = test.canister_state(canister_id).execution_memory_usage();
        let balance_before = test.canister_state(canister_id).system_state.balance();
        let reserved_cycles_before = test
            .canister_state(canister_id)
            .system_state
            .reserved_balance();
        test.upgrade_canister(canister_id, wasm_binary).unwrap();
        let balance_after = test.canister_state(canister_id).system_state.balance();
        let memory_usage_after = test.canister_state(canister_id).execution_memory_usage();

        let reserved_cycles = test
            .canister_state(canister_id)
            .system_state
            .reserved_balance();

        assert_gt!(reserved_cycles, Cycles::zero());
        assert_eq!(
            reserved_cycles - reserved_cycles_before,
            test.cycles_account_manager().storage_reservation_cycles(
                memory_usage_after - memory_usage_before,
                &ResourceSaturation::new(subnet_memory_usage, THRESHOLD, CAPACITY),
                test.subnet_size(),
                CanisterCyclesCostSchedule::Normal,
            )
        );

        assert_ge!(
            balance_before - balance_after,
            reserved_cycles,
            "Unexpected balance change: {} >= {}",
            balance_before - balance_after,
            reserved_cycles,
        );
    });
}

#[test]
fn install_does_not_reserve_cycles_on_system_subnet() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
    const CAPACITY: u64 = 4_000_000_000;
    const THRESHOLD: u64 = CAPACITY / 2;
    const USAGE: u64 = CAPACITY - THRESHOLD;

    let mut test = ExecutionTestBuilder::new()
        .with_subnet_type(SubnetType::System)
        .with_subnet_execution_memory(CAPACITY as i64)
        .with_subnet_memory_reservation(0)
        .with_subnet_memory_threshold(THRESHOLD as i64)
        .build();

    // Create a canister with a memory allocation of `THRESHOLD` bytes.
    let canister_id = test
        .create_canister_with_allocation(CYCLES, None, Some(THRESHOLD))
        .unwrap();
    test.install_canister(canister_id, UNIVERSAL_CANISTER_WASM.to_vec())
        .unwrap();

    // Create a second canister that attempts to grow its memory above the threshold
    // where reservations would trigger. Because it's a system subnet we expect
    // that no cycles reservation will be made.
    let canister_id = test.create_canister(CYCLES);
    let balance_before = test.canister_state(canister_id).system_state.balance();
    test.install_canister(
        canister_id,
        wat_canister()
            .init(wat_fn().stable_grow((USAGE / WASM_PAGE_SIZE_IN_BYTES) as i32 - 1))
            .build_wasm(),
    )
    .unwrap();
    let balance_after = test.canister_state(canister_id).system_state.balance();

    // Message execution fee is an order of a few million cycles.
    assert_lt!(balance_before - balance_after, Cycles::new(1_000_000_000));

    let reserved_cycles = test
        .canister_state(canister_id)
        .system_state
        .reserved_balance();
    assert_eq!(reserved_cycles, Cycles::zero());
}

#[test]
fn create_canister_reserves_cycles_for_memory_allocation() {
    cycles_reserved_for_app_and_verified_app_subnets(|subnet_type| {
        const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
        const CAPACITY: u64 = 20_000_000_000;
        const THRESHOLD: u64 = CAPACITY / 2;
        const USAGE: u64 = CAPACITY - THRESHOLD;

        let mut test = ExecutionTestBuilder::new()
            .with_subnet_type(subnet_type)
            .with_subnet_execution_memory(CAPACITY as i64)
            .with_subnet_memory_reservation(0)
            .with_subnet_memory_threshold(THRESHOLD as i64)
            .build();

        test.create_canister_with_allocation(CYCLES, None, Some(USAGE))
            .unwrap();

        let subnet_memory_usage =
            CAPACITY - test.subnet_available_memory().get_execution_memory() as u64;

        let balance_before = CYCLES;
        let canister_id = test
            .create_canister_with_settings(
                balance_before,
                CanisterSettingsArgsBuilder::new()
                    .with_memory_allocation(USAGE)
                    .with_reserved_cycles_limit(CYCLES.get())
                    .build(),
            )
            .unwrap();
        let balance_after = test.canister_state(canister_id).system_state.balance();

        assert_eq!(
            test.canister_state(canister_id)
                .memory_allocation()
                .bytes()
                .get(),
            USAGE,
        );

        let reserved_cycles = test
            .canister_state(canister_id)
            .system_state
            .reserved_balance();

        assert_gt!(reserved_cycles, Cycles::zero());
        assert_eq!(
            reserved_cycles,
            test.cycles_account_manager().storage_reservation_cycles(
                NumBytes::new(USAGE),
                &ResourceSaturation::new(subnet_memory_usage, THRESHOLD, CAPACITY),
                test.subnet_size(),
                CanisterCyclesCostSchedule::Normal,
            )
        );

        assert_ge!(
            balance_before - balance_after,
            reserved_cycles,
            "Unexpected balance change: {} >= {}",
            balance_before - balance_after,
            reserved_cycles,
        );
    });
}

#[test]
fn update_settings_reserves_cycles_for_memory_allocation() {
    cycles_reserved_for_app_and_verified_app_subnets(|subnet_type| {
        const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
        const CAPACITY: u64 = 20_000_000_000;
        const THRESHOLD: u64 = CAPACITY / 2;
        const USAGE: u64 = CAPACITY - THRESHOLD;

        let mut test = ExecutionTestBuilder::new()
            .with_subnet_type(subnet_type)
            .with_subnet_execution_memory(CAPACITY as i64)
            .with_subnet_memory_reservation(0)
            .with_subnet_memory_threshold(THRESHOLD as i64)
            .build();

        test.create_canister_with_allocation(CYCLES, None, Some(THRESHOLD))
            .unwrap();

        let canister_id = test
            .create_canister_with_settings(
                CYCLES,
                CanisterSettingsArgsBuilder::new()
                    .with_reserved_cycles_limit(CYCLES.get())
                    .build(),
            )
            .unwrap();

        let subnet_memory_usage =
            CAPACITY - test.subnet_available_memory().get_execution_memory() as u64;

        // TODO(RUN-745): This should be `execution_memory_usage()`.
        let memory_usage_before = test.canister_state(canister_id).memory_usage();
        let balance_before = test.canister_state(canister_id).system_state.balance();
        test.canister_update_allocations_settings(canister_id, None, Some(USAGE))
            .unwrap();
        let balance_after = test.canister_state(canister_id).system_state.balance();
        let memory_usage_after = NumBytes::new(USAGE);

        assert_eq!(
            test.canister_state(canister_id)
                .memory_allocation()
                .bytes()
                .get(),
            USAGE
        );

        let reserved_cycles = test
            .canister_state(canister_id)
            .system_state
            .reserved_balance();

        assert_gt!(reserved_cycles, Cycles::zero());
        assert_eq!(
            reserved_cycles,
            test.cycles_account_manager().storage_reservation_cycles(
                memory_usage_after - memory_usage_before,
                &ResourceSaturation::new(subnet_memory_usage, THRESHOLD, CAPACITY),
                test.subnet_size(),
                CanisterCyclesCostSchedule::Normal,
            )
        );

        assert_ge!(
            balance_before - balance_after,
            reserved_cycles,
            "Unexpected balance change: {} >= {}",
            balance_before - balance_after,
            reserved_cycles,
        );
    });
}

#[test]
fn test_upgrade_with_skip_pre_upgrade_preserves_stable_memory() {
    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = test.universal_canister().unwrap();

    test.ingress(
        canister_id,
        "update",
        wasm().set_pre_upgrade(wasm().trap()).reply().build(),
    )
    .unwrap();

    let data = [1, 2, 3, 5, 8, 13];
    let update = wasm()
        .stable_grow(1)
        .stable_write(42, &data)
        .reply()
        .build();
    let result = test.ingress(canister_id, "update", update);
    let reply = get_reply(result);
    assert_eq!(reply, vec![] as Vec<u8>);

    // Check that the upgrade of the canister succeeds if the pre_upgrade is skipped.
    test.upgrade_canister_v2(
        canister_id,
        UNIVERSAL_CANISTER_WASM.to_vec(),
        CanisterUpgradeOptions {
            skip_pre_upgrade: Some(true),
            wasm_memory_persistence: None,
        },
    )
    .unwrap();

    // Set pre_upgrade again after the previous upgrade.
    test.ingress(
        canister_id,
        "update",
        wasm().set_pre_upgrade(wasm().trap()).reply().build(),
    )
    .unwrap();

    // Check that the canister traps if pre_upgrade is executed.
    let err = test
        .upgrade_canister_v2(
            canister_id,
            UNIVERSAL_CANISTER_WASM.to_vec(),
            CanisterUpgradeOptions {
                skip_pre_upgrade: None,
                wasm_memory_persistence: None,
            },
        )
        .unwrap_err();
    assert_eq!(ErrorCode::CanisterCalledTrap, err.code());

    let query = wasm()
        .stable_read(42, data.len() as u32)
        .append_and_reply()
        .build();
    let result = test.ingress(canister_id, "query", query);
    let reply = get_reply(result);
    assert_eq!(reply, data);
}

#[test]
fn resource_saturation_scaling_works_in_create_canister() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
    const CAPACITY: u64 = 20_000_000_000;
    const THRESHOLD: u64 = CAPACITY / 2;
    const USAGE: u64 = CAPACITY - THRESHOLD;
    const SCALING: u64 = 4;

    let mut test = ExecutionTestBuilder::new()
        .with_subnet_execution_memory(CAPACITY as i64)
        .with_subnet_memory_reservation(0)
        .with_subnet_memory_threshold(THRESHOLD as i64)
        .with_resource_saturation_scaling(SCALING as usize)
        .build();

    test.create_canister_with_allocation(CYCLES, None, Some(THRESHOLD / SCALING))
        .unwrap();

    let subnet_memory_usage =
        CAPACITY - test.subnet_available_memory().get_execution_memory() as u64;

    let balance_before = CYCLES;
    let canister_id = test
        .create_canister_with_settings(
            balance_before,
            CanisterSettingsArgsBuilder::new()
                .with_memory_allocation(USAGE)
                .with_reserved_cycles_limit(CYCLES.get())
                .build(),
        )
        .unwrap();

    let balance_after = test.canister_state(canister_id).system_state.balance();

    assert_eq!(
        test.canister_state(canister_id)
            .memory_allocation()
            .bytes()
            .get(),
        USAGE,
    );

    let reserved_cycles = test
        .canister_state(canister_id)
        .system_state
        .reserved_balance();

    assert_gt!(reserved_cycles, Cycles::zero());
    assert_eq!(
        reserved_cycles,
        test.cycles_account_manager().storage_reservation_cycles(
            NumBytes::new(USAGE),
            &ResourceSaturation::new(
                subnet_memory_usage / SCALING,
                THRESHOLD / SCALING,
                CAPACITY / SCALING
            ),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
        )
    );

    assert_ge!(
        balance_before - balance_after,
        reserved_cycles,
        "Unexpected balance change: {} >= {}",
        balance_before - balance_after,
        reserved_cycles,
    );
}

#[test]
fn resource_saturation_scaling_works_in_install_code() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
    const CAPACITY: u64 = 20_000_000_000;
    const THRESHOLD: u64 = CAPACITY / 2;
    const SCALING: u64 = 4;

    let mut test = ExecutionTestBuilder::new()
        .with_subnet_execution_memory(CAPACITY as i64)
        .with_subnet_memory_reservation(0)
        .with_subnet_memory_threshold(THRESHOLD as i64)
        .with_resource_saturation_scaling(SCALING as usize)
        .build();

    test.create_canister_with_allocation(CYCLES, None, Some(THRESHOLD / SCALING))
        .unwrap();

    let canister_id = test
        .create_canister_with_settings(
            CYCLES,
            CanisterSettingsArgsBuilder::new()
                .with_reserved_cycles_limit(CYCLES.get())
                .build(),
        )
        .unwrap();

    let wat = r#"
        (module
            (import "ic0" "stable64_grow" (func $stable64_grow (param i64) (result i64)))
            (func (export "canister_init")
                (if (i64.eq (call $stable64_grow (i64.const 150000)) (i64.const -1))
                  (then (unreachable))
                )
            )
            (memory 0)
        )"#;

    let wasm_binary = wat::parse_str(wat).unwrap();

    let subnet_memory_usage =
        CAPACITY - test.subnet_available_memory().get_execution_memory() as u64;
    let memory_usage_before = test.canister_state(canister_id).execution_memory_usage();
    let balance_before = test.canister_state(canister_id).system_state.balance();
    test.install_canister(canister_id, wasm_binary).unwrap();
    let balance_after = test.canister_state(canister_id).system_state.balance();
    let memory_usage_after = test.canister_state(canister_id).execution_memory_usage();

    let reserved_cycles = test
        .canister_state(canister_id)
        .system_state
        .reserved_balance();

    assert_gt!(reserved_cycles, Cycles::zero());
    assert_eq!(
        reserved_cycles,
        test.cycles_account_manager().storage_reservation_cycles(
            memory_usage_after - memory_usage_before,
            &ResourceSaturation::new(
                subnet_memory_usage / SCALING,
                THRESHOLD / SCALING,
                CAPACITY / SCALING
            ),
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
        )
    );

    assert_ge!(
        balance_before - balance_after,
        reserved_cycles,
        "Unexpected balance change: {} >= {}",
        balance_before - balance_after,
        reserved_cycles,
    );
}

#[test]
fn update_settings_respects_reserved_cycles_limit_on_memory_allocation() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
    const CAPACITY: u64 = 20_000_000_000;
    const USAGE: u64 = 10_000_000_000;

    let mut test = ExecutionTestBuilder::new()
        .with_subnet_execution_memory(CAPACITY as i64)
        .with_subnet_memory_reservation(0)
        .with_subnet_memory_threshold(0)
        .build();

    let canister_id = test.create_canister(CYCLES);

    test.canister_state_mut(canister_id)
        .system_state
        .set_reserved_balance_limit(Cycles::new(1));

    let err = test
        .canister_update_allocations_settings(canister_id, None, Some(USAGE))
        .unwrap_err();

    assert_eq!(
        err.code(),
        ErrorCode::ReservedCyclesLimitExceededInMemoryAllocation
    );
    assert!(err
        .description()
        .contains("Cannot increase memory allocation"));
    assert!(err
        .description()
        .contains("due to its reserved cycles limit"));
}

#[test]
fn install_respects_reserved_cycles_limit_on_memory_grow() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
    const CAPACITY: u64 = 20_000_000_000;

    let mut test = ExecutionTestBuilder::new()
        .with_subnet_execution_memory(CAPACITY as i64)
        .with_subnet_memory_reservation(0)
        .with_subnet_memory_threshold(0)
        .build();

    let canister_id = test.create_canister(CYCLES);

    let wat = r#"
        (module
            (import "ic0" "stable64_grow" (func $stable64_grow (param i64) (result i64)))
            (func (export "canister_init")
                (if (i64.eq (call $stable64_grow (i64.const 150000)) (i64.const -1))
                  (then (unreachable))
                )
            )
            (memory 0)
        )"#;

    let wasm_binary = wat::parse_str(wat).unwrap();

    test.canister_state_mut(canister_id)
        .system_state
        .set_reserved_balance_limit(Cycles::new(1));

    let err = test.install_canister(canister_id, wasm_binary).unwrap_err();

    assert_eq!(
        err.code(),
        ErrorCode::ReservedCyclesLimitExceededInMemoryGrow
    );
    assert!(err.description().contains("Canister cannot grow memory by"));
    assert!(err
        .description()
        .contains("due to its reserved cycles limit"));
}

#[test]
fn create_canister_fails_with_reserved_cycles_limit_exceeded() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
    const CAPACITY: u64 = 20_000_000_000;

    let mut test = ExecutionTestBuilder::new()
        .with_subnet_execution_memory(CAPACITY as i64)
        .with_subnet_memory_reservation(0)
        .with_subnet_memory_threshold(0)
        .build();

    let uc = test
        .canister_from_cycles_and_binary(CYCLES, UNIVERSAL_CANISTER_WASM.to_vec())
        .unwrap();

    // Set the memory allocation to exceed the reserved cycles limit.
    let settings = CanisterSettingsArgsBuilder::new()
        .with_memory_allocation(1_000_000)
        .with_reserved_cycles_limit(1)
        .build();
    let args = CreateCanisterArgs {
        settings: Some(settings),
        sender_canister_version: None,
    };

    let create_canister = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(args.encode())
                .on_reject(wasm().reject_message().reject()),
            Cycles::new(CYCLES.get() / 2),
        )
        .build();

    let result = test.ingress(uc, "update", create_canister).unwrap();

    let err_msg = match result {
        WasmResult::Reply(_) => unreachable!("Unexpected reply, expected reject"),
        WasmResult::Reject(err_msg) => err_msg,
    };

    assert!(err_msg.contains("Cannot increase memory allocation"));
    assert!(err_msg.contains("due to its reserved cycles limit"));
}

#[test]
fn create_canister_can_set_reserved_cycles_limit() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
    const CAPACITY: u64 = 20_000_000_000;

    let mut test = ExecutionTestBuilder::new()
        .with_subnet_execution_memory(CAPACITY as i64)
        .with_subnet_memory_reservation(0)
        .with_subnet_memory_threshold(0)
        .build();

    let uc = test
        .canister_from_cycles_and_binary(CYCLES, UNIVERSAL_CANISTER_WASM.to_vec())
        .unwrap();

    // Since we are not setting the memory allocation and the memory usage of an
    // empty canister is zero, setting the reserved cycles limit should succeed.
    let settings = CanisterSettingsArgsBuilder::new()
        .with_reserved_cycles_limit(1)
        .build();
    let args = CreateCanisterArgs {
        settings: Some(settings),
        sender_canister_version: None,
    };

    let create_canister = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(args.encode())
                .on_reject(wasm().reject_message().reject()),
            Cycles::new(CYCLES.get() / 2),
        )
        .build();

    let result = test.ingress(uc, "update", create_canister);
    let reply = get_reply(result);
    let canister_id = Decode!(reply.as_slice(), CanisterIdRecord)
        .unwrap()
        .get_canister_id();

    assert_eq!(
        test.canister_state(canister_id)
            .system_state
            .reserved_balance_limit(),
        Some(Cycles::new(1))
    );
}

#[test]
fn create_canister_sets_default_reserved_cycles_limit() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    let mut test = ExecutionTestBuilder::new().build();

    let uc = test
        .canister_from_cycles_and_binary(CYCLES, UNIVERSAL_CANISTER_WASM.to_vec())
        .unwrap();

    let args = CreateCanisterArgs {
        settings: None,
        sender_canister_version: None,
    };

    let create_canister = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::CreateCanister,
            call_args()
                .other_side(args.encode())
                .on_reject(wasm().reject_message().reject()),
            Cycles::new(CYCLES.get() / 2),
        )
        .build();

    let result = test.ingress(uc, "update", create_canister);
    let reply = get_reply(result);
    let canister_id = CanisterIdRecord::decode(&reply).unwrap().get_canister_id();

    assert_eq!(
        test.canister_state(canister_id)
            .system_state
            .reserved_balance_limit(),
        Some(
            test.cycles_account_manager()
                .default_reserved_balance_limit()
        )
    );
}

#[test]
fn update_settings_can_set_reserved_cycles_limit() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
    const CAPACITY: u64 = 20_000_000_000;

    let mut test = ExecutionTestBuilder::new()
        .with_subnet_execution_memory(CAPACITY as i64)
        .with_subnet_memory_reservation(0)
        .with_subnet_memory_threshold(0)
        .build();

    let canister_id = test
        .create_canister_with_settings(
            CYCLES,
            CanisterSettingsArgsBuilder::new()
                .with_reserved_cycles_limit(1)
                .build(),
        )
        .unwrap();

    assert_eq!(
        test.canister_state(canister_id)
            .system_state
            .reserved_balance_limit(),
        Some(Cycles::new(1))
    );
}

#[test]
fn canister_status_contains_reserved_cycles() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
    const CAPACITY: u64 = 20_000_000_000;

    let mut test = ExecutionTestBuilder::new()
        .with_subnet_execution_memory(CAPACITY as i64)
        .with_subnet_memory_reservation(0)
        .with_subnet_memory_threshold(0)
        .build();

    let canister_id = test
        .create_canister_with_allocation(CYCLES, None, Some(1_000_000))
        .unwrap();
    let result = test.canister_status(canister_id);
    let reply = get_reply(result);
    let status = Decode!(reply.as_slice(), CanisterStatusResultV2).unwrap();
    assert_eq!(
        status.reserved_cycles(),
        test.cycles_account_manager()
            .storage_reservation_cycles(
                NumBytes::new(1_000_000),
                &ResourceSaturation::new(0, 0, CAPACITY),
                test.subnet_size(),
                CanisterCyclesCostSchedule::Normal,
            )
            .get()
    );
    assert_eq!(
        status.reserved_cycles(),
        test.canister_state(canister_id)
            .system_state
            .reserved_balance()
            .get()
    );
    assert_lt!(0, status.reserved_cycles());
}

#[test]
fn canister_status_contains_reserved_cycles_limit() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(CYCLES);
    let result = test.canister_status(canister_id);
    let reply = get_reply(result);
    let status = CanisterStatusResultV2::decode(&reply).unwrap();
    assert_eq!(
        status.settings().reserved_cycles_limit(),
        candid::Nat::from(
            test.cycles_account_manager()
                .default_reserved_balance_limit()
                .get()
        ),
    );

    test.canister_update_reserved_cycles_limit(canister_id, Cycles::new(42))
        .unwrap();

    let result = test.canister_status(canister_id);
    let reply = get_reply(result);
    let status = CanisterStatusResultV2::decode(&reply).unwrap();
    assert_eq!(
        status.settings().reserved_cycles_limit(),
        candid::Nat::from(42_u32),
    );
}

#[test]
fn upload_chunk_works_from_white_list() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(CYCLES);

    let chunk = vec![1, 2, 3, 4, 5];
    let reply = UploadChunkReply {
        hash: ic_crypto_sha2::Sha256::hash(&chunk).to_vec(),
    }
    .encode();

    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk,
    };

    let result = test.subnet_message("upload_chunk", upload_args.encode());

    assert_eq!(result, Ok(WasmResult::Reply(reply)));
}

#[test]
fn upload_chunk_works_from_controller() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(CYCLES);
    let uc = test
        .canister_from_cycles_and_binary(CYCLES, UNIVERSAL_CANISTER_WASM.to_vec())
        .unwrap();
    test.set_controller(canister_id, uc.into()).unwrap();

    let chunk = vec![1, 2, 3, 4, 5];
    let reply = UploadChunkReply {
        hash: ic_crypto_sha2::Sha256::hash(&chunk).to_vec(),
    }
    .encode();

    let args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk,
    };

    let upload_chunk = wasm()
        .call_with_cycles(
            CanisterId::ic_00(),
            Method::UploadChunk,
            call_args()
                .other_side(args.encode())
                .on_reject(wasm().reject_message().reject()),
            Cycles::new(CYCLES.get() / 2),
        )
        .build();

    let result = test.ingress(uc, "update", upload_chunk);
    assert_eq!(result, Ok(WasmResult::Reply(reply)));
}

#[test]
fn chunk_store_methods_fail_from_non_controller() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(CYCLES);
    let uc = test
        .canister_from_cycles_and_binary(CYCLES, UNIVERSAL_CANISTER_WASM.to_vec())
        .unwrap();

    let methods = [
        (
            Method::UploadChunk,
            UploadChunkArgs {
                canister_id: canister_id.into(),
                chunk: vec![1, 2, 3, 4, 5],
            }
            .encode(),
        ),
        (
            Method::ClearChunkStore,
            ClearChunkStoreArgs {
                canister_id: canister_id.into(),
            }
            .encode(),
        ),
        (
            Method::StoredChunks,
            StoredChunksArgs {
                canister_id: canister_id.into(),
            }
            .encode(),
        ),
    ];

    for (method, args) in methods {
        let wasm = wasm()
            .call_with_cycles(
                CanisterId::ic_00(),
                method,
                call_args()
                    .other_side(args)
                    .on_reject(wasm().reject_message().reject()),
                Cycles::new(CYCLES.get() / 2),
            )
            .build();

        let result = test.ingress(uc, "update", wasm);
        let expected_err = format!(
            "Only the controllers of the canister {} can control it.",
            canister_id
        );
        match result {
            Ok(WasmResult::Reject(reject)) => {
                assert!(
                    reject.contains(&expected_err),
                    "Reject \"{reject}\" does not contain expected error \"{expected_err}\""
                );
            }
            other => panic!("Expected reject, but got {:?}", other),
        }
    }
}

#[test]
fn upload_chunk_fails_when_allocation_exceeded() {
    /// Return the number of bytes of canister memory used from updating the
    /// canister allocation settings.
    fn history_memory_usage_from_one_settings_update() -> NumBytes {
        const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

        let mut test = ExecutionTestBuilder::new().build();

        let canister_id = test.create_canister(CYCLES);
        // Reserve enough memory for just one chunk. Include an extra 40 KiB for canister history.
        let chunk_size = wasm_chunk_store::chunk_size();
        test.canister_update_allocations_settings(
            canister_id,
            None,
            Some(chunk_size.get() + 40 * 1024),
        )
        .unwrap();

        test.canister_state(canister_id).memory_usage()
    }

    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(CYCLES);
    let memory_needed_for_history = history_memory_usage_from_one_settings_update();
    // Reserve enough memory for just one chunk. Include an extra 40 KiB for canister history.
    let chunk_size = wasm_chunk_store::chunk_size();
    test.canister_update_allocations_settings(
        canister_id,
        None,
        Some(chunk_size.get() + memory_needed_for_history.get()),
    )
    .unwrap();

    // First chunk upload succeeds
    let chunk = vec![1, 2, 3, 4, 5];
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk,
    };
    let result = test.subnet_message("upload_chunk", upload_args.encode());
    assert!(result.is_ok());
    let initial_subnet_available_memory = test.subnet_available_memory();

    // Uploading the same chunk again succeeds.
    let result = test.subnet_message("upload_chunk", upload_args.encode());
    assert!(result.is_ok());
    assert_eq!(
        test.subnet_available_memory(),
        initial_subnet_available_memory
    );

    // Second chunk upload fails
    let chunk = vec![4, 4];
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk,
    };
    let result = test.subnet_message("upload_chunk", upload_args.encode());
    let error_code = result.unwrap_err().code();
    assert_eq!(error_code, ErrorCode::InsufficientMemoryAllocation);

    assert_eq!(
        test.subnet_available_memory(),
        initial_subnet_available_memory
    );
}

#[test]
fn upload_chunk_fails_when_subnet_memory_exceeded() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    // Create subnet with only enough memory for one chunk.
    let chunk_size = wasm_chunk_store::chunk_size();
    let default_subnet_memory_reservation = Config::default().subnet_memory_reservation;
    let mut test = ExecutionTestBuilder::new()
        .with_subnet_execution_memory(
            (default_subnet_memory_reservation.get() + chunk_size.get()) as i64,
        )
        .build();

    let canister_id = test.create_canister(CYCLES);

    // First chunk upload succeeds
    let chunk = vec![1, 2, 3, 4, 5];
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk,
    };
    let result = test.subnet_message("upload_chunk", upload_args.encode());
    assert!(result.is_ok());
    let initial_subnet_available_memory = test.subnet_available_memory();

    // Uploading the same chunk again succeeds.
    let result = test.subnet_message("upload_chunk", upload_args.encode());
    assert!(result.is_ok());
    assert_eq!(
        test.subnet_available_memory(),
        initial_subnet_available_memory
    );

    // Second chunk upload fails
    let chunk = vec![4, 4];
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk,
    };
    let result = test.subnet_message("upload_chunk", upload_args.encode());
    let error_code = result.unwrap_err().code();
    assert_eq!(error_code, ErrorCode::SubnetOversubscribed);

    assert_eq!(
        test.subnet_available_memory(),
        initial_subnet_available_memory
    );
}

#[test]
fn upload_chunk_counts_to_memory_usage() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
    let chunk_size = wasm_chunk_store::chunk_size();

    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(CYCLES);

    let initial_memory_usage = test.canister_state(canister_id).memory_usage();
    let initial_subnet_available_memory = test.subnet_available_memory().get_execution_memory();

    // Check memory usage after one chunk uploaded.
    let chunk = vec![1, 2, 3, 4, 5];
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk,
    };
    let result = test.subnet_message("upload_chunk", upload_args.encode());
    assert!(result.is_ok());
    assert_eq!(
        test.canister_state(canister_id).memory_usage(),
        chunk_size + initial_memory_usage
    );
    assert_eq!(
        test.subnet_available_memory().get_execution_memory(),
        initial_subnet_available_memory - chunk_size.get() as i64
    );

    // Check memory usage after uploading the same chunk again.
    let result = test.subnet_message("upload_chunk", upload_args.encode());
    assert!(result.is_ok());
    assert_eq!(
        test.canister_state(canister_id).memory_usage(),
        chunk_size + initial_memory_usage
    );
    assert_eq!(
        test.subnet_available_memory().get_execution_memory(),
        initial_subnet_available_memory - chunk_size.get() as i64
    );

    // Check memory usage after two chunks uploaded.
    let chunk = vec![4; 1000];
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk,
    };
    let result = test.subnet_message("upload_chunk", upload_args.encode());
    assert!(result.is_ok());
    assert_eq!(
        test.canister_state(canister_id).memory_usage(),
        NumBytes::from(2 * chunk_size.get()) + initial_memory_usage
    );
    assert_eq!(
        test.subnet_available_memory().get_execution_memory(),
        initial_subnet_available_memory - 2 * chunk_size.get() as i64
    );

    // Check memory usage after three chunks uploaded.
    let chunk = vec![6; 1024 * 1024];
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk,
    };
    let result = test.subnet_message("upload_chunk", upload_args.encode());
    assert!(result.is_ok());
    assert_eq!(
        test.canister_state(canister_id).memory_usage(),
        NumBytes::from(3 * chunk_size.get()) + initial_memory_usage
    );
    assert_eq!(
        test.subnet_available_memory().get_execution_memory(),
        initial_subnet_available_memory - 3 * chunk_size.get() as i64
    );
}

#[test]
fn uninstall_clears_wasm_chunk_store() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = test.create_canister(CYCLES);

    // Upload a chunk
    let chunk = vec![1, 2, 3, 4, 5];
    let reply = UploadChunkReply {
        hash: ic_crypto_sha2::Sha256::hash(&chunk).to_vec(),
    }
    .encode();
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk,
    };
    let result = test.subnet_message("upload_chunk", upload_args.encode());
    assert_eq!(result, Ok(WasmResult::Reply(reply)));

    // Uninstall canister and check that chunk is gone.
    test.uninstall_code(canister_id).unwrap();
    assert_eq!(
        test.canister_state(canister_id)
            .system_state
            .wasm_chunk_store
            .memory_usage(),
        NumBytes::from(0)
    );
}

#[test]
fn upload_chunk_fails_when_freeze_threshold_triggered() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
    let instructions = SchedulerConfig::application_subnet().upload_wasm_chunk_instructions;

    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = test.create_canister(CYCLES);
    let initial_subnet_available_memory = test.subnet_available_memory();

    // Make balance just a bit higher than freezing threshold plus the charge
    // for one upload so upload_chunk fails.
    let threshold = test.freezing_threshold(canister_id);
    let new_balance = threshold
        + test.cycles_account_manager().execution_cost(
            instructions,
            test.subnet_size(),
            CanisterCyclesCostSchedule::Normal,
            test.canister_wasm_execution_mode(canister_id),
        )
        + Cycles::from(1_000_u128);
    let to_remove = test.canister_state(canister_id).system_state.balance() - new_balance;
    test.canister_state_mut(canister_id)
        .system_state
        .remove_cycles(to_remove, CyclesUseCase::BurnedCycles);

    // Upload a chunk
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk: vec![42; 10],
    };
    let error = test
        .subnet_message("upload_chunk", upload_args.encode())
        .unwrap_err();

    assert_eq!(error.code(), ErrorCode::InsufficientCyclesInMemoryGrow);
    assert!(
        error
            .description()
            .contains("additional cycles are required"),
        "Unexpected error: {}",
        error.description()
    );

    assert_eq!(
        test.subnet_available_memory(),
        initial_subnet_available_memory
    );
}

#[test]
#[cfg(not(all(target_arch = "aarch64", target_vendor = "apple")))]
fn upload_chunk_fails_when_it_exceeds_chunk_size() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = test.create_canister(CYCLES);
    let initial_subnet_available_memory = test.subnet_available_memory();

    let max_chunk_size = wasm_chunk_store::chunk_size().get() as usize;

    // Upload a chunk that is too large
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk: vec![42; max_chunk_size + 1],
    };
    test.subnet_message("upload_chunk", upload_args.encode())
        .unwrap_err()
        .assert_contains(
            ErrorCode::CanisterContractViolation,
            "Error from Wasm chunk store: Wasm chunk size 1048577 exceeds the maximum \
        chunk size of 1048576.",
        );

    assert_eq!(
        test.subnet_available_memory(),
        initial_subnet_available_memory
    );
}

#[test]
fn upload_chunk_reserves_cycles() {
    cycles_reserved_for_app_and_verified_app_subnets(|subnet_type| {
        const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

        let memory_usage_after_uploading_one_chunk = {
            const CAPACITY: i64 = 1_000_000_000;

            let mut test = ExecutionTestBuilder::new()
                .with_subnet_type(subnet_type)
                .with_subnet_execution_memory(CAPACITY)
                .with_subnet_memory_reservation(0)
                .with_subnet_memory_threshold(0)
                .build();
            let canister_id = test.create_canister(CYCLES);

            let upload_args = UploadChunkArgs {
                canister_id: canister_id.into(),
                chunk: vec![42; 10],
            };

            let _hash = test
                .subnet_message("upload_chunk", upload_args.encode())
                .unwrap();

            CAPACITY - test.subnet_available_memory().get_execution_memory()
        };

        let mut test = ExecutionTestBuilder::new()
            .with_subnet_memory_reservation(0)
            .with_subnet_memory_threshold(memory_usage_after_uploading_one_chunk + 1)
            .build();
        let canister_id = test.create_canister(CYCLES);
        assert_eq!(
            test.canister_state(canister_id)
                .system_state
                .reserved_balance(),
            Cycles::from(0_u128)
        );

        // Upload a chunk
        let upload_args = UploadChunkArgs {
            canister_id: canister_id.into(),
            chunk: vec![42; 10],
        };
        let _hash = test
            .subnet_message("upload_chunk", upload_args.encode())
            .unwrap();
        assert_eq!(
            test.canister_state(canister_id)
                .system_state
                .reserved_balance(),
            Cycles::from(0_u128)
        );

        // Upload a second chunk which should reserve some cycles.
        let upload_args = UploadChunkArgs {
            canister_id: canister_id.into(),
            chunk: vec![43; 10],
        };
        let _hash = test
            .subnet_message("upload_chunk", upload_args.encode())
            .unwrap();
        let reserved_balance = test
            .canister_state(canister_id)
            .system_state
            .reserved_balance()
            .get();
        assert_lt!(
            0,
            reserved_balance,
            "Reserved balance {} should be positive",
            reserved_balance
        );

        // Uploading the same chunk again should not reserve any further cycles.
        let _hash = test
            .subnet_message("upload_chunk", upload_args.encode())
            .unwrap();
        let new_reserved_balance = test
            .canister_state(canister_id)
            .system_state
            .reserved_balance()
            .get();
        assert_eq!(
            new_reserved_balance, reserved_balance,
            "The current reserved balance {} should match the previous reserved balance {}",
            new_reserved_balance, reserved_balance
        );
    });
}

#[test]
fn clear_chunk_store_works() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(CYCLES);

    let chunk = vec![1, 2, 3, 4, 5];
    let hash = ic_crypto_sha2::Sha256::hash(&chunk);
    let initial_memory_usage = test.canister_state(canister_id).memory_usage();

    // After uploading the chunk, it is present in the store and the memory
    // usage is positive.
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk,
    };
    test.subnet_message("upload_chunk", upload_args.encode())
        .unwrap();
    assert_lt!(
        initial_memory_usage,
        test.canister_state(canister_id).memory_usage()
    );
    assert!(test
        .canister_state(canister_id)
        .system_state
        .wasm_chunk_store
        .get_chunk_data(&hash)
        .is_some());

    // After clearing, the chunk should be absent and memory usage should be
    // zero.
    let clear_args = ClearChunkStoreArgs {
        canister_id: canister_id.into(),
    };
    test.subnet_message("clear_chunk_store", clear_args.encode())
        .unwrap();
    assert_eq!(
        test.canister_state(canister_id).memory_usage(),
        initial_memory_usage
    );
    assert!(test
        .canister_state(canister_id)
        .system_state
        .wasm_chunk_store
        .get_chunk_data(&hash)
        .is_none());
}

#[test]
fn stored_chunks_works() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    let mut test = ExecutionTestBuilder::new().build();

    let canister_id = test.create_canister(CYCLES);

    let chunk1 = vec![1, 2, 3, 4, 5];
    let hash1 = ic_crypto_sha2::Sha256::hash(&chunk1);
    let chunk2 = vec![0x42; 1000];
    let hash2 = ic_crypto_sha2::Sha256::hash(&chunk2);

    // Initial store has no chunks
    let reply = Decode!(
        &get_reply(
            test.subnet_message(
                "stored_chunks",
                StoredChunksArgs {
                    canister_id: canister_id.into(),
                }
                .encode(),
            ),
        ),
        StoredChunksReply
    )
    .unwrap();
    assert_eq!(reply, StoredChunksReply(vec![]));

    // Then one chunk
    test.subnet_message(
        "upload_chunk",
        UploadChunkArgs {
            canister_id: canister_id.into(),
            chunk: chunk1,
        }
        .encode(),
    )
    .unwrap();

    let reply = Decode!(
        &get_reply(
            test.subnet_message(
                "stored_chunks",
                StoredChunksArgs {
                    canister_id: canister_id.into(),
                }
                .encode(),
            ),
        ),
        StoredChunksReply
    )
    .unwrap();
    assert_eq!(
        reply,
        StoredChunksReply(vec![ChunkHash {
            hash: hash1.to_vec()
        }])
    );

    // Then two chunks
    test.subnet_message(
        "upload_chunk",
        UploadChunkArgs {
            canister_id: canister_id.into(),
            chunk: chunk2,
        }
        .encode(),
    )
    .unwrap();

    let reply = Decode!(
        &get_reply(
            test.subnet_message(
                "stored_chunks",
                StoredChunksArgs {
                    canister_id: canister_id.into(),
                }
                .encode(),
            ),
        ),
        StoredChunksReply
    )
    .unwrap();
    let mut expected = vec![
        ChunkHash {
            hash: hash1.to_vec(),
        },
        ChunkHash {
            hash: hash2.to_vec(),
        },
    ];
    expected.sort();
    assert_eq!(reply, StoredChunksReply(expected));
}

#[test]
#[cfg(not(all(target_arch = "aarch64", target_vendor = "apple")))]
fn upload_chunk_fails_when_heap_delta_rate_limited() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    let mut test = ExecutionTestBuilder::new()
        .with_heap_delta_rate_limit(wasm_chunk_store::chunk_size())
        .build();
    let canister_id = test.create_canister(CYCLES);
    assert_eq!(
        test.canister_state(canister_id)
            .system_state
            .reserved_balance(),
        Cycles::from(0_u128)
    );

    // Uploading one chunk will succeed
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk: vec![42; 10],
    };
    let _hash = test
        .subnet_message("upload_chunk", upload_args.encode())
        .unwrap();

    // Uploading the same chunk again will succeed
    let _hash = test
        .subnet_message("upload_chunk", upload_args.encode())
        .unwrap();

    // Uploading the second chunk will fail because of rate limiting.
    let initial_subnet_available_memory = test.subnet_available_memory();
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk: vec![43; 10],
    };
    test.subnet_message("upload_chunk", upload_args.encode())
        .unwrap_err()
        .assert_contains(
            ErrorCode::CanisterContractViolation,
            "Error from Wasm chunk store: Canister is heap delta rate limited. \
        Current delta debit: 1048576, limit: 1048576.",
        );

    assert_eq!(
        test.subnet_available_memory(),
        initial_subnet_available_memory
    );
}

#[test]
fn upload_chunk_increases_subnet_heap_delta() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = test.create_canister(CYCLES);
    assert_eq!(test.state().metadata.heap_delta_estimate, NumBytes::from(0));

    // Uploading one chunk will increase the delta.
    let upload_args = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk: vec![42; 10],
    };
    let _hash = test
        .subnet_message("upload_chunk", upload_args.encode())
        .unwrap();

    assert_eq!(
        test.state().metadata.heap_delta_estimate,
        wasm_chunk_store::chunk_size()
    );

    // Uploading the same chunk again will not increase the delta.
    let _hash = test
        .subnet_message("upload_chunk", upload_args.encode())
        .unwrap();

    assert_eq!(
        test.state().metadata.heap_delta_estimate,
        wasm_chunk_store::chunk_size()
    );
}

#[test]
fn upload_chunk_charges_canister_cycles() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
    let instructions = SchedulerConfig::application_subnet().upload_wasm_chunk_instructions;

    let mut test = ExecutionTestBuilder::new().build();
    let canister_id = test.create_canister(CYCLES);
    let initial_balance = test.canister_state(canister_id).system_state.balance();

    // Uploading one chunk will decrease balance by the cycles corresponding to
    // the instructions for uploading.
    let payload = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk: vec![42; 10],
    }
    .encode();
    let expected_charge = test.cycles_account_manager().execution_cost(
        instructions,
        test.subnet_size(),
        CanisterCyclesCostSchedule::Normal,
        test.canister_wasm_execution_mode(canister_id),
    );
    let _hash = test
        .subnet_message("upload_chunk", payload.clone())
        .unwrap();

    assert_eq!(
        test.canister_state(canister_id).system_state.balance(),
        initial_balance - expected_charge,
    );

    // Uploading the same chunk again will decrease balance by the cycles corresponding to
    // the instructions for uploading.
    let _hash = test.subnet_message("upload_chunk", payload).unwrap();

    assert_eq!(
        test.canister_state(canister_id).system_state.balance(),
        initial_balance - expected_charge - expected_charge,
    );
}

#[test]
fn upload_chunk_charges_if_failing() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);
    let instructions = SchedulerConfig::application_subnet().upload_wasm_chunk_instructions;

    let mut test = ExecutionTestBuilder::new()
        .with_subnet_memory_reservation(0)
        .with_subnet_execution_memory(10)
        .build();
    let canister_id = test.create_canister(CYCLES);
    let initial_balance = test.canister_state(canister_id).system_state.balance();
    // Expected charge is the same as if the upload succeeds.
    let expected_charge = test.cycles_account_manager().execution_cost(
        instructions,
        test.subnet_size(),
        CanisterCyclesCostSchedule::Normal,
        test.canister_wasm_execution_mode(canister_id),
    );

    let payload = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk: vec![42; 10],
    }
    .encode();
    // Upload will fail because subnet does not have space.
    let _err = test.subnet_message("upload_chunk", payload).unwrap_err();

    assert_eq!(
        test.canister_state(canister_id).system_state.balance(),
        initial_balance - expected_charge,
    );
}

/// Check that a canister can call the chunk store methods on itself even if it
/// isn't a controller of itself.
#[test]
fn chunk_store_methods_succeed_from_canister_itself() {
    const CYCLES: Cycles = Cycles::new(1_000_000_000_000_000);

    let mut test = ExecutionTestBuilder::new().build();

    let uc = test
        .canister_from_cycles_and_binary(CYCLES, UNIVERSAL_CANISTER_WASM.to_vec())
        .unwrap();

    assert!(!test
        .canister_state(uc)
        .system_state
        .controllers
        .contains(&uc.into()));

    let methods = [
        (
            Method::UploadChunk,
            UploadChunkArgs {
                canister_id: uc.into(),
                chunk: vec![1, 2, 3, 4, 5],
            }
            .encode(),
        ),
        (
            Method::ClearChunkStore,
            ClearChunkStoreArgs {
                canister_id: uc.into(),
            }
            .encode(),
        ),
        (
            Method::StoredChunks,
            StoredChunksArgs {
                canister_id: uc.into(),
            }
            .encode(),
        ),
    ];

    for (method, args) in methods {
        let wasm = wasm()
            .call_with_cycles(
                CanisterId::ic_00(),
                method,
                call_args()
                    .other_side(args)
                    .on_reject(wasm().reject_message().reject()),
                Cycles::new(CYCLES.get() / 2),
            )
            .build();

        let _result = get_reply(test.ingress(uc, "update", wasm));
    }
}

/// Subnet available memory is recalculated at the beginning of each round.
/// This test checks that the wasm chunk store is accounted for then.
#[test]
fn chunk_store_counts_against_subnet_memory_in_initial_round_computation() {
    let subnet_config = ic_config::subnet_config::SubnetConfig::new(SubnetType::Application);
    // Initialize subnet with enough memory for one chunk but not two (the
    // canister history will take up some memory).
    let hypervisor_config = Config {
        subnet_memory_capacity: wasm_chunk_store::chunk_size() * 2,
        subnet_memory_threshold: NumBytes::from(0),
        subnet_memory_reservation: NumBytes::from(0),
        ..Config::default()
    };
    let env = StateMachineBuilder::new()
        .with_config(Some(StateMachineConfig::new(
            subnet_config,
            hypervisor_config,
        )))
        .build();
    let canister_id = env.create_canister_with_cycles(None, Cycles::new(1 << 64), None);

    // Uploading one chunk is ok.
    let payload = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk: vec![0x42; 1024 * 1024],
    }
    .encode();
    env.execute_ingress(CanisterId::ic_00(), "upload_chunk", payload)
        .unwrap();

    // Start a new round.
    env.tick();

    // The previous chunk should take up subnet memory so there isn't space for
    // a second.
    let payload = UploadChunkArgs {
        canister_id: canister_id.into(),
        chunk: vec![0x43; 1024 * 1024],
    }
    .encode();
    let error = env
        .execute_ingress(CanisterId::ic_00(), "upload_chunk", payload)
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::SubnetOversubscribed);
}

/// Helper function that installs and runs an update call on a canister in
/// Wasm32 and Wasm64 mode and returns the balance and the cost of the call(s).
fn run_canister_in_wasm_mode(is_wasm64_mode: bool, execute_ingress: bool) -> (Cycles, Cycles) {
    let memory_type = if is_wasm64_mode { "i64" } else { "" };
    let canister_wat = format!(
        r#"
        (module
            (func (export "canister_update test")
                (drop (i32.add (i32.const 1) (i32.const 2)))
                (drop (i32.add (i32.const 1) (i32.const 2)))
                (drop (i32.add (i32.const 1) (i32.const 2)))
                (drop (i32.add (i32.const 1) (i32.const 2)))
                (drop (i32.add (i32.const 1) (i32.const 2)))
            )
            (memory {memory_type} 1)
        )
    "#
    );

    let mut test = ExecutionTestBuilder::new().with_wasm64().build();
    let canister_id = test
        .canister_from_cycles_and_wat(DEFAULT_PROVISIONAL_BALANCE, canister_wat)
        .unwrap();

    let balance_before_ingress = test.canister_state(canister_id).system_state.balance();
    let cost_for_install = DEFAULT_PROVISIONAL_BALANCE - balance_before_ingress;

    if execute_ingress {
        let _ = test.ingress(canister_id, "test", vec![]);
    } else {
        return (balance_before_ingress, cost_for_install);
    }

    let balance_after_ingress = test.canister_state(canister_id).system_state.balance();
    let cost_for_ingress = balance_before_ingress - balance_after_ingress;

    (balance_after_ingress, cost_for_ingress)
}

#[test]
fn check_update_call_canister_in_wasm64_mode_is_charged_correctly() {
    let (balance32, execution_cost32) = run_canister_in_wasm_mode(false, true);
    let (balance64, execution_cost64) = run_canister_in_wasm_mode(true, true);

    assert_lt!(balance64, balance32);
    assert_lt!(execution_cost32, execution_cost64);
}

#[test]
fn check_install_code_in_wasm64_mode_is_charged_correctly() {
    let (balance32, execution_cost32) = run_canister_in_wasm_mode(false, false);
    let (balance64, execution_cost64) = run_canister_in_wasm_mode(true, false);

    assert_lt!(balance64, balance32);
    assert_lt!(execution_cost32, execution_cost64);
}

#[test]
fn subnet_info_canister_call_succeeds() {
    let own_subnet_id = subnet_test_id(1);
    let mut test = ExecutionTestBuilder::new()
        .with_own_subnet_id(own_subnet_id)
        .build();
    let uni_canister = test
        .universal_canister_with_cycles(Cycles::new(1_000_000_000_000))
        .unwrap();
    let payload = SubnetInfoArgs {
        subnet_id: own_subnet_id.get(),
    }
    .encode();
    let uc_call = wasm()
        .call_simple(
            CanisterId::ic_00(),
            Method::SubnetInfo,
            call_args().other_side(payload),
        )
        .build();
    let result = test.ingress(uni_canister, "update", uc_call).unwrap();
    let bytes = match result {
        WasmResult::Reply(bytes) => bytes,
        WasmResult::Reject(err_msg) => panic!("Unexpected reject, expected reply: {}", err_msg),
    };
    let SubnetInfoResponse {
        replica_version,
        registry_version,
    } = Decode!(&bytes, SubnetInfoResponse).unwrap();
    assert_eq!(
        replica_version,
        ic_types::ReplicaVersion::default().to_string()
    );
    assert_eq!(registry_version, ic_types::RegistryVersion::default().get());
}

#[test]
fn subnet_info_ingress_fails() {
    let own_subnet_id = subnet_test_id(1);
    let mut test = ExecutionTestBuilder::new()
        .with_own_subnet_id(own_subnet_id)
        .build();
    let payload = SubnetInfoArgs {
        subnet_id: own_subnet_id.get(),
    }
    .encode();
    test.subnet_message(Method::SubnetInfo, payload)
        .unwrap_err()
        .assert_contains(
            ErrorCode::CanisterContractViolation,
            "cannot be called by a user",
        );
}

#[test]
fn node_metrics_history_update_succeeds() {
    let own_subnet_id = subnet_test_id(1);
    let mut test = ExecutionTestBuilder::new()
        .with_own_subnet_id(own_subnet_id)
        .build();
    let uni_canister = test
        .universal_canister_with_cycles(Cycles::new(1_000_000_000_000))
        .unwrap();
    let payload = wasm()
        .call_simple(
            CanisterId::ic_00(),
            Method::NodeMetricsHistory,
            call_args().other_side(
                NodeMetricsHistoryArgs {
                    subnet_id: own_subnet_id.get(),
                    start_at_timestamp_nanos: 0,
                }
                .encode(),
            ),
        )
        .build();
    let result = test.ingress(uni_canister, "update", payload);
    let bytes = get_reply(result);
    let _ = Decode!(&bytes, Vec<NodeMetricsHistoryResponse>).unwrap();
}

#[test]
fn node_metrics_history_ingress_update_fails() {
    let own_subnet_id = subnet_test_id(1);
    let mut test = ExecutionTestBuilder::new()
        .with_own_subnet_id(own_subnet_id)
        .build();
    let payload = NodeMetricsHistoryArgs {
        subnet_id: own_subnet_id.get(),
        start_at_timestamp_nanos: 0,
    }
    .encode();
    test.subnet_message(Method::NodeMetricsHistory, payload)
        .unwrap_err()
        .assert_contains(
            ErrorCode::CanisterContractViolation,
            "cannot be called by a user",
        );
}

#[test]
fn node_metrics_history_ingress_query_fails() {
    let own_subnet_id = subnet_test_id(1);
    let mut test = ExecutionTestBuilder::new()
        .with_own_subnet_id(own_subnet_id)
        .build();
    let payload = NodeMetricsHistoryArgs {
        subnet_id: own_subnet_id.get(),
        start_at_timestamp_nanos: 0,
    }
    .encode();
    test.non_replicated_query(CanisterId::ic_00(), "node_metrics_history", payload)
        .unwrap_err()
        .assert_contains(
            ErrorCode::CanisterMethodNotFound,
            "Query method node_metrics_history not found.",
        );
}

struct MemoryState {
    wasm_memory_limit: Option<NumBytes>,
    wasm_memory_threshold: Option<NumBytes>,
    memory_allocation: Option<MemoryAllocation>,
    hook_status: OnLowWasmMemoryHookStatus,
}

fn helper_update_settings_updates_hook_status(
    used_wasm_memory_pages: u64,
    initial_memory_state: MemoryState,
    updated_memory_state: MemoryState,
    execute_hook_after_install: bool,
) {
    let mut test = ExecutionTestBuilder::new().build();

    let mut initial_settings = CanisterSettingsArgsBuilder::new();

    if let Some(MemoryAllocation::Reserved(memory_allocation)) =
        initial_memory_state.memory_allocation
    {
        initial_settings = initial_settings.with_memory_allocation(memory_allocation.get());
    }

    if let Some(wasm_memory_limit) = initial_memory_state.wasm_memory_limit {
        initial_settings = initial_settings.with_wasm_memory_limit(wasm_memory_limit.get());
    }

    if let Some(wasm_memory_threshold) = initial_memory_state.wasm_memory_threshold {
        initial_settings = initial_settings.with_wasm_memory_threshold(wasm_memory_threshold.get());
    }

    let canister_id = test
        .create_canister_with_settings(*INITIAL_CYCLES, initial_settings.build())
        .unwrap();

    let wat = format!("(module (memory {used_wasm_memory_pages}))");

    let wasm = wat::parse_str(wat).unwrap();

    test.install_canister(canister_id, wasm).unwrap();

    if execute_hook_after_install {
        test.canister_state_mut(canister_id)
            .system_state
            .task_queue
            .pop_front();
    }

    assert_eq!(
        test.canister_state_mut(canister_id)
            .system_state
            .task_queue
            .peek_hook_status(),
        initial_memory_state.hook_status
    );

    let mut settings = CanisterSettingsArgsBuilder::new();

    if updated_memory_state.wasm_memory_threshold != initial_memory_state.wasm_memory_threshold {
        if let Some(updated_wasm_memory_threshold) = updated_memory_state.wasm_memory_threshold {
            settings = settings.with_wasm_memory_threshold(updated_wasm_memory_threshold.get());
        }
    }

    if updated_memory_state.wasm_memory_limit != initial_memory_state.wasm_memory_limit {
        if let Some(updated_wasm_memory_limit) = updated_memory_state.wasm_memory_limit {
            settings = settings.with_wasm_memory_limit(updated_wasm_memory_limit.get());
        }
    }

    if updated_memory_state.memory_allocation != initial_memory_state.memory_allocation {
        if let Some(MemoryAllocation::Reserved(updated_memory_allocation)) =
            updated_memory_state.memory_allocation
        {
            settings = settings.with_memory_allocation(updated_memory_allocation.get());
        }
    }

    let payload = UpdateSettingsArgs {
        canister_id: canister_id.get(),
        settings: settings.build(),
        sender_canister_version: None,
    }
    .encode();

    test.subnet_message(Method::UpdateSettings, payload)
        .unwrap();

    assert_eq!(
        test.canister_state_mut(canister_id)
            .system_state
            .task_queue
            .peek_hook_status(),
        updated_memory_state.hook_status
    );
}

#[test]
fn update_wasm_memory_threshold_updates_hook_status_ready_to_not_satisfied() {
    let used_wasm_memory_pages = 1;
    let used_wasm_memory = used_wasm_memory_pages * WASM_PAGE_SIZE_IN_BYTES;
    let wasm_memory_limit = used_wasm_memory + 100;

    let initial_wasm_memory_threshold = 150;
    // wasm_memory_limit - used_wasm_memory < wasm_memory_threshold
    let initial_hook_status = OnLowWasmMemoryHookStatus::Ready;

    let initial_memory_state = MemoryState {
        wasm_memory_limit: Some(NumBytes::new(wasm_memory_limit)),
        wasm_memory_threshold: Some(NumBytes::new(initial_wasm_memory_threshold)),
        memory_allocation: None,
        hook_status: initial_hook_status,
    };

    let updated_wasm_memory_threshold = 50;
    // wasm_memory_limit - used_wasm_memory > wasm_memory_threshold
    let updated_hook_status = OnLowWasmMemoryHookStatus::ConditionNotSatisfied;

    let updated_memory_state = MemoryState {
        wasm_memory_threshold: Some(NumBytes::from(updated_wasm_memory_threshold)),
        hook_status: updated_hook_status,
        ..initial_memory_state
    };

    helper_update_settings_updates_hook_status(
        used_wasm_memory_pages,
        initial_memory_state,
        updated_memory_state,
        false,
    );
}

#[test]
fn update_wasm_memory_threshold_updates_hook_status_not_satisfied_to_ready() {
    let used_wasm_memory_pages = 1;
    let used_wasm_memory = used_wasm_memory_pages * WASM_PAGE_SIZE_IN_BYTES;
    let wasm_memory_limit = used_wasm_memory + 100;

    let initial_wasm_memory_threshold = 50;
    // wasm_memory_limit - used_wasm_memory > wasm_memory_threshold
    let initial_hook_status = OnLowWasmMemoryHookStatus::ConditionNotSatisfied;

    let initial_memory_state = MemoryState {
        wasm_memory_limit: Some(NumBytes::new(wasm_memory_limit)),
        wasm_memory_threshold: Some(NumBytes::new(initial_wasm_memory_threshold)),
        memory_allocation: None,
        hook_status: initial_hook_status,
    };

    let updated_wasm_memory_threshold = 150;
    // wasm_memory_limit - used_wasm_memory < wasm_memory_threshold
    let updated_hook_status = OnLowWasmMemoryHookStatus::Ready;

    let updated_memory_state = MemoryState {
        wasm_memory_threshold: Some(NumBytes::from(updated_wasm_memory_threshold)),
        hook_status: updated_hook_status,
        ..initial_memory_state
    };

    helper_update_settings_updates_hook_status(
        used_wasm_memory_pages,
        initial_memory_state,
        updated_memory_state,
        false,
    );
}

#[test]
fn update_wasm_memory_threshold_updates_hook_status_executed_to_not_satisfied() {
    let used_wasm_memory_pages = 1;
    let used_wasm_memory = used_wasm_memory_pages * WASM_PAGE_SIZE_IN_BYTES;
    let wasm_memory_limit = used_wasm_memory + 100;

    let initial_wasm_memory_threshold = 150;
    // wasm_memory_limit - used_wasm_memory < wasm_memory_threshold

    // This will simulate execution of the task.
    let pop_from_task_queue = true;

    let initial_hook_status = OnLowWasmMemoryHookStatus::Executed;

    let initial_memory_state = MemoryState {
        wasm_memory_limit: Some(NumBytes::new(wasm_memory_limit)),
        wasm_memory_threshold: Some(NumBytes::new(initial_wasm_memory_threshold)),
        memory_allocation: None,
        hook_status: initial_hook_status,
    };

    let updated_wasm_memory_threshold = 50;
    // wasm_memory_limit - used_wasm_memory > wasm_memory_threshold
    let updated_hook_status = OnLowWasmMemoryHookStatus::ConditionNotSatisfied;

    let updated_memory_state = MemoryState {
        wasm_memory_threshold: Some(NumBytes::from(updated_wasm_memory_threshold)),
        hook_status: updated_hook_status,
        ..initial_memory_state
    };

    helper_update_settings_updates_hook_status(
        used_wasm_memory_pages,
        initial_memory_state,
        updated_memory_state,
        pop_from_task_queue,
    );
}

#[test]
fn update_wasm_memory_threshold_updates_hook_status_executed_is_remembered() {
    let used_wasm_memory_pages = 1;
    let used_wasm_memory = used_wasm_memory_pages * WASM_PAGE_SIZE_IN_BYTES;
    let wasm_memory_limit = used_wasm_memory + 100;

    let initial_wasm_memory_threshold = 150;
    // wasm_memory_limit - used_wasm_memory < wasm_memory_threshold

    // This will simulate execution of the task.
    let pop_from_task_queue = true;

    let initial_hook_status = OnLowWasmMemoryHookStatus::Executed;

    let initial_memory_state = MemoryState {
        wasm_memory_limit: Some(NumBytes::new(wasm_memory_limit)),
        wasm_memory_threshold: Some(NumBytes::new(initial_wasm_memory_threshold)),
        memory_allocation: None,
        hook_status: initial_hook_status,
    };

    let updated_wasm_memory_threshold = 149;
    // wasm_memory_limit - used_wasm_memory < wasm_memory_threshold
    // Because the hook condition is still satisfied, hook status
    // should remain `Executed`.
    let updated_hook_status = OnLowWasmMemoryHookStatus::Executed;

    let updated_memory_state = MemoryState {
        wasm_memory_threshold: Some(NumBytes::from(updated_wasm_memory_threshold)),
        hook_status: updated_hook_status,
        ..initial_memory_state
    };

    helper_update_settings_updates_hook_status(
        used_wasm_memory_pages,
        initial_memory_state,
        updated_memory_state,
        pop_from_task_queue,
    );
}

#[test]
fn update_wasm_memory_limit_updates_hook_status_not_satisfied_to_ready() {
    let used_wasm_memory_pages = 1;
    let used_wasm_memory = used_wasm_memory_pages * WASM_PAGE_SIZE_IN_BYTES;
    let wasm_memory_threshold = 100;

    let initial_wasm_memory_limit = used_wasm_memory + 150;
    // wasm_memory_limit - used_wasm_memory > wasm_memory_threshold
    let initial_hook_status = OnLowWasmMemoryHookStatus::ConditionNotSatisfied;

    let initial_memory_state = MemoryState {
        wasm_memory_limit: Some(NumBytes::new(initial_wasm_memory_limit)),
        wasm_memory_threshold: Some(NumBytes::new(wasm_memory_threshold)),
        memory_allocation: None,
        hook_status: initial_hook_status,
    };

    let updated_wasm_memory_limit = used_wasm_memory + 50;
    // wasm_memory_limit - used_wasm_memory < wasm_memory_threshold
    let updated_hook_status = OnLowWasmMemoryHookStatus::Ready;

    let updated_memory_state = MemoryState {
        wasm_memory_limit: Some(NumBytes::from(updated_wasm_memory_limit)),
        hook_status: updated_hook_status,
        ..initial_memory_state
    };

    helper_update_settings_updates_hook_status(
        used_wasm_memory_pages,
        initial_memory_state,
        updated_memory_state,
        false,
    );
}

#[test]
fn update_wasm_memory_limit_updates_hook_status_ready_to_not_satisfied() {
    let used_wasm_memory_pages = 1;
    let used_wasm_memory = used_wasm_memory_pages * WASM_PAGE_SIZE_IN_BYTES;
    let wasm_memory_threshold = 100;

    let initial_wasm_memory_limit = used_wasm_memory + 50;
    // wasm_memory_limit - used_wasm_memory < wasm_memory_threshold
    let initial_hook_status = OnLowWasmMemoryHookStatus::Ready;

    let initial_memory_state = MemoryState {
        wasm_memory_limit: Some(NumBytes::new(initial_wasm_memory_limit)),
        wasm_memory_threshold: Some(NumBytes::new(wasm_memory_threshold)),
        memory_allocation: None,
        hook_status: initial_hook_status,
    };

    let updated_wasm_memory_limit = used_wasm_memory + 150;
    // wasm_memory_limit - used_wasm_memory > wasm_memory_threshold
    let updated_hook_status = OnLowWasmMemoryHookStatus::ConditionNotSatisfied;

    let updated_memory_state = MemoryState {
        wasm_memory_limit: Some(NumBytes::from(updated_wasm_memory_limit)),
        hook_status: updated_hook_status,
        ..initial_memory_state
    };

    helper_update_settings_updates_hook_status(
        used_wasm_memory_pages,
        initial_memory_state,
        updated_memory_state,
        false,
    );
}

#[test]
fn update_memory_allocation_updates_hook_status_not_satisfied_to_ready() {
    let used_wasm_memory_pages = 1;
    let used_wasm_memory = used_wasm_memory_pages * WASM_PAGE_SIZE_IN_BYTES;
    let wasm_memory_threshold = 1_000;
    let wasm_memory_limit = 100_000;

    let initial_memory_allocation = used_wasm_memory + 1_500;

    // wasm_memory_capacity = min(wasm_memory_limit, memory_allocation - used_non_wasm_memory)
    // wasm_memory_capacity - used_wasm_memory > wasm_memory_threshold
    let initial_hook_status = OnLowWasmMemoryHookStatus::ConditionNotSatisfied;

    let initial_memory_state = MemoryState {
        wasm_memory_limit: Some(NumBytes::new(wasm_memory_limit)),
        wasm_memory_threshold: Some(NumBytes::new(wasm_memory_threshold)),
        memory_allocation: Some(MemoryAllocation::Reserved(NumBytes::from(
            initial_memory_allocation,
        ))),
        hook_status: initial_hook_status,
    };

    let updated_memory_allocation = used_wasm_memory + 500;
    // wasm_memory_capacity - used_wasm_memory < wasm_memory_threshold
    let updated_hook_status = OnLowWasmMemoryHookStatus::Ready;

    let updated_memory_state = MemoryState {
        memory_allocation: Some(MemoryAllocation::Reserved(NumBytes::from(
            updated_memory_allocation,
        ))),
        hook_status: updated_hook_status,
        ..initial_memory_state
    };

    helper_update_settings_updates_hook_status(
        used_wasm_memory_pages,
        initial_memory_state,
        updated_memory_state,
        false,
    );
}

#[test]
fn update_memory_allocation_updates_hook_status_ready_to_not_satisfied() {
    let used_wasm_memory_pages = 1;
    let used_wasm_memory = used_wasm_memory_pages * WASM_PAGE_SIZE_IN_BYTES;
    let wasm_memory_threshold = 1_000;
    let wasm_memory_limit = 100_000;

    // wasm_memory_capacity = min(wasm_memory_limit, memory_allocation - used_non_wasm_memory)

    let initial_memory_allocation = used_wasm_memory + 500;
    // wasm_memory_capacity - used_wasm_memory < wasm_memory_threshold
    let initial_hook_status = OnLowWasmMemoryHookStatus::Ready;

    let initial_memory_state = MemoryState {
        wasm_memory_limit: Some(NumBytes::new(wasm_memory_limit)),
        wasm_memory_threshold: Some(NumBytes::new(wasm_memory_threshold)),
        memory_allocation: Some(MemoryAllocation::Reserved(NumBytes::from(
            initial_memory_allocation,
        ))),
        hook_status: initial_hook_status,
    };

    let updated_memory_allocation = used_wasm_memory + 1_500;
    // wasm_memory_capacity - used_wasm_memory > wasm_memory_threshold
    let updated_hook_status = OnLowWasmMemoryHookStatus::ConditionNotSatisfied;

    let updated_memory_state = MemoryState {
        memory_allocation: Some(MemoryAllocation::Reserved(NumBytes::from(
            updated_memory_allocation,
        ))),
        hook_status: updated_hook_status,
        ..initial_memory_state
    };

    helper_update_settings_updates_hook_status(
        used_wasm_memory_pages,
        initial_memory_state,
        updated_memory_state,
        false,
    );
}

#[test]
fn memory_usage_updates_increment_subnet_available_memory() {
    let mut test = ExecutionTestBuilder::new().build();

    // create a universal canister
    let canister_id = test.create_canister(*INITIAL_CYCLES);
    test.install_code_v2(InstallCodeArgsV2::new(
        CanisterInstallModeV2::Install,
        canister_id,
        UNIVERSAL_CANISTER_WASM.to_vec(),
        vec![],
    ))
    .unwrap();

    // take a canister snapshot
    let take_snapshot_args = TakeCanisterSnapshotArgs {
        canister_id: canister_id.get(),
        replace_snapshot: None,
    };
    let res = test
        .subnet_message("take_canister_snapshot", take_snapshot_args.encode())
        .unwrap();
    let snapshot_id = match res {
        WasmResult::Reply(data) => Decode!(&data, CanisterSnapshotResponse).unwrap().id,
        WasmResult::Reject(msg) => panic!("Unexpected reject: {}", msg),
    };

    // capture current subnet available memory
    let initial_subnet_available_memory = test.subnet_available_memory();

    // grow stable memory
    let payload = wasm().stable_grow(100).reply().build();
    test.ingress(canister_id, "update", payload).unwrap();

    // execution memory should *increase* after growing stable memory => available memory should *decrease*
    let subnet_available_memory = test.subnet_available_memory();
    assert_lt!(
        subnet_available_memory.get_execution_memory(),
        initial_subnet_available_memory.get_execution_memory()
    );

    // load canister snapshot
    let load_snapshot_args = LoadCanisterSnapshotArgs::new(canister_id, snapshot_id, None);
    test.subnet_message("load_canister_snapshot", load_snapshot_args.encode())
        .unwrap();

    // execution memory should get back to its initial value before growing stable memory
    let subnet_available_memory = test.subnet_available_memory();
    assert_eq!(initial_subnet_available_memory, subnet_available_memory);
}

#[test]
fn test_environment_variables_are_changed_via_create_canister() {
    let mut test = ExecutionTestBuilder::new()
        .with_execution_config(Config {
            environment_variables: FlagStatus::Enabled,
            ..Default::default()
        })
        .build();

    let env_vars = BTreeMap::from([
        ("KEY1".to_string(), "VALUE1".to_string()),
        ("KEY2".to_string(), "VALUE2".to_string()),
    ]);

    // Create canister with environment variables.
    let canister_id = test
        .create_canister_with_settings(
            Cycles::new(1_000_000_000_000_000),
            CanisterSettingsArgsBuilder::new()
                .with_environment_variables(
                    env_vars
                        .clone()
                        .into_iter()
                        .map(|(k, v)| EnvironmentVariable { name: k, value: v })
                        .collect::<Vec<_>>(),
                )
                .build(),
        )
        .unwrap();

    // Verify environment variables are set.
    let canister = test.canister_state(canister_id);
    assert_eq!(
        canister.system_state.environment_variables,
        EnvironmentVariables::new(env_vars)
    );
}

#[test]
fn test_environment_variables_are_updated_on_update_settings() {
    let mut test = ExecutionTestBuilder::new()
        .with_execution_config(Config {
            environment_variables: FlagStatus::Enabled,
            ..Default::default()
        })
        .build();
    let canister_id = test.create_canister(Cycles::new(1_000_000_000_000_000));

    let env_vars = EnvironmentVariables::new(BTreeMap::from([
        ("KEY1".to_string(), "VALUE1".to_string()),
        ("KEY2".to_string(), "VALUE2".to_string()),
    ]));

    // Set environment variables via `update_settings`.
    let args = UpdateSettingsArgs {
        canister_id: canister_id.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_environment_variables(
                env_vars
                    .iter()
                    .map(|(name, value)| EnvironmentVariable {
                        name: name.clone(),
                        value: value.clone(),
                    })
                    .collect::<Vec<_>>(),
            )
            .build(),
        sender_canister_version: None,
    };
    test.subnet_message(Method::UpdateSettings, args.encode())
        .unwrap();

    // Verify environment variables are set.
    let canister = test.canister_state(canister_id);
    assert_eq!(canister.system_state.environment_variables, env_vars);

    // Environment variables are unchanged when not specified.
    let args = UpdateSettingsArgs {
        canister_id: canister_id.get(),
        settings: CanisterSettingsArgsBuilder::new().build(),
        sender_canister_version: None,
    };
    test.subnet_message(Method::UpdateSettings, args.encode())
        .unwrap();

    // Verify environment variables are unchanged.
    let canister = test.canister_state(canister_id);
    assert_eq!(canister.system_state.environment_variables, env_vars);
}

#[test]
fn test_environment_variables_are_not_set_when_disabled() {
    let mut test = ExecutionTestBuilder::new()
        .with_execution_config(Config {
            environment_variables: FlagStatus::Disabled,
            ..Default::default()
        })
        .build();

    // Create environment variables.
    let env_vars = BTreeMap::from([
        ("KEY1".to_string(), "VALUE1".to_string()),
        ("KEY2".to_string(), "VALUE2".to_string()),
    ]);
    let env_vars_args = env_vars
        .iter()
        .map(|(name, value)| EnvironmentVariable {
            name: name.clone(),
            value: value.clone(),
        })
        .collect::<Vec<_>>();
    // Create canister with environment variables.
    let canister_id = test
        .create_canister_with_settings(
            Cycles::new(1_000_000_000_000_000),
            CanisterSettingsArgsBuilder::new()
                .with_environment_variables(env_vars_args.clone())
                .build(),
        )
        .unwrap();

    // Verify environment variables are not set.
    let canister = test.canister_state(canister_id);
    assert_eq!(
        canister.system_state.environment_variables,
        EnvironmentVariables::new(BTreeMap::new())
    );

    // Set environment variables via `update_settings`.
    let args = UpdateSettingsArgs {
        canister_id: canister_id.get(),
        settings: CanisterSettingsArgsBuilder::new()
            .with_environment_variables(env_vars_args)
            .build(),
        sender_canister_version: None,
    };
    test.subnet_message(Method::UpdateSettings, args.encode())
        .unwrap();

    // Verify environment variables are not set.
    let canister = test.canister_state(canister_id);
    assert_eq!(
        canister.system_state.environment_variables,
        EnvironmentVariables::new(BTreeMap::new())
    );
}

#[test]
fn test_environment_variables_are_not_set_when_too_many_keys() {
    let mut test = ExecutionTestBuilder::new()
        .with_execution_config(Config {
            environment_variables: FlagStatus::Enabled,
            ..Default::default()
        })
        .build();

    let env_vars = (0..MAX_ENVIRONMENT_VARIABLES + 1)
        .map(|i| (format!("KEY{}", i), "VAL".to_string()))
        .map(|(k, v)| EnvironmentVariable { name: k, value: v })
        .collect::<Vec<_>>();

    // Create canister with environment variables.
    let err = test
        .create_canister_with_settings(
            Cycles::new(1_000_000_000_000_000),
            CanisterSettingsArgsBuilder::new()
                .with_environment_variables(env_vars.clone())
                .build(),
        )
        .unwrap_err();

    assert_eq!(
        err,
        UserError::new(
            ErrorCode::InvalidManagementPayload,
            format!(
                "Too many environment variables: {} (max: {})",
                env_vars.len(),
                MAX_ENVIRONMENT_VARIABLES
            )
        )
    );
}

#[test]
fn test_environment_variables_are_not_set_when_key_is_too_long() {
    let mut test = ExecutionTestBuilder::new()
        .with_execution_config(Config {
            environment_variables: FlagStatus::Enabled,
            ..Default::default()
        })
        .build();

    let long_key = "K".repeat(MAX_ENVIRONMENT_VARIABLE_NAME_LENGTH + 1);
    let env_vars = [
        ("KEY1".to_string(), "VALUE1".to_string()),
        ("KEY2".to_string(), "VALUE2".to_string()),
        (long_key.clone(), "VALUE3".to_string()),
    ];
    let env_vars = env_vars
        .into_iter()
        .map(|(k, v)| EnvironmentVariable { name: k, value: v })
        .collect::<Vec<_>>();

    // Create canister with environment variables.
    let err = test
        .create_canister_with_settings(
            Cycles::new(1_000_000_000_000_000),
            CanisterSettingsArgsBuilder::new()
                .with_environment_variables(env_vars.clone())
                .build(),
        )
        .unwrap_err();

    assert_eq!(
        err,
        UserError::new(
            ErrorCode::InvalidManagementPayload,
            format!(
                "Environment variable name \"{}\" exceeds the maximum allowed length of {}.",
                long_key, MAX_ENVIRONMENT_VARIABLE_NAME_LENGTH
            )
        )
    );
}

#[test]
fn test_environment_variables_are_not_set_when_value_is_too_long() {
    let mut test = ExecutionTestBuilder::new()
        .with_execution_config(Config {
            environment_variables: FlagStatus::Enabled,
            ..Default::default()
        })
        .build();

    let long_value = "V".repeat(MAX_ENVIRONMENT_VARIABLE_VALUE_LENGTH + 1);
    let env_vars = [
        ("KEY1".to_string(), "VALUE1".to_string()),
        ("KEY2".to_string(), "VALUE2".to_string()),
        ("KEY3".to_string(), long_value.clone()),
    ];
    let env_vars = env_vars
        .into_iter()
        .map(|(k, v)| EnvironmentVariable { name: k, value: v })
        .collect::<Vec<_>>();

    // Create canister with environment variables.
    let err = test
        .create_canister_with_settings(
            Cycles::new(1_000_000_000_000_000),
            CanisterSettingsArgsBuilder::new()
                .with_environment_variables(env_vars.clone())
                .build(),
        )
        .unwrap_err();

    assert_eq!(
        err,
        UserError::new(
            ErrorCode::InvalidManagementPayload,
            format!(
                "Environment variable value \"{}\" exceeds the maximum allowed length of {}.",
                long_value, MAX_ENVIRONMENT_VARIABLE_VALUE_LENGTH
            )
        )
    );
}

#[test]
fn test_environment_variables_are_not_set_duplicate_keys() {
    let mut test = ExecutionTestBuilder::new()
        .with_execution_config(Config {
            environment_variables: FlagStatus::Enabled,
            ..Default::default()
        })
        .build();

    let env_vars = [
        ("KEY1".to_string(), "VALUE1".to_string()),
        ("KEY2".to_string(), "VALUE2".to_string()),
        ("KEY2".to_string(), "VALUE3".to_string()),
    ];
    let env_vars = env_vars
        .into_iter()
        .map(|(k, v)| EnvironmentVariable { name: k, value: v })
        .collect::<Vec<_>>();

    // Create canister with environment variables.
    let err = test
        .create_canister_with_settings(
            Cycles::new(1_000_000_000_000_000),
            CanisterSettingsArgsBuilder::new()
                .with_environment_variables(env_vars.clone())
                .build(),
        )
        .unwrap_err();

    assert_eq!(
        err,
        UserError::new(
            ErrorCode::InvalidManagementPayload,
            "Duplicate environment variables are not allowed".to_string(),
        )
    );
}

/// Creates and deploys a pair of universal canisters with the second canister being controlled by the first one
/// in addition to both canisters being controlled by the anonymous principal.
fn install_two_universal_canisters(
    env1: &StateMachine,
    env2: &StateMachine,
) -> (CanisterId, CanisterId) {
    const INITIAL_CYCLES_BALANCE: Cycles = Cycles::new(100_000_000_000_000);

    // Create a canister on each of the two subnets.
    let canister_id1 = env1
        .install_canister_with_cycles(
            UNIVERSAL_CANISTER_WASM.to_vec(),
            vec![],
            None,
            INITIAL_CYCLES_BALANCE,
        )
        .unwrap();
    let canister_id2 = env2
        .install_canister_with_cycles(
            UNIVERSAL_CANISTER_WASM.to_vec(),
            vec![],
            Some(
                CanisterSettingsArgsBuilder::new()
                    .with_controllers(vec![PrincipalId::new_anonymous(), canister_id1.into()])
                    .build(),
            ),
            INITIAL_CYCLES_BALANCE,
        )
        .unwrap();

    (canister_id1, canister_id2)
}

/// Trigger a rename for a setup according to `install_two_universal_canisters`.
fn rename_canister(
    env1: &StateMachine,
    env2: &StateMachine,
    sender_canister: CanisterId,
    old_canister_id: CanisterId,
    new_canister_id: CanisterId,
    new_version: u64,
    new_num_changes: u64,
    send_to_subnet: bool,
) -> WasmResult {
    const MAX_TICKS: usize = 100;

    env1.tick();
    let sender_canister_version = env1
        .get_latest_state()
        .canister_state(&sender_canister)
        .unwrap()
        .system_state
        .canister_version;

    let arguments = RenameCanisterArgs {
        canister_id: old_canister_id.into(),
        rename_to: RenameToArgs {
            canister_id: new_canister_id.into(),
            version: new_version,
            total_num_changes: new_num_changes,
        },
        sender_canister_version,
    };

    // Sending the request to the subnet should always work, sending to IC_00 works only if the
    // routing table maps the (old) canister id to the subnet that we want to address.
    let management_canister = if send_to_subnet {
        CanisterId::from(env2.get_subnet_id())
    } else {
        IC_00
    };

    let msg_id = env1.send_ingress(
        PrincipalId::new_anonymous(),
        sender_canister,
        "update",
        wasm()
            .call_simple(
                management_canister,
                Method::RenameCanister,
                call_args()
                    .other_side(arguments.encode())
                    .on_reject(PayloadBuilder::default().reject_message().reject().build()),
            )
            .build(),
    );

    env1.execute_xnet();
    env2.execute_xnet();
    env1.execute_xnet();
    env2.execute_xnet();
    env1.execute_xnet();

    env1.await_ingress(msg_id, MAX_TICKS).unwrap()
}

#[test]
fn can_rename_canister() {
    let (env1, env2) = two_subnets_simple();

    // Create a canister on each of the two subnets.
    let (canister_id1, canister_id2) = install_two_universal_canisters(&env1, &env2);

    let test_blob: Vec<u8> = vec![42, 41];

    // Modify the memory of the canister
    env2.execute_ingress_as(
        PrincipalId::new_anonymous(),
        canister_id2,
        "update",
        wasm()
            .stable64_grow(1)
            .stable64_write(0, &test_blob)
            .reply()
            .build(),
    )
    .unwrap();

    let verify_stable_memory = |canister_id| {
        env2.start_canister(canister_id).unwrap();

        let wasm_result = env2
            .execute_ingress_as(
                PrincipalId::new_anonymous(),
                canister_id,
                "update",
                wasm()
                    .stable64_read(0, test_blob.len() as u64)
                    .reply_data_append()
                    .reply()
                    .build(),
            )
            .unwrap();

        assert_matches!(wasm_result, WasmResult::Reply(r) if r == test_blob);
    };
    verify_stable_memory(canister_id2);

    env2.stop_canister(canister_id2).unwrap();

    let get_num_changes = |canister_id| {
        env2.get_latest_state()
            .canister_state(&canister_id)
            .unwrap()
            .system_state
            .get_canister_history()
            .get_total_num_changes()
    };

    let new_canister_id = CanisterId::from_u64(3 * CANISTER_IDS_PER_SUBNET - 1);
    let new_version = 42;
    let new_num_changes = 50;
    let old_num_changes = get_num_changes(canister_id2);

    let wasm_result = rename_canister(
        &env1,
        &env2,
        canister_id1,
        canister_id2,
        new_canister_id,
        new_version,
        new_num_changes,
        false,
    );
    assert_matches!(wasm_result, WasmResult::Reply(r) if r == EmptyBlob.encode());

    // Verify that the right canisters are present and can be run, and that version/canister history etc are as expected.
    let verify_rename_happened = |old_canister_id,
                                  new_canister_id,
                                  rename_at_version,
                                  current_version,
                                  expected_num_changes,
                                  expected_history_entry| {
        let new_canister_exists = env2
            .get_latest_state()
            .canister_state(&new_canister_id)
            .is_some();
        let old_canister_exists = env2
            .get_latest_state()
            .canister_state(&old_canister_id)
            .is_some();
        assert!(new_canister_exists);
        assert!(!old_canister_exists);

        // The version should have been updated
        assert_eq!(
            current_version,
            env2.get_latest_state()
                .canister_state(&new_canister_id)
                .unwrap()
                .system_state
                .canister_version
        );
        assert_eq!(
            expected_num_changes,
            env2.get_latest_state()
                .canister_state(&new_canister_id)
                .unwrap()
                .system_state
                .get_canister_history()
                .get_total_num_changes()
        );
        let history_entry = env2
            .get_latest_state()
            .canister_state(&new_canister_id)
            .unwrap()
            .system_state
            .get_canister_history()
            .get_changes(1)
            .next()
            .unwrap()
            .clone();

        assert_eq!(history_entry.canister_version(), rename_at_version);
        assert_eq!(history_entry.details(), &expected_history_entry);

        verify_stable_memory(new_canister_id);
    };

    let expected_history_entry = CanisterChangeDetails::rename_canister(
        canister_id2.into(),
        old_num_changes,
        new_canister_id.into(),
        new_version,
        new_num_changes,
    );
    verify_rename_happened(
        canister_id2,
        new_canister_id,
        new_version + 1,
        new_version + 1,
        new_num_changes + 1,
        expected_history_entry,
    );

    // Check that we can rename it a second time.
    env2.stop_canister(new_canister_id).unwrap();
    let third_canister_id = CanisterId::from_u64(4 * CANISTER_IDS_PER_SUBNET - 1);
    let old_num_changes = get_num_changes(new_canister_id);
    // Version and num_changes are lower than before. Version should just increment, but num_changes will go down.
    let version_before_rename = env2
        .get_latest_state()
        .canister_state(&new_canister_id)
        .unwrap()
        .system_state
        .canister_version;
    let third_version = version_before_rename - 10;
    let third_num_changes = 10;
    assert_lt!(third_version, version_before_rename);
    assert_lt!(third_num_changes, new_num_changes);

    let wasm_result = rename_canister(
        &env1,
        &env2,
        canister_id1,
        new_canister_id,
        third_canister_id,
        third_version,
        third_num_changes,
        true,
    );
    assert_matches!(wasm_result, WasmResult::Reply(r) if r == EmptyBlob.encode());

    let expected_history_entry = CanisterChangeDetails::rename_canister(
        new_canister_id.into(),
        old_num_changes,
        third_canister_id.into(),
        third_version,
        third_num_changes,
    );
    verify_rename_happened(
        new_canister_id,
        third_canister_id,
        version_before_rename + 1,
        version_before_rename + 1,
        third_num_changes + 1,
        expected_history_entry.clone(),
    );

    // Check that everything is the same after a checkpoint.
    env2.checkpointed_tick();
    // Version advanced due messages inside previous call to `verify_rename_happened`.
    verify_rename_happened(
        new_canister_id,
        third_canister_id,
        version_before_rename + 1,
        version_before_rename + 5,
        third_num_changes + 1,
        expected_history_entry,
    );
}

#[test]
fn cannot_rename_from_ingress() {
    let env = StateMachineBuilder::new().build();

    let canister_id = env.install_canister_wat(EMPTY_WAT, vec![], None);

    let new_canister_id = CanisterId::from_u64(3 * CANISTER_IDS_PER_SUBNET - 1);
    let arguments = RenameCanisterArgs {
        canister_id: canister_id.into(),
        rename_to: RenameToArgs {
            canister_id: new_canister_id.into(),
            version: 0,
            total_num_changes: 0,
        },
        sender_canister_version: 0,
    };

    let result = env.execute_ingress(IC_00, Method::RenameCanister, arguments.encode());

    assert_eq!(
        result.unwrap_err().description(),
        "Only canisters can call ic00 method rename_canister"
    );
}

#[test]
fn cannot_rename_from_non_nns() {
    let (env1, env2) = two_subnets_simple();

    // Reversed arguments so that the NNS canister is controlled by the other canister
    let (canister_id2, canister_id1) = install_two_universal_canisters(&env2, &env1);

    env1.stop_canister(canister_id1).unwrap();

    let new_canister_id = CanisterId::from_u64(3 * CANISTER_IDS_PER_SUBNET - 1);

    let wasm_result = rename_canister(
        &env2,
        &env1,
        canister_id2,
        canister_id1,
        new_canister_id,
        0,
        0,
        false,
    );

    assert_matches!(wasm_result, WasmResult::Reject(r) if r.contains("It can only be called by NNS."));
}

#[test]
fn cannot_rename_if_target_exists() {
    const INITIAL_CYCLES_BALANCE: Cycles = Cycles::new(100_000_000_000_000);

    let (env1, env2) = two_subnets_simple();
    let (canister_id1, canister_id2) = install_two_universal_canisters(&env1, &env2);

    // Install target canister id.
    let new_canister_id = env2
        .install_canister_with_cycles(
            UNIVERSAL_CANISTER_WASM.to_vec(),
            vec![],
            None,
            INITIAL_CYCLES_BALANCE,
        )
        .unwrap();

    env2.stop_canister(canister_id2).unwrap();

    let wasm_result = rename_canister(
        &env1,
        &env2,
        canister_id1,
        canister_id2,
        new_canister_id,
        0,
        0,
        false,
    );
    assert_matches!(wasm_result, WasmResult::Reject(r) if r.contains("is already installed"));
}

#[test]
fn cannot_rename_running_canister() {
    let (env1, env2) = two_subnets_simple();
    let (canister_id1, canister_id2) = install_two_universal_canisters(&env1, &env2);

    let new_canister_id = CanisterId::from_u64(3 * CANISTER_IDS_PER_SUBNET - 1);

    let wasm_result = rename_canister(
        &env1,
        &env2,
        canister_id1,
        canister_id2,
        new_canister_id,
        0,
        0,
        false,
    );
    assert_matches!(wasm_result, WasmResult::Reject(r) if r.contains("must be stopped"));
}

#[test]
fn cannot_rename_with_snapshots() {
    let (env1, env2) = two_subnets_simple();
    let (canister_id1, canister_id2) = install_two_universal_canisters(&env1, &env2);

    env2.stop_canister(canister_id2).unwrap();

    env2.take_canister_snapshot(TakeCanisterSnapshotArgs {
        canister_id: canister_id2.into(),
        replace_snapshot: None,
    })
    .unwrap();

    let new_canister_id = CanisterId::from_u64(3 * CANISTER_IDS_PER_SUBNET - 1);

    let wasm_result = rename_canister(
        &env1,
        &env2,
        canister_id1,
        canister_id2,
        new_canister_id,
        0,
        0,
        false,
    );
    assert_matches!(wasm_result, WasmResult::Reject(r) if r.contains("must not have any snapshots"));
}

#[test]
fn only_controllers_can_rename() {
    let (env1, env2) = two_subnets_simple();
    let (canister_id1, canister_id2) = install_two_universal_canisters(&env1, &env2);

    env2.stop_canister(canister_id2).unwrap();

    // Remove `canister_id1` from the list of controllers.
    env2.update_settings(
        &canister_id2,
        CanisterSettingsArgsBuilder::new()
            .with_controllers(vec![PrincipalId::new_anonymous()])
            .build(),
    )
    .unwrap();

    let new_canister_id = CanisterId::from_u64(3 * CANISTER_IDS_PER_SUBNET - 1);

    let wasm_result = rename_canister(
        &env1,
        &env2,
        canister_id1,
        canister_id2,
        new_canister_id,
        0,
        0,
        false,
    );
    assert_matches!(wasm_result, WasmResult::Reject(r) if r.contains("Only the controllers of the canister"));
}
