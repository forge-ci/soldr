//! Status, cache inspection, cache prune-target, version, and cache-clearing
//! commands. Extracted from `main.rs` as part of issue #339, then split into
//! submodules to keep individual files under 1000 LOC.
//!
//! The public surface — anything reachable as `crate::cache::*` — is
//! re-exported from this module so callers outside the cache module do not
//! need to know about the submodule layout.

use crate::core::{SoldrError, SoldrPaths};
use crate::zccache::managed_zccache_cache_dir;
use crate::JSON_SCHEMA_VERSION;
use serde::Serialize;

/// Version of the compiled-in embedded zccache library (soldr#1368).
/// Replaces the deleted managed-download version constant.
pub(crate) const EMBEDDED_ZCCACHE_VERSION: &str = zccache::core::VERSION;

mod release_worktree;
mod report;
mod session;
mod store_budget;
mod trim;

pub(super) use crate::build_cache_session::{output_snippet, ANALYZE_NOTE_LIMIT};
pub(crate) use release_worktree::{
    run_cache_release_worktree_command, run_cache_sweep_trash_command, sweep_trash, SweepReport,
};
pub(crate) use report::run_cache_report_command;
pub(crate) use session::{
    capture_build_baseline, compilations_since_baseline, finalize_build_session_stats,
    run_cache_flush_command, run_cache_shutdown_command, run_session_end_command,
    run_session_start_command,
};
pub(crate) use store_budget::{run_cache_gc_command, run_cache_stats_command};
pub(crate) use trim::{run_cache_prune_target_command, run_cache_trim_target_command, TrimProfile};

pub(crate) fn clear_zccache_cache() -> Result<(), SoldrError> {
    // soldr#1368: no externally-resolved zccache binary to run
    // `zccache clear` against — compile caching lives in the soldr-daemon
    // embedded service. `soldr cache clean` now just removes soldr's own
    // on-disk zccache state directory.
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;

    if zccache_dir.exists() {
        std::fs::remove_dir_all(&zccache_dir)?;
        println!("removed soldr zccache state dir: {}", zccache_dir.display());
    } else {
        println!(
            "soldr zccache state dir already empty: {}",
            zccache_dir.display()
        );
    }
    Ok(())
}

pub(crate) fn purge_soldr_cache() -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let mut purged_anything = false;

    purged_anything |= remove_soldr_artifact_dir("cache", &paths.cache)?;
    purged_anything |= remove_soldr_artifact_dir("bin", &paths.bin)?;

    if !purged_anything {
        println!("soldr cache is already empty: {}", paths.root.display());
    }

    Ok(())
}

fn remove_soldr_artifact_dir(label: &str, path: &std::path::Path) -> Result<bool, SoldrError> {
    if !path.exists() {
        return Ok(false);
    }

    if std::fs::symlink_metadata(path)?.file_type().is_dir() {
        std::fs::remove_dir_all(path)?;
        println!("removed soldr {label} dir: {}", path.display());
    } else {
        std::fs::remove_file(path)?;
        println!("removed soldr {label} entry: {}", path.display());
    }
    Ok(true)
}

#[derive(Serialize)]
pub(crate) struct VersionOutput {
    schema_version: u32,
    command: &'static str,
    pub(crate) soldr_version: String,
    root_dir: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct StatusOutput {
    schema_version: u32,
    command: &'static str,
    soldr_version: String,
    target: String,
    root_dir: String,
    cache_dir: String,
    cache_default_enabled: bool,
    cache_enabled_for_invocation: bool,
    managed_zccache_version: &'static str,
    zccache: ZccacheStatusSnapshot,
    maintenance: Option<crate::daemon::maintenance::MaintenanceStatus>,
    /// Rollup of compile-daemon fallback events -- builds that ran
    /// uncached via direct rustc (soldr#1838 Phase 4). Empty means the
    /// cache was never bypassed. Mirrors the `soldr doctor` field.
    fallbacks: crate::compile_fallback_rollup::FallbackRollup,
}

#[derive(Serialize)]
pub(crate) struct CacheOutput {
    schema_version: u32,
    command: &'static str,
    soldr_version: String,
    managed_zccache_version: &'static str,
    zccache: ZccacheStatusSnapshot,
    maintenance: Option<crate::daemon::maintenance::MaintenanceStatus>,
}

#[derive(Serialize)]
pub(super) struct ZccacheStatusSnapshot {
    cache_dir: String,
    state_dir: String,
    session_log_path: String,
    session_log_present: bool,
    journal_path: String,
    journal_present: bool,
    session_stats_path: String,
    session_stats_present: bool,
    binary_path: Option<String>,
    /// Which precedence chain branch produced `binary_path`:
    /// `"pinned"`, `"local"`, `"managed"`, or `"none"`. Surfaced so
    /// `soldr cache` / `soldr status` consumers don't have to derive
    /// it from the path (issue #420).
    binary_source: &'static str,
    binary_fetched: bool,
    status_lines: Vec<String>,
    status_empty: bool,
}

pub(crate) fn version_output() -> VersionOutput {
    VersionOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "version",
        soldr_version: crate::core::version().to_string(),
        root_dir: SoldrPaths::new()
            .ok()
            .map(|paths| paths.root.display().to_string()),
    }
}

pub(crate) fn collect_status_output(cache_enabled: bool) -> Result<StatusOutput, SoldrError> {
    let target = crate::core::TargetTriple::detect()?;
    let paths = SoldrPaths::new()?;
    Ok(StatusOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "status",
        soldr_version: crate::core::version().to_string(),
        target: target.to_string(),
        root_dir: paths.root.display().to_string(),
        cache_dir: paths.cache.display().to_string(),
        cache_default_enabled: true,
        cache_enabled_for_invocation: cache_enabled,
        managed_zccache_version: EMBEDDED_ZCCACHE_VERSION,
        zccache: collect_zccache_status(&paths)?,
        maintenance: crate::daemon::maintenance::read_status(&paths),
        fallbacks: crate::compile_fallback_rollup::collect(
            &paths,
            crate::compile_fallback_rollup::DOCTOR_RECENT_LIMIT,
        ),
    })
}

pub(crate) fn collect_cache_output() -> Result<CacheOutput, SoldrError> {
    let paths = SoldrPaths::new()?;
    Ok(CacheOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache",
        soldr_version: crate::core::version().to_string(),
        managed_zccache_version: EMBEDDED_ZCCACHE_VERSION,
        zccache: collect_zccache_status(&paths)?,
        maintenance: crate::daemon::maintenance::read_status(&paths),
    })
}

pub(super) fn collect_zccache_status(
    paths: &SoldrPaths,
) -> Result<ZccacheStatusSnapshot, SoldrError> {
    let zccache_dir = managed_zccache_cache_dir(paths)?;
    let session_log_path = crate::cache_lib::session_log_path(&zccache_dir);
    let session_log_present = session_log_path.exists();
    let journal_path = crate::cache_lib::session_journal_path(&zccache_dir);
    let journal_present = journal_path.exists();
    let session_stats_path = crate::cache_lib::session_stats_path(&zccache_dir);
    let session_stats_present = session_stats_path.exists();

    // soldr#1593: zccache is linked into soldr and has no standalone
    // runtime binary. Keep the nullable reporting fields for JSON schema
    // compatibility without materializing a fake executable alias.
    Ok(ZccacheStatusSnapshot {
        cache_dir: zccache_dir.display().to_string(),
        state_dir: zccache_dir.display().to_string(),
        session_log_path: session_log_path.display().to_string(),
        session_log_present,
        journal_path: journal_path.display().to_string(),
        journal_present,
        session_stats_path: session_stats_path.display().to_string(),
        session_stats_present,
        binary_path: None,
        binary_source: "in-process",
        binary_fetched: false,
        status_lines: Vec::new(),
        status_empty: true,
    })
}

pub(crate) fn print_status_output(output: &StatusOutput) {
    println!("soldr {}", output.soldr_version);
    println!("target: {}", output.target);
    println!("root dir: {}", output.root_dir);
    println!("cache dir: {}", output.cache_dir);
    println!("cache default: enabled");
    println!(
        "cache mode: {}",
        if output.cache_enabled_for_invocation {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("zccache version: {}", output.managed_zccache_version);
    print_zccache_status_snapshot(&output.zccache);
    print_maintenance_status(output.maintenance.as_ref());
    crate::compile_fallback_rollup::print_section(&output.fallbacks);
}

pub(crate) fn print_cache_output(output: &CacheOutput) {
    print_zccache_status_snapshot(&output.zccache);
    print_maintenance_status(output.maintenance.as_ref());
}

pub(crate) fn print_maintenance_status(
    status: Option<&crate::daemon::maintenance::MaintenanceStatus>,
) {
    println!();
    println!("cache maintenance:");
    let Some(status) = status else {
        println!("  no daemon maintenance pass recorded yet");
        return;
    };
    println!("  owning root:    {}", status.owning_root);
    println!("  daemon identity: {}", status.daemon_identity);
    println!("  embedded root:  {}", status.embedded_cache_root);
    println!("  budget policy:  {}", status.disk_policy.source);
    if let Some(report) = status.zccache.as_ref() {
        let fill = if report.budget_bytes == 0 {
            0.0
        } else {
            report.usage_after_bytes as f64 * 100.0 / report.budget_bytes as f64
        };
        println!(
            "  zccache:        pressure={} budget={} bytes usage={} bytes ({fill:.1}%) reclaimed={} bytes / {} items",
            report.pressure,
            report.budget_bytes,
            report.usage_after_bytes,
            report.bytes_reclaimed,
            report.artifacts_removed,
        );
        if let Some(measured_at) = status
            .zccache_measured_at_ms
            .filter(|measured_at| *measured_at != status.attempted_at_ms)
        {
            // soldr#3077: a deferred pass cannot reach the store, so this is
            // the last pass that did.
            println!("  zccache measured: {measured_at} ms since epoch (last pass that reached the store)");
        }
    } else if let Some(error) = status.zccache_error.as_ref() {
        println!("  zccache:        failed ({error})");
    }
    if let (Some(free), Some(percent)) =
        (status.filesystem_free_bytes, status.filesystem_free_percent)
    {
        println!("  filesystem:     free={free} bytes ({percent:.1}%)");
    }
    println!(
        "  schedule:       pressure every 5m; full age pass every 24h; 72h pressure / 30d absolute expiry (embedded zccache: 4d pressure)"
    );
    println!(
        "  last attempted: {} ms since epoch",
        status.attempted_at_ms
    );
    match status.successful_at_ms {
        Some(value) => println!("  last successful: {value} ms since epoch"),
        None => println!(
            "  deferred:       {}",
            status.deferred_reason.as_deref().unwrap_or("pass failed")
        ),
    }
    for (name, component) in [
        ("cook", &status.cook),
        ("history", &status.history),
        ("pep517 targets", &status.pep517_targets),
        ("pep517 wheels", &status.pep517_wheels),
        ("trash", &status.trash),
        ("workspace targets", &status.workspace_targets),
        ("daemon events", &status.daemon_events),
        ("legacy zccache", &status.legacy_zccache),
    ] {
        if let Some(error) = component.error.as_deref() {
            println!("  {name} error: {error}");
        }
    }
}

fn print_zccache_status_snapshot(snapshot: &ZccacheStatusSnapshot) {
    println!("soldr zccache cache dir: {}", snapshot.cache_dir);
    println!("soldr zccache state dir: {}", snapshot.state_dir);
    println!(
        "last session log: {} ({})",
        snapshot.session_log_path,
        if snapshot.session_log_present {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "last session journal: {} ({})",
        snapshot.journal_path,
        if snapshot.journal_present {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "last session stats: {} ({})",
        snapshot.session_stats_path,
        if snapshot.session_stats_present {
            "present"
        } else {
            "missing"
        }
    );

    if let Some(binary_path) = &snapshot.binary_path {
        println!(
            "zccache binary: {binary_path} (source: {})",
            snapshot.binary_source
        );
        if snapshot.binary_source == "embedded" {
            println!(
                "zccache status: embedded in soldr-daemon; use `soldr daemon status` for live daemon health"
            );
        } else if snapshot.status_empty {
            println!("zccache status: no output");
        } else {
            for line in &snapshot.status_lines {
                println!("zccache: {line}");
            }
        }
    } else {
        println!("zccache runtime: in-process (compiled into soldr)");
        println!(
            "zccache status: embedded in soldr-daemon; use `soldr daemon status` for live daemon health"
        );
    }
}

pub(crate) fn print_json<T: Serialize>(value: &T) -> Result<(), SoldrError> {
    serde_json::to_writer_pretty(std::io::stdout(), value)
        .map_err(|e| SoldrError::Other(format!("failed to serialize JSON output: {e}")))?;
    println!();
    Ok(())
}
