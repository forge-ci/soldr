//! Shared helpers for the `cli_*` integration test binaries. Each binary
//! pulls in `mod common;`; `#![allow(dead_code)]` silences unused-helper warnings on a
//! per-binary basis without sprinkling allows over individual helpers. The
//! same situation applies to imports: some integration binaries use
//! `serde_json::Value` / `std::io::Write` / `Duration` / `Instant`, others
//! don't — `#![allow(unused_imports)]` keeps the common-helpers pattern
//! working under `-D warnings`.
#![allow(dead_code, unused_imports)]

mod clippy_tools;
use clippy_tools::{fake_cargo_clippy_script, fake_clippy_driver_script};

/// soldr#2951 — the single barrier for the soldr-toolchain catalogue
/// environment variables. See the module doc for why it is deliberately not
/// `TEST_PROCESS_ENV_LOCK`.
pub(crate) mod catalogue_env;
/// soldr#3128 — a fixture child whose deadline covers its whole process
/// tree and whose output collection is bounded on every OS.
pub(crate) mod fixture_command;
pub(crate) mod isolated_daemon;
pub(crate) mod route_claim;
pub(crate) mod tracked_child;

pub(crate) use fixture_command::{isolated_soldr_command_in, isolated_soldr_command_with_target};

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);
static ALIAS_LOCK: Mutex<()> = Mutex::new(());
static MATERIALIZED_ALIAS_PAIRS: Mutex<Vec<(PathBuf, PathBuf)>> = Mutex::new(Vec::new());

/// Resolve the soldr binary path for tests. Prefers the `SOLDR_BIN`
/// env var so a runner that downloaded a pre-built artifact can point
/// the tests at it; falls back to `env!("CARGO_BIN_EXE_soldr")` so
/// local `cargo test` keeps working without setup.
///
/// soldr#1039 / #1038 phase 1: `env!` expands at compile time, baking
/// the build-machine's absolute path into the test binary. On a target
/// runner that downloaded the test artifact from a Linux builder,
/// that path doesn't exist. This helper centralizes the lookup so
/// production CI can set `SOLDR_BIN` explicitly without touching any
/// test code.
/// soldr#1766: integration fixtures build in bare temp workspaces that
/// deliberately carry no `rust-toolchain.toml`, and the pin search walks
/// ancestors, which cannot reach one from under the OS temp dir.
///
/// Declaring the opt-out here rather than at each call site is deliberate:
/// there are ~92 direct `Command::new(soldr_bin())` spawns across the test
/// tree, and a per-site opt-in would silently miss every future one. Every
/// spawn resolves the binary through this function, so setting it once in
/// the test process covers them all by inheritance.
///
/// Tests that mean to exercise the pin requirement itself live in
/// `toolchain.rs` unit tests, where this never runs.
fn allow_unpinned_fixtures() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var(soldr_cli::toolchain::ALLOW_UNPINNED_ENV_VAR, "1");
        // soldr#2566: under native CI lanes this test binary itself runs
        // beneath `soldr cargo nextest run`, so it inherits the outer
        // soldr's IN_SOLDR_PID. Tests model fresh user sessions — the
        // soldr children they spawn are new roots, not re-entries — so
        // scrub the marker once for the whole test process. The canary
        // suite (cli_reentrancy_guard_canary) re-injects it explicitly to
        // prove strict-mode rejection end-to-end.
        std::env::remove_var("IN_SOLDR_PID");
    });
}

pub(crate) fn soldr_bin() -> PathBuf {
    allow_unpinned_fixtures();
    let soldr = std::env::var_os("SOLDR_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_soldr")));
    materialize_runtime_alias(&soldr, "soldr-daemon");
    soldr
}

/// Cargo capability inherited by a test process. Under prescribed CI this is
/// the named shim that re-enters the source Soldr binary; Cargo itself supplies
/// its absolute binary for ordinary local `cargo test` / Nextest runs.
pub(crate) fn cargo_bin() -> std::ffi::OsString {
    std::env::var_os("CARGO").expect("test process must inherit the Cargo capability")
}

/// Companion to `soldr_bin` for tests that need the daemon binary.
/// Prefers `SOLDR_DAEMON_BIN`; falls back to the in-crate compile-time
/// path. soldr#1039.
#[allow(dead_code)]
pub(crate) fn soldr_daemon_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("SOLDR_DAEMON_BIN") {
        return PathBuf::from(p);
    }
    runtime_alias_path(&soldr_bin(), "soldr-daemon")
}

fn runtime_alias_path(soldr: &Path, stem: &str) -> PathBuf {
    let file = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    };
    soldr.parent().expect("soldr binary parent").join(file)
}

fn materialize_runtime_alias(soldr: &Path, stem: &str) {
    let _guard = ALIAS_LOCK.lock().expect("runtime alias lock");
    let target = runtime_alias_path(soldr, stem);
    let pair = (soldr.to_path_buf(), target.clone());
    let mut materialized = MATERIALIZED_ALIAS_PAIRS
        .lock()
        .expect("materialized alias pairs lock");
    if materialized.contains(&pair) {
        return;
    }
    if files_equal(soldr, &target) {
        materialized.push(pair);
        return;
    }
    let tmp = target.with_extension(format!("alias-tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    if std::fs::hard_link(soldr, &tmp).is_err() {
        std::fs::copy(soldr, &tmp).unwrap_or_else(|err| {
            panic!(
                "failed to materialize {} from {}: {err}",
                target.display(),
                soldr.display()
            )
        });
    }
    let _ = std::fs::remove_file(&target);
    std::fs::rename(&tmp, &target).expect("install runtime alias");
    materialized.push(pair);
}

/// Whether `left` and `right` already hold the same bytes.
///
/// # The identity short-circuit (soldr#3168)
///
/// `materialize_runtime_alias` creates the alias with `std::fs::hard_link`, so
/// in the steady state these two paths are **the same file**:
///
/// ```text
/// $ stat -c 'ino=%i nlink=%h %n' target/debug/soldr target/debug/soldr-daemon
/// ino=11042743 nlink=35 target/debug/soldr
/// ino=11042743 nlink=35 target/debug/soldr-daemon
/// ```
///
/// Without the check below this reads 110 MB twice -- 220 MB -- to conclude a
/// file equals itself, and it does so once per test **process**: the
/// `MATERIALIZED_ALIAS_PAIRS` memo above is per-process, and nextest gives
/// every test its own. Measured at ~24 ms per test on a warm page cache.
///
/// `soldr_platform::fs::identity::same_file` answers that in O(1) and is
/// strictly *more* correct than the byte scan for this caller's question: two
/// paths to one inode cannot disagree, and the scan only ever confirmed that
/// the long way round. It already existed -- the host-specific comparison
/// belongs behind the `soldr-platform` boundary, not in a test helper.
///
/// The byte comparison stays as the fallback, which is the case it was written
/// for -- a distinct file whose contents may or may not match.
///
/// # Honest scope
///
/// soldr#3168 first measured this as a 15.7% suite win and that was wrong: a
/// cold-vs-warm comparison, where page-cache warming accounted for nearly all
/// of it. A warm-vs-warm A/B/A put the real effect at ~1.4%, inside run-to-run
/// noise on a 16-core box. It is kept because it is O(1) replacing O(size) and
/// because a 4-vCPU CI runner is the case the local measurement flatters, not
/// because the local number justifies it.
pub(crate) fn files_equal(left: &Path, right: &Path) -> bool {
    if soldr_platform::fs::identity::same_file(left, right) {
        return true;
    }
    let Ok(left_meta) = std::fs::metadata(left) else {
        return false;
    };
    let Ok(right_meta) = std::fs::metadata(right) else {
        return false;
    };
    if left_meta.len() != right_meta.len() {
        return false;
    }
    let Ok(left_file) = std::fs::File::open(left) else {
        return false;
    };
    let Ok(right_file) = std::fs::File::open(right) else {
        return false;
    };
    let mut left = std::io::BufReader::new(left_file);
    let mut right = std::io::BufReader::new(right_file);
    let mut left_buf = [0_u8; 64 * 1024];
    let mut right_buf = [0_u8; 64 * 1024];
    loop {
        let Ok(left_read) = left.read(&mut left_buf) else {
            return false;
        };
        let Ok(right_read) = right.read(&mut right_buf) else {
            return false;
        };
        if left_read != right_read || left_buf[..left_read] != right_buf[..right_read] {
            return false;
        }
        if left_read == 0 {
            return true;
        }
    }
}

/// Resolve the runtime checkout used by source-coupled archived tests.
#[allow(dead_code)]
pub(crate) fn workspace_root() -> PathBuf {
    if let Some(path) = std::env::var_os("SOLDR_TEST_WORKSPACE_ROOT") {
        return PathBuf::from(path);
    }
    if let Ok(current_dir) = std::env::current_dir() {
        for ancestor in current_dir.ancestors() {
            if ancestor.join("Cargo.toml").is_file()
                && ancestor.join("crates/soldr-cli/Cargo.toml").is_file()
            {
                return ancestor.to_path_buf();
            }
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("crates/soldr-cli has a workspace root two levels up")
        .to_path_buf()
}

#[allow(dead_code)]
pub(crate) fn crate_root() -> PathBuf {
    workspace_root().join("crates").join("soldr-cli")
}

/// Resolve the test-fixtures directory. Prefers `SOLDR_TEST_FIXTURES_DIR`
/// (set by the cross-build CI when fixtures are packaged separately
/// from the test binary); falls back to `<CARGO_MANIFEST_DIR>/tests/fixtures`
/// for local-dev. soldr#1040 / #1038 phase 2.
#[allow(dead_code)]
pub(crate) fn fixtures_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("SOLDR_TEST_FIXTURES_DIR") {
        return PathBuf::from(p);
    }
    crate_root().join("tests").join("fixtures")
}

pub(crate) fn isolated_soldr_command() -> Command {
    let mut command = Command::new(soldr_bin());
    scrub_outer_soldr_env(&mut command);
    // soldr#3452: the front door finds a toolchain pin in any ancestor of its
    // working directory, as rustup does. A test that leaves the cwd at its
    // default (the crate directory) would therefore pick up this repository's
    // own rust-toolchain.toml and try to prepare its real toolchain. Start in a
    // directory with no pin above it; a test that needs a pin (or any other
    // cwd) sets `current_dir` after this call and wins.
    command.current_dir(pin_free_working_dir());
    command
}

/// An empty directory outside the repository, shared by every test in the
/// process, so no `rust-toolchain.toml` exists in it or above it.
pub(crate) fn pin_free_working_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| unique_temp_dir("pin-free-cwd")).clone()
}

/// Ambient variables that can select a daemon route independently of the
/// fixture's explicit HOME/root. Nested Soldr commands must start without
/// these values; a test that needs one sets it after calling the helper.
pub(crate) const OUTER_ROUTE_ENV_VARS: &[&str] = &[
    soldr_cli::core::SOLDR_CACHE_DIR_ENV_VAR,
    soldr_cli::daemon::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR,
    soldr_cli::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR,
    soldr_cli::daemon::session_endpoint::SOLDR_SESSION_ENDPOINT_PATH_ENV,
    soldr_cli::daemon::session_endpoint::SOLDR_CONTROL_ENDPOINT_PATH_ENV,
    "SOLDR_INTERNAL_BROKER_INSTANCE_ID",
    "RUNNING_PROCESS_SERVICE_DEF_DIR",
];

pub(crate) fn is_outer_route_env(name: &str) -> bool {
    OUTER_ROUTE_ENV_VARS.contains(&name) || name.starts_with("RUNNING_PROCESS_BROKER_V1_")
}

/// Stop the stable broker a fixture's front-door calls may have started
/// under its temp HOME. `daemon stop` deliberately leaves the broker
/// running (soldr#2549), so a fixture that never stops it leaks one
/// detached broker process per run — 96 were found alive on one dev box
/// (soldr#2568). Best effort: an absent broker is a cheap no-op.
pub(crate) fn stop_fixture_broker(cache_root: &std::path::Path, home_root: &std::path::Path) {
    let mut command = isolated_soldr_command();
    command
        .args(["broker", "stop"])
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home_root)
        .env("USERPROFILE", home_root);
    let _ = command.output();
}

/// Stop the broker-routed daemon generation a fixture's front-door compiles
/// launched under its temp HOME and cache root.
///
/// soldr#3136: a fixture that talks to its own directly spawned daemon over
/// the private control endpoint still compiles through the broker's SESSION
/// route, and the broker launches a separate daemon generation for that
/// route. `broker stop` retains daemon routes by design (soldr#2549), so this
/// must run before [`stop_fixture_broker`]. Best effort: with no routed
/// daemon it is a cheap no-op.
pub(crate) fn stop_fixture_daemon_route(cache_root: &std::path::Path, home_root: &std::path::Path) {
    let mut command = isolated_soldr_command();
    command
        .args(["daemon", "stop"])
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home_root)
        .env("USERPROFILE", home_root);
    let _ = command.output();
}

/// Drop guard for fixtures that have no other teardown struct. Declare it
/// FIRST in the test body so it drops LAST — after any daemon guard has
/// already stopped the daemon the broker fronts.
pub(crate) struct BrokerHomeGuard {
    cache_root: std::path::PathBuf,
    home_root: std::path::PathBuf,
}

impl BrokerHomeGuard {
    pub(crate) fn new(cache_root: &std::path::Path, home_root: &std::path::Path) -> Self {
        Self {
            cache_root: cache_root.to_path_buf(),
            home_root: home_root.to_path_buf(),
        }
    }
}

impl Drop for BrokerHomeGuard {
    fn drop(&mut self) {
        // soldr#3516: `broker stop` retains daemon routes by design
        // (soldr#2549), so a fixture that ran `daemon start` (or any routed
        // compile) and only stopped the broker left its daemon behind as an
        // orphan. Stop the routed generation first, then the broker, then
        // hard-reap whatever still survives. This runs during unwinding too,
        // so a panicking test cannot orphan its daemon either.
        stop_fixture_daemon_route(&self.cache_root, &self.home_root);
        stop_fixture_broker(&self.cache_root, &self.home_root);
        reap_fixture_daemon(&self.cache_root, &[&self.cache_root, &self.home_root]);
    }
}

/// True when `path` lies under one of `roots` (compared both as given and
/// canonicalized, so a symlinked temp dir such as macOS `/var` still matches).
pub(crate) fn path_is_under_any(path: &Path, roots: &[&Path]) -> bool {
    roots.iter().any(|root| {
        path.starts_with(root) || fs::canonicalize(root).is_ok_and(|real| path.starts_with(real))
    })
}

/// soldr#3516: terminate the daemon recorded in `cache_root`'s route claim if
/// it is still alive after a graceful stop. Only a process whose image lives
/// under one of `fixture_roots` is ever signalled, so a recycled PID or a
/// claim naming a real daemon can never turn into authority to kill it.
pub(crate) fn reap_fixture_daemon(cache_root: &Path, fixture_roots: &[&Path]) {
    use soldr_platform::process::{inspect, terminate};
    use std::time::{Duration, Instant};

    let Some(pid) = route_claim::route_claim_pid(cache_root) else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while inspect::is_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if !inspect::is_alive(pid) {
        return;
    }
    let Some(exe) = inspect::executable_path(pid) else {
        return;
    };
    if !path_is_under_any(&exe, fixture_roots) {
        eprintln!(
            "fixture reap: route claim PID {pid} runs {} outside the fixture; not signalling it",
            exe.display()
        );
        return;
    }
    eprintln!(
        "fixture reap: daemon PID {pid} ({}) survived daemon/broker stop; terminating it",
        exe.display()
    );
    terminate::terminate_pid(pid);
}

pub(crate) fn scrub_outer_soldr_env(command: &mut Command) -> &mut Command {
    command
        // soldr#1766: fixtures build in bare temp workspaces that deliberately
        // have no rust-toolchain.toml, and ancestor-walking will not find one
        // under the OS temp dir. They are exercising other behavior, so opt
        // them out of the pin requirement rather than seeding a manifest into
        // every fixture.
        .env(soldr_cli::toolchain::ALLOW_UNPINNED_ENV_VAR, "1")
        // soldr#3466: a cacheable `cargo build/test/...` front door eagerly
        // materializes managed cmake + ninja into `$SOLDR_CACHE_DIR/bin/syslib`.
        // Every fixture has its own fresh cache root, so 92 fixture front
        // doors each downloaded both bundles from GitHub (~3-5 s apiece) for
        // workspaces that never run cmake. Select system cmake instead, the
        // same opt-out the lib unit tests use; a fixture that exercises the
        // managed-cmake injection removes this after calling the helper.
        .env(soldr_cli::blessed_build::USE_SYSTEM_CMAKE_ENV_VAR, "1")
        // soldr#3530: the same shape, one lookup earlier. Each fresh cache
        // root made the front door fetch the toolchain catalogue
        // (zackees.github.io/soldr-toolchain + raw.githubusercontent.com parts)
        // before doing anything else: ~5 s per fixture on a GitHub runner,
        // x127 cargo_front_door tests = the whole 604 s CI-vs-local gap.
        // Measured on a real runner (runs 36964344956, 36965017158): 5.2 s ->
        // 0.17 s per test with the lookup disabled. Fixtures model a front
        // door, not catalogue resolution; one that exercises the catalogue
        // removes this after calling the helper (fetch_tools tests drive it
        // in-process under `catalogue_env::CatalogueEnvGuard`).
        .env(
            soldr_cli::fetch::manifest_lookup::MANIFEST_DISABLE_ENV_VAR,
            "1",
        )
        .env_remove("RUSTC_WRAPPER")
        // `soldr ci-test` deliberately points this override at the source
        // binary. Nested fixture front doors model fresh callers: inheriting
        // the outer override makes them skip their own managed-daemon service
        // registration, then their compiler re-entry asks the broker for a
        // route whose service definition was never installed.
        .env_remove("SOLDR_RUSTC_WRAPPER")
        // soldr#2545: the outer dogfooded suite driver exports the owned
        // effective-wrapper mirror beside RUSTC_WRAPPER. Scrubbing only one
        // of the pair would make every wrapper-shaped fixture child read as
        // Soldr-owned drift; the pair travels together.
        .env_remove(soldr_cli::wrapper_identity::EFFECTIVE_WRAPPER_ENV)
        .env_remove(soldr_cli::wrapper_identity::EFFECTIVE_WRAPPER_ORIGIN_ENV)
        // Fixtures spawn soldr from a non-soldr test process; the outer
        // dogfooded `soldr cargo nextest` stamps IN_SOLDR_PID, which would
        // make every fixture child look like unsanctioned re-entrancy once
        // CI flips SOLDR_REENTRANCY_GUARD=strict (soldr#2566). Scrub both.
        .env_remove(soldr_cli::reentrancy_guard::IN_SOLDR_PID_ENV)
        .env_remove(soldr_cli::reentrancy_guard::GUARD_MODE_ENV)
        // An agent's own `SOLDR_PORCELAIN=1` must not turn a fixture's stderr
        // assertions into JSON-only runs; a porcelain test sets it explicitly.
        .env_remove("SOLDR_PORCELAIN")
        // soldr#2785: never let a fixture delegate to a globally-installed
        // soldr. `global_upgrade::maybe_delegate` walks ancestors for
        // `[workspace.metadata.soldr] prefer_newer_global`, which THIS
        // checkout sets, and these tests run with cargo's cwd inside it. On a
        // hit it runs `<global soldr> --version` as a child process -- and per
        // its own doc that child "stages a broker image under the inherited
        // HOME and spawns `broker serve`", which is what made the
        // broker-absent tests find a broker in their isolated homes
        // (soldr#2521 D).
        //
        // Two costs, both unwanted here. The probe is a process spawn per
        // invocation: it is 143-271ms of the 151/209/280ms front-door traces
        // in the soldr#2785 failures, i.e. nearly the whole startup. And if
        // the installed soldr were ever NEWER than the one under test, the
        // fixture would silently exercise the installed binary instead of the
        // branch's -- a test that passes by testing the wrong thing.
        //
        // `cli_global_upgrade.rs` deliberately does not use this helper, so
        // the delegation policy keeps its own coverage.
        .env(
            soldr_cli::global_upgrade::GLOBAL_DELEGATION_DISABLE_ENV_VAR,
            "1",
        )
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        // soldr#3823: the fast-linker default drives the link through a clang
        // driver, and on a host without system clang (and without rustc's own
        // lld) that clang is managed LLVM -- which needs the catalogue that
        // MANIFEST_DISABLE above switches off. Fixtures model a front door,
        // not linker selection, so leave the linker alone; a test that
        // exercises linker injection sets or removes SOLDR_LINKER itself after
        // calling this helper (cli_cargo_linker, cli_cargo_dylint_link).
        .env("SOLDR_LINKER", "default")
        .env_remove("CARGO_BUILD_TARGET")
        // Local Docker builders keep the outer compilation cache in a named
        // /target volume. Test fixtures must still exercise their own
        // workspace-local target/ trees and sidecars.
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        // #692: parent-cache-sharing env vars leak from the setup-soldr
        // action's exported environment ("Parent-cache sharing is
        // default-on" per CLAUDE.md). They make the test fixture's
        // child soldr take the "user already set ZCCACHE_PATH_REMAP"
        // branch and skip the `--private-env` injection that the
        // contract-matrix test asserts on, breaking
        // `cli_zccache_contract_matrix` on every Linux/macOS CI run.
        // Scrub them so soldr always exercises its own injection path
        // under tests, regardless of the parent process's parent-cache
        // configuration.
        .env_remove("ZCCACHE_PATH_REMAP")
        .env_remove("ZCCACHE_WORKTREE_ROOT")
        .env_remove("SOLDR_PATH_REMAP")
        // Self-relocation markers leak from an outer dogfooding soldr
        // (`soldr cargo test ...`) into the test-built soldr child. A
        // stale SOLDR_ORIGINAL_EXE makes `soldr_binary_source()` resolve
        // the OUTER soldr binary, so `toolchain link` shims get written
        // from the wrong executable; a stale SOLDR_RELOCATED_EXE
        // suppresses the child's own relocation. Scrub both so the test
        // binary behaves like a fresh top-level invocation.
        .env_remove("SOLDR_ORIGINAL_EXE")
        .env_remove("SOLDR_RELOCATED_EXE")
        // soldr#3516: a fixture's HOME must decide every per-user location.
        // An ambient XDG base dir would place the fixture broker's service
        // definitions and staged daemon images in the developer's real
        // config tree even under a throwaway HOME. A test that needs one sets
        // it after this helper.
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME");
    for name in OUTER_ROUTE_ENV_VARS {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        let should_scrub = name.to_str().is_some_and(|name| {
            is_outer_route_env(name)
                || name.starts_with("CARGO_TARGET_")
                // Outer cache controls and machine-wide Cargo front-door exports
                // are outer-process implementation details, not fixture overrides
                // for nested Soldr. Individual tests can set an intended
                // SOLDR_REAL_* value after this helper; the two cache-disable
                // flags must never bypass the SESSION route under test.
                || name.starts_with("SOLDR_REAL_")
                || matches!(name, "ZCCACHE_DISABLE" | soldr_cli::cache_lib::CACHE_ENABLED_ENV_VAR)
        });
        if should_scrub {
            command.env_remove(name);
        }
    }
    command
}

pub(crate) fn rustup_which(tool: &str) -> String {
    let output = isolated_soldr_command()
        .args(["rustup", "which", tool])
        .output()
        .expect("failed to resolve tool through soldr rustup");
    assert!(
        output.status.success(),
        "soldr rustup which failed for {tool}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub(crate) fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let counter = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let process_id = std::process::id();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{process_id}-{counter}-{nanos}"));
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

mod gc_fixtures;
pub(crate) use gc_fixtures::{
    seed_gc_candidate, seed_gc_file_candidate, seed_gc_worktree_candidate,
};

pub(crate) fn toml_string(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

pub(crate) fn path_display_variants(path: &Path) -> Vec<String> {
    let mut variants = vec![path.display().to_string()];
    if let Ok(canonical) = fs::canonicalize(path) {
        let canonical = canonical.display().to_string();
        if !variants.contains(&canonical) {
            variants.push(canonical);
        }
    }
    variants
}

pub(crate) fn discovered_private_zccache_cache_dir(cache_root: &Path) -> PathBuf {
    let private_root = cache_root.join("cache").join("zccache").join("private");
    let mut dirs: Vec<PathBuf> = fs::read_dir(&private_root)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", private_root.display()))
        .map(|entry| entry.expect("read private zccache dir").path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    assert_eq!(
        dirs.len(),
        1,
        "expected one private zccache cache dir under {}, found {:?}",
        private_root.display(),
        dirs
    );
    let namespace_dir = dirs.remove(0);
    let mut version_dirs: Vec<PathBuf> = fs::read_dir(&namespace_dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", namespace_dir.display()))
        .map(|entry| entry.expect("read private zccache version dir").path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with('v'))
        })
        .collect();
    version_dirs.sort();
    if version_dirs.len() == 1 {
        return version_dirs.remove(0);
    }
    namespace_dir
}

/// Find the embedded store owned by this fixture's broker daemon route.
pub(crate) fn discovered_embedded_zccache_store(cache_root: &Path) -> PathBuf {
    let daemon_state = cache_root.join("cache/zccache/daemon-state");
    let mut routes: Vec<PathBuf> = fs::read_dir(&daemon_state)
        .unwrap_or_else(|error| panic!("read {}: {error}", daemon_state.display()))
        .map(|entry| entry.expect("read daemon route").path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("embedded-v1-soldr-daemon-"))
        })
        .collect();
    routes.sort();
    assert_eq!(routes.len(), 1, "expected one embedded route: {routes:?}");
    routes
        .remove(0)
        .join(zccache::core::config::versioned_subdir())
}

pub(crate) fn logged_cargo_wrapper(log: &str) -> Option<String> {
    log.lines().find_map(|line| {
        let wrapper = line.strip_prefix("cargo wrapper=")?;
        wrapper
            .split_once(" rustc=")
            .map(|(wrapper, _)| wrapper.to_string())
    })
}

pub(crate) fn log_contains_owned_soldr_wrapper(log: &str, cache_root: &Path) -> bool {
    if log.contains(env!("CARGO_BIN_EXE_soldr")) {
        return true;
    }

    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        let Some(wrapper) = logged_cargo_wrapper(log) else {
            return false;
        };
        let runtime_root = cache_root.join("runtime").join("soldr-self");
        path_display_variants(&runtime_root)
            .iter()
            .any(|path| wrapper.contains(path))
    } else {
        let _ = cache_root;
        false
    }
}

pub(crate) fn log_contains_toolchain_homes(
    log: &str,
    prefix: &str,
    cargo_home: &Path,
    rustup_home: &Path,
) -> bool {
    for cargo_home in path_display_variants(cargo_home) {
        for rustup_home in path_display_variants(rustup_home) {
            if log.contains(&format!(
                "{prefix} cargo_home={cargo_home} rustup_home={rustup_home}"
            )) {
                return true;
            }
        }
    }
    false
}

pub(crate) fn fake_script_path(dir: &Path, name: &str) -> PathBuf {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        dir.join(format!("{name}.cmd"))
    } else {
        dir.join(name)
    }
}

pub(crate) fn write_fake_script(path: &Path, body: &str) {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        fs::write(path, body.replace('\n', "\r\n")).expect("failed to write fake script");
    } else {
        fs::write(path, body).expect("failed to write fake script");
        soldr_platform::fs::permissions::make_executable(path)
            .expect("failed to chmod fake script");
    }
}

pub(crate) fn fake_rustc_output_dir(log_path: &Path) -> PathBuf {
    let output_dir = log_path
        .parent()
        .expect("fake tool log should have a parent")
        .join("rustc-output");
    fs::create_dir_all(&output_dir).expect("failed to create fake rustc output directory");
    output_dir
}

pub(crate) fn fake_cargo_script(log_path: &Path) -> String {
    let output_dir = fake_rustc_output_dir(log_path);
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             if \"%~1\"==\"metadata\" (\n\
               if defined SOLDR_TEST_CARGO_METADATA_PATH (\n\
                 type \"%SOLDR_TEST_CARGO_METADATA_PATH%\"\n\
               ) else (\n\
                 echo cargo wrapper=%RUSTC_WRAPPER% rustc=%RUSTC% cache=%SOLDR_CACHE_ENABLED% session=%ZCCACHE_SESSION_ID% sccache_dir=%SCCACHE_DIR% zccache_dir=%ZCCACHE_CACHE_DIR% path_remap=%ZCCACHE_PATH_REMAP% worktree_root=%ZCCACHE_WORKTREE_ROOT% zccache_mode=%ZCCACHE_MODE%>>\"{0}\"\n\
                 for /f \"tokens=1,* delims==\" %%A in ('set CARGO_TARGET_ 2^>nul') do @echo cargo_target_env %%A=%%B>>\"{0}\"\n\
                 for /f \"tokens=1,* delims==\" %%A in ('set CARGO_PROFILE_ 2^>nul') do @echo cargo_profile_env %%A=%%B>>\"{0}\"\n\
                 echo {{}}\n\
               )\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"--version\" (\n\
               echo cargo version>>\"{0}\"\n\
               echo cargo 1.0.0-test\n\
               exit /b 0\n\
             )\n\
             echo cargo wrapper=%RUSTC_WRAPPER% rustc=%RUSTC% cache=%SOLDR_CACHE_ENABLED% session=%ZCCACHE_SESSION_ID% sccache_dir=%SCCACHE_DIR% zccache_dir=%ZCCACHE_CACHE_DIR% path_remap=%ZCCACHE_PATH_REMAP% worktree_root=%ZCCACHE_WORKTREE_ROOT% zccache_mode=%ZCCACHE_MODE%>>\"{0}\"\n\
             for /f \"tokens=1,* delims==\" %%A in ('set CARGO_TARGET_ 2^>nul') do @echo cargo_target_env %%A=%%B>>\"{0}\"\n\
             for /f \"tokens=1,* delims==\" %%A in ('set CARGO_PROFILE_ 2^>nul') do @echo cargo_profile_env %%A=%%B>>\"{0}\"\n\
             if defined RUSTC_WRAPPER (\n\
             call \"%RUSTC_WRAPPER%\" \"%RUSTC%\" --crate-name demo --emit dep-info,link -o \"{1}\\demo\" --out-dir \"{1}\"\n\
             ) else (\n\
             call \"%RUSTC%\" --crate-name demo --emit dep-info,link -o \"{1}\\demo\" --out-dir \"{1}\"\n\
             )\n\
             exit /b %ERRORLEVEL%\n",
            log_path.display(),
            output_dir.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             log_cargo_target_envs() {{\n\
               env | grep '^CARGO_TARGET_' | while IFS= read -r line; do\n\
                 echo \"cargo_target_env $line\" >> \"{0}\"\n\
               done\n\
             }}\n\
             log_cargo_profile_envs() {{\n\
               env | grep '^CARGO_PROFILE_' | while IFS= read -r line; do\n\
                 echo \"cargo_profile_env $line\" >> \"{0}\"\n\
               done\n\
             }}\n\
             if [ \"$1\" = \"metadata\" ]; then\n\
               if [ -n \"${{SOLDR_TEST_CARGO_METADATA_PATH:-}}\" ]; then\n\
                 cat \"$SOLDR_TEST_CARGO_METADATA_PATH\"\n\
               else\n\
                 echo \"cargo wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} cache=${{SOLDR_CACHE_ENABLED:-}} session=${{ZCCACHE_SESSION_ID:-}} sccache_dir=${{SCCACHE_DIR:-}} zccache_dir=${{ZCCACHE_CACHE_DIR:-}} path_remap=${{ZCCACHE_PATH_REMAP:-}} worktree_root=${{ZCCACHE_WORKTREE_ROOT:-}} zccache_mode=${{ZCCACHE_MODE:-}}\" >> \"{0}\"\n\
                 log_cargo_target_envs\n\
                 log_cargo_profile_envs\n\
                 echo '{{}}'\n\
               fi\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"--version\" ]; then\n\
               echo 'cargo version' >> \"{0}\"\n\
               echo 'cargo 1.0.0-test'\n\
               exit 0\n\
             fi\n\
             echo \"cargo wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} cache=${{SOLDR_CACHE_ENABLED:-}} session=${{ZCCACHE_SESSION_ID:-}} sccache_dir=${{SCCACHE_DIR:-}} zccache_dir=${{ZCCACHE_CACHE_DIR:-}} path_remap=${{ZCCACHE_PATH_REMAP:-}} worktree_root=${{ZCCACHE_WORKTREE_ROOT:-}} zccache_mode=${{ZCCACHE_MODE:-}}\" >> \"{0}\"\n\
             log_cargo_target_envs\n\
             log_cargo_profile_envs\n\
             if [ -n \"${{RUSTC_WRAPPER:-}}\" ]; then\n\
               \"$RUSTC_WRAPPER\" \"$RUSTC\" --crate-name demo --emit dep-info,link -o \"{1}/demo\" --out-dir \"{1}\"\n\
             else\n\
               \"$RUSTC\" --crate-name demo --emit dep-info,link -o \"{1}/demo\" --out-dir \"{1}\"\n\
             fi\n",
            log_path.display(),
            output_dir.display()
        )
    }
}

pub(crate) fn fake_cargo_with_jobserver_script(log_path: &Path) -> String {
    let output_dir = fake_rustc_output_dir(log_path);
    format!(
        "#!/bin/sh\n\
         echo \"cargo wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} cache=${{SOLDR_CACHE_ENABLED:-}} session=${{ZCCACHE_SESSION_ID:-}} zccache_dir=${{ZCCACHE_CACHE_DIR:-}}\" >> \"{}\"\n\
         exec 3</dev/null\n\
         exec 4>/dev/null\n\
         export CARGO_MAKEFLAGS='-j --jobserver-fds=3,4'\n\
         export SOLDR_TEST_JOBSERVER_READ_FD=3\n\
         export SOLDR_TEST_JOBSERVER_WRITE_FD=4\n\
         \"$RUSTC_WRAPPER\" \"$RUSTC\" --crate-name demo --emit dep-info,link -o \"{1}/demo\" --out-dir \"{1}\"\n",
        log_path.display(),
        output_dir.display()
    )
}

pub(crate) fn fake_rustc_script(log_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             setlocal EnableDelayedExpansion\n\
             if \"%~1\"==\"-Vv\" (\n\
                echo rustc 1.0.0-test\n\
                echo host: x86_64-pc-windows-msvc\n\
                echo release: 1.0.0-test\n\
                exit /b 0\n\
             )\n\
             echo rustc %*>>\"{}\"\n\
             set \"all_args=%*\"\n\
             call :materialize_outputs %*\n\
             exit /b 0\n\
             :materialize_outputs\n\
             if \"%~1\"==\"\" exit /b 0\n\
             if \"%~1\"==\"--crate-name\" (\n\
               set \"crate_name=%~2\"\n\
               shift\n\
             )\n\
             if \"%~1\"==\"-o\" (\n\
               call :materialize_one \"%~2\"\n\
               if not \"!all_args:dep-info=!\"==\"!all_args!\" (\n\
                 for %%D in (\"%~2\") do call :materialize_one \"%%~dpD!crate_name!.d\"\n\
               )\n\
               shift\n\
             )\n\
             shift\n\
             goto materialize_outputs\n\
             :materialize_one\n\
             for %%D in (\"%~1\") do if not exist \"%%~dpD\" mkdir \"%%~dpD\"\n\
             type nul > \"%~1\"\n\
             exit /b 0\n",
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"-Vv\" ]; then\n\
               echo 'rustc 1.0.0-test'\n\
               echo 'host: x86_64-unknown-linux-gnu'\n\
               echo 'release: 1.0.0-test'\n\
               exit 0\n\
             fi\n\
             echo \"rustc $*\" >> \"{}\"\n\
             all_args=$*\n\
             output_path=\n\
             output_dir=\n\
             crate_name=unknown\n\
             while [ \"$#\" -gt 0 ]; do\n\
               case \"$1\" in\n\
                 -o) shift; output_path=$1 ;;\n\
                 --out-dir) shift; output_dir=$1 ;;\n\
                 --crate-name) shift; crate_name=$1 ;;\n\
               esac\n\
               shift\n\
             done\n\
             if [ -n \"$output_path\" ]; then\n\
               mkdir -p \"$(dirname \"$output_path\")\"\n\
               : > \"$output_path\"\n\
               case \"$all_args\" in\n\
                 *dep-info*) : > \"${{output_dir:-$(dirname \"$output_path\")}}/$crate_name.d\" ;;\n\
               esac\n\
             fi\n",
            log_path.display()
        )
    }
}

pub(crate) fn fake_version_tool_script(log_path: &Path, tool_name: &str) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             echo {0} cargo_home=%CARGO_HOME% rustup_home=%RUSTUP_HOME% args=%*>>\"{1}\"\n\
             if defined SOLDR_TEST_TOOL_HANG goto hang\n\
             if defined SOLDR_TEST_TOOL_EXIT_CODE exit /b %SOLDR_TEST_TOOL_EXIT_CODE%\n\
             echo {0} 1.0.0 (fake)\n\
             exit /b 0\n\
             :hang\n\
             goto hang\n",
            tool_name,
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             echo \"{0} cargo_home=${{CARGO_HOME:-}} rustup_home=${{RUSTUP_HOME:-}} args=$*\" >> \"{1}\"\n\
             if [ -n \"${{SOLDR_TEST_TOOL_HANG:-}}\" ]; then while :; do :; done; fi\n\
             if [ -n \"${{SOLDR_TEST_TOOL_EXIT_CODE:-}}\" ]; then exit \"$SOLDR_TEST_TOOL_EXIT_CODE\"; fi\n\
             echo \"{0} 1.0.0 (fake)\"\n",
            tool_name,
            log_path.display()
        )
    }
}

pub(crate) fn fake_cargo_fmt_script(log_path: &Path, source_path: &Path, rustfmt: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             set \"fmt=%RUSTFMT%\"\n\
             if not defined RUSTFMT set \"fmt={2}\"\n\
             echo cargo fmt rustfmt=%fmt% env_rustfmt=%RUSTFMT% cache=%SOLDR_CACHE_ENABLED%>>\"{0}\"\n\
             if \"%~1\"==\"fmt\" (\n\
               call \"%fmt%\" \"{1}\"\n\
               exit /b %ERRORLEVEL%\n\
             )\n\
             echo unsupported fake cargo fmt invocation %* 1>&2\n\
             exit /b 1\n",
            log_path.display(),
            source_path.display(),
            rustfmt.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             fmt=\"${{RUSTFMT:-{2}}}\"\n\
             echo \"cargo fmt rustfmt=$fmt env_rustfmt=${{RUSTFMT:-}} cache=${{SOLDR_CACHE_ENABLED:-}}\" >> \"{0}\"\n\
             if [ \"$1\" = \"fmt\" ]; then\n\
               \"$fmt\" \"{1}\"\n\
               exit $?\n\
             fi\n\
             echo \"unsupported fake cargo fmt invocation: $*\" >&2\n\
             exit 1\n",
            log_path.display(),
            source_path.display(),
            rustfmt.display()
        )
    }
}

pub(crate) fn fake_rustup_script(log_path: &Path, tool_dir: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             echo rustup %* cargo_home=%CARGO_HOME% rustup_home=%RUSTUP_HOME%>>\"{0}\"\n\
             if \"%~1\"==\"which\" (\n\
               if \"%~2\"==\"cargo\" (\n\
                 echo {1}\n\
                 exit /b 0\n\
               )\n\
               if \"%~2\"==\"rustc\" (\n\
                 echo {2}\n\
                 exit /b 0\n\
               )\n\
               if \"%~2\"==\"rustfmt\" (\n\
                 echo {3}\n\
                 exit /b 0\n\
               )\n\
               if \"%~2\"==\"rustdoc\" (\n\
                 echo {4}\n\
                 exit /b 0\n\
               )\n\
             )\n\
             echo unsupported rustup invocation %* 1>&2\n\
             exit /b 1\n",
            log_path.display(),
            tool_dir.join("cargo.cmd").display(),
            tool_dir.join("rustc.cmd").display(),
            tool_dir.join("rustfmt.cmd").display(),
            tool_dir.join("rustdoc.cmd").display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             echo \"rustup $* cargo_home=${{CARGO_HOME:-}} rustup_home=${{RUSTUP_HOME:-}}\" >> \"{0}\"\n\
             if [ \"$1\" = \"which\" ]; then\n\
               case \"$2\" in\n\
                 cargo)\n\
                   echo \"{1}\"\n\
                   exit 0\n\
                   ;;\n\
                 rustc)\n\
                   echo \"{2}\"\n\
                   exit 0\n\
                   ;;\n\
                 rustfmt)\n\
                   echo \"{3}\"\n\
                   exit 0\n\
                   ;;\n\
                 rustdoc)\n\
                   echo \"{4}\"\n\
                   exit 0\n\
                   ;;\n\
               esac\n\
             fi\n\
             echo \"unsupported rustup invocation: $*\" >&2\n\
             exit 1\n",
            log_path.display(),
            tool_dir.join("cargo").display(),
            tool_dir.join("rustc").display(),
            tool_dir.join("rustfmt").display(),
            tool_dir.join("rustdoc").display()
        )
    }
}

pub(crate) fn fake_failing_rustup_script(log_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             echo rustup %* cargo_home=%CARGO_HOME% rustup_home=%RUSTUP_HOME%>>\"{}\"\n\
             echo rustup should not have been invoked 1>&2\n\
             exit /b 1\n",
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             echo \"rustup $* cargo_home=${{CARGO_HOME:-}} rustup_home=${{RUSTUP_HOME:-}}\" >> \"{}\"\n\
             echo \"rustup should not have been invoked\" >&2\n\
             exit 1\n",
            log_path.display()
        )
    }
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) fn fake_zccache_script(log_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             if \"%~1\"==\"start\" goto soldr_zccache_start\n\
             if \"%~1\"==\"stop\" (\n\
               echo zccache stop cache_dir=%ZCCACHE_CACHE_DIR% daemon_namespace=%ZCCACHE_DAEMON_NAMESPACE%>>\"{0}\"\n\
               if defined SOLDR_TEST_ZCCACHE_STALE_START_ONCE type nul > \"%SOLDR_TEST_ZCCACHE_STALE_START_ONCE%.stopped\"\n\
               if defined SOLDR_TEST_ZCCACHE_SESSION_START_LOST_ONCE type nul > \"%SOLDR_TEST_ZCCACHE_SESSION_START_LOST_ONCE%.stopped\"\n\
               if defined SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER type nul > \"%SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER%\"\n\
               exit /b 0\n\
             )\n\
              if \"%~1\"==\"session-start\" (\n\
                echo zccache session-start cache_dir=%ZCCACHE_CACHE_DIR% daemon_namespace=%ZCCACHE_DAEMON_NAMESPACE% args=%*>>\"{0}\"\n\
                if defined SOLDR_TEST_ZCCACHE_SESSION_START_LOST_ONCE (\n\
                  if not exist \"%SOLDR_TEST_ZCCACHE_SESSION_START_LOST_ONCE%.stopped\" (\n\
                    if not exist \"%SOLDR_TEST_ZCCACHE_SESSION_START_LOST_ONCE%.failed\" goto soldr_zccache_session_start_lost_first\n\
                    goto soldr_zccache_session_start_lost_retry\n\
                  )\n\
                )\n\
                if not \"%~4\"==\"\" type nul > \"%~4\"\n\
                if not \"%~6\"==\"\" type nul > \"%~6\"\n\
                echo {{\"session_id\":\"test-session\"}}\n\
                exit /b 0\n\
             )\n\
             if \"%~1\"==\"session-end\" (\n\
               echo zccache session-end %~2 %~3 cache_dir=%ZCCACHE_CACHE_DIR% daemon_namespace=%ZCCACHE_DAEMON_NAMESPACE%>>\"{0}\"\n\
               if \"%~3\"==\"--json\" (\n\
                 echo {{\"status\":\"ok\",\"session_id\":\"test-session\",\"duration_ms\":1200,\"compilations\":10,\"hits\":7,\"misses\":3,\"non_cacheable\":2,\"errors\":1,\"time_saved_ms\":900,\"unique_sources\":4,\"bytes_read\":111,\"bytes_written\":222,\"hit_rate\":0.7}}\n\
               ) else (\n\
                 echo hits: 1\n\
               )\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"rust-plan\" (\n\
               echo zccache rust-plan %~2 cache_dir=%ZCCACHE_CACHE_DIR% args=%*>>\"{0}\"\n\
               if /I \"%~2\"==\"restore\" if defined SOLDR_TEST_RUST_PLAN_STALE (\n\
                 echo {{\"operation\":\"restore\",\"restored_file_count\":0,\"artifact_absent_from_restored_plan\":1,\"compatibility\":{{\"status\":\"ok\",\"errors\":[]}}}}\n\
                 exit /b 0\n\
               )\n\
               echo {{\"operation\":\"%~2\",\"compatibility\":{{\"status\":\"ok\",\"errors\":[]}}}}\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"status\" (\n\
               if defined SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER (\n\
                 if exist \"%SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER%\" (\n\
                   echo daemon not running 1>&2\n\
                   exit /b 1\n\
                 )\n\
               )\n\
               echo hits=7\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"flush\" goto soldr_zccache_flush\n\
             if \"%~1\"==\"clear\" (\n\
               echo zccache clear cache_dir=%ZCCACHE_CACHE_DIR%>>\"{0}\"\n\
               exit /b 0\n\
             )\n\
             set \"rustc=%~1\"\n\
             shift\n\
             echo zccache wrapper cache_dir=%ZCCACHE_CACHE_DIR% daemon_namespace=%ZCCACHE_DAEMON_NAMESPACE% %rustc% %*>>\"{0}\"\n\
             call \"%rustc%\" %*\n\
             exit /b %ERRORLEVEL%\n\
             :soldr_zccache_start\n\
             echo zccache start cache_dir=%ZCCACHE_CACHE_DIR% daemon_namespace=%ZCCACHE_DAEMON_NAMESPACE%>>\"{0}\"\n\
             if defined SOLDR_TEST_ZCCACHE_STALE_LOCK_ONCE goto soldr_zccache_start_stale_lock\n\
             if not defined SOLDR_TEST_ZCCACHE_STALE_START_ONCE exit /b 0\n\
             if exist \"%SOLDR_TEST_ZCCACHE_STALE_START_ONCE%.stopped\" exit /b 0\n\
             if not exist \"%SOLDR_TEST_ZCCACHE_STALE_START_ONCE%.failed\" (\n\
               type nul > \"%SOLDR_TEST_ZCCACHE_STALE_START_ONCE%.failed\"\n\
               echo failed to start daemon: daemon process 3197 exists but not accepting connections 1>&2\n\
               exit /b 1\n\
             )\n\
             echo zccache start retried before stop 1>&2\n\
             exit /b 66\n\
             :soldr_zccache_start_stale_lock\n\
             if not defined ZCCACHE_CACHE_DIR (\n\
               echo zccache start missing ZCCACHE_CACHE_DIR 1>&2\n\
               exit /b 67\n\
             )\n\
             if not exist \"%SOLDR_TEST_ZCCACHE_STALE_LOCK_ONCE%.failed\" (\n\
               if not exist \"%ZCCACHE_CACHE_DIR%\" mkdir \"%ZCCACHE_CACHE_DIR%\"\n\
               echo 3197>\"%ZCCACHE_CACHE_DIR%\\daemon.lock\"\n\
               type nul > \"%SOLDR_TEST_ZCCACHE_STALE_LOCK_ONCE%.failed\"\n\
               echo failed to start daemon: daemon started but not accepting connections after 10s 1>&2\n\
               exit /b 1\n\
             )\n\
             if exist \"%ZCCACHE_CACHE_DIR%\\daemon.lock\" (\n\
               echo zccache start retried while stale daemon.lock remained 1>&2\n\
               exit /b 66\n\
             )\n\
             exit /b 0\n\
             :soldr_zccache_flush\n\
             echo zccache flush args=%* cache_dir=%ZCCACHE_CACHE_DIR%>>\"{0}\"\n\
             if defined SOLDR_TEST_ZCCACHE_FLUSH_UNSUPPORTED goto soldr_zccache_flush_unsupported\n\
             if not \"%~2\"==\"--json\" goto soldr_zccache_flush_plain\n\
             if defined SOLDR_TEST_ZCCACHE_FLUSH_NO_JSON goto soldr_zccache_flush_no_json\n\
             echo {{\"status\":\"ok\",\"bytes_written\":4096,\"duration_ms\":12}}\n\
             exit /b 0\n\
             :soldr_zccache_flush_plain\n\
             echo flushed\n\
             exit /b 0\n\
             :soldr_zccache_flush_unsupported\n\
             echo error: unrecognized subcommand 'flush' 1>&2\n\
             exit /b 2\n\
             :soldr_zccache_flush_no_json\n\
             echo error: unexpected argument '--json' found 1>&2\n\
             exit /b 2\n\
             :soldr_zccache_session_start_lost_first\n\
             type nul > \"%SOLDR_TEST_ZCCACHE_SESSION_START_LOST_ONCE%.failed\"\n\
             echo zccache lost connection to daemon no response 1>&2\n\
             exit /b 1\n\
             :soldr_zccache_session_start_lost_retry\n\
             echo zccache session-start retried before stop 1>&2\n\
             exit /b 66\n",
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
               start)\n\
                 echo \"zccache start cache_dir=${{ZCCACHE_CACHE_DIR:-}} daemon_namespace=${{ZCCACHE_DAEMON_NAMESPACE:-}}\" >> \"{0}\"\n\
                 if [ -n \"${{SOLDR_TEST_ZCCACHE_STALE_LOCK_ONCE:-}}\" ]; then\n\
                   if [ -z \"${{ZCCACHE_CACHE_DIR:-}}\" ]; then\n\
                     echo 'zccache start missing ZCCACHE_CACHE_DIR' >&2\n\
                     exit 67\n\
                   fi\n\
                   lock_path=\"${{ZCCACHE_CACHE_DIR}}/daemon.lock\"\n\
                   if [ ! -e \"${{SOLDR_TEST_ZCCACHE_STALE_LOCK_ONCE}}.failed\" ]; then\n\
                     mkdir -p \"${{ZCCACHE_CACHE_DIR}}\"\n\
                     printf '3197\\n' > \"$lock_path\"\n\
                     : > \"${{SOLDR_TEST_ZCCACHE_STALE_LOCK_ONCE}}.failed\"\n\
                     echo 'failed to start daemon: daemon started but not accepting connections after 10s' >&2\n\
                     exit 1\n\
                   fi\n\
                   if [ -e \"$lock_path\" ]; then\n\
                     echo 'zccache start retried while stale daemon.lock remained' >&2\n\
                     exit 66\n\
                   fi\n\
                 fi\n\
                 if [ -n \"${{SOLDR_TEST_ZCCACHE_STALE_START_ONCE:-}}\" ]; then\n\
                   if [ ! -e \"${{SOLDR_TEST_ZCCACHE_STALE_START_ONCE}}.stopped\" ]; then\n\
                     if [ ! -e \"${{SOLDR_TEST_ZCCACHE_STALE_START_ONCE}}.failed\" ]; then\n\
                       : > \"${{SOLDR_TEST_ZCCACHE_STALE_START_ONCE}}.failed\"\n\
                       echo 'failed to start daemon: daemon process 3197 exists but not accepting connections' >&2\n\
                       exit 1\n\
                     fi\n\
                     echo 'zccache start retried before stop' >&2\n\
                     exit 66\n\
                   fi\n\
                 fi\n\
                 exit 0\n\
                 ;;\n\
               stop)\n\
                 echo \"zccache stop cache_dir=${{ZCCACHE_CACHE_DIR:-}} daemon_namespace=${{ZCCACHE_DAEMON_NAMESPACE:-}}\" >> \"{0}\"\n\
                 if [ -n \"${{SOLDR_TEST_ZCCACHE_STALE_START_ONCE:-}}\" ]; then\n\
                   : > \"${{SOLDR_TEST_ZCCACHE_STALE_START_ONCE}}.stopped\"\n\
                 fi\n\
                 if [ -n \"${{SOLDR_TEST_ZCCACHE_SESSION_START_LOST_ONCE:-}}\" ]; then\n\
                   : > \"${{SOLDR_TEST_ZCCACHE_SESSION_START_LOST_ONCE}}.stopped\"\n\
                 fi\n\
                 if [ -n \"${{SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER:-}}\" ]; then\n\
                   : > \"${{SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER}}\"\n\
                 fi\n\
                 exit 0\n\
                 ;;\n\
                session-start)\n\
                  echo \"zccache session-start cache_dir=${{ZCCACHE_CACHE_DIR:-}} daemon_namespace=${{ZCCACHE_DAEMON_NAMESPACE:-}} args=$*\" >> \"{0}\"\n\
                  if [ -n \"${{SOLDR_TEST_ZCCACHE_SESSION_START_LOST_ONCE:-}}\" ] && [ ! -e \"${{SOLDR_TEST_ZCCACHE_SESSION_START_LOST_ONCE}}.stopped\" ]; then\n\
                    if [ ! -e \"${{SOLDR_TEST_ZCCACHE_SESSION_START_LOST_ONCE}}.failed\" ]; then\n\
                      : > \"${{SOLDR_TEST_ZCCACHE_SESSION_START_LOST_ONCE}}.failed\"\n\
                      echo 'zccache[err][R]: lost connection to daemon (no response)' >&2\n\
                      exit 1\n\
                    fi\n\
                    echo 'zccache session-start retried before stop' >&2\n\
                    exit 66\n\
                  fi\n\
                  : > \"$4\"\n\
                  : > \"$6\"\n\
                  echo '{{\"session_id\":\"test-session\"}}'\n\
                  exit 0\n\
               ;;\n\
               session-end)\n\
                 echo \"zccache session-end $2 $3 cache_dir=${{ZCCACHE_CACHE_DIR:-}} daemon_namespace=${{ZCCACHE_DAEMON_NAMESPACE:-}}\" >> \"{0}\"\n\
                 if [ \"${{3:-}}\" = \"--json\" ]; then\n\
                   printf '{{\"status\":\"ok\",\"session_id\":\"test-session\",\"duration_ms\":1200,\"compilations\":10,\"hits\":7,\"misses\":3,\"non_cacheable\":2,\"errors\":1,\"time_saved_ms\":900,\"unique_sources\":4,\"bytes_read\":111,\"bytes_written\":222,\"hit_rate\":0.7}}\\n'\n\
                 else\n\
                   echo 'hits: 1'\n\
                 fi\n\
                 exit 0\n\
                 ;;\n\
              rust-plan)\n\
                echo \"zccache rust-plan $2 cache_dir=${{ZCCACHE_CACHE_DIR:-}} args=$*\" >> \"{0}\"\n\
                if [ \"$2\" = \"restore\" ] && [ -n \"${{SOLDR_TEST_RUST_PLAN_STALE:-}}\" ]; then\n\
                  printf '{{\"operation\":\"restore\",\"restored_file_count\":0,\"artifact_absent_from_restored_plan\":1,\"compatibility\":{{\"status\":\"ok\",\"errors\":[]}}}}\\n'\n\
                  exit 0\n\
                fi\n\
                printf '{{\"operation\":\"%s\",\"compatibility\":{{\"status\":\"ok\",\"errors\":[]}}}}\\n' \"$2\"\n\
                exit 0\n\
                ;;\n\
              status)\n\
                if [ -n \"${{SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER:-}}\" ] && [ -e \"${{SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER}}\" ]; then\n\
                  echo 'daemon not running' >&2\n\
                  exit 1\n\
                fi\n\
                echo 'hits=7'\n\
                exit 0\n\
                ;;\n\
              flush)\n\
                echo \"zccache flush args=$* cache_dir=${{ZCCACHE_CACHE_DIR:-}}\" >> \"{0}\"\n\
                if [ -n \"${{SOLDR_TEST_ZCCACHE_FLUSH_UNSUPPORTED:-}}\" ]; then\n\
                  echo \"error: unrecognized subcommand 'flush'\" >&2\n\
                  exit 2\n\
                fi\n\
                if [ \"${{2:-}}\" = \"--json\" ]; then\n\
                  if [ -n \"${{SOLDR_TEST_ZCCACHE_FLUSH_NO_JSON:-}}\" ]; then\n\
                    echo \"error: unexpected argument '--json' found\" >&2\n\
                    exit 2\n\
                  fi\n\
                  printf '{{\"status\":\"ok\",\"bytes_written\":4096,\"duration_ms\":12}}\\n'\n\
                else\n\
                  echo 'flushed'\n\
                fi\n\
                exit 0\n\
                ;;\n\
               clear)\n\
                 echo \"zccache clear cache_dir=${{ZCCACHE_CACHE_DIR:-}}\" >> \"{0}\"\n\
                 exit 0\n\
                 ;;\n\
             esac\n\
             rustc=\"$1\"\n\
             shift\n\
             if [ -n \"${{SOLDR_TEST_JOBSERVER_READ_FD:-}}\" ]; then\n\
               if ! eval \": <&$SOLDR_TEST_JOBSERVER_READ_FD\"; then\n\
                 echo \"jobserver read fd $SOLDR_TEST_JOBSERVER_READ_FD is not open\" >&2\n\
                 exit 42\n\
               fi\n\
               if ! eval \": >&$SOLDR_TEST_JOBSERVER_WRITE_FD\"; then\n\
                 echo \"jobserver write fd $SOLDR_TEST_JOBSERVER_WRITE_FD is not open\" >&2\n\
                 exit 42\n\
               fi\n\
               echo \"zccache jobserver fds ok read=$SOLDR_TEST_JOBSERVER_READ_FD write=$SOLDR_TEST_JOBSERVER_WRITE_FD\" >> \"{0}\"\n\
             fi\n\
             echo \"zccache wrapper cache_dir=${{ZCCACHE_CACHE_DIR:-}} daemon_namespace=${{ZCCACHE_DAEMON_NAMESPACE:-}} $rustc $*\" >> \"{0}\"\n\
             \"$rustc\" \"$@\"\n",
            log_path.display()
        )
    }
}

pub(crate) fn fake_custom_wrapper_script(log_path: &Path, wrapper_name: &str) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             set \"rustc=%~1\"\n\
             shift\n\
             echo {1} wrapper %rustc% %*>>\"{0}\"\n\
             call \"%rustc%\" %*\n\
             exit /b %ERRORLEVEL%\n",
            log_path.display(),
            wrapper_name
        )
    } else {
        format!(
            "#!/bin/sh\n\
             rustc=\"$1\"\n\
             shift\n\
             echo \"{1} wrapper $rustc $*\" >> \"{0}\"\n\
             \"$rustc\" \"$@\"\n",
            log_path.display(),
            wrapper_name
        )
    }
}

pub(crate) fn install_fake_toolchain(log_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let dir = unique_temp_dir("fake-toolchain");
    let cargo = fake_script_path(&dir, "cargo");
    let rustc = fake_script_path(&dir, "rustc");
    let zccache = fake_script_path(&dir, "zccache");
    write_fake_script(&cargo, &fake_cargo_script(log_path));
    write_fake_script(&rustc, &fake_rustc_script(log_path));
    write_fake_script(&zccache, &fake_zccache_script(log_path));
    (cargo, rustc, zccache)
}

fn fake_clang_script(log_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             echo clang %*>>\"{}\"\n\
             echo clang version 0.0.0 (fake)\n\
             exit /b 0\n",
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             echo \"clang $*\" >> \"{}\"\n\
             echo 'clang version 0.0.0 (fake)'\n\
             exit 0\n",
            log_path.display()
        )
    }
}

pub(crate) fn install_fake_clippy_toolchain(
    log_path: &Path,
) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = unique_temp_dir("fake-clippy-toolchain");
    let cargo = fake_script_path(&dir, "cargo");
    let rustc = fake_script_path(&dir, "rustc");
    let zccache = fake_script_path(&dir, "zccache");
    let clippy_driver = fake_script_path(&dir, "clippy-driver");
    write_fake_script(&cargo, &fake_cargo_clippy_script(log_path, &clippy_driver));
    write_fake_script(&rustc, &fake_rustc_script(log_path));
    write_fake_script(&zccache, &fake_zccache_script(log_path));
    write_fake_script(&clippy_driver, &fake_clippy_driver_script(log_path));
    (cargo, rustc, zccache, clippy_driver)
}

pub(crate) fn install_fake_jobserver_toolchain(log_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let dir = unique_temp_dir("fake-jobserver-toolchain");
    let cargo = fake_script_path(&dir, "cargo");
    let rustc = fake_script_path(&dir, "rustc");
    let zccache = fake_script_path(&dir, "zccache");

    write_fake_script(&cargo, &fake_cargo_with_jobserver_script(log_path));
    write_fake_script(&rustc, &fake_rustc_script(log_path));
    write_fake_script(&zccache, &fake_zccache_script(log_path));
    (cargo, rustc, zccache)
}

pub(crate) fn install_fake_version_toolchain(
    tool_dir: &Path,
    log_path: &Path,
) -> (PathBuf, PathBuf, PathBuf) {
    let cargo = fake_script_path(tool_dir, "cargo");
    let rustc = fake_script_path(tool_dir, "rustc");
    let rustfmt = fake_script_path(tool_dir, "rustfmt");
    write_fake_script(&cargo, &fake_version_tool_script(log_path, "cargo"));
    write_fake_script(&rustc, &fake_version_tool_script(log_path, "rustc"));
    write_fake_script(&rustfmt, &fake_version_tool_script(log_path, "rustfmt"));
    (cargo, rustc, rustfmt)
}

pub(crate) fn install_fake_cargo_fmt_toolchain(
    log_path: &Path,
    source_path: &Path,
) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    let (rustup, cargo, rustc, rustfmt) = install_fake_rustup_toolchain(log_path);
    let tool_dir = cargo
        .parent()
        .expect("fake cargo should live in a tool dir")
        .to_path_buf();
    let zccache = fake_script_path(&tool_dir, "zccache");
    write_fake_script(
        &cargo,
        &fake_cargo_fmt_script(log_path, source_path, &rustfmt),
    );
    write_fake_script(&zccache, &fake_zccache_script(log_path));
    (rustup, cargo, rustc, rustfmt, zccache)
}

pub(crate) fn write_rustfmt_source(cache_root: &Path) -> PathBuf {
    let src_dir = cache_root.join("src");
    fs::create_dir_all(&src_dir).expect("failed to create rustfmt source dir");
    let source_path = src_dir.join("lib.rs");
    fs::write(&source_path, "fn main( ) {}\n").expect("failed to write rustfmt source");
    source_path
}

pub(crate) fn install_fake_wrapper(log_path: &Path, wrapper_name: &str) -> PathBuf {
    let dir = unique_temp_dir("fake-wrapper");
    let wrapper = fake_script_path(&dir, wrapper_name);
    write_fake_script(
        &wrapper,
        &fake_custom_wrapper_script(log_path, wrapper_name),
    );
    wrapper
}

pub(crate) fn install_fake_rustup_toolchain(
    log_path: &Path,
) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = unique_temp_dir("fake-rustup-toolchain");
    let cargo = fake_script_path(&dir, "cargo");
    let rustc = fake_script_path(&dir, "rustc");
    let rustfmt = fake_script_path(&dir, "rustfmt");
    let rustdoc = fake_script_path(&dir, "rustdoc");
    let rustup = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        dir.join("rustup.bat")
    } else {
        fake_script_path(&dir, "rustup")
    };
    write_fake_script(&cargo, &fake_version_tool_script(log_path, "cargo"));
    write_fake_script(&rustc, &fake_version_tool_script(log_path, "rustc"));
    write_fake_script(&rustfmt, &fake_version_tool_script(log_path, "rustfmt"));
    write_fake_script(&rustdoc, &fake_version_tool_script(log_path, "rustdoc"));
    write_fake_script(&rustup, &fake_rustup_script(log_path, &dir));
    (rustup, cargo, rustc, rustfmt)
}

pub(crate) fn install_failing_fake_rustup(log_path: &Path) -> PathBuf {
    let dir = unique_temp_dir("fake-rustup-failure");
    let rustup = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        dir.join("rustup.bat")
    } else {
        fake_script_path(&dir, "rustup")
    };
    write_fake_script(&rustup, &fake_failing_rustup_script(log_path));
    rustup
}

/// Fake rustup that logs one line per invocation to `log_path`, with the
/// argv joined by `\u{1f}` (ASCII unit separator) so test assertions can
/// reason about the exact argv even when individual arguments contain
/// spaces. Always exits 0.
pub(crate) fn fake_logging_rustup_script(log_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             setlocal enabledelayedexpansion\n\
             set \"line=\"\n\
             :loop\n\
             if \"%~1\"==\"\" goto done\n\
             if defined line (set \"line=!line!\u{1f}%~1\") else (set \"line=%~1\")\n\
             shift\n\
             goto loop\n\
             :done\n\
             echo !line!>>\"{}\"\n\
             exit /b 0\n",
            log_path.display()
        )
    } else {
        // Use ASCII Unit Separator (\037) between argv elements so assertions
        // can split deterministically even if an individual arg contains
        // spaces. Hand-roll the join in /bin/sh.
        format!(
            "#!/bin/sh\n\
             sep=$(printf '\\037')\n\
             out=\"\"\n\
             first=1\n\
             for arg in \"$@\"; do\n\
               if [ $first -eq 1 ]; then\n\
                 out=\"$arg\"\n\
                 first=0\n\
               else\n\
                 out=\"$out${{sep}}$arg\"\n\
               fi\n\
             done\n\
             printf '%s\\n' \"$out\" >> \"{}\"\n\
             exit 0\n",
            log_path.display()
        )
    }
}

/// Install a fake rustup that logs argv per invocation. Returns the path
/// to the fake binary, ready to hand to `SOLDR_TEST_RUSTUP_BIN`.
pub(crate) fn install_logging_fake_rustup(log_path: &Path) -> PathBuf {
    let dir = unique_temp_dir("fake-rustup-logging");
    let rustup = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        dir.join("rustup.bat")
    } else {
        fake_script_path(&dir, "rustup")
    };
    write_fake_script(&rustup, &fake_logging_rustup_script(log_path));
    rustup
}

/// Read every argv invocation logged by `fake_logging_rustup_script`.
/// Each returned `Vec<String>` is one invocation, with argv split on the
/// ASCII unit separator.
pub(crate) fn read_logged_rustup_invocations(log_path: &Path) -> Vec<Vec<String>> {
    let text = fs::read_to_string(log_path).unwrap_or_default();
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.split('\u{1f}').map(str::to_string).collect())
        .collect()
}

pub(crate) fn seed_rust_toolchain_toml(dir: &Path, contents: &str) {
    fs::create_dir_all(dir).expect("failed to create workspace dir");
    fs::write(dir.join("rust-toolchain.toml"), contents)
        .expect("failed to write rust-toolchain.toml");
}

/// Seed a fake rustup toolchain directory that satisfies the soldr#2977
/// readiness contract, so production code classifies it `Ready`.
///
/// The contract is shared by every caller
/// (`toolchain_readiness::classify_toolchain_dir`): a toolchain directory is
/// reusable only when it carries **both** `lib/rustlib/multirust-channel-manifest.toml`
/// and a *host-native* `bin/rustc` (`rustc.exe` on Windows). The cargo front
/// door's preparation memo additionally fingerprints `lib/rustlib/components`.
/// A fixture that omits any of those classifies `Partial`, and the front door
/// then refuses to fingerprint the directory and records no memo at all — so
/// keep the layout here in one place rather than open-coding it per test.
pub(crate) fn seed_fake_toolchain_dir(
    toolchain_dir: &Path,
    rustc_contents: &[u8],
    components_contents: &[u8],
) -> PathBuf {
    let bin = toolchain_dir.join("bin");
    let rustlib = toolchain_dir.join("lib").join("rustlib");
    fs::create_dir_all(&bin).expect("create fake toolchain bin");
    fs::create_dir_all(&rustlib).expect("create fake toolchain rustlib");
    fs::write(bin.join("rustc"), rustc_contents).expect("seed fake rustc identity");
    // `classify_toolchain_dir` looks for the host-native filename. `EXE_SUFFIX`
    // is ".exe" on Windows and empty elsewhere, which keeps this helper inside
    // the soldr#2493 platform-cfg boundary without a cfg.
    let suffix = std::env::consts::EXE_SUFFIX;
    if !suffix.is_empty() {
        fs::write(bin.join(format!("rustc{suffix}")), rustc_contents)
            .expect("seed fake host-native rustc identity");
    }
    fs::write(rustlib.join("components"), components_contents)
        .expect("seed fake component identity");
    fs::write(
        rustlib.join("multirust-channel-manifest.toml"),
        b"manifest-version = '2'\n",
    )
    .expect("seed fake channel manifest");
    // soldr#3376: readiness reads the standard-library files, not just the
    // `components` claim, so a fake toolchain needs them for the host and for
    // the targets these fixtures declare.
    let host = soldr_cli::core::TargetTriple::host()
        .expect("detect test host triple")
        .triple();
    for (triple, with_std) in [
        (host.as_str(), true),
        ("wasm32-unknown-unknown", false),
        ("x86_64-unknown-linux-musl", false),
    ] {
        let lib = rustlib.join(triple).join("lib");
        fs::create_dir_all(&lib).expect("create fake target lib dir");
        fs::write(lib.join("libcore-0123456789abcdef.rlib"), b"core").expect("seed fake libcore");
        if with_std {
            fs::write(lib.join("libstd-0123456789abcdef.rlib"), b"std").expect("seed fake libstd");
        }
    }
    toolchain_dir.to_path_buf()
}

/// Fake cargo that logs one line per invocation to `log_path`, with the
/// argv joined by `\u{1f}` (ASCII unit separator) so test assertions can
/// reason about the exact argv even when individual arguments contain
/// spaces. Always exits 0. Mirrors `fake_logging_rustup_script` but
/// writes to a separate log file so concurrent rustup + cargo
/// invocations under `toolchain prepare` don't interleave.
pub(crate) fn fake_logging_cargo_script(log_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        // One file per invocation, claimed via `mkdir` — exclusive **only
        // under a parent that already exists**, which is why
        // [`install_logging_fake_cargo`] pre-creates the slot root; see the
        // soldr#2589 note there. NOT `>>` appends to a shared file: cmd's
        // append is
        // seek-then-write, so concurrent children (e.g. `lint deps` running
        // deny/audit/machete in parallel) could clobber each other's lines
        // — the target-run x86_64-msvc lane lost the `audit` line exactly
        // that way. POSIX `>>` is O_APPEND and unaffected. Slot names carry
        // a sortable centisecond timestamp so sequential consumers (the
        // toolchain tests assert invocation order) still read back in
        // order; process-spawn overhead dwarfs centisecond ties.
        //
        // soldr#2589: claiming the slot is not enough — the write into it can
        // still be lost. The 2026-08-17 recurrence had all three `lint deps`
        // legs exit 0 (so the writer's existence guard passed) with one line
        // absent at read time and a teed `The process cannot access the file
        // because it is being used by another process.` beside it: the
        // redirect created `line.txt` and lost its content, and a 0-byte file
        // satisfies `if not exist`. So verify the *size*, and retry the write
        // into the SAME slot (`echo >` truncates, so a retry is idempotent).
        // Re-claiming a fresh slot instead would strand the abandoned one —
        // `rmdir` fails while the handle that caused the violation is open,
        // and the reader now treats a blank slot as a hard error.
        format!(
            "@echo off\n\
             setlocal enabledelayedexpansion\n\
             set \"line=\"\n\
             :loop\n\
             if \"%~1\"==\"\" goto done\n\
             if defined line (set \"line=!line!\u{1f}%~1\") else (set \"line=%~1\")\n\
             shift\n\
             goto loop\n\
             :done\n\
             set \"stamp=!TIME: =0!\"\n\
             set \"stamp=!stamp::=!\"\n\
             set \"stamp=!stamp:.=!\"\n\
             set \"stamp=!stamp:,=!\"\n\
             :mkslot\n\
             set \"slot={0}.d\\!stamp!_!RANDOM!_!RANDOM!\"\n\
             mkdir \"!slot!\" 2>nul || goto mkslot\n\
             set \"tries=0\"\n\
             :writeline\n\
             set /a tries+=1\n\
             echo !line!>\"!slot!\\line.txt\"\n\
             set \"size=\"\n\
             for %%A in (\"!slot!\\line.txt\") do set \"size=%%~zA\"\n\
             if not defined size set \"size=0\"\n\
             if !size! GTR 0 exit /b 0\n\
             if !tries! GEQ 10 exit /b 97\n\
             if !tries! GEQ 2 ping -n 2 127.0.0.1 >nul\n\
             goto writeline\n",
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             sep=$(printf '\\037')\n\
             out=\"\"\n\
             first=1\n\
             for arg in \"$@\"; do\n\
               if [ $first -eq 1 ]; then\n\
                 out=\"$arg\"\n\
                 first=0\n\
               else\n\
                 out=\"$out${{sep}}$arg\"\n\
               fi\n\
             done\n\
             printf '%s\\n' \"$out\" >> \"{}\"\n\
             exit 0\n",
            log_path.display()
        )
    }
}

/// Install a fake cargo that logs argv per invocation. Returns the path
/// to the fake binary, ready to hand to `SOLDR_TEST_CARGO_BIN`.
pub(crate) fn install_logging_fake_cargo(log_path: &Path) -> PathBuf {
    // soldr#2589: create the slot root before any child can run.
    //
    // The Windows script claims its slot with `mkdir "<root>\<slot>"`, and
    // that is a reliable exclusive claim only when `<root>` already exists.
    // When two children race to create the *parent* as well, cmd's `md` can
    // report success to **both** — reproduced directly: 32 concurrent writers
    // logged `writer0` and `writer1` claiming the identical slot path on
    // their first attempt, followed by one sharing violation and one lost
    // line. (A single-component `mkdir` under an existing parent is exclusive;
    // 64 concurrent processes racing one such path yield exactly one winner.)
    //
    // That is the whole flake: `lint deps` starts deny/audit/machete
    // simultaneously against a fresh log root, so the first two to reach
    // `mkdir` are exactly the pair that can collide — which is why the lost
    // line was always one of the first tools to start.
    let slot_root = PathBuf::from(format!("{}.d", log_path.display()));
    fs::create_dir_all(&slot_root).expect("failed to create fake-cargo slot root");
    let dir = unique_temp_dir("fake-cargo-logging");
    let cargo = fake_script_path(&dir, "cargo");
    write_fake_script(&cargo, &fake_logging_cargo_script(log_path));
    cargo
}

/// Fake `cargo` that responds to `--version` with a deterministic string
/// (matching the real cargo `cargo X.Y.Z (sha date)` format used by
/// `toolchain ensure --json`'s smoke verify) AND otherwise logs argv to
/// `log_path`. Useful for the `ensure` test which needs both a valid
/// `--version` capture AND to assert `cargo install` argv for plugins.
///
/// `version` must not contain literal `(` / `)` on Windows because cmd.exe
/// `echo` treats those as block delimiters — we substitute them through the
/// caret-escape `^(` / `^)` form on Windows automatically.
pub(crate) fn fake_logging_versioned_cargo_script(log_path: &Path, version: &str) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        let escaped = escape_for_cmd_echo(version);
        format!(
            "@echo off\n\
             if \"%~1\"==\"--version\" (\n\
               echo {1}\n\
               exit /b 0\n\
             )\n\
             setlocal enabledelayedexpansion\n\
             set \"line=\"\n\
             :loop\n\
             if \"%~1\"==\"\" goto done\n\
             if defined line (set \"line=!line!\u{1f}%~1\") else (set \"line=%~1\")\n\
             shift\n\
             goto loop\n\
             :done\n\
             echo !line!>>\"{0}\"\n\
             exit /b 0\n",
            log_path.display(),
            escaped
        )
    } else {
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then\n\
               echo '{1}'\n\
               exit 0\n\
             fi\n\
             sep=$(printf '\\037')\n\
             out=\"\"\n\
             first=1\n\
             for arg in \"$@\"; do\n\
               if [ $first -eq 1 ]; then\n\
                 out=\"$arg\"\n\
                 first=0\n\
               else\n\
                 out=\"$out${{sep}}$arg\"\n\
               fi\n\
             done\n\
             printf '%s\\n' \"$out\" >> \"{0}\"\n\
             exit 0\n",
            log_path.display(),
            version
        )
    }
}

/// Escape `(` and `)` for inclusion in a cmd.exe `echo` line inside an
/// `if (...) ( ... )` block (where the un-escaped parens close the block
/// prematurely). The standard escape is `^(` / `^)`.
fn escape_for_cmd_echo(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        match ch {
            '(' => out.push_str("^("),
            ')' => out.push_str("^)"),
            other => out.push(other),
        }
    }
    out
}

/// Install a fake `cargo` that responds to `--version` AND logs argv on
/// every other invocation. Returns the path to the fake binary.
pub(crate) fn install_logging_versioned_fake_cargo(log_path: &Path, version: &str) -> PathBuf {
    let dir = unique_temp_dir("fake-cargo-versioned");
    let cargo = fake_script_path(&dir, "cargo");
    write_fake_script(
        &cargo,
        &fake_logging_versioned_cargo_script(log_path, version),
    );
    cargo
}

/// Fake `rustc` that responds to `--version` (deterministic) and
/// otherwise exits 0. Used by the ensure JSON smoke-verify test.
pub(crate) fn fake_versioned_rustc_script(version: &str) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        let escaped = escape_for_cmd_echo(version);
        format!(
            "@echo off\n\
             if \"%~1\"==\"--version\" (\n\
               echo {0}\n\
               exit /b 0\n\
             )\n\
             exit /b 0\n",
            escaped
        )
    } else {
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then\n\
               echo '{0}'\n\
               exit 0\n\
             fi\n\
             exit 0\n",
            version
        )
    }
}

/// Install a fake `rustc` that responds to `--version` deterministically.
/// Suitable for `SOLDR_TEST_RUSTC_BIN`.
pub(crate) fn install_versioned_fake_rustc(version: &str) -> PathBuf {
    let dir = unique_temp_dir("fake-rustc-versioned");
    let rustc = fake_script_path(&dir, "rustc");
    write_fake_script(&rustc, &fake_versioned_rustc_script(version));
    rustc
}

/// Fake `rustc` that always exits non-zero on `--version`. Used to
/// exercise the `smoke_verify.ok == false` branch.
pub(crate) fn fake_failing_rustc_script() -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "@echo off\n\
         echo simulated rustc failure 1>&2\n\
         exit /b 1\n"
            .to_string()
    } else {
        "#!/bin/sh\n\
         echo 'simulated rustc failure' >&2\n\
         exit 1\n"
            .to_string()
    }
}

/// Install a fake `rustc` that fails any `--version` call.
pub(crate) fn install_failing_fake_rustc() -> PathBuf {
    let dir = unique_temp_dir("fake-rustc-failing");
    let rustc = fake_script_path(&dir, "rustc");
    write_fake_script(&rustc, &fake_failing_rustc_script());
    rustc
}

/// Read every argv invocation logged by `fake_logging_cargo_script`.
/// Each returned `Vec<String>` is one invocation, with argv split on
/// the ASCII unit separator.
pub(crate) fn read_logged_cargo_invocations(log_path: &Path) -> Vec<Vec<String>> {
    // Two sources, merged in order: the shared append log (Unix scripts and
    // the versioned-cargo script) first, then the Windows per-invocation
    // slot directory sorted by its timestamped slot names — see
    // `fake_logging_cargo_script` for why Windows cannot share one file.
    let mut lines: Vec<String> = fs::read_to_string(log_path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let slot_root = PathBuf::from(format!("{}.d", log_path.display()));
    if let Ok(entries) = fs::read_dir(&slot_root) {
        let mut slots: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        slots.sort();
        for slot in slots {
            match fs::read_to_string(slot.join("line.txt")) {
                // soldr#2589: a claimed slot whose line is blank contributes
                // nothing once the terminal `.filter(!trim().is_empty())`
                // below runs — which is exactly how the 2026-08-17 machete
                // line vanished while every leg still exited 0. An empty slot
                // is writer-loss, not an empty invocation; say so.
                Ok(text) if text.trim().is_empty() => panic!(
                    "claimed fake-cargo slot {} has a blank line.txt \
                     (soldr#2589 -- the writer claimed the slot but its \
                     content never landed)",
                    slot.display()
                ),
                Ok(text) => lines.extend(text.lines().map(str::to_string)),
                // soldr#2589: a slot dir was claimed (mkdir succeeded) but
                // its line never became readable. Silently skipping is how
                // invocations vanished from two Windows lanes post-#2562;
                // fail loudly so the next occurrence localizes itself.
                Err(error) => panic!(
                    "claimed fake-cargo slot {} has no readable line.txt: {error}                      (soldr#2589 -- the writer lost the line after claiming)",
                    slot.display()
                ),
            }
        }
    }
    lines
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.split('\u{1f}').map(str::to_string).collect())
        .collect()
}

pub(crate) fn prepend_to_path(dir: &Path) -> std::ffi::OsString {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(std::env::split_paths(&existing));
    std::env::join_paths(paths).expect("failed to join PATH")
}

/// PATH value for tests that need to verify soldr's tool resolution falls back
/// to its rustup path. Strips the runner's real cargo/rustc entries so
/// `probe_toolchain_binary`'s PATH search can't shadow the in-test fakes.
/// On Windows we keep `System32` so `Command::new` can still spawn `.cmd`
/// shims via `cmd.exe`.
pub(crate) fn isolated_test_path() -> std::ffi::OsString {
    // soldr#3578: the front door's linker shim resolves `clang` from PATH only.
    // These tests pin PATH to the system dirs precisely so nothing ambient can
    // satisfy a tool lookup -- but that left the driver with no clang at all on
    // any host whose `/usr/bin` has none (the managed fallback wants a
    // `CanonicalV2` catalogue entry and errors when it cannot fetch one), so
    // every fixture that reached the linker died on the shim instead of testing
    // its subject. Prepend a hermetic stub carrying only `clang`: the pinning
    // intent is preserved, and the driver resolves without a real LLVM.
    let fake_llvm = isolated_fake_llvm_dir();
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        let system_root = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
        let dirs = [fake_llvm, system_root.join("System32"), system_root];
        std::env::join_paths(dirs).expect("failed to join isolated PATH")
    } else {
        std::env::join_paths([
            fake_llvm,
            isolated_posix_utils_dir(),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ])
        .expect("failed to join isolated PATH")
    }
}

/// The POSIX utilities fixture scripts call by name (`exec sleep`, `dirname`,
/// `mkdir -p`, ...).
const FIXTURE_POSIX_UTILS: &[&str] = &[
    "basename", "cat", "chmod", "cp", "cut", "date", "dirname", "env", "false", "grep", "head",
    "kill", "ln", "ls", "mkdir", "mktemp", "mv", "printf", "readlink", "realpath", "rm", "sed",
    "sleep", "sort", "tail", "tee", "touch", "tr", "true", "uname", "wc",
];

/// One process-wide directory of forwarders for [`FIXTURE_POSIX_UTILS`], each
/// re-executing the utility the caller's `PATH` resolves.
///
/// `isolated_test_path` pins PATH to the system directories so no ambient
/// tool can satisfy a lookup, which assumed the utilities live in `/usr/bin`
/// or `/bin`. They do not everywhere: on NixOS both are envfs mounts that
/// resolve a name through the *calling process's* PATH, so under the pinned
/// PATH a fixture found no `sleep`, `dirname` or `mkdir` at all. Forwarding
/// only these names keeps the pin's intent -- no ambient compiler, Cargo or
/// soldr becomes reachable -- while the fixtures get their shell utilities.
/// The forwarder execs the PATH entry, not its canonical target, so a
/// multicall binary (coreutils, busybox) still sees the utility's name.
fn isolated_posix_utils_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = unique_temp_dir("isolated-posix-utils");
        let ambient = std::env::var_os("PATH").unwrap_or_default();
        for name in FIXTURE_POSIX_UTILS {
            let Some(found) = std::env::split_paths(&ambient)
                .map(|entry| entry.join(name))
                .find(|candidate| candidate.is_file())
            else {
                continue;
            };
            write_fake_script(
                &fake_script_path(&dir, name),
                &format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", found.display()),
            );
        }
        dir
    })
    .clone()
}

/// One process-wide fake LLVM bin directory holding just a `clang` stub.
///
/// `OnceLock` so the stub is created once per test binary rather than once per
/// `isolated_test_path()` call, and so every test in a process shares the same
/// directory (which keeps the path stable in failure output).
fn isolated_fake_llvm_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = unique_temp_dir("isolated-fake-llvm");
        let log = dir.join("clang-invocations.log");
        write_fake_script(&fake_script_path(&dir, "clang"), &fake_clang_script(&log));
        dir
    })
    .clone()
}

/// How long to keep retrying a spawn that answers `ETXTBSY`.
///
/// Generous relative to the window it covers -- the fork/exec gap is
/// microseconds -- because the cost of being wrong is asymmetric: a few wasted
/// milliseconds against an intermittent failure of the whole Linux suite.
const ETXTBSY_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
const ETXTBSY_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

/// Retry `attempt` while it reports the executable as busy.
///
/// Matched through `ErrorKind::ExecutableFileBusy` rather than a raw errno and
/// a `#[cfg(unix)]` pair. Two reasons, and the second is the load-bearing one:
/// the errno is `ETXTBSY` on Linux but `26` means something unrelated on
/// Windows, and a host `#[cfg]` outside `crates/soldr-platform` is a
/// boundary-ratchet violation (`platform_cfg_boundary_ratchet.py`). The kind
/// is portable, so there is nothing to gate -- on a platform that never
/// produces it, this is a plain passthrough.
fn retry_while_text_file_busy<T>(
    mut attempt: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let deadline = std::time::Instant::now() + ETXTBSY_RETRY_BUDGET;
    loop {
        match attempt() {
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(ETXTBSY_RETRY_INTERVAL);
            }
            other => return other,
        }
    }
}

/// Spawn a command whose program was just materialized by this test run.
///
/// soldr#2854: staging an executable and immediately spawning it intermittently
/// fails on Linux with
///
/// ```text
/// Os { code: 26, kind: ExecutableFileBusy, message: "Text file busy" }
/// ```
///
/// The window is Rust's fork -> exec gap. Between the two the child holds a
/// copy of every descriptor the parent had -- `CLOEXEC` only takes effect *at*
/// exec -- so while one thread is still inside `fs::copy` with a write
/// descriptor open, another thread's forked child transiently holds that
/// descriptor too, and the kernel refuses to exec a file anybody can write.
///
/// ## Why this retries instead of staging more carefully
///
/// The tidier fix is to materialize to a sibling and rename into place, which
/// this repo already does in `materialize_runtime_alias` and `isolated_daemon`.
/// It was measured and **it does not close this window** -- rename republishes
/// the *same inode* the write descriptor refers to. Under a stress harness of
/// 40 rounds x 16 threads on the Linux runner:
///
/// ```text
/// plain copy (before)          ETXTBSY = 97
/// copy aside + atomic rename   ETXTBSY = 66
/// bounded retry                ETXTBSY =  0
/// ```
///
/// Recorded so the better-reading fix is not proposed again on its looks.
///
/// Only the busy condition is retried. Every other spawn failure returns
/// immediately and unchanged, so a missing binary still fails fast with its own
/// diagnostic.
pub fn spawn_staged(command: &mut Command) -> std::io::Result<std::process::Child> {
    retry_while_text_file_busy(|| command.spawn())
}

/// `output()` form of [`spawn_staged`], for fixtures that run the staged image
/// to completion instead of holding a handle.
pub fn output_staged(command: &mut Command) -> std::io::Result<std::process::Output> {
    retry_while_text_file_busy(|| command.output())
}
