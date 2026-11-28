//! A single pending/uncollected output event with an owned transfer reservation.
//! This covers the facade event, not the native pumps or Windows blocking I/O.

use super::*;
use crate::ProcessOutputEvent;

#[path = "process_output_wire.rs"]
mod wire;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompilerOutputError {
    Admission(HubError),
    Terminal(Terminal),
}

impl From<HubError> for CompilerOutputError {
    fn from(error: HubError) -> Self {
        Self::Admission(error)
    }
}

pub(crate) struct CompilerOutputRead {
    hub: Arc<OperationHub>,
    process: OpaqueToken,
    operation: OpaqueToken,
    session: Arc<ProcessSession>,
    cancellation: CancellationToken,
    completed: bool,
    disposition: OutputLeaseDisposition,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum OutputLeaseDisposition {
    Native,
    WirePending,
    Transferred,
}

pub(crate) struct CompilerOutput {
    // Drop the payload before the lease releases its capacity and read slot.
    event: Option<ProcessOutputEvent>,
    lease: CompilerOutputRead,
}

impl CompilerOutput {
    /// Validate and deliver under the same authority lock. The callback must
    /// be bounded, synchronous, and must not reenter the hub. It must not move
    /// a copied payload outside this reservation's lifetime.
    pub(crate) fn collect<T>(
        mut self,
        consume: impl FnOnce(Option<&ProcessOutputEvent>) -> T,
    ) -> Result<T, CompilerOutputError> {
        let mut state = self.lease.hub.state.lock().map_err(|_| HubError::Closed)?;
        let operation = state
            .operations
            .get(&self.lease.operation)
            .ok_or(HubError::Closed)?;
        if let Some(result) = operation.terminal {
            return Err(CompilerOutputError::Terminal(result.terminal));
        }
        if !state.resources.contains_key(&self.lease.process) {
            return Err(HubError::Closed.into());
        }
        // Release the mutex before propagating a callback panic, so lease
        // Drop can revoke the process and reclaim credits without poisoning.
        let consumed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            consume(self.event.as_ref())
        }));
        let notify = if consumed.is_ok() {
            self.lease.completed = true;
            OperationHub::terminal_locked(
                &mut state,
                self.lease.operation,
                TerminalResult {
                    terminal: Terminal::Completed,
                    resource: None,
                },
            )?
        } else {
            None
        };
        drop(state);
        if let Some(notify) = notify {
            notify.notify_one();
        }
        match consumed {
            Ok(value) => Ok(value),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
}

impl OperationHub {
    pub(crate) fn begin_compiler_output(
        self: &Arc<Self>,
        store: u64,
        process: OpaqueToken,
    ) -> Result<CompilerOutputRead, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        self.begin_compiler_output_locked(&mut state, store, process)
    }

    fn begin_compiler_output_locked(
        self: &Arc<Self>,
        state: &mut State,
        store: u64,
        process: OpaqueToken,
    ) -> Result<CompilerOutputRead, HubError> {
        let slot = state.resources.get(&process).ok_or(HubError::Closed)?;
        Self::validate_resource(slot, store, PROCESS_KIND, PROCESS_RIGHT)?;
        let ResourceValue::CompilerProcess(value) = &slot.value else {
            return Err(HubError::WrongKind);
        };
        if value.output_busy {
            return Err(HubError::Quota);
        }
        let session = Arc::clone(value.session.as_ref().ok_or(HubError::Closed)?);
        if Self::transfer_capacity(state).saturating_add(MAX_PROCESS_OUTPUT_CHUNK)
            > self.blob_limits.maximum_sketch_bytes
        {
            return Err(HubError::Quota);
        }
        let (operation, _) =
            self.submit_locked(state, store, Some(process), PROCESS_KIND, PROCESS_RIGHT)?;
        let cancellation = CancellationSource::new();
        let token = cancellation.token();
        state
            .operations
            .get_mut(&operation)
            .ok_or(HubError::Closed)?
            .producer_cancel = Some(cancellation);
        let ResourceValue::CompilerProcess(value) = &mut state
            .resources
            .get_mut(&process)
            .ok_or(HubError::Closed)?
            .value
        else {
            return Err(HubError::WrongKind);
        };
        value.output_busy = true;
        state.reserved_process_output_bytes += MAX_PROCESS_OUTPUT_CHUNK;
        Self::record_transfer_capacity(state);
        Ok(CompilerOutputRead {
            hub: Arc::clone(self),
            process,
            operation,
            session,
            cancellation: token,
            completed: false,
            disposition: OutputLeaseDisposition::Native,
        })
    }
}

impl CompilerOutputRead {
    pub(crate) async fn receive(self) -> Result<CompilerOutput, CompilerOutputError> {
        let event = self.receive_event().await?;
        Ok(CompilerOutput { event, lease: self })
    }

    async fn receive_event(&self) -> Result<Option<ProcessOutputEvent>, CompilerOutputError> {
        let event =
            crate::async_engine::cancellable(&self.cancellation, self.session.next_output())
                .await
                .map_err(|_| {
                    self.hub
                        .state
                        .lock()
                        .ok()
                        .and_then(|state| {
                            state
                                .operations
                                .get(&self.operation)
                                .and_then(|op| op.terminal)
                        })
                        .map_or(CompilerOutputError::Admission(HubError::Closed), |result| {
                            CompilerOutputError::Terminal(result.terminal)
                        })
                })?;
        {
            let mut state = self.hub.state.lock().map_err(|_| HubError::Closed)?;
            let operation = state
                .operations
                .get(&self.operation)
                .ok_or(HubError::Closed)?;
            if let Some(result) = operation.terminal {
                return Err(CompilerOutputError::Terminal(result.terminal));
            }
            let slot = state
                .resources
                .get_mut(&self.process)
                .ok_or(HubError::Closed)?;
            let ResourceValue::CompilerProcess(value) = &mut slot.value else {
                return Err(HubError::WrongKind.into());
            };
            if let Some(ProcessOutputEvent::Chunk(chunk)) = &event {
                let bytes = match chunk {
                    crate::ProcessOutputChunk::Stdout(bytes)
                    | crate::ProcessOutputChunk::Stderr(bytes) => bytes,
                };
                if bytes.capacity() > MAX_PROCESS_OUTPUT_CHUNK {
                    return Err(HubError::Quota.into());
                }
                let total = value
                    .output_bytes
                    .checked_add(bytes.len())
                    .ok_or(HubError::Quota)?;
                if total > value.output_limit {
                    return Err(HubError::Quota.into());
                }
                value.output_bytes = total;
            }
        }
        Ok(event)
    }
}

impl Drop for CompilerOutputRead {
    fn drop(&mut self) {
        if self.disposition == OutputLeaseDisposition::Transferred {
            return;
        }
        let Ok(mut state) = self.hub.state.lock() else {
            return;
        };
        let operation = if self.disposition == OutputLeaseDisposition::Native {
            state.operations.remove(&self.operation)
        } else {
            // A failed or cancelled wire producer leaves a typed terminal for
            // its guest. Panic/producer disappearance must also wake the waiter.
            if let Ok(Some(notify)) = OperationHub::terminal_locked(
                &mut state,
                self.operation,
                TerminalResult {
                    terminal: Terminal::Closed,
                    resource: None,
                },
            ) {
                notify.notify_one();
            }
            None
        };
        if let Some(ResourceSlot {
            value: ResourceValue::CompilerProcess(value),
            ..
        }) = state.resources.get_mut(&self.process)
        {
            value.output_busy = false;
        }
        let notifications = if !self.completed && state.resources.contains_key(&self.process) {
            // Abandonment may have raced pipe consumption. Never replay bytes
            // or allow a subsequent reader to continue from uncertain state.
            OperationHub::close_resource_with_terminal_locked(
                &mut state,
                self.process,
                Terminal::Closed,
            )
            .unwrap_or_default()
        } else {
            Vec::new()
        };
        state.reserved_process_output_bytes = state
            .reserved_process_output_bytes
            .saturating_sub(MAX_PROCESS_OUTPUT_CHUNK);
        drop(state);
        if let Some(operation) = operation {
            operation.notify.notify_one();
        }
        for notify in notifications {
            notify.notify_one();
        }
        let _ = self.hub.drive_blob_writes();
        let _ = self.hub.drive_blob_reads();
    }
}
