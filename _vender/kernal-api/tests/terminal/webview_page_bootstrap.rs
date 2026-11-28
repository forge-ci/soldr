#![cfg(feature = "tauri-webview")]

use kernal_api::webview::{PageBootstrapError, WebviewPageBootstrap};

#[test]
fn bootstrap_validates_before_copying_source() {
    let source = "window.example = 1; // trailing comment";
    assert_eq!(WebviewPageBootstrap::new(source).unwrap().source(), source);
    assert!(WebviewPageBootstrap::new("").is_ok());
    assert!(WebviewPageBootstrap::new(&"a".repeat(65536)).is_ok());
    assert_eq!(
        WebviewPageBootstrap::new(&"a".repeat(65537)),
        Err(PageBootstrapError::SourceTooLarge)
    );
    assert_eq!(
        WebviewPageBootstrap::new("a\0b"),
        Err(PageBootstrapError::ContainsNul)
    );
}
