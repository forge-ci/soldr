//! Native jobs belonging to one Wasm logical root's existing resource hub.
use super::*;
use crate::operations::{OP_WEBVIEW_CAPTURE, OP_WEBVIEW_CLOSE, OP_WEBVIEW_LOAD, OP_WEBVIEW_OPEN};

const MAX_JOBS: usize = 128;

pub(crate) struct SketchWebviews {
    service: Arc<WebviewService>,
    jobs: Mutex<Vec<async_engine::Task<()>>>,
    closing: AtomicBool,
    stop: async_engine::CancellationSource,
    failed: Arc<AtomicBool>,
}

struct PanicFlag(Arc<AtomicBool>);
impl Drop for PanicFlag {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.store(true, Ordering::Release);
        }
    }
}

struct JobGuard {
    service: Arc<WebviewService>,
    operation: OpaqueToken,
    revoke: Option<OpaqueToken>,
    finished: bool,
}
impl Drop for JobGuard {
    fn drop(&mut self) {
        if !self.finished {
            let terminal = if std::thread::panicking() {
                Terminal::Trapped
            } else {
                Terminal::Cancelled
            };
            self.service
                .hub
                .finish_external_operation(self.operation, terminal);
            if let Some(view) = self.revoke {
                self.service.revoke_with_terminal(view, terminal);
            }
        }
    }
}

impl SketchWebviews {
    pub(crate) fn new(
        client: ExternalWebviewClient,
        hub: Arc<OperationHub>,
        runtime: &RuntimeHandle,
    ) -> Result<Arc<Self>, WebviewError> {
        if !client.service.runtime.same_runtime_for_wasm(runtime) {
            return Err(WebviewError::HostFailure(
                "webview belongs to another runtime".into(),
            ));
        }
        Ok(Arc::new(Self {
            service: Arc::new(WebviewService {
                #[cfg(feature = "tauri-webview-test-support")]
                trace: Arc::clone(&client.service.trace),
                #[cfg(feature = "tauri-webview-test-support")]
                app_id: client.service.app_id.clone(),
                runtime: runtime.clone(),
                backend: client.service.backend.clone(),
                hub,
                native: Mutex::new(BTreeMap::new()),
                closing: Mutex::new(BTreeSet::new()),
            }),
            jobs: Mutex::new(Vec::new()),
            closing: AtomicBool::new(false),
            stop: async_engine::CancellationSource::new(),
            failed: Arc::new(AtomicBool::new(false)),
        }))
    }

    fn launch(
        &self,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<(), HubError> {
        let mut jobs = self.admit_job()?;
        self.spawn_job(&mut jobs, future);
        Ok(())
    }

    #[cfg(feature = "tauri-webview-test-support")]
    pub(crate) fn trace_abi(&self, phase: &'static str, opcode: Option<u32>) {
        self.service
            .trace
            .record(Instant::now(), phase, opcode, None);
    }

    fn admit_job(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Vec<async_engine::Task<()>>>, HubError> {
        let mut jobs = self.jobs.lock().map_err(|_| HubError::Closed)?;
        jobs.retain(|job| !job.is_finished());
        if self.closing.load(Ordering::Acquire) {
            return Err(HubError::Closed);
        }
        if jobs.len() >= MAX_JOBS {
            return Err(HubError::Quota);
        }
        Ok(jobs)
    }

    fn spawn_job(
        &self,
        jobs: &mut Vec<async_engine::Task<()>>,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let flag = PanicFlag(Arc::clone(&self.failed));
        jobs.push(self.service.runtime.launch(async move {
            let _flag = flag;
            future.await;
        }));
    }

    pub(crate) fn submit(
        self: &Arc<Self>,
        store: u64,
        kind: u32,
        arg0: u64,
        arg1: u64,
    ) -> Result<u64, HubError> {
        if arg1 != 0 {
            return Err(HubError::Invalid);
        }
        // Reject scheduler pressure before reserving or revoking any view.
        let mut jobs = self.admit_job()?;
        let token = OpaqueToken::from_wire(arg0);
        let (view, operation, url) = match kind {
            OP_WEBVIEW_OPEN => {
                let (view, operation, url) =
                    self.service.hub.begin_granted_webview_open(store, token)?;
                (view, operation, Some(url))
            }
            OP_WEBVIEW_LOAD => (
                token,
                self.service.hub.begin_external_webview_wait(store, token)?,
                None,
            ),
            OP_WEBVIEW_CAPTURE => (
                token,
                self.service
                    .hub
                    .begin_external_webview_capture(store, token)?,
                None,
            ),
            OP_WEBVIEW_CLOSE => (
                token,
                self.service
                    .hub
                    .begin_external_webview_close(store, token)?,
                None,
            ),
            _ => return Err(HubError::Invalid),
        };
        let cancellation = self
            .service
            .hub
            .bind_producer_cancellation(store, operation)?;
        let guard = JobGuard {
            service: Arc::clone(&self.service),
            operation,
            revoke: (kind != OP_WEBVIEW_CAPTURE).then_some(view),
            finished: false,
        };
        let this = Arc::clone(self);
        let future = async move {
            let mut guard = guard;
            let result = this
                .execute(store, kind, view, operation, url, cancellation)
                .await;
            if let Err(error) = result {
                let terminal = match error {
                    WebviewError::Cancelled => Terminal::Cancelled,
                    WebviewError::TimedOut => Terminal::TimedOut,
                    WebviewError::WindowClosed => Terminal::Closed,
                    _ => Terminal::Rejected,
                };
                this.service
                    .hub
                    .finish_external_operation(operation, terminal);
                if let Some(view) = guard.revoke {
                    this.service.revoke_with_terminal(view, terminal);
                }
            }
            guard.finished = true;
        };
        self.spawn_job(&mut jobs, future);
        Ok(operation.wire())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Existing function complexity measured at ratchet adoption"
    )]
    async fn execute(
        self: &Arc<Self>,
        store: u64,
        kind: u32,
        view: OpaqueToken,
        operation: OpaqueToken,
        url: Option<Arc<str>>,
        cancellation: async_engine::CancellationToken,
    ) -> Result<(), WebviewError> {
        if cancellation.is_cancelled() {
            return Err(WebviewError::Cancelled);
        }
        match kind {
            OP_WEBVIEW_OPEN => {
                let request = NativeWebviewRequest::parse(
                    url.as_deref().ok_or(WebviewError::InvalidUrl)?,
                    WebviewPermissions::deny_all(),
                )
                .map_err(map_native)?;
                let mut native = async_engine::cancellable(
                    &cancellation,
                    self.service.backend.open(
                        request,
                        self.service.hub.acquire_native_open().map_err(map_hub)?,
                    ),
                )
                .await
                .map_err(|_| WebviewError::Cancelled)?
                .map_err(map_native)?;
                let terminal = native.wait_until_terminal().map_err(map_native)?;
                self.service
                    .native
                    .lock()
                    .map_err(|_| WebviewError::WindowClosed)?
                    .insert(view, native);
                let service = Arc::clone(&self.service);
                let stop = self.stop.token();
                self.launch(async move {
                    if let Ok(terminal) = async_engine::cancellable(&stop, terminal).await {
                        let reason = match terminal {
                            Ok(Ok(())) => return,
                            Ok(Err(error)) => terminal_for_native(&error),
                            Err(_) => Terminal::Closed,
                        };
                        if !service.is_explicitly_closing(view) {
                            service.revoke_with_terminal(view, reason);
                        }
                    }
                })
                .map_err(map_hub)?;
                if !self.service.hub.finish_external_open(operation, view) {
                    return Err(WebviewError::Cancelled);
                }
            }
            OP_WEBVIEW_LOAD => {
                let receiver = self
                    .service
                    .native
                    .lock()
                    .map_err(|_| WebviewError::WindowClosed)?
                    .get_mut(&view)
                    .ok_or(WebviewError::WindowClosed)?
                    .wait_until_loaded()
                    .map_err(map_native)?;
                let loaded_at = async_engine::cancellable(
                    &cancellation,
                    async_engine::timeout(Duration::from_secs(30), receiver),
                )
                .await
                .map_err(|_| WebviewError::Cancelled)?
                .map_err(|_| WebviewError::TimedOut)?
                .map_err(|_| WebviewError::WindowClosed)?
                .map_err(map_native)?;
                #[cfg(feature = "tauri-webview-test-support")]
                self.service
                    .trace
                    .record(loaded_at, "load-finished", None, None);
                #[cfg(not(feature = "tauri-webview-test-support"))]
                let _ = loaded_at;
                self.service
                    .hub
                    .finish_external_operation(operation, Terminal::Completed);
            }
            OP_WEBVIEW_CAPTURE => {
                #[cfg(feature = "tauri-webview-test-support")]
                self.trace_abi("capture-requested", None);
                let request = self
                    .service
                    .native
                    .lock()
                    .map_err(|_| WebviewError::WindowClosed)?
                    .get(&view)
                    .ok_or(WebviewError::WindowClosed)?
                    .capture_png(
                        Arc::clone(&self.service.hub),
                        store,
                        operation,
                        ViewportCaptureLimits::default(),
                    )?
                    .for_guest();
                // The encoder publishes directly into this guest operation.
                // Its native guard must not consume the guest's blob result.
                if async_engine::timeout(Duration::from_secs(30), cancellation.cancelled())
                    .await
                    .is_err()
                {
                    self.service
                        .hub
                        .finish_external_operation(operation, Terminal::TimedOut);
                }
                #[cfg(feature = "tauri-webview-test-support")]
                if request.exceeded_encoded_byte_limit() {
                    self.trace_abi("capture-encoded-byte-limit", None);
                }
                drop(request);
            }
            OP_WEBVIEW_CLOSE => {
                self.service.mark_explicitly_closing(view);
                let result = async {
                    let mut native = self
                        .service
                        .take_native(view)
                        .ok_or(WebviewError::WindowClosed)?;
                    let closed = native.wait_until_closed().map_err(map_native)?;
                    native.close().map_err(map_native)?;
                    async_engine::cancellable(
                        &cancellation,
                        async_engine::timeout(Duration::from_secs(30), closed),
                    )
                    .await
                    .map_err(|_| WebviewError::Cancelled)?
                    .map_err(|_| WebviewError::TimedOut)?
                    .map_err(|_| WebviewError::WindowClosed)?;
                    self.service
                        .hub
                        .finish_external_operation(operation, Terminal::Completed);
                    self.service.revoke(view);
                    Ok(())
                }
                .await;
                self.service.clear_explicitly_closing(view);
                result?;
            }
            _ => {
                return Err(WebviewError::HostFailure(
                    "unsupported native operation".into(),
                ))
            }
        }
        Ok(())
    }

    pub(crate) async fn shutdown(&self) -> Result<(), WebviewError> {
        self.closing.store(true, Ordering::Release);
        self.stop.cancel();
        let jobs = std::mem::take(&mut *self.jobs.lock().map_err(|_| WebviewError::WindowClosed)?);
        for job in jobs {
            if job.await.is_err() {
                self.failed.store(true, Ordering::Release);
            }
        }
        let native = std::mem::take(
            &mut *self
                .service
                .native
                .lock()
                .map_err(|_| WebviewError::WindowClosed)?,
        );
        let mut closed = Vec::new();
        for (_, mut view) in native {
            if let Ok(receiver) = view.wait_until_closed() {
                closed.push(receiver);
            }
            view.close().map_err(map_native)?;
        }
        async_engine::timeout(Duration::from_secs(30), async {
            for receiver in closed {
                let _ = receiver.await;
            }
            while self.service.hub.snapshot().active_native_captures != 0
                || self.service.hub.snapshot().active_native_opens != 0
            {
                async_engine::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .map_err(|_| WebviewError::TimedOut)?;
        if self.failed.load(Ordering::Acquire) {
            return Err(WebviewError::HostFailure("native job panicked".into()));
        }
        #[cfg(feature = "tauri-webview-test-support")]
        {
            let snapshot = self.service.hub.snapshot();
            self.service.trace.record(
                Instant::now(),
                "hub-drained",
                None,
                Some(WebviewTestObservation {
                    active_clocks: snapshot.active_clocks,
                    active_output_jobs: snapshot.active_output_jobs,
                    active_native_captures: snapshot.active_native_captures,
                    active_native_opens: snapshot.active_native_opens,
                    live_blobs: snapshot.live_blobs,
                    retained_transfer_capacity: snapshot.retained_transfer_capacity,
                    native_backings: self
                        .service
                        .native
                        .lock()
                        .map_err(|_| WebviewError::WindowClosed)?
                        .len(),
                    live_resources: snapshot.live_resources,
                    pending_operations: snapshot.pending_operations,
                }),
            );
        }
        Ok(())
    }
}
