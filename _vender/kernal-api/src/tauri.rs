//! Private raw-Wry backend for the future generated webview operations.
//!
//! This module deliberately does not use `tauri::WebviewWindowBuilder`.
//! Its external-URL route installs the Tauri IPC scripts and handler even
//! when no application command has been configured. A plain native window
//! instead hosts a raw Wry view. Before its first external navigation, the
//! Linux adapter removes Wry's injected scripts and IPC endpoint. No application
//! IPC handler or custom URI scheme is installed. The only native authority it
//! receives is rendering an approved external HTTP(S) document.
//!
//! The public semantic façade at the end of this module submits every native
//! transition through the shared generation-safe operation hub. Raw Wry
//! objects remain private physical backing only.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;

use tauri_runtime::{
    window::{PendingWindow, WindowBuilder},
    ExitRequestedEventAction, RunEvent, Runtime as _, RuntimeHandle as _, WindowDispatch as _,
};
use tauri_runtime_wry::{WindowBuilderWrapper, Wry, WryHandle, WryWindowDispatcher};
use url::Url;
use wry::{NewWindowResponse, PageLoadEvent, WebView, WebViewBuilder};

#[cfg(target_os = "linux")]
#[path = "tauri/linux_webkitgtk.rs"]
mod linux_webkitgtk;
#[cfg(feature = "wasm-sketch-host")]
pub(crate) mod sketch;

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
use wry::raw_window_handle::{HandleError, HasWindowHandle, WindowHandle};

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
use wry::WebViewBuilderExtUnix as _;

#[cfg(target_os = "linux")]
use wry::WebViewExtUnix as _;

use crate::async_engine::{self, OneshotReceiver, OneshotSender, RuntimeHandle};
use crate::operations::{HubError, OpaqueToken, OperationHub, Terminal};

pub(crate) mod capture;
mod monitors;
#[cfg(feature = "tauri-webview-test-support")]
mod trace;
mod window_control;
mod window_options;
pub use capture::{ViewportCaptureLimits, WebviewSnapshot, WebviewSnapshotChunk};
pub use monitors::{WebviewLogicalRect, WebviewMonitor};
#[cfg(feature = "tauri-webview-test-support")]
pub use trace::WebviewTestTraceEvent;
#[cfg(test)]
use window_control::navigation_grant_target;
#[cfg(feature = "tauri-webview-test-support")]
pub(crate) use window_control::ExpectedPresentation;
#[cfg(feature = "tauri-webview-test-support")]
pub use window_control::WebviewTestPresentation;
use window_options::validate_app_id;
#[cfg(test)]
use window_options::MAX_APP_ID_BYTES;
pub use window_options::{
    BestEffort, WebviewWindowOptions, WebviewWindowSupport, WindowOptionsError,
};

static NEXT_LABEL: AtomicU64 = AtomicU64::new(1);
static NEXT_WEBVIEW_STORE: AtomicU64 = AtomicU64::new(1);

// Wry's WebView is deliberately !Send.  The Tauri Wry event-loop thread owns
// this private retention map; commands only route closures to that thread.
thread_local! {
    static UI_WEBVIEWS: RefCell<HashMap<u64, WebView>> = RefCell::new(HashMap::new());
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
#[derive(Clone)]
struct NativeWindowHandle(WryWindowDispatcher<()>);

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
impl HasWindowHandle for NativeWindowHandle {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        self.0.window_handle()
    }
}

/// Why native navigation could not continue.  This remains private until the
/// generated operation layer maps it to facade-owned semantic errors.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum NativeWebviewError {
    #[error("webview URL is malformed or not an HTTP(S) URL")]
    InvalidUrl,
    #[error("webview navigation was rejected because {0}")]
    RejectedNavigation(String),
    #[error("the webview window was closed before its requested page loaded")]
    WindowClosed,
    #[error("the native webview host failed: {0}")]
    HostFailure(String),
}

/// A narrowly scoped request accepted by the native backend.  The registry
/// integration will provide the operation timeout and cancellation policy;
/// this object intentionally carries no ambient capabilities.
#[derive(Clone, Debug)]
pub(crate) struct NativeWebviewRequest {
    url: Url,
    permissions: WebviewPermissions,
    window: Option<WebviewWindowOptions>,
    bootstrap: Option<WebviewPageBootstrap>,
}

impl NativeWebviewRequest {
    pub(crate) fn parse(
        url: &str,
        permissions: WebviewPermissions,
    ) -> Result<Self, NativeWebviewError> {
        // `url::Url` intentionally repairs `https:///name` into
        // `https://name/`. That is useful for browsers but violates this
        // capability's before-effects policy: the caller did not provide a
        // syntactically valid authority, so reject the original spelling.
        let Some((_, authority_and_path)) = url.split_once("://") else {
            return Err(NativeWebviewError::InvalidUrl);
        };
        if authority_and_path.is_empty() || authority_and_path.starts_with('/') {
            return Err(NativeWebviewError::InvalidUrl);
        }
        let url = Url::parse(url).map_err(|_| NativeWebviewError::InvalidUrl)?;
        if is_allowed_url(&url) {
            Ok(Self {
                url,
                permissions,
                window: None,
                bootstrap: None,
            })
        } else {
            Err(NativeWebviewError::InvalidUrl)
        }
    }
}

/// The event-loop owner.  Construct this on the process's UI/main thread and
/// run it there.  It intentionally does not create an async runtime: callers
/// supply the one facade-owned [`RuntimeHandle`] that receives completions.
pub(crate) struct NativeWebviewLoop {
    runtime: Wry<()>,
}

/// Private command front-end tied to one raw Wry event loop.
#[derive(Clone)]
pub(crate) struct NativeWebviewBackend {
    async_runtime: RuntimeHandle,
    wry: WryHandle<()>,
    // Decided once on the UI thread after the toolkit initialized.
    support: WebviewWindowSupport,
}

impl NativeWebviewLoop {
    /// Initializes raw Wry without a Tauri application, plugins, commands,
    /// capability files, or IPC.  This must execute on the platform's UI
    /// thread; the caller subsequently drives [`Self::run`].
    pub(crate) fn new(
        async_runtime: RuntimeHandle,
        app_id: Option<&str>,
    ) -> Result<(Self, NativeWebviewBackend), NativeWebviewError> {
        #[cfg(target_os = "linux")]
        linux_webkitgtk::prepare_renderer_environment();
        let init_args = crate::native_webview_window::runtime_init_args(app_id)
            .map_err(NativeWebviewError::HostFailure)?;
        let runtime = Wry::new(init_args)
            .map_err(|error| NativeWebviewError::HostFailure(error.to_string()))?;
        let backend = NativeWebviewBackend {
            async_runtime,
            wry: runtime.handle(),
            support: WebviewWindowSupport::for_host(
                crate::native_webview_window::display_places_windows(),
            ),
        };
        Ok((Self { runtime }, backend))
    }

    /// Runs Wry's canonical event loop on its owner thread.  Async callers
    /// never block this loop: creation runs in the supplied runtime's blocking
    /// lane and Wry routes it back using its supported event-loop messages.
    pub(crate) fn run(self) -> i32 {
        self.runtime.run_return(|event| {
            // Keep the shell alive after the final window disappears until
            // the caller has observed its completion and explicitly asks to
            // exit. Wry otherwise auto-exits first, losing the final result.
            if let RunEvent::ExitRequested { code: None, tx, .. } = event {
                let _ = tx.send(ExitRequestedEventAction::Prevent);
            }
        })
    }
}

impl NativeWebviewBackend {
    /// Stops the raw Wry event loop after its resources have been released.
    /// Test/application shell code owns this decision; webview operations only
    /// dispatch through the existing loop.
    pub(crate) fn request_exit(&self) -> Result<(), NativeWebviewError> {
        self.wry
            .request_exit(0)
            .map_err(|error| NativeWebviewError::HostFailure(error.to_string()))
    }

    /// Validates before dispatching any native work, then asks raw Wry to make
    /// a window on the event-loop thread.  This function itself does not make
    /// a generated operation: the caller owns the operation/resource token.
    pub(crate) async fn open(
        &self,
        request: NativeWebviewRequest,
        lease: crate::operations::NativeOpenLease,
    ) -> Result<NativeWebview, NativeWebviewError> {
        let (created_sender, created_receiver) = async_engine::oneshot_channel();
        let backend = self.clone();
        self.async_runtime
            .launch_blocking(move || backend.create_on_wry_thread(request, created_sender, lease))
            .detach();

        created_receiver.await.map_err(|_| {
            NativeWebviewError::HostFailure("event loop stopped during creation".into())
        })?
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Existing function complexity measured at ratchet adoption"
    )]
    fn create_on_wry_thread(
        &self,
        request: NativeWebviewRequest,
        created_sender: OneshotSender<Result<NativeWebview, NativeWebviewError>>,
        lease: crate::operations::NativeOpenLease,
    ) {
        let (completion, load_waiter) = LoadCompletion::new(request.url.clone());
        let (terminal, terminal_waiter) = TerminalCompletion::new();
        let (closed, close_waiter) = CloseCompletion::new();
        let created_sender = Arc::new(Mutex::new(Some(created_sender)));
        let native_id = NEXT_LABEL.fetch_add(1, Ordering::Relaxed);
        let label = format!("kernal-api-webview-{native_id}");
        let window_builder = native_window_builder(request.window.as_ref(), self.support);
        let pending_window = match PendingWindow::<(), Wry<()>>::new(window_builder, label) {
            Ok(window) => window,
            Err(error) => {
                let _ = created_sender
                    .lock()
                    .expect("creation sender lock poisoned")
                    .take()
                    .expect("creation sender is present")
                    .send(Err(NativeWebviewError::HostFailure(error.to_string())));
                return;
            }
        };
        // This call intentionally occurs off the UI thread. Tauri Wry
        // documents
        // that its handle synchronously routes `create_window` to the event
        // loop, which avoids the Windows callback deadlock caused by invoking
        // it inside a Wry event-loop callback.
        let detached = match self.wry.create_window(
            pending_window,
            None::<for<'a> fn(tauri_runtime::window::RawWindow<'a>)>,
        ) {
            Ok(window) => window,
            Err(error) => {
                let _ = created_sender
                    .lock()
                    .expect("creation sender lock poisoned")
                    .take()
                    .expect("creation sender is present")
                    .send(Err(NativeWebviewError::HostFailure(error.to_string())));
                return;
            }
        };
        let dispatcher = detached.dispatcher;
        debug_assert!(detached.webview.is_none());
        let completion_on_close = Arc::clone(&completion);
        let terminal_on_close = Arc::clone(&terminal);
        let closed_on_close = Arc::clone(&closed);
        dispatcher.on_window_event(move |event| {
            if matches!(event, tauri_runtime::window::WindowEvent::Destroyed) {
                capture::cancel_for_view(native_id);
                let removed = UI_WEBVIEWS.with(|webviews| webviews.borrow_mut().remove(&native_id));
                drop(removed);
                completion_on_close.finish(Err(NativeWebviewError::WindowClosed));
                terminal_on_close.finish(Err(NativeWebviewError::WindowClosed));
                closed_on_close.finish();
            }
        });

        // The native getter synchronously routes to the event loop. Query on
        // this creation worker, never inside the UI closure below. Script-free
        // routes do not need a scale query or bootstrap context.
        let bootstrap_source = request.bootstrap.as_ref().map(|script| {
            script.for_origin(&request.url, dispatcher.scale_factor().unwrap_or(1.0))
        });
        let window_for_ui = dispatcher.clone();
        let completion_for_ui = Arc::clone(&completion);
        let terminal_for_ui = Arc::clone(&terminal);
        let created_sender_for_ui = Arc::clone(&created_sender);
        if let Err(error) = dispatcher.run_on_main_thread(move || {
            let _lease = lease;
            let transparent = request
                .window
                .as_ref()
                .is_some_and(WebviewWindowOptions::is_transparent);
            let skip_taskbar = request
                .window
                .as_ref()
                .is_some_and(WebviewWindowOptions::skips_taskbar);
            let bootstrap_origin = request.bootstrap.as_ref().map(|_| request.url.origin());
            let result = build_isolated_webview(
                &window_for_ui,
                request.url,
                request.permissions,
                transparent,
                bootstrap_source,
                completion_for_ui,
                terminal_for_ui,
            );
            let created_sender = created_sender_for_ui
                .lock()
                .expect("creation sender lock poisoned")
                .take();
            match (result, created_sender) {
                (Ok(webview), Some(created_sender)) if !created_sender.is_closed() => {
                    UI_WEBVIEWS.with(|webviews| {
                        webviews.borrow_mut().insert(native_id, webview);
                    });
                    let excluded = if skip_taskbar {
                        crate::native_webview_window::exclude_from_taskbar(&window_for_ui)
                    } else {
                        Ok(())
                    };
                    match excluded {
                        Ok(()) => {
                            let _ = created_sender.send(Ok(NativeWebview {
                                window: window_for_ui,
                                native_id,
                                completion,
                                load_waiter: Some(load_waiter),
                                terminal_waiter: Some(terminal_waiter),
                                close_waiter: Some(close_waiter),
                                close_requested: AtomicBool::new(false),
                                skip_taskbar,
                                bootstrap_origin,
                            }));
                        }
                        Err(error) => {
                            let removed = UI_WEBVIEWS
                                .with(|webviews| webviews.borrow_mut().remove(&native_id));
                            drop(removed);
                            let _ = window_for_ui.close();
                            let _ =
                                created_sender.send(Err(NativeWebviewError::HostFailure(error)));
                        }
                    }
                }
                (Ok(webview), _) => {
                    // Cancellation during UI construction leaves no resource
                    // holder, so release the direct Wry view immediately.
                    drop(webview);
                    let _ = window_for_ui.close();
                }
                (Err(error), Some(created_sender)) => {
                    let _ = window_for_ui.close();
                    let _ = created_sender.send(Err(error));
                }
                (Err(_), None) => {
                    let _ = window_for_ui.close();
                }
            }
        }) {
            let _ = dispatcher.close();
            if let Some(created_sender) = created_sender
                .lock()
                .expect("creation sender lock poisoned")
                .take()
            {
                let _ =
                    created_sender.send(Err(NativeWebviewError::HostFailure(error.to_string())));
            }
        }
    }
}

/// Private native resource.  The generated resource table owns its public
/// identity; this type owns only Wry dispatchers and one load completion.
pub(crate) struct NativeWebview {
    window: WryWindowDispatcher<()>,
    native_id: u64,
    completion: Arc<LoadCompletion>,
    load_waiter: Option<OneshotReceiver<Result<Instant, NativeWebviewError>>>,
    terminal_waiter: Option<OneshotReceiver<Result<(), NativeWebviewError>>>,
    close_waiter: Option<OneshotReceiver<()>>,
    close_requested: AtomicBool,
    // Re-asserted on every show: Windows re-adds a taskbar button.
    skip_taskbar: bool,
    // A bootstrap view's navigation stays on this origin.
    bootstrap_origin: Option<url::Origin>,
}

impl NativeWebview {
    /// Point the load waiter at a navigated page. A still-pending waiter
    /// for the previous page ends without a result.
    pub(crate) fn rearm_load(&mut self, target: Url) {
        self.load_waiter = Some(self.completion.rearm(target));
    }

    /// Installs one waiter for the requested navigation's finished load.
    /// A resource can have one generated operation waiting at a time.
    pub(crate) fn wait_until_loaded(
        &mut self,
    ) -> Result<OneshotReceiver<Result<Instant, NativeWebviewError>>, NativeWebviewError> {
        self.load_waiter
            .take()
            .ok_or_else(|| NativeWebviewError::HostFailure("load waiter already consumed".into()))
    }

    /// Receives an isolation or host-terminal fault even when the page-load
    /// operation has already completed.  The generated registry uses this to
    /// revoke its resource generation and wake later operations.
    pub(crate) fn wait_until_terminal(
        &mut self,
    ) -> Result<OneshotReceiver<Result<(), NativeWebviewError>>, NativeWebviewError> {
        self.terminal_waiter.take().ok_or_else(|| {
            NativeWebviewError::HostFailure("terminal waiter already consumed".into())
        })
    }

    /// Resolves only once the native window has actually been destroyed.
    pub(crate) fn wait_until_closed(&mut self) -> Result<OneshotReceiver<()>, NativeWebviewError> {
        self.close_waiter
            .take()
            .ok_or_else(|| NativeWebviewError::HostFailure("close waiter already consumed".into()))
    }

    /// Requests native close once and wakes a pending waiter.  Wry routes the
    /// close to the event-loop thread; no async or native runtime is created.
    pub(crate) fn close(&self) -> Result<(), NativeWebviewError> {
        if self
            .close_requested
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            if let Err(error) = self.window.close() {
                self.close_requested.store(false, Ordering::Release);
                let error = NativeWebviewError::HostFailure(error.to_string());
                self.completion.finish(Err(error.clone()));
                return Err(error);
            }
            let native_id = self.native_id;
            let _ = self.window.run_on_main_thread(move || {
                capture::cancel_for_view(native_id);
                let removed = UI_WEBVIEWS.with(|webviews| webviews.borrow_mut().remove(&native_id));
                drop(removed);
            });
            self.completion
                .finish(Err(NativeWebviewError::WindowClosed));
        }
        Ok(())
    }
}

impl Drop for NativeWebview {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct LoadCompletion {
    state: Mutex<LoadTarget>,
}

// The page a load waiter is waiting for, and its one-shot result channel.
struct LoadTarget {
    url: Url,
    sender: Option<OneshotSender<Result<Instant, NativeWebviewError>>>,
}

struct TerminalCompletion {
    sender: Mutex<Option<OneshotSender<Result<(), NativeWebviewError>>>>,
}

impl TerminalCompletion {
    fn new() -> (Arc<Self>, OneshotReceiver<Result<(), NativeWebviewError>>) {
        let (sender, receiver) = async_engine::oneshot_channel();
        (
            Arc::new(Self {
                sender: Mutex::new(Some(sender)),
            }),
            receiver,
        )
    }

    fn finish(&self, result: Result<(), NativeWebviewError>) {
        let sender = self
            .sender
            .lock()
            .expect("terminal completion lock poisoned")
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(result);
        }
    }
}

struct CloseCompletion {
    sender: Mutex<Option<OneshotSender<()>>>,
}

impl CloseCompletion {
    fn new() -> (Arc<Self>, OneshotReceiver<()>) {
        let (sender, receiver) = async_engine::oneshot_channel();
        (
            Arc::new(Self {
                sender: Mutex::new(Some(sender)),
            }),
            receiver,
        )
    }

    fn finish(&self) {
        let sender = self
            .sender
            .lock()
            .expect("close completion lock poisoned")
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
    }
}

impl LoadCompletion {
    fn new(
        target: Url,
    ) -> (
        Arc<Self>,
        OneshotReceiver<Result<Instant, NativeWebviewError>>,
    ) {
        let (sender, receiver) = async_engine::oneshot_channel();
        (
            Arc::new(Self {
                state: Mutex::new(LoadTarget {
                    url: target,
                    sender: Some(sender),
                }),
            }),
            receiver,
        )
    }

    /// Wait for a different page from now on; the previous waiter's sender
    /// is dropped, so a pending wait for it ends instead of hanging.
    fn rearm(&self, target: Url) -> OneshotReceiver<Result<Instant, NativeWebviewError>> {
        let (sender, receiver) = async_engine::oneshot_channel();
        *self.state.lock().expect("load completion lock poisoned") = LoadTarget {
            url: target,
            sender: Some(sender),
        };
        receiver
    }

    fn finish(&self, result: Result<(), NativeWebviewError>) {
        let result = result.map(|()| Instant::now());
        let sender = self
            .state
            .lock()
            .expect("load completion lock poisoned")
            .sender
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(result);
        }
    }

    fn matches_requested(&self, loaded: &Url) -> bool {
        self.state
            .lock()
            .expect("load completion lock poisoned")
            .url
            == *loaded
    }
}

/// Carry every validated presentation option into the native window builder.
/// A position the display leaves to the compositor is not sent.
fn native_window_builder(
    options: Option<&WebviewWindowOptions>,
    support: WebviewWindowSupport,
) -> WindowBuilderWrapper {
    let Some(options) = options else {
        return WindowBuilderWrapper::new().title("kernal-api external-content proof");
    };
    let builder = WindowBuilderWrapper::new()
        .title(options.title())
        .inner_size(f64::from(options.width), f64::from(options.height))
        .decorations(options.decorations)
        .always_on_top(options.always_on_top)
        .skip_taskbar(options.skip_taskbar);
    let builder = match (options.position, support.position) {
        (Some((x, y)), BestEffort::Requested) => builder.position(f64::from(x), f64::from(y)),
        _ => builder,
    };
    crate::native_webview_window::transparent_window(builder, options.transparent)
}

fn build_isolated_webview(
    dispatcher: &WryWindowDispatcher<()>,
    target: Url,
    permissions: WebviewPermissions,
    transparent: bool,
    bootstrap_source: Option<String>,
    completion: Arc<LoadCompletion>,
    terminal: Arc<TerminalCompletion>,
) -> Result<WebView, NativeWebviewError> {
    #[cfg(not(target_os = "linux"))]
    let _ = permissions;
    let completion_for_navigation = Arc::clone(&completion);
    let terminal_for_navigation = Arc::clone(&terminal);
    let completion_for_popup = Arc::clone(&completion);
    let terminal_for_popup = Arc::clone(&terminal);
    let completion_for_load = Arc::clone(&completion);
    let bootstrap_origin = bootstrap_source.as_ref().map(|_| target.origin());
    let builder = WebViewBuilder::new()
        // Deliberately do not call `with_ipc_handler`: Wry documents that it
        // exposes `window.ipc.postMessage` to page JavaScript.
        // Do not navigate during construction. The Linux adapter must remove
        // backend-injected scripts and endpoints before any external page runs.
        .with_incognito(true)
        .with_transparent(transparent)
        .with_clipboard(false)
        .with_devtools(false)
        .with_general_autofill_enabled(false)
        .with_navigation_handler(move |url| match Url::parse(&url) {
            Ok(url) if navigation_allowed(&url, bootstrap_origin.as_ref()) => true,
            Ok(url) => {
                let reason = if is_allowed_url(&url) {
                    "cross-origin bootstrap navigation"
                } else {
                    url.scheme()
                };
                let error = NativeWebviewError::RejectedNavigation(reason.to_owned());
                completion_for_navigation.finish(Err(error.clone()));
                terminal_for_navigation.finish(Err(error));
                false
            }
            Err(_) => {
                let error = NativeWebviewError::RejectedNavigation("malformed URL".into());
                completion_for_navigation.finish(Err(error.clone()));
                terminal_for_navigation.finish(Err(error));
                false
            }
        })
        .with_new_window_req_handler(move |url, _| {
            let scheme = Url::parse(&url)
                .map(|url| url.scheme().to_owned())
                .unwrap_or_else(|_| "malformed URL".into());
            let error = NativeWebviewError::RejectedNavigation(format!("popup to {scheme}"));
            completion_for_popup.finish(Err(error.clone()));
            terminal_for_popup.finish(Err(error));
            NewWindowResponse::Deny
        })
        .with_on_page_load_handler(move |event, loaded_url| {
            // Finished is the requested lifecycle signal, not a claim that
            // the network response was an HTTP success on every engine.
            if matches!(event, PageLoadEvent::Finished)
                && Url::parse(&loaded_url)
                    .is_ok_and(|loaded| completion_for_load.matches_requested(&loaded))
            {
                completion_for_load.finish(Ok(()));
            }
        })
        .with_download_started_handler(|_, _| false);

    // Linux must add the caller script AFTER removing backend scripts/IPC.
    #[cfg(not(target_os = "linux"))]
    let builder = if let Some(source) = bootstrap_source.as_ref() {
        builder.with_initialization_script_for_main_only(source.clone(), true)
    } else {
        builder
    };

    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    {
        linux_webkitgtk::ensure_font_dpi();
        let webview = builder
            .build_gtk(&dispatcher.default_vbox().map_err(host_failure)?)
            .map_err(host_failure)?;
        linux_webkitgtk::remove_host_bridge(&webview.webview())?;
        if let Some(source) = bootstrap_source.as_ref() {
            linux_webkitgtk::install_page_bootstrap(&webview.webview(), source)?;
        }
        linux_webkitgtk::configure_permissions(&webview.webview(), permissions);
        webview.load_url(target.as_str()).map_err(host_failure)?;
        Ok(webview)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    {
        let webview = builder
            .build(&NativeWindowHandle(dispatcher.clone()))
            .map_err(host_failure)?;
        webview.load_url(target.as_str()).map_err(host_failure)?;
        Ok(webview)
    }
}

fn host_failure(error: impl std::fmt::Display) -> NativeWebviewError {
    NativeWebviewError::HostFailure(error.to_string())
}

fn is_allowed_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && !url.cannot_be_a_base()
        && url.username().is_empty()
        && url.password().is_none()
}

/// Facade-owned failures for an external webview operation.
///
/// No native backend, runtime, or window value is exposed through this type.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WebviewError {
    #[error("the native viewport capture queue is full")]
    CaptureBusy,
    #[error("viewport capture exceeds its pixel limit or has invalid dimensions")]
    CapturePixelLimit,
    #[error("viewport capture exceeds its encoded-byte or blob quota")]
    CaptureByteLimit,
    #[error("native viewport capture or PNG encoding failed")]
    CaptureFailed,
    #[error("webview URL is malformed or not an HTTP(S) URL")]
    InvalidUrl,
    #[error("webview navigation was rejected: {0}")]
    RejectedNavigation(String),
    #[error("webview load timed out")]
    TimedOut,
    #[error("webview operation was cancelled")]
    Cancelled,
    #[error("the webview window was closed")]
    WindowClosed,
    #[error("the webview host failed: {0}")]
    HostFailure(String),
    /// A presentation value (size or application id) failed validation
    /// before any native effect.
    #[error("invalid webview window option: {0}")]
    InvalidWindowOptions(WindowOptionsError),
    /// Another timed or untimed terminal wait is currently pending.
    #[error("a terminal webview wait is already active")]
    TerminalWaitInProgress,
}

/// Semantic permissions for one external webview.
///
/// All permissions are denied by default. Enabling user media permits only
/// microphone/camera requests on Linux WebKitGTK; unrelated WebKit permission
/// requests retain their engine-default denial. This type intentionally does
/// not expose a WebKit, Wry, or Tauri policy object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WebviewPermissions {
    pub(crate) allow_user_media: bool,
}

impl WebviewPermissions {
    /// Start from the deny-by-default webview permission policy.
    pub const fn deny_all() -> Self {
        Self {
            allow_user_media: false,
        }
    }

    /// Permit microphone/camera requests for this webview where the host
    /// supports user media. This does not grant geolocation, notifications,
    /// downloads, clipboard access, or host IPC.
    pub const fn allow_user_media(mut self) -> Self {
        self.allow_user_media = true;
        self
    }
}

fn navigation_allowed(url: &Url, required_origin: Option<&url::Origin>) -> bool {
    is_allowed_url(url) && required_origin.is_none_or(|origin| *origin == url.origin())
}

/// Invalid caller-supplied bootstrap source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PageBootstrapError {
    #[error("page bootstrap exceeds 65536 UTF-8 bytes")]
    SourceTooLarge,
    #[error("page bootstrap contains a NUL character")]
    ContainsNul,
}

/// Explicit native-caller opt-in to document-start page JavaScript.
///
/// The trusted caller owns source correctness and effects. Source executes in
/// a block in the main frame's ordinary page world, not an isolated privileged
/// world. No native IPC or guest ABI capability is installed. Runtime syntax
/// errors follow normal page error reporting; they are not host-open errors.
///
/// The block reserves the lexical binding `kernalWindow`: a frozen object with
/// `initialScaleFactor`, the native window's creation-time scale (physical
/// pixels per logical pixel). This finite positive snapshot is independent of
/// browser zoom, defaults to 1 on unavailable/invalid native data, and does not
/// update after moving between displays. It exposes no native methods or IPC.
/// Source must not redeclare `kernalWindow` in the same block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebviewPageBootstrap {
    source: String,
}

impl WebviewPageBootstrap {
    /// Validate the 64 KiB byte limit and absence of NUL before copying.
    /// This bounds source storage, not execution time or page-side allocations.
    pub fn new(source: &str) -> Result<Self, PageBootstrapError> {
        if source.len() > 65536 {
            return Err(PageBootstrapError::SourceTooLarge);
        }
        if source.contains('\0') {
            return Err(PageBootstrapError::ContainsNul);
        }
        Ok(Self {
            source: source.to_owned(),
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    fn for_origin(&self, target: &Url, native_scale: f64) -> String {
        // WebView2 injects into subframes regardless of Wry's main-only flag.
        // Guard in page code too, including against initial about:blank.
        // The origin is URL-canonicalized and JS-string escaped; source is
        // deliberately trusted caller code, never a remote page's input.
        let scale = if native_scale.is_finite() && native_scale > 0.0 {
            native_scale
        } else {
            1.0
        };
        format!(
            "if (window === window.top && location.origin === \"{}\") {{\nconst kernalWindow = Object.freeze({{ initialScaleFactor: {scale} }});\n{}\n}}\n",
            target.origin().ascii_serialization().escape_default(),
            self.source
        )
    }
}

/// Process-main-thread owner of the opt-in native event loop.
///
/// Construct this on the UI/main thread, hand [`ExternalWebviewClient`] to
/// async work, then call [`Self::run`]. The supplied facade runtime is the
/// only async runtime used by callbacks and operations.
pub struct ExternalWebviewHost {
    event_loop: NativeWebviewLoop,
    client: ExternalWebviewClient,
}

/// Instance-scoped semantic client for opening external webviews.
#[derive(Clone)]
pub struct ExternalWebviewClient {
    service: Arc<WebviewService>,
    store: u64,
}

/// One host-validated HTTP(S) URL to authorize before guest instantiation.
/// This grants no filesystem, arbitrary navigation, or network API authority.
#[derive(Clone)]
pub struct WebviewUrlGrant {
    url: Arc<str>,
}

impl WebviewUrlGrant {
    #[cfg(feature = "wasm-sketch-worker")]
    pub(crate) fn worker_url(&self) -> &str {
        &self.url
    }

    /// Validate without creating a window or starting native work. Both the
    /// input and canonical URL must fit the fixed 16 KiB authority bound.
    pub fn new(url: &str) -> Result<Self, WebviewError> {
        if url.len() > crate::operations::MAX_WEBVIEW_URL_BYTES {
            return Err(WebviewError::InvalidUrl);
        }
        let request =
            NativeWebviewRequest::parse(url, WebviewPermissions::deny_all()).map_err(map_native)?;
        if request.url.as_str().len() > crate::operations::MAX_WEBVIEW_URL_BYTES {
            return Err(WebviewError::InvalidUrl);
        }
        Ok(Self {
            url: Arc::from(request.url.as_str()),
        })
    }

    pub(crate) fn bind(&self, hub: &OperationHub, store: u64) -> Result<OpaqueToken, HubError> {
        hub.grant_webview_url(store, Arc::clone(&self.url))
    }
}

/// Opaque, generation-safe external-webview resource.
///
/// It is intentionally non-cloneable: dropping it revokes the generation and
/// requests physical close, so abandoned operations cannot retain a window.
pub struct WebviewHandle {
    service: Arc<WebviewService>,
    store: u64,
    resource: OpaqueToken,
    terminal_operation: OpaqueToken,
    terminal_wait_active: AtomicBool,
}

// The hub terminal operation is single-consumer. Admission belongs to the
// borrowed future so cancellation releases it without revoking the window.
struct TerminalWaitGuard<'a>(&'a AtomicBool);

impl Drop for TerminalWaitGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Acceptance-only semantic counters for the process-main-thread smoke test.
///
/// This deliberately reports counts rather than a backend handle, native
/// window, runtime, or raw resource token.
#[cfg(feature = "tauri-webview-test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebviewTestObservation {
    /// Kernel clock producers not yet drained.
    pub active_clocks: usize,
    /// Exact-output jobs not yet joined.
    pub active_output_jobs: usize,
    /// Queued UI captures and native callbacks still holding admission.
    pub active_native_captures: usize,
    /// Native creation requests retained after semantic cancellation.
    pub active_native_opens: usize,
    /// Encoded image resources retained by the shared hub.
    pub live_blobs: usize,
    /// Hub and native chunk allocations still charged to the transfer quota.
    pub retained_transfer_capacity: usize,
    /// Private physical WebView backings retained by the facade.
    pub native_backings: usize,
    /// Live generation-safe semantic resources in the shared hub.
    pub live_resources: usize,
    /// Pending semantic operations in the shared hub.
    pub pending_operations: usize,
}

struct WebviewService {
    #[cfg(feature = "tauri-webview-test-support")]
    trace: Arc<trace::Recorder>,
    // The validated host app id, kept only so the acceptance check can
    // compare it with what the native toolkit received.
    #[cfg(feature = "tauri-webview-test-support")]
    app_id: Option<Arc<str>>,
    runtime: RuntimeHandle,
    backend: NativeWebviewBackend,
    hub: Arc<OperationHub>,
    // This is a physical backing table, not a second authority registry:
    // all identity, ownership, quota, terminal state, and revocation remain
    // in OperationHub. Wry objects cannot be stored in that runtime-neutral
    // hub because they are UI-thread-affine.
    native: Mutex<BTreeMap<OpaqueToken, NativeWebview>>,
    // An explicit semantic close owns its terminal result.  The independent
    // native-terminal watcher must not race it into reporting WindowClosed.
    closing: Mutex<BTreeSet<OpaqueToken>>,
}

// Own the semantic reservation across the native creation await. Native
// creation separately closes its window if the oneshot receiver disappears;
// that physical cleanup cannot reclaim this hub's operation or resource.
struct PendingWebviewOpen {
    service: Arc<WebviewService>,
    store: u64,
    resource: OpaqueToken,
    operation: OpaqueToken,
    transferred: bool,
}

struct PendingUrlGrant {
    hub: Arc<OperationHub>,
    resource: OpaqueToken,
}

impl Drop for PendingUrlGrant {
    fn drop(&mut self) {
        let _ = self.hub.close_resource(self.resource);
    }
}

impl Drop for PendingWebviewOpen {
    fn drop(&mut self) {
        if !self.transferred {
            self.service
                .hub
                .finish_external_operation(self.operation, Terminal::Cancelled);
            let _ = self
                .service
                .hub
                .observe_terminal(self.store, self.operation);
            self.service
                .revoke_with_terminal(self.resource, Terminal::Cancelled);
        }
    }
}

impl ExternalWebviewHost {
    /// Create the private event loop and one root semantic instance.
    pub fn new(runtime: RuntimeHandle) -> Result<Self, WebviewError> {
        Self::create(runtime, None)
    }

    /// Like [`Self::new`], with a stable application id for every window this
    /// host opens, so a compositor can match window rules (for example a KWin
    /// keep-above rule for a desktop widget).
    ///
    /// The id is validated before any native effect: 1 to 255 bytes of
    /// `[A-Za-z0-9._-]` forming at least two dot-separated elements, none
    /// empty and none starting with a digit (reverse-DNS, such as
    /// `dev.example.widget`). An invalid id returns
    /// [`WebviewError::InvalidWindowOptions`] with
    /// [`WindowOptionsError::InvalidAppId`].
    ///
    /// On Linux it becomes the GTK application id, which GTK publishes as the
    /// Wayland `xdg_toplevel` app id. GTK also registers it on the D-Bus
    /// session bus, so use one id per running process. The X11 `WM_CLASS`
    /// stays the program name. Windows and macOS accept and ignore it (the
    /// macOS equivalent is the bundle identifier in `Info.plist`).
    pub fn with_app_id(runtime: RuntimeHandle, app_id: &str) -> Result<Self, WebviewError> {
        validate_app_id(app_id).map_err(WebviewError::InvalidWindowOptions)?;
        Self::create(runtime, Some(app_id))
    }

    fn create(runtime: RuntimeHandle, app_id: Option<&str>) -> Result<Self, WebviewError> {
        let (event_loop, backend) =
            NativeWebviewLoop::new(runtime.clone(), app_id).map_err(map_native)?;
        let hub = OperationHub::new(64, 64).map_err(map_hub)?;
        Ok(Self {
            event_loop,
            client: ExternalWebviewClient {
                service: Arc::new(WebviewService {
                    #[cfg(feature = "tauri-webview-test-support")]
                    trace: Arc::new(trace::Recorder::new()),
                    #[cfg(feature = "tauri-webview-test-support")]
                    app_id: app_id.map(Arc::from),
                    runtime,
                    backend,
                    hub,
                    native: Mutex::new(BTreeMap::new()),
                    closing: Mutex::new(BTreeSet::new()),
                }),
                store: next_store()?,
            },
        })
    }

    /// Obtain the root instance-scoped semantic client.
    pub fn client(&self) -> ExternalWebviewClient {
        self.client.clone()
    }

    /// Run the canonical Tauri/Wry event loop on the creating main thread.
    pub fn run(self) -> i32 {
        self.event_loop.run()
    }
}

impl ExternalWebviewClient {
    /// Return bounded acceptance events and the number omitted at capacity.
    /// A proof must reject a nonzero omitted count instead of assuming a full trace.
    #[cfg(feature = "tauri-webview-test-support")]
    pub fn test_trace(&self) -> (Vec<WebviewTestTraceEvent>, usize) {
        self.service.trace.snapshot()
    }
    /// Create an independently authorized logical instance over the same
    /// host. Handles from one instance cannot be used by another.
    pub fn new_instance(&self) -> Result<Self, WebviewError> {
        Ok(Self {
            service: Arc::clone(&self.service),
            store: next_store()?,
        })
    }

    /// Validate and asynchronously create an isolated external webview.
    pub async fn open_webview(&self, url: &str) -> Result<WebviewHandle, WebviewError> {
        let grant = WebviewUrlGrant::new(url)?;
        self.open_granted_webview(&grant).await
    }

    /// Create an isolated native webview with explicitly supplied permissions.
    pub async fn open_webview_with_permissions(
        &self,
        url: &str,
        permissions: WebviewPermissions,
    ) -> Result<WebviewHandle, WebviewError> {
        let grant = WebviewUrlGrant::new(url)?;
        self.open_granted_with_permissions(&grant, permissions)
            .await
    }

    /// Open a prevalidated host URL. The temporary registry grant is scoped
    /// to this instance and revoked on completion, error, or future drop.
    pub async fn open_granted_webview(
        &self,
        grant: &WebviewUrlGrant,
    ) -> Result<WebviewHandle, WebviewError> {
        self.open_granted_with_permissions(grant, WebviewPermissions::deny_all())
            .await
    }

    async fn open_granted_with_permissions(
        &self,
        grant: &WebviewUrlGrant,
        permissions: WebviewPermissions,
    ) -> Result<WebviewHandle, WebviewError> {
        self.open_granted_with_request(grant, permissions, None, None)
            .await
    }

    /// Open an isolated external page with validated window presentation.
    ///
    /// Reuses the same permission, navigation, lifetime, and no-IPC policy as
    /// [`Self::open_webview_with_permissions`]. URL validation still precedes
    /// native effects. The options constructor performs presentation validation.
    pub async fn open_webview_with_options(
        &self,
        url: &str,
        window: WebviewWindowOptions,
        permissions: WebviewPermissions,
    ) -> Result<WebviewHandle, WebviewError> {
        let grant = WebviewUrlGrant::new(url)?;
        self.open_granted_with_request(&grant, permissions, Some(window), None)
            .await
    }

    /// Open with explicit caller-owned main-frame document-start JavaScript.
    ///
    /// Re-runs on same-origin reloads. All native navigation is restricted to
    /// the original HTTP(S) origin (scheme, host, port); crossing it revokes the
    /// view. Existing open methods remain script-free. Script correctness,
    /// product protocols, and permission choices belong to the caller.
    /// Bootstrap origins are limited to 4096 UTF-8 bytes before script wrapping.
    pub async fn open_webview_with_bootstrap(
        &self,
        url: &str,
        window: WebviewWindowOptions,
        permissions: WebviewPermissions,
        bootstrap: WebviewPageBootstrap,
    ) -> Result<WebviewHandle, WebviewError> {
        let preview = NativeWebviewRequest::parse(url, permissions).map_err(map_native)?;
        if preview.url.origin().ascii_serialization().len() > 4096 {
            return Err(WebviewError::InvalidUrl);
        }
        let grant = WebviewUrlGrant::new(url)?;
        self.open_granted_with_request(&grant, permissions, Some(window), Some(bootstrap))
            .await
    }

    async fn open_granted_with_request(
        &self,
        grant: &WebviewUrlGrant,
        permissions: WebviewPermissions,
        window: Option<WebviewWindowOptions>,
        bootstrap: Option<WebviewPageBootstrap>,
    ) -> Result<WebviewHandle, WebviewError> {
        let grant = PendingUrlGrant {
            hub: Arc::clone(&self.service.hub),
            resource: grant.bind(&self.service.hub, self.store).map_err(map_hub)?,
        };
        let (resource, operation, url) = self
            .service
            .hub
            .begin_granted_webview_open(self.store, grant.resource)
            .map_err(map_hub)?;
        let mut pending = PendingWebviewOpen {
            service: Arc::clone(&self.service),
            store: self.store,
            resource,
            operation,
            transferred: false,
        };
        let mut request = NativeWebviewRequest::parse(&url, permissions).map_err(map_native)?;
        request.window = window;
        request.bootstrap = bootstrap;
        let lease = self.service.hub.acquire_native_open().map_err(map_hub)?;
        let mut native = match self.service.backend.open(request, lease).await {
            Ok(native) => native,
            Err(error) => {
                self.service
                    .hub
                    .finish_external_operation(operation, terminal_for_native(&error));
                self.service.revoke(resource);
                return Err(map_native(error));
            }
        };
        let terminal = native.wait_until_terminal().map_err(map_native)?;
        self.service
            .native
            .lock()
            .map_err(|_| WebviewError::HostFailure("native backing table poisoned".into()))?
            .insert(resource, native);
        if !self.service.hub.finish_external_open(operation, resource) {
            self.service.revoke(resource);
        }
        let service = Arc::clone(&self.service);
        self.service
            .runtime
            .launch(async move {
                // Native callbacks retain only this hub/service state and a
                // receiver; a Store, Caller, guest memory, or UI object never
                // crosses into the callback task.
                let terminal = match terminal.await {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => terminal_for_native(&error),
                    Err(_) => Terminal::Closed,
                };
                if !service.is_explicitly_closing(resource) {
                    service.revoke_with_terminal(resource, terminal);
                }
            })
            .detach();
        match self.service.hub.observe_terminal(self.store, operation) {
            Ok(Some(result)) if result.terminal == Terminal::Completed => {
                let terminal_operation = self
                    .service
                    .hub
                    .begin_external_webview_wait(self.store, resource)
                    .map_err(map_hub)?;
                pending.transferred = true;
                Ok(WebviewHandle {
                    service: Arc::clone(&self.service),
                    store: self.store,
                    resource,
                    terminal_operation,
                    terminal_wait_active: AtomicBool::new(false),
                })
            }
            Ok(Some(result)) => Err(map_terminal(result.terminal)),
            Ok(None) => Err(WebviewError::HostFailure(
                "webview open did not complete".into(),
            )),
            Err(error) => Err(map_hub(error)),
        }
    }

    /// Ask the host to stop once outstanding callbacks have completed.
    pub fn request_exit(&self) -> Result<(), WebviewError> {
        self.service.backend.request_exit().map_err(map_native)
    }

    /// Return acceptance-only semantic counts without exposing a backend type.
    #[cfg(feature = "tauri-webview-test-support")]
    pub fn test_observation(&self) -> WebviewTestObservation {
        let snapshot = self.service.hub.snapshot();
        let native_backings = self.service.native.lock().map_or(0, |native| native.len());
        WebviewTestObservation {
            active_clocks: snapshot.active_clocks,
            active_output_jobs: snapshot.active_output_jobs,
            active_native_captures: snapshot.active_native_captures,
            active_native_opens: snapshot.active_native_opens,
            live_blobs: snapshot.live_blobs,
            retained_transfer_capacity: snapshot.retained_transfer_capacity,
            native_backings,
            live_resources: snapshot.live_resources,
            pending_operations: snapshot.pending_operations,
        }
    }
}

impl WebviewHandle {
    /// Await the requested top-level page's matching `Finished` event.
    /// A timeout revokes this handle and closes the backing native window.
    pub async fn wait_until_loaded(&self, timeout: Duration) -> Result<(), WebviewError> {
        let operation = self
            .service
            .hub
            .begin_external_webview_wait(self.store, self.resource)
            .map_err(map_hub)?;
        let receiver = {
            let mut native =
                self.service.native.lock().map_err(|_| {
                    WebviewError::HostFailure("native backing table poisoned".into())
                })?;
            native
                .get_mut(&self.resource)
                .ok_or(WebviewError::WindowClosed)?
                .wait_until_loaded()
                .map_err(map_native)?
        };
        let terminal = match async_engine::timeout(timeout, receiver).await {
            Ok(Ok(Ok(_loaded_at))) => Terminal::Completed,
            Ok(Ok(Err(error))) => terminal_for_native(&error),
            Ok(Err(_)) => Terminal::Closed,
            Err(_) => Terminal::TimedOut,
        };
        self.service
            .hub
            .finish_external_operation(operation, terminal);
        if terminal != Terminal::Completed {
            self.service.revoke_with_terminal(self.resource, terminal);
        }
        match self.service.hub.observe_terminal(self.store, operation) {
            Ok(Some(result)) if result.terminal == Terminal::Completed => Ok(()),
            Ok(Some(result)) => Err(map_terminal(result.terminal)),
            Ok(None) => Err(WebviewError::HostFailure(
                "load operation did not complete".into(),
            )),
            Err(error) => Err(map_hub(error)),
        }
    }

    /// Await a terminal security or window-close callback after opening.
    ///
    /// This is useful when an allowed top-level document finishes and then
    /// attempts a prohibited redirect or popup. It is itself a hub-owned
    /// operation, so callback completion never needs to retain a Store.
    /// Only one terminal wait may be active; overlapping timed or untimed
    /// waits return [`WebviewError::TerminalWaitInProgress`] without expiring
    /// the window. Dropping the future releases that admission.
    pub async fn wait_until_terminal(&self, timeout: Duration) -> Result<(), WebviewError> {
        self.wait_terminal(Some(timeout)).await
    }

    /// Await user closure, cancellation, or a terminal host/security event
    /// without imposing a lifetime deadline on an interactive window.
    ///
    /// Owns no timer and does not poll periodically. Dropping this borrowed
    /// future leaves the window alive; a subsequent wait observes retained
    /// terminal state, including an event that arrived between waits. Dropping
    /// or cancelling the handle still revokes the window. As with the timed
    /// variant, normal user closure is reported as [`WebviewError::WindowClosed`].
    /// Overlapping terminal waits return [`WebviewError::TerminalWaitInProgress`].
    pub async fn wait_for_terminal(&self) -> Result<(), WebviewError> {
        self.wait_terminal(None).await
    }

    async fn wait_terminal(&self, timeout: Option<Duration>) -> Result<(), WebviewError> {
        self.terminal_wait_active
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| WebviewError::TerminalWaitInProgress)?;
        let _admission = TerminalWaitGuard(&self.terminal_wait_active);
        // A cancellation or window callback may have completed this operation
        // before the caller first awaits it. Poll first; if completion wins
        // the short race before suspension, consume that typed terminal below
        // rather than collapsing it into HubError::Closed.
        if let Some(result) = self
            .service
            .hub
            .observe_terminal(self.store, self.terminal_operation)
            .map_err(map_hub)?
        {
            return Err(map_terminal(result.terminal));
        }
        match self
            .service
            .hub
            .wait_external_operation(self.store, self.terminal_operation)
        {
            Ok(wake) => {
                if let Some(timeout) = timeout {
                    if async_engine::timeout(timeout, wake.notified())
                        .await
                        .is_err()
                    {
                        self.service
                            .revoke_with_terminal(self.resource, Terminal::TimedOut);
                    }
                } else {
                    wake.notified().await;
                }
            }
            // Completion can race the poll above; the final observe below
            // retains the callback's typed terminal result.
            Err(HubError::Closed) => {}
            Err(error) => return Err(map_hub(error)),
        }
        match self
            .service
            .hub
            .observe_terminal(self.store, self.terminal_operation)
        {
            Ok(Some(result)) => Err(map_terminal(result.terminal)),
            Ok(None) => Err(WebviewError::HostFailure(
                "terminal webview operation did not complete".into(),
            )),
            Err(error) => Err(map_hub(error)),
        }
    }

    /// Request close and await native destruction before revoking the handle.
    pub async fn close(self) -> Result<(), WebviewError> {
        let operation = self
            .service
            .hub
            .begin_external_webview_close(self.store, self.resource)
            .map_err(map_hub)?;
        self.service.mark_explicitly_closing(self.resource);
        let mut native = self.service.take_native(self.resource).ok_or_else(|| {
            self.service.clear_explicitly_closing(self.resource);
            WebviewError::WindowClosed
        })?;
        let closed = match native.wait_until_closed() {
            Ok(closed) => closed,
            Err(error) => {
                self.service
                    .hub
                    .finish_external_operation(operation, terminal_for_native(&error));
                self.service
                    .revoke_with_terminal(self.resource, terminal_for_native(&error));
                self.service.clear_explicitly_closing(self.resource);
                return Err(map_native(error));
            }
        };
        if let Err(error) = native.close() {
            self.service
                .hub
                .finish_external_operation(operation, terminal_for_native(&error));
            self.service
                .revoke_with_terminal(self.resource, terminal_for_native(&error));
            self.service.clear_explicitly_closing(self.resource);
            return Err(map_native(error));
        }
        let terminal = match closed.await {
            Ok(()) => Terminal::Completed,
            Err(_) => Terminal::Closed,
        };
        self.service
            .hub
            .finish_external_operation(operation, terminal);
        let outcome = match self.service.hub.observe_terminal(self.store, operation) {
            Ok(Some(result)) if result.terminal == Terminal::Completed => Ok(()),
            Ok(Some(result)) => Err(map_terminal(result.terminal)),
            Ok(None) => Err(WebviewError::HostFailure(
                "close operation did not complete".into(),
            )),
            Err(error) => Err(map_hub(error)),
        };
        // The close operation itself is resource-bound, so publish and consume
        // its successful terminal state before resource revocation wakes any
        // other operation bound to the same generation.
        let _ = self.service.hub.close_resource(self.resource);
        self.service.clear_explicitly_closing(self.resource);
        outcome
    }

    /// Explicitly cancel a handle. Cancellation is terminal and revokes the
    /// resource generation before returning.
    pub fn cancel(&self) {
        self.service
            .revoke_with_terminal(self.resource, Terminal::Cancelled);
    }

    /// Acceptance-only stand-in for a user/window-manager close gesture.
    /// The normal terminal callback path must revoke the same semantic handle
    /// as a real user close, while the smoke can invoke it deterministically.
    #[cfg(feature = "tauri-webview-test-support")]
    pub fn request_window_close_for_test(&self) -> Result<(), WebviewError> {
        let native = self
            .service
            .native
            .lock()
            .map_err(|_| WebviewError::HostFailure("native backing table poisoned".into()))?;
        native
            .get(&self.resource)
            .ok_or(WebviewError::WindowClosed)?
            .close()
            .map_err(map_native)
    }
}

impl Drop for WebviewHandle {
    fn drop(&mut self) {
        self.service
            .revoke_with_terminal(self.resource, Terminal::Cancelled);
    }
}

impl WebviewService {
    fn take_native(&self, resource: OpaqueToken) -> Option<NativeWebview> {
        self.native.lock().ok()?.remove(&resource)
    }

    fn revoke(&self, resource: OpaqueToken) {
        self.revoke_with_terminal(resource, Terminal::Closed);
    }

    fn revoke_with_terminal(&self, resource: OpaqueToken, terminal: Terminal) {
        let _ = self.hub.revoke_external_resource(resource, terminal);
        if let Some(native) = self.take_native(resource) {
            let _ = native.close();
            drop(native);
        }
    }

    fn mark_explicitly_closing(&self, resource: OpaqueToken) {
        if let Ok(mut closing) = self.closing.lock() {
            closing.insert(resource);
        }
    }

    fn clear_explicitly_closing(&self, resource: OpaqueToken) {
        if let Ok(mut closing) = self.closing.lock() {
            closing.remove(&resource);
        }
    }

    fn is_explicitly_closing(&self, resource: OpaqueToken) -> bool {
        self.closing
            .lock()
            .is_ok_and(|closing| closing.contains(&resource))
    }
}

fn next_store() -> Result<u64, WebviewError> {
    NEXT_WEBVIEW_STORE
        .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .map(|value| value + 1)
        .map_err(|_| WebviewError::HostFailure("webview instance identifiers exhausted".into()))
}

fn terminal_for_native(error: &NativeWebviewError) -> Terminal {
    match error {
        NativeWebviewError::RejectedNavigation(_) | NativeWebviewError::InvalidUrl => {
            Terminal::Rejected
        }
        NativeWebviewError::WindowClosed => Terminal::Closed,
        NativeWebviewError::HostFailure(_) => Terminal::Trapped,
    }
}

fn map_native(error: NativeWebviewError) -> WebviewError {
    match error {
        NativeWebviewError::InvalidUrl => WebviewError::InvalidUrl,
        NativeWebviewError::RejectedNavigation(reason) => WebviewError::RejectedNavigation(reason),
        NativeWebviewError::WindowClosed => WebviewError::WindowClosed,
        NativeWebviewError::HostFailure(reason) => WebviewError::HostFailure(reason),
    }
}

fn map_terminal(terminal: Terminal) -> WebviewError {
    match terminal {
        Terminal::Cancelled => WebviewError::Cancelled,
        Terminal::TimedOut => WebviewError::TimedOut,
        Terminal::Closed => WebviewError::WindowClosed,
        Terminal::Rejected => WebviewError::RejectedNavigation("navigation policy".into()),
        Terminal::Completed => WebviewError::HostFailure("unexpected completed error".into()),
        Terminal::Trapped | Terminal::OwnerExited => {
            WebviewError::HostFailure("native webview operation failed".into())
        }
    }
}

fn map_hub(error: HubError) -> WebviewError {
    match error {
        HubError::Quota => WebviewError::HostFailure("webview operation quota exhausted".into()),
        HubError::Closed | HubError::Stale => WebviewError::WindowClosed,
        HubError::Invalid | HubError::WrongKind | HubError::WrongRights | HubError::Exhausted => {
            WebviewError::HostFailure("invalid semantic webview operation".into())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bootstrap_exposes_native_scale_snapshot_before_caller_source() {
        let target = Url::parse("http://127.0.0.1:8080/").unwrap();
        let bootstrap =
            WebviewPageBootstrap::new("window.scale = kernalWindow.initialScaleFactor;").unwrap();
        for scale in [1.0, 1.25, 2.0] {
            let wrapped = bootstrap.for_origin(&target, scale);
            assert!(wrapped.contains(&format!("const kernalWindow = Object.freeze({{ initialScaleFactor: {scale} }});\nwindow.scale")));
        }
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let wrapped = bootstrap.for_origin(&target, invalid);
            assert!(wrapped.contains("initialScaleFactor: 1 }"));
        }
    }

    use super::*;

    #[test]
    fn prevalidated_url_grant_is_bounded_and_rejects_ambient_authority() {
        for url in [
            "file:///etc/passwd",
            "data:text/html,x",
            "javascript:alert(1)",
            "tauri://localhost",
            "https:///bad",
            "https://user@example.test/",
        ] {
            assert!(matches!(
                WebviewUrlGrant::new(url),
                Err(WebviewError::InvalidUrl)
            ));
        }
        let huge = format!(
            "https://example.test/{}",
            "a".repeat(crate::operations::MAX_WEBVIEW_URL_BYTES)
        );
        assert!(matches!(
            WebviewUrlGrant::new(&huge),
            Err(WebviewError::InvalidUrl)
        ));
        let grant = WebviewUrlGrant::new("HTTPS://EXAMPLE.TEST:443/exact?q=1").unwrap();
        let hub = OperationHub::new(4, 4).unwrap();
        let token = grant.bind(&hub, 7).unwrap();
        let (_, _, url) = hub.begin_granted_webview_open(7, token).unwrap();
        assert_eq!(&*url, "https://example.test/exact?q=1");
        hub.close_all(Terminal::Cancelled);
        assert_eq!(hub.snapshot().live_resources, 0);
    }

    #[test]
    fn bootstrap_navigation_is_origin_scoped_without_changing_default_route() {
        let target = Url::parse("http://127.0.0.1:8080/start").unwrap();
        for allowed in [
            "http://127.0.0.1:8080/reload",
            "http://127.0.0.1:8080/start#fragment",
        ] {
            assert!(navigation_allowed(
                &Url::parse(allowed).unwrap(),
                Some(&target.origin())
            ));
        }
        for rejected in [
            "http://localhost:8080/",
            "http://127.0.0.1:8081/",
            "https://127.0.0.1:8080/",
        ] {
            let url = Url::parse(rejected).unwrap();
            assert!(!navigation_allowed(&url, Some(&target.origin())));
            assert!(navigation_allowed(&url, None));
        }
        assert!(!navigation_allowed(
            &Url::parse("file:///tmp/test").unwrap(),
            None
        ));
        let bootstrap = WebviewPageBootstrap::new("window.marker = 1; // comment").unwrap();
        let wrapped = bootstrap.for_origin(&target, 1.0);
        assert!(wrapped.starts_with(
            "if (window === window.top && location.origin === \"http://127.0.0.1:8080\") {\n"
        ));
        assert!(wrapped.ends_with("// comment\n}\n"));
    }

    #[test]
    fn external_url_policy_admits_loopback_and_refuses_ambient_schemes() {
        assert!(NativeWebviewRequest::parse(
            "http://127.0.0.1:8080/page",
            WebviewPermissions::default(),
        )
        .is_ok());
        assert!(NativeWebviewRequest::parse(
            "https://example.test/",
            WebviewPermissions::default(),
        )
        .is_ok());
        for forbidden in [
            "file:///etc/passwd",
            "data:text/html,hello",
            "tauri://localhost",
            "javascript:alert(1)",
            "https:///missing-host",
            "https://user@example.test/",
        ] {
            assert_eq!(
                NativeWebviewRequest::parse(forbidden, WebviewPermissions::default()).unwrap_err(),
                NativeWebviewError::InvalidUrl,
                "must reject {forbidden}",
            );
        }
    }

    #[test]
    fn app_id_accepts_reverse_dns_up_to_the_shared_bound() {
        let longest = format!("dev.{}", "a".repeat(MAX_APP_ID_BYTES - 4));
        assert_eq!(longest.len(), MAX_APP_ID_BYTES);
        for valid in [
            "dev.bosn.widget",
            "org.example.App_2",
            "io.github.zackees.kernal-api",
            "a.b",
            longest.as_str(),
        ] {
            assert_eq!(validate_app_id(valid), Ok(()), "{valid:?}");
        }
        for invalid in [
            "",
            "widget",
            ".dev.widget",
            "dev.widget.",
            "dev..widget",
            "dev.2widget",
            "dev.wid get",
            "dev.widget/x",
            "dev.wídget",
            &format!("{longest}a"),
        ] {
            assert_eq!(
                validate_app_id(invalid),
                Err(WindowOptionsError::InvalidAppId),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn window_support_reports_compositor_placed_controls_as_unsupported() {
        let compositor = WebviewWindowSupport::for_host(true);
        assert_eq!(compositor.position, BestEffort::Unsupported);
        assert_eq!(compositor.keep_above, BestEffort::Unsupported);
        assert_eq!(compositor.skip_taskbar, BestEffort::Unsupported);
        let client_placed = WebviewWindowSupport::for_host(false);
        assert_eq!(client_placed.position, BestEffort::Requested);
        assert_eq!(client_placed.keep_above, BestEffort::Requested);
        assert_eq!(client_placed.skip_taskbar, BestEffort::Requested);
    }

    #[test]
    fn navigation_grant_keeps_a_bootstrap_view_on_its_origin() {
        let origin = Url::parse("http://127.0.0.1:8080/start").unwrap().origin();
        let same = WebviewUrlGrant::new("http://127.0.0.1:8080/runs/7").unwrap();
        let other = WebviewUrlGrant::new("http://127.0.0.1:9090/runs/7").unwrap();
        assert_eq!(
            navigation_grant_target(&same, Some(&origin))
                .unwrap()
                .as_str(),
            "http://127.0.0.1:8080/runs/7"
        );
        assert!(matches!(
            navigation_grant_target(&other, Some(&origin)),
            Err(WebviewError::RejectedNavigation(_))
        ));
        // A script-free view may move to any prevalidated HTTP(S) grant.
        assert!(navigation_grant_target(&other, None).is_ok());
    }

    #[test]
    fn rearmed_load_completion_tracks_only_the_navigated_page() {
        let first = Url::parse("http://127.0.0.1:8080/a").unwrap();
        let second = Url::parse("http://127.0.0.1:8080/b").unwrap();
        let (completion, mut initial) = LoadCompletion::new(first.clone());
        let mut rearmed = completion.rearm(second.clone());
        // The superseded waiter ends without a result rather than hanging.
        assert!(matches!(
            initial.try_recv(),
            Err(crate::async_engine::TryRecvError::Disconnected)
        ));
        assert!(!completion.matches_requested(&first));
        assert!(completion.matches_requested(&second));
        completion.finish(Ok(()));
        assert!(matches!(rearmed.try_recv(), Ok(Ok(_))));
    }

    #[test]
    fn user_media_permission_is_explicitly_opt_in() {
        assert_eq!(
            WebviewPermissions::default(),
            WebviewPermissions::deny_all()
        );
        assert_ne!(
            WebviewPermissions::deny_all(),
            WebviewPermissions::deny_all().allow_user_media()
        );
    }
}
