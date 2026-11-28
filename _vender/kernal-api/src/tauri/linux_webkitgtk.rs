//! Private Linux setup for the WebKitGTK-backed external-webview capability.
//!
//! WebKitGTK snapshots some renderer environment at process launch and reads
//! GTK's screen DPI while creating a view. Keeping those host facts and raw
//! bindings here prevents them from leaking through the semantic facade.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

use gtk::gdk::prelude::MonitorExt as _;
use gtk::prelude::GtkSettingsExt as _;
use webkit2gtk::{
    PermissionRequestExt as _, SettingsExt as _, UserContentManagerExt as _, WebViewExt as _,
};

use super::WebviewPermissions;

#[path = "linux_webkitgtk/dpi.rs"]
mod dpi;

thread_local! {
    static DPI: RefCell<Option<(gtk::Settings, dpi::Correction)>> = const { RefCell::new(None) };
}

/// Host facts that decide the renderer environment, separated from the process
/// state they are read from so the decision itself is testable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostFacts {
    nvidia: bool,
    x11_backend: bool,
    already_set: Vec<String>,
}

fn environment_overrides(facts: &HostFacts) -> BTreeMap<&'static str, &'static str> {
    let mut result = BTreeMap::new();
    let mut set = |key, value| {
        if !facts.already_set.iter().any(|present| present == key) {
            result.insert(key, value);
        }
    };
    // JavaScriptCore gates `SharedArrayBuffer` behind this switch, and a
    // cross-origin-isolated page alone does not enable it. Without it a guest
    // that uses threads -- an Emscripten pthread build loads its module into
    // workers -- fails at the first worker message and never paints.
    set("JSC_useSharedArrayBuffer", "1");
    if facts.nvidia {
        set("__NV_DISABLE_EXPLICIT_SYNC", "1");
        if facts.x11_backend {
            set("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }
    result
}

fn detect_host() -> HostFacts {
    let gdk_backend = std::env::var("GDK_BACKEND").unwrap_or_default();
    let x11_backend = if gdk_backend.is_empty() {
        std::env::var_os("WAYLAND_DISPLAY").is_none()
    } else {
        gdk_backend
            .split(',')
            .next()
            .is_some_and(|backend| backend.trim().eq_ignore_ascii_case("x11"))
    };
    let already_set = [
        "JSC_useSharedArrayBuffer",
        "__NV_DISABLE_EXPLICIT_SYNC",
        "WEBKIT_DISABLE_DMABUF_RENDERER",
    ]
    .into_iter()
    .filter(|key| std::env::var_os(key).is_some())
    .map(str::to_owned)
    .collect();
    HostFacts {
        nvidia: Path::new("/proc/driver/nvidia/version").exists()
            || Path::new("/sys/module/nvidia").exists(),
        x11_backend,
        already_set,
    }
}

/// Must run before Wry initializes WebKitGTK, which snapshots this environment
/// when it launches its renderer processes. Existing user values win.
pub(super) fn prepare_renderer_environment() {
    for (key, value) in environment_overrides(&detect_host()) {
        // SAFETY: this application-level setup runs before the facade creates
        // Wry/WebKitGTK or starts the UI event loop, so no facade thread can
        // concurrently read or modify process environment state.
        unsafe { std::env::set_var(key, value) };
    }
}

/// Published Wry installs an IPC script and endpoint even with no application
/// handler. Remove both before installing any explicitly opted-in caller script.
/// Called on the UI thread before the facade initiates the first navigation.
pub(super) fn remove_host_bridge(
    webview: &webkit2gtk::WebView,
) -> Result<(), super::NativeWebviewError> {
    let manager = webview.user_content_manager().ok_or_else(|| {
        super::NativeWebviewError::HostFailure("webview has no user content manager".into())
    })?;
    manager.remove_all_scripts();
    manager.unregister_script_message_handler("ipc");
    Ok(())
}

/// Called after backend bridge removal and before the first navigation.
pub(super) fn install_page_bootstrap(
    webview: &webkit2gtk::WebView,
    source: &str,
) -> Result<(), super::NativeWebviewError> {
    let manager = webview.user_content_manager().ok_or_else(|| {
        super::NativeWebviewError::HostFailure("webview has no user content manager".into())
    })?;
    let script = webkit2gtk::UserScript::new(
        source,
        webkit2gtk::UserContentInjectedFrames::TopFrame,
        webkit2gtk::UserScriptInjectionTime::Start,
        &[],
        &[],
    );
    manager.add_script(&script);
    Ok(())
}

/// Correct page DPI on the GTK thread without repeatedly dividing our own
/// previous write. A different observed setting becomes the new desktop input.
/// An external reset exactly equal to our last write is indistinguishable.
pub(super) fn ensure_font_dpi() {
    let Some(settings) = gtk::Settings::default() else {
        return;
    };
    let desktop_dpi = settings.gtk_xft_dpi();
    let integer_scale = gtk::gdk::Display::default()
        .and_then(|display| display.primary_monitor().or_else(|| display.monitor(0)))
        .map(|monitor| monitor.scale_factor())
        .unwrap_or(1);
    let effective_dpi = DPI.with(|state| {
        let mut state = state.borrow_mut();
        if state
            .as_ref()
            .is_some_and(|(previous, _)| previous != &settings)
        {
            *state = None;
        }
        let (_, correction) =
            state.get_or_insert_with(|| (settings.clone(), dpi::Correction::default()));
        correction.update(desktop_dpi, integer_scale)
    });
    // Release the state borrow before GTK can invoke property callbacks.
    if desktop_dpi != effective_dpi {
        settings.set_gtk_xft_dpi(effective_dpi);
    }
    if let Some(screen) = gtk::gdk::Screen::default() {
        screen.set_resolution(f64::from(effective_dpi) / 1024.0);
    }
}

/// Installs the opt-in user-media policy on a just-created raw WebKit view.
pub(super) fn configure_permissions(
    webview: &webkit2gtk::WebView,
    permissions: WebviewPermissions,
) {
    if !permissions.allow_user_media {
        return;
    }
    if let Some(settings) = webview.settings() {
        settings.set_enable_media_stream(true);
        settings.set_enable_webaudio(true);
    }
    webview.connect_permission_request(|_, request| {
        use gtk::glib::Cast as _;
        if request
            .downcast_ref::<webkit2gtk::UserMediaPermissionRequest>()
            .is_some()
        {
            request.allow();
            true
        } else {
            false
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(nvidia: bool, x11_backend: bool, already_set: &[&str]) -> HostFacts {
        HostFacts {
            nvidia,
            x11_backend,
            already_set: already_set
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }

    #[test]
    fn environment_decision_table_preserves_user_values() {
        assert_eq!(
            environment_overrides(&facts(false, false, &[])),
            BTreeMap::from([("JSC_useSharedArrayBuffer", "1")])
        );
        let x11_nvidia = environment_overrides(&facts(true, true, &[]));
        assert_eq!(x11_nvidia.get("__NV_DISABLE_EXPLICIT_SYNC"), Some(&"1"));
        assert_eq!(x11_nvidia.get("WEBKIT_DISABLE_DMABUF_RENDERER"), Some(&"1"));
        assert!(!environment_overrides(&facts(true, false, &[]))
            .contains_key("WEBKIT_DISABLE_DMABUF_RENDERER"));
        assert!(!environment_overrides(&facts(
            true,
            true,
            &["JSC_useSharedArrayBuffer", "WEBKIT_DISABLE_DMABUF_RENDERER"],
        ))
        .contains_key("JSC_useSharedArrayBuffer"));
    }
}
