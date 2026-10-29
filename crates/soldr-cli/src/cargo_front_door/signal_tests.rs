//! Signal identity in Cargo build history.

use super::strip_diagnostics;

#[test]
fn cargo_failure_preserves_signal_identity_in_history() {
    let signaled = crate::platform::process::exit::exit_status_from_code(-15);
    let normal_failure = crate::platform::process::exit::exit_status_from_code(37);
    let outcome = strip_diagnostics::StripOutcome::from_cargo(false, None);
    let expected = if signaled.code().is_none() { -143 } else { -15 };
    assert_eq!(outcome.effective_exit_code(&signaled), expected);
    assert_eq!(outcome.effective_exit_code(&normal_failure), 37);
}
