//! Strict PCH serving under clang's strict validation flags (#1952).
//!
//! A clang PCH records each input's spelling, size, mtime and content hash.
//! A byte-identical header whose mtime differs from the record makes a served
//! PCH load and then lose that header's `#pragma once` state, so the
//! consumer fails with `redefinition of ...`. These fixtures drive the real
//! daemon and real clang with FastLED's flags, timestamps on:
//!
//! - producer: `-Werror=invalid-pch -verify-pch -fpch-validate-input-files-content`
//! - consumer: `-Werror=invalid-pch -fpch-validate-input-files-content`
//!
//! The consumer is a bare clang compile, so it checks the served PCH bytes
//! exactly as a build would.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use kernal_api::platform::fs::{set_file_mtime, FileTime};
use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

/// `canonicalize` without Windows' `\\?\` verbatim prefix, which clang
/// never records.
fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    let path = std::fs::canonicalize(path)?;
    let text = path.to_string_lossy();
    Ok(text
        .strip_prefix(r"\\?\")
        .map_or_else(|| path.clone(), PathBuf::from))
}

#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

/// The mtime every fixture header starts with, so the PCH records it.
const RECORDED_SECS: i64 = 1_600_000_000;
/// A later, whole-second mtime standing in for a checkout or editor touch.
const TOUCHED_SECS: i64 = 1_700_000_000;

const HEADER: &str =
    "#pragma once\n// v1\nstruct A { int x; };\ntemplate <class T = int> struct B {};\n";

struct Daemon {
    endpoint: String,
    handle: tokio::task::JoinHandle<()>,
    shutdown: std::sync::Arc<kernal_api::async_engine::Notify>,
    _cache_root: tempfile::TempDir,
}

impl Daemon {
    async fn start() -> Self {
        let endpoint = zccache::ipc::unique_test_endpoint();
        let cache_root = tempfile::tempdir().unwrap();
        let cache_dir: zccache::core::NormalizedPath =
            cache_root.path().join("zccache-cache").into();
        let mut server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
        let shutdown = server.shutdown_handle();
        let handle = tokio::spawn(async move { server.run(0).await.unwrap() });
        Self {
            endpoint,
            handle,
            shutdown,
            _cache_root: cache_root,
        }
    }

    async fn stop(self) {
        self.shutdown.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.handle).await;
    }
}

/// One checkout: `pch.h` includes `inc/a.h`; `use.cpp` includes `a.h` again
/// and uses its template's default argument, so a lost `#pragma once` fails.
struct Checkout {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Checkout {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path()).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("inc")).unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::write(root.join("inc/a.h"), HEADER).unwrap();
        std::fs::write(root.join("pch.h"), "#pragma once\n#include \"a.h\"\n").unwrap();
        std::fs::write(
            root.join("use.cpp"),
            "#include \"a.h\"\nB<> b;\nint main() { A a{}; return a.x; }\n",
        )
        .unwrap();
        for name in ["inc/a.h", "pch.h", "use.cpp"] {
            set_mtime(&root.join(name), RECORDED_SECS);
        }
        Self { _dir: dir, root }
    }

    fn header(&self) -> PathBuf {
        self.root.join("inc/a.h")
    }

    fn pch(&self) -> PathBuf {
        self.root.join("build/pch.h.pch")
    }

    fn log(&self) -> PathBuf {
        self.root.join("logs/session.log")
    }

    fn cwd(&self) -> String {
        self.root.join("build").to_string_lossy().into_owned()
    }

    fn producer_args(&self, include_dir: &str) -> Vec<String> {
        [
            "-x",
            "c++-header",
            "-c",
            &self.root.join("pch.h").to_string_lossy(),
            &format!("-I{include_dir}"),
            "-o",
            &self.pch().to_string_lossy(),
            "-Wno-unused-command-line-argument",
            "-Werror=invalid-pch",
            "-verify-pch",
            "-fpch-validate-input-files-content",
        ]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect()
    }

    fn default_include(&self) -> String {
        self.root.join("inc").to_string_lossy().into_owned()
    }

    /// The consumer's argv, for a compile that loads the PCH via zccache.
    fn consumer_args(&self) -> Vec<String> {
        vec![
            "-c".to_owned(),
            self.root.join("use.cpp").to_string_lossy().into_owned(),
            format!("-I{}", self.default_include()),
            "-include-pch".to_owned(),
            self.pch().to_string_lossy().into_owned(),
            "-Werror=invalid-pch".to_owned(),
            "-fpch-validate-input-files-content".to_owned(),
            "-o".to_owned(),
            self.root.join("build/use.o").to_string_lossy().into_owned(),
        ]
    }

    /// Compile `use.cpp` against the PCH with bare clang (no zccache).
    fn consume(&self, clang: &Path) -> (bool, String) {
        let out = std::process::Command::new(clang)
            .current_dir(self.root.join("build"))
            .arg("-c")
            .arg(self.root.join("use.cpp"))
            .arg(format!("-I{}", self.default_include()))
            .arg("-include-pch")
            .arg(self.pch())
            .args(["-Werror=invalid-pch", "-fpch-validate-input-files-content"])
            .arg("-o")
            .arg(self.root.join("build/use.o"))
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

fn set_mtime(path: &Path, secs: i64) {
    set_file_mtime(path, FileTime::from_unix_time(secs, 0)).unwrap();
}

fn mtime_secs(path: &Path) -> i64 {
    FileTime::from_last_modification_time(&std::fs::metadata(path).unwrap()).unix_seconds()
}

async fn start_session(client: &mut ClientConn, checkout: &Checkout) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: checkout.cwd().into(),
            log_file: Some(checkout.log().to_string_lossy().into_owned().into()),
            track_stats: false,
            journal_path: None,
            profile: false,
            private_daemon: None,
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
}

/// Build the PCH through the daemon. Returns whether it was a cache hit.
async fn build_pch(
    client: &mut ClientConn,
    session_id: &str,
    clang: &Path,
    checkout: &Checkout,
    include_dir: &str,
    replay: Option<&str>,
) -> bool {
    let args = checkout.producer_args(include_dir);
    compile(client, session_id, clang, checkout, args, replay).await
}

/// Run one compile through the daemon; it must succeed. Returns `cached`.
async fn compile(
    client: &mut ClientConn,
    session_id: &str,
    clang: &Path,
    checkout: &Checkout,
    args: Vec<String>,
    replay: Option<&str>,
) -> bool {
    let (exit_code, cached, stderr) =
        compile_outcome(client, session_id, clang, checkout, args, replay).await;
    assert_eq!(exit_code, 0, "compile failed: {stderr}");
    cached
}

/// Run one compile through the daemon. Returns `(exit_code, cached, stderr)`.
async fn compile_outcome(
    client: &mut ClientConn,
    session_id: &str,
    clang: &Path,
    checkout: &Checkout,
    args: Vec<String>,
    replay: Option<&str>,
) -> (i32, bool, String) {
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    // The harness's own ZCCACHE_* settings (e.g. a worktree root pointing at
    // this repository) must not leak into the fixture's checkout.
    env.retain(|(key, _)| !key.starts_with("ZCCACHE_"));
    if let Some(value) = replay {
        env.push(("ZCCACHE_PCH_MTIME_REPLAY".to_owned(), value.to_owned()));
    }
    client
        .send(&Request::Compile {
            session_id: session_id.to_owned(),
            args,
            cwd: checkout.cwd().into(),
            compiler: clang.to_string_lossy().into_owned().into(),
            env: Some(env),
            stdin: Vec::new(),
        })
        .await
        .unwrap();
    loop {
        match client.recv().await.unwrap() {
            Some(Response::CompileProgress { .. }) => continue,
            Some(Response::CompileResult {
                exit_code,
                cached,
                stderr,
                ..
            }) => {
                eprintln!(
                    "--- session log after compile (cached={cached}) ---\n{}",
                    log_text(checkout)
                );
                return (
                    exit_code,
                    cached,
                    String::from_utf8_lossy(&stderr).into_owned(),
                );
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }
    }
}

/// Let the daemon's watcher observe filesystem changes.
fn settle() {
    std::thread::sleep(Duration::from_millis(600));
}

fn assert_consumes(checkout: &Checkout, clang: &Path, context: &str) {
    let (ok, stderr) = checkout.consume(clang);
    assert!(ok, "{context}: consumer failed against the PCH:\n{stderr}");
}

/// Fresh checkout + daemon + session, with one cold PCH build that the
/// consumer accepts. Returns the original PCH bytes.
async fn cold_build(clang: &Path) -> (Checkout, Daemon, ClientConn, String, Vec<u8>) {
    let checkout = Checkout::new();
    let daemon = Daemon::start().await;
    let mut client = zccache::ipc::connect(&daemon.endpoint).await.unwrap();
    let sid = start_session(&mut client, &checkout).await;
    let include = checkout.default_include();
    let cached = build_pch(&mut client, &sid, clang, &checkout, &include, None).await;
    assert!(!cached, "first PCH build must be a miss");
    assert_consumes(&checkout, clang, "cold build");
    let bytes = std::fs::read(checkout.pch()).unwrap();
    std::fs::remove_file(checkout.pch()).unwrap();
    std::fs::remove_file(checkout.root.join("build/use.o")).unwrap();
    (checkout, daemon, client, sid, bytes)
}

/// Whether this clang records xxh3 input hashes (clang >= 19). Older clang
/// stores an `llvm::hash_code`, so zccache cannot verify content and never
/// replays; the content-verified cases below then do not apply.
fn records_xxh3(checkout: &Checkout, pch_bytes: &[u8]) -> bool {
    use zccache::fingerprint::pch_inputs::{content_xxh3, parse_pch_inputs};
    let header = checkout.root.join("pch.h");
    let inputs = parse_pch_inputs(pch_bytes).unwrap().unwrap();
    let Some(record) = inputs.iter().find(|input| Path::new(&input.path) == header) else {
        return false;
    };
    let xxh3 = record.content_xxh3 == Some(content_xxh3(&std::fs::read(&header).unwrap()));
    if !xxh3 {
        eprintln!("skipping: this clang does not record xxh3 input hashes");
    }
    xxh3
}

fn log_text(checkout: &Checkout) -> String {
    std::fs::read_to_string(checkout.log()).unwrap_or_default()
}

/// Negative control: the cached PCH served as-is after an identical header
/// was touched breaks the consumer with the redefinition error.
#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn touched_header_without_replay_breaks_the_consumer() {
    let Some(clang) = zccache::test_support::find_clang() else {
        eprintln!("skipping test: clang not found");
        return;
    };
    let (checkout, daemon, _client, _sid, stale_pch) = cold_build(&clang).await;
    set_mtime(&checkout.header(), TOUCHED_SECS);
    std::fs::write(checkout.pch(), &stale_pch).unwrap();
    let (ok, stderr) = checkout.consume(&clang);
    assert!(!ok, "the stale PCH unexpectedly compiled");
    assert!(
        stderr.contains("redefinition"),
        "expected a redefinition error, got:\n{stderr}"
    );
    daemon.stop().await;
}

#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn touched_identical_header_is_served_with_mtime_replay() {
    let Some(clang) = zccache::test_support::find_clang() else {
        eprintln!("skipping test: clang not found");
        return;
    };
    let (checkout, daemon, mut client, sid, stale_pch) = cold_build(&clang).await;
    if !records_xxh3(&checkout, &stale_pch) {
        daemon.stop().await;
        return;
    }
    set_mtime(&checkout.header(), TOUCHED_SECS);
    settle();

    let include = checkout.default_include();
    let cached = build_pch(&mut client, &sid, &clang, &checkout, &include, None).await;
    assert!(cached, "touched-but-identical header should still hit");
    assert_eq!(std::fs::read(checkout.pch()).unwrap(), stale_pch);
    assert_eq!(
        mtime_secs(&checkout.header()),
        RECORDED_SECS,
        "the header's mtime must be replayed to the recorded value"
    );
    assert!(
        mtime_secs(&checkout.pch()) >= mtime_secs(&checkout.header()),
        "the served PCH must not be older than its inputs"
    );
    let log = log_text(&checkout);
    assert!(log.contains("[PCH_MTIME_REPLAY]"), "log:\n{log}");
    assert_consumes(&checkout, &clang, "replayed hit");
    daemon.stop().await;
}

#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn touched_header_with_replay_disabled_is_a_miss() {
    let Some(clang) = zccache::test_support::find_clang() else {
        eprintln!("skipping test: clang not found");
        return;
    };
    let (checkout, daemon, mut client, sid, pch_bytes) = cold_build(&clang).await;
    if !records_xxh3(&checkout, &pch_bytes) {
        daemon.stop().await;
        return;
    }
    set_mtime(&checkout.header(), TOUCHED_SECS);
    settle();

    let include = checkout.default_include();
    let cached = build_pch(&mut client, &sid, &clang, &checkout, &include, Some("0")).await;
    assert!(!cached, "replay disabled: a touched header must rebuild");
    assert_eq!(mtime_secs(&checkout.header()), TOUCHED_SECS);
    let log = log_text(&checkout);
    assert!(
        log.contains("mtime_mismatch_replay_disabled"),
        "log:\n{log}"
    );
    assert_consumes(&checkout, &clang, "replay disabled rebuild");

    // The rebuilt PCH records the touched mtime, so it differs from the
    // cached bytes under the same key. Depending on publication timing it
    // either replaces the entry or the key is evicted as a conflict (#1244)
    // and the next build stores it; either way an unchanged tree hits again
    // within two builds, and every served PCH passes the gate.
    let mut hit = false;
    for _ in 0..2 {
        std::fs::remove_file(checkout.pch()).unwrap();
        settle();
        hit = build_pch(&mut client, &sid, &clang, &checkout, &include, Some("0")).await;
        assert_consumes(&checkout, &clang, "rebuild on unchanged tree");
        if hit {
            break;
        }
    }
    assert!(hit, "the rebuilt PCH should be served on an unchanged tree");
    daemon.stop().await;
}

#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn same_size_content_edit_is_a_miss() {
    let Some(clang) = zccache::test_support::find_clang() else {
        eprintln!("skipping test: clang not found");
        return;
    };
    let (checkout, daemon, mut client, sid, pch_bytes) = cold_build(&clang).await;
    if !records_xxh3(&checkout, &pch_bytes) {
        daemon.stop().await;
        return;
    }
    let edited = HEADER.replace("// v1", "// v2");
    assert_eq!(edited.len(), HEADER.len());
    std::fs::write(checkout.header(), edited).unwrap();
    // Worst case: the edit even keeps the recorded mtime.
    set_mtime(&checkout.header(), RECORDED_SECS);
    settle();

    let include = checkout.default_include();
    let cached = build_pch(&mut client, &sid, &clang, &checkout, &include, None).await;
    assert!(!cached, "a same-size content edit must rebuild the PCH");
    assert_consumes(&checkout, &clang, "same-size edit");
    daemon.stop().await;
}

#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn size_change_is_a_miss() {
    let Some(clang) = zccache::test_support::find_clang() else {
        eprintln!("skipping test: clang not found");
        return;
    };
    let (checkout, daemon, mut client, sid, _) = cold_build(&clang).await;
    std::fs::write(checkout.header(), format!("{HEADER}// grown\n")).unwrap();
    settle();

    let include = checkout.default_include();
    let cached = build_pch(&mut client, &sid, &clang, &checkout, &include, None).await;
    assert!(!cached, "a size change must rebuild the PCH");
    assert_consumes(&checkout, &clang, "size change");
    daemon.stop().await;
}

#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn different_include_spelling_is_a_miss() {
    let Some(clang) = zccache::test_support::find_clang() else {
        eprintln!("skipping test: clang not found");
        return;
    };
    let (checkout, daemon, mut client, sid, _) = cold_build(&clang).await;
    // Same directory, different spelling: the PCH would record it.
    let respelled = format!("{}/build/../inc", checkout.root.display());
    let cached = build_pch(&mut client, &sid, &clang, &checkout, &respelled, None).await;
    assert!(!cached, "a different -I spelling must not reuse the PCH");
    assert_consumes(&checkout, &clang, "respelled include");
    daemon.stop().await;
}

#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn pch_from_another_checkout_is_refused() {
    let Some(clang) = zccache::test_support::find_clang() else {
        eprintln!("skipping test: clang not found");
        return;
    };
    let (checkout_a, daemon, _client_a, _sid_a, _) = cold_build(&clang).await;
    let checkout_b = Checkout::new();
    set_mtime(&checkout_a.header(), TOUCHED_SECS);
    settle();

    let mut client_b = zccache::ipc::connect(&daemon.endpoint).await.unwrap();
    let sid_b = start_session(&mut client_b, &checkout_b).await;
    let include_b = checkout_b.default_include();
    let cached = build_pch(&mut client_b, &sid_b, &clang, &checkout_b, &include_b, None).await;
    assert!(!cached, "a PCH from another checkout must not be served");
    assert_eq!(
        mtime_secs(&checkout_a.header()),
        TOUCHED_SECS,
        "nothing in the other checkout may be touched"
    );
    assert_consumes(&checkout_b, &clang, "checkout B");
    daemon.stop().await;
}

/// FastLED's layout: `top.h.pch` is built with `-include-pch pch.h.pch`, so
/// it records the base PCH's size and mtime. On a clean rebuild both are
/// served from the cache; the base lands with a fresh mtime, and serving the
/// chain must replay the recorded one or clang rejects the chain.
#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn chained_pch_is_served_with_base_mtime_replay() {
    let Some(clang) = zccache::test_support::find_clang() else {
        eprintln!("skipping test: clang not found");
        return;
    };
    let checkout = Checkout::new();
    let root = &checkout.root;
    std::fs::write(
        root.join("top.h"),
        "#pragma once\n#include \"a.h\"\nstruct C { A a; };\n",
    )
    .unwrap();
    std::fs::write(
        root.join("use_top.cpp"),
        "#include \"a.h\"\n#include \"top.h\"\nB<> b;\nint main() { C c{}; return c.a.x; }\n",
    )
    .unwrap();
    set_mtime(&root.join("top.h"), RECORDED_SECS);
    let top_pch = root.join("build/top.h.pch");
    let include = checkout.default_include();
    let top_args = || -> Vec<String> {
        let mut args = checkout.producer_args(&include);
        args[3] = root.join("top.h").to_string_lossy().into_owned();
        args[6] = top_pch.to_string_lossy().into_owned();
        args.push("-include-pch".to_owned());
        args.push(checkout.pch().to_string_lossy().into_owned());
        args
    };
    let consume_top = || {
        let out = std::process::Command::new(&clang)
            .current_dir(root.join("build"))
            .arg("-c")
            .arg(root.join("use_top.cpp"))
            .arg(format!("-I{include}"))
            .arg("-include-pch")
            .arg(&top_pch)
            .args(["-Werror=invalid-pch", "-fpch-validate-input-files-content"])
            .arg("-o")
            .arg(root.join("build/use_top.o"))
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    let daemon = Daemon::start().await;
    let mut client = zccache::ipc::connect(&daemon.endpoint).await.unwrap();
    let sid = start_session(&mut client, &checkout).await;
    assert!(!build_pch(&mut client, &sid, &clang, &checkout, &include, None).await);
    assert!(!compile(&mut client, &sid, &clang, &checkout, top_args(), None).await);
    let (ok, stderr) = consume_top();
    assert!(ok, "cold chain: {stderr}");

    // Clean rebuild: both PCHs come back from the cache.
    std::fs::remove_file(checkout.pch()).unwrap();
    std::fs::remove_file(&top_pch).unwrap();
    settle();
    assert!(build_pch(&mut client, &sid, &clang, &checkout, &include, None).await);
    // The re-materialized base carries a fresh mtime, not the recorded one.
    set_mtime(&checkout.pch(), TOUCHED_SECS);
    settle();
    assert!(
        compile(&mut client, &sid, &clang, &checkout, top_args(), None).await,
        "the chained PCH should be served"
    );
    let log = log_text(&checkout);
    assert!(log.contains("[PCH_MTIME_REPLAY]"), "log:\n{log}");
    assert_ne!(mtime_secs(&checkout.pch()), TOUCHED_SECS);
    let (ok, stderr) = consume_top();
    assert!(ok, "served chain must load: {stderr}");
    daemon.stop().await;
}

/// A PCH already on disk, loaded by a compile zccache runs: the build tool
/// never re-runs the PCH step (its output looks current), so zccache must
/// repair the drifted header before clang loads the PCH. Covers a touch
/// forwards (checkout, editor save) and backwards (cache/tar restore).
async fn loaded_pch_is_repaired_before_the_consumer_runs(drifted_secs: i64) {
    let Some(clang) = zccache::test_support::find_clang() else {
        eprintln!("skipping test: clang not found");
        return;
    };
    let checkout = Checkout::new();
    let daemon = Daemon::start().await;
    let mut client = zccache::ipc::connect(&daemon.endpoint).await.unwrap();
    let sid = start_session(&mut client, &checkout).await;
    let include = checkout.default_include();
    build_pch(&mut client, &sid, &clang, &checkout, &include, None).await;
    let pch_bytes = std::fs::read(checkout.pch()).unwrap();
    if !records_xxh3(&checkout, &pch_bytes) {
        daemon.stop().await;
        return;
    }
    set_mtime(&checkout.header(), drifted_secs);
    let (ok, stderr) = checkout.consume(&clang);
    assert!(
        !ok,
        "negative control: the drifted header should break bare clang"
    );
    assert!(stderr.contains("redefinition"), "stderr:\n{stderr}");
    settle();

    let args = checkout.consumer_args();
    compile(&mut client, &sid, &clang, &checkout, args, None).await;
    assert_eq!(mtime_secs(&checkout.header()), RECORDED_SECS);
    assert_eq!(
        std::fs::read(checkout.pch()).unwrap(),
        pch_bytes,
        "the on-disk PCH is never modified"
    );
    let log = log_text(&checkout);
    assert!(log.contains("for loaded"), "log:\n{log}");
    daemon.stop().await;
}

#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn loaded_pch_header_touched_forwards_is_repaired() {
    loaded_pch_is_repaired_before_the_consumer_runs(TOUCHED_SECS).await;
}

#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn loaded_pch_header_moved_backwards_is_repaired() {
    loaded_pch_is_repaired_before_the_consumer_runs(RECORDED_SECS - 86_400).await;
}

/// A real edit to a header the loaded PCH records is never papered over:
/// zccache leaves the file alone and clang's strict validation rejects the
/// PCH. Replay switched off leaves an identical touched header alone too.
#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn loaded_pch_with_real_edit_or_replay_off_is_not_repaired() {
    let Some(clang) = zccache::test_support::find_clang() else {
        eprintln!("skipping test: clang not found");
        return;
    };
    let checkout = Checkout::new();
    let daemon = Daemon::start().await;
    let mut client = zccache::ipc::connect(&daemon.endpoint).await.unwrap();
    let sid = start_session(&mut client, &checkout).await;
    let include = checkout.default_include();
    build_pch(&mut client, &sid, &clang, &checkout, &include, None).await;
    let pch_bytes = std::fs::read(checkout.pch()).unwrap();
    if !records_xxh3(&checkout, &pch_bytes) {
        daemon.stop().await;
        return;
    }

    // Replay off: the touched header keeps its new mtime.
    set_mtime(&checkout.header(), TOUCHED_SECS);
    settle();
    let args = checkout.consumer_args();
    let (exit_code, _, _) =
        compile_outcome(&mut client, &sid, &clang, &checkout, args, Some("0")).await;
    assert_ne!(exit_code, 0, "replay off: clang must still reject the PCH");
    assert_eq!(mtime_secs(&checkout.header()), TOUCHED_SECS);

    // Same-size real edit: refused, never replayed.
    std::fs::write(checkout.header(), HEADER.replace("v1", "v2")).unwrap();
    set_mtime(&checkout.header(), TOUCHED_SECS);
    settle();
    let args = checkout.consumer_args();
    let (exit_code, _, _) = compile_outcome(&mut client, &sid, &clang, &checkout, args, None).await;
    assert_ne!(exit_code, 0, "an edited header must invalidate the PCH");
    assert_eq!(mtime_secs(&checkout.header()), TOUCHED_SECS);
    let log = log_text(&checkout);
    assert!(log.contains("[PCH_NOT_REPAIRED]"), "log:\n{log}");
    assert!(log.contains("content_mismatch"), "log:\n{log}");
    daemon.stop().await;
}
