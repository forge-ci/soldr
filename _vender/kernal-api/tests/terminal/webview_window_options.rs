#![cfg(feature = "tauri-webview")]

use kernal_api::webview::{
    BestEffort, ExternalWebviewHost, WebviewError, WebviewWindowOptions, WebviewWindowSupport,
    WindowOptionsError,
};

#[test]
fn window_options_preserve_product_title_and_logical_size() {
    let options = WebviewWindowOptions::new("FastLED — preview", 1280, 720).unwrap();
    assert_eq!(options.title(), "FastLED — preview");
    assert_eq!(options.logical_size(), (1280, 720));
    assert_eq!(options.clone(), options);
}

#[test]
fn window_options_reject_invalid_values_before_native_effects() {
    for (width, height) in [(0, 1), (1, 0), (16385, 1), (1, 16385), (u32::MAX, 1)] {
        assert_eq!(
            WebviewWindowOptions::new("title", width, height),
            Err(WindowOptionsError::InvalidSize)
        );
    }
    for title in ["nul\0title", "line\nbreak", "tab\ttitle"] {
        assert_eq!(
            WebviewWindowOptions::new(title, 1, 1),
            Err(WindowOptionsError::InvalidTitle)
        );
    }
    assert!(WebviewWindowOptions::new("", 1, 16384).is_ok());
    assert!(WebviewWindowOptions::new(&"é".repeat(512), 16384, 1).is_ok());
    assert_eq!(
        WebviewWindowOptions::new(&"é".repeat(513), 1, 1),
        Err(WindowOptionsError::InvalidTitle)
    );
}

#[test]
fn widget_presentation_defaults_to_an_ordinary_decorated_window() {
    let options = WebviewWindowOptions::new("widget", 320, 240).unwrap();
    assert!(options.has_decorations());
    assert!(!options.is_transparent());
    assert!(!options.is_always_on_top());
    assert!(!options.skips_taskbar());
}

#[test]
fn widget_presentation_builders_are_recorded_without_native_effects() {
    let options = WebviewWindowOptions::new("widget", 320, 240)
        .unwrap()
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true);
    assert!(!options.has_decorations());
    assert!(options.is_transparent());
    assert!(options.is_always_on_top());
    assert!(options.skips_taskbar());
    assert_eq!(options.title(), "widget");
    assert_eq!(options.logical_size(), (320, 240));
    let restored = options
        .clone()
        .decorations(true)
        .transparent(false)
        .always_on_top(false)
        .skip_taskbar(false);
    assert_eq!(
        restored,
        WebviewWindowOptions::new("widget", 320, 240).unwrap()
    );
    assert_ne!(restored, options);
}

#[test]
fn invalid_app_ids_are_rejected_before_any_native_host_exists() {
    // Validation precedes event-loop construction, so these calls are safe
    // on a test-worker thread with no display: a valid id would initialize
    // the native host here and is exercised by the main-thread smoke instead.
    let runtime = kernal_api::async_engine::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    // One byte over the AppUserModelID bound shared by every host.
    let too_long = format!("dev.{}", "a".repeat(125));
    for app_id in [
        "",
        "nodots",
        ".leading.dot",
        "trailing.dot.",
        "double..dot",
        "dev.1digit",
        "1dev.example",
        "dev.example/widget",
        "dev.example widget",
        "dev.exämple",
        "dev.example\0",
        too_long.as_str(),
    ] {
        assert_eq!(
            ExternalWebviewHost::with_app_id(runtime.handle(), app_id).err(),
            Some(WebviewError::InvalidWindowOptions(
                WindowOptionsError::InvalidAppId
            )),
            "{app_id:?} must be rejected before native effects"
        );
    }
}

#[test]
fn initial_position_is_recorded_and_bounded_before_native_effects() {
    let options = WebviewWindowOptions::new("widget", 72, 72).unwrap();
    assert_eq!(options.logical_position(), None);
    let placed = options.clone().initial_position(-1920, 40).unwrap();
    assert_eq!(placed.logical_position(), Some((-1920, 40)));
    assert_ne!(placed, options);
    assert_eq!(
        options
            .clone()
            .initial_position(-32768, 32767)
            .unwrap()
            .logical_position(),
        Some((-32768, 32767))
    );
    for (x, y) in [(i32::MIN, 0), (0, i32::MAX), (32768, 0), (0, -32769)] {
        assert_eq!(
            options.clone().initial_position(x, y),
            Err(WindowOptionsError::InvalidPosition),
            "({x}, {y}) must be rejected before native effects"
        );
    }
}

#[test]
fn best_effort_outcomes_are_typed_and_distinguish_unsupported() {
    // A caller matches on the outcome instead of probing strings; an
    // unsupported request is not an error and not a silent success.
    let describe = |outcome: BestEffort| match outcome {
        BestEffort::Requested => "requested",
        BestEffort::Unsupported => "unsupported",
    };
    assert_eq!(describe(BestEffort::Requested), "requested");
    assert_eq!(describe(BestEffort::Unsupported), "unsupported");
    assert_ne!(BestEffort::Requested, BestEffort::Unsupported);
    let support = WebviewWindowSupport {
        position: BestEffort::Unsupported,
        keep_above: BestEffort::Unsupported,
        skip_taskbar: BestEffort::Requested,
    };
    assert_eq!(support.position, BestEffort::Unsupported);
    assert_eq!(support.clone(), support);
}
