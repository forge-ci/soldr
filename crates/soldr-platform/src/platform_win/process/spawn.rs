//! Windows detached spawn and exec-vs-spawn mechanics.

use std::io;
use std::process::{Child, Command, ExitStatus};

/// Spawn `command` detached from the caller's console and session: its own
/// process group, no console window, and no inherited console.
pub fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    use std::os::windows::process::CommandExt;
    // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW.
    const FLAGS: u32 = 0x0000_0200 | 0x0000_0008 | 0x0800_0000;
    command.creation_flags(FLAGS);
    // soldr#3098: spawns share, staged writes exclude (Windows has no fork
    // inheritance of this kind, but the funnel stays uniform across hosts).
    let _spawn = crate::platform::process::spawn_exclusion::spawn_shared();
    command.spawn()
}

/// Map an optional log file onto the running-process daemon stdio type.
pub fn daemon_stdio(log: Option<&std::fs::File>) -> running_process::DaemonStdio<'_> {
    let Some(log) = log else {
        return running_process::DaemonStdio::default();
    };
    running_process::DaemonStdio {
        stdout: running_process::DaemonStdioSource::File(log),
        stderr: running_process::DaemonStdioSource::File(log),
    }
}

/// Windows has no exec: spawn and wait, returning the exit status.
/// The child is the rest of this command, so it gets the whole-command budget.
pub fn exec_or_status(command: &mut Command) -> io::Result<ExitStatus> {
    crate::platform::wait::status_for(command, crate::platform::wait::WHOLE_COMMAND)
}

/// Test seam for soldr#3098. Windows has no fork-to-exec window in which
/// a child inherits this process's descriptors, so `hold` is ignored; the
/// child is spawned under the shared spawn guard when `spawn_guard` is set.
pub fn spawn_holding_fork_window(
    command: &mut Command,
    _hold: std::time::Duration,
    spawn_guard: bool,
) -> io::Result<Child> {
    let _spawn = spawn_guard.then(crate::platform::process::spawn_exclusion::spawn_shared);
    command.spawn()
}
