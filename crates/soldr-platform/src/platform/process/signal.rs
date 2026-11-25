//! SIGTERM detection for the daemon's fast-exit hook.
//!
//! Issue #1286 (F1) originally added this so container stops and `pkill
//! soldr-daemon` would trigger a graceful drain instead of an abrupt kill.
//! soldr#3059 reversed that: the caller (`soldr-daemon`'s
//! `fast_exit_on_signal`) now treats an arriving SIGTERM as a signal to
//! exit within milliseconds — write an end-of-stream marker, say so on
//! stderr, exit non-zero — rather than to drain. This module only detects
//! that the signal arrived; it carries no opinion about what the caller
//! does in response. Unix hosts register a `tokio` SIGTERM signal stream;
//! Windows (soldr#3096) registers a named terminate event that
//! `terminate::signal_pid(pid, false)` sets -- the platform's SIGTERM
//! equivalent, so the same hook fires on every host.
//!
//! The Windows implementation creates its event eagerly, when this function
//! is *called*, so callers should construct the future before handing it
//! to a task: a terminator that races the task's first poll must still
//! find the event.

pub use crate::platform_imp::process::signal::{
    forward_child_termination, kill_owned_process_group, wait_for_terminate_signal,
    ChildTerminationGuard,
};
