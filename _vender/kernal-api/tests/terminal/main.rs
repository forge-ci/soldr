//! Terminal input/style and webview window behaviour.
//!
//! One linked test binary per category, not per file: each module below was
//! its own top-level integration test, and each one statically linked this
//! crate's whole graph. Test IDs are now `terminal::<module>::<test>`.
//! See AGENTS.md for the rule.

mod bounded_terminal_input;
mod terminal_input_ownership;
mod terminal_input_windows;
mod terminal_keys;
mod terminal_style;
mod webview_page_bootstrap;
mod webview_window_options;

mod system_tray;
