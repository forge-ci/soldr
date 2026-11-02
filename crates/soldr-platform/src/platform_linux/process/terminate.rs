//! Linux process termination.

use std::io;
use std::process::Child;
use std::time::Duration;

/// How a tree kill was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeKill {
    /// The whole tree was killed (`taskkill /T /F` or the signal group).
    TreeKilled,
    /// The tree kill failed; the direct child was killed instead.
    ProcessKilled,
}

/// Kill `child`'s whole process group (SIGTERM, then SIGKILL after a short
/// grace), falling back to killing the child itself.
pub fn terminate_tree(child: &mut Child) -> io::Result<TreeKill> {
    let pgid = child.id() as libc::pid_t;
    let term_result = signal_process_group(pgid, libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(100));
    let kill_result = signal_process_group(pgid, libc::SIGKILL);
    if term_result.is_ok() || kill_result.is_ok() {
        return Ok(TreeKill::TreeKilled);
    }
    child.kill()?;
    Ok(TreeKill::ProcessKilled)
}

/// Terminate a single PID with SIGTERM.
pub fn terminate_pid(pid: u32) {
    let _ = signal_pid(pid, false);
}

/// Signal a single PID: SIGTERM, or SIGKILL when `force` is set. A pid that
/// is already gone (ESRCH) counts as success.
pub fn signal_pid(pid: u32, force: bool) -> io::Result<()> {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    // Refuse anything `pid_t` cannot represent as a single process, BEFORE
    // the syscall. `pid_t` is signed, so a `u32` above `i32::MAX` arrives at
    // `kill` as a negative number, and negative arguments do not mean "that
    // process": -N signals process group N, and -1 signals every process the
    // caller may signal. `signal_pid(4294967295, true)` would therefore
    // SIGKILL the user's entire session rather than one stale pid. Real
    // Linux and macOS pids never come near that range, but these values
    // arrive from pid files and environment variables, not only from
    // `Child::id()`.
    let target = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0);
    let Some(target) = target else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to signal pid {pid}: not a single-process id"),
        ));
    };
    // SAFETY: kill(2) on a pid the caller captured. ESRCH — the process
    // exited between the probe and the signal — is success for a
    // terminate request.
    let rc = unsafe { libc::kill(target, signal) };
    if rc == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error)
}

fn signal_process_group(pgid: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: `pgid` is the spawned child's PID after `process_group(0)`.
    // Negating it asks the kernel to signal every member of the group.
    let rc = unsafe { libc::kill(-pgid, signal) };
    if rc == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn signal_an_unallocated_pid_is_success_not_error() {
        // PID 999999 is effectively unallocated; ESRCH must map to Ok.
        signal_pid(999_999, false).expect("esrch is success");
    }

    #[test]
    fn terminate_tree_reports_esrch_group_as_killed() {
        // A child that already exited: the group is gone, so killpg fails
        // with ESRCH which signal_process_group treats as success.
        let mut child = Command::new("true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn true");
        child.wait().expect("reap true");
        assert_eq!(
            terminate_tree(&mut child).expect("tree kill"),
            TreeKill::TreeKilled
        );
    }
}

#[cfg(test)]
mod out_of_range_pid_tests {
    use super::*;

    /// `pid_t` is signed. Without a range check, `kill(4294967295, SIGKILL)`
    /// is `kill(-1, SIGKILL)`, which the kernel reads as "every process this
    /// user may signal". A stale pid file or a mistyped environment variable
    /// would end the user's session. The value must be rejected before the
    /// syscall, not merely reported afterwards.
    #[test]
    fn a_pid_outside_pid_t_is_refused_without_signalling_anything() {
        for pid in [u32::MAX, i32::MAX as u32 + 1, 0] {
            let error = signal_pid(pid, true).expect_err("must not reach kill(2)");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    /// The companion probe answers the same way: a pid the kernel cannot name
    /// is not alive. Left unguarded it returns `true`, because `kill(-1, 0)`
    /// succeeds whenever the caller can signal anything at all.
    #[test]
    fn a_pid_outside_pid_t_is_not_alive() {
        assert!(!crate::platform_linux::process::inspect::is_alive(u32::MAX));
        assert!(!crate::platform_linux::process::inspect::is_alive(0));
    }
}
