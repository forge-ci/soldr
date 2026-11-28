//! macOS half of the external webview's window presentation.
//!
//! A macOS application id is the bundle identifier, fixed by `Info.plist`
//! for a bundled app and absent for a bare executable; it cannot be changed
//! at run time, so a validated app id is accepted and not applied here.
//! Keep-above is tao's floating window level. Taskbar exclusion has no
//! per-window form: it switches the application to the accessory activation
//! policy (no Dock icon, no menu bar). A transparent NSWindow needs Tauri's
//! `macos-private-api` feature, which this crate does not enable; only the
//! WKWebView background is made transparent.

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use tauri_runtime::RuntimeInitArgs;
use tauri_runtime_wry::{WindowBuilderWrapper, WryWindowDispatcher};

/// Event-loop arguments. The app id has no macOS effect.
pub(crate) fn runtime_init_args(app_id: Option<&str>) -> Result<RuntimeInitArgs, String> {
    let _ = app_id;
    Ok(RuntimeInitArgs::default())
}

/// AppKit lets the application place and stack its windows.
pub(crate) fn display_places_windows() -> bool {
    false
}

/// Window-level transparency is unavailable without the private API.
pub(crate) fn transparent_window(
    builder: WindowBuilderWrapper,
    transparent: bool,
) -> WindowBuilderWrapper {
    let _ = transparent;
    builder
}

/// Acceptance-only: Wry replaces the window's content view with its own
/// parent view, so tao's inner size measures a detached view and stays at
/// the creation size. Measure the live content view instead.
#[cfg(feature = "tauri-webview-test-support")]
pub(crate) fn client_logical_size(
    window: &WryWindowDispatcher<()>,
) -> Result<Option<(f64, f64)>, String> {
    use objc2_app_kit::NSView;
    use tauri_runtime::WindowDispatch as _;
    use wry::raw_window_handle::RawWindowHandle;

    let handle = window.window_handle().map_err(|error| error.to_string())?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return Err("unexpected native window handle".into());
    };
    // SAFETY: the AppKit handle's `ns_view` is this live window's view, and
    // this runs on the main thread that owns it.
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let content = view
        .window()
        .and_then(|window| window.contentView())
        .ok_or("the view has no window content view")?;
    let size = content.frame().size;
    Ok(Some((size.width, size.height)))
}

/// Hide the application from the Dock and the app switcher. Runs on the
/// main thread (inside an event-loop closure); the policy is process-wide
/// and stays in effect until the process exits.
pub(crate) fn exclude_from_taskbar(window: &WryWindowDispatcher<()>) -> Result<(), String> {
    let _ = window;
    let main_thread = MainThreadMarker::new()
        .ok_or("the activation policy can only change on the main thread")?;
    let application = NSApplication::sharedApplication(main_thread);
    if application.activationPolicy() == NSApplicationActivationPolicy::Accessory
        || application.setActivationPolicy(NSApplicationActivationPolicy::Accessory)
    {
        Ok(())
    } else {
        Err("AppKit refused the accessory activation policy".into())
    }
}

/// Acceptance-only check, run on the main thread: a skip-taskbar window has
/// switched the application to the accessory activation policy. The neutral
/// facade has already compared the portable getters.
#[cfg(feature = "tauri-webview-test-support")]
pub(crate) fn verify_presentation(
    _window: &WryWindowDispatcher<()>,
    _webview: Option<&wry::WebView>,
    expected: &crate::tauri::ExpectedPresentation<'_>,
) -> Result<(), String> {
    // The app id, decorations and transparency have no further AppKit
    // evidence here; the request is still recorded.
    let crate::tauri::ExpectedPresentation {
        app_id,
        decorations,
        transparent,
        skip_taskbar,
    } = expected;
    let _ = (app_id, decorations, transparent);
    if *skip_taskbar {
        let main_thread =
            MainThreadMarker::new().ok_or("presentation check ran off the main thread")?;
        let policy = NSApplication::sharedApplication(main_thread).activationPolicy();
        if policy != NSApplicationActivationPolicy::Accessory {
            return Err(format!(
                "skip-taskbar window left the activation policy at {policy:?}"
            ));
        }
    }
    Ok(())
}
