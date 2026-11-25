//! macOS command configuration.

use std::process::Command;

/// macOS has no console-window flash to suppress.
pub fn suppress_console(_command: &mut Command) {}

/// Put the child in its own process group so a tree kill can target the
/// whole group with one signal.
pub fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// No parent-death signal exists on macOS. Graceful termination is forwarded
/// by the cargo front door; SIGKILL of the parent cannot be caught.
pub fn configure_parent_death(_command: &mut Command) {}

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
