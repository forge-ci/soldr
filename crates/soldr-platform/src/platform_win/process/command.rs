//! Windows command configuration.

use std::process::Command;

/// Windows console windows flash briefly for every spawned child; suppress
/// them for background tool invocations. No-op on other hosts.
pub fn suppress_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

/// Windows has no POSIX process groups; `taskkill /T` walks the tree
/// without one, so there is nothing to configure here.
pub fn configure_process_group(_command: &mut Command) {}

/// Standard Command children have no kill-on-close job here. The observed
/// running-process path owns containment; ordinary children rely on explicit
/// terminate_tree cleanup. Parent termination remains a containment gap.
pub fn configure_parent_death(_command: &mut Command) {}

/// Whether child process groups are available for POSIX group termination.
pub fn supports_process_groups() -> bool {
    false
}

/// Windows has no argv[0] replacement; the executable image decides the
/// program identity.
pub fn arg0(_command: &mut Command, _name: &str) {}
