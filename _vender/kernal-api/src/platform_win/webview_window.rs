//! Windows half of the external webview's window presentation.
//!
//! The application id becomes the process's explicit AppUserModelID, which
//! the taskbar uses to group windows and match pinned shortcuts. Keep-above
//! is tao's `HWND_TOPMOST`. Taskbar exclusion is tao's `ITaskbarList` tab
//! removal plus the `WS_EX_TOOLWINDOW` style, re-asserted on every show
//! because tao recomputes the extended style (restoring `WS_EX_APPWINDOW`)
//! whenever it changes visibility. Transparency uses tao's layered-window
//! path together with WebView2's transparent background.

use tauri_runtime::window::WindowBuilder as _;
use tauri_runtime::RuntimeInitArgs;
use tauri_runtime::WindowDispatch as _;
use tauri_runtime_wry::{WindowBuilderWrapper, WryWindowDispatcher};
use windows::core::HSTRING;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
use wry::raw_window_handle::RawWindowHandle;

/// Event-loop arguments. With an app id, first set it as the process's
/// explicit AppUserModelID, before any window exists.
pub(crate) fn runtime_init_args(app_id: Option<&str>) -> Result<RuntimeInitArgs, String> {
    if let Some(app_id) = app_id {
        // SAFETY: the HSTRING outlives the call, which copies the id.
        unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(app_id)) }
            .map_err(|error| format!("AppUserModelID {app_id:?} was refused: {error}"))?;
    }
    Ok(RuntimeInitArgs::default())
}

/// Win32 lets the application place and stack its windows.
pub(crate) fn display_places_windows() -> bool {
    false
}

/// Request a transparent top-level window.
pub(crate) fn transparent_window(
    builder: WindowBuilderWrapper,
    transparent: bool,
) -> WindowBuilderWrapper {
    builder.transparent(transparent)
}

fn hwnd(window: &WryWindowDispatcher<()>) -> Result<HWND, String> {
    let handle = window.window_handle().map_err(|error| error.to_string())?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Ok(HWND(handle.hwnd.get() as *mut _)),
        other => Err(format!("unexpected native window handle {other:?}")),
    }
}

/// Make the window a tool window without an app-window bit, so the taskbar
/// and Alt+Tab skip it.
pub(crate) fn exclude_from_taskbar(window: &WryWindowDispatcher<()>) -> Result<(), String> {
    let hwnd = hwnd(window)?;
    // SAFETY: `hwnd` is this live view's top-level window; only its
    // extended style bits change.
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let style = (style | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
    }
    Ok(())
}

/// Acceptance-only check: the process AppUserModelID is the host app id and
/// a skip-taskbar window has the tool-window style. The neutral facade has
/// already compared the portable getters.
#[cfg(feature = "tauri-webview-test-support")]
pub(crate) fn verify_presentation(
    window: &WryWindowDispatcher<()>,
    _webview: Option<&wry::WebView>,
    expected: &crate::tauri::ExpectedPresentation<'_>,
) -> Result<(), String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::GetCurrentProcessExplicitAppUserModelID;

    let crate::tauri::ExpectedPresentation {
        app_id,
        decorations,
        transparent,
        skip_taskbar,
    } = expected;
    let _ = (decorations, transparent);
    if let Some(app_id) = app_id {
        // SAFETY: the returned string is owned by the caller and freed once.
        let actual = unsafe {
            let id = GetCurrentProcessExplicitAppUserModelID()
                .map_err(|error| format!("no AppUserModelID: {error}"))?;
            let text = id.to_string();
            CoTaskMemFree(Some(id.0 as *const _));
            text.map_err(|error| error.to_string())?
        };
        if actual != *app_id {
            return Err(format!("AppUserModelID {actual:?}, expected {app_id:?}"));
        }
    }
    let hwnd = hwnd(window)?;
    // SAFETY: reads the live view's extended style.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    let tool = style & WS_EX_TOOLWINDOW.0 as isize != 0;
    let app_window = style & WS_EX_APPWINDOW.0 as isize != 0;
    if *skip_taskbar && (!tool || app_window) {
        return Err(format!(
            "skip-taskbar window has extended style {style:#x} (tool={tool}, app-window={app_window})"
        ));
    }
    Ok(())
}

/// Acceptance-only: tao's inner size is current here.
#[cfg(feature = "tauri-webview-test-support")]
pub(crate) fn client_logical_size(
    window: &WryWindowDispatcher<()>,
) -> Result<Option<(f64, f64)>, String> {
    let _ = window;
    Ok(None)
}
