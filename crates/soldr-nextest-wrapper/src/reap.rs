//! Reap processes a test left behind (soldr#3739).
//!
//! Integration fixtures auto-start soldr daemons and brokers that detach
//! from the test's process group, so killing the group does not reach them;
//! about 25 fixtures left one alive per run until its 30-minute idle
//! timeout. Every process the test started inherits the test's private
//! `TMPDIR`, so after the test exits any process still carrying that exact
//! value is a leak: it gets SIGTERM, then SIGKILL after a short grace.

use soldr_platform::process::test_child::{self, TestSignal};
use std::ffi::OsStr;
use std::time::{Duration, Instant};

const TERM_GRACE: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(25);

/// Terminate every process other than this one whose `TMPDIR` is `tmpdir`.
/// Returns how many were found.
pub(crate) fn reap_tmpdir_holders(tmpdir: &OsStr) -> usize {
    let me = std::process::id();
    let pids: Vec<u32> = test_child::pids_with_env("TMPDIR", tmpdir)
        .into_iter()
        .filter(|pid| *pid != me)
        .collect();
    if pids.is_empty() {
        return 0;
    }
    crate::write_stderr(&format!(
        "nextest wrapper: test left {} process(es) running ({pids:?}); terminating (soldr#3739)\n",
        pids.len()
    ));
    for pid in &pids {
        test_child::signal_process(*pid, TestSignal::Terminate);
    }
    let deadline = Instant::now() + TERM_GRACE;
    while pids.iter().any(|pid| test_child::pid_alive(*pid)) && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    for pid in pids.iter().filter(|pid| test_child::pid_alive(**pid)) {
        test_child::signal_process(*pid, TestSignal::Kill);
    }
    pids.len()
}

#[cfg(test)]
#[path = "reap_tests.rs"]
mod tests;
