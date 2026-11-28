//! Terminal input offers a bounded wait beside the optional-timeout one.

use std::time::{Duration, Instant};

use kernal_api::terminal_input::{TerminalInputCore, TerminalInputError};

#[test]
fn a_bounded_wait_on_an_idle_core_reports_closed_without_blocking() {
    let core = TerminalInputCore::new();
    let started = Instant::now();
    assert!(matches!(
        core.wait_for_event_within(Duration::from_secs(30)),
        Err(TerminalInputError::Closed)
    ));
    assert!(started.elapsed() < Duration::from_secs(10));
}
