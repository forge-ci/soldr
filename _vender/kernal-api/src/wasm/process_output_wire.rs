//! Move native output into a bounded, collection-only operation result.

use super::*;

impl OperationHub {
    #[cfg(feature = "wasm-component-compiler-experiment")]
    pub(crate) fn collect_compiler_output_component(
        &self,
        store: u64,
        token: OpaqueToken,
        allowance: &crate::operations::ComponentResourceLease,
        copy: impl FnOnce(Option<&ProcessOutputEvent>) -> u64,
    ) -> Result<u64, HubError> {
        if !allowance.covers(self, MAX_PROCESS_OUTPUT_CHUNK) {
            return Err(HubError::Quota);
        }
        self.collect_compiler_output_wire(store, token, MAX_PROCESS_OUTPUT_CHUNK, copy)
    }
    pub(crate) fn submit_compiler_output(
        self: &Arc<Self>,
        runtime: RuntimeHandle,
        store: u64,
        process: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        if state.closed {
            return Err(HubError::Closed);
        }
        Self::poll_process_jobs(&mut state, &mut Context::from_waker(Waker::noop()));
        if state.process_job_failed {
            return Err(HubError::Closed);
        }
        // Operation collection cannot recycle a still-running producer slot.
        if state.process_io_jobs.len() >= self.maximum_operations {
            return Err(HubError::Quota);
        }
        let mut read = self.begin_compiler_output_locked(&mut state, store, process)?;
        let operation = read.operation;
        read.disposition = OutputLeaseDisposition::WirePending;
        state
            .operations
            .get_mut(&operation)
            .ok_or(HubError::Closed)?
            .is_compiler_read = true;
        let hub = Arc::clone(self);
        let job = runtime.launch(async move {
            match read.receive_event().await {
                Ok(event) => {
                    // publish consumes the lease only after transferring both
                    // payload and credit under the authority mutex.
                    let _ = (CompilerOutput { event, lease: read }).publish();
                }
                Err(error) => {
                    let terminal = match error {
                        CompilerOutputError::Terminal(terminal) => terminal,
                        CompilerOutputError::Admission(HubError::Closed) => Terminal::Closed,
                        CompilerOutputError::Admission(_) => Terminal::Rejected,
                    };
                    let _ = hub.terminal(
                        operation,
                        TerminalResult {
                            terminal,
                            resource: None,
                        },
                    );
                    drop(read);
                }
            }
            Ok(())
        });
        state.process_io_jobs.push(job);
        Ok(operation)
    }

    /// Destination validation must precede this call. The bounded callback
    /// runs under the authority mutex and must not reenter. Core writes
    /// directly to shared cells; only collect_compiler_output_component may
    /// retain a copy, backed by its separately owned lowering allowance.
    pub(crate) fn collect_compiler_output_wire(
        &self,
        store: u64,
        token: OpaqueToken,
        capacity: usize,
        copy: impl FnOnce(Option<&ProcessOutputEvent>) -> u64,
    ) -> Result<u64, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let operation = state.operations.get(&token).ok_or(HubError::Invalid)?;
        if operation.owner.store != store {
            return Err(HubError::Stale);
        }
        if !operation.is_compiler_read {
            return Err(HubError::WrongKind);
        }
        if capacity < MAX_PROCESS_OUTPUT_CHUNK {
            return Err(HubError::Quota);
        }
        let Some(result) = operation.terminal else {
            return Ok(0);
        };
        let process = operation.resource.ok_or(HubError::WrongKind)?;
        let payload = if result.terminal == Terminal::Completed {
            let resource = state.resources.get(&process).ok_or(HubError::Closed)?;
            Self::validate_resource(resource, store, PROCESS_KIND, PROCESS_RIGHT)?;
            let event = operation.compiler_output.as_ref().ok_or(HubError::Closed)?;
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| copy(event.as_ref()))) {
                Ok(payload) => payload,
                Err(panic) => {
                    drop(state);
                    let _ = self.abandon_compiler_output_wire(store, token);
                    std::panic::resume_unwind(panic);
                }
            }
        } else {
            0
        };
        let mut operation = state.operations.remove(&token).ok_or(HubError::Closed)?;
        // Free bytes before the locked transition makes their charge reusable.
        drop(operation.compiler_output.take());
        if let Some(ResourceSlot {
            value: ResourceValue::CompilerProcess(value),
            ..
        }) = state.resources.get_mut(&process)
        {
            value.output_busy = false;
        }
        state.resumes = state.resumes.saturating_add(1);
        drop(state);
        drop(operation);
        self.drive_blob_writes()?;
        self.drive_blob_reads()?;
        Ok((payload << 8) | u64::from(status(result.terminal)))
    }

    pub(crate) fn abandon_compiler_output_wire(
        &self,
        store: u64,
        token: OpaqueToken,
    ) -> Result<(), HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let operation = state.operations.get(&token).ok_or(HubError::Invalid)?;
        if operation.owner.store != store {
            return Err(HubError::Stale);
        }
        if !operation.is_compiler_read {
            return Err(HubError::WrongKind);
        }
        let process = operation.resource.ok_or(HubError::WrongKind)?;
        let mut operation = state.operations.remove(&token).ok_or(HubError::Invalid)?;
        drop(operation.compiler_output.take());
        let notifications = if state.resources.contains_key(&process) {
            Self::close_resource_with_terminal_locked(&mut state, process, Terminal::Closed)?
        } else {
            Vec::new()
        };
        drop(state);
        if let Some(cancel) = operation.producer_cancel.take() {
            cancel.cancel();
        }
        operation.notify.notify_one();
        for notify in notifications {
            notify.notify_one();
        }
        self.drive_blob_writes()?;
        self.drive_blob_reads()
    }

    pub(crate) fn cancel_compiler_output_wire(
        &self,
        store: u64,
        token: OpaqueToken,
    ) -> Result<bool, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let Some(operation) = state.operations.get(&token) else {
            return Ok(false);
        };
        if !operation.is_compiler_read {
            return Ok(false);
        }
        if operation.owner.store != store {
            return Err(HubError::Stale);
        }
        let process = operation.resource.ok_or(HubError::WrongKind)?;
        let notifications = if state.resources.contains_key(&process) {
            Self::close_resource_with_terminal_locked(&mut state, process, Terminal::Cancelled)?
        } else {
            Vec::new()
        };
        drop(state);
        for notify in notifications {
            notify.notify_one();
        }
        self.drive_blob_writes()?;
        self.drive_blob_reads()?;
        Ok(true)
    }
}

impl CompilerOutput {
    fn publish(mut self) -> Result<(), HubError> {
        let mut state = self.lease.hub.state.lock().map_err(|_| HubError::Closed)?;
        let operation = state
            .operations
            .get(&self.lease.operation)
            .ok_or(HubError::Closed)?;
        if state.closed || !operation.is_compiler_read || operation.terminal.is_some() {
            return Err(HubError::Closed);
        }
        let resource = state
            .resources
            .get(&self.lease.process)
            .ok_or(HubError::Closed)?;
        OperationHub::validate_resource(
            resource,
            operation.owner.store,
            PROCESS_KIND,
            PROCESS_RIGHT,
        )?;
        let reservation = state
            .reserved_process_output_bytes
            .checked_sub(MAX_PROCESS_OUTPUT_CHUNK)
            .ok_or(HubError::Closed)?;
        state
            .operations
            .get_mut(&self.lease.operation)
            .ok_or(HubError::Closed)?
            .compiler_output = Some(self.event.take());
        state.reserved_process_output_bytes = reservation;
        self.lease.disposition = OutputLeaseDisposition::Transferred;
        let notify = OperationHub::terminal_locked(
            &mut state,
            self.lease.operation,
            TerminalResult {
                terminal: Terminal::Completed,
                resource: None,
            },
        )?;
        drop(state);
        if let Some(notify) = notify {
            notify.notify_one();
        }
        Ok(())
    }
}
