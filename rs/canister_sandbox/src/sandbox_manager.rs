//! The sandbox manager provides the actual functionality of the sandbox
//! process. It allows the replica controller process to manage
//! everything required in order to execute code. It holds three
//! kinds of resources that it manages on behalf of the replica
//! controller process:
//!
//! - CanisterWasm: The (wasm) code corresponding to one canister
//! - State: The heap and other (mutable) user state associated with a canister
//! - Execution: An ongoing execution of a canister, using one wasm and state
//!   object
//!
//! All of the above objects as well as the functionality provided
//! towards the controller are found in this module.
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs::File;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::protocol::id::{ExecId, MemoryId, WasmId};
use crate::protocol::sbxsvc::{CreateExecutionStateSuccessReply, OpenMemoryRequest};
use crate::protocol::structs::{
    ExecutionStateModifications, MemoryModifications, SandboxExecInput, SandboxExecOutput,
    StateModifications,
};
use crate::{controller_service::ControllerService, protocol};
use ic_config::embedders::Config as EmbeddersConfig;
use ic_embedders::{
    wasm_executor::WasmStateChanges, wasm_utils::Segments, InitialStateData, SerializedModuleBytes,
    WasmtimeEmbedder,
};
use ic_interfaces::execution_environment::{
    ExecutionMode, HypervisorError, HypervisorResult, SystemApi, WasmExecutionOutput,
};
use ic_logger::ReplicaLogger;
use ic_management_canister_types_private::Global;
use ic_replicated_state::{
    page_map::{PageAllocatorRegistry, PageMapSerialization},
    EmbedderCache, Memory, PageMap,
};
use ic_types::CanisterId;

use crate::dts::{DeterministicTimeSlicingHandler, PausedExecution};

/// A canister execution currently in progress.
struct Execution {
    /// Id of the execution. This is used in communicating back to
    /// the replica (e.g. for syscalls) such that replica can associate
    /// events with the correct execution.
    exec_id: ExecId,

    /// The canister wasm used in this execution.
    embedder_cache: Arc<EmbedderCache>,

    /// The sandbox manager that is responsible for
    /// 1) Providing the controller to talk to the replica process.
    /// 2) Creating a new execution state.
    sandbox_manager: Arc<SandboxManager>,
}

impl Execution {
    /// Creates new execution based on canister wasm and state. In order
    /// to start the execution, the given state object will be "locked" --
    /// if that cannot be done, then creation of execution will fail.
    /// The actual code to be run will be scheduled to the given
    /// thread pool.
    ///
    /// This will *actually* schedule and initiate a new execution.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_on_worker_thread(
        exec_id: ExecId,
        embedder_cache: Arc<EmbedderCache>,
        wasm_memory: Arc<Memory>,
        stable_memory: Arc<Memory>,
        sandbox_manager: Arc<SandboxManager>,
        workers: &mut threadpool::ThreadPool,
        exec_input: SandboxExecInput,
        total_timer: std::time::Instant,
    ) {
        let wasm_memory = (*wasm_memory).clone();
        let stable_memory = (*stable_memory).clone();

        let execution = Arc::new(Self {
            exec_id,
            embedder_cache,
            sandbox_manager,
        });

        workers.execute(move || {
            execution.run(exec_id, exec_input, wasm_memory, stable_memory, total_timer)
        });
    }

    // Actual wasm code execution -- this is run on the target thread
    // in the thread pool.
    fn run(
        &self,
        exec_id: ExecId,
        exec_input: SandboxExecInput,
        mut wasm_memory: Memory,
        mut stable_memory: Memory,
        total_timer: std::time::Instant,
    ) {
        let run_timer = std::time::Instant::now();

        let message_instruction_limit =
            exec_input.execution_parameters.instruction_limits.message();
        let instruction_limit_to_report = exec_input
            .execution_parameters
            .instruction_limits
            .limit_to_report();
        let slice_instruction_limit = exec_input.execution_parameters.instruction_limits.slice();
        let sandbox_manager = Arc::clone(&self.sandbox_manager);
        let out_of_instructions_handler = DeterministicTimeSlicingHandler::new(
            i64::try_from(instruction_limit_to_report.get()).unwrap_or(i64::MAX),
            i64::try_from(message_instruction_limit.get()).unwrap_or(i64::MAX),
            i64::try_from(slice_instruction_limit.get()).unwrap_or(i64::MAX),
            move |slice, paused_execution| {
                {
                    let mut guard = sandbox_manager.repr.lock().unwrap();
                    guard.paused_executions.insert(exec_id, paused_execution);
                }
                sandbox_manager
                    .controller
                    .execution_paused(protocol::ctlsvc::ExecutionPausedRequest { exec_id, slice });
            },
        );

        let (
            slice,
            WasmExecutionOutput {
                wasm_result,
                num_instructions_left,
                allocated_bytes,
                allocated_guaranteed_response_message_bytes,
                instance_stats,
                system_api_call_counters,
            },
            deltas,
            mut instance_or_system_api,
        ) = ic_embedders::wasm_executor::process(
            exec_input.func_ref,
            exec_input.api_type,
            exec_input.canister_current_memory_usage,
            exec_input.canister_current_message_memory_usage,
            exec_input.execution_parameters,
            exec_input.subnet_available_memory,
            exec_input.sandbox_safe_system_state,
            &self.embedder_cache,
            &self.sandbox_manager.embedder,
            &mut wasm_memory,
            &mut stable_memory,
            &exec_input.globals,
            self.sandbox_manager.log.clone(),
            exec_input.wasm_reserved_pages,
            Rc::new(out_of_instructions_handler),
        );

        let system_api = match &mut instance_or_system_api {
            // Here we use `store_data_mut` instead of
            // `into_store_data` because the later will drop the
            // wasmtime Instance which can be an expensive
            // operation. Mutating the store instead allows us
            // to delay the drop until after the execution
            // completed message is sent back to the main
            // process.
            Ok(instance) => instance
                .store_data_mut()
                .system_api_mut()
                .expect("System api not present in the wasmtime instance"),
            Err(system_api) => system_api,
        };

        match wasm_result {
            Ok(_) => {
                let state_modifications = {
                    let execution_state_modifications = deltas.map(
                        |WasmStateChanges {
                             dirty_page_indices,
                             globals,
                         }| {
                            ExecutionStateModifications::new(
                                globals,
                                &wasm_memory,
                                &stable_memory,
                                &dirty_page_indices.wasm_memory_delta,
                                &dirty_page_indices.stable_memory_delta,
                            )
                        },
                    );

                    StateModifications {
                        execution_state_modifications,
                        system_state_modifications: system_api.take_system_state_modifications(),
                    }
                };
                if state_modifications.execution_state_modifications.is_some() {
                    self.sandbox_manager
                        .add_memory(exec_input.next_wasm_memory_id, wasm_memory);
                    self.sandbox_manager
                        .add_memory(exec_input.next_stable_memory_id, stable_memory);
                }
                let wasm_output = WasmExecutionOutput {
                    wasm_result,
                    allocated_bytes,
                    allocated_guaranteed_response_message_bytes,
                    num_instructions_left,
                    instance_stats,
                    system_api_call_counters,
                };
                self.sandbox_manager.controller.execution_finished(
                    protocol::ctlsvc::ExecutionFinishedRequest {
                        exec_id: self.exec_id,
                        exec_output: SandboxExecOutput {
                            slice,
                            wasm: wasm_output,
                            state: state_modifications,
                            execute_total_duration: total_timer.elapsed(),
                            execute_run_duration: run_timer.elapsed(),
                        },
                    },
                );
            }
            Err(HypervisorError::Aborted) => {
                // Do not send any reply to the controller because the execution
                // was aborted and the controller removed `exec_id` on its side.
            }
            Err(err) => {
                // Set the execution error in the system API to capture cases
                // where the Wasm execution trapped outside of the system API.
                // This is important to ensure that the proper state modifications
                // are extracted from the system API.
                system_api.set_execution_error(err.clone());
                let wasm_output = WasmExecutionOutput {
                    wasm_result: Err(err),
                    num_instructions_left,
                    allocated_bytes,
                    allocated_guaranteed_response_message_bytes,
                    instance_stats,
                    system_api_call_counters,
                };

                self.sandbox_manager.controller.execution_finished(
                    protocol::ctlsvc::ExecutionFinishedRequest {
                        exec_id: self.exec_id,
                        exec_output: SandboxExecOutput {
                            slice,
                            wasm: wasm_output,
                            // If the execution resulted in an error, we only want to persist
                            // the system state modifications.
                            state: StateModifications {
                                execution_state_modifications: None,
                                system_state_modifications: system_api
                                    .take_system_state_modifications(),
                            },
                            execute_total_duration: total_timer.elapsed(),
                            execute_run_duration: run_timer.elapsed(),
                        },
                    },
                );
            }
        }
    }
}

/// Manages the entirety of the sandbox process. It provides the methods
/// through which the controller process (the replica) manages the
/// sandboxed execution.
pub struct SandboxManager {
    repr: Mutex<SandboxManagerInt>,
    controller: Arc<dyn ControllerService>,
    embedder: Arc<WasmtimeEmbedder>,
    page_allocator_registry: Arc<PageAllocatorRegistry>,
    log: ReplicaLogger,
}
struct SandboxManagerInt {
    caches: HashMap<WasmId, Arc<EmbedderCache>>,
    memories: HashMap<MemoryId, Arc<Memory>>,
    paused_executions: HashMap<ExecId, PausedExecution>,
    workers_for_replicated_execution: threadpool::ThreadPool,
    workers_for_non_replicated_execution: threadpool::ThreadPool,
    workers_for_cleanup: threadpool::ThreadPool,
}

impl SandboxManager {
    /// Creates new sandbox manager. In order to operate, it needs
    /// an established backward RPC channel to the controller process
    /// to relay e.g. syscalls and completions.
    pub fn new(
        controller: Arc<dyn ControllerService>,
        config: EmbeddersConfig,
        log: ReplicaLogger,
    ) -> Self {
        let embedder = Arc::new(WasmtimeEmbedder::new(config.clone(), log.clone()));
        SandboxManager {
            repr: Mutex::new(SandboxManagerInt {
                caches: HashMap::new(),
                memories: HashMap::new(),
                paused_executions: HashMap::new(),
                workers_for_replicated_execution: threadpool::ThreadPool::new(1),
                workers_for_non_replicated_execution: threadpool::ThreadPool::new(
                    config.query_execution_threads_per_canister,
                ),
                workers_for_cleanup: threadpool::ThreadPool::new(1),
            }),
            controller,
            embedder,
            log,
            page_allocator_registry: Arc::new(PageAllocatorRegistry::new()),
        }
    }

    pub fn open_wasm_serialized(
        &self,
        wasm_id: WasmId,
        serialized_module: &SerializedModuleBytes,
    ) -> HypervisorResult<(Arc<EmbedderCache>, Duration)> {
        let mut guard = self.repr.lock().unwrap();
        assert!(
            !guard.caches.contains_key(&wasm_id),
            "Failed to open wasm session {}: id is already in use",
            wasm_id,
        );
        let deserialization_timer = Instant::now();
        let instance_pre = self
            .embedder
            .deserialize_module_and_pre_instantiate(serialized_module);
        let cache = Arc::new(EmbedderCache::new(instance_pre.clone()));
        let deserialization_time = deserialization_timer.elapsed();
        guard.caches.insert(wasm_id, Arc::clone(&cache));
        match instance_pre {
            Ok(_) => Ok((cache, deserialization_time)),
            Err(err) => Err(err),
        }
    }

    pub fn open_wasm_via_file(
        &self,
        wasm_id: WasmId,
        serialized_module_file: File,
    ) -> HypervisorResult<(Arc<EmbedderCache>, Duration)> {
        let mut guard = self.repr.lock().unwrap();
        assert!(
            !guard.caches.contains_key(&wasm_id),
            "Failed to open wasm session {}: id is already in use",
            wasm_id,
        );
        let deserialization_timer = Instant::now();
        let instance_pre = self
            .embedder
            .read_file_and_pre_instantiate(serialized_module_file);
        let cache = Arc::new(EmbedderCache::new(instance_pre.clone()));
        let deserialization_time = deserialization_timer.elapsed();
        guard.caches.insert(wasm_id, Arc::clone(&cache));
        match instance_pre {
            Ok(_) => Ok((cache, deserialization_time)),
            Err(err) => Err(err),
        }
    }

    /// Closes previously opened wasm instance, by id.
    pub fn close_wasm(&self, wasm_id: WasmId) {
        let mut guard = self.repr.lock().unwrap();
        let removed = guard.caches.remove(&wasm_id);
        assert!(
            removed.is_some(),
            "Failed to close wasm session {}: id not found",
            wasm_id
        );
    }

    /// Opens a new memory requested by the replica process.
    pub fn open_memory(&self, request: OpenMemoryRequest) {
        let mut guard = self.repr.lock().unwrap();
        guard.open_memory(request, &self.page_allocator_registry);
    }

    /// Adds a new memory after sandboxed execution.
    fn add_memory(&self, memory_id: MemoryId, memory: Memory) {
        let mut guard = self.repr.lock().unwrap();
        guard.add_memory(memory_id, memory);
    }

    /// Closes previously opened memory instance, by id.
    pub fn close_memory(&self, memory_id: MemoryId) {
        let mut guard = self.repr.lock().unwrap();
        let removed = guard.memories.remove(&memory_id);
        assert!(
            removed.is_some(),
            "Failed to close state {}: id not found",
            memory_id
        );
        // Dropping memory may be expensive. Do it on a worker thread to avoid
        // blocking the main thread of the sandbox process.
        guard.workers_for_cleanup.execute(move || drop(removed));
    }

    /// Starts Wasm execution using specific code and state, passing
    /// execution input.
    ///
    /// Note that inside here we start a transaction and the state of
    /// execution can not and does not change while we are processing
    /// this particular session.
    pub fn start_execution(
        sandbox_manager: &Arc<SandboxManager>,
        exec_id: ExecId,
        wasm_id: WasmId,
        wasm_memory_id: MemoryId,
        stable_memory_id: MemoryId,
        exec_input: SandboxExecInput,
    ) {
        let total_timer = std::time::Instant::now();
        let mut guard = sandbox_manager.repr.lock().unwrap();
        let wasm_runner = guard.caches.get(&wasm_id).unwrap_or_else(|| {
            unreachable!(
                "Failed to open exec session {}: wasm {} not found",
                exec_id, wasm_id
            )
        });
        let wasm_memory = guard.memories.get(&wasm_memory_id).unwrap_or_else(|| {
            unreachable!(
                "Failed to open exec session {}: wasm memory {} not found",
                exec_id, wasm_memory_id,
            )
        });
        let stable_memory = guard.memories.get(&stable_memory_id).unwrap_or_else(|| {
            unreachable!(
                "Failed to open exec session {}: stable memory {} not found",
                exec_id, stable_memory_id,
            )
        });
        match exec_input.execution_parameters.execution_mode {
            ExecutionMode::Replicated => Execution::start_on_worker_thread(
                exec_id,
                Arc::clone(wasm_runner),
                Arc::clone(wasm_memory),
                Arc::clone(stable_memory),
                Arc::clone(sandbox_manager),
                &mut guard.workers_for_replicated_execution,
                exec_input,
                total_timer,
            ),
            ExecutionMode::NonReplicated => Execution::start_on_worker_thread(
                exec_id,
                Arc::clone(wasm_runner),
                Arc::clone(wasm_memory),
                Arc::clone(stable_memory),
                Arc::clone(sandbox_manager),
                &mut guard.workers_for_non_replicated_execution,
                exec_input,
                total_timer,
            ),
        };
    }

    /// Resume the paused Wasm execution.
    pub fn resume_execution(sandbox_manager: &Arc<SandboxManager>, exec_id: ExecId) {
        let paused_execution = {
            let mut guard = sandbox_manager.repr.lock().unwrap();
            guard
                .paused_executions
                .remove(&exec_id)
                .unwrap_or_else(|| unreachable!("Failed to get paused execution {}", exec_id))
        };
        paused_execution.resume();
    }

    /// Abort the paused Wasm execution.
    pub fn abort_execution(sandbox_manager: &Arc<SandboxManager>, exec_id: ExecId) {
        let paused_execution = {
            let mut guard = sandbox_manager.repr.lock().unwrap();
            guard
                .paused_executions
                .remove(&exec_id)
                .unwrap_or_else(|| unreachable!("Failed to get paused execution {}", exec_id))
        };
        paused_execution.abort();
    }

    /// Takes ownership of the passed in file descriptors.
    pub fn create_execution_state(
        &self,
        wasm_id: WasmId,
        bytes: File,
        initial_state_data: File,
        wasm_page_map: PageMapSerialization,
        next_wasm_memory_id: MemoryId,
        canister_id: CanisterId,
        stable_memory_page_map: PageMapSerialization,
    ) -> HypervisorResult<CreateExecutionStateSuccessReply> {
        let timer = Instant::now();
        let (embedder_cache, deserialization_time) = self.open_wasm_via_file(wasm_id, bytes)?;

        // Reading from the initial state file would mutate the file descriptor
        // and later or concurrent uses of the same cache entry would fail. But
        // we can mmap the data without mutating the fd.
        let initial_state_data: InitialStateData = {
            use nix::sys::mman::{mmap, MapFlags, ProtFlags};
            use std::os::{fd::AsRawFd, unix::fs::MetadataExt};

            let mmap_size = initial_state_data.metadata().unwrap().size() as usize;
            let data = if mmap_size == 0 {
                &[]
            } else {
                // SAFETY: The address is valid because it is null, we have checked
                // the size is positive and the fd is valid since it comes from a
                // `File`. We're mapping privately so the data won't be mutated.
                let mmap_ptr = unsafe {
                    mmap(
                        std::ptr::null_mut(),
                        mmap_size,
                        ProtFlags::PROT_READ,
                        MapFlags::MAP_PRIVATE,
                        initial_state_data.as_raw_fd(),
                        0,
                    )
                }
                .unwrap_or_else(|err| panic!("Reading InitialStateData failed: {:?}", err))
                    as *mut u8;
                // SAFETY: We've mmapped `mmap_size` and gotten a succesful
                // reply at address `mmap_ptr` and the mapping is readonly
                // private.
                unsafe { std::slice::from_raw_parts(mmap_ptr, mmap_size) }
            };
            bincode::deserialize(data).unwrap()
        };

        let (wasm_memory_modifications, exported_globals) = self
            .create_initial_memory_and_globals(
                &embedder_cache,
                &initial_state_data.data_segments,
                wasm_page_map,
                next_wasm_memory_id,
                canister_id,
                stable_memory_page_map,
            )?;
        Ok(CreateExecutionStateSuccessReply {
            wasm_memory_modifications,
            exported_globals,
            deserialization_time,
            total_sandbox_time: timer.elapsed(),
        })
    }

    fn create_initial_memory_and_globals(
        &self,
        embedder_cache: &EmbedderCache,
        data_segments: &Segments,
        wasm_page_map: PageMapSerialization,
        next_wasm_memory_id: MemoryId,
        canister_id: CanisterId,
        stable_memory_page_map: PageMapSerialization,
    ) -> HypervisorResult<(MemoryModifications, Vec<Global>)> {
        let embedder = Arc::clone(&self.embedder);

        let mut wasm_page_map =
            PageMap::deserialize(wasm_page_map, &self.page_allocator_registry).unwrap();
        let stable_mem_page_map =
            PageMap::deserialize(stable_memory_page_map, &self.page_allocator_registry).unwrap();

        let (exported_globals, wasm_memory_delta, wasm_memory_size) =
            ic_embedders::wasm_executor::get_initial_globals_and_memory(
                data_segments,
                embedder_cache,
                &embedder,
                &mut wasm_page_map,
                canister_id,
                &stable_mem_page_map,
            )?;

        let wasm_memory = Memory::new(wasm_page_map, wasm_memory_size);

        // Send all necessary data for creating the execution state to replica.
        let wasm_memory_modifications = MemoryModifications {
            page_delta: wasm_memory.page_map.serialize_delta(&wasm_memory_delta),
            size: wasm_memory_size,
        };

        // Save the memory for future message executions.
        self.add_memory(next_wasm_memory_id, wasm_memory);

        Ok((wasm_memory_modifications, exported_globals))
    }
}

impl SandboxManagerInt {
    fn open_memory(
        &mut self,
        request: OpenMemoryRequest,
        page_allocator_registry: &PageAllocatorRegistry,
    ) {
        let page_map =
            PageMap::deserialize(request.memory.page_map, page_allocator_registry).unwrap();
        let memory = Memory::new(page_map, request.memory.num_wasm_pages);
        self.add_memory(request.memory_id, memory);
    }

    fn add_memory(&mut self, memory_id: MemoryId, memory: Memory) {
        assert!(
            !self.memories.contains_key(&memory_id),
            "Failed to open memory {}: id is already in use",
            memory_id
        );
        let memory = Arc::new(memory);
        self.memories.insert(memory_id, memory);
    }
}
