//! Unit tests for [`crate::cargo_front_door`]: the cargo argv parser, the
//! low-disk warning helper, and the cargo-subcommand sniffer.
//! Lives inside the `cargo_front_door/` module directory so `mod.rs`
//! stays comfortably under the 1000-LOC ceiling.

use super::*;

/// Create a directory symlink, or return false when the platform/session
/// cannot make one (Windows needs Developer Mode or elevation).
fn try_symlink_dir(src: &Path, dst: &Path) -> bool {
    crate::platform::fs::links::create(&src.to_string_lossy(), dst, true).is_ok()
}

#[test]
fn closure_walk_terminates_on_a_symlink_cycle() {
    // #1662. `collect_closure_files` and `add_cargo_closure_path` are
    // mutually recursive and used `Path::is_dir()`, which FOLLOWS symlinks
    // (unlike `DirEntry::metadata()`, which does not). A directory symlink
    // pointing back at an ancestor therefore recursed until the stack blew.
    let target = tempfile::tempdir().expect("tempdir");
    let nested = target.path().join("debug").join("deps");
    std::fs::create_dir_all(&nested).expect("mkdir");
    std::fs::write(nested.join("libthing.rlib"), b"x").expect("write");

    if !try_symlink_dir(target.path(), &nested.join("cycle")) {
        eprintln!("skipping: cannot create directory symlinks here");
        return;
    }

    let mut paths = BTreeMap::new();
    // The assertion is that this returns at all.
    collect_closure_files(&mut paths, target.path(), target.path());

    assert!(
        paths.keys().any(|k| k.ends_with("libthing.rlib")),
        "the real artifact should still be collected: {paths:?}"
    );
    assert!(
        !paths.keys().any(|k| k.contains("cycle")),
        "a symlink must not contribute paths: {paths:?}"
    );
}

#[test]
fn closure_walk_does_not_escape_the_target_dir() {
    // The walk must not wander outside `target_dir` via a symlink: files
    // found out there were already refused by `add_cargo_closure_path`'s
    // `strip_prefix` guard, but nothing stopped the *directory* recursion.
    let target = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("tempdir");
    std::fs::write(outside.path().join("secret.rlib"), b"x").expect("write");
    std::fs::write(target.path().join("own.rlib"), b"x").expect("write");

    if !try_symlink_dir(outside.path(), &target.path().join("escape")) {
        eprintln!("skipping: cannot create directory symlinks here");
        return;
    }

    let mut paths = BTreeMap::new();
    collect_closure_files(&mut paths, target.path(), target.path());

    assert!(paths.keys().any(|k| k.ends_with("own.rlib")));
    assert!(
        !paths.keys().any(|k| k.contains("secret")),
        "walk escaped the target dir: {paths:?}"
    );
}
use crate::LOW_DISK_WARNING_THRESHOLD_BYTES;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Serialises tests that mutate process-wide environment variables so
/// they don't race under parallel `cargo test`.
use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(value) = &self.previous {
            std::env::set_var(self.key, value);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

fn remove_env_vars(keys: &[&'static str]) -> Vec<EnvVarGuard> {
    keys.iter().map(|&key| EnvVarGuard::remove(key)).collect()
}

fn command_env_override(
    command: &std::process::Command,
    key: &'static str,
) -> Option<Option<OsString>> {
    command
        .get_envs()
        .find(|(candidate, _)| *candidate == OsStr::new(key))
        .map(|(_, value)| value.map(OsString::from))
}

#[test]
fn target_registry_memo_is_not_exported_without_daemon_ack() {
    let root = tempfile::tempdir().expect("temp root");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    paths.ensure_dirs().expect("soldr dirs");
    let target = root.path().join("workspace").join("target");
    assert!(!target.exists(), "test requires a clean target directory");

    let mut command = std::process::Command::new("cargo");
    apply_target_registry_memo(&mut command, &target, &paths);

    assert_eq!(
        command_env_override(
            &command,
            crate::wrapper_target::TARGET_REGISTRY_RECORDED_ENV_VAR,
        ),
        None,
        "the client must not claim a daemon-owned registry touch was recorded",
    );
    assert!(
        !target.exists(),
        "memoization must not create target/ as a side effect"
    );
}

#[test]
fn zthreads_fallback_removes_plain_rustflags_token() {
    let _lock = ENV_LOCK.lock();
    let _guards = remove_env_vars(&[
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
        zthreads_fallback::ATTEMPTED_ENV,
        "RUSTC_BOOTSTRAP",
    ]);
    std::env::set_var("RUSTFLAGS", "-C debuginfo=1 -Zthreads=8");

    let plan = zthreads_fallback::plan_from_environment().expect("plain flags are removable");
    assert_eq!(plan.value, "8");
    assert_eq!(
        plan.env.get("RUSTFLAGS"),
        Some(&Some(String::from("-C debuginfo=1")))
    );
}

#[test]
fn zthreads_fallback_removes_encoded_and_target_tokens() {
    let _lock = ENV_LOCK.lock();
    let _guards = remove_env_vars(&[
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
        zthreads_fallback::ATTEMPTED_ENV,
        "RUSTC_BOOTSTRAP",
    ]);
    std::env::set_var(
        "CARGO_ENCODED_RUSTFLAGS",
        "-C\x1fopt-level=2\x1f-Zthreads=4",
    );
    std::env::set_var(
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
        "-C target-cpu=native -Zthreads=4",
    );

    let plan = zthreads_fallback::plan_from_environment().expect("encoded flags are removable");
    assert_eq!(plan.value, "4");
    assert_eq!(
        plan.env.get("CARGO_ENCODED_RUSTFLAGS"),
        Some(&Some(String::from("-C\x1fopt-level=2")))
    );
    assert_eq!(
        plan.env
            .get("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS"),
        Some(&Some(String::from("-C target-cpu=native")))
    );
}

#[test]
fn zthreads_fallback_rejects_other_z_flags_and_bootstrap() {
    let _lock = ENV_LOCK.lock();
    let _guards = remove_env_vars(&[
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
        zthreads_fallback::ATTEMPTED_ENV,
        "RUSTC_BOOTSTRAP",
    ]);
    std::env::set_var("RUSTFLAGS", "-Zthreads=8 -Zshare-generics=y");
    assert!(zthreads_fallback::plan_from_environment().is_none());

    std::env::set_var("RUSTFLAGS", "-Zthreads=8");
    std::env::set_var("RUSTC_BOOTSTRAP", "1");
    assert!(zthreads_fallback::plan_from_environment().is_none());
}

#[test]
fn zthreads_fallback_warning_has_ci_and_local_forms() {
    assert_eq!(
        zthreads_fallback::render_warning("8", true, false),
        "::warning::soldr: stable Rust rejected -Zthreads=8; retrying once without it. Build output is unchanged, but compilation may be slower."
    );
    assert!(zthreads_fallback::render_warning("8", false, true).starts_with("\x1b[33m"));
    assert!(
        zthreads_fallback::render_warning("8", false, false).contains("compilation may be slower")
    );
    assert!(zthreads_fallback::diagnostic_matches(
        "error: the option `Z` is only accepted on the nightly compiler"
    ));
    assert!(!zthreads_fallback::diagnostic_matches(
        "error: could not compile"
    ));
    assert!(zthreads_fallback::render_config_hint().contains("Cargo config"));
    assert!(resolved_toolchain_is_nightly(Some("nightly-2026-07-22")));
    assert!(!resolved_toolchain_is_nightly(Some("1.94.1")));
}

#[test]
fn zthreads_retry_replays_original_front_door_contract() {
    let args = argv(&["run", "--no-gc-target", "--no-trampoline", "--", "payload"]);
    let uncached = ZthreadsRetryContext::new(&args, false, true);

    assert_eq!(
        uncached.cli_args(),
        vec![
            "--no-cache",
            "--trust-inherited-soldr-env",
            "cargo",
            "run",
            "--no-gc-target",
            "--no-trampoline",
            "--",
            "payload",
        ],
        "the retry must replay top-level state and the original pre-normalization Cargo argv",
    );

    let cached = ZthreadsRetryContext::new(&argv(&["build", "--release"]), true, false);
    assert_eq!(
        cached.cli_args(),
        vec!["cargo", "build", "--release"],
        "a managed retry should continue through the normal cached front door",
    );
}

#[test]
fn target_registry_memo_does_not_export_a_client_side_marker() {
    let root = tempfile::tempdir().expect("temp root");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    paths.ensure_dirs().expect("soldr dirs");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(workspace.join("nested")).expect("workspace dirs");
    let lexical_target = workspace.join("nested").join("..").join("target");
    assert!(
        !workspace.join("target").exists(),
        "test requires a clean target"
    );

    let mut command = std::process::Command::new("cargo");
    apply_target_registry_memo(&mut command, &lexical_target, &paths);

    assert_eq!(
        command_env_override(
            &command,
            crate::wrapper_target::TARGET_REGISTRY_RECORDED_ENV_VAR,
        ),
        None,
    );
}

#[test]
fn cargo_wait_timeout_is_disabled_when_unset_or_zero() {
    let _lock = ENV_LOCK.lock();

    let _guard = EnvVarGuard::remove(CARGO_WAIT_TIMEOUT_ENV_VAR);
    assert_eq!(cargo_wait_timeout().expect("unset timeout"), None);
    drop(_guard);

    let _guard = EnvVarGuard::set(CARGO_WAIT_TIMEOUT_ENV_VAR, "0");
    assert_eq!(cargo_wait_timeout().expect("zero timeout"), None);
}

#[test]
fn cargo_wait_timeout_accepts_positive_seconds() {
    let _lock = ENV_LOCK.lock();
    let _guard = EnvVarGuard::set(CARGO_WAIT_TIMEOUT_ENV_VAR, "7");

    assert_eq!(
        cargo_wait_timeout().expect("positive timeout"),
        Some(Duration::from_secs(7))
    );
}

#[test]
fn cargo_wait_timeout_rejects_invalid_values() {
    let _lock = ENV_LOCK.lock();

    for value in ["", "-1", "not-a-number", "18446744073709551616"] {
        let _guard = EnvVarGuard::set(CARGO_WAIT_TIMEOUT_ENV_VAR, value);
        let message = cargo_wait_timeout()
            .expect_err("invalid timeout must fail")
            .to_string();
        assert!(
            message.contains(CARGO_WAIT_TIMEOUT_ENV_VAR),
            "diagnostic must name the variable for {value:?}: {message}"
        );
    }
}

#[test]
fn diagnostic_capture_returns_when_a_leaked_handle_holds_the_pipe_open() {
    // The regression this guards (#422 / the bounded drain): when a cargo
    // grandchild inherits the stderr write handle, the pipe never reaches
    // EOF, and the drain must give up after CAPTURE_PIPE_EOF_GRACE rather
    // than block forever.
    //
    // Driven through the channel rather than a real `cmd /C ... start /B
    // ping` fixture. The old version asserted `elapsed >= 1750ms`, which
    // asserted that the *fixture* had worked — that `ping` really did leak
    // the handle — not anything about the code. On a fast host the leak did
    // not materialise, the drain returned in 466ms, and the test failed
    // while the actual contract was being honoured. Holding the sender open
    // reproduces "nobody will ever close this pipe" exactly and
    // deterministically, on every platform.
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(CapturePipeMessage::Chunk(b"leaked diagnostic".to_vec()))
        .expect("send chunk");

    let start = Instant::now();
    let drained = drain_capture_pipe_after_child_exit(&rx, "test capture");
    let elapsed = start.elapsed();

    // Sender still alive => no Eof, no Disconnected: the grace bounds it.
    drop(tx);

    assert_eq!(
        String::from_utf8_lossy(&drained),
        "leaked diagnostic",
        "bytes already in the pipe must survive the bounded drain"
    );
    assert!(
        elapsed < CAPTURE_PIPE_EOF_GRACE + Duration::from_secs(2),
        "drain must be bounded by the grace window; elapsed={elapsed:?}"
    );
}

#[test]
fn diagnostic_capture_returns_immediately_once_the_pipe_closes() {
    // The common case: the writer goes away, so the drain must return at once
    // rather than sitting out the full grace window.
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(CapturePipeMessage::Chunk(b"all output".to_vec()))
        .expect("send chunk");
    drop(tx);

    let start = Instant::now();
    let drained = drain_capture_pipe_after_child_exit(&rx, "test capture");
    let elapsed = start.elapsed();

    assert_eq!(String::from_utf8_lossy(&drained), "all output");
    assert!(
        elapsed < CAPTURE_PIPE_EOF_GRACE,
        "a closed pipe must not wait out the grace window; elapsed={elapsed:?}"
    );
}
#[test]
fn timeout_error_mentions_cleanup_and_recovery() {
    let err = SoldrError::Other(format!(
        "cargo diagnostic capture timed out after 1 seconds (set {CARGO_WAIT_TIMEOUT_ENV_VAR} to override); killed child process tree"
    ));
    let cleanup = CargoAbortCleanupReport {
        orphan_rmetas_pruned: 2,
        incremental_dirs_removed: 1,
    };
    let msg = augment_aborted_cargo_error(err, cleanup, true).to_string();

    assert!(
        msg.contains("soldr cleanup after abort: pruned 2 orphan .rmeta file(s), removed 1 incremental/ dir(s)"),
        "timeout message should summarize cleanup: {msg}"
    );
    assert!(
        msg.contains("ZCCACHE_DISABLE=1 soldr cargo clean -p <crate>"),
        "timeout message should include actionable recovery: {msg}"
    );
    assert!(
        msg.contains("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
        "timeout message should point at fail-fast compile diagnostics: {msg}"
    );
    assert!(
        msg.contains("ZCCACHE_DISABLE=1 soldr cargo <same args>"),
        "timeout message should point at the cache bypass retry: {msg}"
    );
    assert!(
        msg.contains("ZCCACHE_DISABLE=1"),
        "timeout message should mention the zccache disable escape hatch: {msg}"
    );
    assert!(
        msg.contains("soldr logs paths"),
        "timeout message should point at durable log discovery: {msg}"
    );
}

#[test]
fn cargo_timeout_retry_policy_is_compile_like_and_cache_enabled() {
    let _lock = ENV_LOCK.lock();
    let _guard = EnvVarGuard::remove(CARGO_TIMEOUT_RETRY_DISABLE_ENV_VAR);

    for verb in [
        "build", "b", "check", "c", "test", "t", "clippy", "doc", "d",
    ] {
        assert!(
            cargo_timeout_retry_allowed(true, &[String::from(verb)]),
            "{verb} should be eligible for a no-cache timeout retry"
        );
    }
    for verb in ["run", "r", "bench", "install", "metadata", "clean"] {
        assert!(
            !cargo_timeout_retry_allowed(true, &[String::from(verb)]),
            "{verb} should not be retried automatically"
        );
    }
    assert!(
        !cargo_timeout_retry_allowed(false, &[String::from("build")]),
        "already no-cache cargo runs should not recurse into another retry"
    );
}

#[test]
fn cargo_timeout_retry_policy_honors_disable_env() {
    let _lock = ENV_LOCK.lock();
    let _guard = EnvVarGuard::set(CARGO_TIMEOUT_RETRY_DISABLE_ENV_VAR, "1");

    assert!(
        !cargo_timeout_retry_allowed(true, &[String::from("build")]),
        "{CARGO_TIMEOUT_RETRY_DISABLE_ENV_VAR}=1 should disable automatic retry"
    );
}

#[test]
fn cargo_wait_heartbeat_distinguishes_deadline_configuration() {
    let no_deadline =
        cargo_wait_heartbeat_message("cargo diagnostic capture", Duration::from_secs(120), None);
    assert_eq!(
        no_deadline,
        "soldr: cargo diagnostic capture still running after 120s (no wall-clock deadline configured)"
    );
    assert!(!no_deadline.contains("timeout"));

    let explicit = cargo_wait_heartbeat_message(
        "cargo diagnostic capture",
        Duration::from_secs(120),
        Some(Duration::from_secs(1800)),
    );

    assert_eq!(
        explicit,
        format!(
            "soldr: cargo diagnostic capture still running after 120s (explicit timeout 1800s from {CARGO_WAIT_TIMEOUT_ENV_VAR})"
        )
    );
}

fn spawn_slow_wait_test_child() -> std::process::Child {
    let mut command =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "ping -n 2 127.0.0.1 >nul"]);
            command
        } else {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "sleep 0.25"]);
            command
        };
    configure_cargo_child_for_timeout(&mut command);
    command.spawn().expect("spawn slow fake Cargo child")
}

#[test]
fn cargo_wait_none_outlives_simulated_former_default() {
    let simulated_former_default = Duration::from_millis(50);

    let mut no_deadline_child = spawn_slow_wait_test_child();
    let started = Instant::now();
    let status = wait_for_cargo_child_with_heartbeat(
        &mut no_deadline_child,
        "fake Cargo without deadline",
        None,
        Duration::from_millis(100),
    )
    .expect("unset timeout must let the child finish");
    assert!(status.success());
    assert!(
        started.elapsed() > simulated_former_default,
        "fake child must outlive the simulated former default"
    );

    let mut explicit_deadline_child = spawn_slow_wait_test_child();
    let error = wait_for_cargo_child_with_heartbeat(
        &mut explicit_deadline_child,
        "fake Cargo with explicit deadline",
        Some(simulated_former_default),
        Duration::from_millis(100),
    )
    .expect_err("the same child must be killed by an explicit deadline");
    assert!(
        error.to_string().contains("timed out after"),
        "explicit timeout should retain kill/reap behavior: {error}"
    );
}

#[test]
fn aborted_build_cleanup_removes_incremental_dirs() {
    let target = tempfile::tempdir().expect("temp target");
    let host_incremental = target.path().join("debug").join("incremental");
    let target_incremental = target
        .path()
        .join("x86_64-pc-windows-msvc")
        .join("release")
        .join("incremental");
    let deps = target.path().join("debug").join("deps");
    std::fs::create_dir_all(host_incremental.join("crate-a")).expect("host incremental");
    std::fs::create_dir_all(target_incremental.join("crate-b")).expect("target incremental");
    std::fs::create_dir_all(&deps).expect("deps");
    std::fs::write(deps.join("libkeep.rlib"), b"keep").expect("deps file");

    let removed = cleanup_target_incremental_dirs_after_aborted_build(target.path());

    assert_eq!(removed, 2);
    assert!(!host_incremental.exists());
    assert!(!target_incremental.exists());
    assert!(deps.join("libkeep.rlib").exists());
}

#[test]
fn aborted_build_cleanup_prunes_rmetas_and_incremental_dirs() {
    // soldr#2996: the sweep used to hang off a target cache plan, so it only
    // ran for callers who had opted into the cache -- never on a default
    // build, which is exactly when a failed build strands an rmeta. It now
    // resolves the target directory from the cargo args like every other
    // hook, which is what this test drives via `--target-dir`.
    let root = tempfile::tempdir().expect("temp root");
    let target = root.path().join("target");
    let deps = target.join("debug").join("deps");
    let incremental = target.join("debug").join("incremental");
    std::fs::create_dir_all(&deps).expect("deps");
    std::fs::create_dir_all(incremental.join("crate-a")).expect("incremental");
    let orphan_rmeta = deps.join("libstale.rmeta");
    std::fs::write(&orphan_rmeta, b"stale").expect("orphan rmeta");

    let cache_plan = CargoCachePlan::for_test_without_wrapper();
    let args = [
        String::from("build"),
        String::from("--target-dir"),
        target.display().to_string(),
    ];

    let cleanup = cleanup_after_aborted_cargo_run(&cache_plan, &args, true);

    assert_eq!(cleanup.orphan_rmetas_pruned, 1);
    assert_eq!(cleanup.incremental_dirs_removed, 1);
    assert!(!orphan_rmeta.exists());
    assert!(!incremental.exists());
}

#[test]
fn child_cargo_scrubs_soldr_cache_lifecycle_controls() {
    let mut command = std::process::Command::new("cargo");
    command.env(SOLDR_CACHE_LIFECYCLE_ENV_VAR, "command");
    command.env(SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR, "1");

    scrub_soldr_cache_lifecycle_env_for_child_cargo(&mut command);

    assert_eq!(
        command_env_override(&command, SOLDR_CACHE_LIFECYCLE_ENV_VAR),
        Some(None)
    );
    assert_eq!(
        command_env_override(&command, SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR),
        Some(None)
    );
}

#[test]
fn fresh_workspace_env_guard_removes_and_restores_soldr_workspace_state() {
    let _lock = ENV_LOCK.lock();
    let _zccache = EnvVarGuard::set(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR, "/old/zccache");
    let _target_bundle = EnvVarGuard::set(crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR, "/old/bundle");
    let _setup = EnvVarGuard::set("SETUP_SOLDR_WORKSPACE", "/old/workspace");
    let _cache_dir = EnvVarGuard::set("SOLDR_CACHE_DIR", "/intentional/cache");

    {
        let _guard = FreshSoldrWorkspaceEnvGuard::apply_unless_trusted(false);

        assert!(std::env::var_os(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR).is_none());
        assert!(std::env::var_os(crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR).is_none());
        assert!(std::env::var_os("SETUP_SOLDR_WORKSPACE").is_none());
        assert_eq!(
            std::env::var_os("SOLDR_CACHE_DIR"),
            Some(OsString::from("/intentional/cache"))
        );
    }

    assert_eq!(
        std::env::var_os(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR),
        Some(OsString::from("/old/zccache"))
    );
    assert_eq!(
        std::env::var_os(crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR),
        Some(OsString::from("/old/bundle"))
    );
    assert_eq!(
        std::env::var_os("SETUP_SOLDR_WORKSPACE"),
        Some(OsString::from("/old/workspace"))
    );
}

#[test]
fn trusted_workspace_env_guard_leaves_inherited_soldr_state_available() {
    let _lock = ENV_LOCK.lock();
    let _zccache = EnvVarGuard::set(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR, "/old/zccache");

    let _guard = FreshSoldrWorkspaceEnvGuard::apply_unless_trusted(true);

    assert_eq!(
        std::env::var_os(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR),
        Some(OsString::from("/old/zccache"))
    );
}

#[test]
fn child_cargo_scrubs_inherited_soldr_workspace_state() {
    let _lock = ENV_LOCK.lock();
    let _setup = EnvVarGuard::set("SETUP_SOLDR_WORKSPACE", "/old/workspace");
    let mut command = std::process::Command::new("cargo");
    command.env(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR, "/old/zccache");
    command.env(crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR, "/old/bundle");

    scrub_inherited_soldr_workspace_env_for_child_cargo(&mut command);

    assert_eq!(
        command_env_override(&command, crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR),
        Some(None)
    );
    assert_eq!(
        command_env_override(&command, crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR),
        Some(None)
    );
    assert_eq!(
        command_env_override(&command, "SETUP_SOLDR_WORKSPACE"),
        Some(None)
    );
}

#[test]
fn low_disk_warning_formats_yellow_below_threshold() {
    let message = low_disk_warning_for_free_bytes(1536 * 1024 * 1024, true)
        .expect("expected low-disk warning below threshold");
    assert!(message.contains("\x1b[33mwarning\x1b[0m"));
    assert!(message.contains("1.5 GB free"));
    assert!(message.contains("Run `soldr gc`"));
}

#[test]
fn low_disk_warning_omits_at_threshold() {
    assert!(low_disk_warning_for_free_bytes(LOW_DISK_WARNING_THRESHOLD_BYTES, true).is_none());
}

#[test]
fn low_disk_probe_failure_is_nonfatal() {
    let warning = low_disk_warning_for_path(std::path::Path::new("."), true, |_| {
        Err(std::io::Error::other("probe failed"))
    });
    assert!(warning.is_none());
}

#[test]
fn cargo_args_detect_explicit_target_flag() {
    assert!(cargo_args_specify_target(&[
        "build".into(),
        "--target".into(),
        "x86_64-pc-windows-msvc".into(),
    ]));
    assert!(cargo_args_specify_target(&[
        "build".into(),
        "--target=x86_64-pc-windows-msvc".into(),
    ]));
}

#[test]
fn cargo_args_ignore_target_after_passthrough_separator() {
    assert!(!cargo_args_specify_target(&[
        "test".into(),
        "--".into(),
        "--target".into(),
        "ignored".into(),
    ]));
}

#[test]
fn cargo_args_reject_reserved_no_cache_before_passthrough_separator() {
    assert!(cargo_args_use_reserved_no_cache(&[
        "build".into(),
        "--no-cache".into(),
    ]));
    assert!(!cargo_args_use_reserved_no_cache(&[
        "test".into(),
        "--".into(),
        "--no-cache".into(),
    ]));
}

#[test]
fn first_cargo_subcommand_skips_leading_flags() {
    assert_eq!(
        first_cargo_subcommand(&["--verbose".into(), "nextest".into(), "run".into()]),
        Some("nextest")
    );
    assert_eq!(
        first_cargo_subcommand(&["nextest".into(), "run".into()]),
        Some("nextest")
    );
    assert_eq!(first_cargo_subcommand(&["--help".into()]), None);
    assert_eq!(first_cargo_subcommand(&[]), None);
}

#[test]
fn first_cargo_subcommand_stops_at_passthrough_separator() {
    assert_eq!(
        first_cargo_subcommand(&["--".into(), "nextest".into()]),
        None
    );
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn cargo_args_are_cacheable_for_direct_build() {
    assert!(cargo_args_are_cacheable(&argv(&["build"])));
    assert!(cargo_args_are_cacheable(&argv(&["build", "--release"])));
    assert!(cargo_args_are_cacheable(&argv(&["b"])));
}

#[test]
fn cargo_args_are_cacheable_for_chef_cook() {
    // soldr cook (issue #359) routes `cargo chef cook` through this front
    // door. The outer process orchestrates an inner `cargo build` against
    // a stub project, so we must seed RUSTC_WRAPPER for the inner build
    // to pick zccache up.
    assert!(cargo_args_are_cacheable(&argv(&["chef", "cook"])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "chef",
        "cook",
        "--release",
    ])));
    assert!(cargo_args_are_cacheable(&argv(&["chef", "prepare"])));
}

#[test]
fn cargo_args_are_not_cacheable_for_direct_clean() {
    assert!(!cargo_args_are_cacheable(&argv(&["clean"])));
    assert!(!cargo_args_are_cacheable(&argv(&["fmt"])));
}

#[test]
fn cargo_args_are_not_cacheable_for_direct_miri_driver_hooks() {
    // Miri owns its own interpreter/runtime driver path. Soldr still
    // exports RUSTC_WRAPPER to cache-enabled `cargo miri`, but does not
    // run target/rust-plan build hooks around the Miri driver itself.
    assert!(!cargo_args_are_cacheable(&argv(&["miri"])));
}

#[test]
fn no_cache_preflight_covers_every_compiler_capable_surface() {
    for subcommand in [
        "build",
        "test",
        "install",
        "miri",
        "package",
        "publish",
        "nextest",
        "future-third-party-compiler",
    ] {
        assert!(
            cargo_args_may_compile_unmediated(&argv(&[subcommand])),
            "{subcommand} may compile without the managed wrapper"
        );
    }
}

#[test]
fn no_cache_preflight_skips_known_non_compiling_surfaces() {
    for subcommand in [
        "clean", "fetch", "fmt", "metadata", "search", "tree", "update", "audit", "deny", "machete",
    ] {
        assert!(
            !cargo_args_may_compile_unmediated(&argv(&[subcommand])),
            "{subcommand} is known not to compile"
        );
    }
}

#[test]
fn no_cache_preflight_treats_every_watch_shape_as_compiling() {
    for args in [
        argv(&["watch"]),
        argv(&["watch", "-x", "miri"]),
        argv(&["watch", "-x", "future-third-party-compiler"]),
        argv(&["watch", "-s", "make generated-rust"]),
    ] {
        assert!(
            cargo_args_may_compile_unmediated(&args),
            "cargo watch may launch an unmediated compiler: {args:?}"
        );
    }
}

#[test]
fn cargo_args_are_cacheable_for_every_registry_inner_build_subcommand() {
    // Issue #824 raised against `cargo zigbuild` specifically, but the
    // sub-agent audit of the known_tools registry surfaced six others
    // that have the same shape: outer cargo invocation spawns (or is)
    // an inner cargo build / test / doc whose rustc invocations need
    // RUSTC_WRAPPER to be present in the env. This test pins the
    // expected classification so a future "add a tool, forget to set
    // wraps_inner_cargo_build" regression breaks the build.
    //
    // Source of truth: each ToolSpec's `wraps_inner_cargo_build` field
    // and the per-entry comment justifying the value. This test asserts
    // that classification flows all the way through the front-door
    // cacheable predicate.
    for sub in [
        "nextest",       // runs `cargo test`
        "llvm-cov",      // runs `cargo test`/`build`; chains RUSTC_WRAPPER itself
        "udeps",         // embeds cargo crate; inherits parent env
        "semver-checks", // runs `cargo doc` for baseline + current
        "expand",        // calls `cargo rustc` directly
        "chef",          // runs `cargo build` for the stub project
        "zigbuild",      // the #824 repro — wraps `cargo build` with zig linker
        "xwin",          // wraps `cargo build` with the msvc-on-linux toolchain
        "binstall",      // Compile-strategy fallback shells `cargo install`
        "dylint",        // runs configured compiler plugins through cargo
    ] {
        assert!(
            cargo_args_are_cacheable(&argv(&[sub])),
            "subcommand {sub:?} must classify as cacheable so RUSTC_WRAPPER \
             propagates to its inner cargo (registry says \
             wraps_inner_cargo_build=true)",
        );
    }
}

#[test]
fn dylint_unavailable_diagnostic_names_host_component_and_remediation() {
    let error = dylint_unavailable_error(
        "dylint-link",
        "6.0.3",
        &SoldrError::UnsupportedPlatform("no matching asset".into()),
    );
    let message = error.to_string();
    assert!(message.contains(&crate::core::TargetTriple::host().unwrap().triple()));
    assert!(message.contains("dylint-link"));
    assert!(message.contains("Dylint v6.0.3 is not built for this machine"));
    assert!(message.contains("Soldr will not build Dylint from source"));
    assert!(message.contains("Corrective action:"));
}

#[test]
fn cached_dylint_link_is_revalidated_and_evicted() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp
        .path()
        .join(crate::platform::executable::name::native("dylint-link"));
    std::fs::write(&binary, b"not an executable").unwrap();
    let result = crate::fetch::FetchResult {
        binary_path: binary.clone(),
        version: "6.0.3".into(),
        cached: true,
    };

    let error = validated_dylint_link_prebuilt(&result).unwrap_err();
    assert!(error.to_string().starts_with("smoke test failed:"));
    assert!(
        !binary.exists(),
        "incompatible cached prebuilt must be evicted before returning an error"
    );
}

#[test]
fn managed_dylint_missing_prebuilt_is_binary_or_error() {
    let mut source_build_ran = false;
    let result = resolve_dylint_binary(
        "cargo-dylint",
        Err(SoldrError::UnsupportedPlatform("no matching asset".into())),
        || {
            source_build_ran = true;
            Ok(PathBuf::from("/source/bin/cargo-dylint"))
        },
    );
    assert!(result.is_err());
    assert!(!source_build_ran);
}

#[test]
fn dylint_dependency_cook_marker_is_private_to_front_door() {
    let args = vec![
        "+nightly-2026-04-16".to_string(),
        DYLINT_DEPENDENCY_COOK_FLAG.to_string(),
        "check".to_string(),
        "--".to_string(),
        DYLINT_DEPENDENCY_COOK_FLAG.to_string(),
    ];
    let (cleaned, found) = strip_dylint_dependency_cook_flag(&args);
    assert!(found);
    assert_eq!(
        cleaned,
        [
            "+nightly-2026-04-16",
            "check",
            "--",
            DYLINT_DEPENDENCY_COOK_FLAG
        ]
    );
}

#[test]
fn cargo_args_are_not_cacheable_for_static_analysis_tools() {
    // The three static-analysis tools in the registry don't spawn
    // rustc at all; engaging zccache would pay the session-start/stop
    // tax for zero hit value.
    for sub in ["deny", "audit", "machete"] {
        assert!(
            !cargo_args_are_cacheable(&argv(&[sub])),
            "subcommand {sub:?} must classify as non-cacheable (registry \
             says wraps_inner_cargo_build=false)",
        );
    }
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_short_exec_single_token() {
    assert!(cargo_args_are_cacheable(&argv(&["watch", "-x", "build"])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_short_exec_multi_token() {
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "-x",
        "build --release",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_long_exec_equals_form() {
    assert!(cargo_args_are_cacheable(&argv(&["watch", "--exec=build"])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "--exec=build --release",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_long_exec_space_form() {
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch", "--exec", "build",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_shell_form_strips_leading_cargo() {
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "-s",
        "cargo build --release",
    ])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "--shell",
        "cargo build --release",
    ])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "--shell=cargo build --release",
    ])));
}

#[test]
fn cargo_args_are_not_cacheable_for_watch_with_uncacheable_inner() {
    assert!(!cargo_args_are_cacheable(&argv(&["watch", "-x", "clean"])));
    assert!(!cargo_args_are_cacheable(&argv(&["watch", "-x", "fmt"])));
}

#[test]
fn cargo_args_apply_rustfmt_shim_for_direct_and_watch_fmt() {
    assert!(cargo_args_should_apply_rustfmt_shim(&argv(&["fmt"])));
    assert!(cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch", "-x", "fmt",
    ])));
    assert!(cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch",
        "--exec=fmt --check",
    ])));
    assert!(cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch",
        "-s",
        "cargo fmt --check",
    ])));
    assert!(cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch",
        "--shell",
        "cargo +nightly fmt --check",
    ])));
}

#[test]
fn cargo_args_do_not_apply_rustfmt_shim_for_non_fmt_watch() {
    assert!(!cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch", "-x", "build",
    ])));
    assert!(!cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch",
        "-s",
        "cargo clean",
    ])));
    assert!(!cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch", "--", "-x", "fmt",
    ])));
}

#[test]
fn rustfmt_shim_env_is_applied_to_watch_fmt_when_cache_enabled() {
    let _lock = ENV_LOCK.lock();
    let _guards = remove_env_vars(&["RUSTFMT"]);

    for args in [
        argv(&["watch", "-x", "fmt"]),
        argv(&["watch", "-s", "cargo fmt --check"]),
    ] {
        let mut command = std::process::Command::new("cargo");
        let guard = maybe_apply_rustfmt_zccache_shim(&mut command, &args, true)
            .expect("watch fmt should get a rustfmt shim");

        let rustfmt = command_env_override(&command, "RUSTFMT")
            .expect("RUSTFMT env override")
            .expect("RUSTFMT env value");
        let rustfmt_path = PathBuf::from(rustfmt);
        assert!(
            rustfmt_path.is_file(),
            "RUSTFMT shim should exist while guard is alive: {}",
            rustfmt_path.display()
        );
        let expected_name = format!("rustfmt{}", std::env::consts::EXE_SUFFIX);
        assert_eq!(
            rustfmt_path.file_name().and_then(|name| name.to_str()),
            Some(expected_name.as_str())
        );
        assert_eq!(
            command_env_override(&command, crate::shim_dir::SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR)
                .flatten()
                .as_deref(),
            Some(OsStr::new("1")),
            "cargo-watch must inherit the child-shim recursion sentinel"
        );
        drop(guard);
    }
}

#[test]
fn rustfmt_shim_env_is_not_applied_when_cache_is_disabled() {
    let _lock = ENV_LOCK.lock();
    let _guards = remove_env_vars(&["RUSTFMT"]);
    let args = argv(&["watch", "-x", "fmt"]);
    let mut command = std::process::Command::new("cargo");

    assert!(maybe_apply_rustfmt_zccache_shim(&mut command, &args, false).is_none());
    assert!(
        command_env_override(&command, "RUSTFMT").is_none(),
        "cache-disabled cargo watch fmt should leave rustfmt direct"
    );
}

#[test]
fn cargo_args_are_cacheable_for_watch_when_any_inner_is_cacheable() {
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch", "-x", "build", "-x", "clean",
    ])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch", "-x", "clean", "-x", "build",
    ])));
}

#[test]
fn cargo_args_are_not_cacheable_for_bare_watch() {
    assert!(!cargo_args_are_cacheable(&argv(&["watch"])));
    assert!(!cargo_args_are_cacheable(&argv(&["watch", "--clear"])));
}

#[test]
fn cargo_args_ignore_exec_after_passthrough_separator() {
    // Anything after `--` is not parsed as a watch-flag value.
    assert!(!cargo_args_are_cacheable(&argv(&[
        "watch", "--", "-x", "build",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_inner_release_flag() {
    // `-x 'build --release'` — tokens after `build` should not break the
    // detection, and the outer cacheable answer is still true.
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "-x",
        "build --release --workspace",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_toolchain_pin() {
    // `+nightly` is a cargo toolchain shorthand that should be skipped when
    // locating the `watch` subcommand.
    assert!(cargo_args_are_cacheable(&argv(&[
        "+nightly", "watch", "-x", "build",
    ])));
}

// -------------------------------------------------------------------------
// Retired target-GC flags remain stripped as compatibility no-ops (#1818).
// -------------------------------------------------------------------------

#[test]
fn strip_no_gc_target_flag_removes_combined_form() {
    let cleaned = strip_no_gc_target_flags(&argv(&["build", "--no-gc-target", "--release"]));
    assert_eq!(cleaned, argv(&["build", "--release"]));
}

#[test]
fn strip_no_gc_target_flag_removes_before_only() {
    let cleaned = strip_no_gc_target_flags(&argv(&["check", "--no-gc-target-before"]));
    assert_eq!(cleaned, argv(&["check"]));
}

#[test]
fn strip_no_gc_target_flag_removes_after_only() {
    let cleaned =
        strip_no_gc_target_flags(&argv(&["build", "--no-gc-target-after", "--workspace"]));
    assert_eq!(cleaned, argv(&["build", "--workspace"]));
}

#[test]
fn strip_no_gc_target_flag_default_no_op() {
    let cleaned = strip_no_gc_target_flags(&argv(&["build", "--release"]));
    assert_eq!(cleaned, argv(&["build", "--release"]));
}

#[test]
fn strip_no_gc_target_flag_passes_through_after_separator() {
    // Flags after `--` belong to the program cargo runs and must not be
    // touched. This mirrors how `--no-trampoline` is handled.
    let cleaned = strip_no_gc_target_flags(&argv(&[
        "run",
        "--bin",
        "foo",
        "--",
        "--no-gc-target",
        "--no-gc-target-after",
    ]));
    assert_eq!(
        cleaned,
        argv(&[
            "run",
            "--bin",
            "foo",
            "--",
            "--no-gc-target",
            "--no-gc-target-after",
        ])
    );
}

#[test]
fn strip_no_gc_target_flag_handles_repeated_flags() {
    let cleaned = strip_no_gc_target_flags(&argv(&[
        "build",
        "--no-gc-target-before",
        "--no-gc-target-after",
    ]));
    assert_eq!(cleaned, argv(&["build"]));
}

// Issue #755: cargo built-in verbs must not trigger the fuzzy "did you
// mean: cargo X?" hint. The External arm hands them straight to cargo;
// treating them as typos is misleading.
#[test]
fn suggest_cargo_subcommand_typo_skips_cargo_builtin_verbs() {
    for verb in crate::cli_args::CARGO_BUILTIN_VERBS {
        assert_eq!(
            suggest_cargo_subcommand_typo(verb),
            None,
            "cargo built-in verb {verb:?} must not be suggested as a typo of a known subcommand",
        );
    }
}

#[test]
fn suggest_cargo_subcommand_typo_still_catches_genuine_typos() {
    // Regression guard for issue #412: a clear typo of a registered
    // cargo subcommand (e.g. `ntest` → `nextest`) still gets the hint.
    assert_eq!(
        suggest_cargo_subcommand_typo("ntest").as_deref(),
        Some("nextest"),
        "fuzzy hint must still fire for genuine typos of known cargo subcommands",
    );
}

#[test]
fn suggest_cargo_subcommand_typo_returns_none_for_unrelated_input() {
    // Sanity check: random garbage that isn't close to any candidate
    // gets no suggestion at all.
    assert_eq!(
        suggest_cargo_subcommand_typo("completely-made-up-name"),
        None,
    );
}

// Issue #816: SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS env-var handling.
// The bool guard parses truthy / falsy values consistently with the
// pattern other soldr env vars use.

#[test]
fn force_managed_cargo_subcommands_defaults_to_false_when_unset() {
    // Serialize on the env mutex used elsewhere in this file so we don't
    // race against other env-touching tests in the same binary.
    let _guard = ENV_LOCK.lock();
    let prev = std::env::var_os(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    // SAFETY: the test acquires ENV_LOCK to serialize against any other
    // test that mutates process env.
    unsafe {
        std::env::remove_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    }
    assert!(!force_managed_cargo_subcommands());
    if let Some(value) = prev {
        unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, value);
        }
    }
}

#[test]
fn force_managed_cargo_subcommands_parses_falsey_strings_as_false() {
    let _guard = ENV_LOCK.lock();
    let prev = std::env::var_os(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    for falsey in ["", " ", "0", "false", "no", "off", "  off  "] {
        unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, falsey);
        }
        assert!(
            !force_managed_cargo_subcommands(),
            "value {falsey:?} should parse as false",
        );
    }
    match prev {
        Some(value) => unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, value);
        },
        None => unsafe {
            std::env::remove_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
        },
    }
}

#[test]
fn force_managed_cargo_subcommands_parses_truthy_strings_as_true() {
    let _guard = ENV_LOCK.lock();
    let prev = std::env::var_os(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    for truthy in ["1", "true", "yes", "on", "TRUE", " on "] {
        unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, truthy);
        }
        assert!(
            force_managed_cargo_subcommands(),
            "value {truthy:?} should parse as true",
        );
    }
    match prev {
        Some(value) => unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, value);
        },
        None => unsafe {
            std::env::remove_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
        },
    }
}

#[test]
fn cargo_json_closure_includes_artifact_and_matching_fingerprint_tree() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target/debug");
    std::fs::create_dir_all(target.join("deps")).unwrap();
    std::fs::create_dir_all(target.join(".fingerprint/serde-abc")).unwrap();
    std::fs::write(target.join("deps/libserde-abc.rlib"), b"rlib").unwrap();
    std::fs::write(
        target.join(".fingerprint/serde-abc/dep-lib-serde"),
        b"fingerprint",
    )
    .unwrap();
    let json = format!(
        "{}\n{}\n",
        serde_json::json!({
            "reason": "compiler-artifact",
            "filenames": [target.join("deps/libserde-abc.rlib")],
        }),
        serde_json::json!({"reason": "build-finished", "success": true}),
    );

    let closure = parse_cargo_artifact_closure(json.as_bytes(), &target);
    assert!(closure.iter().any(|path| path == "deps/libserde-abc.rlib"));
    assert!(closure
        .iter()
        .any(|path| path == ".fingerprint/serde-abc/dep-lib-serde"));
}

#[test]
fn cargo_json_closure_rejects_unknown_messages_for_walker_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target/debug");
    std::fs::create_dir_all(&target).unwrap();
    let json = b"{\"reason\":\"future-cargo-message\"}\n";
    assert!(parse_cargo_artifact_closure(json, &target).is_empty());
}

#[test]
fn find_on_path_locates_executable_in_a_path_dir() {
    let _guard = ENV_LOCK.lock();
    let dir = tempfile::tempdir().unwrap();
    let exe_name = crate::platform::executable::name::native("soldr-test-find-on-path-fixture");
    let exe_path = dir.path().join(exe_name);
    std::fs::write(&exe_path, b"#!/bin/sh\nexit 0\n").unwrap();
    let source = std::fs::metadata(&exe_path).unwrap().permissions();
    crate::platform::fs::permissions::make_executable_from(&exe_path, &source).unwrap();

    let prev_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = std::ffi::OsString::from(dir.path());
    if !prev_path.is_empty() {
        let sep = crate::platform::host::facts::path_list_separator();
        new_path.push(sep);
        new_path.push(&prev_path);
    }
    unsafe {
        std::env::set_var("PATH", &new_path);
    }

    // The probe name is intentionally unsuffixed on both platforms — on
    // Windows the PATHEXT sweep in find_on_path picks up the `.exe`.
    let resolved = find_on_path("soldr-test-find-on-path-fixture");
    unsafe {
        std::env::set_var("PATH", &prev_path);
    }

    let resolved_path = resolved.expect("fixture must be found on PATH");
    assert!(
        resolved_path.is_file(),
        "resolved path {resolved_path:?} must exist",
    );
    assert!(
        resolved_path
            .parent()
            .map(|p| p == dir.path())
            .unwrap_or(false),
        "resolved path {resolved_path:?} must live under the fixture dir {:?}",
        dir.path(),
    );
}

#[test]
fn find_on_path_returns_none_when_missing() {
    let _guard = ENV_LOCK.lock();
    let dir = tempfile::tempdir().unwrap();
    let prev_path = std::env::var_os("PATH").unwrap_or_default();
    let new_path: std::ffi::OsString = dir.path().into();
    unsafe {
        std::env::set_var("PATH", &new_path);
    }
    let resolved = find_on_path("definitely-not-on-path-soldr-test-816");
    unsafe {
        std::env::set_var("PATH", &prev_path);
    }
    assert_eq!(resolved, None);
}

// ---------------------------------------------------------------------------
// compute_subcommand_env_overrides — fixes ring's build.rs picking the
// GNU clang driver instead of clang-cl when cross-compiling to
// *-pc-windows-msvc via cargo-xwin.
// ---------------------------------------------------------------------------

fn argvec(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

#[test]
fn nextest_archive_blessed_target_detects_archive_only() {
    assert_eq!(
        nextest_archive_blessed_target(&argvec(
            "nextest archive --target aarch64-apple-darwin --workspace"
        )),
        Some("aarch64-apple-darwin"),
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec(
            "--manifest-path Cargo.toml nextest archive --target=x86_64-apple-darwin"
        )),
        Some("x86_64-apple-darwin"),
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec(
            "nextest --color always archive --target=x86_64-apple-darwin"
        )),
        Some("x86_64-apple-darwin"),
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec("nextest run --archive-file dist/tests.tar.zst")),
        None,
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec("nextest run archive --target x86_64-apple-darwin")),
        None,
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec(
            "nextest archive --target x86_64-unknown-linux-musl"
        )),
        None,
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec("nextest archive --target x86_64-pc-windows-msvc")),
        Some("x86_64-pc-windows-msvc"),
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec("nextest archive --target aarch64-pc-windows-msvc")),
        Some("aarch64-pc-windows-msvc"),
    );
}

#[test]
fn nextest_archive_zig_target_detects_linux_archive_targets_only() {
    for target in [
        "aarch64-unknown-linux-gnu",
        "aarch64-unknown-linux-musl",
        "x86_64-unknown-linux-musl",
    ] {
        assert_eq!(
            nextest_archive_zig_target(&argvec(&format!(
                "nextest archive --target {target} --workspace"
            ))),
            Some(target),
        );
    }
    assert_eq!(
        nextest_archive_zig_target(&argvec("nextest run --target aarch64-unknown-linux-gnu")),
        None,
    );
    assert_eq!(
        nextest_archive_zig_target(&argvec("nextest archive --target x86_64-unknown-linux-gnu")),
        None,
    );
}

#[test]
fn nextest_archive_linux_bootstrap_reconstructs_zig_linker_env() {
    let _lock = ENV_LOCK.lock();
    let tmp = tempfile::tempdir().unwrap();
    let zig = tmp
        .path()
        .join(crate::platform::executable::name::native("zig"));
    std::fs::write(&zig, b"fake zig").unwrap();
    let _zig = EnvVarGuard::set("ZIG", &zig);

    let targets = [
        (
            "aarch64-unknown-linux-gnu",
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER",
        ),
        (
            "aarch64-unknown-linux-musl",
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER",
        ),
    ];
    let _linkers = remove_env_vars(&targets.map(|(_, key)| key));
    for (target, linker_key) in targets {
        let paths = SoldrPaths::with_root(tmp.path().join(target));
        let mut bin_dirs = Vec::new();
        let mut env = Vec::new();
        let mut cargo_args = Vec::new();

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(append_subcommand_transitive_bin_dirs(
                "nextest",
                &argvec(&format!("nextest archive --target {target} --workspace")),
                &paths,
                &mut bin_dirs,
                &mut env,
                &mut cargo_args,
            ))
            .unwrap();

        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert!(bin_dirs.iter().any(|dir| dir == zig.parent().unwrap()));
        assert!(
            map.get(linker_key)
                .is_some_and(|value| value.contains("zigbuild-shims") && value.contains(target)),
            "{target} archive must reconstruct its target linker: {map:?}"
        );
    }
}

#[test]
fn arm_cross_linker_preflight_rejects_missing_or_host_fallback() {
    let target = "aarch64-unknown-linux-gnu";
    assert!(validate_zig_cross_linker(target, None).is_err());
    for linker in ["clang", "clang-18", "cc", "gcc", "ld"] {
        assert!(
            validate_zig_cross_linker(target, Some(std::ffi::OsStr::new(linker))).is_err(),
            "bare host linker {linker} must fail before ARM objects are built"
        );
    }
    assert!(validate_zig_cross_linker(
        target,
        Some(std::ffi::OsStr::new(
            "/tmp/zigbuild-shims/aarch64-unknown-linux-gnu/cc"
        ))
    )
    .is_ok());
}

#[test]
fn build_env_cache_inputs_include_cross_toolchain_identity() {
    let _lock = ENV_LOCK.lock();
    let _linker = EnvVarGuard::set(
        "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER",
        "/tmp/arm-linker",
    );
    let _cc = EnvVarGuard::set("CC_aarch64_unknown_linux_gnu", "/tmp/arm-cc");

    let first_inputs = build_env_inputs(None);
    let first_hash = stable_hash_json(&first_inputs);
    let inputs: std::collections::HashMap<_, _> = first_inputs.into_iter().collect();
    assert_eq!(
        inputs
            .get("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER")
            .map(String::as_str),
        Some("/tmp/arm-linker")
    );
    assert_eq!(
        inputs
            .get("CC_aarch64_unknown_linux_gnu")
            .map(String::as_str),
        Some("/tmp/arm-cc")
    );

    std::env::set_var(
        "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER",
        "/tmp/different-arm-linker",
    );
    let second_hash = stable_hash_json(&build_env_inputs(None));
    assert_ne!(
        first_hash, second_hash,
        "changing target linker identity must invalidate restored build metadata"
    );
}

#[test]
fn nextest_archive_darwin_bootstrap_reuses_blessed_env() {
    let _lock = ENV_LOCK.lock();
    let tmp = tempfile::tempdir().unwrap();
    let sdk = tmp.path().join("MacOSX.fake.sdk");
    let llvm_bin = tmp.path().join("llvm-bin");
    let fake_dsymutil = llvm_bin.join(crate::platform::executable::name::native("dsymutil"));
    std::fs::create_dir_all(&sdk).unwrap();
    std::fs::create_dir_all(&llvm_bin).unwrap();
    std::fs::write(&fake_dsymutil, b"fake dsymutil").unwrap();

    let _sdkroot = EnvVarGuard::set("SDKROOT", &sdk);
    let _llvm = EnvVarGuard::set("SOLDR_LLVM_DIR", &llvm_bin);
    let _dsymutil = EnvVarGuard::set("SOLDR_DSYMUTIL", &fake_dsymutil);
    let _legacy_sys = EnvVarGuard::set(crate::blessed_build::USE_LEGACY_VENDORED_SYS_ENV_VAR, "1");
    let _system_cmake = EnvVarGuard::set(crate::blessed_build::USE_SYSTEM_CMAKE_ENV_VAR, "1");

    let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
    let mut bin_dirs = Vec::new();
    let mut env = Vec::new();
    let mut cargo_args = Vec::new();

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(append_subcommand_transitive_bin_dirs(
            "nextest",
            &argvec("nextest archive --target x86_64-apple-darwin --workspace"),
            &paths,
            &mut bin_dirs,
            &mut env,
            &mut cargo_args,
        ))
        .unwrap();

    assert!(
        bin_dirs.iter().any(|dir| dir == &llvm_bin),
        "managed LLVM bin dir must be prepended for darwin nextest archive: {bin_dirs:?}"
    );
    assert!(
        cargo_args.is_empty(),
        "syslib overrides are opted out in this hermetic test"
    );

    let map: std::collections::HashMap<_, _> = env.into_iter().collect();
    assert_eq!(
        map.get("SDKROOT").map(String::as_str),
        Some(sdk.to_str().unwrap()),
    );
    assert!(
        map.get("CC_x86_64_apple_darwin")
            .is_some_and(|value| value.contains("--target=x86_64-apple-darwin")
                && value.contains("-isysroot")),
        "CC_x86_64_apple_darwin must target the Apple SDK: {map:?}"
    );
    assert_eq!(
        map.get("CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER")
            .map(String::as_str),
        Some("clang"),
    );
    assert!(
        map.get("CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS")
            .is_some_and(|value| value.contains("-fuse-ld=lld")
                && value.contains("-mmacosx-version-min=10.12")),
        "x86_64 darwin rustflags: clang/lld + SDK at the 10.12 floor: {map:?}"
    );
}

#[test]
fn explicit_cross_linker_and_rustflags_override_soldr_fast() {
    let _lock = ENV_LOCK.lock();
    let _soldr_linker = EnvVarGuard::set("SOLDR_LINKER", "fast");
    let root = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(root.path().join("soldr"));

    for (target, linker_key, rustflags_key) in [
        (
            "aarch64-unknown-linux-gnu",
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER",
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
        ),
        (
            "aarch64-unknown-linux-musl",
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER",
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS",
        ),
    ] {
        let mut command = std::process::Command::new("cargo");
        command.env(linker_key, "/tmp/zigbuild-shims/target-linker");
        command.env(rustflags_key, "-C link-self-contained=no");
        target::apply_linker_override_blocking(
            &mut command,
            &argvec(&format!("build --target {target}")),
            None,
            &paths,
        )
        .unwrap();

        assert_eq!(
            command_env_override(&command, linker_key),
            Some(Some("/tmp/zigbuild-shims/target-linker".into())),
            "SOLDR_LINKER=fast must not replace the explicit {target} linker",
        );
        assert_eq!(
            command_env_override(&command, rustflags_key),
            Some(Some("-C link-self-contained=no".into())),
            "SOLDR_LINKER=fast must not replace the explicit {target} rustflags",
        );
    }
}

#[test]
fn command_target_linker_overrides_parent_environment() {
    let _lock = ENV_LOCK.lock();
    let _parent = EnvVarGuard::set(
        "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER",
        "/tmp/parent-linker",
    );
    let _soldr_linker = EnvVarGuard::set("SOLDR_LINKER", "fast");
    let root = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let mut command = std::process::Command::new("cargo");
    command.env(
        "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER",
        "/tmp/command-linker",
    );

    target::apply_linker_override_blocking(
        &mut command,
        &argvec("build --target aarch64-unknown-linux-gnu"),
        None,
        &paths,
    )
    .unwrap();

    assert_eq!(
        command_env_override(&command, "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER"),
        Some(Some("/tmp/command-linker".into())),
    );
}

#[test]
fn known_cargo_build_target_uses_explicit_target_arg() {
    let _lock = ENV_LOCK.lock();
    let _guard = EnvVarGuard::remove("CARGO_BUILD_TARGET");

    assert_eq!(
        target::known_cargo_build_target(
            &argvec("build --release --target x86_64-apple-darwin"),
            None,
        ),
        Some("x86_64-apple-darwin".to_string()),
    );
    assert_eq!(
        target::known_cargo_build_target(
            &argvec("build --release --target=aarch64-apple-darwin"),
            None,
        ),
        Some("aarch64-apple-darwin".to_string()),
    );
}

#[test]
fn known_cargo_build_target_prefers_defaulted_target_then_env() {
    let _lock = ENV_LOCK.lock();
    let _guard = EnvVarGuard::set("CARGO_BUILD_TARGET", "x86_64-unknown-linux-musl");

    assert_eq!(
        target::known_cargo_build_target(&argvec("build"), Some("x86_64-pc-windows-msvc")),
        Some("x86_64-pc-windows-msvc".to_string()),
    );
    assert_eq!(
        target::known_cargo_build_target(&argvec("build"), None),
        Some("x86_64-unknown-linux-musl".to_string()),
    );
}

#[test]
fn xwin_arm64_msvc_target_injects_cc_clang_cl_env() {
    let env = compute_subcommand_env_overrides(&argvec(
        "xwin build --release --target aarch64-pc-windows-msvc",
    ));
    let map: std::collections::HashMap<_, _> = env.into_iter().collect();
    assert_eq!(
        map.get("CC_aarch64_pc_windows_msvc").map(String::as_str),
        Some("clang-cl"),
    );
    assert_eq!(
        map.get("CXX_aarch64_pc_windows_msvc").map(String::as_str),
        Some("clang-cl"),
    );
    assert_eq!(
        map.get("AR_aarch64_pc_windows_msvc").map(String::as_str),
        Some("llvm-lib"),
    );
}

#[test]
fn xwin_x64_msvc_target_injects_underscored_triple_keys() {
    let env = compute_subcommand_env_overrides(&argvec(
        "xwin build --target x86_64-pc-windows-msvc --release",
    ));
    let map: std::collections::HashMap<_, _> = env.into_iter().collect();
    assert_eq!(
        map.get("CC_x86_64_pc_windows_msvc").map(String::as_str),
        Some("clang-cl"),
    );
}

#[test]
fn zigbuild_does_not_inject_cc_overrides_even_for_msvc_target() {
    // cargo-zigbuild also builds for windows-msvc but uses zig as the
    // linker — cc-rs's clang / clang-cl distinction is xwin-specific.
    let env = compute_subcommand_env_overrides(&argvec(
        "zigbuild --target x86_64-pc-windows-msvc --release",
    ));
    assert!(
        env.is_empty(),
        "zigbuild lane shouldn't inject xwin env: {env:?}"
    );
}

#[test]
fn zigbuild_env_overrides_include_cc_and_linker_for_supported_target() {
    let _lock = ENV_LOCK.lock();
    let expected_keys = [
        "CC_aarch64_unknown_linux_musl",
        "CXX_aarch64_unknown_linux_musl",
        "AR_aarch64_unknown_linux_musl",
        "RANLIB_aarch64_unknown_linux_musl",
        "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER",
    ];
    let _guards = remove_env_vars(&expected_keys);

    let dir = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(dir.path().join("soldr"));
    let mut env = Vec::new();

    append_zigbuild_env_overrides(&paths, "aarch64-unknown-linux-musl", &mut env).unwrap();

    let map: std::collections::HashMap<_, _> = env.into_iter().collect();
    for key in expected_keys {
        let value = map.get(key).unwrap_or_else(|| panic!("missing {key}"));
        assert!(
            value.contains("zigbuild-shims"),
            "{key} should point at generated zigbuild shim, got {value}"
        );
    }
}

#[test]
fn xwin_with_non_msvc_target_does_not_inject_anything() {
    let env =
        compute_subcommand_env_overrides(&argvec("xwin build --target x86_64-unknown-linux-gnu"));
    assert!(
        env.is_empty(),
        "non-msvc target shouldn't inject xwin env: {env:?}"
    );
}

#[test]
fn xwin_without_target_does_not_inject_anything() {
    // The injection is keyed on an explicit triple (so we can name the
    // env vars); skip when no triple is in the args.
    let env = compute_subcommand_env_overrides(&argvec("xwin build --release"));
    assert!(
        env.is_empty(),
        "no --target shouldn't inject xwin env: {env:?}"
    );
}

#[test]
fn xwin_download_subverb_does_not_inject() {
    // `cargo xwin download` only fetches the MSVC SDK; cc-rs never
    // runs, so no env injection is needed.
    let env =
        compute_subcommand_env_overrides(&argvec("xwin download --target aarch64-pc-windows-msvc"));
    assert!(
        env.is_empty(),
        "xwin download shouldn't inject env: {env:?}"
    );
}

#[test]
fn non_xwin_subcommand_does_not_inject_anything() {
    let env = compute_subcommand_env_overrides(&argvec(
        "build --target x86_64-pc-windows-msvc --release",
    ));
    assert!(
        env.is_empty(),
        "bare cargo build shouldn't inject xwin env: {env:?}"
    );
}

#[test]
fn zlib_ng_arm_wrapper_written_only_for_aarch64_msvc() {
    let dir = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(dir.path().join("soldr"));

    // Non-arm / non-msvc triples: no wrapper, no env.
    for triple in [
        "x86_64-pc-windows-msvc",
        "aarch64-unknown-linux-musl",
        "aarch64-apple-darwin",
    ] {
        let got = ensure_zlib_ng_arm_cmake_wrapper(&paths, triple).unwrap();
        assert!(got.is_none(), "{triple} must not get the wrapper");
    }

    // The arm-msvc lane gets the DASH-triple env var (the form the
    // cmake crate checks before cargo-xwin's underscore form) plus a
    // wrapper that chain-includes cargo-xwin's file and disables the
    // clang-cl-incompatible zlib-ng ARM toggles.
    let (key, value) = ensure_zlib_ng_arm_cmake_wrapper(&paths, "aarch64-pc-windows-msvc")
        .unwrap()
        .expect("aarch64-pc-windows-msvc gets the wrapper");
    assert_eq!(key, "CMAKE_TOOLCHAIN_FILE_aarch64-pc-windows-msvc");
    let body = std::fs::read_to_string(&value).expect("wrapper file exists");
    assert!(
        body.contains("$ENV{CMAKE_TOOLCHAIN_FILE_aarch64_pc_windows_msvc}"),
        "wrapper must chain-include cargo-xwin's underscore-form toolchain file: {body}"
    );
    for toggle in ["WITH_NEON OFF", "WITH_ARMV8 OFF", "WITH_ARMV6 OFF"] {
        assert!(body.contains(toggle), "wrapper must force {toggle}: {body}");
    }

    // Idempotent: second call rewrites the same path.
    let (key2, value2) = ensure_zlib_ng_arm_cmake_wrapper(&paths, "aarch64-pc-windows-msvc")
        .unwrap()
        .expect("second call still yields the wrapper");
    assert_eq!((key, value), (key2, value2));
}

#[test]
fn journal_miss_reasons_parse_build_scoped_jsonl() {
    let body = [
        r#"{"outcome":"hit","miss_reason":"ignored"}"#,
        r#"{"outcome":"miss","miss_reason":"context_not_found"}"#,
        r#"{"outcome":"miss","miss_reason":"context_not_found"}"#,
        r#"{"outcome":"link_miss","miss_reason":"no_artifact_for_key"}"#,
        r#"{"outcome":"miss"}"#,
        "not-json",
    ]
    .join("\n");

    let reasons = parse_build_miss_reasons_from_journal(&body);

    assert_eq!(reasons.len(), 3);
    assert_eq!(reasons[0].reason, "context_not_found");
    assert_eq!(reasons[0].count, 2);
    assert_eq!(reasons[1].reason, "no_artifact_for_key");
    assert_eq!(reasons[1].count, 1);
    assert_eq!(reasons[2].reason, "unknown");
    assert_eq!(reasons[2].count, 1);
}

#[test]
fn embedded_compile_journal_path_matches_service_layout() {
    let root = tempfile::tempdir().expect("temp root");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));

    assert_eq!(
        embedded_compile_journal_path(&paths),
        crate::zccache_embedded::embedded_cache_root(&paths)
            .join(zccache::core::config::versioned_subdir())
            .join("logs")
            .join("compile_journal.jsonl")
    );
}

#[test]
fn miss_reasons_do_not_fall_back_to_full_global_journal() {
    let root = tempfile::tempdir().expect("temp root");
    let global_journal = root.path().join("compile_journal.jsonl");
    std::fs::write(
        &global_journal,
        r#"{"outcome":"miss","miss_reason":"old_build"}"#,
    )
    .expect("write old global journal");
    let reasons = read_build_miss_reasons(None);

    assert!(
        reasons.is_empty(),
        "missing archived tail must not parse unrelated global journal entries"
    );
}

#[test]
fn compile_journal_tail_archive_keeps_current_build_only() {
    let root = tempfile::tempdir().expect("temp root");
    let source = root.path().join("compile_journal.jsonl");
    std::fs::write(&source, "{\"record\":\"old-build\"}\n").expect("write old journal");
    let start_offset = std::fs::metadata(&source).expect("metadata").len();
    std::fs::write(
        &source,
        "{\"record\":\"old-build\"}\n{\"record\":\"new-build-1\"}\n{\"record\":\"new-build-2\"}\n",
    )
    .expect("append journal");

    let archived = copy_session_artifact_tail(
        &source,
        &root.path().join("history").join("1"),
        "compile_journal.jsonl",
        start_offset,
    )
    .expect("archive path");

    let body = std::fs::read_to_string(archived).expect("archived body");
    assert_eq!(
        body,
        "{\"record\":\"new-build-1\"}\n{\"record\":\"new-build-2\"}\n"
    );
}

#[test]
fn compile_journal_tail_waits_for_expected_entries() {
    let root = tempfile::tempdir().expect("temp root");
    let source = root.path().join("compile_journal.jsonl");
    std::fs::write(&source, "old-build\n").expect("write old journal");
    let start_offset = std::fs::metadata(&source).expect("metadata").len();
    let writer_source = source.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(&writer_source, "old-build\nnew-build-1\n").expect("write first tail");
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(&writer_source, "old-build\nnew-build-1\nnew-build-2\n")
            .expect("write second tail");
    });

    assert!(wait_for_compile_journal_tail(&source, start_offset, 2));
    assert_eq!(
        count_complete_compile_journal_tail_entries(&source, start_offset),
        Some(2)
    );
}

#[test]
fn compile_journal_tail_ready_journal_returns_without_sleeping() {
    // soldr#1536: the pre-#1536 wait demanded three consecutive 25 ms
    // "stable" polls (a fixed ~75 ms floor per build) even when the
    // journal already held every expected entry. A complete journal
    // must now return on the first check with ZERO sleeps.
    let root = tempfile::tempdir().expect("temp root");
    let source = root.path().join("compile_journal.jsonl");
    std::fs::write(&source, "old-build\n").expect("write old journal");
    let start_offset = std::fs::metadata(&source).expect("metadata").len();
    std::fs::write(&source, "old-build\nnew-build-1\nnew-build-2\n").expect("append tail");

    let mut sleeps = 0usize;
    let ready = wait_for_compile_journal_tail_with(
        &source,
        start_offset,
        2,
        Duration::from_secs(2),
        || sleeps += 1,
    );
    assert!(ready);
    assert_eq!(
        sleeps, 0,
        "a complete journal must not pay any polling floor"
    );
}

#[test]
fn compile_journal_tail_partial_trailing_line_is_not_complete() {
    // A trailing line without its newline is still being written (by
    // the journal thread or a concurrent build) — it must not count as
    // a complete entry. Completion of the line on the next poll ends
    // the wait.
    let root = tempfile::tempdir().expect("temp root");
    let source = root.path().join("compile_journal.jsonl");
    std::fs::write(&source, "old-build\n").expect("write old journal");
    let start_offset = std::fs::metadata(&source).expect("metadata").len();
    std::fs::write(&source, "old-build\nnew-build-1\nnew-build-2").expect("partial tail");

    assert_eq!(
        count_complete_compile_journal_tail_entries(&source, start_offset),
        Some(1)
    );

    // The injected "sleep" completes the trailing line — the wait must
    // only return after that completion (i.e. the partial line alone
    // did not satisfy it).
    let finisher = source.clone();
    let ready = wait_for_compile_journal_tail_with(
        &source,
        start_offset,
        2,
        Duration::from_secs(2),
        move || {
            std::fs::write(&finisher, "old-build\nnew-build-1\nnew-build-2\n")
                .expect("finish tail");
        },
    );
    assert!(ready);
    assert_eq!(
        count_complete_compile_journal_tail_entries(&source, start_offset),
        Some(2),
        "wait must have returned only after the line was completed"
    );
}

#[test]
fn compile_journal_tail_archive_drops_partial_trailing_line() {
    let root = tempfile::tempdir().expect("temp root");
    let source = root.path().join("compile_journal.jsonl");
    std::fs::write(&source, "{\"record\":\"old-build\"}\n").expect("write old journal");
    let start_offset = std::fs::metadata(&source).expect("metadata").len();
    std::fs::write(
        &source,
        "{\"record\":\"old-build\"}\n{\"record\":\"complete-1\"}\n{\"record\":\"complete-2\"}\n{\"record\":\"part",
    )
    .expect("append tail");

    let archived = copy_session_artifact_tail(
        &source,
        &root.path().join("history").join("2"),
        "compile_journal.jsonl",
        start_offset,
    )
    .expect("archive path");

    let body = std::fs::read_to_string(archived).expect("archived body");
    assert_eq!(
        body, "{\"record\":\"complete-1\"}\n{\"record\":\"complete-2\"}\n",
        "an in-flight partial trailing line must not land in the archive"
    );
}

#[test]
fn compile_journal_history_reuses_upstream_secret_fixture() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/zccache/compile_journal_env_security_v1.json"
    ))
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("compile_journal.jsonl");
    let mut record = serde_json::json!({
        "outcome": "miss",
        "compiler": "/usr/bin/rustc",
        "args": ["--crate-name", "fixture"],
        "cwd": "/repo",
        "exit_code": 0,
    });
    record["env"] = fixture["input_env"].clone();
    std::fs::write(
        &source,
        format!("{}\n", serde_json::to_string(&record).unwrap()),
    )
    .unwrap();
    let archived = copy_session_artifact_tail(
        &source,
        &temp.path().join("history/1"),
        "compile_journal.jsonl",
        0,
    )
    .expect("sanitized archive");
    let archived = std::fs::read_to_string(archived).unwrap();
    for fragment in fixture["forbidden_fragments"].as_array().unwrap() {
        let fragment = fragment.as_str().unwrap();
        assert!(
            !archived.contains(fragment),
            "secret fragment persisted: {fragment}"
        );
    }
    for name in fixture["forbidden_names"].as_array().unwrap() {
        let name = name.as_str().unwrap();
        assert!(!archived.contains(name), "secret name persisted: {name}");
    }
    assert!(archived.contains("CARGO_CRATE_NAME"));
    assert!(archived.contains("safe_crate"));
}

#[test]
fn persist_build_log_history_trusts_daemon_finalized_aggregate() {
    // soldr#1536: when the daemon acknowledged BuildSessionEnd the
    // persisted record already carries the finalized aggregate. The
    // wrapper must keep it verbatim instead of re-deriving it with a
    // full `daemon_events` scan (which here would zero it out —
    // there are no events in this fresh redb).
    let root = tempfile::tempdir().expect("temp root");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let db_path = crate::cache_lib::data_db_path(&paths);
    let mut record = new_build_record(4242, "/repo".to_string(), 1_000);
    record.crate_count = 7;
    record.slowest_crate_us = Some(9_000);
    record.slowest_crate_name = Some("daemon-said-so".to_string());
    record.ended_at_ms = Some(2_000);
    record.exit_code = Some(0);
    crate::daemon::db::upsert_build(&db_path, &record).expect("seed record");

    let session_dir = root.path().join("zc");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let session = crate::build_cache_session::BuildCacheSession {
        cache_dir: session_dir.clone(),
        session_id: "test-session".to_string(),
        session_log_path: session_dir.join("last-session.log"),
        journal_path: session_dir.join("last-session.jsonl"),
        session_stats_path: session_dir.join("last-session-stats.json"),
    };

    for daemon_finalized in [true, false] {
        persist_build_log_history_inner(&BuildLogHistoryRequest {
            paths: &paths,
            build_session_id: 4242,
            repo_root: Path::new("/repo"),
            started_at_ms: 1_000,
            session: &session,
            compile_journal_start_len: 0,
            exit_code: 0,
            ended_at_ms: 2_000,
            daemon_finalized,
        })
        .expect("persist history");
        let stored = crate::daemon::db::get_build(&db_path, 4242)
            .expect("read")
            .expect("record");
        if daemon_finalized {
            assert_eq!(stored.crate_count, 7, "daemon aggregate must be kept");
            assert_eq!(stored.slowest_crate_us, Some(9_000));
            assert_eq!(stored.slowest_crate_name.as_deref(), Some("daemon-said-so"));
        } else {
            assert_eq!(
                stored.crate_count, 0,
                "fallback path re-derives the aggregate from (empty) events"
            );
        }
    }
}

#[test]
fn persist_build_log_history_excludes_stale_legacy_session_files() {
    // Regression for #1827: fixed-name legacy files belong to no current
    // embedded-service build and must never enter per-build history.
    let root = tempfile::tempdir().expect("temp root");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let session_dir = root.path().join("zc");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let session = crate::build_cache_session::BuildCacheSession {
        cache_dir: session_dir.clone(),
        session_id: "stale-global-session".to_string(),
        session_log_path: session_dir.join("last-session.log"),
        journal_path: session_dir.join("last-session.jsonl"),
        session_stats_path: session_dir.join("last-session-stats.json"),
    };
    let sentinel = "DO_NOT_ARCHIVE_THIS_LEGACY_SECRET";
    std::fs::write(&session.session_log_path, sentinel).expect("legacy log");
    std::fs::write(
        &session.journal_path,
        format!(r#"{{"env":[["TOKEN","{sentinel}"]]}}"#),
    )
    .expect("legacy journal");
    std::fs::write(
        &session.session_stats_path,
        r#"{"status":"ok","hits":1,"misses":0,"compilations":1}"#,
    )
    .expect("stats");

    let compile_journal = embedded_compile_journal_path(&paths);
    std::fs::create_dir_all(compile_journal.parent().unwrap()).expect("journal parent");
    let mut compile_journal_start_len = 0;
    for (build_session_id, crate_name) in [(5150, "first"), (5151, "second")] {
        use std::io::Write as _;
        let mut journal = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&compile_journal)
            .expect("open compile journal");
        writeln!(
            journal,
            "{{\"outcome\":\"hit\",\"crate_name\":\"{crate_name}\"}}"
        )
        .expect("append compile journal");
        drop(journal);

        persist_build_log_history_inner(&BuildLogHistoryRequest {
            paths: &paths,
            build_session_id,
            repo_root: Path::new("/repo"),
            started_at_ms: 1_000,
            session: &session,
            compile_journal_start_len,
            exit_code: 0,
            ended_at_ms: 2_000,
            daemon_finalized: true,
        })
        .expect("persist history");
        compile_journal_start_len = std::fs::metadata(&compile_journal)
            .expect("journal metadata")
            .len();

        let archive = build_log_history_dir(&paths, build_session_id);
        assert!(!archive.join("last-session.log").exists());
        assert!(!archive.join("last-session.jsonl").exists());
        let archive_body = std::fs::read_to_string(archive.join("compile_journal.jsonl"))
            .expect("build-scoped journal");
        assert!(archive_body.contains(crate_name));
        assert!(!archive_body.contains(sentinel));

        let record =
            crate::daemon::db::get_build(&crate::cache_lib::data_db_path(&paths), build_session_id)
                .expect("read build")
                .expect("record");
        let log_paths = record.log_paths.expect("log paths");
        assert!(log_paths.session_log_path.is_none());
        assert!(log_paths.journal_path.is_none());
        assert!(log_paths.archived_session_log_path.is_none());
        assert!(log_paths.archived_journal_path.is_none());
        assert!(log_paths.archived_session_stats_path.is_some());
        assert!(log_paths.archived_compile_journal_path.is_some());
    }
}

#[test]
fn build_session_bookkeeping_never_falls_back_to_direct_state_db() {
    let root = tempfile::tempdir().expect("temp root");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");

    let db_path = crate::cache_lib::data_db_path(&paths);
    super::build_session::start_and_warn_on_jobs_drift(&paths, 99, &repo, 1_000);
    super::build_session::persist_build_session_end_fallback(&paths, 99, 0, 1_250);

    assert!(
        !db_path.exists(),
        "unavailable-daemon session bookkeeping must not open state.sqlite3 directly"
    );
}

#[test]
fn build_session_waits_for_root_lease() {
    let root = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let maintenance = crate::cache_lib::build_active::MaintenanceLease::try_acquire(&paths)
        .unwrap()
        .unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();

    let worker = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        // Acquiring the build-activity lease must block until the maintenance
        // pass releases the root, or a build could race a destructive GC
        // (soldr#1667). The paired BuildSessionStart publish is now a separate
        // caller step, so this asserts the lease-acquire wait directly.
        let result = begin_build_activity_lease(&paths, 7);
        done_tx.send(result.is_ok()).unwrap();
    });

    started_rx.recv().unwrap();
    assert!(done_rx
        .recv_timeout(std::time::Duration::from_millis(100))
        .is_err());

    drop(maintenance);

    assert!(done_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap());
    worker.join().unwrap();
}

#[test]
fn compile_journal_history_uses_effective_embedded_version_root() {
    let root = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let journal = embedded_compile_journal_path(&paths);
    assert_eq!(
        journal,
        crate::zccache_embedded::embedded_cache_root(&paths)
            .join(zccache::core::config::versioned_subdir())
            .join("logs/compile_journal.jsonl")
    );
}

// soldr#1790: rendering-level RED->GREEN evidence for the build-log writer. Since
// soldr#1814/#2257, daemon-owned history reaches production exclusively over IPC; this
// fixture injects the response payload rather than reopening state.sqlite3 from the CLI.
#[test]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn write_build_log_reflects_seeded_compile_session_events() {
    let root = tempfile::tempdir().expect("temp root");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let cwd_dir = root.path().join("project");
    std::fs::create_dir_all(&cwd_dir).expect("mkdir cwd");

    let session_id = 9001_u64;
    let events = vec![
        crate::daemon::db::Event {
            ts_ms: 1_000,
            session_id: Some(session_id),
            kind: crate::daemon::db::EventKind::CompileStart,
            crate_name: Some("crate-a".to_string()),
            duration_us: None,
            target_dir: None,
            exit_code: None,
        },
        crate::daemon::db::Event {
            ts_ms: 1_500,
            session_id: Some(session_id),
            kind: crate::daemon::db::EventKind::CompileEnd,
            crate_name: Some("crate-a".to_string()),
            duration_us: Some(500_000),
            target_dir: None,
            exit_code: Some(0),
        },
        crate::daemon::db::Event {
            ts_ms: 1_600,
            session_id: Some(session_id),
            kind: crate::daemon::db::EventKind::CompileEnd,
            crate_name: Some("crate-b".to_string()),
            duration_us: Some(100_000),
            target_dir: None,
            exit_code: Some(0),
        },
    ];

    let args = vec![
        "soldr".to_string(),
        "cargo".to_string(),
        "build".to_string(),
    ];
    let request = crate::build_log::BuildLogRequest {
        paths: &paths,
        session_id,
        cwd: &cwd_dir,
        args: &args,
        started_at_ms: 1_000,
        ended_at_ms: 1_600,
        exit_code: 0,
        compile_journal_path: None,
        compile_journal_start_len: 0,
        // soldr#1799: a managed-home binary must render `managed` (checkable).
        toolchain: Some(crate::build_log::ToolchainHomes {
            home_origin: "managed",
            binary: paths.root.join("cargo").join("bin").join("cargo"),
        }),
        wrapper: None,
        fingerprint_dirty: Vec::new(),
    };

    let path = crate::build_log::write_build_log_with_history_for_test(&request, &events)
        .expect("write_build_log");
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("xml"));
    let raw = std::fs::read_to_string(&path).expect("read build log");
    // soldr#1799: the discriminant CI keys on, plus the binary that justifies
    // it. Without the path, home_origin="managed" is unfalsifiable.
    assert!(
        raw.contains("<toolchain") && raw.contains("home_origin=\"managed\""),
        "build log must carry the toolchain home origin, got:
{raw}"
    );
    assert!(
        raw.contains("binary=\""),
        "build log must name the resolved binary, got:
{raw}"
    );
    assert!(
        raw.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"),
        "must start with the XML declaration: {raw}"
    );

    // Both crates must be present as compile items.
    assert!(
        raw.contains("<item crate=\"crate-a\" duration_ms=\"500\" cache=\"unknown\"/>"),
        "crate-a compile item: {raw}"
    );
    assert!(
        raw.contains("<item crate=\"crate-b\" duration_ms=\"100\" cache=\"unknown\"/>"),
        "crate-b compile item: {raw}"
    );

    // Derived link section: crate-b has the later CompileEnd, so it is
    // treated as the linking crate. It also still appears in the
    // compile section above (intentional, not a double-count bug).
    assert!(
        raw.contains("<item crate=\"crate-b\" duration_ms=\"100\"/>"),
        "crate-b link item: {raw}"
    );
    assert!(raw.contains("derived=\"true\""), "{raw}");

    // The compile AND link group nodes both carry the derived
    // build-settings attributes.
    for group in ["<compile", "<link"] {
        let start = raw
            .find(group)
            .unwrap_or_else(|| panic!("{group} node missing: {raw}"));
        let head_end = raw[start..]
            .find('>')
            .map(|i| start + i)
            .unwrap_or(raw.len());
        let head = &raw[start..head_end];
        for attr_name in ["target=", "profile=", "debug=", "opt_level=", "lto="] {
            assert!(
                head.contains(attr_name),
                "{group} node missing {attr_name}: {head}"
            );
        }
    }

    // Totals: wall_ms spans the request's started/ended, cpu_ms sums the
    // two compile durations (500ms + 100ms).
    assert!(raw.contains("wall_ms=\"600\""), "{raw}");
    assert!(raw.contains("cpu_ms=\"600\""), "{raw}");
    assert!(raw.contains("crate_count=\"2\""), "{raw}");
}

#[test]
fn compile_fallback_summary_is_concise_and_prints_full_log_path() {
    let path = PathBuf::from(r"C:\state\logs\compile-daemon-fallbacks.jsonl");
    let summary = compile_fallback_summary_message(137, &path);
    assert_eq!(summary.lines().count(), 1);
    assert!(summary.contains("137 compiler invocation(s)"));
    assert!(summary.contains("used direct compiler"));
    assert!(summary.contains(&path.display().to_string()));
}

#[test]
fn stale_fallback_scrub_is_hardlink_safe_and_marker_gated() {
    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("target");
    let fingerprint = target.join("debug/.fingerprint/demo-123");
    std::fs::create_dir_all(&fingerprint).expect("create fingerprint directory");
    let cache_blob = temp.path().join("cache-blob");
    let output = fingerprint.join("output-lib-demo");
    let notice = b"soldr: compile daemon unavailable after 30000ms \
\xe2\x80\x94 falling back to direct uncached rustc (soldr#1657); \
reason=daemon unavailable\n";
    let mut persisted = notice.to_vec();
    persisted.extend_from_slice(b"warning: real compiler diagnostic\n");
    std::fs::write(&cache_blob, &persisted).expect("write cache fixture");
    std::fs::hard_link(&cache_blob, &output).expect("hardlink fingerprint output");
    let mut shared_permissions = std::fs::metadata(&cache_blob).unwrap().permissions();
    shared_permissions.set_readonly(true);
    std::fs::set_permissions(&cache_blob, shared_permissions).expect("protect shared hardlink");

    let unrelated = target.join("debug/not-a-fingerprint/output-lib-demo");
    std::fs::create_dir_all(unrelated.parent().unwrap()).expect("create unrelated directory");
    std::fs::write(&unrelated, &persisted).expect("write unrelated fixture");

    let cargo_lock_path = target.join("debug/.cargo-lock");
    let cargo_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&cargo_lock_path)
        .expect("open Cargo lock fixture");
    fs2::FileExt::try_lock_exclusive(&cargo_lock).expect("hold Cargo lock fixture");
    assert!(matches!(
        scrub_cached_fallback_diagnostics_once(&target).expect("defer active target"),
        FallbackOutputScrub::DeferredForActiveBuild(path) if path == cargo_lock_path
    ));
    assert!(!target.join(FALLBACK_OUTPUT_SCRUB_MARKER).exists());
    fs2::FileExt::unlock(&cargo_lock).expect("release Cargo lock fixture");

    assert_eq!(
        scrub_cached_fallback_diagnostics_once(&target).expect("scrub idle target"),
        FallbackOutputScrub::Complete(1)
    );
    assert_eq!(
        std::fs::read(&output).expect("read scrubbed output"),
        b"warning: real compiler diagnostic\n"
    );
    assert_eq!(
        std::fs::read(&cache_blob).expect("read source cache blob"),
        persisted,
        "replacement must not mutate the hardlinked source"
    );
    assert!(
        std::fs::metadata(&cache_blob)
            .expect("cache blob metadata")
            .permissions()
            .readonly(),
        "cache blob must remain protected"
    );
    assert_eq!(
        std::fs::read(&unrelated).expect("read unrelated output"),
        persisted
    );
    assert!(target.join(FALLBACK_OUTPUT_SCRUB_MARKER).is_file());

    let later = fingerprint.join("output-lib-later");
    std::fs::write(&later, notice).expect("simulate later stale output");
    assert_eq!(
        scrub_cached_fallback_diagnostics_once(&target).expect("marker-gated second pass"),
        FallbackOutputScrub::AlreadyDone
    );
    assert_eq!(std::fs::read(&later).unwrap(), notice);
}

// soldr#1788: cargo-dylint prebuilt fallback. An unusable downloaded
// asset is linked against GLIBC_2.39, so it downloads cleanly on Debian 12
// and then fails `smoke_test_or_evict`'s `--version` probe. These cover the
// policy in `resolve_dylint_binary` without touching the network.

#[test]
fn dylint_uses_prebuilt_when_fetch_succeeds() {
    let mut source_build_ran = false;
    let fetched = Ok(crate::fetch::FetchResult {
        binary_path: PathBuf::from("/managed/bin/cargo-dylint"),
        version: "6.0.3".to_string(),
        cached: false,
    });

    let resolved = resolve_dylint_binary("cargo-dylint", fetched, || {
        source_build_ran = true;
        Ok(PathBuf::from("/source/bin/cargo-dylint"))
    })
    .expect("successful fetch must resolve");

    assert_eq!(resolved, PathBuf::from("/managed/bin/cargo-dylint"));
    assert!(
        !source_build_ran,
        "a usable prebuilt must not trigger the source build"
    );
}

#[test]
fn dylint_smoke_failure_never_calls_source_build() {
    let mut source_build_ran = false;
    // Shaped like the real `smoke_test_or_evict` error: the download
    // succeeded, the `--version` probe did not.
    let fetched = Err(SoldrError::Other(
        "smoke test failed: cargo-dylint binary at /managed/bin/cargo-dylint \
         did not respond to --version / --help — likely a corrupted download \
         (see soldr#936)"
            .to_string(),
    ));

    let error = resolve_dylint_binary("cargo-dylint", fetched, || {
        source_build_ran = true;
        Ok(PathBuf::from("/source/bin/cargo-dylint"))
    })
    .expect_err("smoke-test failure must fail instead of compiling");

    assert!(
        !source_build_ran,
        "an unusable prebuilt must never trigger the pinned source build"
    );
    let message = error.to_string();
    assert!(
        message.contains("cargo-dylint"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("Dylint v6.2.0 is not built for this machine"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("Soldr will not build Dylint from source"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("Corrective action:"),
        "unexpected error: {message}"
    );
}

// Named `log_summary_tests`, not `log_summary`: a child module of the same
// name would shadow the real `super::log_summary` these tests exercise.
mod log_summary_tests;
