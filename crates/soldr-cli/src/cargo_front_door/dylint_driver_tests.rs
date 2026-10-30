//! Driver-gate tests for the `soldr cargo dylint` front door.
//!
//! Split out of `tests.rs` (soldr#2945): that file is long past the
//! per-file ceiling and the ratchet refuses to let it grow, so the
//! assertions this change needed had to land somewhere new. The driver
//! gate is a self-contained contract -- a channel, a published asset, and
//! the refusal that fires when they do not line up -- so it is a natural
//! seam rather than an arbitrary cut.

use super::*;

#[test]
fn missing_dylint_driver_fails_before_tool_launch() {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("soldr"));
    let plan = crate::dylint_toolchain::DylintToolchainPlan::identity(
        "nightly-2026-05-28".to_string(),
        "1.96.0-nightly".to_string(),
        "0123456789abcdef".to_string(),
    );

    let error = crate::dylint_driver::require_prebuilt_driver(&plan, &paths)
        .expect_err("an absent driver must fail before cargo-dylint launches");

    let message = error.to_string();
    assert!(
        message.contains("dylint-driver"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("nightly-2026-05-28"),
        "unexpected error: {message}"
    );
    // soldr#2945: the diagnostic names the missing asset and refuses to blame
    // the host or the Dylint version, which publishes drivers for every
    // supported triple. What is missing is a driver for *this nightly*.
    assert!(
        message.contains("no usable Dylint driver for nightly-2026-05-28"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("dylint-driver 6.2.0-nightly-2026-05-28"),
        "unexpected error: {message}"
    );
    assert!(
        !message.contains("is not built for this machine"),
        "the driver gate must not blame the host: {message}"
    );
    assert!(
        message.contains("Corrective action:"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("SOLDR_ALLOW_DYLINT_DRIVER_BUILD"),
        "unexpected error: {message}"
    );
}
