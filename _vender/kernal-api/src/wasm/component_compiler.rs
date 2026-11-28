//! Private Component candidate. All process work remains in OperationHub.
// Candidate entry point is currently exercised only by the artifact proof.
#![cfg_attr(not(test), allow(dead_code))]
use crate::async_engine::RuntimeHandle;
#[cfg(test)]
use crate::operations::BlobLimits;
use crate::operations::{
    ComponentResourceBudget, ComponentResourceLease, HubError, OpaqueToken, OperationHub,
};
use std::sync::Arc;
use wasmtime::component::{
    Accessor, HasData, Resource, ResourceTable, Source, StreamConsumer, StreamResult,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::compiler_dispatch::tests::compiler_helper_spec;
    use std::time::Duration;
    mod proof {
        wasmtime::component::bindgen!({
            path: "src/guest_component_compiler.wit", world: "compiler-proof",
        });
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Existing end-to-end output-copy proof is 104 lines"
    )]
    #[test]
    fn output_copy_is_one_shot_and_keeps_lowering_allowance_until_drop() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    let hub = OperationHub::new(64, 64).unwrap();
                    let budget = ComponentResourceBudget::new(Arc::clone(&hub));
                    let spec = compiler_helper_spec();
                    let grant = hub
                        .grant_compiler(0, spec, Duration::from_secs(15))
                        .unwrap();
                    let spawn = Operation {
                        hub: Arc::clone(&hub),
                        token: hub
                            .submit_compiler_spawn(runtime.handle(), 0, grant)
                            .unwrap(),
                        kind: OperationKind::Spawn,
                    };
                    let process = OpaqueToken::from_wire(spawn.wait().await.unwrap());
                    let mut state = State {
                        hub: Arc::clone(&hub),
                        budget: Arc::clone(&budget),
                        runtime: runtime.handle(),
                        table: ResourceTable::new(),
                        initial: None,
                        limits: wasmtime::StoreLimitsBuilder::new().build(),
                        output_copies: 0,
                        cancel_read: None,
                    };
                    let mut copied = None;
                    // stdout and stderr EOF events may race their buffered chunks.
                    // Consume only a small, fixed number of normal terminal events;
                    // this proof specifically requires a copied payload.
                    for _ in 0..8 {
                        let permit = budget.acquire(65536).unwrap();
                        let operation = Operation {
                            hub: Arc::clone(&hub),
                            token: hub
                                .submit_compiler_output(runtime.handle(), 0, process)
                                .unwrap(),
                            kind: OperationKind::Read,
                        };
                        operation.ready().await.unwrap();
                        let output = state
                            .table
                            .push(Output {
                                operation: Some(operation),
                                _permit: permit,
                            })
                            .unwrap();
                        let before = hub.snapshot().retained_transfer_capacity;
                        let data = compilers::HostOutput::copy(
                            &mut state,
                            Resource::new_borrow(output.rep()),
                        )
                        .unwrap()
                        .unwrap();
                        match &data.tag {
                            OutputTag::Stdout | OutputTag::Stderr => {
                                assert!(!data.bytes.is_empty());
                                assert!(data.bytes.len() <= 65536);
                                copied = Some((output, before, data));
                                break;
                            }
                            OutputTag::StdoutEof | OutputTag::StderrEof | OutputTag::Exhausted => {
                                assert!(data.bytes.is_empty());
                                drop(data);
                                compilers::HostOutput::drop(&mut state, output).unwrap();
                            }
                            OutputTag::StdoutAbandoned
                            | OutputTag::StderrAbandoned
                            | OutputTag::StdoutError
                            | OutputTag::StderrError => {
                                panic!("unexpected output failure: {:?}", data.tag)
                            }
                        }
                    }
                    let (output, before, data) = copied.expect("fixture produced no output chunk");
                    let output_copies = state.output_copies;
                    assert_eq!(hub.snapshot().retained_transfer_capacity, before - 65536);
                    assert_eq!(budget.live_resources(), 1);
                    assert!(matches!(
                        compilers::HostOutput::copy(&mut state, Resource::new_borrow(output.rep()))
                            .unwrap(),
                        Err(Error::Rejected)
                    ));
                    assert_eq!(state.output_copies, output_copies);
                    assert_eq!(hub.snapshot().retained_transfer_capacity, before - 65536);
                    // In real execution the synchronous canonical lowering owns this
                    // Vec until it returns; only then may guest code drop the resource.
                    drop(data);
                    compilers::HostOutput::drop(&mut state, output).unwrap();
                    assert_eq!(
                        hub.snapshot().retained_transfer_capacity,
                        before - 2 * 65536
                    );
                    assert_eq!(budget.live_resources(), 0);
                    budget.begin_teardown().unwrap();
                    hub.close_all(crate::operations::Terminal::Closed);
                    drop(state);
                    hub.join_process_jobs().await.unwrap();
                    budget.finish_teardown().unwrap();
                    assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    #[ignore = "requires freshly built KERNAL_COMPONENT_COMPILER_WASM"]
    fn actual_guest_spawns_drains_hashes_waits_and_closes() {
        execute_artifact("KERNAL_COMPONENT_COMPILER_WASM", false, false);
    }

    #[test]
    #[ignore = "requires lowering-trap-proof KERNAL_COMPONENT_COMPILER_TRAP_WASM encoded with --trap-realloc"]
    fn actual_guest_lowering_trap_retains_credit_until_store_destruction() {
        execute_artifact("KERNAL_COMPONENT_COMPILER_TRAP_WASM", true, false);
    }

    #[test]
    #[ignore = "requires freshly built KERNAL_COMPONENT_COMPILER_WASM"]
    fn actual_guest_cancelled_read_return_drops_pending_authority() {
        execute_artifact("KERNAL_COMPONENT_COMPILER_WASM", false, true);
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Existing function complexity measured at ratchet adoption"
    )]
    fn execute_artifact(variable: &str, expect_lowering_trap: bool, cancel_read: bool) {
        let bytes = std::fs::read(std::env::var_os(variable).expect("Component compiler artifact"))
            .unwrap();
        let mut config = wasmtime::Config::new();
        config
            .wasm_component_model_async(true)
            .concurrency_support(true)
            .consume_fuel(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let component = wasmtime::component::Component::new(&engine, bytes).unwrap();
        let mut linker = wasmtime::component::Linker::new(&engine);
        bindings::CompilerClient::add_to_linker::<State, Host>(&mut linker, |state| state).unwrap();
        hash_bindings::HashClient::add_to_linker::<State, wasmtime::component::HasSelf<State>>(
            &mut linker,
            |state| state,
        )
        .unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        // Match the Core artifact proof's explicitly larger bounded payload:
        // normal Component probe defaults remain unchanged.
        let hub = OperationHub::with_blob_limits(
            64,
            64,
            BlobLimits::new(64 * 1024, 4 * 1024 * 1024, 8 * 1024 * 1024).unwrap(),
        )
        .unwrap();
        let budget = ComponentResourceBudget::new(Arc::clone(&hub));
        let cancellation = crate::async_engine::CancellationSource::new();
        let output = tempfile::tempdir().unwrap();
        let artifact = output.path().join("compiler-artifact");
        let neighbor = output.path().join("ungranted-neighbor");
        std::fs::write(&neighbor, b"leave this file alone").unwrap();
        let grant = hub
            .grant_compiler(0, compiler_helper_spec(), Duration::from_secs(15))
            .unwrap();
        let mut store = wasmtime::Store::new(
            &engine,
            State {
                hub: Arc::clone(&hub),
                budget: Arc::clone(&budget),
                runtime: runtime.handle(),
                table: ResourceTable::new(),
                initial: Some(grant),
                output_copies: 0,
                cancel_read: cancel_read.then(|| cancellation.clone()),
                limits: wasmtime::StoreLimitsBuilder::new()
                    .memory_size(8 * 1024 * 1024)
                    .build(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_fuel(500_000_000).unwrap();
        let result = runtime
            .run_within(
                crate::async_engine::timeout(Duration::from_secs(30), async {
                    let guest =
                        proof::CompilerProof::instantiate_async(&mut store, &component, &linker)
                            .await?;
                    let result = crate::async_engine::cancellable(
                        &cancellation.token(),
                        store.run_concurrent(async |accessor| guest.call_run(accessor).await),
                    )
                    .await???;
                    wasmtime::ensure!(result.is_ok(), "shared compiler policy rejected execution");
                    Ok::<(), wasmtime::Error>(())
                }),
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        let copied = store.data().output_copies;
        let retained_before_teardown = hub.snapshot().retained_transfer_capacity;
        // Even failed instantiation, cancellation, and lowering traps must destroy
        // the entire Store before canonical return-value credits can be reused.
        budget.begin_teardown().unwrap();
        hub.close_all(crate::operations::Terminal::Closed);
        let live_before_drop = budget.live_resources();
        let early_release = (live_before_drop > 0).then(|| budget.finish_teardown());
        drop(store);
        let retained_after_store_drop = hub.snapshot().retained_transfer_capacity;
        let joined = runtime
            .run_within(hub.join_process_jobs(), std::time::Duration::from_secs(60))
            .expect("test runtime budget");
        budget.finish_teardown().unwrap();
        assert_eq!(budget.live_resources(), 0);
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.live_resources, 0);
        assert_eq!(snapshot.pending_operations, 0);
        assert_eq!(snapshot.retained_process_jobs, 0);
        assert_eq!(snapshot.retained_transfer_capacity, 0);
        joined.unwrap();
        let result = result.unwrap();
        if expect_lowering_trap {
            let error = result.expect_err("allocator fault must trap");
            assert!(format!("{error:#}").contains("unreachable"), "{error:#}");
            assert_eq!(copied, 1, "trap must occur after first host output copy");
            assert!(live_before_drop > 0);
            assert_eq!(early_release, Some(Err(HubError::WrongRights)));
            assert!(retained_before_teardown >= 65536);
            assert!(
                retained_after_store_drop >= 65536,
                "Store drop cannot refund lowering credit"
            );
        } else if cancel_read {
            let error = result.expect_err("read-return checkpoint must cancel execution");
            assert!(error
                .downcast_ref::<crate::async_engine::Cancelled>()
                .is_some());
            assert!(cancellation.is_cancelled());
            assert_eq!(copied, 0, "read cancelled before output publication");
            assert!(live_before_drop > 0);
            assert_eq!(early_release, Some(Err(HubError::WrongRights)));
            assert!(retained_after_store_drop >= 65536);
        } else {
            result.unwrap();
            assert!(copied > 1);
            assert!(
                !artifact.exists(),
                "a Component miss has no output authority"
            );
            assert_eq!(std::fs::read(&neighbor).unwrap(), b"leave this file alone");
        }
    }
}

mod bindings {
    wasmtime::component::bindgen!({
        path: "src/guest_component_compiler.wit", world: "compiler-client",
        imports: { default: trappable },
        with: {
            "kernal:compiler-experiment/compilers.grant": super::Grant,
            "kernal:compiler-experiment/compilers.process": super::Process,
            "kernal:compiler-experiment/compilers.output": super::Output,
        },
    });
}
mod hash_bindings {
    wasmtime::component::bindgen!({
        path: "src/guest_component_hash.wit", world: "hash-client",
        imports: { default: trappable },
        with: { "kernal:hash-experiment/hashes.hasher": super::Hash },
    });
}
use bindings::kernal::compiler_experiment::compilers::{self, Error, Exit, OutputData, OutputTag};
use hash_bindings::kernal::hash_experiment::hashes;

struct State {
    hub: Arc<OperationHub>,
    budget: Arc<ComponentResourceBudget>,
    runtime: RuntimeHandle,
    table: ResourceTable,
    initial: Option<OpaqueToken>,
    limits: wasmtime::StoreLimits,
    #[cfg(test)]
    output_copies: usize,
    #[cfg(test)]
    cancel_read: Option<crate::async_engine::CancellationSource>,
}

enum AuthorityKind {
    Grant,
    Process,
    Hash,
}
struct Authority {
    hub: Arc<OperationHub>,
    token: OpaqueToken,
    kind: AuthorityKind,
    _permit: ComponentResourceLease,
}
impl Drop for Authority {
    fn drop(&mut self) {
        let _ = match self.kind {
            AuthorityKind::Grant => self.hub.abandon_compiler_resource(0, self.token, true),
            AuthorityKind::Process => self.hub.abandon_compiler_resource(0, self.token, false),
            AuthorityKind::Hash => self.hub.abandon_hash(0, self.token),
        };
    }
}
// bindgen re-exports its `with` types. This entire module stays private.
pub struct Grant(Authority);
pub struct Process(Authority);
pub struct Hash(Authority);
pub struct Output {
    operation: Option<Operation>,
    _permit: ComponentResourceLease,
}

enum OperationKind {
    Spawn,
    Read,
    Wait,
    Close,
}
struct Operation {
    hub: Arc<OperationHub>,
    token: OpaqueToken,
    kind: OperationKind,
}
impl Drop for Operation {
    fn drop(&mut self) {
        let _ = match self.kind {
            OperationKind::Spawn => self.hub.abandon_compiler_spawn(0, self.token),
            OperationKind::Read => self.hub.abandon_compiler_output_wire(0, self.token),
            OperationKind::Wait => self.hub.abandon_compiler_scalar(0, self.token, false),
            OperationKind::Close => self.hub.abandon_compiler_scalar(0, self.token, true),
        };
    }
}
impl Operation {
    async fn wait(&self) -> Result<u64, Error> {
        loop {
            let result = if matches!(self.kind, OperationKind::Wait) {
                self.hub
                    .collect_compiler_wait(0, self.token)
                    .map_err(error)?
            } else {
                self.hub.poll_wire(0, self.token.wire())
            };
            if result != 0 {
                return terminal(result);
            }
            self.ready().await?;
        }
    }
    async fn ready(&self) -> Result<(), Error> {
        self.hub
            .suspend_wire(0, self.token.wire())
            .map_err(error)?
            .notified()
            .await;
        Ok(())
    }
}
fn error(value: HubError) -> Error {
    match value {
        HubError::Closed | HubError::Stale => Error::Closed,
        _ => Error::Rejected,
    }
}
fn terminal(value: u64) -> Result<u64, Error> {
    match value as u8 {
        1 => Ok(value >> 8),
        2 => Err(Error::Cancelled),
        3 => Err(Error::TimedOut),
        6 => Err(Error::Closed),
        7 => Err(Error::Rejected),
        _ => Err(Error::Failed),
    }
}

struct Host;
impl HasData for Host {
    type Data<'a> = &'a mut State;
}
impl compilers::Host for State {
    fn granted(&mut self) -> wasmtime::Result<Result<Option<Resource<Grant>>, Error>> {
        let Some(token) = self.initial else {
            return Ok(Ok(None));
        };
        let permit = match self.budget.acquire(0) {
            Ok(permit) => permit,
            Err(e) => return Ok(Err(error(e))),
        };
        self.initial = None;
        let grant = Grant(Authority {
            hub: Arc::clone(&self.hub),
            token,
            kind: AuthorityKind::Grant,
            _permit: permit,
        });
        Ok(Ok(Some(self.table.push(grant)?)))
    }
    fn lookup_cache(
        &mut self,
        command: Resource<Grant>,
        key_0: u64,
        key_1: u64,
        key_2: u64,
        key_3: u64,
    ) -> wasmtime::Result<Result<compilers::CacheStatus, Error>> {
        let mut key = [0; 32];
        for (index, word) in [key_0, key_1, key_2, key_3].into_iter().enumerate() {
            key[index * 8..(index + 1) * 8].copy_from_slice(&word.to_le_bytes());
        }
        let grant = self.table.get(&command)?;
        Ok(self
            .hub
            .compiler_cache_status(0, grant.0.token, key)
            .map(|hit| {
                if hit {
                    compilers::CacheStatus::Hit
                } else {
                    compilers::CacheStatus::Miss
                }
            })
            .map_err(error))
    }
}
impl compilers::HostGrant for State {
    fn drop(&mut self, resource: Resource<Grant>) -> wasmtime::Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}
impl compilers::HostProcess for State {
    fn drop(&mut self, resource: Resource<Process>) -> wasmtime::Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}
impl compilers::HostOutput for State {
    fn drop(&mut self, resource: Resource<Output>) -> wasmtime::Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
    fn copy(&mut self, resource: Resource<Output>) -> wasmtime::Result<Result<OutputData, Error>> {
        let output = self.table.get_mut(&resource)?;
        let Some(operation) = &output.operation else {
            return Ok(Err(Error::Rejected));
        };
        let mut data = None;
        let result = operation.hub.collect_compiler_output_component(
            0,
            operation.token,
            &output._permit,
            |event| {
                use crate::{
                    ProcessOutputChunk as Chunk, ProcessOutputCompletion as Completion,
                    ProcessOutputEvent as Event,
                };
                let (tag, bytes): (_, &[u8]) = match event {
                    Some(Event::Chunk(Chunk::Stdout(bytes))) => (OutputTag::Stdout, bytes),
                    Some(Event::Chunk(Chunk::Stderr(bytes))) => (OutputTag::Stderr, bytes),
                    Some(Event::Completion(completion)) => (
                        match completion {
                            Completion::StdoutEof => OutputTag::StdoutEof,
                            Completion::StderrEof => OutputTag::StderrEof,
                            Completion::StdoutAbandoned => OutputTag::StdoutAbandoned,
                            Completion::StderrAbandoned => OutputTag::StderrAbandoned,
                            Completion::StdoutError(_) => OutputTag::StdoutError,
                            Completion::StderrError(_) => OutputTag::StderrError,
                        },
                        &[],
                    ),
                    None => (OutputTag::Exhausted, &[]),
                };
                // Unlike Core, this copy escapes the event lease. The Output's
                // separately admitted 64-KiB permit survives canonical lowering.
                data = Some(OutputData {
                    tag,
                    bytes: bytes.to_vec(),
                });
                #[cfg(test)]
                {
                    self.output_copies += 1;
                }
                0
            },
        );
        match result {
            Ok(0) => Ok(Err(Error::Failed)),
            Ok(status) => {
                drop(output.operation.take());
                Ok(terminal(status).and_then(|_| data.ok_or(Error::Failed)))
            }
            Err(e) => Ok(Err(error(e))),
        }
    }
}

impl compilers::HostWithStore for Host {
    async fn spawn<T: Send>(
        accessor: &Accessor<T, Self>,
        command: Resource<Grant>,
    ) -> wasmtime::Result<Result<Resource<Process>, Error>> {
        let start = accessor.with(|mut access| {
            let state = access.get();
            // Ownership has left the guest even if admission fails.
            let grant = state.table.delete(command)?;
            let token = grant.0.token;
            let permit = match state.budget.acquire(0) {
                Ok(permit) => permit,
                Err(e) => return Ok(Err(error(e))),
            };
            let operation = match state
                .hub
                .submit_compiler_spawn(state.runtime.clone(), 0, token)
            {
                Ok(token) => Operation {
                    hub: Arc::clone(&state.hub),
                    token,
                    kind: OperationKind::Spawn,
                },
                Err(e) => return Ok(Err(error(e))),
            };
            Ok::<_, wasmtime::Error>(Ok((operation, grant, permit)))
        })?;
        let (operation, _grant, permit) = match start {
            Ok(start) => start,
            Err(e) => return Ok(Err(e)),
        };
        let token = match operation.wait().await {
            Ok(token) => OpaqueToken::from_wire(token),
            Err(e) => return Ok(Err(e)),
        };
        let process = Process(Authority {
            hub: Arc::clone(&operation.hub),
            token,
            kind: AuthorityKind::Process,
            _permit: permit,
        });
        accessor.with(|mut access| Ok(Ok(access.get().table.push(process)?)))
    }

    async fn read<T: Send>(
        accessor: &Accessor<T, Self>,
        child: Resource<Process>,
    ) -> wasmtime::Result<Result<Resource<Output>, Error>> {
        let start = accessor.with(|mut access| {
            let state = access.get();
            let process = state.table.get(&child)?.0.token;
            // Both the table slot and lowering allowance precede native read.
            let permit = match state.budget.acquire(65536) {
                Ok(permit) => permit,
                Err(e) => return Ok(Err(error(e))),
            };
            let token = match state
                .hub
                .submit_compiler_output(state.runtime.clone(), 0, process)
            {
                Ok(token) => token,
                Err(e) => return Ok(Err(error(e))),
            };
            Ok::<_, wasmtime::Error>(Ok((
                Operation {
                    hub: Arc::clone(&state.hub),
                    token,
                    kind: OperationKind::Read,
                },
                permit,
            )))
        })?;
        let (operation, permit) = match start {
            Ok(start) => start,
            Err(e) => return Ok(Err(e)),
        };
        #[cfg(test)]
        if let Some(cancel) = accessor.with(|mut access| access.get().cancel_read.take()) {
            // Hold an admitted but unreturned read in the real Wasmtime host
            // future. Outer cancellation must clean it up during Store teardown.
            cancel.cancel();
            std::future::pending::<()>().await;
        }
        if let Err(e) = operation.ready().await {
            return Ok(Err(e));
        }
        let output = Output {
            operation: Some(operation),
            _permit: permit,
        };
        accessor.with(|mut access| Ok(Ok(access.get().table.push(output)?)))
    }

    async fn wait<T: Send>(
        accessor: &Accessor<T, Self>,
        child: Resource<Process>,
    ) -> wasmtime::Result<Result<Exit, Error>> {
        let start = accessor.with(|mut access| {
            let state = access.get();
            let process = state.table.get(&child)?.0.token;
            Ok::<_, wasmtime::Error>(
                state
                    .hub
                    .submit_compiler_wait(state.runtime.clone(), 0, process)
                    .map(|token| Operation {
                        hub: Arc::clone(&state.hub),
                        token,
                        kind: OperationKind::Wait,
                    })
                    .map_err(error),
            )
        })?;
        let operation = match start {
            Ok(start) => start,
            Err(e) => return Ok(Err(e)),
        };
        Ok(operation.wait().await.map(|payload| Exit {
            code: if payload & (1_u64 << 32) != 0 {
                Some(payload as u32 as i32)
            } else {
                None
            },
            success: payload & (1_u64 << 33) != 0,
        }))
    }

    async fn close<T: Send>(
        accessor: &Accessor<T, Self>,
        child: Resource<Process>,
    ) -> wasmtime::Result<Result<(), Error>> {
        let start = accessor.with(|mut access| {
            let state = access.get();
            let process = state.table.delete(child)?;
            Ok::<_, wasmtime::Error>(
                state
                    .hub
                    .submit_compiler_close(state.runtime.clone(), 0, process.0.token)
                    .map(|token| {
                        (
                            Operation {
                                hub: Arc::clone(&state.hub),
                                token,
                                kind: OperationKind::Close,
                            },
                            process,
                        )
                    })
                    .map_err(error),
            )
        })?;
        let (operation, _process) = match start {
            Ok(start) => start,
            Err(e) => return Ok(Err(e)),
        };
        Ok(operation.wait().await.map(|_| ()))
    }
}

impl hashes::Host for State {
    fn create(&mut self) -> wasmtime::Result<Result<Resource<Hash>, hashes::Error>> {
        let permit = match self.budget.acquire(0) {
            Ok(permit) => permit,
            Err(_) => return Ok(Err(hashes::Error::Rejected)),
        };
        let operation = match self.hub.submit_hash_create(0) {
            Ok(token) => token,
            Err(_) => return Ok(Err(hashes::Error::Rejected)),
        };
        let result = self.hub.poll_wire(0, operation.wire());
        if result as u8 != 1 {
            return Ok(Err(hashes::Error::Failed));
        }
        let hash = Hash(Authority {
            hub: Arc::clone(&self.hub),
            token: OpaqueToken::from_wire(result >> 8),
            kind: AuthorityKind::Hash,
            _permit: permit,
        });
        Ok(Ok(self.table.push(hash)?))
    }
    fn finish(&mut self, hash: Resource<Hash>) -> wasmtime::Result<Result<Vec<u8>, hashes::Error>> {
        let hash = self.table.delete(hash)?;
        let mut digest = [0; 32];
        Ok(self
            .hub
            .finish_hash_into(0, hash.0.token, &mut digest)
            .map(|()| digest.to_vec())
            .map_err(|_| hashes::Error::Rejected))
    }
}
impl hashes::HostHasher for State {
    fn drop(&mut self, hash: Resource<Hash>) -> wasmtime::Result<()> {
        self.table.delete(hash)?;
        Ok(())
    }
}

const MAX_HASH_UPDATE_BYTES: usize = 64 * 1024;
const HASH_INPUT_CHUNK_BYTES: usize = 256;

/// Receives scalar-only frames, so the guest cannot force canonical lifting of
/// a nested, attacker-sized list. `last` is the sole success signal: a dropped
/// reader, cancellation, malformed frame, or oversized input never commits.
struct HashInputConsumer {
    bytes: Vec<u8>,
    lease: Option<ComponentResourceLease>,
    result: Option<HashInputResultSender>,
}

type HashInputResultSender =
    crate::async_engine::OneshotSender<Result<(Vec<u8>, ComponentResourceLease), ()>>;

enum HashInputFrame {
    Continue,
    Complete,
    Rejected,
}

impl HashInputConsumer {
    fn reject(&mut self) {
        if let Some(result) = self.result.take() {
            let _ = result.send(Err(()));
        }
    }

    fn accept(&mut self) {
        if let Some(result) = self.result.take() {
            let lease = self.lease.take().expect("hash input lease is present");
            let _ = result.send(Ok((std::mem::take(&mut self.bytes), lease)));
        }
    }

    fn receive(&mut self, chunk: hashes::InputChunk) -> HashInputFrame {
        let used = usize::from(chunk.used);
        if used > HASH_INPUT_CHUNK_BYTES
            || (!chunk.last && used == 0)
            || self.bytes.len() + used > MAX_HASH_UPDATE_BYTES
        {
            self.reject();
            return HashInputFrame::Rejected;
        }
        let words = [
            chunk.word_0,
            chunk.word_1,
            chunk.word_2,
            chunk.word_3,
            chunk.word_4,
            chunk.word_5,
            chunk.word_6,
            chunk.word_7,
            chunk.word_8,
            chunk.word_9,
            chunk.word_10,
            chunk.word_11,
            chunk.word_12,
            chunk.word_13,
            chunk.word_14,
            chunk.word_15,
            chunk.word_16,
            chunk.word_17,
            chunk.word_18,
            chunk.word_19,
            chunk.word_20,
            chunk.word_21,
            chunk.word_22,
            chunk.word_23,
            chunk.word_24,
            chunk.word_25,
            chunk.word_26,
            chunk.word_27,
            chunk.word_28,
            chunk.word_29,
            chunk.word_30,
            chunk.word_31,
        ];
        let mut remaining = used;
        for word in words {
            let bytes = word.to_le_bytes();
            let count = remaining.min(bytes.len());
            self.bytes.extend_from_slice(&bytes[..count]);
            remaining -= count;
            if remaining == 0 {
                break;
            }
        }
        if chunk.last {
            return HashInputFrame::Complete;
        }
        HashInputFrame::Continue
    }
}

impl Drop for HashInputConsumer {
    fn drop(&mut self) {
        // Store teardown and a producer that vanishes without its explicit
        // terminator are both rejection, never an implicit EOF commit.
        self.reject();
    }
}

impl<T: Send + 'static> StreamConsumer<T> for HashInputConsumer {
    type Item = hashes::InputChunk;

    fn poll_consume(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        mut store: wasmtime::StoreContextMut<T>,
        mut source: Source<'_, Self::Item>,
        finish: bool,
    ) -> std::task::Poll<wasmtime::Result<StreamResult>> {
        if finish {
            self.reject();
            return std::task::Poll::Ready(Ok(StreamResult::Cancelled));
        }
        // A valid non-terminal frame carries at least one byte; a terminal
        // frame may carry zero for an empty update. Bound work before lifting.
        if source.remaining(&mut store) > MAX_HASH_UPDATE_BYTES - self.bytes.len() + 1 {
            self.reject();
            return std::task::Poll::Ready(Ok(StreamResult::Dropped));
        }
        while source.remaining(&mut store) != 0 {
            let mut chunk = Vec::with_capacity(1);
            source.read(&mut store, &mut chunk)?;
            let chunk = chunk.pop().expect("source reported a frame");
            match self.receive(chunk) {
                HashInputFrame::Continue => {}
                HashInputFrame::Complete => {
                    if source.remaining(&mut store) != 0 {
                        self.reject();
                    } else {
                        self.accept();
                    }
                    return std::task::Poll::Ready(Ok(StreamResult::Dropped));
                }
                HashInputFrame::Rejected => {
                    if source.remaining(&mut store) != 0 {
                        self.reject();
                    }
                    return std::task::Poll::Ready(Ok(StreamResult::Dropped));
                }
            }
        }
        std::task::Poll::Ready(Ok(StreamResult::Completed))
    }
}

impl hashes::HostHasherWithStore for wasmtime::component::HasSelf<State> {
    async fn update<T: Send>(
        accessor: &Accessor<T, Self>,
        hash: Resource<Hash>,
        mut chunks: wasmtime::component::StreamReader<hashes::InputChunk>,
    ) -> wasmtime::Result<Result<(), hashes::Error>> {
        let (sender, receiver) = crate::async_engine::oneshot_channel();
        let start: Result<_, hashes::Error> = accessor.with(|mut access| {
            let (token, hub) = match access.get().table.get(&hash) {
                Ok(hash) => (hash.0.token, Arc::clone(&access.get().hub)),
                Err(error) => {
                    chunks.close(access)?;
                    return Err(error.into());
                }
            };
            let lease = match access.get().budget.acquire(MAX_HASH_UPDATE_BYTES) {
                Ok(scratch) => scratch,
                Err(_) => {
                    chunks.close(access)?;
                    return Ok::<
                        Result<(Arc<OperationHub>, OpaqueToken), hashes::Error>,
                        wasmtime::Error,
                    >(Err(hashes::Error::Rejected));
                }
            };
            chunks.pipe(
                access,
                HashInputConsumer {
                    bytes: Vec::with_capacity(MAX_HASH_UPDATE_BYTES),
                    lease: Some(lease),
                    result: Some(sender),
                },
            )?;
            Ok::<Result<(Arc<OperationHub>, OpaqueToken), hashes::Error>, wasmtime::Error>(Ok((
                hub, token,
            )))
        })?;
        let (hub, token) = match start {
            Ok(start) => start,
            Err(error) => return Ok(Err(error)),
        };
        let (bytes, _scratch) = match receiver.await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(())) | Err(_) => return Ok(Err(hashes::Error::Rejected)),
        };
        let operation = match hub.submit_hash_update(0, token, &bytes) {
            Ok(token) => token,
            Err(_) => return Ok(Err(hashes::Error::Rejected)),
        };
        Ok(if hub.poll_wire(0, operation.wire()) == 1 {
            Ok(())
        } else {
            Err(hashes::Error::Failed)
        })
    }
}
