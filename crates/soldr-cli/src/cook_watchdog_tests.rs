use super::*;

// ---------------------------------------------------------------------
// Pure decision logic: no real sleeping, no real filesystem.
// ---------------------------------------------------------------------

#[test]
fn cook_watchdog_fires_after_n_seconds_of_no_progress() {
    let timeout = Duration::from_secs(900);
    assert!(!timed_out(Duration::from_secs(899), timeout));
    assert!(timed_out(Duration::from_secs(900), timeout));
    assert!(timed_out(Duration::from_secs(1_000), timeout));
}

#[test]
fn cook_watchdog_resets_on_progress() {
    // Simulate the driver loop's bookkeeping: each time `progressed`
    // reports a change, the caller resets its "since progress" clock to
    // zero, so the timeout can never fire on that tick.
    let previous_marker = 100_u64;
    let current_marker = 101_u64;
    assert!(progressed(previous_marker, current_marker));

    let same_marker = 101_u64;
    assert!(!progressed(current_marker, same_marker));
}

#[test]
fn cook_watchdog_disabled_when_timeout_is_zero() {
    std::env::remove_var(NO_PROGRESS_ENV_VAR);
    std::env::set_var(NO_PROGRESS_ENV_VAR, "0");
    let resolved = no_progress_timeout_from_env();
    std::env::remove_var(NO_PROGRESS_ENV_VAR);
    assert_eq!(resolved, None);
}

#[test]
fn cook_watchdog_unset_env_uses_default() {
    std::env::remove_var(NO_PROGRESS_ENV_VAR);
    let resolved = no_progress_timeout_from_env();
    assert_eq!(
        resolved,
        Some(Duration::from_secs(DEFAULT_NO_PROGRESS_SECS))
    );
}

#[test]
fn cook_watchdog_explicit_positive_value_overrides_default() {
    std::env::remove_var(NO_PROGRESS_ENV_VAR);
    std::env::set_var(NO_PROGRESS_ENV_VAR, "42");
    let resolved = no_progress_timeout_from_env();
    std::env::remove_var(NO_PROGRESS_ENV_VAR);
    assert_eq!(resolved, Some(Duration::from_secs(42)));
}

#[test]
fn cook_watchdog_heartbeat_cadence_fires_as_expected() {
    let interval = Duration::from_secs(300);
    assert!(!heartbeat_due(Duration::from_secs(299), interval));
    assert!(heartbeat_due(Duration::from_secs(300), interval));
    assert!(heartbeat_due(Duration::from_secs(600), interval));
}

#[test]
fn cook_watchdog_config_from_env_disabled_short_circuits() {
    let config = WatchdogConfig::disabled();
    assert!(config.no_progress_timeout.is_none());
}

#[test]
fn cook_watchdog_config_with_timeout_is_armed() {
    let config = WatchdogConfig::with_timeout(Duration::from_secs(10));
    assert_eq!(config.no_progress_timeout, Some(Duration::from_secs(10)));
}

// ---------------------------------------------------------------------
// Fake progress probe: dependency-injected instead of touching the
// filesystem or real wall-clock time.
// ---------------------------------------------------------------------

struct FakeProbe {
    values: std::collections::VecDeque<u64>,
    last: u64,
}

impl FakeProbe {
    fn new(values: Vec<u64>) -> Self {
        Self {
            values: values.into(),
            last: 0,
        }
    }
}

impl ProgressProbe for FakeProbe {
    fn sample(&mut self) -> u64 {
        if let Some(next) = self.values.pop_front() {
            self.last = next;
        }
        self.last
    }
}

#[test]
fn fake_probe_reports_progress_transitions() {
    let mut probe = FakeProbe::new(vec![1, 1, 2, 2, 3]);
    let mut previous = probe.sample(); // 1
    assert!(!progressed(previous, probe.sample())); // 1 -> 1
    let current = probe.sample(); // 2
    assert!(progressed(previous, current));
    previous = current;
    assert!(!progressed(previous, probe.sample())); // 2 -> 2
}

#[test]
fn stall_dump_dirname_is_unique_enough_across_calls() {
    let first = stall_dump_dirname();
    std::thread::sleep(Duration::from_millis(2));
    let second = stall_dump_dirname();
    assert!(first.starts_with("cook-stall-"));
    assert_ne!(first, second);
}

#[test]
fn target_dir_probe_detects_new_artifact_mtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deps = dir.path().join("debug").join("deps");
    std::fs::create_dir_all(&deps).expect("mkdir");

    let mut probe = TargetDirProbe::new(dir.path().to_path_buf());
    let before = probe.sample();

    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(deps.join("libfoo.rlib"), b"stub").expect("write artifact");

    let after = probe.sample();
    assert!(
        progressed(before, after),
        "expected a fresh artifact to move the newest-mtime marker: before={before} after={after}"
    );
}

// ---------------------------------------------------------------------
// Real async driver: fires on a synthetic hang, resets on synthetic
// progress. Uses tokio's paused/auto-advancing time so no test sleeps in
// real wall-clock time.
// ---------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn run_with_watchdog_returns_ok_when_disabled_even_if_slow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = crate::core::SoldrPaths::with_root(dir.path().to_path_buf());
    let config = WatchdogConfig::disabled();

    let result: Result<i32, SoldrError> =
        run_with_watchdog("prepare", dir.path().to_path_buf(), &paths, config, async {
            tokio::time::sleep(Duration::from_secs(10_000)).await;
            Ok(7)
        })
        .await;

    assert_eq!(result.expect("disabled watchdog must not intervene"), 7);
}

#[tokio::test(start_paused = true)]
async fn run_with_watchdog_passes_through_fast_success() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = crate::core::SoldrPaths::with_root(dir.path().to_path_buf());
    let config = WatchdogConfig::with_timeout(Duration::from_secs(900));

    let result: Result<i32, SoldrError> =
        run_with_watchdog("cook", dir.path().to_path_buf(), &paths, config, async {
            Ok(42)
        })
        .await;

    assert_eq!(result.expect("fast future must pass through"), 42);
}

#[tokio::test(start_paused = true)]
async fn run_with_watchdog_times_out_on_genuine_silence() {
    let dir = tempfile::tempdir().expect("tempdir");
    // No activity is ever written under `dir`, so every 30s sample reports
    // the same (zero) marker and the watchdog must fire once the no-progress
    // timeout elapses.
    let paths = crate::core::SoldrPaths::with_root(dir.path().to_path_buf());
    let config = WatchdogConfig::with_timeout(Duration::from_secs(120));

    let result: Result<i32, SoldrError> =
        run_with_watchdog("cook", dir.path().to_path_buf(), &paths, config, async {
            // Never resolves within the test's paused-time budget.
            std::future::pending::<()>().await;
            Ok(0)
        })
        .await;

    let err = result.expect_err("silent future must trip the watchdog");
    let message = err.to_string();
    assert!(
        message.contains("no progress for 120s"),
        "unexpected message: {message}"
    );
    assert!(
        message.contains("stack dump at"),
        "unexpected message: {message}"
    );
}

/// Stall integration test: spawns a real "cook child" (a long-sleeping
/// process, via `running_process` -- never a raw `std::process::Command`
/// spawn) with no target-directory activity, then asserts the watchdog (a)
/// writes a dump directory naming that child's pid/cmdline and (b)
/// terminates it.
#[tokio::test(start_paused = true)]
async fn run_with_watchdog_stall_integration_dumps_and_kills_real_child() {
    use running_process::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};

    let dir = tempfile::tempdir().expect("tempdir");
    let paths = crate::core::SoldrPaths::with_root(dir.path().to_path_buf());
    let config = WatchdogConfig::with_timeout(Duration::from_secs(120));

    let child_config = ProcessConfig {
        command: CommandSpec::Argv(vec!["sleep".to_string(), "300".to_string()]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    };
    let child = NativeProcess::new(child_config);
    child.start().expect("spawn fake cook child (sleep)");
    let child_pid = child.pid().expect("child pid");

    let result: Result<(), SoldrError> =
        run_with_watchdog("cook", dir.path().to_path_buf(), &paths, config, async {
            std::future::pending::<()>().await;
            Ok(())
        })
        .await;

    let err = result.expect_err("no activity anywhere must trip the watchdog");
    let message = err.to_string();
    let dump_dir = message
        .rsplit("stack dump at ")
        .next()
        .expect("message names the dump dir")
        .trim();

    let descendants = std::fs::read_to_string(format!("{dump_dir}/cook-descendants.txt"))
        .expect("cook-descendants.txt written");
    assert!(
        descendants.contains(&child_pid.to_string()),
        "expected child pid {child_pid} in dump:\n{descendants}"
    );
    assert!(
        descendants.contains("sleep"),
        "expected child cmdline in dump:\n{descendants}"
    );

    // The watchdog terminates the descendant tree before returning; give the
    // signal a moment to land, then confirm the process is gone.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !kernal_api::platform::process::is_alive(child_pid),
        "expected the sleep child to have been terminated"
    );

    let _ = child.wait(Some(Duration::from_secs(2)));
    let _ = child.close();
}
