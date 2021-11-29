use ic_canister_sandbox_common::protocol;
use ic_canister_sandbox_common::protocol::id::{StateId, WasmId};
use ic_canister_sandbox_common::protocol::sbxsvc::{
    MemoryDeltaSerialization, MemorySerialization, StateSerialization,
};
use ic_canister_sandbox_common::sandbox_service::SandboxService;
use ic_embedders::{WasmExecutionInput, WasmExecutionOutput};
use ic_logger::ReplicaLogger;
use ic_replicated_state::canister_state::execution_state::{
    SandboxExecutionState, SandboxExecutionStateHandle, SandboxExecutionStateOwner,
    SandboxExecutionStateSynchronization, WasmBinary,
};
use ic_replicated_state::page_map::PageAllocatorDelta;
use ic_replicated_state::{EmbedderCache, ExecutionState, Memory, PageIndex};
use ic_system_api::{StaticSystemState, SystemStateAccessorDirect};
use ic_types::CanisterId;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use crate::active_execution_state_registry::ActiveExecutionStateRegistry;
use crate::controller_service_impl::ControllerServiceImpl;
use crate::elastic_cache::Cache;
use crate::launch_as_process::create_sandbox_process;
use crate::unique_id::UniqueId;

#[derive(Clone)]
pub struct SandboxProcess {
    /// Registry for all executions that are currently running on
    /// this backend process.
    execution_states: Arc<ActiveExecutionStateRegistry>,

    /// Handle for IPC down to sandbox.
    sandbox_service: Arc<dyn SandboxService>,

    // Cache compiled objects on the sandbox process.
    compilation_cache: Arc<Cache<UniqueId, WasmId>>,
}

/// Manages the lifetime of a remote execution state and provides its id.
pub struct OpenedState {
    sandbox_service: Arc<dyn SandboxService>,
    state_id: StateId,
}

impl OpenedState {
    fn new(sandbox_service: Arc<dyn SandboxService>) -> Self {
        Self {
            sandbox_service,
            state_id: StateId::new(),
        }
    }
}

impl SandboxExecutionStateOwner for OpenedState {
    fn get_id(&self) -> usize {
        self.state_id.as_usize()
    }
}

impl Drop for OpenedState {
    fn drop(&mut self) {
        self.sandbox_service
            .close_state(protocol::sbxsvc::CloseStateRequest {
                state_id: self.state_id,
            })
            .on_completion(|_| {});
    }
}

impl std::fmt::Debug for OpenedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenedState")
            .field("state_id", &self.state_id)
            .finish()
    }
}

/// Manages sandboxed processes, forwards requests to the appropriate
/// process.
pub struct SandboxedExecutionController {
    backends: Mutex<HashMap<CanisterId, SandboxProcess>>,
    logger: ReplicaLogger,
    compile_count: AtomicU64,
}

impl SandboxedExecutionController {
    /// Create a new sandboxed execution controller. It provides the
    /// same interface as the
    pub fn new(logger: ReplicaLogger) -> Self {
        Self {
            backends: Mutex::new(HashMap::new()),
            logger,
            compile_count: AtomicU64::new(0),
        }
    }

    fn get_sandbox_process(&self, canister_id: &CanisterId) -> SandboxProcess {
        let mut guard = self.backends.lock().unwrap();
        if let Some(sandbox_process) = (*guard).get(canister_id) {
            // Sandbox backend running for this canister already.
            sandbox_process.clone()
        } else {
            // No sandbox backend found for this canister. Start a new
            // one and register it.
            let reg = Arc::new(ActiveExecutionStateRegistry::new());
            let controller_service =
                ControllerServiceImpl::new(Arc::clone(&reg), self.logger.clone());
            let sandbox_service = create_sandbox_process(controller_service, canister_id);

            let sandbox_service_copy = Arc::clone(&sandbox_service);

            // Set up compilation cache. Set up "deleter" in cache to issue
            // CloseWasmRequest when we drop something from cache.
            let compilation_cache = Cache::new(
                move |_key, wasm_id: WasmId| {
                    sandbox_service_copy
                        .close_wasm(protocol::sbxsvc::CloseWasmRequest { wasm_id })
                        .on_completion(|_| {});
                },
                2,
            );
            let sandbox_process = SandboxProcess {
                execution_states: reg,
                sandbox_service,
                compilation_cache,
            };
            (*guard).insert(*canister_id, sandbox_process.clone());
            sandbox_process
        }
    }

    fn get_wasm_binary_key(&self, wasm_binary: &WasmBinary) -> UniqueId {
        let mut embedder_cache = wasm_binary.embedder_cache.lock().unwrap();
        if let Some(cache) = embedder_cache.as_ref() {
            if let Some(unique_id) = cache.downcast::<UniqueId>() {
                return unique_id.clone();
            }
        }
        let unique_id = UniqueId::new();
        *embedder_cache = Some(EmbedderCache::new(unique_id.clone()));
        unique_id
    }

    pub fn process(
        &self,
        WasmExecutionInput {
            api_type,
            system_state,
            canister_current_memory_usage,
            execution_parameters,
            func_ref,
            mut execution_state,
            cycles_account_manager,
        }: WasmExecutionInput,
    ) -> WasmExecutionOutput {
        // Determine which process we want to run this on.
        let sandbox_process = self.get_sandbox_process(&system_state.canister_id());

        // Create channel through which we will receive the execution
        // output from closure (running by IPC thread at end of
        // execution).
        let (tx, rx) = std::sync::mpsc::sync_channel::<Option<protocol::structs::ExecOutput>>(1);

        let static_system_state =
            StaticSystemState::new(&system_state, cycles_account_manager.subnet_type());

        // Generate an ID for this execution, register it. We need to
        // pass the system state accessor as well as the completion
        // function that gets our result back in the end.
        let exec_id = sandbox_process.execution_states.register_execution(
            SystemStateAccessorDirect::new(system_state, cycles_account_manager),
            move |_id, exec_output| {
                tx.send(exec_output).unwrap();
            },
        );

        // Now set up resources on the sandbox to drive the execution.

        // Get compiled wasm object in sandbox. Ask cache first, upload + compile if
        // needed.
        let wasm_key = self.get_wasm_binary_key(&*execution_state.wasm_binary);
        let wasm_id = *sandbox_process.compilation_cache.get(&wasm_key, |_key| {
            let wasm_id = WasmId::new();
            sandbox_process
                .sandbox_service
                .open_wasm(protocol::sbxsvc::OpenWasmRequest {
                    wasm_id,
                    wasm_file_path: None,
                    wasm_src: execution_state.wasm_binary.binary.as_slice().to_vec(),
                })
                .on_completion(|_| {});
            self.compile_count.fetch_add(1, Ordering::Relaxed);
            wasm_id
        });

        let state_handle = open_remote_state(&sandbox_process.sandbox_service, &execution_state);
        let state_id = StateId::from(state_handle.get_id());
        let subnet_available_memory = execution_parameters.subnet_available_memory.clone();

        sandbox_process
            .sandbox_service
            .open_execution(protocol::sbxsvc::OpenExecutionRequest {
                exec_id,
                wasm_id,
                state_id,
                exec_input: protocol::structs::ExecInput {
                    func_ref,
                    api_type,
                    globals: execution_state.exported_globals.clone(),
                    canister_current_memory_usage,
                    execution_parameters,
                    static_system_state,
                },
            })
            .on_completion(|_| {});

        // Wait for completion.
        let exec_output = rx.recv().unwrap().unwrap();

        // Release all resources on the sandbox process (compiled wasm is
        // left cached).
        sandbox_process
            .sandbox_service
            .close_execution(protocol::sbxsvc::CloseExecutionRequest { exec_id })
            .on_completion(|_| {});

        // Release the system state (we need to return it to the caller).
        let system_state = sandbox_process
            .execution_states
            .unregister_execution(exec_id)
            .unwrap()
            .release_system_state();

        // Unless execution trapped, commit state.
        if exec_output.wasm_result.is_ok() {
            if let Some(state_modifications) = exec_output.state_modifications {
                // TODO: If a canister has broken out of wasm then it might have allocated more
                // wasm or stable memory then allowed. We should add an additional check here
                // that thet canister is still within it's allowed memory usage.
                let wasm_memory_page_refs: Vec<_> = state_modifications
                    .wasm_memory_page_delta
                    .iter()
                    .map(|page| (page.index, &page.bytes))
                    .collect();
                let wasm_memory_delta = execution_state
                    .wasm_memory
                    .page_map
                    .update(&wasm_memory_page_refs[..]);
                execution_state.wasm_memory.size = state_modifications.wasm_memory_size;

                let stable_memory_page_refs: Vec<_> = state_modifications
                    .stable_memory_page_delta
                    .iter()
                    .map(|page| (page.index, &page.bytes))
                    .collect();
                let stable_memory_delta = execution_state
                    .stable_memory
                    .page_map
                    .update(&stable_memory_page_refs[..]);
                execution_state.stable_memory.size = state_modifications.stable_memory_size;

                execution_state.exported_globals = state_modifications.globals;

                execution_state.sandbox_state = SandboxExecutionState::delta(
                    state_handle,
                    wasm_memory_delta,
                    stable_memory_delta,
                );

                // Unconditionally update the subnet available memory.
                // This value is actually a shared value under a RwLock, and the non-sandbox
                // workflow involves directly updating the value. So failed executions are
                // responsible for reseting the value themselves (see
                // `SystemApiImpl::take_execution_result`).
                subnet_available_memory.set(state_modifications.subnet_available_memory);
            }
        }

        WasmExecutionOutput {
            wasm_result: exec_output.wasm_result,
            num_instructions_left: exec_output.num_instructions_left,
            system_state,
            execution_state,
            instance_stats: exec_output.instance_stats,
        }
    }

    pub fn compile_count_for_testing(&self) -> u64 {
        self.compile_count.load(Ordering::Relaxed)
    }
}

// Returns the id of the remote state after making sure that
// the remote state is in sync with the local state.
fn open_remote_state(
    sandbox_service: &Arc<dyn SandboxService>,
    execution_state: &ExecutionState,
) -> SandboxExecutionStateHandle {
    let mut guard = execution_state.sandbox_state.lock().unwrap();
    match &*guard {
        SandboxExecutionState::Synced(id) => id.clone(),
        SandboxExecutionState::Unsynced(synchronisation) => {
            let globals = execution_state.exported_globals.clone();
            let state = match synchronisation {
                SandboxExecutionStateSynchronization::Full => {
                    let wasm_memory_page_map = execution_state.wasm_memory.page_map.serialize();
                    let wasm_memory = MemorySerialization {
                        page_map: wasm_memory_page_map,
                        num_wasm_pages: execution_state.wasm_memory.size,
                    };
                    let stable_memory_page_map = execution_state.stable_memory.page_map.serialize();
                    let stable_memory = MemorySerialization {
                        page_map: stable_memory_page_map,
                        num_wasm_pages: execution_state.stable_memory.size,
                    };
                    StateSerialization::Full {
                        globals,
                        wasm_memory,
                        stable_memory,
                    }
                }
                SandboxExecutionStateSynchronization::Delta {
                    parent_state_handle,
                    wasm_memory_pages,
                    wasm_memory_page_allocator,
                    stable_memory_pages,
                    stable_memory_page_allocator,
                } => {
                    let wasm_memory = serialize_memory_delta(
                        &execution_state.wasm_memory,
                        wasm_memory_pages,
                        wasm_memory_page_allocator,
                    );
                    let stable_memory = serialize_memory_delta(
                        &execution_state.stable_memory,
                        stable_memory_pages,
                        stable_memory_page_allocator,
                    );
                    StateSerialization::Delta {
                        parent_state_id: StateId::from(parent_state_handle.get_id()),
                        globals,
                        wasm_memory,
                        stable_memory,
                    }
                }
            };
            let opened_state = OpenedState::new(Arc::clone(sandbox_service));
            sandbox_service
                .open_state(protocol::sbxsvc::OpenStateRequest {
                    state_id: StateId::from(opened_state.get_id()),
                    state,
                })
                .on_completion(|_| {});
            let id = SandboxExecutionStateHandle::new(Arc::new(opened_state));
            *guard = SandboxExecutionState::Synced(id.clone());
            id
        }
    }
}

// Creates a serialization-friendly representation of the memory delta from
// the list of modified pages and the page allocator delta.
fn serialize_memory_delta<T: Copy>(
    memory: &Memory<T>,
    pages: &[PageIndex],
    page_allocator: &PageAllocatorDelta,
) -> MemoryDeltaSerialization<T> {
    let page_delta = memory.page_map.serialize_delta(pages);
    let page_allocator = match page_allocator {
        PageAllocatorDelta::Unchanged => None,
        PageAllocatorDelta::Created => Some(memory.page_map.serialize_allocator()),
    };
    MemoryDeltaSerialization {
        page_delta,
        page_allocator,
        num_wasm_pages: memory.size,
    }
}