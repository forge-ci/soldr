//! Semantic capture service; all host selection stays at the crate root.

use super::*;

#[allow(dead_code)] // Some typed failures originate on only one native host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureError {
    InvalidDimensions,
    PixelLimit,
    BlobLimit,
    Cancelled,
    NativeFailure,
    EncodingFailure,
    InvalidPng,
}

pub(crate) struct NativeCancellation(Box<dyn Fn()>);
impl NativeCancellation {
    pub(crate) fn new(cancel: impl Fn() + 'static) -> Self {
        Self(Box::new(cancel))
    }
    pub(crate) fn cancel(&self) {
        (self.0)();
    }
}
impl Drop for NativeCancellation {
    fn drop(&mut self) {
        self.cancel();
    }
}

thread_local! {
    static UI_CAPTURES: RefCell<BTreeMap<OpaqueToken, (u64, Option<NativeCancellation>)>> = const { RefCell::new(BTreeMap::new()) };
}

pub(super) fn cancel_for_view(native_id: u64) {
    let removed = UI_CAPTURES.with(|captures| {
        let mut captures = captures.borrow_mut();
        let keys: Vec<_> = captures
            .iter()
            .filter(|(_, (owner, _))| *owner == native_id)
            .map(|(key, _)| *key)
            .collect();
        keys.into_iter()
            .filter_map(|key| captures.remove(&key))
            .collect::<Vec<_>>()
    });
    drop(removed);
}

/// Per-snapshot admission limits, also constrained by the host's blob quota.
#[derive(Clone, Copy, Debug)]
pub struct ViewportCaptureLimits {
    /// Maximum physical pixels, including the native backing scale.
    pub maximum_pixels: u64,
    /// Maximum encoded PNG bytes admitted into the shared blob store.
    pub maximum_encoded_bytes: usize,
}
impl Default for ViewportCaptureLimits {
    fn default() -> Self {
        Self {
            maximum_pixels: 4_000_000,
            maximum_encoded_bytes: 1024 * 1024,
        }
    }
}

/// Opaque snapshot resource. Dropping it releases its quota-accounted PNG.
pub struct WebviewSnapshot {
    hub: Arc<OperationHub>,
    store: u64,
    resource: OpaqueToken,
}
impl WebviewSnapshot {
    /// Pull a bounded PNG chunk. An empty chunk means end of image.
    /// Requests above the host's chunk limit are rejected before copying.
    pub fn read_chunk(
        &self,
        maximum_bytes: usize,
    ) -> Result<WebviewSnapshotChunk<'_>, WebviewError> {
        if maximum_bytes == 0 {
            return Err(WebviewError::CaptureByteLimit);
        }
        self.hub
            .read_blob_chunk(self.store, self.resource, maximum_bytes, false)
            .map(WebviewSnapshotChunk)
            .map_err(map_hub)
    }
}
impl Drop for WebviewSnapshot {
    fn drop(&mut self) {
        let _ = self.hub.close_resource(self.resource);
    }
}

/// Borrowed snapshot bytes whose allocation remains charged until dropped.
pub struct WebviewSnapshotChunk<'a>(crate::operations::NativeBlobChunk<'a>);
impl std::ops::Deref for WebviewSnapshotChunk<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

pub(super) struct CaptureRequest {
    hub: Arc<OperationHub>,
    store: u64,
    operation: OpaqueToken,
    window: WryWindowDispatcher<()>,
    error: Arc<Mutex<Option<CaptureError>>>,
    guest_owned: bool,
}
impl CaptureRequest {
    #[cfg(all(feature = "wasm-sketch-host", feature = "tauri-webview-test-support"))]
    pub(super) fn exceeded_encoded_byte_limit(&self) -> bool {
        self.error
            .lock()
            .is_ok_and(|error| matches!(*error, Some(CaptureError::BlobLimit)))
    }

    #[cfg(feature = "wasm-sketch-host")]
    pub(super) fn for_guest(mut self) -> Self {
        self.guest_owned = true;
        self
    }
}
impl Drop for CaptureRequest {
    fn drop(&mut self) {
        self.hub
            .finish_external_operation(self.operation, Terminal::Cancelled);
        // Completion may have won just before the awaiting future was dropped.
        // Reclaim an unconsumed successful result as well as pending encoding.
        if !self.guest_owned {
            if let Ok(Some(result)) = self.hub.observe_terminal(self.store, self.operation) {
                if let Some(blob) = result.resource {
                    let _ = self.hub.close_resource(blob);
                }
            }
        }
        let operation = self.operation;
        let _ = self.window.run_on_main_thread(move || {
            let removed = UI_CAPTURES.with(|captures| captures.borrow_mut().remove(&operation));
            drop(removed);
        });
    }
}

impl NativeWebview {
    pub(super) fn capture_png(
        &self,
        hub: Arc<OperationHub>,
        store: u64,
        operation: OpaqueToken,
        limits: ViewportCaptureLimits,
    ) -> Result<CaptureRequest, WebviewError> {
        let lease = hub.acquire_native_capture().map_err(|error| {
            if error == HubError::Quota {
                WebviewError::CaptureBusy
            } else {
                map_hub(error)
            }
        })?;
        let error = Arc::new(Mutex::new(None));
        let native_id = self.native_id;
        let request = CaptureRequest {
            guest_owned: false,
            hub: Arc::clone(&hub),
            store,
            operation,
            window: self.window.clone(),
            error: Arc::clone(&error),
        };
        self.window
            .run_on_main_thread(move || {
                UI_CAPTURES.with(|captures| {
                    captures.borrow_mut().insert(operation, (native_id, None));
                });
                let callback_error = Arc::clone(&error);
                let callback_hub = Arc::clone(&hub);
                let result = UI_WEBVIEWS.with(|views| {
                    let views = views.borrow();
                    let view = views.get(&native_id).ok_or(CaptureError::Cancelled)?;
                    crate::native_viewport_capture::capture(
                        view,
                        Arc::clone(&hub),
                        store,
                        operation,
                        limits.maximum_pixels,
                        limits.maximum_encoded_bytes,
                        move |result| {
                            let _lease = lease;
                            if let Err(error) = result {
                                if let Ok(mut slot) = callback_error.lock() {
                                    *slot = Some(error);
                                }
                                callback_hub
                                    .finish_external_operation(operation, Terminal::Rejected);
                            }
                            let removed = UI_CAPTURES
                                .with(|captures| captures.borrow_mut().remove(&operation));
                            drop(removed);
                        },
                    )
                });
                match result {
                    Ok(cancellation) => {
                        // A callback is allowed to run before capture returns.
                        // Never recreate its entry after it has already completed.
                        UI_CAPTURES.with(|captures| {
                            if let Some((_, slot)) = captures.borrow_mut().get_mut(&operation) {
                                *slot = Some(cancellation);
                            }
                        });
                    }
                    Err(failure) => {
                        if let Ok(mut slot) = error.lock() {
                            *slot = Some(failure);
                        }
                        hub.finish_external_operation(operation, Terminal::Rejected);
                        let removed =
                            UI_CAPTURES.with(|captures| captures.borrow_mut().remove(&operation));
                        drop(removed);
                    }
                }
            })
            .map_err(|error| WebviewError::HostFailure(error.to_string()))?;
        Ok(request)
    }
}

impl WebviewHandle {
    /// Capture the rendered viewport as an opaque, bounded PNG resource.
    /// Dropping the future revokes publication and requests native cancellation.
    pub async fn capture_visible_png(
        &self,
        limits: ViewportCaptureLimits,
        timeout: Duration,
    ) -> Result<WebviewSnapshot, WebviewError> {
        if limits.maximum_pixels == 0 {
            return Err(WebviewError::CapturePixelLimit);
        }
        if limits.maximum_encoded_bytes == 0 {
            return Err(WebviewError::CaptureByteLimit);
        }
        let operation = self
            .service
            .hub
            .begin_external_webview_capture(self.store, self.resource)
            .map_err(map_hub)?;
        let request = (|| {
            let native =
                self.service.native.lock().map_err(|_| {
                    WebviewError::HostFailure("native backing table poisoned".into())
                })?;
            native
                .get(&self.resource)
                .ok_or(WebviewError::WindowClosed)?
                .capture_png(Arc::clone(&self.service.hub), self.store, operation, limits)
        })();
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                self.service
                    .hub
                    .finish_external_operation(operation, Terminal::Rejected);
                let _ = self.service.hub.observe_terminal(self.store, operation);
                return Err(error);
            }
        };
        match self
            .service
            .hub
            .wait_external_operation(self.store, operation)
        {
            Ok(wake) => {
                if async_engine::timeout(timeout, wake.notified())
                    .await
                    .is_err()
                {
                    self.service
                        .hub
                        .finish_external_operation(operation, Terminal::TimedOut);
                }
            }
            // Completion can win before waiter registration. Consume its
            // terminal below instead of collapsing it into a closed error.
            Err(HubError::Closed) => {}
            Err(error) => return Err(map_hub(error)),
        }
        let terminal = self
            .service
            .hub
            .observe_terminal(self.store, operation)
            .map_err(map_hub)?
            .ok_or_else(|| WebviewError::HostFailure("capture did not complete".into()))?;
        if terminal.terminal != Terminal::Completed {
            return Err(if terminal.terminal == Terminal::Rejected {
                request
                    .error
                    .lock()
                    .ok()
                    .and_then(|mut error| error.take())
                    .map(map_capture)
                    .unwrap_or_else(|| map_terminal(terminal.terminal))
            } else {
                map_terminal(terminal.terminal)
            });
        }
        let resource = terminal
            .resource
            .ok_or_else(|| WebviewError::HostFailure("capture omitted its snapshot".into()))?;
        Ok(WebviewSnapshot {
            hub: Arc::clone(&self.service.hub),
            store: self.store,
            resource,
        })
    }
}

fn map_capture(error: CaptureError) -> WebviewError {
    match error {
        CaptureError::InvalidDimensions | CaptureError::PixelLimit => {
            WebviewError::CapturePixelLimit
        }
        CaptureError::BlobLimit => WebviewError::CaptureByteLimit,
        CaptureError::Cancelled => WebviewError::Cancelled,
        CaptureError::NativeFailure | CaptureError::EncodingFailure | CaptureError::InvalidPng => {
            WebviewError::CaptureFailed
        }
    }
}

/// Acceptance-only pause released on drop, with a native-side timeout backstop.
#[cfg(feature = "tauri-webview-test-support")]
pub struct WebviewTestUiPause(Option<std::sync::mpsc::Sender<()>>);

#[cfg(feature = "tauri-webview-test-support")]
impl Drop for WebviewTestUiPause {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            let _ = release.send(());
        }
    }
}

#[cfg(feature = "tauri-webview-test-support")]
impl WebviewHandle {
    /// Pause UI dispatch after acknowledging the barrier, for deterministic
    /// cancellation/admission tests. Drop the guard to resume the UI thread.
    pub async fn pause_ui_for_test(&self) -> Result<WebviewTestUiPause, WebviewError> {
        let window = self
            .service
            .native
            .lock()
            .map_err(|_| WebviewError::HostFailure("native backing table poisoned".into()))?
            .get(&self.resource)
            .ok_or(WebviewError::WindowClosed)?
            .window
            .clone();
        let (release, receive) = std::sync::mpsc::channel();
        let pause = WebviewTestUiPause(Some(release));
        let (ready, waiting) = async_engine::oneshot_channel();
        window
            .run_on_main_thread(move || {
                let _ = ready.send(());
                let _ = receive.recv_timeout(Duration::from_secs(20));
            })
            .map_err(|error| WebviewError::HostFailure(error.to_string()))?;
        async_engine::timeout(Duration::from_secs(5), waiting)
            .await
            .map_err(|_| WebviewError::TimedOut)?
            .map_err(|_| WebviewError::WindowClosed)?;
        Ok(pause)
    }
}
