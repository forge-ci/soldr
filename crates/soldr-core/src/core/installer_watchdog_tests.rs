use super::*;

const TEST_EXPLICIT_CEILING_ENV_VAR: &str = "SOLDR_TEST_INSTALLER_EXPLICIT_CEILING_SECS";
const HIGH_VOLUME_BYTES: usize = 256 * 1024;
const STDOUT_BEGIN: &str = "capture-stdout-begin:";
const STDOUT_END: &str = ":capture-stdout-end";
const STDERR_BEGIN: &str = "capture-stderr-begin:";
const STDERR_END: &str = ":capture-stderr-end";

#[test]
fn heartbeat_due_requires_both_silence_and_emission_spacing() {
    let interval = Duration::from_secs(10);
    assert!(heartbeat_due(
        Duration::from_secs(10),
        Duration::from_secs(60),
        interval
    ));
    assert!(!heartbeat_due(
        Duration::from_secs(3),
        Duration::from_secs(60),
        interval
    ));
    assert!(!heartbeat_due(
        Duration::from_secs(60),
        Duration::from_secs(3),
        interval
    ));
}

#[test]
fn heartbeat_line_names_context_elapsed_and_warns_against_killing() {
    let line = heartbeat_line(
        "rustup toolchain install 1.95.0",
        Duration::from_secs(95),
        Duration::from_secs(30),
    );
    assert!(line.contains("rustup toolchain install 1.95.0"), "{line}");
    assert!(line.contains("95s elapsed"), "{line}");
    assert!(line.contains("no output for 30s"), "{line}");
    assert!(line.contains("do not kill"), "{line}");
}

struct NeverProgress;
impl ProgressProbe for NeverProgress {
    fn made_progress(&mut self) -> bool {
        false
    }
}
struct AlwaysProgress;
impl ProgressProbe for AlwaysProgress {
    fn made_progress(&mut self) -> bool {
        true
    }
}

fn is_windows_test_host() -> bool {
    crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
}
fn unix_shell(script: &str) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", script]);
    command
}
fn steady_progress_command() -> Command {
    if is_windows_test_host() {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-Command", "1..8 | ForEach-Object { Write-Output stdout-progress; [Console]::Error.WriteLine('stderr-progress'); Start-Sleep -Milliseconds 250 }"]);
        command
    } else {
        unix_shell("i=0; while [ $i -lt 16 ]; do printf '%s\\n' stdout-progress; printf '%s\\n' stderr-progress >&2; sleep 0.15; i=$((i + 1)); done")
    }
}
fn chatty_capture_command() -> Command {
    if is_windows_test_host() {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-Command", "while ($true) { Write-Output stdout-progress; [Console]::Error.WriteLine('stderr-progress'); Start-Sleep -Milliseconds 50 }"]);
        command
    } else {
        unix_shell("while :; do printf '%s\\n' stdout-progress; printf '%s\\n' stderr-progress >&2; sleep 0.05; done")
    }
}
fn high_volume_capture_command() -> Command {
    if is_windows_test_host() {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-Command", "$out = [string]::new([char]'O', 1024); $err = [string]::new([char]'E', 1024); [Console]::Out.Write('capture-stdout-begin:'); [Console]::Error.Write('capture-stderr-begin:'); 1..256 | ForEach-Object { [Console]::Out.Write($out); [Console]::Error.Write($err) }; [Console]::Out.Write(':capture-stdout-end'); [Console]::Error.Write(':capture-stderr-end')"]);
        command
    } else {
        unix_shell("out=$(printf '%1024s' '' | tr ' ' O); err=$(printf '%1024s' '' | tr ' ' E); printf 'capture-stdout-begin:'; printf 'capture-stderr-begin:' >&2; i=0; while [ $i -lt 256 ]; do printf '%s' \"$out\"; printf '%s' \"$err\" >&2; i=$((i + 1)); done; printf ':capture-stdout-end'; printf ':capture-stderr-end' >&2")
    }
}
fn quiet_wait_command() -> Command {
    if is_windows_test_host() {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 4",
        ]);
        command
    } else {
        unix_shell("sleep 2")
    }
}
/// Deliberately outlives the Windows ten-second stall budget. Keep this
/// separate from `quiet_wait_command`: active-work tests need their short
/// successful child, whereas a true-stall fixture must not accidentally exit
/// before the platform's real watchdog threshold.
fn true_stall_command() -> Command {
    if is_windows_test_host() {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 30",
        ]);
        command
    } else {
        unix_shell("sleep 2")
    }
}
fn delayed_path_command() -> Command {
    if is_windows_test_host() {
        // A cold PowerShell 5.1 startup took over four seconds in the Server
        // Core KVM guest, consuming the fixture's six-second stall budget
        // before its intended two-second quiet wait even began. cmd.exe plus
        // loopback ping gives the same silent interval without that startup.
        let mut command = Command::new("cmd.exe");
        command.args([
            "/D",
            "/C",
            "ping -n 3 127.0.0.1 >NUL && echo C:\\toolchains\\nightly\\bin\\rustc.exe",
        ]);
        command
    } else {
        unix_shell("sleep 2; printf '%s\\n' /toolchains/nightly/bin/rustc")
    }
}
fn test_stall_timeout() -> Duration {
    if is_windows_test_host() {
        Duration::from_secs(10)
    } else {
        Duration::from_millis(500)
    }
}
/// The safety ceiling for tests whose contract is the *stall* timeout.
///
/// Five of the six callers pass this as the ceiling that must **not** fire --
/// they assert that steady or quiet-but-active work survives
/// [`test_stall_timeout`]. The ceiling's exact value is therefore not the
/// thing under test; it only has to sit far enough above the fixture's real
/// runtime that host speed cannot decide the outcome.
///
/// It used to be 5s off Windows, against a `steady_progress_command` fixture
/// that spawns `sh` and runs 16 sleep-and-print cycles -- about 2.4s of
/// nominal work, so barely 2x of headroom. soldr#2968 moved the x86_64 macOS
/// replay onto `macos-15` under Rosetta, where every one of those ~48
/// emulated process operations costs several times more: the fixture took
/// 5.895s and tripped the ceiling
/// (`category=safety-ceiling total_elapsed=5s safety_ceiling=5s`), failing
/// `steady_progress_can_outlast_the_old_deadline` for a reason that has
/// nothing to do with the watchdog's behaviour.
///
/// The host branch is gone rather than given a third arm: emulation is only
/// the slowest case we happen to have measured, and a ceiling picked to be
/// unreachable does not need to know which host it is on. A genuinely hung
/// installer is still caught -- by this ceiling at 30s, and long before that
/// by the 500ms stall timeout that these tests actually exercise.
///
/// The sixth caller, `safety_ceiling_wins_even_when_work_keeps_progressing`,
/// passes this as the *stall* timeout behind a 45ms ceiling; a larger value
/// there only sharpens its assertion that the ceiling wins.
fn test_safety_timeout() -> Duration {
    Duration::from_secs(30)
}
fn test_short_safety_timeout() -> Duration {
    if is_windows_test_host() {
        Duration::from_millis(500)
    } else {
        Duration::from_millis(45)
    }
}
fn test_capture_safety_timeout() -> Duration {
    if is_windows_test_host() {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(1)
    }
}
fn wait_for_test_child<P: ProgressProbe>(
    mut command: Command,
    config: InstallerWatchdogConfig,
    probe: P,
) -> Result<ExitStatus, SoldrError> {
    let mut child = command.spawn().unwrap();
    let (_sender, receiver) = mpsc::channel();
    wait_for_child_with_watchdog(
        &mut child,
        "test installer",
        "test",
        &config,
        &receiver,
        probe,
    )
}

#[test]
fn steady_progress_can_outlast_the_old_deadline() {
    let mut command = steady_progress_command();
    assert!(run_installer_command(
        &mut command,
        "test installer",
        "test",
        InstallerWatchdogConfig::for_test(test_stall_timeout(), test_safety_timeout())
    )
    .unwrap()
    .success());
}
#[test]
fn captured_manager_lookup_can_wait_past_the_short_command_silence_budget() {
    let _lock = crate::test_util::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prior = std::env::var_os(super::super::COMMAND_OUTPUT_TIMEOUT_ENV_VAR);
    std::env::set_var(super::super::COMMAND_OUTPUT_TIMEOUT_ENV_VAR, "1");
    let mut command = delayed_path_command();
    let started = Instant::now();
    let output = run_installer_command_output(
        &mut command,
        "manager lookup",
        "manager-which",
        InstallerWatchdogConfig::for_test(Duration::from_secs(10), test_safety_timeout()),
    )
    .expect("installer watchdog must not apply the generic one-second silence budget");
    match prior {
        Some(value) => std::env::set_var(super::super::COMMAND_OUTPUT_TIMEOUT_ENV_VAR, value),
        None => std::env::remove_var(super::super::COMMAND_OUTPUT_TIMEOUT_ENV_VAR),
    }
    assert!(output.status.success(), "{output:?}");
    assert!(
        started.elapsed() > Duration::from_secs(1),
        "fixture did not outlast the generic one-second silence budget"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("rustc"),
        "captured lookup output was lost: {output:?}"
    );
}
#[test]
fn captured_high_volume_dual_stream_drains_both_pipes() {
    let mut command = high_volume_capture_command();
    let output = run_installer_command_output(
        &mut command,
        "high-volume capture",
        "manager-which",
        InstallerWatchdogConfig::for_test(Duration::from_secs(10), test_safety_timeout()),
    )
    .expect("both capture pipes must drain beyond pipe capacity");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout must be UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr must be UTF-8");
    let stdout_payload = stdout
        .strip_prefix(STDOUT_BEGIN)
        .and_then(|value| value.strip_suffix(STDOUT_END))
        .expect("complete distinct stdout markers");
    let stderr_payload = stderr
        .strip_prefix(STDERR_BEGIN)
        .and_then(|value| value.strip_suffix(STDERR_END))
        .expect("complete distinct stderr markers");
    assert_eq!(stdout_payload.len(), HIGH_VOLUME_BYTES);
    assert!(stdout_payload.bytes().all(|byte| byte == b'O'));
    assert_eq!(stderr_payload.len(), HIGH_VOLUME_BYTES);
    assert!(stderr_payload.bytes().all(|byte| byte == b'E'));
}
#[test]
fn captured_chatty_command_reaches_the_safety_ceiling_and_is_reaped() {
    let safety_timeout = test_capture_safety_timeout();
    let mut command = chatty_capture_command();
    let started = Instant::now();
    let error = run_installer_command_output(
        &mut command,
        "chatty capture",
        "manager-which",
        InstallerWatchdogConfig::for_test(Duration::from_secs(10), safety_timeout),
    )
    .expect_err("the safety ceiling must stop even a capture command that keeps both pipes busy");
    assert!(started.elapsed() < safety_timeout + Duration::from_secs(3), "the watchdog must kill and reap the chatty capture command within its bounded safety ceiling");
    let message = error.to_string();
    assert!(message.contains("category=safety-ceiling"), "{message}");
    assert!(message.contains("phase=manager-which"), "{message}");
    assert!(message.contains("killed installer process"), "{message}");
}
#[test]
fn a_true_stall_reports_category_phase_and_elapsed_times() {
    let error = wait_for_test_child(
        true_stall_command(),
        InstallerWatchdogConfig::for_test(test_stall_timeout(), test_safety_timeout()),
        NeverProgress,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("category=stall"), "{error}");
    assert!(error.contains("phase=test"), "{error}");
    assert!(error.contains("total_elapsed="), "{error}");
    assert!(error.contains("since_progress="), "{error}");
}
#[test]
fn quiet_active_work_does_not_false_stall() {
    assert!(wait_for_test_child(
        quiet_wait_command(),
        InstallerWatchdogConfig::for_test(test_stall_timeout(), test_safety_timeout()),
        AlwaysProgress
    )
    .unwrap()
    .success());
}
#[test]
fn quiet_cpu_active_work_resets_the_watchdog() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
        return;
    }
    let mut command = unix_shell("yes >/dev/null & worker=$!; sleep 0.2; kill \"$worker\"; wait \"$worker\" 2>/dev/null || true");
    assert!(run_installer_command(
        &mut command,
        "quiet compiler simulation",
        "compile",
        InstallerWatchdogConfig::for_test(test_stall_timeout(), test_safety_timeout())
    )
    .unwrap()
    .success());
}
#[test]
fn watchdog_terminates_the_installer_process_group() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let descendant_pid_file = temp.path().join("descendant.pid");
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "sleep 30 & echo $! > \"$1\"; sleep 30",
            "watchdog-test",
        ])
        .arg(&descendant_pid_file);
    let error = run_installer_command(
        &mut command,
        "process tree test",
        "test",
        InstallerWatchdogConfig::for_test(Duration::from_millis(100), Duration::from_secs(1)),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("category=stall"), "{error}");
    let descendant_pid = std::fs::read_to_string(&descendant_pid_file)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    for _ in 0..50 {
        if !std::path::Path::new("/proc")
            .join(descendant_pid.to_string())
            .exists()
        {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("descendant {descendant_pid} survived the watchdog process-group kill");
}
#[test]
fn safety_ceiling_wins_even_when_work_keeps_progressing() {
    let error = wait_for_test_child(
        quiet_wait_command(),
        InstallerWatchdogConfig::for_test(test_safety_timeout(), test_short_safety_timeout()),
        AlwaysProgress,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("category=safety-ceiling"), "{error}");
}
#[test]
fn shared_safety_ceiling_applies_without_an_explicit_operation_limit() {
    let _lock = crate::test_util::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous_shared = std::env::var_os(INSTALLER_SAFETY_TIMEOUT_ENV_VAR);
    let previous_explicit = std::env::var_os(TEST_EXPLICIT_CEILING_ENV_VAR);
    unsafe {
        std::env::set_var(INSTALLER_SAFETY_TIMEOUT_ENV_VAR, "123");
        std::env::remove_var(TEST_EXPLICIT_CEILING_ENV_VAR);
    }
    assert_eq!(
        InstallerWatchdogConfig::from_env(TEST_EXPLICIT_CEILING_ENV_VAR).safety_timeout,
        Duration::from_secs(123)
    );
    unsafe {
        match previous_shared {
            Some(value) => std::env::set_var(INSTALLER_SAFETY_TIMEOUT_ENV_VAR, value),
            None => std::env::remove_var(INSTALLER_SAFETY_TIMEOUT_ENV_VAR),
        }
        match previous_explicit {
            Some(value) => std::env::set_var(TEST_EXPLICIT_CEILING_ENV_VAR, value),
            None => std::env::remove_var(TEST_EXPLICIT_CEILING_ENV_VAR),
        }
    }
}
#[test]
fn an_ordinary_run_still_writes_child_stdout_to_stdout() {
    assert_eq!(child_stdout_route(false, false), ChildStdoutRoute::Stdout);
}
#[test]
fn a_payload_stdout_relocates_the_child_rather_than_dropping_it() {
    assert_eq!(child_stdout_route(false, true), ChildStdoutRoute::Stderr);
}
#[test]
fn suppression_wins_over_relocation() {
    assert_eq!(child_stdout_route(true, true), ChildStdoutRoute::Discard);
    assert_eq!(child_stdout_route(true, false), ChildStdoutRoute::Discard);
}
#[test]
fn the_payload_route_never_returns_stdout() {
    for suppress in [true, false] {
        assert_ne!(
            child_stdout_route(suppress, true),
            ChildStdoutRoute::Stdout,
            "suppress={suppress}"
        );
    }
}
