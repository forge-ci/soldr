//! Linux command configuration.

use std::process::Command;

/// Linux has no console-window flash to suppress.
pub fn suppress_console(_command: &mut Command) {}

/// Put the child in its own process group so a tree kill can target the
/// whole group with one signal.
pub fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// Kill the child if its spawning parent dies, even across process groups.
pub fn configure_parent_death(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    let parent = std::process::id();
    // SAFETY: only async-signal-safe syscalls run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The parent may have died before prctl armed the signal.
            if libc::getppid() as u32 != parent {
                libc::_exit(127);
            }
            Ok(())
        });
    }
}

/// Whether child process groups are available for POSIX group termination.
pub fn supports_process_groups() -> bool {
    true
}

/// Replace argv[0] so the child reports the multicall name it was invoked
/// as rather than the physical binary.
pub fn arg0(command: &mut Command, name: &str) {
    use std::os::unix::process::CommandExt;
    command.arg0(name);
}

#[cfg(test)]
#[path = "command_wait_tests.rs"]
mod wait_tests;
