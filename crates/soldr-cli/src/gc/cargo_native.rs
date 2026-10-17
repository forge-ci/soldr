//! `soldr gc cargo`, `soldr gc locations`, `soldr gc sweep`.
//!
//! Wraps nightly cargo's `-Zgc clean gc` (see `invoke_cargo_native_gc`)
//! and orchestrates a full sweep that pairs cargo's own GC with
//! soldr's workspace-target purge over registered targets. Also owns
//! the read-only `gc locations` enumeration used by the doctor / UI.

use crate::cache::print_json;
use crate::core::{SoldrError, SoldrPaths};
use crate::{GcCargoArgs, GcSweepArgs, JSON_SCHEMA_VERSION, SOLDR_GC_CARGO_TOOLCHAIN_ENV_VAR};
use serde::Serialize;

use super::purge::{resolve_gc_dev_roots, run_gc_purge_candidates};
use super::walks::fast_directory_size_and_files;

#[derive(Serialize)]
pub(super) struct GcCargoOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    toolchain: String,
    pub(super) exit_code: i32,
    dry_run: bool,
    args: Vec<String>,
    stdout_bytes: u64,
    stderr_bytes: u64,
    pub(super) skipped: bool,
    pub(super) skipped_reason: Option<String>,
}

#[derive(Serialize)]
struct GcLocationOutput {
    kind: &'static str,
    path: String,
    exists: bool,
    size_bytes: u64,
    size_human: String,
    file_count: u64,
    owner: &'static str,
    purge_safety: &'static str,
}

#[derive(Serialize)]
struct GcLocationsOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    locations: Vec<GcLocationOutput>,
    total_size_bytes: u64,
    total_size_human: String,
}

#[derive(Serialize)]
struct GcSweepOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    dry_run: bool,
    cargo_gc: Option<GcCargoOutput>,
    cargo_gc_aggressive: Option<GcCargoOutput>,
    soldr_targets: Option<SoldrTargetsSummary>,
    locations: Vec<GcLocationOutput>,
    elapsed_ms: u128,
}

#[derive(Serialize, Default)]
struct SoldrTargetsSummary {
    selected_count: usize,
    succeeded_count: usize,
    failed_count: usize,
    reclaimed_bytes: u64,
    reclaimed_human: String,
}

/// `soldr gc cargo` — shell out to nightly cargo's `-Zgc clean gc`.
pub(crate) fn run_gc_cargo_command(args: GcCargoArgs) -> Result<(), SoldrError> {
    let outcome = invoke_cargo_native_gc(&args, false)?;
    if args.json {
        print_json(&outcome)?;
    }
    if outcome.skipped {
        // Explicit gc cargo treats a missing nightly as a hard error,
        // unlike gc sweep which downgrades the missing toolchain to a
        // skip. We surfaced the skip JSON above for callers that care,
        // but the exit code must reflect the failure.
        return Err(SoldrError::Other(
            outcome
                .skipped_reason
                .unwrap_or_else(|| "cargo nightly GC unavailable".into()),
        ));
    }
    if outcome.exit_code != 0 {
        std::process::exit(outcome.exit_code);
    }
    Ok(())
}

/// Common implementation backing `gc cargo` and the cargo-step of
/// `gc sweep`. When `skip_when_missing` is true (sweep), a missing
/// nightly toolchain returns a `skipped = true` outcome with
/// `exit_code = 0` so the orchestrator can continue.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(super) fn invoke_cargo_native_gc(
    args: &GcCargoArgs,
    skip_when_missing: bool,
) -> Result<GcCargoOutput, SoldrError> {
    let toolchain = resolve_gc_cargo_toolchain(args.toolchain.as_deref());

    let mut forwarded: Vec<String> = Vec::new();
    push_optional_flag(&mut forwarded, "--max-src-age", args.max_src_age.as_deref());
    push_optional_flag(
        &mut forwarded,
        "--max-crate-age",
        args.max_crate_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-index-age",
        args.max_index_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-git-co-age",
        args.max_git_co_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-git-db-age",
        args.max_git_db_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-download-age",
        args.max_download_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-src-size",
        args.max_src_size.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-crate-size",
        args.max_crate_size.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-git-size",
        args.max_git_size.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-download-size",
        args.max_download_size.as_deref(),
    );
    if args.dry_run {
        forwarded.push("--dry-run".to_string());
    }

    // Final `cargo` argv: -Zgc clean gc [forwarded...]
    let mut cargo_argv: Vec<String> =
        vec!["-Zgc".to_string(), "clean".to_string(), "gc".to_string()];
    cargo_argv.extend(forwarded.iter().cloned());

    // We invoke via `rustup run <toolchain> cargo ...` so the
    // workspace's rust-toolchain.toml override does not silently win.
    if !rustup_run_available_for(&toolchain) {
        if skip_when_missing {
            return Ok(GcCargoOutput {
                schema_version: JSON_SCHEMA_VERSION,
                command: "gc",
                mode: "cargo",
                toolchain: toolchain.clone(),
                exit_code: 0,
                dry_run: args.dry_run,
                args: cargo_argv,
                stdout_bytes: 0,
                stderr_bytes: 0,
                skipped: true,
                skipped_reason: Some(format!(
                    "rustup toolchain {toolchain} not installed; skipping cargo GC"
                )),
            });
        }
        return Err(SoldrError::Other(format!(
            "rustup toolchain {toolchain} not installed; install it with `rustup toolchain install {toolchain}` or pass --toolchain <name>"
        )));
    }

    let mut command = std::process::Command::new("rustup");
    command.arg("run").arg(&toolchain).arg("cargo");
    command.args(&cargo_argv);
    crate::core::suppress_windows_console_window(&mut command);

    eprintln!(
        "soldr gc cargo: rustup run {toolchain} cargo {}",
        cargo_argv.join(" ")
    );

    let output = crate::platform::wait::output_for(&mut command, crate::platform::wait::LONG_CHILD)
        .map_err(|e| {
            SoldrError::Other(format!(
                "failed to invoke rustup run {toolchain} cargo: {e}"
            ))
        })?;

    // Stream cargo's output through to the user's terminal.
    use std::io::Write as _;
    let _ = std::io::stdout().write_all(&output.stdout);
    let _ = std::io::stderr().write_all(&output.stderr);

    Ok(GcCargoOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "gc",
        mode: "cargo",
        toolchain,
        exit_code: output.status.code().unwrap_or(1),
        dry_run: args.dry_run,
        args: cargo_argv,
        stdout_bytes: output.stdout.len() as u64,
        stderr_bytes: output.stderr.len() as u64,
        skipped: false,
        skipped_reason: None,
    })
}

fn push_optional_flag(out: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(v) = value {
        out.push(format!("{flag}={v}"));
    }
}

fn resolve_gc_cargo_toolchain(flag: Option<&str>) -> String {
    flag.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var(SOLDR_GC_CARGO_TOOLCHAIN_ENV_VAR)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "nightly".to_string())
}

/// Best-effort probe: `rustup toolchain list` must list the supplied
/// channel. We avoid actually shelling into the toolchain here because
/// `rustup run <missing> cargo --version` will install on demand,
/// which we don't want for a probe.
fn rustup_run_available_for(toolchain: &str) -> bool {
    let mut command = std::process::Command::new("rustup");
    command.args(["toolchain", "list"]);
    crate::core::suppress_windows_console_window(&mut command);
    let output = match crate::platform::wait::output_for(&mut command, crate::platform::wait::PROBE)
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .any(|line| line.trim().starts_with(toolchain))
}

/// `soldr gc locations` — read-only enumeration of every cache dir
/// soldr cares about, with sizes (no last-used derivation yet).
pub(crate) fn run_gc_locations_command(json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let entries = enumerate_cache_locations(&paths);
    let total_size_bytes: u64 = entries.iter().map(|e| e.size_bytes).sum();

    if json {
        let output = GcLocationsOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "gc",
            mode: "locations",
            total_size_bytes,
            total_size_human: crate::cache_lib::target_registry::human_size(total_size_bytes),
            locations: entries,
        };
        print_json(&output)?;
    } else {
        println!("soldr gc locations:");
        println!(
            "  total tracked size: {}",
            crate::cache_lib::target_registry::human_size(total_size_bytes)
        );
        for entry in &entries {
            println!(
                "  [{:>8}] {}  size={}  files={}  owner={}  purge_safety={}",
                entry.kind,
                entry.path,
                entry.size_human,
                entry.file_count,
                entry.owner,
                entry.purge_safety,
            );
        }
    }
    Ok(())
}

/// Enumerate every directory soldr cares about: cargo home subdirs,
/// rustup home subdirs, soldr's own cache root, and the state.sqlite3
/// file. Missing paths are reported with `exists = false` and zero
/// size so the JSON shape stays predictable.
fn enumerate_cache_locations(paths: &SoldrPaths) -> Vec<GcLocationOutput> {
    let mut entries: Vec<GcLocationOutput> = Vec::new();

    if let Some(cargo_home) = crate::core::resolve_cargo_home() {
        for (kind, suffix, owner, purge_safety) in &[
            ("cargo_registry_src", "registry/src", "cargo", "regenerable"),
            (
                "cargo_registry_cache",
                "registry/cache",
                "cargo",
                "regenerable",
            ),
            (
                "cargo_registry_index",
                "registry/index",
                "cargo",
                "regenerable",
            ),
            ("cargo_git_db", "git/db", "cargo", "regenerable"),
            (
                "cargo_git_checkouts",
                "git/checkouts",
                "cargo",
                "regenerable",
            ),
        ] {
            let path = cargo_home.join(suffix);
            entries.push(gc_location_for(kind, &path, owner, purge_safety));
        }
        // .global-cache is a single file in cargo's stable tree.
        let global_cache = cargo_home.join(".global-cache");
        entries.push(gc_location_for(
            "cargo_global_cache",
            &global_cache,
            "cargo",
            "regenerable",
        ));
    }

    if let Some(rustup_home) = crate::core::resolve_rustup_home() {
        entries.push(gc_location_for(
            "rustup_toolchains",
            &rustup_home.join("toolchains"),
            "rustup",
            "user_action",
        ));
        entries.push(gc_location_for(
            "rustup_update_hashes",
            &rustup_home.join("update-hashes"),
            "rustup",
            "regenerable",
        ));
    }

    entries.push(gc_location_for(
        "soldr_cache",
        &paths.cache,
        "soldr",
        "regenerable",
    ));
    entries.push(gc_location_for(
        "soldr_state_db",
        &paths.root.join("state.sqlite3"),
        "soldr",
        "user_action",
    ));

    entries
}

fn gc_location_for(
    kind: &'static str,
    path: &std::path::Path,
    owner: &'static str,
    purge_safety: &'static str,
) -> GcLocationOutput {
    let exists = path.exists();
    let (size_bytes, file_count) = if exists {
        fast_directory_size_and_files(path)
    } else {
        (0, 0)
    };
    GcLocationOutput {
        kind,
        path: path.display().to_string(),
        exists,
        size_bytes,
        size_human: crate::cache_lib::target_registry::human_size(size_bytes),
        file_count,
        owner,
        purge_safety,
    }
}

/// `soldr gc sweep` — orchestrate locations + cargo gc + soldr target
/// purge in one go.
pub(crate) fn run_gc_sweep_command(args: GcSweepArgs) -> Result<(), SoldrError> {
    let start = std::time::Instant::now();
    let paths = SoldrPaths::new()?;
    let cargo_gc_enabled = !args.no_cargo_gc;

    // 1. Locations table (always — read-only).
    let locations = enumerate_cache_locations(&paths);

    // 2. Cargo's clean gc with conservative ages (unless disabled).
    let cargo_gc_outcome = if cargo_gc_enabled {
        // Use conservative defaults — let cargo's own ~1mo / ~3mo
        // policy decide. We don't pass any --max-*-age flags so the
        // user can configure cargo independently.
        let cargo_args = GcCargoArgs {
            dry_run: args.dry_run,
            toolchain: None,
            max_src_age: None,
            max_crate_age: None,
            max_index_age: None,
            max_git_co_age: None,
            max_git_db_age: None,
            max_download_age: None,
            max_src_size: None,
            max_crate_size: None,
            max_git_size: None,
            max_download_size: None,
            json: args.json,
        };
        Some(invoke_cargo_native_gc(&cargo_args, true)?)
    } else {
        None
    };

    // 3. soldr's target purge over registered workspaces.
    let soldr_targets = if args.dry_run {
        if !args.json {
            eprintln!("soldr gc sweep: dry-run; skipping soldr target purge");
        }
        None
    } else {
        Some(run_soldr_target_purge_for_sweep(
            &paths, args.all, args.json,
        )?)
    };

    // 4. Aggressive second cargo pass.
    let cargo_gc_aggressive = if args.aggressive && cargo_gc_enabled {
        let cfg = paths
            .load_config()
            .map_err(|error| SoldrError::Other(error.to_string()))?;
        let floor = cfg.auto_gc.min_age_secs;
        let aggressive_args = aggressive_cargo_args(args.json, args.dry_run, floor);
        Some(invoke_cargo_native_gc(&aggressive_args, true)?)
    } else {
        None
    };

    if !args.json {
        eprintln!("soldr gc sweep: done in {} ms", start.elapsed().as_millis());
    }

    if args.json {
        let output = GcSweepOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "gc",
            mode: "sweep",
            dry_run: args.dry_run,
            cargo_gc: cargo_gc_outcome,
            cargo_gc_aggressive,
            soldr_targets,
            locations,
            elapsed_ms: start.elapsed().as_millis(),
        };
        print_json(&output)?;
    }
    Ok(())
}

fn aggressive_cargo_args(json: bool, dry_run: bool, min_age_secs: u64) -> GcCargoArgs {
    // Helper: clamp `aggressive_days * 86_400` to the configured min
    // age, then serialize it using Cargo's accepted duration grammar.
    let clamp = |days: u64| -> String {
        let secs = crate::cache_lib::auto_gc::clamp_age_to_floor(
            days.saturating_mul(86_400),
            min_age_secs,
        );
        crate::cache_lib::auto_gc::cargo_gc_duration_arg(secs)
    };
    GcCargoArgs {
        dry_run,
        toolchain: None,
        max_src_age: Some(clamp(7)),
        max_crate_age: Some(clamp(14)),
        max_index_age: None,
        max_git_co_age: Some(clamp(7)),
        max_git_db_age: None,
        max_download_age: None,
        max_src_size: None,
        max_crate_size: None,
        max_git_size: None,
        max_download_size: None,
        json,
    }
}

fn run_soldr_target_purge_for_sweep(
    _paths: &SoldrPaths,
    purge_all: bool,
    json: bool,
) -> Result<SoldrTargetsSummary, SoldrError> {
    use crate::cache_lib::gc::{parse_duration, parse_size, GcOptions};
    let paths = SoldrPaths::new()?;
    let dev_roots = resolve_gc_dev_roots(&paths)?;
    let cfg = paths
        .load_config()
        .map_err(|error| SoldrError::Other(error.to_string()))?;
    let older_than_seconds = crate::cache_lib::auto_gc::clamp_age_to_floor(
        parse_duration("10d").map_err(SoldrError::Other)?,
        cfg.auto_gc.min_age_secs,
    );
    let larger_than_bytes = parse_size("256M").map_err(SoldrError::Other)?;

    let options = GcOptions {
        older_than_seconds,
        larger_than_bytes,
        dev_roots,
        dry_run: false,
    };
    // Snapshot-then-release, same as the `soldr gc` front door (#1681).
    let report = super::daemon_gc_scan(&paths, &options)?;
    if report.candidates.is_empty() {
        return Ok(SoldrTargetsSummary::default());
    }
    let purge_summary = run_gc_purge_candidates(&paths, &report.candidates, purge_all, json)?;
    Ok(SoldrTargetsSummary {
        selected_count: purge_summary.selected_count,
        succeeded_count: purge_summary.succeeded_count,
        failed_count: purge_summary.failed_count,
        reclaimed_bytes: purge_summary.reclaimed_bytes,
        reclaimed_human: crate::cache_lib::target_registry::human_size(
            purge_summary.reclaimed_bytes,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggressive_cargo_gc_uses_cargo_accepted_duration_syntax() {
        let args = aggressive_cargo_args(false, true, 0);
        assert_eq!(args.max_src_age.as_deref(), Some("604800 seconds"));
        assert_eq!(args.max_crate_age.as_deref(), Some("1209600 seconds"));
        assert_eq!(args.max_git_co_age.as_deref(), Some("604800 seconds"));
    }
}
