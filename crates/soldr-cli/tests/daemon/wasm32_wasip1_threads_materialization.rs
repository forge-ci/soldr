//! Real front-door regression for soldr#2919.
//!
//! The fixture is source-only.  A cold managed compile must stage the actual
//! `.wasm` command artifact, then a graceful daemon restart must materialize
//! the same artifact into a different empty target directory from the cache.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use crate::common;

const TARGET: &str = "wasm32-wasip1-threads";
const BIN: &str = "wasm32_wasip1_threads_materialization";
const REQUIRED_TARGET_ENV: &str = "SOLDR_REQUIRE_WASM32_WASIP1_THREADS_MATERIALIZATION";

struct DaemonGuard {
    cache_root: PathBuf,
    home: PathBuf,
}

impl DaemonGuard {
    /// soldr#3823: under the daemon's own generation key (soldr#3374), not
    /// the one this test process would derive for itself.
    fn pid(&self) -> Option<u32> {
        common::route_claim::route_claim_pid(&self.cache_root)
    }

    fn stop_and_assert_exited(&self) {
        let pid = self
            .pid()
            .expect("daemon route claim after managed compile");
        stop_daemon(&self.cache_root, &self.home);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !soldr_platform::process::inspect::is_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("daemon {pid} survived a successful soldr daemon stop");
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid() {
            let _ = common::isolated_soldr_command()
                .args(["daemon", "stop"])
                .env("SOLDR_CACHE_DIR", &self.cache_root)
                .env("HOME", &self.home)
                .env("USERPROFILE", &self.home)
                .output();
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && soldr_platform::process::inspect::is_alive(pid) {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

#[test]
fn managed_wasm_command_artifact_survives_daemon_restart_and_fresh_target() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::MacOs
    ) {
        return;
    }

    let root = common::unique_temp_dir("wasm32-wasip1-threads-materialization");
    let cache_root = root.join("cache");
    let home = root.join("home");
    let target_required = std::env::var_os(REQUIRED_TARGET_ENV).as_deref() == Some(OsStr::new("1"));
    fs::create_dir_all(&cache_root).expect("create cache root");
    fs::create_dir_all(&home).expect("create isolated home");
    let _broker = common::BrokerHomeGuard::new(&cache_root, &home);
    let Some(rustup_home) = caller_rustup_home() else {
        if target_required {
            panic!("{TARGET} is required by {REQUIRED_TARGET_ENV}=1, but the caller toolchain home is unavailable");
        }
        eprintln!("skipping Wasm materialization: the caller toolchain home is unavailable");
        return;
    };
    if !target_is_available(&home, &rustup_home) {
        if target_required {
            panic!("{TARGET} is required by {REQUIRED_TARGET_ENV}=1, but is not installed for the provisioned toolchain");
        }
        eprintln!(
            "skipping Wasm materialization: {TARGET} is not installed for this isolated toolchain"
        );
        return;
    }
    let guard = DaemonGuard {
        cache_root: cache_root.clone(),
        home: home.clone(),
    };

    let fixture = common::fixtures_dir().join("wasm32-wasip1-threads-materialization");
    let cold_target = root.join("cold-target");
    let before_cold = archived_sessions(&cache_root);
    let cold = soldr_build(&fixture, &cache_root, &home, &rustup_home, &cold_target);
    assert_success(&cold, "cold managed Wasm build");
    assert_wasm_output(&cold_target);
    let cold_stats = new_session(&cache_root, &before_cold);
    assert_eq!(
        stat(&cold_stats, "hits"),
        0,
        "cold source-only build cannot be a hit"
    );
    let cold_misses = stat(&cold_stats, "misses");
    assert!(
        cold_misses > 0,
        "cold source-only build must populate embedded cache"
    );
    let cold_journal_len = journal_lines(&cache_root).len();

    guard.stop_and_assert_exited();

    let warm_target = root.join("warm-target");
    let before_warm = archived_sessions(&cache_root);
    let warm = soldr_build(&fixture, &cache_root, &home, &rustup_home, &warm_target);
    assert_success(&warm, "warm managed Wasm build after daemon restart");
    assert_wasm_output(&warm_target);
    assert!(
        stat(&new_session(&cache_root, &before_warm), "hits") >= cold_misses,
        "fresh target after daemon restart must restore the cold managed outputs"
    );
    let warm_outcomes = journal_outcomes(&journal_lines(&cache_root)[cold_journal_len..]);
    assert!(
        warm_outcomes.iter().any(|outcome| is_cache_hit(outcome)),
        "warm build must record a managed embedded cache hit in newly appended \
         compile-journal records; outcomes: {warm_outcomes:?}"
    );

    guard.stop_and_assert_exited();
}

#[test]
fn missing_wasm_target_is_fatal_only_for_an_authoritative_replay() {
    assert!(missing_target_is_fatal(true));
    assert!(!missing_target_is_fatal(false));
}

fn missing_target_is_fatal(target_required: bool) -> bool {
    target_required
}

fn soldr_build(
    fixture: &Path,
    cache_root: &Path,
    home: &Path,
    rustup_home: &Path,
    target_dir: &Path,
) -> Output {
    common::isolated_soldr_command()
        .args([
            "cargo",
            "build",
            "--locked",
            "--manifest-path",
            fixture
                .join("Cargo.toml")
                .to_str()
                .expect("UTF-8 manifest path"),
            "--target",
            TARGET,
            "--release",
            "--target-dir",
            target_dir.to_str().expect("UTF-8 target path"),
        ])
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("RUSTUP_HOME", rustup_home)
        .env("CARGO_HOME", home.join("cargo"))
        .env("SOLDR_NO_BOOTSTRAP", "1")
        // The test's cache journal is evidence only for Soldr's embedded route;
        // inherited overrides could otherwise silently select an external or
        // disabled wrapper before the journal assertion runs.
        .env_remove("SOLDR_RUSTC_WRAPPER")
        .env_remove("ZCCACHE_DISABLE")
        .env_remove("SOLDR_CACHE_ENABLED")
        .env_remove("SOLDR_NATIVE_CACHE")
        .output()
        .expect("run managed soldr cargo build")
}

fn stop_daemon(cache_root: &Path, home: &Path) {
    let output = common::isolated_soldr_command()
        .args(["daemon", "stop"])
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .output()
        .expect("run soldr daemon stop");
    assert_success(&output, "soldr daemon stop");
}

fn assert_wasm_output(target_dir: &Path) {
    let release = target_dir.join(TARGET).join("release");
    let wasm = release.join(format!("{BIN}.wasm"));
    assert!(
        wasm.is_file(),
        "missing Wasm command artifact: {}",
        wasm.display()
    );
    assert!(
        !release.join(BIN).exists(),
        "the extensionless native-output assumption must not create a sibling"
    );
    let deps = release.join("deps");
    for entry in fs::read_dir(&deps).expect("read Wasm deps output") {
        let path = entry.expect("read deps entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        assert!(
            !(path.is_file() && name.starts_with(BIN) && path.extension().is_none()),
            "zccache must not request an extensionless Wasm dependency output: {}",
            path.display()
        );
    }
}

fn caller_rustup_home() -> Option<PathBuf> {
    let output = common::isolated_soldr_command()
        .args(["rustc", "--print", "sysroot"])
        .env("SOLDR_NO_BOOTSTRAP", "1")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let lines: Vec<_> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() != 1 {
        return None;
    }
    let sysroot = PathBuf::from(lines[0].trim());
    if !sysroot.is_absolute() || !sysroot.is_dir() {
        return None;
    }
    let sysroot = sysroot.canonicalize().ok()?;
    let toolchains = sysroot.parent()?;
    if toolchains.file_name()?.to_str()? != "toolchains" {
        return None;
    }
    Some(toolchains.parent()?.to_path_buf())
}

fn target_is_available(home: &Path, rustup_home: &Path) -> bool {
    common::isolated_soldr_command()
        .args(["rustc", "--print", "target-libdir", "--target", TARGET])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("RUSTUP_HOME", rustup_home)
        .env("CARGO_HOME", home.join("cargo"))
        .env("SOLDR_NO_BOOTSTRAP", "1")
        .output()
        .is_ok_and(|output| {
            if !output.status.success() {
                return false;
            }
            let target_libdir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
            target_libdir.is_dir()
                && fs::read_dir(target_libdir).is_ok_and(|entries| {
                    let mut has_std = false;
                    let mut has_core = false;
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        has_std |= name.starts_with("libstd-") || name.starts_with("std-");
                        has_core |= name.starts_with("libcore-") || name.starts_with("core-");
                    }
                    has_std && has_core
                })
        })
}

fn archived_sessions(cache_root: &Path) -> Vec<PathBuf> {
    let history = cache_root.join("cache").join("zccache").join("history");
    let Ok(entries) = fs::read_dir(history) else {
        return Vec::new();
    };
    let mut stats: Vec<_> = entries
        .map(|entry| {
            entry
                .expect("read history entry")
                .path()
                .join("last-session-stats.json")
        })
        .filter(|path| path.is_file())
        .collect();
    stats.sort();
    stats
}

fn new_session(cache_root: &Path, before: &[PathBuf]) -> serde_json::Value {
    let path = archived_sessions(cache_root)
        .into_iter()
        .find(|path| !before.contains(path))
        .expect("new archived session stats");
    serde_json::from_str(&fs::read_to_string(path).expect("read session stats"))
        .expect("parse session stats")
}

fn stat(stats: &serde_json::Value, key: &str) -> u64 {
    stats
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("missing {key} in session stats: {stats:#?}"))
}

fn journal_lines(cache_root: &Path) -> Vec<String> {
    let path =
        common::discovered_embedded_zccache_store(cache_root).join("logs/compile_journal.jsonl");
    fs::read_to_string(path)
        .expect("read managed compile journal")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Extract the schema-owned `outcome` field from a known-new JSONL tail.
///
/// The journal is the embedded service's authoritative cache record. Its
/// schema has no `cached` boolean: the current zccache journal records `hit`,
/// `miss`, `error`, `cached_error`, `link_hit`, or `link_miss` in `outcome`.
fn journal_outcomes(lines: &[String]) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct CompileJournalRecord {
        outcome: String,
    }

    lines
        .iter()
        .map(|line| {
            serde_json::from_str::<CompileJournalRecord>(line)
                .unwrap_or_else(|error| {
                    panic!("parse newly appended compile journal record: {error}")
                })
                .outcome
        })
        .collect()
}

/// Both compiler and linker cache restores materialize outputs into the fresh
/// target tree. This matches the journal's public classification used by the
/// build-log cache summary (`hit` and `link_hit` are cache hits).
fn is_cache_hit(outcome: &str) -> bool {
    matches!(outcome, "hit" | "link_hit")
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn journal_outcomes_use_the_schema_owned_cache_classification() {
    let lines = vec![
        r#"{"outcome":"miss"}"#.to_owned(),
        r#"{"outcome":"link_hit"}"#.to_owned(),
        r#"{"outcome":"error"}"#.to_owned(),
    ];
    let outcomes = journal_outcomes(&lines);

    assert_eq!(
        outcomes,
        vec!["miss".to_owned(), "link_hit".to_owned(), "error".to_owned()]
    );
    assert!(is_cache_hit(&outcomes[1]));
    assert!(!is_cache_hit(&outcomes[0]));
    assert!(!is_cache_hit(&outcomes[2]));
}
