#![cfg(feature = "terminal-style")]

use kernal_api::terminal_style::{Foreground, StyledText};

#[test]
fn yellow_diagnostics_preserve_text_and_reset_only_foreground() {
    let text = "warning: déjà vu\nnext line";
    assert_eq!(
        StyledText::new(text, Foreground::Yellow, true).to_string(),
        "\x1b[38;5;11mwarning: déjà vu\nnext line\x1b[39m"
    );
    assert_eq!(
        StyledText::new(text, Foreground::Yellow, false).to_string(),
        text
    );
}

#[test]
fn styling_propagates_output_errors() {
    use std::fmt::Write;
    struct Failed;
    impl Write for Failed {
        fn write_str(&mut self, _: &str) -> std::fmt::Result {
            Err(std::fmt::Error)
        }
    }
    assert!(write!(
        Failed,
        "{}",
        StyledText::new("warning", Foreground::Yellow, true)
    )
    .is_err());
}
