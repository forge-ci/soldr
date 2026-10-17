//! Cargo-front-door pre-flight: probe the daemon's `cook_index_v1`
//! for a match and hydrate the workspace `target/` tree on hit
//! (issue #578, meta #579).
//!
//! Auto-hydrate is **ON by default**. Three opt-out mechanisms, in
//! precedence order (highest wins):
//!
//! 1. `SOLDR_COOK_AUTO_HYDRATE=0|1` (env var)
//! 2. `[soldr.cook] auto_hydrate = false` in `rust-toolchain.toml`
//! 3. `[cook] auto_hydrate = false` in `~/.soldr/config.toml`
//!
//! Hot-path budget: a CookLookup miss with the daemon up must cost
//! < 100 ms. A daemon-down path is silent. Pre-flight failure NEVER
//! blocks the cargo invocation — every branch falls through cleanly.

use crate::cache_lib::cook_archive::{
    self, compute_recipe_hash_proxy, extract_skip_existing, quarantine_artifact, sha_abbrev,
    verify_sha256,
};
use crate::core::git::{branch_lineage, origin_url};
use crate::core::tool_output;
use crate::core::{
    read_rust_toolchain_manifest, CookConfig, SoldrConfig, SoldrPaths, TargetTriple,
};
use crate::daemon::client::{self, CookLookupOutcome};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Env var that overrides both file-level settings. `0`/`false`/`no`/
/// `off` disables; anything else (including unset → fall through to
/// the config files) leaves the choice to the next layer.
pub const SOLDR_COOK_AUTO_HYDRATE_ENV: &str = "SOLDR_COOK_AUTO_HYDRATE";

/// Soldr-private channel by which `soldr cook` publishes the target triple
/// its own cook is scoped to, for the front-door invocations it makes
/// in-process (soldr#3043).
///
/// Needed because Phase 1 of `soldr cook` is `cargo chef prepare`, whose argv
/// carries **no** `--target` (cargo-chef's `prepare` does not accept one — see
/// `build_chef_prepare_args` in `cook_indexing.rs`). That invocation is still
/// "build-like" to the front door, so the pre-flight below runs for it, and
/// with only the argv to go on it would extract a `--target`-scoped archive
/// into the bare `target/` root: a full duplicate extraction into a directory
/// Cargo never reads for a `--target` build, which also drops the warm-cook
/// marker outside `resolve_cook_target_dir` so soldr#621's Phase-2
/// short-circuit could never fire.
///
/// Never set by a user; `soldr cook` sets it for itself and an explicit
/// `--target` in argv still wins.
pub const SOLDR_COOK_HYDRATE_TARGET_ENV: &str = "SOLDR_COOK_HYDRATE_TARGET";

/// Conservative decision for a cooked-artifact restore. Unknown historical
/// values always skip instead of gambling CI wall time on a nominal hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CookRestoreDecision {
    Restore {
        estimated_transport_ms: u64,
    },
    Skip {
        estimated_transport_ms: Option<u64>,
        reason: &'static str,
    },
}

#[derive(Debug)]
enum TimedRestoreOutcome<T> {
    Restored(T),
    ShaMismatch,
    Failed,
}

/// Verify and extract under one wall timer. Verification reads the complete
/// archive, so excluding it would under-report the restore cost precisely for
/// the large artifacts this gate exists to police.
fn timed_verified_extract<T, E>(
    verify: impl FnOnce() -> Result<bool, E>,
    extract: impl FnOnce() -> Option<T>,
) -> (TimedRestoreOutcome<T>, u64) {
    let restore_started = Instant::now();
    let outcome = match verify() {
        Ok(true) => extract().map_or(TimedRestoreOutcome::Failed, TimedRestoreOutcome::Restored),
        Ok(false) => TimedRestoreOutcome::ShaMismatch,
        Err(_) => TimedRestoreOutcome::Failed,
    };
    let restore_elapsed_ms =
        u64::try_from(restore_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    (outcome, restore_elapsed_ms)
}

/// Floor on archive restore bandwidth, in compressed bytes per millisecond
/// (16 MiB/s), used to estimate a restore before it runs (soldr#3117).
///
/// Restore is `sha256` verify + zstd decode + tar write. zstd decoding speed
/// is independent of the compression level, so the level-19 pack that took
/// minutes says nothing about it: a 742 MB cook archive (4.4 GB tree) verified
/// and extracted in 5.3 s on a developer host, about 140 MB/s of compressed
/// input, and the CI stable-tree archive is 187 MiB. The floor sits roughly
/// nine times under that measurement so that a slow runner disk still cannot
/// make the estimate optimistic. The measured `performance miss` line after a
/// restore remains the honest check.
const COOK_RESTORE_FLOOR_BYTES_PER_MS: u64 = 16 * 1024 * 1024 / 1000;

fn decide_cook_restore(archive_bytes: u64, compile_duration_ms: u64) -> CookRestoreDecision {
    if archive_bytes == 0 {
        return CookRestoreDecision::Skip {
            estimated_transport_ms: None,
            reason: "archive has no recorded bytes",
        };
    }
    if compile_duration_ms == 0 {
        return CookRestoreDecision::Skip {
            estimated_transport_ms: None,
            reason: "no prior compile duration is recorded",
        };
    }

    // Only the restore is on the table: the pack that produced the archive
    // is sunk cost. The previous estimate charged that pack again and then
    // assumed a restore as slow as a level-19 compression, which declined
    // every hit whose compile the compile cache already served -- exactly
    // the CI shape this gate is meant to speed up (soldr#3117).
    let estimated_transport_ms = archive_bytes.div_ceil(COOK_RESTORE_FLOOR_BYTES_PER_MS);
    if estimated_transport_ms >= compile_duration_ms {
        CookRestoreDecision::Skip {
            estimated_transport_ms: Some(estimated_transport_ms),
            reason: "estimated transport is not cheaper than the avoided compile",
        }
    } else {
        CookRestoreDecision::Restore {
            estimated_transport_ms,
        }
    }
}

fn is_cook_performance_miss(restore_elapsed_ms: u64, compile_duration_ms: u64) -> bool {
    compile_duration_ms > 0 && restore_elapsed_ms >= compile_duration_ms
}

/// Run the pre-flight. Best-effort across the board — any error
/// (missing manifest, missing Cargo.lock, daemon down, parse failure,
/// SHA mismatch, extract failure) silently returns to the caller so
/// cargo runs normally.
pub fn maybe_hydrate(args: &[String], paths: &SoldrPaths, rustc: &Path) {
    let _ = try_hydrate(args, paths, rustc);
}

/// Whether computing a cook lookup key can still pay off.
///
/// Deliberately conservative in both directions:
///
/// * An **explicitly empty** index (`entries == 0`) is the one case we
///   skip on. Nothing can match, so the key work is pure waste.
/// * A status reply **without** cook stats means "unknown" — an older
///   daemon predating the field would otherwise lose hydration it used
///   to perform, so we fall through and behave exactly as before.
/// * A daemon that is **not running** skips too: [`client::cook_lookup`]
///   talks to the same socket and cannot succeed either, so the key work
///   would be discarded a few hundred milliseconds later regardless. This
///   does not suppress hydration that would otherwise have happened; it
///   only stops paying for a lookup that is already guaranteed to fail.
/// * Any **other** failure (a status reply that timed out, a protocol
///   error) is "unknown" and falls through to the lookup. A daemon whose
///   control plane is still warming up after launch answers a 2 s status
///   probe late but the lookup fine; reading that timeout as "no daemon"
///   silently disabled hydration for every `soldr cook` that had just
///   brought its own daemon up (soldr#3117).
fn cook_lookup_is_worthwhile(sock: &Path) -> bool {
    lookup_worthwhile_from_status(client::status(sock))
}

fn lookup_worthwhile_from_status(
    status: Result<crate::daemon::protocol::StatusInfo, client::ClientError>,
) -> bool {
    match status {
        Ok(info) => info.cook_stats.is_none_or(|stats| stats.entries > 0),
        Err(client::ClientError::NotRunning) => false,
        Err(_) => true,
    }
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn try_hydrate(args: &[String], paths: &SoldrPaths, rustc: &Path) -> Option<()> {
    let manifest_path = crate::trampoline::find_nearest_manifest()?;
    let manifest_dir = manifest_path.parent()?.to_path_buf();

    // Cargo.lock must exist — without it the recipe hash is undefined.
    if !manifest_dir.join("Cargo.lock").is_file() {
        return None;
    }

    // Auto-hydrate gating (env > rust-toolchain.toml > config.toml).
    let config = match SoldrConfig::load(&paths.config_file) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(%error, "disabling cook auto-hydration because soldr config is invalid");
            return None;
        }
    };
    if !auto_hydrate_enabled(&manifest_dir, &config.cook) {
        return None;
    }

    // A tree `soldr cook` cooked in place already holds everything the
    // archive would restore (the archive was packed from it). Every later
    // front door in the same tree -- each `soldr ci-test` stage, for one --
    // would otherwise verify and walk the whole archive to add zero files,
    // about 10 s a time in CI (soldr#3117).
    if target_already_cooked(
        &resolve_target_dir(&manifest_dir, args),
        profile_dir_name(&resolve_profile_name(args)),
    ) {
        return None;
    }

    let sock = client::default_sock_path(paths);
    // Building the lookup key below is expensive: a recursive walk that
    // hashes every Cargo.toml, plus three subprocesses (`rustc -V`,
    // `git config --get remote.origin.url`, `git branch --show-current`).
    // Measured at ~138 ms per invocation on Windows (#1843).
    //
    // Cook is opt-in — the index stays empty until `soldr cook` populates
    // it — so for most users every one of those milliseconds computes a
    // key for an index that cannot possibly match. Ask the daemon first:
    // one Status round-trip is far cheaper, and it is the same daemon the
    // lookup would talk to anyway.
    if !cook_lookup_is_worthwhile(&sock) {
        return None;
    }

    let recipe_hash = compute_recipe_hash_proxy(&manifest_dir)?;
    let triple = resolve_target_triple(&manifest_dir, args)?;
    let profile_name = resolve_profile_name(args);
    let channel = read_rust_toolchain_manifest(&manifest_dir)
        .ok()
        .and_then(|m| m.channel)
        .unwrap_or_default();
    let rustc_version = rustc_version_for_hydrate(rustc)?;
    let origin = origin_url(&manifest_dir);

    let lineage = branch_lineage(&manifest_dir);
    let outcome = client::cook_lookup_with_branch_lineage(
        &sock,
        recipe_hash,
        triple,
        profile_name.clone(),
        channel,
        rustc_version,
        origin,
        lineage,
    )
    .ok()?;

    let CookLookupOutcome::Hit {
        sha256,
        path,
        size_bytes,
        origin_url_normalized,
        matched_recipe_hash,
        exact_recipe_match,
        branch_name,
        compile_duration_ms,
        save_elapsed_ms: _,
    } = outcome
    else {
        return None;
    };

    let estimated_transport_ms = match decide_cook_restore(size_bytes, compile_duration_ms) {
        CookRestoreDecision::Restore {
            estimated_transport_ms,
        } => estimated_transport_ms,
        CookRestoreDecision::Skip {
            estimated_transport_ms,
            reason,
        } => {
            let estimate = estimated_transport_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            eprintln!(
                "soldr cook: decision=skip  size_bytes={size_bytes}  estimated_transport_ms={estimate}  compile_elapsed_ms={compile_duration_ms}  reason={reason}"
            );
            return None;
        }
    };

    let artifact = PathBuf::from(&path);
    let target_dir = resolve_target_dir(&manifest_dir, args);
    let (restore_outcome, restore_elapsed_ms) = timed_verified_extract(
        || verify_sha256(&artifact, &sha256),
        || {
            std::fs::create_dir_all(&target_dir).ok()?;
            extract_skip_existing(&artifact, &target_dir).ok()
        },
    );
    let report = match restore_outcome {
        TimedRestoreOutcome::Restored(report) => report,
        TimedRestoreOutcome::ShaMismatch => {
            let abbrev = sha_abbrev(&sha256);
            if let Ok(quarantined) = quarantine_artifact(&artifact) {
                eprintln!(
                    "{} cook artifact sha256 mismatch — quarantined to {}",
                    yellow_warning_prefix(),
                    quarantined.display()
                );
            } else {
                eprintln!(
                    "{} cook artifact sha256 mismatch for {abbrev} — quarantine failed",
                    yellow_warning_prefix()
                );
            }
            return None;
        }
        TimedRestoreOutcome::Failed => return None,
    };

    // Fire-and-forget touch so the auto-GC pass prefers entries that
    // are actually serving traffic.
    let _ = client::cook_touch(&sock, sha256);

    emit_hydrate_line(HydrateLog {
        sha256: &sha256,
        size_bytes,
        origin_hint: origin_url_normalized.as_deref(),
        matched_recipe_hash: matched_recipe_hash.as_ref(),
        exact_recipe_match,
        branch_name: branch_name.as_deref(),
        restore_elapsed_ms,
        compile_duration_ms,
        estimated_transport_ms,
        report: &report,
    });
    if is_cook_performance_miss(restore_elapsed_ms, compile_duration_ms) {
        eprintln!(
            "soldr cook: performance miss  decision=restore  size_bytes={size_bytes}  elapsed_ms={restore_elapsed_ms}  compile_elapsed_ms={compile_duration_ms}"
        );
    }
    Some(())
}

struct HydrateLog<'a> {
    sha256: &'a [u8; 32],
    size_bytes: u64,
    origin_hint: Option<&'a str>,
    matched_recipe_hash: Option<&'a [u8; 32]>,
    exact_recipe_match: bool,
    branch_name: Option<&'a str>,
    restore_elapsed_ms: u64,
    compile_duration_ms: u64,
    estimated_transport_ms: u64,
    report: &'a cook_archive::ExtractReport,
}

fn emit_hydrate_line(log: HydrateLog<'_>) {
    let HydrateLog {
        sha256,
        size_bytes,
        origin_hint,
        matched_recipe_hash,
        exact_recipe_match,
        branch_name,
        restore_elapsed_ms,
        compile_duration_ms,
        estimated_transport_ms,
        report,
    } = log;
    let mib = size_bytes as f64 / 1024.0 / 1024.0;
    let origin = origin_hint.unwrap_or("none");
    let match_kind = if exact_recipe_match {
        "exact"
    } else {
        "fallback"
    };
    let matched = matched_recipe_hash
        .map(sha_abbrev)
        .unwrap_or_else(|| "unknown".to_string());
    let branch = branch_name.unwrap_or("unknown");
    eprintln!(
        "{}  sha256={}  size_bytes={size_bytes}  size={mib:.1} MiB  elapsed_ms={restore_elapsed_ms}  estimated_transport_ms={estimated_transport_ms}  compile_elapsed_ms={compile_duration_ms}  decision=restore  origin-hint={origin}  match={match_kind} recipe={matched} branch={branch}  (files +{} ={})",
        green_hydrate_prefix(),
        sha_abbrev(sha256),
        report.files_written,
        report.files_skipped,
    );
}

fn green_hydrate_prefix() -> String {
    crate::color_choice::paint_stderr(
        "soldr cook: auto-hydrate activated",
        crate::color_choice::GREEN,
    )
}

fn yellow_warning_prefix() -> String {
    crate::color_choice::paint_stderr("soldr cook: warning:", crate::color_choice::YELLOW)
}

/// Resolve auto-hydrate setting with the documented precedence.
pub fn auto_hydrate_enabled(manifest_dir: &Path, global: &CookConfig) -> bool {
    if let Some(val) = std::env::var_os(SOLDR_COOK_AUTO_HYDRATE_ENV) {
        let s = val.to_string_lossy();
        let trimmed = s.trim().to_ascii_lowercase();
        if !trimmed.is_empty() {
            return crate::core::flag_value(&trimmed);
        }
    }
    if let Ok(manifest) = read_rust_toolchain_manifest(manifest_dir) {
        if let Some(b) = manifest
            .soldr
            .and_then(|s| s.cook)
            .and_then(|c| c.auto_hydrate)
        {
            return b;
        }
    }
    global.auto_hydrate
}

fn resolve_target_triple(manifest_dir: &Path, args: &[String]) -> Option<String> {
    if let Some(value) = super::subcommand::cargo_args_target_value(args) {
        return Some(value);
    }
    TargetTriple::detect_in_dir(manifest_dir)
        .ok()
        .map(|t| t.to_string())
}

/// cargo profile NAME (the same string cargo passes through to
/// `[profile.<NAME>]`). Maps to a directory name via
/// [`profile_dir_name`].
fn resolve_profile_name(args: &[String]) -> String {
    if let Some(value) = extract_arg_value(args, "--profile") {
        return value;
    }
    if has_flag(args, "--release") {
        return "release".to_string();
    }
    "dev".to_string()
}

/// Map a cargo profile name to its target/ directory name. cargo
/// special-cases `dev` to `target/debug/`.
fn profile_dir_name(profile: &str) -> &str {
    if profile == "dev" {
        "debug"
    } else {
        profile
    }
}

/// Resolve the extraction root for a hydrated cook archive.
///
/// The packed archive (PR 2) records entries with the profile dir
/// as the leading component (`release/`, `debug/`, ...). With no
/// `--target`, that means the bare `target/` directory is the right
/// extraction root and the packed paths fall into place. But with
/// `--target X`, `soldr cook`'s pack source is `target/X/<profile>`
/// (see `resolve_cook_target_dir` in `cook.rs`), so the extraction
/// root must be `target/X` for the same entries to land where cargo
/// actually reads them (soldr#3043). We key this off the LITERAL
/// presence of `--target` in argv, not [`resolve_target_triple`],
/// because that is what determined the packer's source directory.
///
/// The one caller whose argv cannot carry `--target` is `soldr cook`'s
/// own Phase-1 `cargo chef prepare`; it announces the cook's target
/// scope through [`SOLDR_COOK_HYDRATE_TARGET_ENV`] instead, which
/// [`explicit_target_scope`] consults after the argv.
///
/// Residual asymmetry this does NOT fix: [`resolve_target_triple`]
/// (used to build the index LOOKUP key) falls back to the detected
/// host triple when `--target` is absent, so a plain `cargo build`
/// on a host machine can still match an archive that was cooked with
/// `--target <host>` and extract it at bare `target/` instead of
/// `target/<host>/`. For a plain `cargo build` that IS the directory
/// cargo reads, so the restore is still useful; closing the key/scope
/// mismatch properly needs an archive-format or index-schema decision,
/// not a fix here. Should be filed as a `research:` issue per
/// CLAUDE.md's Agent Code-Smell Reporting Rule.
fn resolve_target_dir(manifest_dir: &Path, args: &[String]) -> PathBuf {
    let root = if let Some(env_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        let p = PathBuf::from(&env_dir);
        if p.is_absolute() {
            p
        } else {
            manifest_dir.join(p)
        }
    } else {
        manifest_dir.join("target")
    };
    match explicit_target_scope(args) {
        Some(triple) => root.join(triple),
        None => root,
    }
}

/// The triple this invocation's artifacts are scoped under, or `None` for an
/// unscoped (host, no `--target`) build whose artifacts land directly in the
/// target root. argv wins; [`SOLDR_COOK_HYDRATE_TARGET_ENV`] is the fallback
/// for `soldr cook`'s own `cargo chef prepare` (soldr#3043).
/// Whether `soldr cook` has already cooked `target_dir` in place.
///
/// `target_dir` is the archive's extraction root (`target/<triple>/`); the
/// cook writes its marker one level down, in the profile directory it cooked
/// (`resolve_cook_target_dir` in cook.rs), so the check is scoped to the
/// profile this build resolves to.
fn target_already_cooked(target_dir: &Path, profile_dir: &str) -> bool {
    target_dir
        .join(profile_dir)
        .join(crate::cook::COOK_MARKER_FILE_NAME)
        .is_file()
}

fn explicit_target_scope(args: &[String]) -> Option<String> {
    if let Some(triple) = super::subcommand::cargo_args_target_value(args) {
        return Some(triple);
    }
    std::env::var(SOLDR_COOK_HYDRATE_TARGET_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `rustc -V` for the cook-index key. A failure is not silent: the hydrate
/// lookup degrades to a miss, but only after saying why (soldr#3381).
pub(crate) fn rustc_version_for_hydrate(rustc: &Path) -> Option<String> {
    let mut stderr = std::io::stderr();
    rustc_version_for_hydrate_with(rustc, &mut stderr, tool_output::default_log_path())
}

pub(crate) fn rustc_version_for_hydrate_with(
    rustc: &Path,
    warn: &mut dyn std::io::Write,
    log_path: Option<PathBuf>,
) -> Option<String> {
    let sinks = tool_output::ToolSinks {
        stderr: &mut *warn,
        log_path,
    };
    match tool_output::rustc_version_line_with_sinks(rustc, sinks) {
        Ok(version) => Some(version),
        Err(error) => {
            let _ = writeln!(
                warn,
                "soldr: warning: cook hydrate skipped; could not resolve rustc version: {error}"
            );
            None
        }
    }
}

fn extract_arg_value(args: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        // Program/test-harness args after `--` are not cargo's (soldr#3694).
        if arg == "--" {
            break;
        }
        if arg == flag {
            return iter.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::sync::Mutex;
    use tempfile::TempDir;

    // Env-mutating tests are serialized — `auto_hydrate_enabled`
    // reads `SOLDR_COOK_AUTO_HYDRATE` from the process environment.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// soldr#3694: `soldr cargo run -- --target foo` passes `--target foo`
    /// to the program; cook hydrate must not scope itself to target `foo`.
    #[test]
    fn target_after_double_dash_is_not_a_cargo_target() {
        let args: Vec<String> = ["run", "--", "--target", "foo"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let temp = TempDir::new().unwrap();
        assert_ne!(
            resolve_target_triple(temp.path(), &args).as_deref(),
            Some("foo")
        );
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_ne!(explicit_target_scope(&args).as_deref(), Some("foo"));
    }

    /// soldr#3381: a failing `rustc -V` must not silently turn a cook hit
    /// into a miss — the warning carries the fake rustc's stderr.
    #[test]
    fn hydrate_rustc_version_failure_warns_with_stderr() {
        let temp = TempDir::new().unwrap();
        let rustc =
            tool_output::write_fake_tool(temp.path(), "rustc", "", "MARKER_HYDRATE_3381", 1);
        let log = temp.path().join("small-tools.jsonl");
        let mut warn = Vec::new();
        let version = rustc_version_for_hydrate_with(&rustc, &mut warn, Some(log.clone()));
        assert!(version.is_none());
        let warn = String::from_utf8(warn).unwrap();
        assert!(warn.contains("cook hydrate skipped"), "{warn}");
        assert!(warn.contains("MARKER_HYDRATE_3381"), "{warn}");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("MARKER_HYDRATE_3381"), "{logged}");
    }

    /// soldr#3389 contract: a successful probe still forwards and logs stderr.
    #[test]
    fn hydrate_rustc_version_success_forwards_and_logs_stderr() {
        let temp = TempDir::new().unwrap();
        let rustc = tool_output::write_fake_tool(
            temp.path(),
            "rustc",
            "rustc 9.9.9",
            "MARKER_HYDRATE_OK",
            0,
        );
        let log = temp.path().join("small-tools.jsonl");
        let mut warn = Vec::new();
        let version = rustc_version_for_hydrate_with(&rustc, &mut warn, Some(log.clone()));
        assert_eq!(version.as_deref(), Some("rustc 9.9.9"));
        let warn = String::from_utf8(warn).unwrap();
        assert!(warn.contains("-V: MARKER_HYDRATE_OK"), "{warn}");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("MARKER_HYDRATE_OK"), "{logged}");
    }

    fn clear_env() {
        std::env::remove_var(SOLDR_COOK_AUTO_HYDRATE_ENV);
    }

    /// The env state every `resolve_target_dir` assertion below depends on:
    /// no `CARGO_TARGET_DIR` and no cook target scope, so the expected root is
    /// the tempdir's own `target/`. Not hypothetical — the documented Linux
    /// dev loop (`ci/perf_local.py`) exports `CARGO_TARGET_DIR=/target` into
    /// the runner container, and an inherited value moves the resolved root
    /// out of the tempdir and fails these tests for reasons unrelated to the
    /// behaviour they pin. Callers must already hold [`ENV_LOCK`].
    fn clear_target_dir_env() {
        std::env::remove_var("CARGO_TARGET_DIR");
        std::env::remove_var(SOLDR_COOK_HYDRATE_TARGET_ENV);
    }

    #[test]
    fn env_var_disables_overriding_everything() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.94.1\"\n[soldr.cook]\nauto_hydrate = true\n",
        )
        .unwrap();
        std::env::set_var(SOLDR_COOK_AUTO_HYDRATE_ENV, "0");
        let cfg = CookConfig {
            auto_hydrate: true,
            ..CookConfig::default()
        };
        assert!(!auto_hydrate_enabled(dir.path(), &cfg));
        clear_env();
    }

    #[test]
    fn env_var_enables_overriding_project_disable() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.94.1\"\n[soldr.cook]\nauto_hydrate = false\n",
        )
        .unwrap();
        std::env::set_var(SOLDR_COOK_AUTO_HYDRATE_ENV, "1");
        let cfg = CookConfig {
            auto_hydrate: false,
            ..CookConfig::default()
        };
        assert!(auto_hydrate_enabled(dir.path(), &cfg));
        clear_env();
    }

    #[test]
    fn rust_toolchain_disable_beats_global_enable() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_env();
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.94.1\"\n[soldr.cook]\nauto_hydrate = false\n",
        )
        .unwrap();
        let cfg = CookConfig {
            auto_hydrate: true,
            ..CookConfig::default()
        };
        assert!(!auto_hydrate_enabled(dir.path(), &cfg));
    }

    #[test]
    fn no_overrides_means_global_default_wins() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_env();
        let dir = TempDir::new().unwrap();
        let cfg = CookConfig::default();
        assert!(cfg.auto_hydrate, "global default must be ON");
        assert!(auto_hydrate_enabled(dir.path(), &cfg));
    }

    #[test]
    fn profile_resolution_matches_cargo_semantics() {
        assert_eq!(resolve_profile_name(&[]), "dev");
        assert_eq!(
            resolve_profile_name(&["build".into(), "--release".into()]),
            "release"
        );
        assert_eq!(
            resolve_profile_name(&["build".into(), "--profile".into(), "ci".into()]),
            "ci"
        );
        assert_eq!(
            resolve_profile_name(&["build".into(), "--profile=ci".into()]),
            "ci"
        );
        assert_eq!(profile_dir_name("dev"), "debug");
        assert_eq!(profile_dir_name("release"), "release");
        assert_eq!(profile_dir_name("ci"), "ci");
    }

    #[test]
    fn cook_cost_gate_restores_when_estimated_restore_is_cheaper() {
        // 500 MiB at the 16 MiB/s floor is ~31.3 s, against a two-minute compile.
        let archive: u64 = 500 * 1024 * 1024;
        let expected = archive.div_ceil(COOK_RESTORE_FLOOR_BYTES_PER_MS);
        assert!((31_000..32_000).contains(&expected), "{expected}");
        assert_eq!(
            decide_cook_restore(archive, 120_000),
            CookRestoreDecision::Restore {
                estimated_transport_ms: expected
            }
        );
    }

    #[test]
    fn cook_cost_gate_is_the_ci_shape_that_used_to_be_declined() {
        // soldr#3117: the CI stable tree -- 187 MiB archive, a compile the
        // compile cache served in 95 s, and a pack that took minutes. The old
        // estimate charged the pack twice over and declined; the restore
        // estimate is about 12 s and must win.
        let archive: u64 = 187 * 1024 * 1024;
        assert!(matches!(
            decide_cook_restore(archive, 95_000),
            CookRestoreDecision::Restore {
                estimated_transport_ms
            } if estimated_transport_ms < 15_000
        ));
    }

    #[test]
    fn cook_cost_gate_skips_when_transport_would_lose() {
        let archive: u64 = 500 * 1024 * 1024;
        let expected = archive.div_ceil(COOK_RESTORE_FLOOR_BYTES_PER_MS);
        assert_eq!(
            decide_cook_restore(archive, 9_000),
            CookRestoreDecision::Skip {
                estimated_transport_ms: Some(expected),
                reason: "estimated transport is not cheaper than the avoided compile",
            }
        );
    }

    #[test]
    fn cook_cost_gate_skips_without_prior_observations() {
        assert!(matches!(
            decide_cook_restore(1, 0),
            CookRestoreDecision::Skip {
                reason: "no prior compile duration is recorded",
                ..
            }
        ));
        assert!(matches!(
            decide_cook_restore(0, 5_000),
            CookRestoreDecision::Skip {
                reason: "archive has no recorded bytes",
                ..
            }
        ));
    }

    #[test]
    fn a_tree_cooked_in_place_needs_no_restore() {
        let dir = TempDir::new().unwrap();
        assert!(!target_already_cooked(dir.path(), "debug"));
        // The marker lives in the cooked profile directory, not the root.
        std::fs::write(dir.path().join(crate::cook::COOK_MARKER_FILE_NAME), b"{}").unwrap();
        assert!(!target_already_cooked(dir.path(), "debug"));
        std::fs::create_dir_all(dir.path().join("debug")).unwrap();
        std::fs::write(
            dir.path()
                .join("debug")
                .join(crate::cook::COOK_MARKER_FILE_NAME),
            b"{}",
        )
        .unwrap();
        assert!(target_already_cooked(dir.path(), "debug"));
        assert!(!target_already_cooked(dir.path(), "release"));
    }

    #[test]
    fn only_a_daemon_that_is_not_running_makes_the_lookup_pointless() {
        assert!(!lookup_worthwhile_from_status(Err(
            client::ClientError::NotRunning
        )));
        // soldr#3117: a status probe that timed out against a daemon still
        // warming up is not evidence of an empty index.
        assert!(lookup_worthwhile_from_status(Err(client::ClientError::Io(
            std::io::Error::from(std::io::ErrorKind::WouldBlock)
        ))));
        assert!(lookup_worthwhile_from_status(Err(
            client::ClientError::Protocol("garbled".into())
        )));
    }

    #[test]
    fn cook_performance_miss_is_an_honest_wall_time_comparison() {
        assert!(is_cook_performance_miss(10_000, 10_000));
        assert!(is_cook_performance_miss(10_001, 10_000));
        assert!(!is_cook_performance_miss(9_999, 10_000));
        assert!(!is_cook_performance_miss(10_000, 0));
    }

    #[test]
    fn resolve_target_dir_without_explicit_target_is_the_bare_target_dir() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_target_dir_env();
        let dir = TempDir::new().unwrap();
        let args = [
            "chef".to_string(),
            "cook".to_string(),
            "--recipe-path".to_string(),
            "r.json".to_string(),
            "--workspace".to_string(),
        ];
        let expected = dir.path().join("target");
        assert_eq!(resolve_target_dir(dir.path(), &args), expected);
    }

    #[test]
    fn resolve_target_dir_appends_the_explicit_target_triple() {
        // Serialized with the CARGO_TARGET_DIR-mutating test below: that one
        // sets a process-global var this assertion depends on being unset.
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_target_dir_env();
        let dir = TempDir::new().unwrap();
        let args = [
            "build".to_string(),
            "--target".to_string(),
            "x86_64-unknown-linux-gnu".to_string(),
        ];
        let expected = dir.path().join("target").join("x86_64-unknown-linux-gnu");
        assert_eq!(resolve_target_dir(dir.path(), &args), expected);
    }

    #[test]
    fn resolve_target_dir_accepts_the_equals_spelling() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_target_dir_env();
        let dir = TempDir::new().unwrap();
        let args = [
            "build".to_string(),
            "--target=aarch64-unknown-linux-gnu".to_string(),
        ];
        let expected = dir.path().join("target").join("aarch64-unknown-linux-gnu");
        assert_eq!(resolve_target_dir(dir.path(), &args), expected);
    }

    #[test]
    fn resolve_target_dir_honours_cargo_target_dir_with_an_explicit_target() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let manifest_dir = TempDir::new().unwrap();
        let target_dir = TempDir::new().unwrap();
        std::env::set_var("CARGO_TARGET_DIR", target_dir.path());
        let args = [
            "build".to_string(),
            "--target".to_string(),
            "x86_64-unknown-linux-gnu".to_string(),
        ];
        let resolved = resolve_target_dir(manifest_dir.path(), &args);
        std::env::remove_var("CARGO_TARGET_DIR");
        let expected = target_dir.path().join("x86_64-unknown-linux-gnu");
        assert_eq!(resolved, expected);
    }

    // soldr#3043: `cargo chef prepare` (soldr cook's Phase 1) carries no
    // `--target`, so without this fallback the hydrate would extract a
    // `--target`-scoped archive to the bare `target/` root — a duplicate
    // extraction Cargo never reads, with the warm-cook marker landing outside
    // `resolve_cook_target_dir` so soldr#621's short-circuit can never fire.
    #[test]
    fn resolve_target_dir_falls_back_to_the_cook_target_scope_env() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_target_dir_env();
        let dir = TempDir::new().unwrap();
        let args = [
            "chef".to_string(),
            "prepare".to_string(),
            "--recipe-path".to_string(),
            "r.json".to_string(),
        ];
        std::env::set_var(SOLDR_COOK_HYDRATE_TARGET_ENV, "aarch64-apple-darwin");
        let resolved = resolve_target_dir(dir.path(), &args);
        std::env::remove_var(SOLDR_COOK_HYDRATE_TARGET_ENV);
        let expected = dir.path().join("target").join("aarch64-apple-darwin");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn an_explicit_target_argv_beats_the_cook_target_scope_env() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_target_dir_env();
        let dir = TempDir::new().unwrap();
        let args = [
            "chef".to_string(),
            "cook".to_string(),
            "--target".to_string(),
            "x86_64-unknown-linux-gnu".to_string(),
        ];
        std::env::set_var(SOLDR_COOK_HYDRATE_TARGET_ENV, "aarch64-apple-darwin");
        let resolved = resolve_target_dir(dir.path(), &args);
        std::env::remove_var(SOLDR_COOK_HYDRATE_TARGET_ENV);
        let expected = dir.path().join("target").join("x86_64-unknown-linux-gnu");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn a_blank_cook_target_scope_env_is_ignored() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_target_dir_env();
        let dir = TempDir::new().unwrap();
        let args = ["chef".to_string(), "prepare".to_string()];
        std::env::set_var(SOLDR_COOK_HYDRATE_TARGET_ENV, "   ");
        let resolved = resolve_target_dir(dir.path(), &args);
        std::env::remove_var(SOLDR_COOK_HYDRATE_TARGET_ENV);
        assert_eq!(resolved, dir.path().join("target"));
    }

    #[test]
    fn cook_restore_elapsed_includes_archive_verification() {
        let (outcome, elapsed_ms) = timed_verified_extract(
            || {
                std::thread::sleep(std::time::Duration::from_millis(25));
                Ok::<_, ()>(true)
            },
            || Some(()),
        );

        assert!(matches!(outcome, TimedRestoreOutcome::Restored(())));
        assert!(
            elapsed_ms >= 20,
            "verification delay was excluded from restore elapsed time: {elapsed_ms} ms"
        );
    }
}
