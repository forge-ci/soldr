//! Scalar compiler observations share the supplied runtime and tracked jobs.
use super::*;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum CompilerScalarKind {
    Wait,
    Close,
}

impl OperationHub {
    pub(crate) fn submit_compiler_wait(
        self: &Arc<Self>,
        runtime: RuntimeHandle,
        store: u64,
        process: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        self.admit_compiler_scalar(&mut state)?;
        let (operation, _) = self.submit_locked(
            &mut state,
            store,
            Some(process),
            PROCESS_KIND,
            PROCESS_RIGHT,
        )?;
        let source = CancellationSource::new();
        let cancellation = source.token();
        let slot = state
            .operations
            .get_mut(&operation)
            .ok_or(HubError::Closed)?;
        slot.compiler_scalar = Some(CompilerScalarKind::Wait);
        slot.producer_cancel = Some(source);
        let hub = Arc::clone(self);
        let producer = ScalarProducer {
            hub: Arc::clone(self),
            operation,
        };
        state.process_io_jobs.push(runtime.launch(async move {
            if let Ok(result) =
                crate::async_engine::cancellable(&cancellation, hub.wait_compiler(store, process))
                    .await
            {
                let mut state = hub.state.lock().map_err(|_| HubError::Closed)?;
                if let Some(slot) = state.operations.get_mut(&operation) {
                    if slot.terminal.is_none() {
                        slot.compiler_exit = result.as_ref().ok().copied();
                        let terminal = if result.is_ok() {
                            Terminal::Completed
                        } else {
                            Terminal::Closed
                        };
                        let notify = Self::terminal_locked(
                            &mut state,
                            operation,
                            TerminalResult {
                                terminal,
                                resource: None,
                            },
                        )?;
                        drop(state);
                        if let Some(notify) = notify {
                            notify.notify_one();
                        }
                    }
                }
            }
            drop(producer);
            Ok(())
        }));
        Ok(operation)
    }

    pub(crate) fn submit_compiler_close(
        self: &Arc<Self>,
        runtime: RuntimeHandle,
        store: u64,
        process: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        self.admit_compiler_scalar(&mut state)?;
        let slot = state.resources.get(&process).ok_or(HubError::Closed)?;
        Self::validate_resource(slot, store, PROCESS_KIND, PROCESS_RIGHT)?;
        let ResourceValue::CompilerProcess(value) = &slot.value else {
            return Err(HubError::WrongKind);
        };
        let cleanup = Arc::clone(&value.cleanup);
        // No borrowed process: revocation must not cancel its own close observer.
        let (operation, _) =
            self.submit_locked(&mut state, store, None, PROCESS_KIND, PROCESS_RIGHT)?;
        let source = CancellationSource::new();
        let cancellation = source.token();
        let slot = state
            .operations
            .get_mut(&operation)
            .ok_or(HubError::Closed)?;
        slot.compiler_scalar = Some(CompilerScalarKind::Close);
        slot.producer_cancel = Some(source);
        let notifications =
            Self::close_resource_with_terminal_locked(&mut state, process, Terminal::Closed)?;
        let hub = Arc::clone(self);
        let producer = ScalarProducer {
            hub: Arc::clone(self),
            operation,
        };
        state.process_io_jobs.push(runtime.launch(async move {
            if let Ok(result) =
                crate::async_engine::cancellable(&cancellation, cleanup.wait()).await
            {
                let terminal = if result.is_ok() {
                    Terminal::Completed
                } else {
                    Terminal::Closed
                };
                let _ = hub.terminal(
                    operation,
                    TerminalResult {
                        terminal,
                        resource: None,
                    },
                );
            }
            drop(producer);
            Ok(())
        }));
        drop(state);
        for notify in notifications {
            notify.notify_one();
        }
        Ok(operation)
    }

    fn admit_compiler_scalar(&self, state: &mut State) -> Result<(), HubError> {
        if state.closed {
            return Err(HubError::Closed);
        }
        Self::poll_process_jobs(state, &mut Context::from_waker(Waker::noop()));
        if state.process_job_failed {
            return Err(HubError::Closed);
        }
        if state.process_io_jobs.len() >= self.maximum_operations {
            return Err(HubError::Quota);
        }
        Ok(())
    }

    pub(crate) fn collect_compiler_wait(
        &self,
        store: u64,
        token: OpaqueToken,
    ) -> Result<u64, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let slot = state.operations.get(&token).ok_or(HubError::Invalid)?;
        if slot.owner.store != store {
            return Err(HubError::Stale);
        }
        if slot.compiler_scalar != Some(CompilerScalarKind::Wait) {
            return Err(HubError::WrongKind);
        }
        let Some(result) = slot.terminal else {
            return Ok(0);
        };
        let payload = if result.terminal == Terminal::Completed {
            let resource = state
                .resources
                .get(&slot.resource.ok_or(HubError::WrongKind)?)
                .ok_or(HubError::Closed)?;
            Self::validate_resource(resource, store, PROCESS_KIND, PROCESS_RIGHT)?;
            let exit = slot.compiler_exit.ok_or(HubError::Closed)?;
            u64::from(exit.exit_code().unwrap_or(0) as u32)
                | (u64::from(exit.exit_code().is_some()) << 32)
                | (u64::from(exit.is_success()) << 33)
        } else {
            0
        };
        state.operations.remove(&token);
        state.resumes = state.resumes.saturating_add(1);
        Ok((payload << 8) | u64::from(status(result.terminal)))
    }

    pub(crate) fn abandon_compiler_scalar(
        &self,
        store: u64,
        token: OpaqueToken,
        close: bool,
    ) -> Result<(), HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let slot = state.operations.get(&token).ok_or(HubError::Invalid)?;
        if slot.owner.store != store {
            return Err(HubError::Stale);
        }
        let kind = if close {
            CompilerScalarKind::Close
        } else {
            CompilerScalarKind::Wait
        };
        if slot.compiler_scalar != Some(kind) {
            return Err(HubError::WrongKind);
        }
        let slot = state.operations.remove(&token).ok_or(HubError::Invalid)?;
        drop(state);
        if let Some(cancel) = slot.producer_cancel {
            cancel.cancel();
        }
        slot.notify.notify_one();
        Ok(())
    }

    pub(crate) fn abandon_compiler_resource(
        &self,
        store: u64,
        token: OpaqueToken,
        grant: bool,
    ) -> Result<(), HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let slot = state.resources.get(&token).ok_or(HubError::Closed)?;
        Self::validate_resource(
            slot,
            store,
            if grant { GRANT_KIND } else { PROCESS_KIND },
            PROCESS_RIGHT,
        )?;
        let notifications =
            Self::close_resource_with_terminal_locked(&mut state, token, Terminal::Closed)?;
        drop(state);
        for notify in notifications {
            notify.notify_one();
        }
        Ok(())
    }
}

struct ScalarProducer {
    hub: Arc<OperationHub>,
    operation: OpaqueToken,
}
impl Drop for ScalarProducer {
    fn drop(&mut self) {
        let Ok(mut state) = self.hub.state.lock() else {
            return;
        };
        if state
            .operations
            .get(&self.operation)
            .is_none_or(|slot| slot.terminal.is_some())
        {
            return;
        }
        // Producer disappearance closes only pending work. A successful exit
        // remains revocable by authority changes, not by this guard's Drop.
        let notify = OperationHub::terminal_locked(
            &mut state,
            self.operation,
            TerminalResult {
                terminal: Terminal::Closed,
                resource: None,
            },
        );
        drop(state);
        if let Ok(Some(notify)) = notify {
            notify.notify_one();
        }
    }
}
