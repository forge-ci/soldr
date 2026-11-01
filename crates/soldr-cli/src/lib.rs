//! The soldr library crate — the single compilation of the whole
//! module tree (#1490 Phase 1).
//!
//! Before Phase 1 the bin tree (`main.rs`) and this lib tree each
//! declared the same ~40K LOC of modules, so every build compiled
//! them twice. Now every module is declared exactly once, here;
//! `src/main.rs` is a thin shim that calls [`run`], and the CLI entry
//! logic itself lives in [`soldr_main`], glob-re-exported at the crate
//! root so historical `crate::<item>` paths keep resolving.
//!
//! Integration tests under `tests/` keep their
//! `use soldr_cli::core::*` / `soldr_cli::fetch::*` /
//! `soldr_cli::cache_lib::*` imports unchanged.

// Cross-target `--lib` checks intentionally leave host-only CLI paths unused.
// The retired external-cache lifecycle (soldr#2900) is guarded by a source
// lint under `tests/guards/`, so it cannot hide behind this allowance.
#![allow(dead_code, unused_imports)]

/// Global allocator for the whole `soldr` multicall binary (soldr#3038).
///
/// soldr previously declared no `#[global_allocator]`, so every surface —
/// including the long-lived `soldr-daemon` — ran on the Rust default
/// (glibc's ptmalloc on Linux). A canonical daemon on production hardware
/// reached 11.7 GiB of private anonymous memory, growing in step with
/// compile volume and never giving any of it back; 116 anonymous mappings
/// pinned at exactly 64 MiB (glibc's per-thread `HEAP_MAX_SIZE` across the
/// daemon's 37 threads) were consistent with either live data or allocator
/// arena retention, and there was no instrumentation to tell which.
///
/// mimalloc is far more aggressive about returning freed pages to the OS
/// than a per-thread ptmalloc arena, and — unlike the previous default —
/// exposes exact allocator counters (`mimalloc_pprof::prof::stats()`,
/// notably `heap.committed`/`heap.detailed`) that let a future
/// investigation tell "the daemon is holding live data" from "the allocator
/// is holding freed pages" instead of guessing from `/proc/<pid>/maps`.
///
/// Declared exactly once, here in the facade crate that both `src/main.rs`
/// (the `soldr` `[[bin]]`) and every multicall alias link — including
/// `soldr-daemon`, reached via `argv[0]` dispatch in
/// [`daemon_entry`] — so one declaration covers every surface. A binary may
/// have at most one `#[global_allocator]`; a second declaration anywhere
/// else in the dependency graph would be a compile error, which is the
/// enforcement that keeps this the only one.
///
/// This wires in the allocator and its always-on exact counters
/// unconditionally (see `Cargo.toml`'s `zccache` entry for why the
/// `mimalloc-allocator` feature — which pulls `mimalloc-pprof` with its
/// default `pprof`/`MI_PPROF=1` — stays on). It does **not** start the
/// crate's *sampled* heap profiler: nothing here calls
/// `mimalloc_pprof::prof::start`, so that stays opt-in at runtime, off by
/// default.
#[global_allocator]
static GLOBAL_ALLOCATOR: ::zccache::mimalloc_pprof::MiMalloc = ::zccache::mimalloc_pprof::MiMalloc;

/// Neutral host-platform facade (#2493): the single selection site lives
/// in `soldr-platform`; this crate calls only `crate::platform::…`.
pub(crate) use soldr_platform as platform;

/// Process-wide barrier for unit tests that mutate environment variables.
///
/// Rust runs a crate's unit tests in one process, so module-local mutexes do
/// not prevent two different modules from racing on values such as `SDKROOT`.
///
/// soldr#1663: this is now the *only* env barrier in the crate. Modules that
/// used to declare their own `static ENV_LOCK: Mutex<()>` alias this instead
/// (`use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;`), because two mutexes
/// guarding the same variable provide no mutual exclusion at all — which is
/// exactly what happened with `SOLDR_USE_LEGACY_XWIN`, mutated from
/// `blessed_build` under this lock and from `main_tests` under a private one.
/// `env_lock_lint.rs` fails the build if a private barrier reappears.
/// soldr#1994: the barrier itself now lives in `soldr-core` so upstream
/// crates can share it. Re-exported here under the original name, so every
/// existing `crate::TEST_PROCESS_ENV_LOCK` and
/// `use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;` is unchanged.
#[cfg(test)]
pub(crate) use soldr_core::test_util::TEST_PROCESS_ENV_LOCK;

/// RAII guard that sets or removes an environment variable for the duration
/// of a test and restores the previous value on drop.
///
/// soldr#1663 wants restoration to be panic-safe: a test that snapshots a
/// variable, mutates it, and restores inline leaves the process environment
/// rewritten for every later test in the binary if it panics in between.
/// `Drop` runs during unwinding, so this cannot leak that way.
///
/// Take [`TEST_PROCESS_ENV_LOCK`] for the guard's whole lifetime — this type
/// makes restoration safe, not the mutation atomic.
#[cfg(test)]
pub(crate) struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl EnvVarGuard {
    pub(crate) fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }

    pub(crate) fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, previous }
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

/// RAII guard that changes the process working directory for the duration of
/// a test and restores it on drop.
///
/// The working directory is process-global state exactly like an environment
/// variable, and unit tests share one process — but [`TEST_PROCESS_ENV_LOCK`]
/// and `env_lock_lint.rs` only cover env vars, so cwd had neither a barrier
/// nor a panic-safe guard.
///
/// That gap blocked the 0.8.26 release. `env_cmd::build_env_block` passes
/// `std::env::current_dir()` as the workspace root, so
/// `env_block_does_not_guess_pyo3_no_python` failed on macOS the moment a
/// parallel test happened to be chdir'd into a manifest declaring a PyO3
/// abi3 extension — the resolver read *that* workspace and emitted
/// `PYO3_NO_PYTHON`.
///
/// Same contract as [`EnvVarGuard`]: hold [`TEST_PROCESS_ENV_LOCK`] for the
/// guard's whole lifetime. This type makes restoration safe, not the mutation
/// atomic. It deliberately does **not** take the lock itself — callers such as
/// `prepare_cmd`'s `rustup_add_target_scopes_to_pinned_toolchain_channel`
/// already hold it, and `crate::platform::sync::Mutex` is not reentrant, so acquiring it
/// here would deadlock rather than protect anything.
#[cfg(test)]
pub(crate) struct CwdGuard {
    previous: std::path::PathBuf,
}

#[cfg(test)]
impl CwdGuard {
    pub(crate) fn enter(path: &std::path::Path) -> Self {
        let previous = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(path).expect("chdir");
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for CwdGuard {
    fn drop(&mut self) {
        // Best-effort: a failure here cannot be reported from Drop, and
        // panicking during unwind would abort the whole test process.
        let _ = std::env::set_current_dir(&self.previous);
    }
}

#[cfg(test)]
mod cwd_guard_tests {
    use super::CwdGuard;

    #[test]
    fn cwd_guard_restores_on_normal_exit() {
        let _env = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = std::env::current_dir().expect("cwd");
        let tmp = tempfile::tempdir().expect("tempdir");
        {
            let _cwd = CwdGuard::enter(tmp.path());
            assert_ne!(std::env::current_dir().expect("cwd"), before);
        }
        assert_eq!(std::env::current_dir().expect("cwd"), before);
    }

    // The property the old inline `set_current_dir(prev)` in `archive_cmd`
    // did not have: a panic between chdir and restore left the process cwd
    // pointing at a temp dir for every later test in the binary.
    #[test]
    fn cwd_guard_restores_while_unwinding() {
        let _env = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = std::env::current_dir().expect("cwd");
        let tmp = tempfile::tempdir().expect("tempdir");

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _cwd = CwdGuard::enter(tmp.path());
            panic!("deliberate panic inside the guarded scope");
        }));

        assert!(panicked.is_err(), "the guarded scope must have panicked");
        assert_eq!(
            std::env::current_dir().expect("cwd"),
            before,
            "Drop runs during unwinding, so the cwd must be restored even              when the guarded body panics"
        );
    }
}

pub mod archive_cmd;
pub mod binaries;
/// soldr#1012 PR 5 — blessed cross-compile sysroot prep called from
/// `Commands::Build` for canonical target triples. Coordinates the
/// xwin-cache materialization + clang-shim install + env var setup.
pub mod blessed_build;
pub mod bootstrap;
pub(crate) mod broker_bringup;
pub mod broker_cmd;
mod broker_control_transport;
pub(crate) mod broker_daemon_cache;
/// soldr#3251: broker-owned reclamation of idle daemon routes and registrations.
pub(crate) mod broker_daemon_disk;
/// soldr#2493: broker route-acquisition deadlines and their `soldr doctor`
/// surface, split out of `broker_server` to keep it under the 1,000-line
/// production-source ceiling.
pub(crate) mod broker_deadlines;
pub(crate) mod broker_debug;
pub(crate) mod broker_endpoint_owner;
/// soldr#2388: container-safe broker/session socket identity (graceful fallback
/// when the OS provides no `/etc/machine-id`).
pub mod broker_identity;
pub(crate) mod broker_idle;
pub(crate) mod broker_image_watch;
pub(crate) mod broker_inventory;
mod broker_launcher;
pub(crate) mod broker_lease;
pub(crate) mod broker_policy;
pub(crate) mod broker_purge;
pub(crate) mod broker_reaper;
pub(crate) mod broker_retry;
pub(crate) mod broker_server;
pub mod broker_skew;
pub mod broker_spawn;
/// soldr#2900 — Soldr-owned per-build cache bookkeeping. Replaces the
/// retired private-zccache lifecycle module, which still carried subprocess
/// plumbing after the cache service moved in-process.
pub(crate) mod build_cache_session;
pub mod build_from_source_cmd;
/// soldr#1790 — always-on hierarchical per-build XML log
/// (`<soldr root>/logs/builds/<timestamp>-<cwd-slug>.xml`). See the
/// module doc for the schema and the derived-link / cpu_ms caveats.
pub mod build_log;
pub mod builtin_verbs;
pub mod cache;
pub(crate) mod cache_health;
pub mod cargo_diagnostics;
pub mod cargo_front_door;
pub mod cargo_metadata_soldr;
pub(crate) mod cargo_toolchain_shim;
mod cc_clang_cross;
pub mod cc_cmd;
/// soldr#2867 — frozen host-validation plan and executor.
pub(crate) mod ci_test;
pub mod cli_args;
pub mod cli_dispatch;
/// The canonical stderr/stdout color rule (soldr#3437): one predicate and one
/// `paint`/ANSI helper for every surface that colorizes output.
pub(crate) mod color_choice;
pub mod compile_diagnostics;
pub mod compile_dispatch;
pub mod compile_fallback_rollup;
/// No-progress watchdog for `soldr cook`'s cargo-chef phases (CI silence
/// follow-up to soldr#3043).
pub(crate) mod compile_stall_watch;
/// soldr#1081 — Shared compile dispatch logic used by both
/// the soldr-as-RUSTC_WRAPPER hot path (`wrapper.rs`) and multicall
/// `zccache-soldr` dispatch. Owns the hang-safe retry budget contract.
pub mod config_cmd;
pub mod cook;
/// soldr#3117 -- keeps the daemon route alive for the whole of `soldr cook`.
pub(crate) mod cook_route_hold;
/// soldr#3518 -- durable journal making the in-place cook skeleton crash-safe.
pub(crate) mod cook_source_journal;
pub mod cook_source_snapshot;
/// Blessed target prep for `soldr cook --target`, matching `soldr build`.
pub(crate) mod cook_target_prep;
pub(crate) mod cook_watchdog;
pub(crate) mod cook_watchdog_dump;
pub mod daemon_entry;
/// soldr#2360 — actionable attribution for a daemon-unavailable compile
/// dispatch failure, split out of `compile_dispatch.rs` (over the #1966
/// line ceiling) into its own module.
/// soldr#2023 — `soldr daemon status` rendering, split out of
/// `soldr_main.rs` so that file could stop growing.
pub(crate) mod daemon_status_render;
mod darwin_lld_config;
pub mod docker_cross;
pub mod doctor;
pub(crate) mod doctor_cook;
mod dylint_cook;
/// soldr#3042 step 3 — which of the two Dylint target trees a
/// `soldr dylint cook` invocation is preparing (`target/dylint/target` vs.
/// `target/dylint/tests`), split out so `dylint_cook.rs` does not have to
/// grow past the line ceiling to add the second tree.
mod dylint_cook_tree;
/// soldr#2945 — the driver half of the old `dylint_toolchain.rs`: the
/// binary-or-exit gate on `dylint-driver`, its catalogue fetch, and the
/// per-host runtime environment the driver needs to load a nightly's
/// `rustc_private` libraries. Split out because the provenance work pushed
/// `dylint_toolchain.rs` past the hard 1,000-line ceiling that
/// `.github/scripts/loc_ceiling.py` enforces with no grandfathering.
pub(crate) mod dylint_driver;
/// soldr#2945 — the one reader of the nightly a workspace's Dylint lint
/// libraries declare, glob expansion included. `dylint_toolchain`,
/// `dylint_cook`, and `ci_test::plan` all resolve the Dylint channel through
/// it instead of each deriving or hard-coding its own answer.
pub(crate) mod dylint_libraries;
pub(crate) mod dylint_library_store;
pub(crate) mod dylint_library_uplift;
pub(crate) mod dylint_prepare;
/// soldr#3567/#3605 — the single Rustup home Dylint provisions and runs in.
pub(crate) mod dylint_rustup_home;
pub(crate) mod dylint_target;
pub mod dylint_toolchain;
pub(crate) mod dylint_toolchain_readiness;
/// soldr#938 — `soldr env --target` subcommand implementation.
pub mod env_cmd;
/// soldr#1059 — `soldr exec <cmd>` PATH-prepend wrapper.
pub mod exec_cmd;
/// soldr#2024 — guarantee one line when soldr exits non-zero having
/// neither reported anything nor run a child that could have.
pub(crate) mod exit_guard;
/// soldr#1817 — COW-detach zccache-delivered outputs before a direct
/// compiler runs after the daemon becomes unavailable mid-build.
pub(crate) mod fallback_detach;
/// soldr#1543 — overlap `cargo fetch` with blessed SDK preparation on
/// the `soldr build --target` surface.
pub mod fetch_overlap;
pub mod gc;
/// Project policy that can hand a local checkout invocation to a newer
/// globally-installed soldr binary.
pub mod global_upgrade;
pub(crate) mod host_pressure;
pub(crate) mod host_tool_link;
/// soldr#2310 — `soldr install <github-url|path>` prebuilt-first tool install.
pub mod install;
pub mod install_shims;
pub mod ld_cmd;
pub mod linker;
pub mod linker_project;
pub mod linker_shim;
/// soldr#2038 - extensible CI/build-surface policy engine (`soldr lint ci`).
pub mod lint_ci;
/// soldr#820 — `soldr logs` discoverable runtime-log surface.
/// soldr#1721 - cache-aware unified validation command.
pub mod lint_cmd;
/// soldr#3958 - declared `soldr lint` extras and the porcelain runner.
pub(crate) mod lint_steps;
pub(crate) mod llvm_fetch_wait;
pub mod logs_cmd;
/// soldr#3698 — `soldr logs view` / `soldr logs prune`.
pub mod logs_retention;
/// soldr#1079 — Windows MSVC host-toolchain auto-discovery. Probes
/// vswhere + the Windows SDK and synthesizes LIB/INCLUDE/PATH/LIBPATH
/// onto the current process so `soldr cargo build` / `soldr cargo test`
/// succeed from a plain PowerShell without the downstream `$env:LIB`
/// workaround. Detect-host now; managed-catalogue fallback is a
/// follow-up in the same issue.
pub mod msvc_host;
pub mod multicall;
/// soldr#2614 — musl-host prerequisite probes (libgcc unwinder + cc linker).
pub(crate) mod musl_host;
pub mod native_cc;
pub mod optimize;
pub mod optimize_detect;
pub mod optimize_windows;
pub mod prepare_cmd;
/// Precedence contract for `soldr prepare --github-env`'s exported Rust flags.
/// Separate module because `prepare_cmd` is over the LOC ratchet's ceiling.
#[cfg(test)]
mod prepare_env_contract_tests;
mod prepare_github_env;
/// Save/restore state tests split from `prepare_cmd` to keep the LOC ratchet green.
#[cfg(test)]
mod prepare_state_tests;
/// soldr#939 — PyO3 auto-detection via cargo metadata.
pub mod pyo3_detect;
pub mod reentrancy_guard;
pub mod release_sidecar;
mod replay_nextest;
pub(crate) mod route_refusal;
pub(crate) mod route_refusal_sentinel;
/// soldr#2877 — daemon-mediated resident-permit lease for `cargo fmt`
/// fan-out, plus the explicit-file source snapshot/restore guard that
/// protects against a killed rustfmt child leaving a file empty.
pub(crate) mod rustfmt_admission;
pub mod save_load;
pub mod session_transport;
pub mod shim_dir;
pub(crate) mod shim_hygiene;
pub mod shim_materialize;
/// soldr#2571 — opt-in per-phase startup breadcrumbs for the front door,
/// so a client that wedges before its first byte still names the phase.
pub mod startup_trace;
/// soldr#1838 Phase 3 -- every timeout soldr honours, resolved through
/// the real production resolvers so the report cannot drift.
pub(crate) mod system_toolchain_pin;
/// soldr#997 — friendly target aliases + Rust-triple passthrough.
/// See module doc for the `soldr build --target <alias>` UX contract.
pub mod target_alias;
pub mod target_lifecycle;
pub(crate) mod timeout_registry;
pub mod toolchain;
pub(crate) mod toolchain_dir_name;
pub mod toolchain_doctor;
pub mod toolchain_ensure;
pub mod toolchain_link;
pub(crate) mod toolchain_plugins;
pub(crate) mod toolchain_prepare;
pub mod toolchain_readiness;
pub(crate) mod toolchain_std_libs;
pub mod trampoline;
/// soldr#3894 — verdict ledger: skip a check whose exact inputs already passed.
pub mod verdict;
/// soldr#2024 — the `--as <version>` trampoline, split out of
/// `soldr_main.rs` so that file could stop growing.
pub mod version_trampoline;
/// soldr#2139 gap 1 — the `soldr wheel --target <triple>` surface. Thin,
/// abi3-only front end over the existing `soldr maturin ...` execution path.
pub mod wheel_bundle;
pub mod wheel_cmd;
pub mod wheel_stamp;
pub(crate) mod worktree_target;
pub mod wrapper;
pub mod wrapper_identity;
/// `wrapper_target` holds the wrapper hot-path target-registry routing
/// extracted out of `wrapper.rs` so integration tests under
/// `tests/cargo_front_door/cli_wrapper_perf.rs` can drive it in-process (issue #474).
pub mod wrapper_target;
pub mod zccache;
pub mod zccache_compat;

// #1490 Phase 2 facade (mechanics rule M3): every module that was
// ever a `mod` in soldr-cli stays reachable as `soldr_cli::<name>` /
// `crate::<name>` via these re-exports of the extracted soldr-core
pub use soldr_cache::cache_lib;
pub use soldr_core::{
    build_log_meta, cargo_path_check, core, defender, defender_probe, fuzzy_match, self_relocate,
    startup_profile, test_util,
};
pub use soldr_daemon::{daemon, zccache_embedded};
pub use soldr_fetch::fetch;

/// CLI entry logic — formerly the `main.rs` crate root (#1490 Phase 1).
mod soldr_main;

/// The full soldr CLI entry point, called by the `src/main.rs` shim.
pub use soldr_main::run;
/// Historical `crate::<item>` paths (consts, dispatch helpers) resolve
/// through this glob just as they did when `main.rs` was the crate
/// root.
pub(crate) use soldr_main::*;
