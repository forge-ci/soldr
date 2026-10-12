//! Windows test-process supervision: inert. Nextest isolates every Windows
//! test in its own Job Object and never runs the Unix wrapper there (see
//! `platform/process/test_child.rs`).

use crate::platform::process::test_child::TestSignal;
use std::io;
use std::path::Path;
use std::process::Command;

/// Nothing to configure: Nextest owns the Job Object.
pub fn configure_test_child(
    _command: &mut Command,
    _parent_pid: u32,
    _cgroup_procs: Option<&Path>,
) {
}

/// Signal the process group led by `pid`.
pub fn signal_group(pid: u32, signal: TestSignal) {
    signal_process(pid, signal);
}

/// Signal one process.
pub fn signal_process(pid: u32, signal: TestSignal) {
    if signal == TestSignal::Kill {
        super::terminate::terminate_pid(pid);
    }
}

/// Whether `pid` still exists.
pub fn pid_alive(pid: u32) -> bool {
    super::inspect::is_alive(pid)
}

/// No-op: Windows delivers no SIGTERM to record.
pub fn install_termination_flag() {}

/// Never set on Windows.
pub fn received_termination() -> Option<TestSignal> {
    None
}

/// Create a private directory.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir(path)
}

/// The host page size, for converting `/proc/<pid>/statm` pages to bytes.
pub fn page_size() -> u64 {
    4096
}

/// The exit code (Windows has no signal deaths).
pub fn returncode(status: &std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// Inert: only Linux gives tests a private `TMPDIR` to match on (soldr#3739).
pub fn pids_with_env(_name: &str, _value: &std::ffi::OsStr) -> Vec<u32> {
    Vec::new()
}
