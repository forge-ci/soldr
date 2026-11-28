//! Native window control of an open external webview: presentation (resize,
//! move, show, hide, focus), top-level navigation to a prevalidated grant,
//! and the acceptance-only read-backs.
//!
//! Every operation is reserved in the shared operation hub under its own
//! right, so a revoked or closed handle fails each one with
//! [`WebviewError::WindowClosed`] and a concurrent revocation owns the typed
//! reason. Native work is routed through the Wry window dispatcher, never a
//! second event loop. Nothing here installs IPC or a custom scheme.

use tauri_runtime::WindowDispatch as _;
use tauri_runtime_wry::WryWindowDispatcher;
use url::Url;

use super::window_options::{validate_position, validate_size};
use super::{
    map_hub, map_terminal, navigation_allowed, BestEffort, ExternalWebviewClient, WebviewError,
    WebviewHandle, WebviewUrlGrant, WebviewWindowSupport, UI_WEBVIEWS,
};
use crate::async_engine;
use crate::operations::{HubError, OpaqueToken, OperationHub, Terminal};

#[cfg(feature = "tauri-webview-test-support")]
use super::WebviewWindowOptions;

/// Acceptance-only window state as the native toolkit reports it.
///
/// These are toolkit observations, not compositor guarantees: on Wayland the
/// compositor decides keep-above and placement and may never report them.
#[cfg(feature = "tauri-webview-test-support")]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WebviewTestPresentation {
    /// Whether the toolkit reports the window as shown.
    pub visible: bool,
    /// Whether the window has native decorations.
    pub decorated: bool,
    /// Whether the toolkit reports the window as kept above others.
    pub always_on_top: bool,
    /// Client-area width in logical pixels.
    pub logical_width: f64,
    /// Client-area height in logical pixels.
    pub logical_height: f64,
    /// Outer top-left corner, horizontal, in logical pixels.
    pub logical_x: f64,
    /// Outer top-left corner, vertical, in logical pixels.
    pub logical_y: f64,
}

/// What the concrete platform tree checks against its native window.
#[cfg(feature = "tauri-webview-test-support")]
pub(crate) struct ExpectedPresentation<'a> {
    pub(crate) app_id: Option<&'a str>,
    pub(crate) decorations: bool,
    pub(crate) transparent: bool,
    pub(crate) skip_taskbar: bool,
}

/// Which hub right an operation needs.
#[derive(Clone, Copy)]
enum Authority {
    /// Presentation: resize, move, show, hide, focus.
    Window,
    /// Replace the top-level page.
    Navigate,
}

impl Authority {
    fn begin(
        self,
        hub: &OperationHub,
        store: u64,
        resource: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        match self {
            Self::Window => hub.begin_external_webview_window(store, resource),
            Self::Navigate => hub.begin_external_webview_navigate(store, resource),
        }
    }
}

/// The URL a navigation grant may load in a view. A view opened with a page
/// bootstrap stays on its original origin (scheme, host, port), exactly as
/// its navigation handler enforces; a script-free view accepts any grant.
pub(crate) fn navigation_grant_target(
    grant: &WebviewUrlGrant,
    bootstrap_origin: Option<&url::Origin>,
) -> Result<Url, WebviewError> {
    let url = Url::parse(&grant.url).map_err(|_| WebviewError::InvalidUrl)?;
    if navigation_allowed(&url, bootstrap_origin) {
        Ok(url)
    } else {
        Err(WebviewError::RejectedNavigation(
            "cross-origin bootstrap navigation".into(),
        ))
    }
}

#[cfg(feature = "tauri-webview-test-support")]
fn native_scale(window: &WryWindowDispatcher<()>) -> Result<f64, WebviewError> {
    let scale = window
        .scale_factor()
        .map_err(|error| WebviewError::HostFailure(error.to_string()))?;
    if scale.is_finite() && scale > 0.0 {
        Ok(scale)
    } else {
        Err(WebviewError::HostFailure(format!(
            "invalid native scale factor {scale}"
        )))
    }
}

/// The client area in logical pixels as the window system lays it out. The
/// platform tree answers on the UI thread where the toolkit's own getter is
/// stale (macOS: Wry replaces the content view tao measures); otherwise
/// tao's inner size is used.
#[cfg(feature = "tauri-webview-test-support")]
fn logical_client_size(window: &WryWindowDispatcher<()>) -> Result<(f64, f64), WebviewError> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let ui_window = window.clone();
    window
        .run_on_main_thread(move || {
            let _ = sender.send(crate::native_webview_window::client_logical_size(
                &ui_window,
            ));
        })
        .map_err(|error| WebviewError::HostFailure(error.to_string()))?;
    let native = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| WebviewError::HostFailure("native size query did not run".into()))?
        .map_err(WebviewError::HostFailure)?;
    if let Some(size) = native {
        return Ok(size);
    }
    let scale = native_scale(window)?;
    let size = window
        .inner_size()
        .map_err(|error| WebviewError::HostFailure(error.to_string()))?;
    Ok((
        f64::from(size.width) / scale,
        f64::from(size.height) / scale,
    ))
}

impl ExternalWebviewClient {
    /// Which best-effort window controls this host's display honours.
    ///
    /// Decided once when the host starts: on Wayland the compositor places
    /// and stacks windows itself, so position, keep-above and taskbar
    /// exclusion report [`BestEffort::Unsupported`]; X11, Windows and macOS
    /// report [`BestEffort::Requested`].
    pub fn window_support(&self) -> WebviewWindowSupport {
        self.service.backend.support
    }
}

impl WebviewHandle {
    fn native_window(&self) -> Result<(WryWindowDispatcher<()>, u64), WebviewError> {
        // Clone the dispatcher before native synchronous queries: never hold
        // the backing-table lock while waiting for the UI thread.
        let native = self
            .service
            .native
            .lock()
            .map_err(|_| WebviewError::HostFailure("native backing table poisoned".into()))?;
        let native = native
            .get(&self.resource)
            .ok_or(WebviewError::WindowClosed)?;
        Ok((native.window.clone(), native.native_id))
    }

    /// Acceptance-only check of the native title, logical client-area size,
    /// decorations, and the host's own evidence for the remaining options.
    /// Allows one logical pixel of native rounding. Intended for controlled
    /// desktops: a window manager may legitimately constrain production sizes.
    ///
    /// On Linux this also reads back the GTK application id, the GTK
    /// skip-taskbar hint, and the RGBA visual and transparent WebKit
    /// background. On Windows it reads back the process AppUserModelID and
    /// the tool-window style; on macOS the accessory activation policy. Each
    /// is proof the toolkit received the request, not that a compositor
    /// honoured it. Keep-above is observed separately through
    /// [`Self::presentation_for_test`] because a Wayland compositor may refuse it.
    #[cfg(feature = "tauri-webview-test-support")]
    pub fn verify_window_options_for_test(
        &self,
        expected: &WebviewWindowOptions,
    ) -> Result<(), WebviewError> {
        let (window, native_id) = self.native_window()?;
        let host_error = |error: tauri_runtime::Error| WebviewError::HostFailure(error.to_string());
        let title = window.title().map_err(host_error)?;
        let size = logical_client_size(&window)?;
        let decorated = window.is_decorated().map_err(host_error)?;
        if title != expected.title
            || decorated != expected.decorations
            || (size.0 - f64::from(expected.width)).abs() > 1.0
            || (size.1 - f64::from(expected.height)).abs() > 1.0
        {
            return Err(WebviewError::HostFailure(format!(
                "window presentation mismatch: title={title:?}, decorated={decorated}, logical_size={size:?}, expected={expected:?}"
            )));
        }
        let app_id = self.service.app_id.clone();
        let decorations = expected.decorations;
        let transparent = expected.transparent;
        let skip_taskbar = expected.skip_taskbar;
        let (sender, receiver) = std::sync::mpsc::channel();
        let ui_window = window.clone();
        window
            .run_on_main_thread(move || {
                let expected = ExpectedPresentation {
                    app_id: app_id.as_deref(),
                    decorations,
                    transparent,
                    skip_taskbar,
                };
                let result = UI_WEBVIEWS.with(|webviews| {
                    crate::native_webview_window::verify_presentation(
                        &ui_window,
                        webviews.borrow().get(&native_id),
                        &expected,
                    )
                });
                let _ = sender.send(result);
            })
            .map_err(host_error)?;
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|_| WebviewError::HostFailure("native presentation check did not run".into()))?
            .map_err(|mismatch| {
                WebviewError::HostFailure(format!("native presentation mismatch: {mismatch}"))
            })
    }

    /// Acceptance-only snapshot of the window state the native toolkit reports.
    #[cfg(feature = "tauri-webview-test-support")]
    pub fn presentation_for_test(&self) -> Result<WebviewTestPresentation, WebviewError> {
        let (window, _) = self.native_window()?;
        let host_error = |error: tauri_runtime::Error| WebviewError::HostFailure(error.to_string());
        let (logical_width, logical_height) = logical_client_size(&window)?;
        let position = window.outer_position().map_err(host_error)?;
        let scale = native_scale(&window)?;
        Ok(WebviewTestPresentation {
            visible: window.is_visible().map_err(host_error)?,
            decorated: window.is_decorated().map_err(host_error)?,
            always_on_top: window.is_always_on_top().map_err(host_error)?,
            logical_width,
            logical_height,
            logical_x: f64::from(position.x) / scale,
            logical_y: f64::from(position.y) / scale,
        })
    }

    /// Request a new logical client-area size, bounded like
    /// [`super::WebviewWindowOptions::new`] (each dimension 1 through 16384).
    ///
    /// Completes once the native event loop has processed the request. The
    /// window manager may still constrain the final size. A revoked or closed
    /// handle returns [`WebviewError::WindowClosed`].
    pub async fn set_size(&self, width: u32, height: u32) -> Result<(), WebviewError> {
        validate_size(width, height).map_err(WebviewError::InvalidWindowOptions)?;
        self.window_operation(move |window| {
            window.set_size(tauri_runtime::dpi::Size::Logical(
                tauri_runtime::dpi::LogicalSize::new(f64::from(width), f64::from(height)),
            ))
        })
        .await
    }

    /// Best effort: move the window's outer top-left corner to logical
    /// `(x, y)`, each coordinate -32768 through 32767.
    ///
    /// Returns [`BestEffort::Unsupported`] without sending anything when the
    /// display leaves placement to the compositor (Wayland; see
    /// [`ExternalWebviewClient::window_support`]); otherwise
    /// [`BestEffort::Requested`] once the native event loop has processed
    /// it. A revoked or closed handle returns [`WebviewError::WindowClosed`]
    /// either way.
    pub async fn set_position(&self, x: i32, y: i32) -> Result<BestEffort, WebviewError> {
        validate_position(x, y).map_err(WebviewError::InvalidWindowOptions)?;
        let support = self.service.backend.support.position;
        self.window_operation(move |window| match support {
            BestEffort::Unsupported => Ok(()),
            BestEffort::Requested => window.set_position(tauri_runtime::dpi::Position::Logical(
                tauri_runtime::dpi::LogicalPosition::new(f64::from(x), f64::from(y)),
            )),
        })
        .await?;
        Ok(support)
    }

    /// Show the window. Completes once the native event loop has processed
    /// the request. A window opened with
    /// [`super::WebviewWindowOptions::skip_taskbar`] is kept out of the
    /// taskbar again, because Windows re-adds a taskbar button on show. A
    /// revoked or closed handle returns [`WebviewError::WindowClosed`].
    pub async fn show(&self) -> Result<(), WebviewError> {
        let skip_taskbar = self.skips_taskbar()?;
        self.native_operation(
            Authority::Window,
            |window| {
                window.show()?;
                if skip_taskbar {
                    window.set_skip_taskbar(true)?;
                }
                Ok(())
            },
            move |window| {
                if skip_taskbar {
                    crate::native_webview_window::exclude_from_taskbar(window)?;
                }
                Ok(())
            },
        )
        .await
    }

    /// Hide the window without closing it or revoking the handle; the page
    /// keeps running. Completes once the native event loop has processed the
    /// request. A revoked or closed handle returns [`WebviewError::WindowClosed`].
    pub async fn hide(&self) -> Result<(), WebviewError> {
        self.window_operation(|window| window.hide()).await
    }

    /// Best effort: ask to focus the window. Window managers may refuse focus
    /// stealing (Wayland compositors usually do), so success means only that
    /// the native event loop processed the request. A revoked or closed
    /// handle returns [`WebviewError::WindowClosed`].
    pub async fn focus(&self) -> Result<(), WebviewError> {
        self.window_operation(|window| window.set_focus()).await
    }

    /// Replace this view's top-level page with a prevalidated HTTP(S) grant,
    /// reusing the window instead of opening another.
    ///
    /// The view keeps its isolation: no IPC, no custom scheme, incognito,
    /// and the same navigation handler. A view opened with a page bootstrap
    /// only accepts a grant on its original origin; any other is rejected
    /// with [`WebviewError::RejectedNavigation`] before native effects, and
    /// the view stays open. Completes once the native event loop has started
    /// the load; [`Self::wait_until_loaded`] then waits for this page (the
    /// previous page's waiter, if still pending, ends). A revoked or closed
    /// handle returns [`WebviewError::WindowClosed`].
    pub async fn navigate(&self, grant: &WebviewUrlGrant) -> Result<(), WebviewError> {
        let (target, native_id) = {
            let mut native =
                self.service.native.lock().map_err(|_| {
                    WebviewError::HostFailure("native backing table poisoned".into())
                })?;
            let native = native
                .get_mut(&self.resource)
                .ok_or(WebviewError::WindowClosed)?;
            let target = navigation_grant_target(grant, native.bootstrap_origin.as_ref())?;
            native.rearm_load(target.clone());
            (target, native.native_id)
        };
        self.native_operation(
            Authority::Navigate,
            |_| Ok(()),
            move |_| {
                UI_WEBVIEWS.with(|webviews| {
                    webviews
                        .borrow()
                        .get(&native_id)
                        .ok_or_else(|| "the webview is gone".to_owned())?
                        .load_url(target.as_str())
                        .map_err(|error| error.to_string())
                })
            },
        )
        .await
    }

    fn skips_taskbar(&self) -> Result<bool, WebviewError> {
        let native = self
            .service
            .native
            .lock()
            .map_err(|_| WebviewError::HostFailure("native backing table poisoned".into()))?;
        native
            .get(&self.resource)
            .map(|native| native.skip_taskbar)
            .ok_or(WebviewError::WindowClosed)
    }

    async fn window_operation(
        &self,
        apply: impl FnOnce(&WryWindowDispatcher<()>) -> Result<(), tauri_runtime::Error>,
    ) -> Result<(), WebviewError> {
        self.native_operation(Authority::Window, apply, |_| Ok(()))
            .await
    }

    // One hub-owned operation. Like `close`, the native work is routed
    // through the Wry dispatcher; the hub owns the operation, and a
    // concurrent revocation completes it with the revocation's terminal.
    // `apply` dispatches through the window; `on_ui` then runs on the event
    // loop thread after it, with the same window, so completion means the
    // loop processed both.
    async fn native_operation(
        &self,
        authority: Authority,
        apply: impl FnOnce(&WryWindowDispatcher<()>) -> Result<(), tauri_runtime::Error>,
        on_ui: impl FnOnce(&WryWindowDispatcher<()>) -> Result<(), String> + Send + 'static,
    ) -> Result<(), WebviewError> {
        let operation = authority
            .begin(&self.service.hub, self.store, self.resource)
            .map_err(map_hub)?;
        let mut failure = None;
        let terminal = match self.native_window() {
            Err(error) => {
                failure = Some(error);
                Terminal::Closed
            }
            Ok((window, _)) => {
                let (sender, receiver) = async_engine::oneshot_channel();
                let ui_window = window.clone();
                let dispatched = apply(&window).and_then(|()| {
                    window.run_on_main_thread(move || {
                        let _ = sender.send(on_ui(&ui_window));
                    })
                });
                match dispatched {
                    Err(error) => {
                        failure = Some(WebviewError::HostFailure(error.to_string()));
                        Terminal::Trapped
                    }
                    Ok(()) => match receiver.await {
                        Ok(Ok(())) => Terminal::Completed,
                        Ok(Err(error)) => {
                            failure = Some(WebviewError::HostFailure(error));
                            Terminal::Trapped
                        }
                        Err(_) => Terminal::Closed,
                    },
                }
            }
        };
        self.service
            .hub
            .finish_external_operation(operation, terminal);
        match self.service.hub.observe_terminal(self.store, operation) {
            Ok(Some(result)) if result.terminal == Terminal::Completed => Ok(()),
            // A revocation that raced this request owns the typed reason.
            Ok(Some(result)) if result.terminal != terminal => Err(map_terminal(result.terminal)),
            Ok(Some(result)) => Err(failure.unwrap_or_else(|| map_terminal(result.terminal))),
            Ok(None) => Err(WebviewError::HostFailure(
                "window operation did not complete".into(),
            )),
            Err(error) => Err(map_hub(error)),
        }
    }
}
