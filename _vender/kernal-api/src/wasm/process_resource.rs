//! One-shot native compiler grants and cancellation-safe direct-child cleanup.
//! ABI adapters must use this authority rather than owning native supervisors.

use super::*;
use crate::async_engine::{CancellationSource, CancellationToken, RuntimeHandle};
use crate::{ProcessSession, ProcessSessionOptions, SpawnSpec, StreamMode};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicU8;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

const GRANT_KIND: u8 = 10;
const PROCESS_KIND: u8 = 11;
const PROCESS_RIGHT: u8 = 1;
const MAX_PROCESS_JOBS: usize = 4;
pub(super) const MAX_PROCESS_OUTPUT_CHUNK: usize = 64 * 1024;
const MAX_PROCESS_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
// Pinned substrate: one shared queue slot, two scratch buffers, two pending
// sends, and two Windows blocking-read buffers. Payload allowance, not RSS;
// facade events, allocator overhead, OS pipes and child memory are separate.
const NATIVE_PROCESS_OUTPUT_ALLOWANCE: usize = 7 * MAX_PROCESS_OUTPUT_CHUNK;

#[path = "process_output.rs"]
mod output;

#[path = "process_scalar.rs"]
mod scalar;
pub(super) use scalar::CompilerScalarKind;

#[cfg(test)]
pub(super) struct SpawnCheckpoint {
    started: crate::async_engine::OneshotSender<Arc<ProcessSession>>,
    resume: crate::async_engine::OneshotReceiver<()>,
}

#[cfg(test)]
pub(super) struct CleanupCheckpoint {
    started: crate::async_engine::OneshotSender<()>,
    resume: crate::async_engine::OneshotReceiver<CleanupFault>,
}

#[cfg(test)]
pub(super) enum CleanupFault {
    None,
    Error,
    Panic,
}

pub(super) struct CompilerGrant {
    spec: Option<SpawnSpec>,
    deadline: Duration,
    // The private fixture supplies a precomputed cache key and outcome. A
    // production host would derive those from its metadata/content facts. The
    // guest may compare only its bounded, already-derived key against this
    // grant; it cannot enumerate or modify the cache.
    cache: Option<([u8; 32], bool)>,
}

pub(super) struct CompilerProcess {
    session: Option<Arc<ProcessSession>>,
    cleanup: Arc<CompilerCleanup>,
    cancel: CancellationSource,
    output_busy: bool,
    output_bytes: usize,
    output_limit: usize,
}

/// Observation-only completion retained independently of revocable authority.
pub(crate) struct CompilerCleanup {
    // 0 pending, 1 acknowledged success, 2 failure/producer disappearance.
    result: AtomicU8,
    finished: CancellationSource,
}

impl CompilerCleanup {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            result: AtomicU8::new(0),
            finished: CancellationSource::new(),
        })
    }

    fn finish(&self, result: Result<(), HubError>) {
        let terminal = if result.is_ok() { 1 } else { 2 };
        if self
            .result
            .compare_exchange(0, terminal, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // Reuse the facade's sticky broadcast rather than a one-waker Task
            // or a Notify permit that could strand concurrent/late observers.
            self.finished.cancel();
        }
    }

    pub(crate) async fn wait(&self) -> Result<(), HubError> {
        self.finished.token().cancelled().await;
        if self.result.load(Ordering::Acquire) == 1 {
            Ok(())
        } else {
            Err(HubError::Closed)
        }
    }
}

struct CompilerCleanupProducer(Arc<CompilerCleanup>);

impl Drop for CompilerCleanupProducer {
    fn drop(&mut self) {
        // Dropping a supervisor, including before its first poll or on panic,
        // is never evidence that its native resources were reclaimed.
        self.0.finish(Err(HubError::Closed));
    }
}

impl Drop for CompilerProcess {
    fn drop(&mut self) {
        // Revocation can run under the authority mutex. Only signal here;
        // the tracked job performs process I/O and reaping outside that lock.
        self.cancel.cancel();
    }
}

impl OperationHub {
    pub(crate) fn grant_compiler(
        &self,
        store: u64,
        spec: SpawnSpec,
        deadline: Duration,
    ) -> Result<OpaqueToken, HubError> {
        self.grant_compiler_with_cache(store, spec, deadline, None)
    }

    pub(crate) fn grant_compiler_with_cache(
        &self,
        store: u64,
        spec: SpawnSpec,
        deadline: Duration,
        cache: Option<([u8; 32], bool)>,
    ) -> Result<OpaqueToken, HubError> {
        // Host-owned, exact paths and environment; no ambient lookup or cwd.
        if !Path::new(&spec.program).is_absolute()
            || !spec
                .current_dir
                .as_ref()
                .is_some_and(|path| path.is_absolute())
            || !spec.clear_env
            || spec
                .lifetime_owner
                .is_some_and(|owner| owner != crate::platform::process::LifetimeOwner::Spawner)
            || deadline.is_zero()
            || deadline > Duration::from_secs(300)
        {
            return Err(HubError::Invalid);
        }
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let token = self.create_resource_value_locked(
            &mut state,
            store,
            GRANT_KIND,
            PROCESS_RIGHT,
            false,
            ResourceValue::CompilerGrant(CompilerGrant {
                spec: Some(
                    spec.stdin(StreamMode::Null)
                        .stdout(StreamMode::Piped)
                        .stderr(StreamMode::Piped)
                        .kill_when_owner_dies(true),
                ),
                deadline,
                cache,
            }),
        )?;
        state
            .resources
            .get_mut(&token)
            .ok_or(HubError::Closed)?
            .reserved = false;
        Ok(token)
    }

    /// Compare a guest-derived, fixed-size cache key against the exact
    /// host-authorized identity. This has no cache mutation or process effect.
    pub(crate) fn compiler_cache_status(
        &self,
        store: u64,
        grant: OpaqueToken,
        key: [u8; 32],
    ) -> Result<bool, HubError> {
        let state = self.state.lock().map_err(|_| HubError::Closed)?;
        let slot = state.resources.get(&grant).ok_or(HubError::Invalid)?;
        Self::validate_resource(slot, store, GRANT_KIND, PROCESS_RIGHT)?;
        let ResourceValue::CompilerGrant(CompilerGrant {
            cache: Some((expected, hit)),
            ..
        }) = &slot.value
        else {
            return Err(HubError::Invalid);
        };
        if &key != expected {
            return Err(HubError::Invalid);
        }
        Ok(*hit)
    }

    pub(crate) fn submit_compiler_spawn(
        self: &Arc<Self>,
        runtime: RuntimeHandle,
        store: u64,
        grant: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        if state.closed {
            return Err(HubError::Closed);
        }
        Self::poll_process_jobs(&mut state, &mut Context::from_waker(Waker::noop()));
        if state.process_job_failed {
            return Err(HubError::Closed);
        }
        if state.process_jobs.len() >= MAX_PROCESS_JOBS.min(self.maximum_resources) {
            return Err(HubError::Quota);
        }
        let slot = state.resources.get(&grant).ok_or(HubError::Invalid)?;
        Self::validate_resource(slot, store, GRANT_KIND, PROCESS_RIGHT)?;
        let ResourceValue::CompilerGrant(CompilerGrant { spec: Some(_), .. }) = &slot.value else {
            return Err(HubError::Closed);
        };
        if Self::transfer_capacity(&state).saturating_add(NATIVE_PROCESS_OUTPUT_ALLOWANCE)
            > self.blob_limits.maximum_sketch_bytes
        {
            return Err(HubError::Quota);
        }
        let (operation, _) =
            self.submit_locked(&mut state, store, Some(grant), GRANT_KIND, PROCESS_RIGHT)?;
        let cancel = CancellationSource::new();
        let cancellation = cancel.token();
        let cleanup = CompilerCleanup::new();
        let process = match self.create_resource_value_locked(
            &mut state,
            store,
            PROCESS_KIND,
            PROCESS_RIGHT,
            false,
            ResourceValue::CompilerProcess(CompilerProcess {
                session: None,
                cleanup: Arc::clone(&cleanup),
                cancel,
                output_busy: false,
                output_bytes: 0,
                output_limit: MAX_PROCESS_OUTPUT_BYTES,
            }),
        ) {
            Ok(token) => token,
            Err(error) => {
                state.operations.remove(&operation);
                return Err(error);
            }
        };
        state
            .operations
            .get_mut(&operation)
            .ok_or(HubError::Closed)?
            .created_resource = Some(process);
        state
            .operations
            .get_mut(&operation)
            .ok_or(HubError::Closed)?
            .is_compiler_spawn = true;
        state.reserved_native_process_output_bytes += NATIVE_PROCESS_OUTPUT_ALLOWANCE;
        Self::record_transfer_capacity(&mut state);
        let ResourceValue::CompilerGrant(grant) = &mut state
            .resources
            .get_mut(&grant)
            .ok_or(HubError::Closed)?
            .value
        else {
            return Err(HubError::WrongKind);
        };
        // Consume only after all quotas have been reserved. Never resurrect a
        // command whose launch may have begun, including native spawn errors.
        let spec = grant.spec.take().ok_or(HubError::Closed)?;
        let deadline = grant.deadline;
        let admitted = Instant::now();
        let hub = Arc::clone(self);
        // Construct outside the future so a task dropped before first poll
        // still resolves every observation capability to failure.
        let producer = CompilerCleanupProducer(cleanup);
        let job = runtime.launch(async move {
            let result = Arc::clone(&hub)
                .supervise_compiler(spec, operation, process, cancellation, admitted, deadline)
                .await;
            // No Drop-based refund: panic, dropped tasks, and uncertain native
            // cleanup must not make their allowance reusable by another job.
            let result = result.and_then(|()| hub.release_native_process_output());
            producer.0.finish(result);
            result
        });
        state.process_jobs.push(job);
        Ok(operation)
    }

    /// Observe exit without consuming output or transferring process authority.
    /// Dropping this observer does not cancel the compiler. Resource revocation,
    /// however, wakes it and must win over a concurrently available exit status.
    pub(crate) async fn wait_compiler(
        &self,
        store: u64,
        process: OpaqueToken,
    ) -> Result<crate::ProcessSessionExit, HubError> {
        let (session, cancellation) = {
            let state = self.state.lock().map_err(|_| HubError::Closed)?;
            let slot = state.resources.get(&process).ok_or(HubError::Closed)?;
            Self::validate_resource(slot, store, PROCESS_KIND, PROCESS_RIGHT)?;
            let ResourceValue::CompilerProcess(value) = &slot.value else {
                return Err(HubError::WrongKind);
            };
            (
                Arc::clone(value.session.as_ref().ok_or(HubError::Closed)?),
                value.cancel.token(),
            )
        };
        let result = crate::async_engine::cancellable(&cancellation, session.wait())
            .await
            .map_err(|_| HubError::Closed)?
            .map_err(|_| HubError::Closed)?;
        let state = self.state.lock().map_err(|_| HubError::Closed)?;
        let slot = state.resources.get(&process).ok_or(HubError::Closed)?;
        Self::validate_resource(slot, store, PROCESS_KIND, PROCESS_RIGHT)?;
        Ok(result)
    }

    pub(crate) fn close_compiler(
        &self,
        store: u64,
        process: OpaqueToken,
    ) -> Result<Arc<CompilerCleanup>, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let slot = state.resources.get(&process).ok_or(HubError::Closed)?;
        Self::validate_resource(slot, store, PROCESS_KIND, PROCESS_RIGHT)?;
        let ResourceValue::CompilerProcess(value) = &slot.value else {
            return Err(HubError::WrongKind);
        };
        let completion = Arc::clone(&value.cleanup);
        let notifications =
            Self::close_resource_with_terminal_locked(&mut state, process, Terminal::Closed)?;
        drop(state);
        for notify in notifications {
            notify.notify_one();
        }
        Ok(completion)
    }

    async fn supervise_compiler(
        self: Arc<Self>,
        spec: SpawnSpec,
        operation: OpaqueToken,
        process: OpaqueToken,
        cancellation: CancellationToken,
        admitted: Instant,
        deadline: Duration,
    ) -> Result<(), HubError> {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        if admitted.elapsed() >= deadline {
            let _ = self.terminal(
                operation,
                TerminalResult {
                    terminal: Terminal::TimedOut,
                    resource: None,
                },
            );
            return Ok(());
        }
        // Do not drop a spawning future: if revocation races native creation,
        // retain responsibility for any returned child and explicitly reap it.
        // An uninterruptible native spawn is not claimed to be contained here.
        #[cfg(test)]
        self.process_spawn_attempts.fetch_add(1, Ordering::SeqCst);
        let session = match spec
            .spawn_session(ProcessSessionOptions {
                max_queued_chunks: 1,
                max_chunk_bytes: MAX_PROCESS_OUTPUT_CHUNK,
                kill_on_drop: true,
                ..ProcessSessionOptions::default()
            })
            .await
        {
            Ok(session) => Arc::new(session),
            Err(_error) => {
                #[cfg(test)]
                eprintln!("compiler-session spawn rejected: {_error:?}");
                let _ = self.terminal(
                    operation,
                    TerminalResult {
                        terminal: Terminal::Rejected,
                        resource: None,
                    },
                );
                // A failed start has no reader-cleanup acknowledgement. Keep
                // the allowance charged and fail closed rather than assuming
                // every substrate error happened before native creation.
                return Err(HubError::Closed);
            }
        };
        #[cfg(test)]
        {
            let checkpoint = {
                let mut checkpoint = self.process_spawn_checkpoint.lock().unwrap();
                checkpoint.take()
            };
            if let Some(checkpoint) = checkpoint {
                let _ = checkpoint.started.send(Arc::clone(&session));
                let _ = checkpoint.resume.await;
            }
        }
        let published = if admitted.elapsed() >= deadline {
            let _ = self.terminal(
                operation,
                TerminalResult {
                    terminal: Terminal::TimedOut,
                    resource: None,
                },
            );
            Err(HubError::Closed)
        } else {
            self.publish_compiler(operation, process, Arc::clone(&session))
        };
        if published.is_ok() {
            let remaining = deadline.saturating_sub(admitted.elapsed());
            if crate::async_engine::timeout(remaining, cancellation.cancelled())
                .await
                .is_err()
            {
                let _ = self.revoke_external_resource(process, Terminal::TimedOut);
            }
        }
        // Revocation, failed publication, and deadline all use the same reaper.
        // This is direct-child cleanup, never a descendant-tree guarantee.
        let (output, lifecycle) =
            crate::async_engine::join(self.cleanup_compiler_output(&session), async {
                let killed = session.kill().await;
                let reaped = session.wait().await;
                killed.and(reaped.map(|_| ()))
            })
            .await;
        output.and(lifecycle).map_err(|_| HubError::Closed)
    }

    async fn cleanup_compiler_output(&self, session: &ProcessSession) -> std::io::Result<()> {
        #[cfg(test)]
        let fault = {
            let checkpoint = self.process_cleanup_checkpoint.lock().unwrap().take();
            if let Some(checkpoint) = checkpoint {
                let _ = checkpoint.started.send(());
                checkpoint.resume.await.unwrap_or(CleanupFault::None)
            } else {
                CleanupFault::None
            }
        };
        let result = session.shutdown_output().await;
        #[cfg(test)]
        match fault {
            CleanupFault::None => {}
            CleanupFault::Error => return Err(std::io::Error::other("injected cleanup failure")),
            CleanupFault::Panic => panic!("injected cleanup panic"),
        }
        result
    }

    fn release_native_process_output(&self) -> Result<(), HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        state.reserved_native_process_output_bytes = state
            .reserved_native_process_output_bytes
            .checked_sub(NATIVE_PROCESS_OUTPUT_ALLOWANCE)
            .ok_or(HubError::Closed)?;
        drop(state);
        let _ = self.drive_blob_writes();
        let _ = self.drive_blob_reads();
        Ok(())
    }

    fn publish_compiler(
        &self,
        operation: OpaqueToken,
        process: OpaqueToken,
        session: Arc<ProcessSession>,
    ) -> Result<(), HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        if state.closed
            || state
                .operations
                .get(&operation)
                .is_none_or(|op| op.terminal.is_some())
        {
            return Err(HubError::Closed);
        }
        let slot = state.resources.get_mut(&process).ok_or(HubError::Closed)?;
        let ResourceValue::CompilerProcess(value) = &mut slot.value else {
            return Err(HubError::WrongKind);
        };
        value.session = Some(session);
        let notify = Self::terminal_locked(
            &mut state,
            operation,
            TerminalResult {
                terminal: Terminal::Completed,
                resource: Some(process),
            },
        )?;
        drop(state);
        if let Some(notify) = notify {
            notify.notify_one();
        }
        Ok(())
    }

    pub(crate) fn abandon_compiler_spawn(
        &self,
        store: u64,
        operation: OpaqueToken,
    ) -> Result<(), HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let op = state.operations.get(&operation).ok_or(HubError::Invalid)?;
        if op.owner.store != store {
            return Err(HubError::Stale);
        }
        if !op.is_compiler_spawn {
            return Err(HubError::WrongKind);
        }
        let process = op.created_resource.ok_or(HubError::WrongKind)?;
        let op = state
            .operations
            .remove(&operation)
            .ok_or(HubError::Invalid)?;
        let notifications = if state.resources.contains_key(&process) {
            Self::close_resource_with_terminal_locked(&mut state, process, Terminal::Closed)?
        } else {
            Vec::new()
        };
        drop(state);
        op.notify.notify_one();
        for notify in notifications {
            notify.notify_one();
        }
        Ok(())
    }

    fn poll_process_jobs(state: &mut State, context: &mut Context<'_>) {
        let mut failed = false;
        for jobs in [&mut state.process_jobs, &mut state.process_io_jobs] {
            jobs.retain_mut(|job| match Pin::new(job).poll(context) {
                Poll::Pending => true,
                Poll::Ready(Ok(Ok(()))) => false,
                Poll::Ready(_) => {
                    failed = true;
                    false
                }
            });
        }
        state.process_job_failed |= failed;
    }

    pub(crate) async fn join_process_jobs(&self) -> Result<(), HubError> {
        // A Task has one join waker. Serialize observers without taking its
        // handle out of State; cancellation releases this permit, not the job.
        // Service lifetime (#433): this join ends when the hub's jobs do and
        // is cancellable by dropping it, so it carries no time budget.
        #[allow(deprecated)]
        let _join = self.process_join.acquire().await;
        std::future::poll_fn(|context| {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => return Poll::Ready(Err(HubError::Closed)),
            };
            if !state.closed {
                return Poll::Ready(Err(HubError::WrongRights));
            }
            Self::poll_process_jobs(&mut state, context);
            if !state.process_jobs.is_empty() || !state.process_io_jobs.is_empty() {
                return Poll::Pending;
            }
            Poll::Ready(if state.process_job_failed {
                Err(HubError::Closed)
            } else {
                Ok(())
            })
        })
        .await
    }
}

#[cfg(test)]
#[path = "process_resource_tests.rs"]
mod tests;
