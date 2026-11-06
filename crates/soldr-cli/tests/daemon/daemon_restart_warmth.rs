//! Restart warmth end-to-end for soldr#2436 phase 6.
//!
//! Phase 5 embedded zccache durability work: the dependency graph is
//! saved on a registration-count trigger and drained on graceful daemon
//! shutdown, so a daemon restart must not orphan the contexts the previous
//! generation learned. This test proves it through the real front door:
//! a cold `soldr cargo check` populates the cache, the daemon is stopped
//! gracefully, and a second build against a fresh target directory must be
//! served warm by the restarted daemon — with zero `context_not_found`
//! attributions in the compile journal after the restart.

use crate::common;

use crate::common::unique_temp_dir;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Panic-safe daemon ownership, mirroring `agent_worktree_share`'s guard:
/// the daemon auto-starts from the fixture's first cacheable compile and
/// pins its cache root, so it must be stopped before the root is removed
/// even when an assertion unwinds.
struct DaemonGuard {
    workdir: PathBuf,
    cache_dir: PathBuf,
    home_dir: PathBuf,
}

impl DaemonGuard {
    fn daemon_pid(&self) -> Option<u32> {
        common::route_claim::route_claim_pid(&self.cache_dir)
    }

    fn stop_daemon(&self) -> std::process::Output {
        let mut command = common::isolated_soldr_command();
        command
            .args(["daemon", "stop"])
            .env("SOLDR_CACHE_DIR", &self.cache_dir)
            .env("HOME", &self.home_dir)
            .env("USERPROFILE", &self.home_dir);
        command.output().expect("run soldr daemon stop")
    }

    fn wait_for_daemon_exit(&self, pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !soldr_platform::process::inspect::is_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    /// Gracefully stop the live daemon and wait for the process to exit.
    /// The graceful path is the point of the test: it is what drains the
    /// dependency graph to disk before the process goes away.
    fn stop_and_assert_exited(&self) {
        let pid = self.daemon_pid().expect("daemon PID publication");
        let output = self.stop_daemon();
        assert!(
            output.status.success(),
            "soldr daemon stop failed: stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            self.wait_for_daemon_exit(pid),
            "soldr daemon PID {pid} survived a successful daemon stop"
        );
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon_pid() {
            let _ = self.stop_daemon();
            let _ = self.wait_for_daemon_exit(pid);
        }
        // soldr#3136: stop the broker the front-door calls started under
        // `home_dir` *before* removing `workdir`, which contains that home.
        // A `BrokerHomeGuard` dropped after this guard ran `broker stop`
        // against an already-deleted home, could not reach the broker, and
        // left it running for the rest of the suite.
        common::stop_fixture_broker(&self.cache_dir, &self.home_dir);
        if let Err(error) = fs::remove_dir_all(&self.workdir) {
            eprintln!(
                "warning: could not remove fixture root {}: {error}",
                self.workdir.display()
            );
        }
    }
}

#[test]
fn graceful_daemon_restart_serves_the_next_build_warm() {
    // The daemon-restart mechanics are portable, but the Windows and macOS
    // lanes already carry known daemon-spawn timing flakes (soldr#2624);
    // phase 6's acceptance lane is Linux, so keep the new coverage there.
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Linux
    ) {
        return;
    }

    let workdir = unique_temp_dir("daemon-restart-warmth");
    let cache_dir = workdir.join("cache-root");
    let home_dir = workdir.join("home");
    fs::create_dir_all(&cache_dir).expect("create cache dir");
    fs::create_dir_all(&home_dir).expect("create home dir");
    let guard = DaemonGuard {
        workdir: workdir.clone(),
        cache_dir: cache_dir.clone(),
        home_dir: home_dir.clone(),
    };

    let crate_dir = workdir.join("fixture-crate");
    create_test_crate(&crate_dir);

    // Session 1: cold build. Every cacheable unit misses and is published.
    // The aarch64 target-run hosts ship rustup with no default toolchain
    // configured (soldr#2614's bootstrap gap), so the fixture's bare
    // temp-dir compile cannot resolve a cargo there. Classify that exact
    // failure from the fixture's own stderr — probing the test process's
    // environment proved wrong twice: the answer differs between this
    // process's cwd (the repo checkout) and the scrubbed fixture.
    let cold_output = match soldr_cargo_check(&guard, &crate_dir, &workdir.join("cold-target")) {
        Ok(output) => output,
        Err(BuildSkip::NoDefaultToolchain(detail)) => {
            eprintln!(
                "skipping restart-warmth E2E: fixture host has no default \
                 rustup toolchain (soldr#2614): {detail}"
            );
            return;
        }
    };
    let cold = read_json(&latest_archived_session_stats(&cache_dir, &cold_output));
    let cold_sessions = archived_session_stats(&cache_dir);
    let cold_misses = u64_field(&cold, "misses");
    assert_eq!(
        u64_field(&cold, "hits"),
        0,
        "cold build unexpectedly hit cache: {cold:#?}"
    );
    assert!(
        cold_misses > 0,
        "cold build must contain cacheable misses: {cold:#?}"
    );

    // Graceful restart boundary. The stop drains the dependency graph, so
    // once the process has exited the journal is complete for session 1.
    guard.stop_and_assert_exited();
    let journal = compile_journal_path(&cache_dir);
    let session_one_lines = journal_lines(&journal).len();
    assert!(
        session_one_lines > 0,
        "cold build must journal its compiles at {}",
        journal.display()
    );

    // Session 2: same worktree, fresh target directory, restarted daemon.
    // A fresh target forces cargo to re-drive every unit through the
    // wrapper; the restarted daemon must recognize each context from the
    // drained graph and serve it warm.
    let warm_output = soldr_cargo_check(&guard, &crate_dir, &workdir.join("warm-target"))
        .expect("warm build cannot lose the toolchain the cold build resolved");
    let warm = read_json(&new_archived_session_stats(
        &cache_dir,
        &cold_sessions,
        &warm_output,
    ));
    let warm_hits = u64_field(&warm, "hits");
    assert_eq!(
        warm_hits,
        cold_misses,
        "restarted daemon must serve every cold-published unit warm: \
         cold={cold:#?} warm={warm:#?}; journal={:#?}",
        journal_lines(&journal),
    );

    // Stop again so the restarted generation's journal writes are flushed,
    // then attribute every post-restart compile: a `context_not_found`
    // after a graceful restart is exactly the miss storm phase 5 fixed.
    guard.stop_and_assert_exited();
    let all_lines = journal_lines(&journal);
    let restarted = &all_lines[session_one_lines..];
    assert!(
        !restarted.is_empty(),
        "warm build must journal its compiles at {}",
        journal.display()
    );
    let orphaned: Vec<&str> = restarted
        .iter()
        .filter(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|entry| {
                    entry
                        .get("miss_reason")
                        .and_then(Value::as_str)
                        .map(|reason| reason == "context_not_found")
                })
                .unwrap_or(false)
        })
        .map(|line| line.as_str())
        .collect();
    assert!(
        orphaned.is_empty(),
        "graceful daemon restart orphaned {} compile context(s): {orphaned:#?}",
        orphaned.len()
    );
}

fn create_test_crate(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("create src/");
    fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "restart_warmth_fixture"
version = "0.1.0"
edition = "2021"

"#,
    )
    .expect("write Cargo.toml");
    fs::write(
        dir.join("src").join("main.rs"),
        r#"
fn main() {
    println!("fixture");
}
"#,
    )
    .expect("write src/main.rs");
}

/// The one fixture failure that means "skip", not "fail".
#[derive(Debug)]
enum BuildSkip {
    /// rustup exists but no default toolchain is configured, so a bare
    /// temp-dir crate cannot resolve any cargo (soldr#2614 hosts).
    NoDefaultToolchain(String),
}

/// Runs the fixture build and returns soldr's combined output for the
/// missing-stats diagnostics, mirroring `agent_worktree_share`.
fn soldr_cargo_check(
    guard: &DaemonGuard,
    worktree: &Path,
    target_dir: &Path,
) -> Result<String, BuildSkip> {
    let mut command = common::isolated_soldr_command();
    command
        .args(["cargo", "check"])
        .current_dir(worktree)
        .env("SOLDR_CACHE_DIR", &guard.cache_dir)
        .env("HOME", &guard.home_dir)
        .env("USERPROFILE", &guard.home_dir)
        .env("CARGO_TARGET_DIR", target_dir);
    let output = command.output().expect("spawn soldr cargo check");
    let rendered = format!(
        "stdout={}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    if !output.status.success()
        && rendered.contains("rustup could not choose a version of cargo to run")
    {
        return Err(BuildSkip::NoDefaultToolchain(rendered));
    }
    assert!(
        output.status.success(),
        "soldr cargo check failed in {}: {rendered}",
        worktree.display(),
    );
    Ok(rendered)
}

/// The embedded store's durable compile journal (soldr#2186 layout): the
/// versioned daemon-state logs directory, not the unversioned session logs.
/// Newest mtime of the embedded store's depgraph snapshot, if one exists.
///
/// The snapshot is what a dependency-graph batch save writes, so its
/// appearance (or a fresh mtime on an existing one) is the observable that a
/// save actually landed. `None` means no snapshot has been written yet.
fn depgraph_snapshot_mtime(cache_dir: &Path) -> Option<std::time::SystemTime> {
    fn newest(dir: &Path, found: &mut Option<std::time::SystemTime>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                newest(&path, found);
            } else if path.file_name().is_some_and(|name| name == "depgraph.bin") {
                if let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) {
                    if found.is_none_or(|current| modified > current) {
                        *found = Some(modified);
                    }
                }
            }
        }
    }
    let mut found = None;
    newest(&cache_dir.join("cache").join("zccache"), &mut found);
    found
}

/// Block until a depgraph save lands, or `budget` elapses.
///
/// "Lands" means the snapshot appears, or an existing one is rewritten with a
/// newer mtime. Expiry is deliberately not an assertion: when the save already
/// ran during the preceding build there is no further write to observe, and
/// the caller's own orphan-count assertion is what decides the outcome either
/// way.
fn wait_for_depgraph_save(cache_dir: &Path, budget: Duration) {
    let baseline = depgraph_snapshot_mtime(cache_dir);
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        match (depgraph_snapshot_mtime(cache_dir), baseline) {
            (Some(_), None) => return,
            (Some(current), Some(before)) if current > before => return,
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn compile_journal_path(cache_dir: &Path) -> PathBuf {
    common::discovered_embedded_zccache_store(cache_dir)
        .join("logs")
        .join("compile_journal.jsonl")
}

fn journal_lines(journal: &Path) -> Vec<String> {
    let raw = fs::read_to_string(journal)
        .unwrap_or_else(|error| panic!("read {}: {error}", journal.display()));
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

fn latest_archived_session_stats(cache_dir: &Path, check_output: &str) -> PathBuf {
    archived_session_stats(cache_dir)
        .into_iter()
        .next()
        .unwrap_or_else(|| {
            panic!(
                "no archived session stats under {} after: {check_output}",
                cache_dir.join("cache/zccache/history").display()
            )
        })
}

fn new_archived_session_stats(
    cache_dir: &Path,
    previous: &[PathBuf],
    check_output: &str,
) -> PathBuf {
    archived_session_stats(cache_dir)
        .into_iter()
        .find(|path| !previous.contains(path))
        .unwrap_or_else(|| {
            panic!(
                "no newly archived session stats after warm build (cold build left {} session(s)) after: {check_output}",
                previous.len(),
            )
        })
}

fn archived_session_stats(cache_dir: &Path) -> Vec<PathBuf> {
    let history_dir = cache_dir.join("cache").join("zccache").join("history");
    let mut sessions: Vec<_> = fs::read_dir(&history_dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", history_dir.display()))
        .map(|entry| entry.expect("read history entry").path())
        .filter(|path| path.join("last-session-stats.json").is_file())
        .collect();
    sessions.sort();
    sessions
        .into_iter()
        .map(|session| session.join("last-session-stats.json"))
        .collect()
}

fn read_json(path: &Path) -> Value {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(raw.trim())
        .unwrap_or_else(|e| panic!("parse {}: {e}\n{raw}", path.display()))
}

fn u64_field(stats: &Value, key: &str) -> u64 {
    stats
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing or non-u64 `{key}` in {stats:#?}"))
}

/// soldr#2645 (deferred from #2436 phase 6): SIGKILL loss bound.
///
/// The embedded zccache saves the dependency graph once ≥32 new contexts
/// have registered (DEPGRAPH_SAVE_BATCH, polled every 5s), so a hard kill
/// may orphan at most one save batch. A fixture with 30+ compile units is
/// what makes the bound falsifiable: pre-phase-5, a kill after any wait
/// lost EVERY context and the rebuild attributed them all as
/// `context_not_found`.
#[test]
fn hard_killed_daemon_loses_at_most_one_save_batch() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Linux
    ) {
        return;
    }

    let workdir = unique_temp_dir("daemon-hard-kill-loss");
    let cache_dir = workdir.join("cache-root");
    let home_dir = workdir.join("home");
    fs::create_dir_all(&cache_dir).expect("create cache dir");
    fs::create_dir_all(&home_dir).expect("create home dir");
    let guard = DaemonGuard {
        workdir: workdir.clone(),
        cache_dir: cache_dir.clone(),
        home_dir: home_dir.clone(),
    };

    let crate_dir = workdir.join("fixture-workspace");
    create_many_unit_workspace(&crate_dir, 34);

    let cold_output = match soldr_cargo_check(&guard, &crate_dir, &workdir.join("cold-target")) {
        Ok(output) => output,
        Err(BuildSkip::NoDefaultToolchain(detail)) => {
            eprintln!(
                "skipping hard-kill loss-bound E2E: fixture host has no \
                 default rustup toolchain (soldr#2614): {detail}"
            );
            return;
        }
    };
    let cold = read_json(&latest_archived_session_stats(&cache_dir, &cold_output));
    let cold_misses = u64_field(&cold, "misses");
    assert!(
        cold_misses >= 34,
        "the fixture must register enough contexts to cross the 32-entry \
         save batch: {cold:#?}"
    );

    // Wait for the 5s-polled batch trigger to land its save (≥32 unsaved
    // contexts make one due at the next tick) by watching for the depgraph
    // snapshot to be written, rather than sleeping a fixed budget.
    //
    // soldr#2996: this was `sleep(Duration::from_secs(15))` — three poll
    // periods spent unconditionally, on a test that is already serialized by
    // `.config/nextest.toml`'s one-thread runtime group. The 15s ceiling is
    // retained as the bound, so the falsifiability argument is unchanged: if
    // batch saving were broken nothing would appear, the wait would expire,
    // and the assertion below would still see every context orphaned.
    wait_for_depgraph_save(&cache_dir, Duration::from_secs(15));

    let pid = guard.daemon_pid().expect("daemon PID publication");
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && soldr_platform::process::inspect::is_alive(pid) {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !soldr_platform::process::inspect::is_alive(pid),
        "SIGKILLed daemon {pid} must exit"
    );

    let journal = compile_journal_path(&cache_dir);
    let pre_rebuild_lines = journal_lines(&journal).len();

    // Rebuild against a fresh target: every unit re-drives the wrapper and
    // the successor daemon must recognize all but at most one batch.
    let warm_output = soldr_cargo_check(&guard, &crate_dir, &workdir.join("warm-target"))
        .expect("rebuild cannot lose the toolchain the cold build resolved");
    let warm = read_json(&new_archived_session_stats(
        &cache_dir,
        &archived_session_stats(&cache_dir)
            .into_iter()
            .take(1)
            .collect::<Vec<_>>(),
        &warm_output,
    ));
    let _ = warm; // stats retained for the panic path via read_json above

    guard.stop_and_assert_exited();
    let all_lines = journal_lines(&journal);
    let orphaned = all_lines[pre_rebuild_lines..]
        .iter()
        .filter(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|entry| {
                    entry
                        .get("miss_reason")
                        .and_then(Value::as_str)
                        .map(|reason| reason == "context_not_found")
                })
                .unwrap_or(false)
        })
        .count();
    assert!(
        orphaned <= 32,
        "hard kill must lose at most one save batch (32), lost {orphaned}"
    );
}

/// A workspace with `count` leaf library crates plus a root binary that
/// depends on all of them — enough distinct compile units to cross the
/// dependency graph's 32-registration save batch in one cold build.
fn create_many_unit_workspace(dir: &Path, count: usize) {
    let mut members = String::new();
    let mut deps = String::new();
    let mut calls = String::new();
    for index in 0..count {
        let name = format!("m{index:02}");
        members.push_str(&format!("    \"{name}\",\n"));
        deps.push_str(&format!("{name} = {{ path = \"../{name}\" }}\n"));
        calls.push_str(&format!("    total += {name}::value();\n"));
        let member_dir = dir.join(&name);
        fs::create_dir_all(member_dir.join("src")).expect("member src dir");
        fs::write(
            member_dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .expect("member manifest");
        fs::write(
            member_dir.join("src").join("lib.rs"),
            format!("pub fn value() -> u64 {{ {index} }}\n"),
        )
        .expect("member lib");
    }
    let root_dir = dir.join("root");
    fs::create_dir_all(root_dir.join("src")).expect("root src dir");
    fs::write(
        root_dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{deps}"
        ),
    )
    .expect("root manifest");
    fs::write(
        root_dir.join("src").join("main.rs"),
        format!(
            "fn main() {{\n    let mut total = 0u64;\n{calls}    println!(\"{{total}}\");\n}}\n"
        ),
    )
    .expect("root main");
    fs::write(
        dir.join("Cargo.toml"),
        format!("[workspace]\nmembers = [\n{members}    \"root\",\n]\nresolver = \"2\"\n"),
    )
    .expect("workspace manifest");
}
