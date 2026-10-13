//! Reproduction test for the daemon RSS ceiling watchdog (soldr#3038).
//!
//! The motivating observation: a canonical `soldr-daemon` on production
//! hardware reached 11.7 GiB of private anonymous memory over ~90 minutes of
//! continuous building. This test cannot reproduce that scale inside a
//! nextest budget -- it drives a small, hermetic, dependency-chained
//! workspace through a real daemon instead of a multi-hour production
//! fleet -- so it is not a reproduction of the 11.7 GiB growth. What it *is*
//! a real regression gate for: the owner's stated target, "the daemon should
//! operate below half a gigabyte of RAM" (`CEILING_BYTES` below), exercised
//! against a real `soldr-daemon` process serving a real (if small) compile
//! workload end to end -- daemon spawn, wrapped `rustc` invocations routed
//! through the embedded zccache service, and an in-process RSS sample taken
//! by the daemon about itself.
//!
//! Uses the daemon's own `SOLDR_DAEMON_RSS_CEILING_BYTES` watchdog
//! (`soldr_cli::daemon::rss_ceiling`) rather than sampling RSS from outside
//! the process: an external poller can miss a spike between samples, and a
//! breach here must produce a legible assertion message (process role,
//! pid, ceiling, observed peak, dump path) rather than a bare "connection
//! refused" from a daemon that died mid-build.
//!
//! soldr#3057 reversed the original soldr#3038 "record and keep running"
//! decision: on breach, the daemon now dumps its memory state and exits
//! non-zero immediately (`soldr_cli::daemon::rss_ceiling::die_on_breach`,
//! private to that module -- reached through `run_watchdog`).
//! [`daemon_stays_under_the_512mib_target_while_serving_a_chained_build`]
//! is the "must not breach in ordinary use" regression gate, and
//! [`daemon_dies_and_dumps_memory_when_the_ceiling_is_breached`] is the
//! "when it does breach, the failure must be legible" gate --
//! [`assert_build_succeeded_or_report_breach`] is the shared helper both
//! could use that checks for a breach dump *before* trusting a transport
//! error to mean anything.
//! [`daemon_rss_retention_rate_per_compile_stays_within_budget`] is the
//! `#[ignore]`d rate instrument for soldr#3059.
//!
//! soldr#3128 added two more, which test this fixture rather than the daemon:
//! [`a_timed_out_fixture_child_is_reaped_with_its_tree_and_its_output`] and
//! [`a_fixture_child_that_exits_on_its_own_reports_status_and_output`]. They
//! exist because the gates above can only report what they observe, and their
//! own deadline used to be unenforceable -- `Child::kill()` ends one process
//! while the whole build tree holds the pipes, so `wait_with_output()` never
//! returned and the panic carrying the diagnostic never ran. On the Windows
//! target-run lanes that turned every failure of this file into a bare
//! `TIMEOUT [> 300s]` with empty output. See `common::tracked_child`.
//!
//! `SOLDR_*` variables are forwarded wholesale into a broker-spawned daemon
//! (`crate::daemon::lifecycle::spawn_env::FORWARDED_ENV_PREFIX`), so setting
//! `SOLDR_DAEMON_RSS_CEILING_BYTES` on the first `soldr cargo build` in this
//! test is enough to arm the watchdog in the daemon it spawns -- no direct
//! daemon control plumbing is needed here, unlike the IPC-query fixtures in
//! `cli_daemon_builds.rs`.

#![allow(clippy::print_stdout)]

use crate::common;
use soldr_cli::core::SoldrPaths;
use soldr_cli::daemon::rss_ceiling::{self, RssCeilingStatus};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The owner's stated target steady state (soldr#3038): "the daemon should
/// operate below half a gigabyte of RAM."
const CEILING_BYTES: u64 = 512 * 1024 * 1024;

/// Chained workspace crates. Each depends on the previous by path, so the
/// build is inherently sequential (real dependency-graph work, not just
/// parallel fan-out) and every crate is a distinct `rustc` invocation routed
/// through the wrapper -- the unit soldr's cache and the daemon's in-memory
/// state actually key on.
const CRATE_COUNT: usize = 12;

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

fn crate_name(index: usize) -> String {
    format!("wk{index:02}")
}

/// Writes a small virtual workspace of `CRATE_COUNT` library crates chained
/// by path dependency (`wk01` depends on `wk00`, ...). No external
/// dependencies and no network access: the workload must be hermetic so the
/// test is not flaky on an offline or sandboxed runner. Returns the
/// workspace root.
fn write_workload_workspace(root: &Path) -> PathBuf {
    let project = root.join("workload");
    fs::create_dir_all(&project).expect("create workload root");
    let members: String = (0..CRATE_COUNT)
        .map(|i| format!("    \"{}\",\n", crate_name(i)))
        .collect();
    fs::write(
        project.join("Cargo.toml"),
        format!("[workspace]\nresolver = \"2\"\nmembers = [\n{members}]\n"),
    )
    .expect("write workspace Cargo.toml");

    for index in 0..CRATE_COUNT {
        write_workload_crate(&project, index, 0);
    }
    project
}

/// A single trivial crate -- no workspace, one file. Used only to prime a
/// broker singleton cheaply (see
/// [`daemon_dies_and_dumps_memory_when_the_ceiling_is_breached`]): the
/// point of this build is to spawn/discover the broker for `home_root`,
/// not to exercise any real compile-driven memory pressure.
fn write_trivial_crate(root: &Path) -> PathBuf {
    let project = root.join("trivial");
    fs::create_dir_all(project.join("src")).expect("create trivial crate dir");
    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"trivial\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write trivial Cargo.toml");
    fs::write(
        project.join("src").join("lib.rs"),
        "pub fn f() -> u32 { 1 }\n",
    )
    .expect("write trivial lib.rs");
    project
}

/// (Re)writes one chained crate's `Cargo.toml` + `src/lib.rs`. `extra_fns`
/// lets a rebuild pass append fresh functions to force a real content-hash
/// change (a cache miss, not a no-op rebuild) without touching every crate.
fn write_workload_crate(project: &Path, index: usize, extra_fns: usize) {
    let name = crate_name(index);
    let dir = project.join(&name);
    fs::create_dir_all(dir.join("src")).expect("create crate src dir");
    let dep = if index == 0 {
        String::new()
    } else {
        format!(
            "{} = {{ path = \"../{}\" }}\n",
            crate_name(index - 1),
            crate_name(index - 1)
        )
    };
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{dep}"
        ),
    )
    .expect("write crate Cargo.toml");

    let mut body = String::new();
    for fn_index in 0..20 + extra_fns {
        body.push_str(&format!(
            "pub fn f{fn_index}(x: u64) -> u64 {{ x.wrapping_mul({fn_index_plus}).wrapping_add(1) }}\n",
            fn_index_plus = fn_index as u64 + 1
        ));
    }
    if index > 0 {
        let prev = crate_name(index - 1);
        body.push_str(&format!(
            "pub fn chained(x: u64) -> u64 {{ {prev}::f0(x).wrapping_add(f0(x)) }}\n"
        ));
    }
    fs::write(dir.join("src").join("lib.rs"), body).expect("write crate lib.rs");
}

fn daemon_bin() -> PathBuf {
    common::soldr_daemon_bin()
}

/// Recursively collect every directory under `root` whose name starts with
/// `memory-breach-`, up to `max_depth` levels down. Bounded, simple manual
/// walk (no `walkdir` dependency) -- these are hermetic per-test temp trees,
/// not arbitrary filesystems, so a shallow bounded recursion is enough and
/// a breach directory is never itself descended into (it has no nested
/// `memory-breach-*` children).
fn find_memory_breach_dirs(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<PathBuf>) {
        if depth > max_depth {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let path = entry.path();
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("memory-breach-")
            {
                out.push(path);
                continue;
            }
            walk(&path, depth + 1, max_depth, out);
        }
    }
    let mut out = Vec::new();
    walk(root, 0, max_depth, &mut out);
    out
}

/// Find the most recent `memory-breach-*` dump reachable from either
/// `cache_root` (where the daemon's own dump lands, under
/// `<SOLDR_CACHE_DIR>/cache/soldr-daemon/`) or `home_root` (where the
/// broker's dump lands, under its own HOME-anchored, broker-owned root --
/// see `rss_ceiling`'s and `broker_server.rs`'s module docs for why the
/// broker watches itself rather than the daemon it spawned) and parse its
/// `summary.json`. Directory names sort chronologically
/// (`memory-breach-<unix_millis>-<pid>`), so lexicographic `max` is enough
/// to pick the latest without parsing every name.
///
/// This is the crux of soldr#3057's legibility requirement: a caller must
/// check for this *before* interpreting a command failure, because a
/// process that died mid-build from a breach produces a transport-level
/// error (broken pipe, connection refused) that says nothing about memory
/// on its own.
fn newest_breach_summary(
    cache_root: &Path,
    home_root: &Path,
) -> Option<rss_ceiling::BreachSummary> {
    let paths = SoldrPaths::with_root(cache_root.to_path_buf());
    let mut dirs = find_memory_breach_dirs(&paths.cache.join("soldr-daemon"), 2);
    // The broker's own root is HOME-anchored (`running-process`'s
    // per-OS user-config directory, e.g. `~/.config/running-process/
    // soldr-broker` on Linux) rather than SOLDR_CACHE_DIR-anchored, so it
    // is not reachable from `cache_root` at all -- walk `home_root`
    // itself rather than duplicating that path-construction logic here.
    dirs.extend(find_memory_breach_dirs(home_root, 8));
    let newest = dirs
        .into_iter()
        .max_by_key(|path| path.file_name().map(|name| name.to_os_string()))?;
    let body = fs::read_to_string(newest.join("summary.json")).ok()?;
    serde_json::from_str(&body).ok()
}

/// Assert a `soldr` invocation's output represents success -- but check for
/// a memory-breach dump under `cache_root` or `home_root` FIRST (see
/// [`newest_breach_summary`] for why both), and if one exists, fail with
/// `rss_ceiling::legible_breach_message` (role, pid, ceiling, observed
/// peak, dump path) instead of the raw transport/exit-code failure. This is
/// the caller-side half of the soldr#3057 contract: production dumps then
/// dies; a test driving that same process must not blame the connection
/// error a dying daemon or broker produces without first checking whether a
/// breach explains it.
fn assert_build_succeeded_or_report_breach(
    args: &[&str],
    cache_root: &Path,
    home_root: &Path,
    output: &std::process::Output,
) {
    if output.status.success() {
        return;
    }
    if let Some(summary) = newest_breach_summary(cache_root, home_root) {
        panic!(
            "soldr {args:?} failed, but this is a memory-ceiling breach, not a bare \
             transport error: {}",
            rss_ceiling::legible_breach_message(&summary)
        );
    }
    panic!(
        "soldr {args:?} did not exit 0 and no memory-breach dump was found under {} or {}: \
         stdout={} stderr={}",
        cache_root.display(),
        home_root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Same shape as `cli_daemon_lifecycle.rs::run_soldr_with_timeout`: a
/// broker-mediated `soldr cargo ...` invocation (not the direct-control IPC
/// pattern `cli_daemon_builds.rs` uses for non-compile queries), because
/// driving a real compile through the wrapper needs the front door's normal
/// daemon discovery/spawn path -- which is exactly the path that forwards
/// `SOLDR_DAEMON_RSS_CEILING_BYTES` into the daemon it spawns.
///
/// Asserts the build succeeded, checking for a breach dump first (see
/// [`assert_build_succeeded_or_report_breach`]) -- callers that expect (and
/// want to inspect) a breach-induced failure should use
/// [`run_soldr_build_allow_failure`] instead.
fn run_soldr_build(
    args: &[&str],
    cache_root: &Path,
    home_root: &Path,
    current_dir: &Path,
    ceiling_bytes: u64,
    timeout: Duration,
) -> std::process::Output {
    let output = run_soldr_build_allow_failure(
        args,
        cache_root,
        home_root,
        current_dir,
        ceiling_bytes,
        timeout,
    );
    assert_build_succeeded_or_report_breach(args, cache_root, home_root, &output);
    output
}

/// Same as [`run_soldr_build`] but does not assert success -- for the
/// breach-reproduction test, which *expects* the build to fail because the
/// daemon it spawned dumped memory and died mid-build.
fn run_soldr_build_allow_failure(
    args: &[&str],
    cache_root: &Path,
    home_root: &Path,
    current_dir: &Path,
    ceiling_bytes: u64,
    timeout: Duration,
) -> std::process::Output {
    run_soldr_build_with_ceiling(
        args,
        cache_root,
        home_root,
        current_dir,
        Some(ceiling_bytes),
        timeout,
    )
}

/// Same as [`run_soldr_build_allow_failure`], but `ceiling_bytes` is
/// optional: `None` removes [`rss_ceiling::RSS_CEILING_ENV_VAR`] from the
/// child's environment entirely (rather than leaving it unset-but-inherited
/// from this test process), so a caller can prime a broker with no ceiling
/// at all -- see
/// [`daemon_dies_and_dumps_memory_when_the_ceiling_is_breached`] for why
/// that matters: the broker reads the env var exactly once, at its own
/// startup, so a broker spawned with it unset never spawns its own
/// watchdog and cannot breach no matter how large a later route's daemon
/// gets.
fn run_soldr_build_with_ceiling(
    args: &[&str],
    cache_root: &Path,
    home_root: &Path,
    current_dir: &Path,
    ceiling_bytes: Option<u64>,
    timeout: Duration,
) -> std::process::Output {
    let mut cmd = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut cmd);
    cmd.args(args)
        .current_dir(current_dir)
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home_root)
        .env("USERPROFILE", home_root)
        .env(
            soldr_cli::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR,
            common::isolated_daemon::isolated_daemon_executable(&daemon_bin(), cache_root),
        )
        .env("SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS", "10000")
        // soldr#3128: keep the front door's cargo descendant in THIS
        // fixture's process group instead of a fresh one of its own, so the
        // deadline path below can signal the whole build tree on Unix. The
        // front door consumes the marker one hop deep
        // (`cargo_front_door::INHERIT_PARENT_PROCESS_GROUP_ENV`); rustc and
        // the shims inherit the group from cargo without further plumbing.
        .env("SOLDR_INTERNAL_INHERIT_PROCESS_GROUP", "1")
        // soldr#3128: a compiler shim whose daemon exited mid-compile (which
        // is precisely what this file's breach test provokes) waits for a
        // SESSION reply that can never arrive. The wrapper does fail
        // immediately on EOF -- but on Windows the broker relays the session
        // through `copy_bidirectional` over a local socket whose
        // `poll_shutdown` is a no-op, so the daemon's EOF is never forwarded
        // and the wrapper's only remaining bound is this backstop: 30 minutes
        // by default, i.e. 18x this test's whole nextest budget. Bounding it
        // here turns that state into a named SESSION-timeout failure inside
        // the per-build deadline instead of a bare nextest TIMEOUT. It does
        // not repair the relay (that is `running-process`'s contract, see
        // soldr#3128) -- it makes the symptom reportable.
        .env(
            soldr_cli::daemon::client::REPLY_TIMEOUT_ENV,
            COMPILE_REPLY_TIMEOUT_SECS.to_string(),
        );
    match ceiling_bytes {
        Some(bytes) => {
            cmd.env(rss_ceiling::RSS_CEILING_ENV_VAR, bytes.to_string());
        }
        None => {
            cmd.env_remove(rss_ceiling::RSS_CEILING_ENV_VAR);
        }
    }

    let child = common::tracked_child::spawn_tracked(&mut cmd).expect("spawn soldr");
    let pid = child.pid();
    let started = Instant::now();
    let result = child.wait_bounded(timeout);
    if result.timed_out {
        // Both diagnostics below are now reachable: the tree is dead and the
        // pipes were drained from spawn time, so nothing between here and the
        // panic can block (soldr#3128).
        if let Some(summary) = newest_breach_summary(cache_root, home_root) {
            panic!(
                "soldr {args:?} (pid {pid}) timed out after {timeout:?}, but this is a \
                 memory-ceiling breach, not a hang [{}]: {}",
                result.disposition(),
                rss_ceiling::legible_breach_message(&summary)
            );
        }
        panic!(
            "soldr {args:?} (pid {pid}) timed out after {timeout:?} (waited {:?}) [{}]\
             \nstdout:\n{}\nstderr:\n{}",
            started.elapsed(),
            result.disposition(),
            result.stdout_lossy(),
            result.stderr_lossy(),
        );
    }
    result.into_output()
}

/// Per-compile SESSION reply backstop for every `soldr` this fixture spawns.
/// Deliberately well under the 100 s per-build deadline so a wedged compile
/// is reported by the wrapper, naming the compile and the env var, before the
/// fixture's own deadline fires -- and both of those before nextest's 300 s.
/// See `run_soldr_build_with_ceiling` for why the default (1800 s) cannot
/// bound this test.
const COMPILE_REPLY_TIMEOUT_SECS: u64 = 45;

/// Best-effort teardown, bounded (soldr#3128).
///
/// `soldr daemon stop` waits up to 300 s for a shutdown acknowledgement, and
/// this runs from `Drop` -- including while a panic unwinds. An unbounded
/// `.output()` here can therefore outlive nextest's remaining budget and take
/// the already-generated panic diagnostic down with it, which is one of the
/// several ways this fixture could hide its own failure.
fn stop_daemon(cache_root: &Path, home_root: &Path) {
    for args in [["daemon", "stop"], ["broker", "stop"]] {
        let mut cmd = Command::new(common::soldr_bin());
        common::scrub_outer_soldr_env(&mut cmd);
        cmd.args(args)
            .env("SOLDR_CACHE_DIR", cache_root)
            .env("HOME", home_root)
            .env("USERPROFILE", home_root);
        match common::tracked_child::spawn_tracked(&mut cmd) {
            Ok(child) => {
                let result = child.wait_bounded(CLEANUP_TIMEOUT);
                if result.timed_out {
                    // Never a panic: teardown must not replace the failure
                    // the test is reporting. Printed so a stuck stop is
                    // visible in the same captured output.
                    println!(
                        "soldr#3128 cleanup: `soldr {args:?}` exceeded {CLEANUP_TIMEOUT:?} [{}]",
                        result.disposition()
                    );
                }
            }
            Err(error) => println!("soldr#3128 cleanup: spawning `soldr {args:?}` failed: {error}"),
        }
    }
}

/// Budget for each teardown command in [`stop_daemon`].
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(45);

struct DaemonCleanup {
    cache_root: PathBuf,
    home_root: PathBuf,
}

impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        stop_daemon(&self.cache_root, &self.home_root);
    }
}

/// Poll the watchdog status file for up to `deadline`. The watchdog samples
/// every [`rss_ceiling::RSS_SAMPLE_INTERVAL`] (2s) and only starts writing
/// once the ceiling env var was seen at daemon spawn, so the file may not
/// exist for the first couple of seconds after the daemon comes up.
fn wait_for_status(paths: &SoldrPaths, deadline: Instant) -> RssCeilingStatus {
    loop {
        if let Some(status) = rss_ceiling::read_status(paths) {
            if status.sample_count > 0 {
                return status;
            }
        }
        assert!(
            Instant::now() < deadline,
            "soldr-daemon RSS ceiling watchdog never wrote a status file with a sample \
             (soldr#3038): the daemon may not have forwarded SOLDR_DAEMON_RSS_CEILING_BYTES, \
             or never sampled before the deadline"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Poll for a breach dump under `cache_root` or `home_root` for up to
/// `deadline`.
///
/// Necessary because the watchdog only samples every
/// [`rss_ceiling::RSS_SAMPLE_INTERVAL`] (2s): a build whose compiles finish
/// faster than that can get a fully successful client-visible result
/// before the daemon's *next* tick notices it is over the ceiling and
/// dies. The breach is still real, and the dump still lands -- it is just
/// not guaranteed to be visible to the client synchronously. Asserting on
/// `output.status.success()` alone would make this test racy against that
/// timing; polling here (the same pattern [`wait_for_status`] already uses
/// for the non-breaching case) does not.
fn wait_for_breach_summary(
    cache_root: &Path,
    home_root: &Path,
    deadline: Instant,
) -> rss_ceiling::BreachSummary {
    loop {
        if let Some(summary) = newest_breach_summary(cache_root, home_root) {
            return summary;
        }
        assert!(
            Instant::now() < deadline,
            "no memory-breach dump appeared under {} or {} before the deadline (soldr#3057)\n{}",
            cache_root.display(),
            home_root.display(),
            breach_diagnostics(cache_root, home_root),
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// soldr#3762: what a missing dump leaves behind -- the watchdog's last
/// status (did the daemon sample? breach?), any `memory-breach-*` directory
/// at ANY depth (a dump the finder's depth limit missed), and the tail of
/// every log under both roots (the daemon's own account of its exit).
fn breach_diagnostics(cache_root: &Path, home_root: &Path) -> String {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 12 {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                walk(&path, depth + 1, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut report = String::from("--- soldr#3762 diagnostics ---\n");
    let paths = SoldrPaths::with_root(cache_root.to_path_buf());
    report.push_str(&format!(
        "watchdog status: {:?}\n",
        rss_ceiling::read_status(&paths)
    ));
    let mut files = Vec::new();
    walk(cache_root, 0, &mut files);
    walk(home_root, 0, &mut files);
    for file in &files {
        let text = file.to_string_lossy();
        if text.contains("memory-breach") {
            report.push_str(&format!("breach artifact: {}\n", file.display()));
        }
    }
    for file in files
        .iter()
        .filter(|f| f.extension().is_some_and(|e| e == "log"))
    {
        let bytes = fs::read(file).unwrap_or_default();
        let tail = &bytes[bytes.len().saturating_sub(4096)..];
        report.push_str(&format!(
            "--- tail of {} ---\n{}\n",
            file.display(),
            String::from_utf8_lossy(tail)
        ));
    }
    report
}

#[test]
fn daemon_stays_under_the_512mib_target_while_serving_a_chained_build() {
    let cache_root = unique_temp_dir("rss-ceiling-cache");
    let home_root = unique_temp_dir("rss-ceiling-home");
    let cleanup = DaemonCleanup {
        cache_root: cache_root.clone(),
        home_root: home_root.clone(),
    };

    let project = write_workload_workspace(&cache_root);

    // Pass 1: cold build of the whole chain -- CRATE_COUNT distinct rustc
    // invocations, sequential because each crate depends on the last. This
    // is also what spawns the daemon and forwards the ceiling env var into
    // it (see `run_soldr_build`'s doc comment).
    run_soldr_build(
        &["cargo", "build", "--quiet"],
        &cache_root,
        &home_root,
        &project,
        CEILING_BYTES,
        Duration::from_secs(180),
    );

    // Passes 2-4: touch a crate partway down the chain and rebuild. Each
    // pass is a genuine content-hash change (new functions appended, not a
    // no-op touch), so it is a real cache miss + recompile of that crate and
    // everything downstream of it, not a warm no-op that skips the wrapper
    // entirely.
    for (pass, touch_index) in [
        (1, CRATE_COUNT / 3),
        (2, CRATE_COUNT / 2),
        (3, CRATE_COUNT - 1),
    ] {
        write_workload_crate(&project, touch_index, pass * 5);
        run_soldr_build(
            &["cargo", "build", "--quiet"],
            &cache_root,
            &home_root,
            &project,
            CEILING_BYTES,
            Duration::from_secs(120),
        );
    }

    let paths = SoldrPaths::with_root(cache_root.clone());
    // The workload above is done; give the watchdog one more sample window
    // past the workload so a peak reached late in the last build is not
    // missed by racing the file write.
    let status = wait_for_status(&paths, Instant::now() + Duration::from_secs(15));

    // Report the observed peak regardless of pass/fail (soldr#3038 asks for
    // this explicitly): a passing run should still show the headroom under
    // the 512 MiB target, and this workload's small scale relative to the
    // 11.7 GiB production observation is itself a finding worth recording.
    println!(
        "soldr#3038 daemon RSS ceiling reproduction: pid={} ceiling={ceiling_mib:.1}MiB \
         peak={peak_mib:.1}MiB last={last_mib:.1}MiB samples={} mimalloc_heap_committed={:?} \
         mimalloc_detailed_stats={}",
        status.pid,
        status.sample_count,
        status.mimalloc_heap_committed_bytes,
        status.mimalloc_stats_detailed,
        ceiling_mib = status.ceiling_bytes as f64 / (1024.0 * 1024.0),
        peak_mib = status.peak_rss_bytes as f64 / (1024.0 * 1024.0),
        last_mib = status.last_rss_bytes as f64 / (1024.0 * 1024.0),
    );

    assert_eq!(status.ceiling_bytes, CEILING_BYTES);
    // If a breach did land in the trailing sample window above, report it
    // with the same legible message soldr#3057 requires everywhere else --
    // not just the bare status-file numbers -- including the dump path.
    if status.breached {
        if let Some(summary) = newest_breach_summary(&cache_root, &home_root) {
            panic!(
                "soldr#3038 daemon RSS ceiling reproduction breached unexpectedly: {}",
                rss_ceiling::legible_breach_message(&summary)
            );
        }
    }
    assert!(
        !status.breached,
        "soldr-daemon (pid {}) exceeded the {:.1} MiB RSS ceiling: peak observed {:.1} MiB \
         across {} samples -- see rss-ceiling-v1.json under {}",
        status.pid,
        status.ceiling_bytes as f64 / (1024.0 * 1024.0),
        status.peak_rss_bytes as f64 / (1024.0 * 1024.0),
        status.sample_count,
        paths.cache.join("soldr-daemon").display(),
    );

    drop(cleanup);
}

/// Budget for [`daemon_rss_retention_rate_per_compile_stays_within_budget`],
/// in bytes retained per compile served.
///
/// soldr#3059 measured the daemon retaining ~407 KiB/compile in the field
/// (10.09 GiB -> 12.43 GiB across 6,034 compiles in the compile journal,
/// RSS flat across a subsequent 55s idle sample -- growth tracks compile
/// volume, not wall clock). This test's own measured window (27 compiles,
/// see [`daemon_rss_retention_rate_per_compile_stays_within_budget`])
/// corroborates that field number directly on this defect: four repeated
/// runs on the development host that wrote this test landed at 438.5,
/// 467.3, 472.6, and 914.7 KiB/compile -- same order of magnitude as the
/// field figure (within ~2.2x on the high end), on a completely different,
/// much smaller workload. The spread across runs (438-915 KiB/compile) is
/// wide enough that this is a noisy signal, not a precise one -- expected
/// at 27 compiles on a shared, contended dev host -- but every run is
/// roughly an order of magnitude over the budget below, which is the
/// property this test actually needs. (That was the unfixed daemon; see the
/// test's `#[ignore]` reason for the warm-baseline rates measured on main
/// since.) (One caveat: this test forwards a
/// ceiling env var to arm the watchdog for `pid` discovery, and that also
/// starts `mimalloc-pprof`'s sampled profiler -- a source of extra
/// retention the field measurement did not carry. Distinguishing "the same
/// leak" from "the same leak plus a profiler tax" needs a heap profile,
/// which is exactly the follow-up soldr#3059 asks for.)
///
/// 32 KiB is chosen from the DEFECT side only, deliberately: it sits about
/// 14-15x below every measured rate above (so this gate trips immediately on
/// the real leak and keeps tripping on a partial fix that only shaves the
/// rate down). What it is explicitly NOT chosen from is a measured
/// healthy-daemon noise floor: no fixed daemon exists yet to measure one
/// against, so "a healthy daemon should retain close to zero bytes per
/// compile" is a working assumption, not a verified baseline -- and 32 KiB
/// multiplied out over this test's 27-compile window is only a ~864 KiB
/// total allowance, which is not obviously above mimalloc's own segment
/// granularity (segments are multi-MiB), so a fixed daemon could plausibly
/// still trip this budget on legitimate allocator noise rather than a real
/// per-compile leak. If that happens, raise the budget using that fixed
/// daemon's own measured rate as the new floor -- do not lower it
/// pre-emptively for a leak that has not been fixed yet.
const RETENTION_RATE_BUDGET_BYTES_PER_COMPILE: u64 = 32 * 1024;

/// Rebuild passes measured by
/// [`daemon_rss_retention_rate_per_compile_stays_within_budget`]. Touching
/// crate 0 recompiles the whole chain; a mid-chain touch recompiles the tail,
/// so the window serves a warm, known compile load.
const RATE_WINDOW_TOUCHES: [usize; 8] = [
    0,
    CRATE_COUNT / 3,
    0,
    CRATE_COUNT / 3,
    0,
    CRATE_COUNT / 3,
    0,
    CRATE_COUNT / 3,
];

/// How far daemon RSS may *fall* across the measured window before the
/// baseline is judged to still contain a warm-up transient. mimalloc returns
/// memory in multi-MiB segments, so a small shrink is ordinary. soldr#3059
/// observed an ~80 MiB drop with the old one-crate warm-up.
const BASELINE_SHRINK_TOLERANCE_BYTES: u64 = 16 * 1024 * 1024;

/// Count records in the daemon's own compile journal
/// (`compile_journal.jsonl` under `embedded_compile_journal_path`) rather
/// than assuming a compile count from the workload shape (e.g.
/// "CRATE_COUNT compiles"). The journal is the daemon's own accounting of
/// what it actually served -- exactly the population soldr#3059 attributes
/// the RSS growth to -- so it is the honest denominator; a hand-counted
/// "N crates were touched" figure would silently drift from reality the
/// moment a cache-hit/miss/link split stops being 1:1 with "one crate".
///
/// Confirmed empirically (`ts`, `outcome`, `compiler`, `args`, `cwd`,
/// `exit_code`, ... fields present) and against the zccache source: exactly
/// one `CompileJournal::log()` call per `handle_compile_ephemeral`
/// invocation in the embedded backend (`daemon/server/embedded.rs`) --  the
/// only backend soldr uses (the IPC `connection.rs` journal block is a
/// separate, unused-by-soldr path) -- so one line is one compile-or-link
/// unit dispatched to the wrapper, never a sub-phase record. `outcome` does
/// include dedicated `link_hit`/`link_miss` values, so a link step that
/// actually goes through the wrapper gets its own line rather than folding
/// into its compile's -- this workload's `write_workload_crate` produces
/// lib crates only, with no separate link step to record, which is why its
/// journal count (27) matches "one line per rustc invocation" exactly; a
/// workload with binaries would legitimately see more journal lines than
/// crates.
fn count_compile_journal_lines(cache_root: &Path) -> u64 {
    let paths = SoldrPaths::with_root(cache_root.to_path_buf());
    let journal_path = soldr_cli::zccache_embedded::embedded_compile_journal_path(&paths);
    match fs::read_to_string(&journal_path) {
        Ok(body) => body.lines().filter(|line| !line.trim().is_empty()).count() as u64,
        Err(_) => 0,
    }
}

/// soldr#3059: the daemon retains memory *linearly in compiles served*, not
/// on a timer. [`daemon_stays_under_the_512mib_target_while_serving_a_chained_build`]
/// above is an absolute-ceiling instrument: at the measured ~407 KiB/compile
/// rate it needs ~1,289 compiles to trip the 512 MiB ceiling. The ceiling
/// test's own workload (one cold 12-crate build plus three rebuild passes)
/// serves only 27 compiles by the daemon's own journal count -- NOT the ~45
/// the originating issue assumed from the workload's shape; hand-counting
/// crates touched is exactly the kind of assumption
/// [`count_compile_journal_lines`]'s doc comment explains this test avoids
/// -- so that reproduction was always going to pass, not because the
/// workload is the wrong shape, but because the ceiling is the wrong
/// instrument for a linear leak (roughly 48x too small to reach it at 27
/// compiles). This test asserts the RATE instead: bytes retained per
/// compile served, sampled directly via
/// `soldr_platform::host::resources::process_rss_bytes` -- the same
/// function the daemon's own watchdog samples itself with (see
/// `rss_ceiling::sample_tick`) -- rather than through the ceiling
/// watchdog's own 2s-cadence peak tracking, and bracketing that same
/// 27-compile workload with the cold build folded into the measured window
/// instead of spent on warm-up (warm-up here is a single trivial crate
/// instead -- see the test body for why that matters for a per-compile
/// rate). See [`RETENTION_RATE_BUDGET_BYTES_PER_COMPILE`] for the budget
/// and [`count_compile_journal_lines`] for the denominator.
///
/// # Ignored, with a valid instrument (soldr#3059)
///
/// This used to fail on the unfixed daemon at 438-915 KiB/compile. On main
/// (2026-09-13), with the warm-up baseline below, ten local runs measured
/// 9-73 KiB/compile. RSS growth is bimodal and one run in ten crossed the
/// budget, so the test stays `#[ignore]` (this repo's convention for a
/// documented regression instrument kept in-tree; see `cli_cargo_basic.rs`'s
/// `cargo_front_door_defaults_non_msvc_dev_debug_off_and_warns_once_per_repo`)
/// rather than becoming a flaky gate. Run it explicitly with
/// `soldr cargo nextest run -p soldr-cli --test daemon --run-ignored all
/// -E 'test(daemon_rss_retention_rate_per_compile_stays_within_budget)'`. Do
/// not un-ignore it by loosening the budget: first explain the ~6 MiB tail
/// with a heap profile.
#[test]
#[ignore = "soldr#3059: not a CI gate yet. With a warm baseline on main \
    (2026-09-13) the rate is 9-73 KiB/compile (ten local runs: 40 compiles \
    9.1/10.9/14.0/25.3/26.3, 80 compiles 9.2/17.3/9.5/25.9/72.75), down from \
    438-915 KiB/compile when the leak was filed, but RSS growth is bimodal \
    (usually under 2 MiB, sometimes ~6 MiB) and one run in ten exceeds the \
    32 KiB/compile budget. Un-ignoring would flake. Telling allocator segment \
    growth apart from residual retention needs a heap profile. Run \
    explicitly with `--run-ignored all` to see the measured rate."]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn daemon_rss_retention_rate_per_compile_stays_within_budget() {
    let cache_root = unique_temp_dir("rss-rate-cache");
    let home_root = unique_temp_dir("rss-rate-home");
    let cleanup = DaemonCleanup {
        cache_root: cache_root.clone(),
        home_root: home_root.clone(),
    };

    // Warm-up (soldr#3059 follow-up): the baseline has to come after the
    // daemon has served real compiles. The previous one-trivial-crate warm-up
    // left a startup transient in the "before" sample. On main (688eac21) RSS
    // *fell* from ~160 MiB to ~80 MiB across the window, and a saturating
    // subtraction turned that into "0 KiB retained": a pass that measured
    // nothing. So the trivial crate (which spawns the daemon), the cold
    // chained build and one rebuild pass are all warm-up, and only rebuild
    // passes are measured.
    let trivial = write_trivial_crate(&cache_root);
    run_soldr_build(
        &["cargo", "build", "--quiet"],
        &cache_root,
        &home_root,
        &trivial,
        CEILING_BYTES,
        Duration::from_secs(120),
    );
    let project = write_workload_workspace(&cache_root);
    run_soldr_build(
        &["cargo", "build", "--quiet"],
        &cache_root,
        &home_root,
        &project,
        CEILING_BYTES,
        Duration::from_secs(180),
    );
    write_workload_crate(&project, 0, 1);
    run_soldr_build(
        &["cargo", "build", "--quiet"],
        &cache_root,
        &home_root,
        &project,
        CEILING_BYTES,
        Duration::from_secs(120),
    );

    let paths = SoldrPaths::with_root(cache_root.clone());
    let warm_status = wait_for_status(&paths, Instant::now() + Duration::from_secs(15));
    // Let RSS settle past the warm-up builds before taking the "before"
    // sample -- same reasoning as the ceiling test's trailing sample wait.
    std::thread::sleep(rss_ceiling::RSS_SAMPLE_INTERVAL * 2);

    let rss_before = soldr_platform::host::resources::process_rss_bytes(warm_status.pid)
        .expect("daemon RSS must be readable before the measured window");
    let compiles_before = count_compile_journal_lines(&cache_root);

    // Measured window: rebuild passes only. Touching crate `k` appends fresh
    // functions (a real content-hash change, so a genuine miss) and
    // recompiles `k..CRATE_COUNT` down the chain, so the touches in
    // [`RATE_WINDOW_TOUCHES`] serve a known, warm, repeatable compile load.
    for (pass, &touch_index) in RATE_WINDOW_TOUCHES.iter().enumerate() {
        write_workload_crate(&project, touch_index, (pass + 2) * 5);
        run_soldr_build(
            &["cargo", "build", "--quiet"],
            &cache_root,
            &home_root,
            &project,
            CEILING_BYTES,
            Duration::from_secs(120),
        );
    }

    // Settle once more before the "after" sample, for the same reason as
    // above -- a peak reached in the last build's final moments should not
    // be raced against the read.
    std::thread::sleep(rss_ceiling::RSS_SAMPLE_INTERVAL * 2);

    // Guard against a silent-garbage measurement: if the daemon were
    // displaced mid-window (a fresh daemon winning the route, an old one
    // exiting), `process_rss_bytes(warm_status.pid)` would read whatever
    // process the OS has since recycled that pid to -- succeeding with a
    // number that means nothing, not failing loudly. The watchdog status
    // file's `pid` is the daemon's own report of who it currently is;
    // requiring it to still match the pid sampled at the start of the
    // window is a direct, cheap check that both RSS samples came from the
    // same process.
    let final_status = rss_ceiling::read_status(&paths)
        .expect("watchdog status file must still exist after the measured window");
    assert_eq!(
        final_status.pid, warm_status.pid,
        "the daemon serving cache_root was displaced mid-window (pid {} -> {}); the RSS \
         delta below would span two different processes and is meaningless",
        warm_status.pid, final_status.pid
    );

    let rss_after = soldr_platform::host::resources::process_rss_bytes(warm_status.pid)
        .expect("daemon RSS must be readable after the measured window");
    let compiles_after = count_compile_journal_lines(&cache_root);

    let compiles_served = compiles_after.saturating_sub(compiles_before);
    assert!(
        compiles_served > 0,
        "measured window served zero compiles per the daemon's own compile journal \
         ({compiles_before} -> {compiles_after}); the retention rate cannot be computed"
    );

    let retained_bytes = rss_after.saturating_sub(rss_before);
    let shrunk_bytes = rss_before.saturating_sub(rss_after);
    let rate_bytes_per_compile = retained_bytes / compiles_served;

    // Report the measured rate regardless of pass/fail -- soldr#3059 asks
    // for this explicitly, and it is the number this whole test exists to
    // surface.
    println!(
        "soldr#3059 daemon RSS retention rate: pid={pid} rss_before={rss_before_kib:.1}KiB \
         rss_after={rss_after_kib:.1}KiB compiles_served={compiles_served} \
         delta={delta_kib:+.1}KiB retained={retained_kib:.1}KiB rate={rate_kib:.2}KiB/compile \
         budget={budget_kib:.1}KiB/compile",
        pid = warm_status.pid,
        rss_before_kib = rss_before as f64 / 1024.0,
        rss_after_kib = rss_after as f64 / 1024.0,
        delta_kib = (rss_after as f64 - rss_before as f64) / 1024.0,
        retained_kib = retained_bytes as f64 / 1024.0,
        rate_kib = rate_bytes_per_compile as f64 / 1024.0,
        budget_kib = RETENTION_RATE_BUDGET_BYTES_PER_COMPILE as f64 / 1024.0,
    );

    // A shrink beyond allocator noise means the baseline still carried a
    // transient. A rate computed from it reads as zero however much the
    // daemon retains, so refuse to report one rather than pass.
    assert!(
        shrunk_bytes <= BASELINE_SHRINK_TOLERANCE_BYTES,
        "daemon RSS fell {:.1} KiB across the measured window ({:.1} -> {:.1} KiB), more \
         than the {:.1} KiB tolerance: the warm-up baseline still included a transient, so \
         this run cannot measure retention (soldr#3059)",
        shrunk_bytes as f64 / 1024.0,
        rss_before as f64 / 1024.0,
        rss_after as f64 / 1024.0,
        BASELINE_SHRINK_TOLERANCE_BYTES as f64 / 1024.0,
    );

    assert!(
        rate_bytes_per_compile <= RETENTION_RATE_BUDGET_BYTES_PER_COMPILE,
        "soldr-daemon (pid {}) retained {:.1} KiB/compile across {} compiles -- over the \
         {:.1} KiB/compile budget (soldr#3059): {:.1} KiB total retained ({:.1} KiB -> {:.1} \
         KiB); see compile_journal.jsonl under {}",
        warm_status.pid,
        rate_bytes_per_compile as f64 / 1024.0,
        compiles_served,
        RETENTION_RATE_BUDGET_BYTES_PER_COMPILE as f64 / 1024.0,
        retained_bytes as f64 / 1024.0,
        rss_before as f64 / 1024.0,
        rss_after as f64 / 1024.0,
        paths.cache.join("soldr-daemon").display(),
    );

    drop(cleanup);
}

/// Deliberately below the daemon's own real compile-driven peak while
/// serving [`write_workload_workspace`]'s 12-crate chained build (~78-93
/// MiB, measured directly while building this reproduction on a real host)
/// but comfortably above a cold daemon's startup baseline, so the breach
/// happens once real compiles are underway, not before.
const GUARANTEED_BREACH_CEILING_BYTES: u64 = 32 * 1024 * 1024;

/// soldr#3057: reproduces the reversed decision -- on breach, the daemon
/// dumps its memory state and exits immediately, and a caller driving a
/// build through it must see a legible message (role, pid, ceiling,
/// observed peak, dump path), not a bare transport error.
///
/// Two builds against a shared `home_root` but two distinct cache roots:
///
/// 1. A trivial single-crate build against `cache_root_a` with the ceiling
///    env var *removed* entirely. This is what spawns (or discovers) the
///    `soldr-broker` singleton for `home_root` -- and because the broker
///    reads `SOLDR_DAEMON_RSS_CEILING_BYTES` exactly once, at its own
///    startup (`broker_server.rs`'s `serve_loop`), a broker spawned here
///    never arms its own watchdog and cannot breach later no matter how
///    large its own RSS gets serving other routes. Without this priming
///    build, an earlier version of this test found the *broker* breaching
///    first in practice (its own transient RSS spikes while verifying and
///    placing a fresh daemon image can exceed a tight ceiling before the
///    daemon ever gets a sample) -- a real demonstration of the exact same
///    contract, but not a targeted one, and it left the daemon's own
///    `run_watchdog` breach arm (as opposed to the broker's use of the
///    same loop) with no integration coverage of its own.
/// 2. The real 12-crate chained build against a SECOND, fresh
///    `cache_root_b` -- a distinct route, so a brand new daemon is
///    launched for it -- with the ceiling set. `SOLDR_*` env vars forward
///    wholesale into a broker-spawned daemon per route
///    (`daemon::lifecycle::spawn_env::FORWARDED_ENV_PREFIX`), so this
///    daemon (and only this daemon) gets the ceiling and breaches once its
///    real compile-driven peak clears it.
///
/// This test asserts on the dump directly ([`wait_for_breach_summary`])
/// rather than on build 2's own exit status: the watchdog samples every
/// [`rss_ceiling::RSS_SAMPLE_INTERVAL`] (2s), so a fast build can finish
/// with a fully successful, client-visible result before the daemon's
/// *next* tick notices the breach and dies moments later. That race does
/// not make the breach any less real, or the dump any less legible -- it
/// just means "the client saw a failure" is not a safe thing to assert on
/// here. [`assert_build_succeeded_or_report_breach`]'s "check for the dump
/// before trusting a transport error" contract is what a caller uses when
/// a breach genuinely does land mid-build and the client DOES see a
/// failure -- demonstrated directly by [`newest_breach_summary`] finding a
/// *broker*-role dump during manual reproduction of this exact scenario
/// while building soldr#3057 (a broker's own transient RSS spike while
/// verifying and placing a fresh daemon image can breach a tight ceiling
/// before the daemon ever gets a sample -- exactly why build 1 below
/// primes an un-watched broker first, so this test targets the daemon's
/// own breach arm specifically rather than leaving it to chance which of
/// the two breaches first).
#[test]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn daemon_dies_and_dumps_memory_when_the_ceiling_is_breached() {
    let home_root = unique_temp_dir("rss-breach-home");
    let cache_root_a = unique_temp_dir("rss-breach-cache-a");
    let cache_root_b = unique_temp_dir("rss-breach-cache-b");
    let cleanup_a = DaemonCleanup {
        cache_root: cache_root_a.clone(),
        home_root: home_root.clone(),
    };
    let cleanup_b = DaemonCleanup {
        cache_root: cache_root_b.clone(),
        home_root: home_root.clone(),
    };

    // Build 1: no ceiling at all -- primes an un-watched broker singleton
    // for `home_root`. See the function doc above for why this isolates
    // the daemon's breach path from the broker's.
    //
    // soldr#3096: each per-build wait (100s) is deliberately under the
    // nextest budget for this test (`.config/nextest.toml`, 300s) so a
    // stalled build is reported by this test's own panic -- naming the
    // build and carrying its stdout/stderr -- rather than by nextest's
    // bare TIMEOUT line.
    let trivial = write_trivial_crate(&cache_root_a);
    let priming = run_soldr_build_with_ceiling(
        &["cargo", "build", "--quiet"],
        &cache_root_a,
        &home_root,
        &trivial,
        None,
        Duration::from_secs(100),
    );
    // soldr#3128: assert it. A failed priming build does not prove an
    // un-watched broker exists, and every later assertion in this test is
    // premised on one -- so a silent failure here turns the whole test into a
    // coin flip over which process breaches first.
    assert_build_succeeded_or_report_breach(
        &["cargo", "build", "--quiet"],
        &cache_root_a,
        &home_root,
        &priming,
    );

    // Build 2: the real workload, with the ceiling, against a fresh route
    // -- a brand new daemon, forwarded the ceiling, sharing the same
    // (un-watched) broker discovered in build 1.
    let project = write_workload_workspace(&cache_root_b);
    let output = run_soldr_build_allow_failure(
        &["cargo", "build", "--quiet"],
        &cache_root_b,
        &home_root,
        &project,
        GUARANTEED_BREACH_CEILING_BYTES,
        Duration::from_secs(100),
    );
    if output.status.success() {
        // Not a failure of this test: the watchdog samples every
        // RSS_SAMPLE_INTERVAL (2s), and this workload's 12 compiles can
        // finish -- with a fully successful client-visible result -- before
        // the daemon's next tick notices it is over the ceiling. The
        // client-visible-failure case (a daemon or broker breaching before
        // or during a build, which the earlier version of this test relied
        // on unconditionally) is exercised for the *broker* by
        // `assert_build_succeeded_or_report_breach`'s own doc example and
        // by manual reproduction while building soldr#3057; here the
        // breach is asserted directly against the dump below instead of
        // against this build's own exit status.
        println!(
            "soldr#3057: build 2 exited 0 before the daemon's next watchdog tick noticed the \
             breach -- expected given RSS_SAMPLE_INTERVAL is 2s; polling for the dump the \
             daemon writes shortly after."
        );
    }

    // Poll rather than assert immediately: see `wait_for_breach_summary`'s
    // own docs for why the breach is not guaranteed to already be on disk
    // the instant the build call above returns.
    let summary = wait_for_breach_summary(
        &cache_root_b,
        &home_root,
        Instant::now() + Duration::from_secs(20),
    );

    // This is the legible message soldr#3057 requires: role, pid, ceiling,
    // observed peak, and the dump path, printed here so a human reading
    // `nextest` output (or the live demonstration) sees it directly.
    let message = rss_ceiling::legible_breach_message(&summary);
    println!("soldr#3057 breach reproduction: {message}");

    assert_eq!(
        summary.role,
        rss_ceiling::ProcessRole::Daemon,
        "the broker primed with no ceiling in build 1 must never be the one that breaches: \
         {summary:?}"
    );
    assert_eq!(summary.ceiling_bytes, GUARANTEED_BREACH_CEILING_BYTES);
    assert!(
        summary.peak_rss_bytes > GUARANTEED_BREACH_CEILING_BYTES,
        "a recorded breach must show a peak over the ceiling: {summary:?}"
    );
    assert!(
        summary.dump_dir.is_dir(),
        "dump directory must exist on disk: {summary:?}"
    );
    assert!(
        summary.mimalloc_stats_path.is_file(),
        "exact mimalloc counters must be on disk: {summary:?}"
    );

    // soldr#3053: cgroup OOM counters alone are not sufficient evidence
    // (CLAUDE.md, "Diagnosing before capping"), so a breach dump must carry
    // the per-process picture beside them -- the cgroup-wide numbers AND the
    // compiles that were in flight. Asserted here, on a REAL daemon breach,
    // because the unit test in `rss_ceiling.rs` can only prove the writer
    // works; this proves the artifact survives the actual
    // `std::process::exit(1)` path a breaching daemon takes.
    let cgroup_path = summary
        .cgroup_path
        .clone()
        .unwrap_or_else(|| panic!("summary.json must name cgroup.json: {summary:?}"));
    assert_eq!(
        cgroup_path,
        summary.dump_dir.join("cgroup.json"),
        "cgroup.json must live inside the dump directory: {summary:?}"
    );
    assert!(
        cgroup_path.is_file(),
        "cgroup.json must exist on disk: {summary:?}"
    );
    let cgroup_body = fs::read_to_string(&cgroup_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", cgroup_path.display()));
    let cgroup: serde_json::Value = serde_json::from_str(&cgroup_body)
        .unwrap_or_else(|error| panic!("cgroup.json must be valid JSON ({error}): {cgroup_body}"));
    // Presence, not value: every cgroup field is `Option` and serializes as
    // `null` on a host with no readable cgroup v2 controller, which is a
    // legitimate reading and must not be conflated with zero.
    for key in [
        "cgroup_current_bytes",
        "cgroup_peak_bytes",
        "cgroup_limit_bytes",
        "cgroup_oom_kills",
        "cgroup_pids_current",
        "system_available_bytes",
    ] {
        assert!(
            cgroup.get(key).is_some(),
            "cgroup.json must carry {key}: {cgroup_body}"
        );
    }
    assert!(
        cgroup
            .get("in_flight_compiles")
            .and_then(serde_json::Value::as_array)
            .is_some(),
        "cgroup.json must carry the in-flight compile list (possibly empty -- the breach \
         may land between compiles): {cgroup_body}"
    );
    assert!(
        summary.schema_version >= 2,
        "the breach schema must be bumped for the cgroup.json artifact: {summary:?}"
    );

    assert!(message.contains(&summary.role.to_string()));
    assert!(message.contains(&summary.pid.to_string()));
    assert!(message.contains(rss_ceiling::RSS_CEILING_ENV_VAR));

    drop(cleanup_a);
    drop(cleanup_b);
}

/// A child that outlives its parent and keeps the inherited pipes open --
/// the exact shape of a `soldr cargo build` fixture whose rustc/shim
/// grandchildren survive a `Child::kill()` on the root.
///
/// Unix: `sh` backgrounds `sleep` and waits, so `sleep` is a real grandchild
/// rather than an `exec` of the shell itself (shells `exec` the final command
/// of a `-c` string, which would collapse the tree to one process and stop
/// this fixture from modelling anything).
///
/// Windows: `cmd.exe` -> `ping.exe`, soldr#2605's observed shape.
fn lingering_grandchild_command() -> Command {
    if is_windows_host() {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "echo", "hi", "&", "ping", "-n", "60", "127.0.0.1"]);
        cmd
    } else {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo hi; sleep 60 & wait"]);
        cmd
    }
}

/// Host selection is a runtime question here, not a `#[cfg]` one: soldr#2493
/// keeps host `#[cfg]` inside `soldr-platform` (enforced by
/// `.github/scripts/platform_cfg_boundary_ratchet.py`), and both branches
/// above compile everywhere regardless.
fn is_windows_host() -> bool {
    matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    )
}

/// soldr#3128, the regression this file's own guard needed.
///
/// Before the fix, `run_soldr_build_with_ceiling`'s timeout path was
/// `child.kill()` followed by `child.wait_with_output()`. `kill` ends one
/// process; the grandchildren inherit the piped stdout/stderr, so the pipe
/// never reaches EOF and `wait_with_output` blocks forever -- taking the
/// panic on the next line, the one carrying the build's output, with it. On
/// the Windows target-run lanes that surfaced as a bare nextest
/// `TIMEOUT [> 300s]` with empty captured output, which names neither the
/// build nor the stall.
///
/// This test asserts the two properties that make the fixture's own
/// diagnostic reachable, against a child that deliberately leaves a
/// pipe-holding descendant behind: the wait returns well inside its own
/// bound, and the output captured before the deadline is still there.
#[test]
fn a_timed_out_fixture_child_is_reaped_with_its_tree_and_its_output() {
    let mut cmd = lingering_grandchild_command();
    let child = common::tracked_child::spawn_tracked(&mut cmd).expect("spawn lingering fixture");
    let started = Instant::now();
    let result = child.wait_bounded(Duration::from_secs(2));
    let elapsed = started.elapsed();

    assert!(
        result.timed_out,
        "a 60s sleeper must not settle inside a 2s deadline [{}]",
        result.disposition()
    );
    // The descendant would hold the pipes for a further ~58s. Returning
    // inside 30s is the property: the wait is bounded by the helper's own
    // budget (deadline + reap + drain grace), not by the survivor.
    assert!(
        elapsed < Duration::from_secs(30),
        "wait_bounded took {elapsed:?}, which means collection waited on the surviving \
         descendant rather than on its own budget [{}]",
        result.disposition()
    );
    assert!(
        result.stdout_lossy().contains("hi"),
        "output written before the deadline must survive the timeout path; got stdout={:?} \
         stderr={:?} [{}]",
        result.stdout_lossy(),
        result.stderr_lossy(),
        result.disposition()
    );
    assert!(
        result.pipes_closed,
        "the whole tree must be gone, so both pipes reach EOF -- a still-open pipe means a \
         descendant survived the tree kill [{}]",
        result.disposition()
    );
}

/// The ordinary path is unchanged: a child that exits on its own reports its
/// real status and its complete output. (The normal-exit branch used to call
/// the same unbounded `wait_with_output`, so it needed the same fix.)
#[test]
fn a_fixture_child_that_exits_on_its_own_reports_status_and_output() {
    let mut cmd = if is_windows_host() {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "echo", "done"]);
        cmd
    } else {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo done"]);
        cmd
    };
    let child = common::tracked_child::spawn_tracked(&mut cmd).expect("spawn quick fixture");
    let result = child.wait_bounded(Duration::from_secs(30));
    assert!(!result.timed_out, "[{}]", result.disposition());
    assert!(result.pipes_closed, "[{}]", result.disposition());
    assert!(
        result.stdout_lossy().contains("done"),
        "stdout={:?} [{}]",
        result.stdout_lossy(),
        result.disposition()
    );
    let output = result.into_output();
    assert!(output.status.success());
}
