//! Integration coverage for `soldr daemon start --foreground` /
//! `soldr daemon status` / `soldr daemon stop`. Verifies the daemon
//! comes up, answers status, and shuts down cleanly. Retired daemons
//! deliberately leave stale endpoint claims for the next root owner
//! to reclaim.

#![allow(clippy::print_stdout)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::common;
use serde_json::Value;
use soldr_cli::core::SoldrPaths;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

fn isolated_env(cache_root: &Path, home_root: &Path) -> Vec<(&'static str, OsString)> {
    vec![
        ("SOLDR_CACHE_DIR", cache_root.as_os_str().to_os_string()),
        ("HOME", home_root.as_os_str().to_os_string()),
        ("USERPROFILE", home_root.as_os_str().to_os_string()),
    ]
}

fn scrub_outer_soldr_runtime(cmd: &mut Command) {
    common::scrub_outer_soldr_env(cmd);
}

fn wait_for_ready(cache_root: &Path, home_root: &Path, deadline: Instant) -> bool {
    // The protobuf route claim is published before the daemon is fully ready,
    // so require the broker-routed status request to answer too.
    let route_claim = cache_root
        .join("cache")
        .join("soldr-daemon")
        .join("broker-route-claim.pb");
    while Instant::now() < deadline {
        if route_claim.exists() && status_reports_running(cache_root, home_root) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn status_reports_running(cache_root: &Path, home_root: &Path) -> bool {
    let out = run_soldr(&["daemon", "status", "--json"], cache_root, home_root);
    if !out.status.success() {
        return false;
    }
    serde_json::from_slice::<Value>(&out.stdout)
        .ok()
        .and_then(|body| body["running"].as_bool())
        .unwrap_or(false)
}

fn run_soldr(args: &[&str], cache_root: &Path, home_root: &Path) -> std::process::Output {
    let mut cmd = Command::new(common::soldr_bin());
    scrub_outer_soldr_runtime(&mut cmd);
    cmd.args(args);
    for (k, v) in isolated_env(cache_root, home_root) {
        cmd.env(k, v);
    }
    cmd.env(
        soldr_cli::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR,
        common::isolated_daemon::isolated_daemon_executable(
            &common::soldr_daemon_bin(),
            cache_root,
        ),
    );
    cmd.output().expect("failed to run soldr")
}

fn run_soldr_with_timeout(
    args: &[&str],
    cache_root: &Path,
    home_root: &Path,
    current_dir: &Path,
    timeout: Duration,
) -> std::process::Output {
    let mut cmd = Command::new(common::soldr_bin());
    scrub_outer_soldr_runtime(&mut cmd);
    cmd.args(args).current_dir(current_dir);
    for (k, v) in isolated_env(cache_root, home_root) {
        cmd.env(k, v);
    }
    cmd.env(
        soldr_cli::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR,
        common::isolated_daemon::isolated_daemon_executable(
            &common::soldr_daemon_bin(),
            cache_root,
        ),
    );
    cmd.env("SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS", "10000");
    cmd.env("SOLDR_COMPILE_REPLY_TIMEOUT_SECS", "60");
    // soldr#2571: the wedge these timeouts catch produced zero bytes on both
    // streams, so the panic below had nothing to report beyond "15s passed".
    // With the trace on, the captured stderr ends at the last startup phase the
    // child completed — which is the diagnosis.
    cmd.env(soldr_cli::startup_trace::STARTUP_TRACE_ENV_VAR, "1");

    // soldr#3197: both pipes drain from spawn. This used to wait first and read
    // after exit, so a child whose output passed the 64 KB pipe buffer blocked
    // in `write(2)` and was reported here as a timeout, output already sent.
    let output = common::tracked_child::spawn_tracked(&mut cmd)
        .expect("failed to spawn soldr")
        .wait_bounded(timeout);
    if output.timed_out {
        panic!(
            "soldr {:?} timed out after {:?} ({})\nstdout:\n{}\nstderr:\n{}\n{}",
            args,
            timeout,
            output.disposition(),
            output.stdout_lossy(),
            output.stderr_lossy(),
            spawn_log_forensics(cache_root, home_root),
        );
    }
    output.into_output()
}

/// The timed-out child often dies before its first byte of output
/// (soldr#2571: `doctor --json` at 15s with both streams empty), so the
/// only evidence lives in the fixture's spawn logs. Collect their tails
/// into the panic message instead of leaving the next occurrence
/// undiagnosable.
fn spawn_log_forensics(cache_root: &Path, home_root: &Path) -> String {
    let mut report = String::from("spawn-log forensics:\n");
    for (label, path) in [
        (
            "broker-spawn.log",
            home_root
                .join(".soldr")
                .join("broker")
                .join("broker-spawn.log"),
        ),
        ("daemon-spawn.log", cache_root.join("daemon-spawn.log")),
    ] {
        match fs::read_to_string(&path) {
            Ok(content) => {
                // Keep the tail: startup noise ages out, the wedge is recent.
                let tail_start = content.len().saturating_sub(4096);
                let tail = &content[content
                    .char_indices()
                    .map(|(i, _)| i)
                    .find(|&i| i >= tail_start)
                    .unwrap_or(0)..];
                report.push_str(&format!("--- {label} ({}):\n{tail}\n", path.display()));
            }
            Err(err) => {
                report.push_str(&format!(
                    "--- {label} ({}): unreadable: {err}\n",
                    path.display()
                ));
            }
        }
    }
    report
}

struct Daemon {
    cache_root: PathBuf,
    home_root: PathBuf,
}

impl Daemon {
    fn spawn() -> Self {
        let cache_root = unique_temp_dir("daemon-lifecycle-cache");
        let home_root = unique_temp_dir("daemon-lifecycle-home");
        let start = run_soldr(&["daemon", "start"], &cache_root, &home_root);
        assert!(
            start.status.success(),
            "broker-owned daemon start failed: stdout={}; stderr={}",
            String::from_utf8_lossy(&start.stdout),
            String::from_utf8_lossy(&start.stderr)
        );
        // A cold embedded-zccache initialization has a separate 45-second
        // broker route-readiness bound. Keep this post-start fixture wait
        // bounded, but do not misclassify that cold start as a multicall failure.
        let deadline = Instant::now() + Duration::from_secs(40);
        assert!(
            wait_for_ready(&cache_root, &home_root, deadline),
            "daemon never opened its endpoint under {}",
            cache_root.display()
        );
        Self {
            cache_root,
            home_root,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = run_soldr(&["daemon", "stop"], &self.cache_root, &self.home_root);
        let _ = run_soldr(&["broker", "stop"], &self.cache_root, &self.home_root);
    }
}

// Only the `#[cfg(windows)]` herd-spawning regression test below constructs
// this; without the gate it is dead code on non-Windows targets (-D warnings).
struct DaemonCleanup {
    cache_root: PathBuf,
    home_root: PathBuf,
}

impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        let _ = run_soldr(&["daemon", "stop"], &self.cache_root, &self.home_root);
        // The broker outlives `daemon stop` by design (soldr#2549); stop it
        // too or the fixture leaks one detached broker per run (soldr#2568).
        let _ = run_soldr(&["broker", "stop"], &self.cache_root, &self.home_root);
    }
}

fn process_is_alive(pid: u32) -> bool {
    soldr_platform::process::inspect::is_alive(pid)
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    !process_is_alive(pid)
}

struct DetachedDaemonCleanup {
    cache_root: PathBuf,
    home_root: PathBuf,
}

#[test]
fn windows_stop_start_is_immediately_status_ready() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let cache_root = unique_temp_dir("daemon-restart-readiness-cache");
    let home_root = unique_temp_dir("daemon-restart-readiness-home");
    let workspace = unique_temp_dir("daemon-restart-readiness-workspace");
    let _cleanup = DaemonCleanup {
        cache_root: cache_root.clone(),
        home_root: home_root.clone(),
    };

    // soldr#2883: this must exceed soldr's own route-start budget, or the
    // wrapper fires first with a bare "timed out after 90s" and swallows the
    // launcher's more precise attribution -- the exact failure mode that
    // budget's comment describes for a too-tight client bound.
    //
    // 90s was safe while the route budget was 60s, because it could never be
    // reached. Once the budget moved to 180s to stop clipping real work on
    // windows-gnu, 90s became the binding bound: the very next run passed at
    // 89.786s, 0.2s inside it. So the three nest deliberately now --
    // route 180s < this 240s < nextest's 300s tier for this test.
    const DAEMON_START_WAIT: Duration = Duration::from_secs(240);
    let mut generations = Vec::new();
    for (args, timeout) in [
        (&["daemon", "start"][..], DAEMON_START_WAIT),
        (&["daemon", "status", "--json"][..], Duration::from_secs(15)),
        (&["daemon", "stop"][..], Duration::from_secs(15)),
        (&["daemon", "start"][..], DAEMON_START_WAIT),
        (&["daemon", "status", "--json"][..], Duration::from_secs(15)),
    ] {
        let output = run_soldr_with_timeout(args, &cache_root, &home_root, &workspace, timeout);
        assert!(
            output.status.success(),
            "soldr {args:?} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        if args.get(1) == Some(&"status") {
            let body: Value = serde_json::from_slice(&output.stdout).expect("status JSON");
            assert_eq!(body["running"].as_bool(), Some(true));
            generations.push(body["generation"].as_u64().expect("status generation"));
        }
    }
    assert_eq!(generations.len(), 2);
    assert_ne!(
        generations[0], generations[1],
        "restart must replace generation"
    );
    let paths = SoldrPaths::with_root(cache_root.clone());
    // soldr#3823: read under the daemon's generation key (soldr#3374).
    let claim = common::route_claim::with_published_generation(&cache_root, || {
        soldr_cli::daemon::backend_handle_adoption::read_broker_route_claim(&paths)
            .ok()
            .flatten()
    })
    .expect("route claim");
    assert_eq!(claim.started_at_unix_ms, generations[1]);
}

impl Drop for DetachedDaemonCleanup {
    fn drop(&mut self) {
        // soldr#3823: read under the daemon's generation key (soldr#3374).
        let paths = SoldrPaths::with_root(self.cache_root.clone());
        let pid = common::route_claim::with_published_generation(&self.cache_root, || {
            soldr_cli::daemon::lifecycle::read_route_claim_identity(&paths)
        })
        .map(|(pid, _)| pid);
        let _ = run_soldr(&["daemon", "stop"], &self.cache_root, &self.home_root);
        if let Some(pid) = pid {
            let _ = wait_for_process_exit(pid, Duration::from_secs(5));
        }
        // Stop the fixture broker before deleting its install dir out from
        // under it (soldr#2549 keeps it alive past `daemon stop`; soldr#2568).
        let _ = run_soldr(&["broker", "stop"], &self.cache_root, &self.home_root);
        let _ = fs::remove_dir_all(&self.cache_root);
        let _ = fs::remove_dir_all(&self.home_root);
    }
}

#[test]
#[ignore = "invoked by managed_windows_start_has_one_consoleless_owner"]
fn windows_daemon_console_probe_helper() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let pid: u32 = std::env::var("SOLDR_CONSOLE_PROBE_PID")
        .expect("console probe PID")
        .parse()
        .expect("numeric console probe PID");
    assert_eq!(
        soldr_platform::process::inspect::console_attached(pid),
        Some(false),
        "daemon PID {pid} owns a Windows console"
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn managed_windows_start_has_one_consoleless_owner() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let _lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let cache_root = unique_temp_dir("daemon-detached-process-tree-cache");
    let home_root = unique_temp_dir("daemon-detached-process-tree-home");
    let _cleanup = DetachedDaemonCleanup {
        cache_root: cache_root.clone(),
        home_root: home_root.clone(),
    };

    let first = run_soldr(&["daemon", "start"], &cache_root, &home_root);
    assert!(
        first.status.success(),
        "first detached start failed: stdout={}; stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr),
    );
    assert!(
        wait_for_ready(
            &cache_root,
            &home_root,
            Instant::now() + Duration::from_secs(40)
        ),
        "managed daemon never became ready"
    );

    let paths = SoldrPaths::with_root(cache_root.clone());
    // soldr#3823: read under the daemon's generation key (soldr#3374).
    let (pid, exe) = common::route_claim::with_published_generation(&cache_root, || {
        soldr_cli::daemon::lifecycle::read_route_claim_identity(&paths)
    })
    .expect("daemon route claim publication");
    // Two canonical placements exist post-#2364: the per-root
    // self-relocation tree (`<root>/runtime/soldr-daemon/...`) and the
    // broker's route-runtime tree
    // (`.../soldr-broker/routes/<route>/runtime/soldr-daemon/...`) — the
    // broker is the sole daemon spawner and places images in its own tree,
    // while adoption of a previously relocated daemon keeps the per-root
    // shape. Which one wins is timing-dependent; both are canonical, and
    // asserting only the per-root shape made this test flake on whichever
    // spawn path won the race.
    let per_root = exe.starts_with(soldr_cli::self_relocate::daemon_runtime_root(&paths));
    let components: Vec<String> = exe
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    let broker_route = components.iter().any(|c| c == "soldr-broker")
        && components
            .windows(2)
            .any(|pair| pair[0] == "runtime" && pair[1] == "soldr-daemon");
    assert!(
        per_root || broker_route,
        "the PID owner must be a canonical runtime image (per-root \
         `<root>/runtime/soldr-daemon/...` or broker route \
         `.../soldr-broker/routes/<r>/runtime/soldr-daemon/...`): {}",
        exe.display()
    );
    // The daemon owns its route's private lock. The test executable has a
    // different image identity, so an unscoped probe would check another
    // route's (free) lock instead.
    let generations =
        fs::read_dir(soldr_cli::cache_lib::soldr_daemon_dir(&paths).join("generations"))
            .expect("generation directories")
            .map(|entry| entry.expect("generation entry").file_name())
            .collect::<Vec<_>>();
    assert_eq!(generations.len(), 1, "expected one managed daemon route");
    let service_name = generations[0].to_string_lossy();
    assert!(
        soldr_cli::daemon::backend_handle_adoption::with_generation_key(&service_name, || {
            soldr_cli::daemon::lifecycle::RootOwnershipGuard::try_acquire(&paths)
        })
        .expect("probe this route's owner lock")
        .is_none(),
        "the live PID owner must hold its private route lock"
    );

    // A plain second start: `--idle-timeout` is a hard-rejected legacy flag
    // under the broker-owned model (#2441), so the old `--idle-timeout 60`
    // here could never succeed — it went unnoticed for weeks because the
    // Windows lanes kept aborting on earlier failures before this test ever
    // executed. The invariant under test is unchanged: an idempotent second
    // start must preserve the one root owner.
    let second = run_soldr(&["daemon", "start"], &cache_root, &home_root);
    assert!(
        second.status.success(),
        "second detached start failed: {second:?}"
    );
    assert_eq!(
        // soldr#3823: read under the daemon's generation key (soldr#3374).
        common::route_claim::with_published_generation(&cache_root, || {
            soldr_cli::daemon::lifecycle::read_route_claim_identity(&paths)
        })
        .map(|(pid, _)| pid),
        Some(pid),
        "a second managed start must preserve the one root owner"
    );

    let processes =
        soldr_platform::host::resources::process_table().expect("Windows process snapshot");
    let daemon = processes
        .iter()
        .find(|entry| entry.0 == pid)
        .unwrap_or_else(|| panic!("daemon PID {pid} missing from process snapshot"));
    assert_eq!(
        daemon.1.to_ascii_lowercase(),
        "soldr-daemon.exe",
        "route claim must identify the canonical daemon process"
    );

    let probe = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--ignored",
            "--exact",
            "windows_daemon_console_probe_helper",
            "--nocapture",
        ])
        .env("SOLDR_CONSOLE_PROBE_PID", pid.to_string())
        .output()
        .expect("spawn isolated console probe");
    assert!(
        probe.status.success(),
        "daemon console probe failed: stdout={}; stderr={}",
        String::from_utf8_lossy(&probe.stdout),
        String::from_utf8_lossy(&probe.stderr),
    );

    let stop = run_soldr(&["daemon", "stop"], &cache_root, &home_root);
    assert!(
        stop.status.success(),
        "daemon stop failed: stdout={}; stderr={}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr),
    );
    assert!(
        wait_for_process_exit(pid, Duration::from_secs(5)),
        "daemon PID {pid} survived stop"
    );
    assert!(
        soldr_cli::daemon::backend_handle_adoption::with_generation_key(&service_name, || {
            soldr_cli::daemon::lifecycle::RootOwnershipGuard::try_acquire(&paths)
        })
        .expect("probe released route owner lock")
        .is_some(),
        "daemon stop must release its private route lock"
    );
}

#[test]
fn doctor_uses_same_endpoint_as_daemon_status_for_cook_counts() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let daemon = Daemon::spawn();
    let cache_root = daemon.cache_root.clone();
    let home_root = daemon.home_root.clone();
    let workspace = unique_temp_dir("daemon-doctor-workspace");

    let status = run_soldr(&["daemon", "status", "--json"], &cache_root, &home_root);
    assert!(
        status.status.success(),
        "daemon status failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    let status_body: Value = serde_json::from_slice(&status.stdout).expect("status json");
    assert_eq!(status_body["running"].as_bool(), Some(true));

    let doctor = run_soldr_with_timeout(
        &["doctor", "--json"],
        &cache_root,
        &home_root,
        &workspace,
        Duration::from_secs(15),
    );
    assert!(
        doctor.status.success(),
        "doctor failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );
    let doctor_body: Value = serde_json::from_slice(&doctor.stdout).expect("doctor json");
    assert_eq!(
        doctor_body["cook"]["entries"].as_u64(),
        Some(0),
        "doctor must query the same live daemon endpoint as `soldr daemon status`: {doctor_body}"
    );
    assert_eq!(doctor_body["cook"]["total_bytes"].as_u64(), Some(0));
    assert_eq!(doctor_body["cook"]["hits_this_session"].as_u64(), Some(0));

    drop(daemon);
}

#[test]
fn cargo_test_recovers_after_daemon_stop_without_herd_spawning() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let cache_root = unique_temp_dir("daemon-restart-cache");
    let home_root = unique_temp_dir("daemon-restart-home");
    let project = unique_temp_dir("daemon-restart-project");
    let _cleanup = DaemonCleanup {
        cache_root: cache_root.clone(),
        home_root: home_root.clone(),
    };

    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"soldr_daemon_restart_probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::create_dir_all(project.join("src")).expect("create src");
    fs::write(
        project.join("src").join("lib.rs"),
        "pub fn add(left: usize, right: usize) -> usize { left + right }\n\
         #[test]\n\
         fn it_adds() { assert_eq!(add(2, 2), 4); }\n",
    )
    .expect("write lib.rs");

    let first = run_soldr_with_timeout(
        &["cargo", "test", "--quiet"],
        &cache_root,
        &home_root,
        &project,
        Duration::from_secs(150),
    );
    assert!(
        first.status.success(),
        "first soldr cargo test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    let stop = run_soldr(&["daemon", "stop"], &cache_root, &home_root);
    assert!(
        stop.status.success(),
        "daemon stop failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr)
    );
    std::thread::sleep(Duration::from_millis(500));
    fs::remove_dir_all(project.join("target")).expect("remove target for forced recompile");

    let second = run_soldr_with_timeout(
        &["cargo", "test", "--quiet"],
        &cache_root,
        &home_root,
        &project,
        Duration::from_secs(150),
    );
    assert!(
        second.status.success(),
        "second soldr cargo test after daemon stop failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );

    let lifecycle = fs::read_to_string(
        cache_root
            .join("cache")
            .join("soldr-daemon")
            .join("lifecycle.jsonl"),
    )
    .unwrap_or_default();
    let spawn_count = lifecycle
        .lines()
        .filter(|line| line.contains("\"event\":\"spawn\""))
        .count();
    assert!(
        spawn_count <= 2,
        "two cargo test runs with one explicit stop should spawn at most one daemon each; lifecycle={lifecycle}"
    );
}

#[test]
fn status_when_daemon_absent_reports_not_running() {
    let cache_root = unique_temp_dir("daemon-absent-cache");
    let home_root = unique_temp_dir("daemon-absent-home");
    let out = run_soldr(&["daemon", "status", "--json"], &cache_root, &home_root);
    assert!(
        out.status.success(),
        "status against absent daemon should succeed (exit=0). stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: Value = serde_json::from_slice(&out.stdout).expect("status json");
    assert_eq!(body["running"].as_bool(), Some(false));
    let paths = SoldrPaths::with_root(cache_root.clone());
    assert!(
        !soldr_cli::daemon::backend_handle_adoption::broker_route_claim_path(&paths).exists(),
        "a control-tunnel status probe must not launch an absent route"
    );
    let _ = run_soldr(&["broker", "stop"], &cache_root, &home_root);
}

#[test]
fn install_servicedef_writes_running_process_definition() {
    let cache_root = unique_temp_dir("daemon-servicedef-cache");
    let home_root = unique_temp_dir("daemon-servicedef-home");
    let _broker = common::BrokerHomeGuard::new(&cache_root, &home_root);
    let service_root = unique_temp_dir("daemon-servicedef-services");
    let daemon_dir = unique_temp_dir("daemon-servicedef-bin");
    let daemon_binary = daemon_dir.join(
        if matches!(
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::HostOs::Windows
        ) {
            "soldr-daemon.exe"
        } else {
            "soldr-daemon"
        },
    );
    fs::write(&daemon_binary, b"stub daemon").expect("write fake daemon binary");

    let mut cmd = Command::new(common::soldr_bin());
    cmd.args([
        "daemon",
        "install-servicedef",
        "--daemon-binary",
        daemon_binary.to_str().expect("utf8 daemon path"),
        "--json",
    ]);
    for (k, v) in isolated_env(&cache_root, &home_root) {
        cmd.env(k, v);
    }
    cmd.env("RUNNING_PROCESS_SERVICE_DEF_DIR", &service_root);
    cmd.env_remove("RUSTC_WRAPPER");
    let out = cmd.output().expect("run soldr daemon install-servicedef");

    assert!(
        out.status.success(),
        "install-servicedef failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let body: Value = serde_json::from_slice(&out.stdout).expect("servicedef json");
    let service_name = body["service_name"]
        .as_str()
        .expect("service_name")
        .to_string();
    assert!(service_name.starts_with("soldr-daemon-"));
    // #1501 moved servicedef to the running-process v2 surface; the
    // remaining deferred item is the upstream-gated broker-owned
    // UpgradeDaemon handoff (see SOLDR_DAEMON_SERVICE_DEF_DEFERRED).
    assert!(body["deferred"]
        .as_array()
        .expect("deferred array")
        .iter()
        .any(|item| item
            .as_str()
            .is_some_and(|value| value.contains("UpgradeDaemon"))));

    // #1501: servicedefs are written as `.servicedef.v2` protobufs and
    // load through the protocol_v2 loader.
    let loaded = running_process::broker::protocol_v2::ServiceDefinitionLoader::new(&service_root)
        .load(&service_name)
        .expect("running-process loader validates soldr servicedef");
    assert_eq!(loaded.service_name, service_name);
    assert_eq!(
        loaded.isolation,
        running_process::broker::protocol_v2::BrokerIsolation::SharedBroker as i32,
    );
    assert_eq!(
        loaded.binary_path,
        fs::canonicalize(&daemon_binary)
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(loaded.min_version, env!("CARGO_PKG_VERSION"));
}
