//! CLI entry logic — everything that used to be `main.rs`'s crate root
//! (#1490 Phase 1). The binary shim at `src/main.rs` calls [`run`]; `lib.rs`
//! glob-re-exports this module at the crate root so historical `crate::<item>` paths keep resolving.
use clap::Parser;
use std::io::Write;

use crate::{
    archive_cmd, binaries, blessed_build, bootstrap, build_from_source_cmd, cache, cache_lib,
    cargo_diagnostics, cargo_front_door, cargo_metadata_soldr, cargo_path_check, cli_args,
    cli_dispatch, compile_dispatch, cook, core, daemon, defender, defender_probe, doctor,
    dylint_cook, env_cmd, exec_cmd, exit_guard, fetch, fuzzy_match, gc, install_shims, linker,
    lint_cmd, logs_cmd, msvc_host, multicall, native_cc, optimize, optimize_detect,
    optimize_windows, prepare_cmd, pyo3_detect, release_sidecar, save_load, self_relocate,
    shim_dir, shim_materialize, startup_profile, startup_trace, target_alias, test_util, toolchain,
    toolchain_doctor, toolchain_ensure, toolchain_link, trampoline, version_trampoline, wrapper,
    wrapper_target, zccache, zccache_embedded,
};

pub(crate) use crate::cli_args::{
    is_cargo_builtin_verb, CacheSubcommand, Cli, Commands, DaemonBuildsSubcommand,
    DaemonSubcommand, DefenderExclusionsSubcommand, GcCargoArgs, GcListKind, GcSubcommand,
    GcSweepArgs, LogsSubcommand, ToolchainSubcommand, TrimProfileArg, ZccacheSourceArg,
    CARGO_BUILTIN_VERBS, SOLDR_BUILTIN_VERBS,
};

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use crate::exit_guard::guarded_exit;
use crate::fetch::VersionSpec;

#[allow(unused_imports)]
pub(crate) use crate::binaries::{
    apply_implicit_toolchain_homes, current_soldr_binary, non_empty_env_path, parse_tool_spec,
    resolve_toolchain_binary, resolve_toolchain_binary_for_channel, rustup_binary,
    rustup_resolution_failure,
};

// soldr#1368 — argv/cross-compile dispatch helpers extracted from this
// file to stay under the LOC guard. Re-exported at the crate root so
// existing bare-name call sites (and `main_tests.rs` via `use super::*`)
// keep resolving.
#[allow(unused_imports)]
pub(crate) use crate::cli_dispatch::*;

pub(crate) const TEST_CARGO_BIN_ENV_VAR: &str = "SOLDR_TEST_CARGO_BIN";
pub(crate) const TEST_RUSTC_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTC_BIN";
pub(crate) use crate::core::TEST_RUSTUP_BIN_ENV_VAR;
pub(crate) const TEST_FREE_DISK_BYTES_ENV_VAR: &str = "SOLDR_TEST_FREE_DISK_BYTES";
/// Overrides the default `nightly` toolchain used by `soldr gc cargo`
/// to invoke cargo's unstable `-Zgc clean gc`. The `--toolchain` flag
/// on `gc cargo` / `gc sweep` always takes precedence.
pub(crate) const SOLDR_GC_CARGO_TOOLCHAIN_ENV_VAR: &str = "SOLDR_GC_CARGO_TOOLCHAIN";
pub(crate) const JSON_SCHEMA_VERSION: u32 = 1;
pub(crate) const LOW_DISK_WARNING_THRESHOLD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(crate) const RUSTC_WRAPPER_OVERRIDE_ENV_VAR: &str = "SOLDR_RUSTC_WRAPPER";
pub(crate) const CARGO_PROFILE_DEV_DEBUG_ENV_VAR: &str = "CARGO_PROFILE_DEV_DEBUG";
pub(crate) const CARGO_PROFILE_TEST_DEBUG_ENV_VAR: &str = "CARGO_PROFILE_TEST_DEBUG";
/// Picks the linker injected for `soldr cargo ...` builds. See `linker` module
/// and `docs/API.md` for the supported values (`default | ld | mold | rust-lld
/// | fast`).
pub(crate) const LINKER_ENV_VAR: &str = "SOLDR_LINKER";
pub(crate) const REAL_TOOLCHAIN_BINARY_ENV_PREFIX: &str = "SOLDR_REAL_";
/// Opt in to embedding packed Darwin DWARF into linked artifacts (soldr#1775).
///
/// Turning this on makes `soldr cargo` request cargo's JSON message stream so
/// it can build an exact artifact closure, then copy each artifact's dSYM
/// sections into a `__DWARF` segment. That capture changes the command line
/// for every build-like invocation, so it stays off unless asked for.
///
/// soldr#2997: the embed used to ride on whether a target cache plan existed,
/// which meant it never ran on a default build and vanished entirely when the
/// target cache was removed. This is its own gate.
pub(crate) const EMBED_PACKED_DWARF_ENV_VAR: &str = "SOLDR_EMBED_PACKED_DWARF";
pub(crate) const TARGET_CACHE_MODE_ENV_VAR: &str = "SOLDR_TARGET_CACHE_MODE";
pub(crate) const TARGET_CACHE_BUNDLE_DIR_ENV_VAR: &str = "SOLDR_TARGET_CACHE_BUNDLE_DIR";
pub(crate) const TARGET_CACHE_BACKEND_ENV_VAR: &str = "SOLDR_TARGET_CACHE_BACKEND";
/// Selects which thin-slice pruning policy `soldr cargo` ships to zccache. See
/// `docs/THIN_TARGET_CACHE_PRUNING.md` for the rationale and rollout plan.
/// Values: `thin-v1` (legacy opt-out — keeps `.rlib`/`.rmeta`/proc-macro
/// outputs; useful when pinned to managed zccache < 1.9.1) and `thin-v2`
/// (default since soldr v0.7.31 / issue #461; fingerprint-aware aggressive
/// prune that drops library bytes and lets zccache's compilation cache
/// repopulate them).
pub(crate) const TARGET_CACHE_PROFILE_ENV_VAR: &str = "SOLDR_TARGET_CACHE_PROFILE";
/// Reader-thread count for the target-cache tar walk in zccache (issue #272).
/// Forwarded to the `zccache rust-plan save/restore` subprocess via inherited
/// environment; soldr validates the value early so typos fail before cargo
/// metadata runs. Values: `auto` (default; zccache picks a vCPU-bounded count
/// capped at 8), `1` (disable parallelism — sequential tar walk), or any
/// positive integer for an explicit thread count. The actual parallel walk
/// lives in zccache; this constant exists so soldr can reject malformed values
/// at the front door.
pub(crate) const TARGET_CACHE_TAR_THREADS_ENV_VAR: &str = "SOLDR_TARGET_CACHE_TAR_THREADS";
/// Filename of the file-list manifest written next to the thin-slice bundle so
/// downstream tooling can prove what landed in the slice without unpacking it.
pub(crate) const THIN_MANIFEST_FILENAME: &str = "manifest.v2.json";
/// Flag controlling the warm-restore short-circuit (issue #229). Default-on:
/// after a successful `rust-plan save` soldr writes a sentinel describing the
/// plan/job, and on the next `soldr cargo ...` invocation the matching
/// `rust-plan restore` is skipped if the sentinel proves the `target/` tree
/// is already in the exact state restore would produce. This preserves
/// Cargo's mtime-based fingerprints across split CI steps. Set to a falsy
/// value (`0` / `false` / `no` / `off` / empty, case-insensitive) to opt out;
/// unset or any other value keeps the short-circuit enabled.
pub(crate) const SKIP_WARM_RESTORE_ENV_VAR: &str = "SOLDR_RUST_PLAN_SKIP_WARM_RESTORE";
/// Opt-in escape hatch for advanced workflows that intentionally inject
/// soldr/zccache workspace-pinned state into `soldr cargo ...`.
pub(crate) const TRUST_INHERITED_SOLDR_ENV_VAR: &str = "SOLDR_TRUST_INHERITED_ENV";
/// Filename of the sentinel written next to the thin-slice bundle root after
/// a successful `rust-plan save`. Read on the next invocation by
/// `should_skip_warm_restore` to decide whether `rust-plan restore` would be
/// a no-op-but-touches-mtimes operation against an already-warm `target/`.
pub(crate) const WARM_RESTORE_SENTINEL_FILENAME: &str = "last-save.json";
/// Maximum age of a warm-restore sentinel before it is treated as stale and
/// ignored. Five minutes comfortably covers a normal `cargo test --no-run`
/// followed by `cargo test` step pair on GitHub Actions while keeping the
/// short-circuit from kicking in on later, unrelated jobs.
pub(crate) const WARM_RESTORE_MAX_AGE_SECONDS: u64 = 5 * 60;

/// Pin a specific soldr version to handle this invocation. Explicit
/// `--as <version>` flag takes precedence over this env var.
const SOLDR_AS_ENV_VAR: &str = "SOLDR_AS";

/// Collect argv as UTF-8, reporting a non-UTF-8 argument instead of panicking.
///
/// soldr#2658 item 2: this used to be `std::env::args().collect()`, and
/// `std::env::args` *panics* mid-iteration on an argument that is not valid
/// Unicode. On Unix that is reachable with ordinary input -- paths are bytes,
/// so `soldr cargo build --manifest-path <non-utf8-path>` was a raw Rust
/// panic at `std/src/env.rs` with a backtrace note and no mention of soldr.
///
/// Verified on the Docker Linux runner before this change:
///
/// ```text
/// $ soldr $'--ÿþ-not-utf8'
/// thread 'soldr-cli' panicked at library/std/src/env.rs:864:51:
/// called `Result::unwrap()` on an `Err` value: "--ÿþ-not-utf8"
/// ```
///
/// The rest of the CLI is `&str`-typed -- clap parsing, shim-identity and
/// wrapper-invocation checks, the re-entrancy guard's argv classification --
/// so genuinely carrying `OsString` end to end is a separate change. Lossy
/// conversion would be worse than either: soldr mostly *forwards* argv to
/// cargo, and a silently mangled path would produce a wrong build rather than
/// a failed one. So this refuses, names the offending position, and shows the
/// bytes.
///
/// Note the asymmetry with the environment, which was measured at the same
/// time and needs no such handling: non-UTF-8 env var *values and names* both
/// round-trip cleanly, because soldr inherits the environment rather than
/// parsing it.
fn collect_utf8_args(
    args: impl Iterator<Item = std::ffi::OsString>,
) -> Result<Vec<String>, String> {
    let mut collected = Vec::new();
    for (index, arg) in args.enumerate() {
        match arg.into_string() {
            Ok(value) => collected.push(value),
            Err(raw) => {
                return Err(format!(
                    concat!(
                        "soldr: argument {index} is not valid UTF-8: {raw:?}
",
                        "soldr: arguments must be UTF-8; this one cannot be ",
                        "forwarded to cargo without corrupting it (soldr#2658).",
                    ),
                    index = index,
                    raw = raw,
                ));
            }
        }
    }
    Ok(collected)
}

/// Full soldr CLI entry — the `src/main.rs` shim calls this and never
/// returns control flow decisions of its own (#1490 Phase 1).
pub fn run() -> std::process::ExitCode {
    let raw_args = match collect_utf8_args(std::env::args_os()) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            exit_guard::mark_spoke();
            return std::process::ExitCode::from(1);
        }
    };
    // soldr#1934: a trampoline shim cannot set argv[0], so it passes its own
    // path in the environment. Restoring it here — before anything reads argv
    // — is what makes the trampoline and hardlink shapes the same program.
    let raw_args = multicall::apply_shim_argv0_override(raw_args);

    // Re-entrancy guard (soldr#2547/#2566): judge inherited parentage and
    // stamp IN_SOLDR_PID before any dispatch, spawn, or runtime setup.
    if let Some(code) = crate::reentrancy_guard::enforce_and_mark(&raw_args) {
        exit_guard::mark_spoke();
        guarded_exit(code);
    }
    // soldr#2571: from here on, every startup boundary is announced under
    // SOLDR_STARTUP_TRACE. A client that wedges before its command produces
    // output then names the last phase it completed instead of going silent.
    startup_trace::phase(startup_trace::phase::REENTRANCY_GUARD);

    if !multicall::toolchain_shim_should_defer_to_rustc_wrapper(&raw_args) {
        match multicall::maybe_dispatch(&raw_args) {
            Some(multicall::MulticallDispatch::Exit(code)) => guarded_exit(code),
            Some(multicall::MulticallDispatch::ExitCode(code)) => return code,
            Some(multicall::MulticallDispatch::SoldrArgs(args)) => {
                // soldr#3855: a shim-dispatched command (e.g. the `rustfmt`
                // shim cargo fmt launches) talks to the daemon too. Without
                // the broker connector it dialed the private daemon endpoint
                // and always saw NotRunning, even with a live daemon.
                let _ = crate::broker_control_transport::install();
                guarded_exit(block_on_exit_code(run_with_args("soldr", &args)));
            }
            None => {}
        }
    }
    startup_trace::phase(startup_trace::phase::MULTICALL_DISPATCH);

    guarded_exit(run_main(raw_args));
}

fn run_main(raw_args: Vec<String>) -> i32 {
    // RUSTC_WRAPPER mode: cargo passes `soldr /path/to/rustc <args...>`
    // Must be checked before clap parsing.
    if should_self_relocate_for_invocation(&raw_args) {
        match self_relocate::maybe_reexec_from_runtime(&raw_args) {
            // soldr#2024: a relocated soldr ran with our stdio and said
            // whatever there was to say; we only relay its code.
            Ok(Some(code)) => {
                exit_guard::mark_spoke();
                guarded_exit(code)
            }
            Ok(None) => {}
            Err(error) => guarded_exit(report_and_exit(error)),
        }
    }
    startup_trace::phase(startup_trace::phase::SELF_RELOCATE);

    // Route client control through the broker; standalone daemons never install this hook.
    let _ = crate::broker_control_transport::install();
    startup_trace::phase(startup_trace::phase::BROKER_CONTROL_TRANSPORT);
    if raw_args.len() > 1 && wrapper::is_wrapper_invocation(&raw_args[1]) {
        // soldr#2545: a Soldr-owned wrapper lineage must arrive with the
        // effective-wrapper mirror still matching RUSTC_WRAPPER. Drift here
        // means something rewired the wrapper identity mid-build; failing
        // before broker/daemon contact beats silently recompiling the world.
        if let Err(error) =
            crate::wrapper_identity::assert_inherited_wrapper_coherent("wrapper re-entry")
        {
            eprintln!("soldr: {error}");
            exit_guard::mark_spoke();
            guarded_exit(1);
        }
        // Per-phase startup timing for #440. `WrapperProfile::new()` is a
        // cheap branch + one `var_os` syscall when SOLDR_PROFILE_STARTUP
        // is unset, so the dominant production path pays effectively
        // nothing. When set, the profile captures `Instant::now()` at
        // each boundary down to the exec call.
        let mut profile = startup_profile::WrapperProfile::new();
        profile.mark("args_collected");
        // Deliberately NOT calling `global_upgrade::maybe_delegate` here
        // (#1847). That policy decides which soldr owns a *user-facing*
        // invocation; a wrapper invocation is an internal callback Cargo
        // makes once per compile unit, so the question is both wrong and
        // ruinously expensive to ask here:
        //
        // * Wrong — delegating mid-build would swap the compiler wrapper,
        //   and therefore the daemon/cache peer, partway through a build.
        //   The top-level soldr that launched Cargo already applied the
        //   policy for this build.
        // * Expensive — `probe_version` spawns the global soldr binary and
        //   blocks on `--version`. Measured at 52-61 ms here, which showed
        //   up as `pin_check_done` consuming 99.5% of every wrapper
        //   invocation (37-48 ms of a 37-48 ms total). Multiplied by every
        //   compile unit, that is ~20 s on a 500-unit build.
        //
        // The `--as` version pin below is a different question and still
        // applies: an explicitly pinned soldr version must stay in force
        // for the wrapper too, or the build would mix versions.
        if let Some(version) = soldr_as_env_pin() {
            if should_trampoline(&version) {
                return block_on_exit_code(version_trampoline::run(&version, &raw_args[1..]));
            }
        }
        profile.mark("pin_check_done");
        return wrapper::run_rustc_wrapper(&raw_args, profile).unwrap_or_else(report_and_exit);
    }

    // `--as <version>` trampoline. Peeled off before clap so the fetched
    // older soldr parses its own argv on its own terms.
    let (pinned_version, trampoline_args) = match extract_as_pin(&raw_args[1..]) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("soldr: {e}");
            guarded_exit(1);
        }
    };
    let pinned_version = pinned_version.or_else(soldr_as_env_pin);

    // Hand off to a newer global soldr BEFORE touching the broker. The broker
    // is a stable singleton that is never replaced automatically (soldr#2549),
    // so an older soldr that spawned it and then delegated would pin every
    // later build on the machine to its older image. That is how a
    // `pip install .` with a stale `soldr==0.9.6` build backend left a 0.9.6
    // broker behind a 0.9.22 install. The newer soldr's front door spawns it.
    // An `--as` pin never delegates, so it keeps spawning from this image.
    if pinned_version.is_none() {
        if let Some(code) = crate::global_upgrade::maybe_delegate(&raw_args) {
            return code;
        }
        startup_trace::phase(startup_trace::phase::GLOBAL_UPGRADE);
    }

    crate::broker_spawn::maybe_spawn_broker_front_door(&raw_args);
    startup_trace::phase(startup_trace::phase::BROKER_FRONT_DOOR);
    // soldr#3193: after the spawn, never before it -- the notice must not
    // delay the path that makes this invocation work.
    crate::broker_inventory::maybe_toast(&raw_args);

    if let Some(version) = pinned_version {
        if should_trampoline(&version) {
            return block_on_exit_code(version_trampoline::run(&version, &trampoline_args));
        }
        // Short-circuit: requested version == current. Continue with args
        // that have `--as <ver>` stripped.
        return block_on_exit_code(run_with_args(&raw_args[0], &trampoline_args));
    }
    startup_trace::phase(startup_trace::phase::VERSION_PIN);

    block_on_exit_code(run_with_args(&raw_args[0], &raw_args[1..]))
}

// Process-lifetime runtime: this drives the whole CLI command (soldr#3836).
#[cfg_attr(dylint_lib = "ban_raw_sync_primitive", allow(ban_raw_sync_primitive))]
fn block_on_exit_code<F>(future: F) -> i32
where
    F: std::future::Future<Output = Result<i32, SoldrError>>,
{
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(future).unwrap_or_else(report_and_exit),
        Err(err) => report_and_exit(SoldrError::Other(format!(
            "failed to start async runtime: {err}"
        ))),
    }
}

fn soldr_as_env_pin() -> Option<String> {
    std::env::var(SOLDR_AS_ENV_VAR)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

async fn run_with_args(prog: &str, args: &[String]) -> Result<i32, SoldrError> {
    // Reached only once `block_on_exit_code` has built the multi-thread runtime
    // and is driving this future.
    startup_trace::phase(startup_trace::phase::TOKIO_RUNTIME);
    let mut argv: Vec<String> = Vec::with_capacity(args.len() + 1);
    argv.push(prog.to_string());
    argv.extend(args.iter().cloned());
    // Use parse_from (not try_parse_from) so clap handles --help / --version /
    // usage errors with its built-in exit(0) / exit(2), matching the original
    // invocation path's UX exactly.
    let cli = Cli::parse_from(argv);
    startup_trace::phase(startup_trace::phase::CLAP_PARSE);
    let outcome = Box::pin(run_cli(cli)).await.map(|_| 0);
    // soldr#2785: attribute the command body. Everything above this line is
    // startup; without a mark here the trace's last entry is `clap_parse` for
    // every invocation, so a slow command reads as slow argument parsing.
    //
    // A command that never returns -- an `exec` onto a fetched tool, or a
    // genuine wedge -- leaves no line, which is the same "entered but never
    // finished" signal the rest of this trace relies on.
    startup_trace::phase(startup_trace::phase::COMMAND_DISPATCH);
    outcome
}

async fn run_dylint_command(
    args: Vec<String>,
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    if args.first().map(String::as_str) == Some("prepare") {
        return crate::dylint_prepare::run(&args[1..]).await;
    }
    if args.first().map(String::as_str) == Some("cook") {
        return dylint_cook::run(&args[1..], cache_enabled).await;
    }
    let mut forwarded = Vec::with_capacity(args.len() + 1);
    forwarded.push("dylint".to_string());
    forwarded.extend(args);
    cargo_front_door::run_cargo_front_door(&forwarded, cache_enabled, trust_inherited_soldr_env)
        .await
}

include!("soldr_main_build.rs");
include!("soldr_main_dispatch.rs");
include!("soldr_main_commands.rs");
include!("soldr_main_external.rs");
include!("soldr_main_helpers.rs");
