//! Test-process supervision primitives for the native Nextest wrapper
//! (`crates/soldr-nextest-wrapper`, soldr#3453).
//!
//! The wrapper is a cfg-free program; everything it needs from the host --
//! the child's session/death-signal setup, process-group signals, the
//! first-termination flag, private directories and the page size -- lives
//! here. Linux carries the full contract (setsid, `PR_SET_PDEATHSIG`,
//! `PR_SET_PTRACER`, cgroup join). macOS starts a new session only.
//! Windows never runs the wrapper (Nextest isolates each test in a Job
//! Object), so its implementation is inert.

pub use crate::platform_imp::process::test_child::{
    configure_test_child, create_private_dir, install_termination_flag, page_size, pid_alive,
    pids_with_env, received_termination, returncode, signal_group, signal_process,
};

/// The signals the wrapper sends or reacts to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestSignal {
    /// SIGTERM: Nextest's timeout request.
    Terminate,
    /// SIGINT: an interactive cancel.
    Interrupt,
    /// SIGKILL: forced termination.
    Kill,
}
