//! `soldr cargo ...` front door, profile-debug-default detection, linker
//! injection, low-disk warning, and the cargo arg-parsing helpers shared
//! with `rust_plan`. Extracted from `main.rs` as part of issue #339.
//!
//! Split into sub-modules under `cargo_front_door/`:
//! - [`subcommand`] — argv-level cargo subcommand sniffing, cacheability
//!   classification, and target-flag detection.
//! - [`inputs`] — hashing inputs shared with `rust_plan` (profile,
//!   target, feature, env-var, manifest, and config hashes).
//! - [`profile_debug`] — `[profile.<P>].debug` default detection and
//!   the `CARGO_PROFILE_<P>_DEBUG=false` injection / one-shot warning.
//! - [`target`] — target-triple resolution and `SOLDR_LINKER` injection.
//! - [`disk`] — low-disk warning, free-space probing, PATH/arg helpers.
//!
//! This file owns the cross-cutting `run_cargo_front_door` entry, the
//! `--no-gc-target*` flag stripping, the cargo output-capture wrappers,
//! the known-subcommand fetch hook, and the build-session bookkeeping.

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use crate::fetch::VersionSpec;
use crate::trampoline::{
    refresh_sidecar_after_cargo, strip_no_trampoline_flag, try_run_trampoline, FellThroughPlan,
    TrampolineDecision,
};
use crate::zccache::{
    cache_lifecycle_from_env, command_lifetime_shutdown_timeout, CacheLifecycle,
    SOLDR_CACHE_LIFECYCLE_ENV_VAR, SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR,
};
use crate::{gc, resolve_toolchain_binary_for_channel};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wait_timeout::ChildExt;

mod build_session;
pub(crate) mod cache_annotations; // soldr#3924 — where those annotations go
mod cache_plan;
pub(crate) mod cache_states; // soldr#2302 — HIT/MISS annotations + stats
mod clang_cl_shim;
mod cmake_need;
mod component_install;
mod config_args;
pub(crate) mod cook_hydrate;
mod darwin_embed;
/// soldr#2546 — opt-in build process tracing (`soldr --debug`).
pub(crate) mod debug_trace;
mod disk;
mod fingerprint_noise;
mod host_tooling;
mod inputs;
mod install_notice;
mod job_budget;
mod line_endings;
mod lockfile_proof;
mod log_summary;
mod nested_cargo;
/// soldr#2924 — direct nested-Cargo self-lock guard.
mod nested_cargo_guard;
pub(crate) mod no_cache_detach;
mod orphan_rmeta;
/// `--porcelain` / `SOLDR_PORCELAIN` machine-readable front-door mode.
pub(crate) mod porcelain;
/// Failing-test digest for the porcelain summary (soldr#3961).
mod porcelain_failures;
/// `soldr lint` porcelain report and log (soldr#3958).
pub(crate) mod porcelain_lint;
mod profile_debug;
pub(crate) mod provision;
mod strip_diagnostics;
mod subcommand;
mod target;
/// soldr#1802 — elapsed-seconds prefixes on relayed child output.
pub(crate) mod timestamp_tee;
mod zig_shim;
mod zthreads_fallback;

pub(crate) use cache_plan::CargoCachePlan;
use config_args::insert_cargo_global_args;
use nested_cargo_guard::NestedCargoGuard;

const CARGO_WAIT_TIMEOUT_ENV_VAR: &str = "SOLDR_CARGO_WAIT_TIMEOUT_SECS";
/// Internal one-hop marker for commands that must share their Soldr parent's
/// process group. The nested process consumes it before spawning Cargo.
pub(crate) const INHERIT_PARENT_PROCESS_GROUP_ENV: &str = "SOLDR_INTERNAL_INHERIT_PROCESS_GROUP";
const CARGO_TIMEOUT_RETRY_DISABLE_ENV_VAR: &str = "SOLDR_NO_CARGO_TIMEOUT_RETRY";
const CARGO_WAIT_HEARTBEAT_SECS: u64 = 60;
const KILLED_CARGO_REAP_TIMEOUT_SECS: u64 = 5;
const CAPTURE_PIPE_EOF_GRACE: Duration = Duration::from_secs(2);
const COMPILE_JOURNAL_TAIL_WAIT: Duration = Duration::from_secs(2);
const COMPILE_JOURNAL_TAIL_POLL: Duration = Duration::from_millis(25);
const BUILD_HISTORY_RETRY_ATTEMPTS: usize = 20;
const BUILD_HISTORY_RETRY_POLL: Duration = Duration::from_millis(25);
// -- Re-exports for cross-module callers --
// External modules (`gc`, `rust_plan`, `main`)
// reach into `crate::cargo_front_door::*` using the names that existed
// on the flat file. Re-export them from the sub-modules so the public
// API is byte-for-byte identical after the split.
pub(crate) use disk::{
    available_space, existing_filesystem_probe_path, low_disk_warning_for_free_bytes,
    low_disk_warning_for_path,
};
pub(crate) use inputs::{
    build_env_inputs, cargo_config_hash, cargo_feature_inputs, cargo_profile, cargo_target_triple,
    file_hash_or_missing, path_string, rustflags_inputs, selected_cargo_args, sha256_bytes,
    stable_hash_json, workspace_manifest_hashes,
};
pub(crate) use profile_debug::CargoProfileDebugDefault;
pub(crate) use subcommand::{
    cargo_args_are_cacheable, cargo_args_may_compile_unmediated,
    cargo_args_should_apply_rustfmt_shim, cargo_args_specify_target,
    cargo_args_use_reserved_no_cache, first_cargo_subcommand, first_cargo_subcommand_index,
};

/// 64-bit build session id: high 32 bits = unix-ms truncated, low 32
/// bits = pid-XOR-nanos so two concurrent builds in the same ms never
/// collide. Cheap and good enough for in-process correlation.
pub(crate) fn generate_build_session_id() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let high = ((nanos / 1_000_000) as u64) & 0xFFFF_FFFF;
    let low = ((nanos as u64) ^ (std::process::id() as u64)) & 0xFFFF_FFFF;
    (high << 32) | low
}

struct CargoAbortLogRequest<'a> {
    paths: &'a SoldrPaths,
    session_id: u64,
    repo_root: &'a Path,
    started_at_ms: i64,
    ended_at_ms: i64,
    args: &'a [String],
    timeout: bool,
    cargo_wait_timeout: Option<Duration>,
    cleanup: CargoAbortCleanupReport,
    message: &'a str,
    auto_retry_planned: bool,
}

type CargoRunResult = Result<
    (
        std::process::ExitStatus,
        Option<String>,
        Option<Vec<String>>,
        Option<porcelain::PorcelainLogArtifact>,
    ),
    SoldrError,
>;

include!("history_and_timeout.rs");
include!("environment_and_cleanup.rs");
include!("run.rs");
include!("run_prepare.rs");
include!("run_finish.rs");
include!("output_capture.rs");
include!("subcommand_bootstrap.rs");
#[cfg(test)]
fn resolve_dylint_binary<S>(
    crate_name: &str,
    fetched: Result<crate::fetch::FetchResult, SoldrError>,
    _source_build: S,
) -> Result<std::path::PathBuf, SoldrError>
where
    S: FnOnce() -> Result<std::path::PathBuf, SoldrError>,
{
    match fetched {
        Ok(result) => {
            if result.cached {
                eprintln!("soldr: using cached {} v{}", crate_name, result.version);
            } else {
                eprintln!("soldr: downloaded {} v{}", crate_name, result.version);
            }
            Ok(result.binary_path)
        }
        Err(error) => {
            let version = crate::fetch::known_tools::lookup_by_crate(crate_name)
                .and_then(|spec| spec.pinned_version)
                .unwrap_or("unknown");
            Err(dylint_unavailable_error(crate_name, version, &error))
        }
    }
}

/// Resolve transitive runtime dependencies for `cargo-<sub>` and append
/// their bin directories to `extra_bin_dirs` (PATH-prepended on the
/// child cargo) and any required env overrides to `extra_env`.
///
/// Registered bootstraps:
///   - `cargo dylint`, and any other subcommand whose project config
///     declares a bare `dylint-link` → ensure the managed `dylint-link` on
///     PATH (soldr#3571; the config-declared case also fires for plain
///     cargo verbs via `ensure_known_subcommand_tool`).
///   - `cargo zigbuild` → ensure `zig` is on PATH (PR #841).
///   - explicit legacy `cargo zigbuild --target *-apple-darwin` → ensure
///     Apple SDK on disk + set `SDKROOT` env (issue #854).
///   - `cargo xwin build --target *-pc-windows-msvc` → ensure `clang`
///     shim on PATH that forces `--driver-mode=cl` (PR #849).
///   - `cargo nextest archive --target {darwin,windows-msvc}` → reuse the
///     blessed SDK + clang/lld prep from `soldr build` (soldr#1432/#1524).
async fn append_subcommand_transitive_bin_dirs(
    sub: &str,
    args: &[String],
    paths: &SoldrPaths,
    extra_bin_dirs: &mut Vec<std::path::PathBuf>,
    extra_env: &mut Vec<(String, String)>,
    extra_cargo_args: &mut Vec<String>,
) -> Result<(), SoldrError> {
    // soldr#3571: `cargo dylint` always needs the managed `dylint-link`; any
    // OTHER subcommand needs it when the project's own cargo config drives
    // rustc with a bare `dylint-link` (a configured lint crate's standalone
    // compiler pass). The detection lives behind one helper so the two
    // entry points — this transitive bootstrap and the plain-cargo-verb
    // early return in `ensure_known_subcommand_tool`, which never reaches
    // this function — cannot drift apart.
    ensure_dylint_link_bin_dir(sub, args, paths, extra_bin_dirs).await?;
    if sub == "zigbuild" {
        let zig_dir = crate::fetch::ensure_zig(paths).await?;
        extra_bin_dirs.push(zig_dir.clone());
        // Explicit legacy `soldr cargo zigbuild --target *-apple-darwin`
        // needs the Apple SDK on disk + `SDKROOT` exported so cargo-zigbuild's
        // mach-O linker can resolve `-framework IOKit` / etc.
        // Without this, every Rust dep with an Apple-framework
        // dependency (ring, sysinfo, dirs, …) fails to link.
        if let Some(triple) = subcommand::cargo_args_target(args) {
            append_zigbuild_env_overrides(paths, triple, extra_env)?;
            if triple.ends_with("-apple-darwin") {
                let sdk_dir = crate::fetch::ensure_apple_sdk(paths, Some(triple)).await?;
                // Don't clobber a caller-set SDKROOT — escape hatch
                // for users with their own Xcode SDK or a custom one.
                if std::env::var_os("SDKROOT").is_none() {
                    extra_env.push((
                        "SDKROOT".to_string(),
                        sdk_dir.to_string_lossy().into_owned(),
                    ));
                }
                if std::env::var_os("PKG_CONFIG_SYSROOT_DIR").is_none() {
                    extra_env.push((
                        "PKG_CONFIG_SYSROOT_DIR".to_string(),
                        sdk_dir.to_string_lossy().into_owned(),
                    ));
                }
            }
        }
    }
    // `soldr cargo xwin build --target *-pc-windows-msvc` needs a
    // `clang` shim that forces `--driver-mode=cl`. ring's build.rs
    // hard-codes `c.compiler("clang")` for the aarch64 target which
    // overrides cc-rs's env-driven compiler choice (so just setting
    // CC_<triple>=clang-cl doesn't help). Putting our shim on PATH
    // wins ring's `clang` PATH lookup. See `clang_cl_shim` for the
    // full rationale.
    //
    // In addition, the same lane needs a real LLVM toolchain on PATH
    // — clang-cl / lld-link / llvm-lib — for cargo-xwin's link step
    // to succeed on a stock Linux runner that doesn't have `apt install
    // llvm clang lld` baked in. soldr fetches the toolchain from
    // `zackees/clang-tool-chain-bins` (closes #855, sub of meta #853)
    // and prepends its `bin/` to PATH alongside the clang shim. Env
    // overrides for CC_<triple> / CXX_<triple> / AR_<triple> / LD_<triple>
    // are set to absolute paths inside the fetched bin dir so cc-rs
    // and rustc can find the right driver even if PATH ordering shifts.
    //
    // On hosts not in the managed LLVM matrix (today: macOS), we log
    // and skip — those hosts ship Apple's `clang` via Xcode, and the
    // xwin lane is not a primary mac flow. Workflow-side YAML hedges
    // (apt-installed llvm/clang/lld) remain in place; sub-issue #857
    // removes them once this auto-bootstrap proves out across the
    // matrix.
    if let Some(triple) = nextest_archive_zig_target(args) {
        let zig_dir = crate::fetch::ensure_zig(paths).await?;
        extra_bin_dirs.push(zig_dir);
        append_zigbuild_env_overrides(paths, triple, extra_env)?;
    }
    if sub == "xwin" {
        if let Some(triple) = subcommand::cargo_args_target(args) {
            if triple.ends_with("-pc-windows-msvc") {
                match crate::fetch::ensure_llvm_toolchain(paths).await {
                    Ok(llvm_bin_dir) => {
                        let ext = std::env::consts::EXE_SUFFIX;
                        let clang = llvm_bin_dir.join(format!("clang{ext}"));
                        let clang_cl = llvm_bin_dir.join(format!("clang-cl{ext}"));
                        let llvm_lib = llvm_bin_dir.join(format!("llvm-lib{ext}"));
                        let lld_link = llvm_bin_dir.join(format!("lld-link{ext}"));
                        let shim_dir =
                            clang_cl_shim::ensure_clang_cl_shim_for_real_clang(paths, &clang)?;
                        extra_bin_dirs.push(shim_dir);
                        let suffix = triple.replace('-', "_");
                        // Don't clobber caller-set values — escape hatch
                        // for users who pinned their own LLVM build.
                        // Note: `compute_subcommand_env_overrides` sets
                        // bare names (`clang-cl` / `llvm-lib`) for the
                        // same triple; the absolute paths we set here
                        // win because they're pushed into `extra_env`
                        // and applied AFTER the env loop checks
                        // `var_os().is_none()` (transitive env applies
                        // unconditionally — see the apply loop below).
                        // To avoid that double-set racing, gate on
                        // `var_os` here too: the bare-name fallback
                        // still hits PATH which now contains the
                        // LLVM bin dir.
                        for (key, val) in [
                            (format!("CC_{suffix}"), &clang_cl),
                            (format!("CXX_{suffix}"), &clang_cl),
                            (format!("AR_{suffix}"), &llvm_lib),
                            (format!("LD_{suffix}"), &lld_link),
                        ] {
                            if std::env::var_os(&key).is_none() {
                                extra_env.push((key, val.to_string_lossy().into_owned()));
                            }
                        }
                        extra_bin_dirs.push(llvm_bin_dir);
                    }
                    Err(SoldrError::UnsupportedPlatform(msg)) => {
                        let shim_dir = clang_cl_shim::ensure_clang_cl_shim(paths)?;
                        extra_bin_dirs.push(shim_dir);
                        eprintln!(
                            "soldr: skipping managed LLVM bootstrap: {msg}; \
                             falling back to system clang/lld-link/llvm-lib on PATH"
                        );
                    }
                    Err(err) => return Err(err),
                }

                // zlib-ng's ARM optimizations are unbuildable under
                // clang-cl — chain a toolchain-file wrapper that turns
                // them off for the aarch64 lane. See
                // `ensure_zlib_ng_arm_cmake_wrapper` for the full
                // root-cause writeup (cross-run 28574600982 fix).
                if let Some((key, value)) = ensure_zlib_ng_arm_cmake_wrapper(paths, triple)? {
                    if std::env::var_os(&key).is_none() {
                        extra_env.push((key, value));
                    }
                }
            }
        }
    }
    if let Some(triple) = nextest_archive_blessed_target(args) {
        let prep = crate::blessed_build::prepare(paths, triple, &[]).await?;
        if triple.ends_with("-pc-windows-msvc") && prep.xwin_cache_dir.is_none() {
            return Err(SoldrError::Other(format!(
                "cargo nextest archive for {triple} requires the managed xwin-cache; \
                 the blessed toolchain could not materialize it"
            )));
        }
        append_blessed_prep_to_subcommand_bootstrap(
            prep,
            extra_bin_dirs,
            extra_env,
            extra_cargo_args,
        );
    }
    Ok(())
}

fn nextest_archive_blessed_target(args: &[String]) -> Option<&str> {
    let sub_idx = first_cargo_subcommand_index(args)?;
    if args[sub_idx] != "nextest" {
        return None;
    }
    if first_nextest_verb(args, sub_idx) != Some("archive") {
        return None;
    }
    let triple = subcommand::cargo_args_target(args)?;
    (triple.ends_with("-apple-darwin") || triple.ends_with("-pc-windows-msvc")).then_some(triple)
}

fn nextest_archive_zig_target(args: &[String]) -> Option<&str> {
    let sub_idx = first_cargo_subcommand_index(args)?;
    if args[sub_idx] != "nextest" || first_nextest_verb(args, sub_idx) != Some("archive") {
        return None;
    }
    subcommand::cargo_args_target(args).filter(|triple| is_zig_linux_cross_target(triple))
}

fn is_zig_linux_cross_target(triple: &str) -> bool {
    triple.ends_with("-unknown-linux-musl") || triple == "aarch64-unknown-linux-gnu"
}

fn zig_cross_target(args: &[String]) -> Option<&str> {
    if let Some(target) = nextest_archive_zig_target(args) {
        return Some(target);
    }
    let sub_idx = first_cargo_subcommand_index(args)?;
    (args[sub_idx] == "zigbuild")
        .then(|| subcommand::cargo_args_target(args))
        .flatten()
        .filter(|target| is_zig_linux_cross_target(target))
}

fn emit_zig_cross_linker_preflight(
    command: &std::process::Command,
    args: &[String],
) -> Result<(), SoldrError> {
    let Some(target) = zig_cross_target(args) else {
        return Ok(());
    };
    let key = format!(
        "CARGO_TARGET_{}_LINKER",
        target.replace('-', "_").to_ascii_uppercase()
    );
    let linker = command
        .get_envs()
        .find_map(|(name, value)| (name == std::ffi::OsStr::new(&key)).then_some(value))
        .flatten()
        .map(std::ffi::OsStr::to_os_string)
        .or_else(|| std::env::var_os(&key));
    validate_zig_cross_linker(target, linker.as_deref())?;
    eprintln!(
        "soldr: cross-link preflight requested_target={target} effective_target={target} artifact_target={target} linker={} env={key} status=ok",
        linker.as_deref().unwrap_or_default().to_string_lossy()
    );
    Ok(())
}

fn validate_zig_cross_linker(
    target: &str,
    linker: Option<&std::ffi::OsStr>,
) -> Result<(), SoldrError> {
    let linker = linker.ok_or_else(|| {
        SoldrError::Other(format!(
            "cross-link preflight failed for {target}: target linker is unset"
        ))
    })?;
    let path = std::path::Path::new(linker);
    if path.components().count() == 1 {
        let name = linker.to_string_lossy().to_ascii_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        let host_fallback = matches!(name, "cc" | "gcc" | "clang" | "ld")
            || name
                .strip_prefix("clang-")
                .is_some_and(|version| version.chars().all(|ch| ch.is_ascii_digit()));
        if host_fallback {
            return Err(SoldrError::Other(format!(
                "cross-link preflight failed for {target}: `{}` is a bare host linker; configure the target-scoped Zig/cross linker before compiling target objects",
                linker.to_string_lossy()
            )));
        }
    }
    Ok(())
}

fn first_nextest_verb(args: &[String], nextest_idx: usize) -> Option<&str> {
    first_nextest_verb_index(args, nextest_idx)
        .and_then(|index| args.get(index))
        .map(String::as_str)
}

fn first_nextest_verb_index(args: &[String], nextest_idx: usize) -> Option<usize> {
    let mut skip_next = false;
    for (index, arg) in args.iter().enumerate().skip(nextest_idx + 1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            return None;
        }
        if nextest_global_arg_takes_value(arg) {
            skip_next = !arg.contains('=');
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(index);
    }
    None
}

fn nextest_global_arg_takes_value(arg: &str) -> bool {
    matches!(arg, "--color" | "--config-file" | "--tool-config-file")
        || arg.starts_with("--color=")
        || arg.starts_with("--config-file=")
        || arg.starts_with("--tool-config-file=")
}

fn append_blessed_prep_to_subcommand_bootstrap(
    prep: crate::blessed_build::BlessedPrep,
    extra_bin_dirs: &mut Vec<std::path::PathBuf>,
    extra_env: &mut Vec<(String, String)>,
    extra_cargo_args: &mut Vec<String>,
) {
    extra_bin_dirs.extend(prep.path_prefix());
    extra_env.extend(prep.env);
    extra_cargo_args.extend(prep.cargo_args);
}

/// Write the cmake toolchain-file WRAPPER that disables zlib-ng's ARM
/// optimizations for `aarch64-pc-windows-msvc` cross-compiles, and
/// return the `(env var, path)` pair to export on the child cargo.
/// Returns `Ok(None)` for every other triple.
///
/// ## Why (run 28574600982, windows-arm lane)
///
/// libz-ng-sys builds vendored zlib-ng via cmake. Under clang-cl
/// (`_MSC_VER` defined, but none of MSVC's compiler intrinsics exist),
/// zlib-ng's ARM feature detection is self-INCONSISTENT:
///
/// * `HAVE_ARMV8_INTRIN` probes `__crc32w` from `<intrin.h>` — an
///   MSVC-only declaration clang-cl doesn't ship → probe fails. But
///   the GNU-asm probe `HAVE_ARMV8_INLINE_ASM` passes, so cmake
///   enables `ARM_CRC32` anyway — and `acle_intrins.h`'s `_MSC_VER`
///   code path then requires exactly the missing `__crc32b/h/w/d`
///   intrinsics: `crc32_armv8.c` fails with "call to undeclared
///   function '__crc32b'".
/// * The NEON path includes MSVC's `arm64_neon.h`, whose
///   `vld1q_*_x4` macros expand to `neon_ld1m4_*` compiler magic that
///   clang-cl neither declares nor lowers. The `NEON_HAS_LD4` probe
///   dies at link ("undefined symbol: neon_ld1m4_q32"), and the
///   fallback inline functions zlib-ng then compiles collide with the
///   still-defined macros ("type specifier missing" in
///   `adler32_neon.c` via `neon_intrins.h`).
///
/// Turning `WITH_NEON` / `WITH_ARMV8` / `WITH_ARMV6` off makes
/// zlib-ng build its portable C fallbacks — the only combination
/// clang-cl can actually compile until upstream zlib-ng grows real
/// clang-cl ARM64 support.
///
/// ## How the wrapper reaches cmake
///
/// cargo-xwin exports `CMAKE_TOOLCHAIN_FILE_<underscore-triple>` on
/// the child cargo. The `cmake` crate (used by libz-ng-sys's
/// build.rs) checks the DASH-triple form of that variable FIRST
/// (`getenv_target_os` in cmake-rs), so soldr exports
/// `CMAKE_TOOLCHAIN_FILE_aarch64-pc-windows-msvc` pointing at this
/// wrapper. The wrapper chain-includes cargo-xwin's real clang-cl
/// toolchain file via `$ENV{...}` (still present in the build-script
/// environment) so compiler/linker setup is byte-identical, then
/// force-caches the three `WITH_*` toggles.
fn ensure_zlib_ng_arm_cmake_wrapper(
    paths: &SoldrPaths,
    triple: &str,
) -> Result<Option<(String, String)>, SoldrError> {
    if !(triple.starts_with("aarch64-") && triple.ends_with("-pc-windows-msvc")) {
        return Ok(None);
    }
    let dir = paths.root.join("cmake").join(triple);
    std::fs::create_dir_all(&dir).map_err(|e| {
        SoldrError::Other(format!("create cmake wrapper dir {}: {e}", dir.display()))
    })?;
    let wrapper = dir.join("clang-cl-arm-toolchain.cmake");
    let underscore = triple.replace('-', "_");
    let content = format!(
        r#"# Written by soldr (cross-run 28574600982 fix) — do not edit; regenerated each run.
# Chain-include cargo-xwin's generated clang-cl toolchain file (it
# exports the underscore-form env var on the child cargo) so the
# compiler/linker setup is unchanged.
if(DEFINED ENV{{CMAKE_TOOLCHAIN_FILE_{underscore}}})
    include("$ENV{{CMAKE_TOOLCHAIN_FILE_{underscore}}}")
endif()

# zlib-ng's ARM optimizations require MSVC-only compiler intrinsics
# (__crc32*, neon_ld1m4_*) that clang-cl does not implement; its cmake
# feature detection half-enables them anyway and the build dies in
# crc32_armv8.c / adler32_neon.c. Force the portable C fallbacks.
set(WITH_NEON OFF CACHE BOOL "soldr: MSVC-intrinsic-only under clang-cl" FORCE)
set(WITH_ARMV8 OFF CACHE BOOL "soldr: MSVC-intrinsic-only under clang-cl" FORCE)
set(WITH_ARMV6 OFF CACHE BOOL "soldr: MSVC-intrinsic-only under clang-cl" FORCE)
"#
    );
    std::fs::write(&wrapper, content).map_err(|e| {
        SoldrError::Other(format!("write cmake wrapper {}: {e}", wrapper.display()))
    })?;
    Ok(Some((
        format!("CMAKE_TOOLCHAIN_FILE_{triple}"),
        wrapper.to_string_lossy().into_owned(),
    )))
}

fn append_zigbuild_env_overrides(
    paths: &SoldrPaths,
    triple: &str,
    extra_env: &mut Vec<(String, String)>,
) -> Result<(), SoldrError> {
    let wrappers = match zig_shim::ensure_zig_wrappers(paths, triple) {
        Ok(wrappers) => wrappers,
        Err(SoldrError::UnsupportedPlatform(_)) => return Ok(()),
        Err(err) => return Err(err),
    };
    let suffix = triple.replace('-', "_");
    let upper = suffix.to_uppercase();
    for (key, val) in [
        (format!("CC_{suffix}"), wrappers.cc.as_path()),
        (format!("CXX_{suffix}"), wrappers.cxx.as_path()),
        (format!("AR_{suffix}"), wrappers.ar.as_path()),
        (format!("RANLIB_{suffix}"), wrappers.ranlib.as_path()),
        (
            format!("CARGO_TARGET_{upper}_LINKER"),
            wrappers.cc.as_path(),
        ),
    ] {
        if std::env::var_os(&key).is_none() {
            extra_env.push((key, val.to_string_lossy().into_owned()));
        }
    }
    Ok(())
}

/// Compute environment-variable overrides for the child cargo invocation
/// based on the subcommand and its `--target` argument.
///
/// The only rule today: `cargo xwin build --target <triple>` for any
/// `<triple>` ending in `-pc-windows-msvc` injects
///
///   CC_<triple-underscored>  = clang-cl
///   CXX_<triple-underscored> = clang-cl
///   AR_<triple-underscored>  = llvm-lib
///
/// Why: cc-rs (used by ring, blake3, and other C-FFI crates) detects
/// `target=*-pc-windows-msvc` and formats include flags MSVC-style
/// (`/imsvc <path>`). But cc-rs's default driver for that triple,
/// when `cl.exe` isn't on PATH (typical on Linux cross-compile hosts),
/// is the GNU-flavoured `clang` driver — which interprets `/imsvc`
/// as a literal filename. The result is the build.rs error
///   `clang: error: no such file or directory: '/imsvc'`
/// observed when cross-compiling soldr to windows-arm64 via cargo-xwin
/// on a linux runner.
///
/// The fix is to tell cc-rs to use `clang-cl` (clang's MSVC-compatible
/// driver) explicitly. cc-rs reads `CC_<triple-underscored>` ahead of
/// its default heuristic; setting it routes ring's assembly compilation
/// to the right driver and the build succeeds.
///
/// The caller's existing `CC_*` / `CXX_*` / `AR_*` env vars take
/// precedence (the apply loop checks `std::env::var_os` first); this
/// hook only fills in the gaps.
fn compute_subcommand_env_overrides(args: &[String]) -> Vec<(String, String)> {
    let Some(sub) = first_cargo_subcommand(args) else {
        return Vec::new();
    };
    if sub != "xwin" {
        return Vec::new();
    }
    // Verify the inner verb is `build` / `test` / etc. (anything cc-rs
    // would invoke a build script for). cargo-xwin's other verbs
    // (`download`, `pre-download`) don't compile anything.
    let mut after_sub = args.iter().skip_while(|a| a.as_str() != sub);
    after_sub.next(); // consume the matched `xwin`
    let needs_cc = matches!(
        after_sub.clone().next().map(String::as_str),
        Some("build" | "test" | "check" | "run" | "bench" | "doc" | "clippy" | "rustc"),
    );
    if !needs_cc {
        return Vec::new();
    }
    let Some(triple) = subcommand::cargo_args_target(args) else {
        return Vec::new();
    };
    if !triple.ends_with("-pc-windows-msvc") {
        return Vec::new();
    }
    let suffix = triple.replace('-', "_");
    vec![
        (format!("CC_{suffix}"), "clang-cl".to_string()),
        (format!("CXX_{suffix}"), "clang-cl".to_string()),
        (format!("AR_{suffix}"), "llvm-lib".to_string()),
    ]
}

#[cfg(test)]
mod backtrace_policy_tests;
#[cfg(test)]
mod cargo_abort_log_tests;
#[cfg(test)]
mod dylint_driver_tests;
#[cfg(test)]
mod dylint_link_ensure_tests;
#[cfg(test)]
mod dylint_link_validation_tests;
#[cfg(test)]
mod dylint_rustup_home_child_tests;
#[cfg(test)]
mod linker_project_config_tests;
#[cfg(test)]
mod porcelain_tests;
#[cfg(test)]
mod scrub_pool_tests;
#[cfg(test)]
mod signal_tests;
#[cfg(test)]
mod tests;
