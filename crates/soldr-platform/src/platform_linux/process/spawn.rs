//! Linux detached spawn and exec mechanics.

use std::io;
use std::process::{Child, Command, ExitStatus};

/// Spawn `command` in its own process group so a tree kill can target the
/// whole group. (Full session detachment for the daemon is handled by
/// `running-process`, which owns process creation.)
pub fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    // soldr#3098: spawns share, staged writes exclude.
    let _spawn = crate::platform::process::spawn_exclusion::spawn_shared();
    command.spawn()
}

/// Test seam for soldr#3098: spawn `command` but hold the child between
/// fork and exec for `hold`, so any descriptor it inherited from this
/// process stays open that long. With `spawn_guard` it takes the shared
/// spawn guard like every other funnel, so a staged write in progress makes
/// this block first; without it, it models a spawner outside soldr's lock
/// (soldr#3350).
pub fn spawn_holding_fork_window(
    command: &mut Command,
    hold: std::time::Duration,
    spawn_guard: bool,
) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure only sleeps, which is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            std::thread::sleep(hold);
            Ok(())
        });
    }
    let _spawn = spawn_guard.then(crate::platform::process::spawn_exclusion::spawn_shared);
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

/// Replace the current process image with `command` (exec). On success this
/// never returns; on failure it returns the exec error.
pub fn exec_or_status(command: &mut Command) -> io::Result<ExitStatus> {
    use std::os::unix::process::CommandExt;
    Err(command.exec())
}

/// Build the command that launches a shared daemon `binary` (soldr#4012).
///
/// A shared daemon serves every session on the host, but a plain spawn leaves
/// it in the cgroup of whichever session happened to start it, so that one
/// session's `memory.max` caps everyone's compiles. When `own_scope` is set,
/// this process sits under a finite memory limit, and a user systemd manager
/// answers, the daemon is started through `systemd-run --user --scope`: the
/// scope is registered for `systemd-run`'s own PID, which then execs the
/// daemon, so the PID and the parent (the reaping broker) are unchanged.
/// Otherwise the daemon is spawned directly, exactly as before.
pub fn daemon_command(binary: &std::path::Path, unit: &str, own_scope: bool) -> Command {
    let wrapper = own_scope
        .then(crate::platform::host::resources::HostResourceSnapshot::capture)
        .filter(|snapshot| snapshot.cgroup_limit_bytes.is_some())
        .and_then(|_| user_scope_launcher());
    let Some(systemd_run) = wrapper else {
        return Command::new(binary);
    };
    let mut command = Command::new(systemd_run);
    command.args(scope_args(binary, unit));
    // The daemon is spawned from a baseline environment; systemd-run needs
    // the user bus, so carry its coordinates explicitly.
    for key in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
}

fn scope_args(binary: &std::path::Path, unit: &str) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = [
        "--user",
        "--scope",
        "--quiet",
        "--collect",
        "--slice=app.slice",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    args.push(format!("--unit={}", scope_unit_name(unit)).into());
    args.push("--".into());
    args.push(binary.as_os_str().to_owned());
    args
}

/// A unique, systemd-legal unit name: unit names allow `[A-Za-z0-9:_.\-]`.
fn scope_unit_name(unit: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let safe: String = unit
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!(
        "soldr-daemon-{safe}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// `systemd-run`, resolved once, only when a throwaway user scope can
/// actually be created here (no user manager in containers or CI images).
fn user_scope_launcher() -> Option<std::path::PathBuf> {
    static LAUNCHER: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    LAUNCHER
        .get_or_init(|| {
            let path = std::env::var_os("PATH")?;
            let candidate = std::env::split_paths(&path)
                .map(|dir| dir.join("systemd-run"))
                .find(|candidate| candidate.is_file())?;
            // Output is inherited: in the broker it lands in the broker log,
            // which is where a failed probe should be explained.
            let mut probe = Command::new(&candidate);
            probe
                .args(["--user", "--scope", "--quiet", "--collect", "--", "true"])
                .stdin(std::process::Stdio::null());
            let probe =
                crate::platform::wait::status_for(&mut probe, std::time::Duration::from_secs(10))
                    .ok()?;
            probe.success().then_some(candidate)
        })
        .clone()
}

#[cfg(test)]
mod daemon_scope_tests {
    use super::*;

    #[test]
    fn opting_out_spawns_the_daemon_directly() {
        let command = daemon_command(std::path::Path::new("/opt/soldr-daemon"), "route", false);
        assert_eq!(command.get_program(), "/opt/soldr-daemon");
        assert_eq!(command.get_args().count(), 0);
    }

    #[test]
    fn scope_args_exec_the_daemon_in_a_fresh_user_scope() {
        let args = scope_args(std::path::Path::new("/opt/soldr-daemon"), "soldr v0.9/abc");
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            &args[..5],
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--slice=app.slice"
            ]
        );
        assert!(
            args[5].starts_with("--unit=soldr-daemon-soldr-v0.9-abc-"),
            "{}",
            args[5]
        );
        assert_eq!(&args[6..], ["--", "/opt/soldr-daemon"]);
    }

    #[test]
    fn unit_names_are_unique_and_systemd_legal() {
        let first = scope_unit_name("a/b c");
        let second = scope_unit_name("a/b c");
        assert_ne!(first, second);
        assert!(first
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')));
    }
}
