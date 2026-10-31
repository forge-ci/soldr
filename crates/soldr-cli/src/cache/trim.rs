//! `soldr cache prune-target` and `soldr cache trim-target` — pruning and
//! archival prep for cargo `target/` directories.

use crate::core::SoldrError;
use crate::JSON_SCHEMA_VERSION;
use serde::Serialize;

use super::print_json;

pub(crate) fn run_cache_prune_target_command(
    target_dir: std::path::PathBuf,
    dry_run: bool,
    keep_latest: bool,
    json: bool,
) -> Result<(), SoldrError> {
    let canonical = std::path::absolute(&target_dir).unwrap_or_else(|_| target_dir.clone());
    let opts = crate::cache_lib::prune_target::PruneTargetOptions {
        target_dir: canonical.clone(),
        dry_run,
        keep_latest,
    };
    let report = crate::cache_lib::prune_target::prune_target(&opts)
        .map_err(|e| SoldrError::Other(format!("cache prune-target failed: {e}")))?;

    if json {
        let output = build_cache_prune_target_output(&canonical, dry_run, keep_latest, &report);
        print_json(&output)?;
    } else {
        print_cache_prune_target_text(&canonical, dry_run, keep_latest, &report);
    }
    Ok(())
}

fn build_cache_prune_target_output(
    target_dir: &std::path::Path,
    dry_run: bool,
    keep_latest: bool,
    report: &crate::cache_lib::prune_target::PruneTargetReport,
) -> CachePruneTargetOutput {
    CachePruneTargetOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache prune-target",
        target_dir: target_dir.display().to_string(),
        dry_run,
        keep_latest,
        scanned: report.scanned,
        kept: report.kept,
        deleted: report.deleted,
        reclaimed_bytes: report.reclaimed_bytes,
        reclaimed_human: crate::cache_lib::target_registry::human_size(report.reclaimed_bytes),
        keep_decisions_from_fingerprint: report.keep_decisions_from_fingerprint,
        keep_decisions_from_mtime: report.keep_decisions_from_mtime,
        entries: report
            .entries
            .iter()
            .map(|entry| CachePruneTargetEntryOutput {
                path: entry.path.display().to_string(),
                prefix: entry.prefix.clone(),
                hash: entry.hash.clone(),
                size_bytes: entry.size_bytes,
                size_human: crate::cache_lib::target_registry::human_size(entry.size_bytes),
                mtime_unix: entry.mtime_unix,
                action: match entry.action {
                    crate::cache_lib::prune_target::PruneAction::Keep => "keep",
                    crate::cache_lib::prune_target::PruneAction::Delete => "delete",
                },
            })
            .collect(),
    }
}

fn print_cache_prune_target_text(
    target_dir: &std::path::Path,
    dry_run: bool,
    keep_latest: bool,
    report: &crate::cache_lib::prune_target::PruneTargetReport,
) {
    println!("soldr cache prune-target: {}", target_dir.display());
    println!(
        "  mode: {}",
        if dry_run {
            "dry-run (use --force to actually delete)"
        } else {
            "force"
        }
    );
    println!(
        "  strategy: {}",
        if keep_latest {
            "keep-latest (one hash family per prefix; aggressive — issue #316)"
        } else {
            "orphan-siblings (newest entry per (parent_dir, prefix) — issue #336)"
        }
    );
    println!(
        "  scanned={} kept={} deleted={} reclaimed={}",
        report.scanned,
        report.kept,
        report.deleted,
        crate::cache_lib::target_registry::human_size(report.reclaimed_bytes),
    );
    if report.keep_decisions_from_fingerprint + report.keep_decisions_from_mtime > 0 {
        println!(
            "  rank source: {} fingerprint, {} fs-mtime",
            report.keep_decisions_from_fingerprint, report.keep_decisions_from_mtime
        );
    }
    let mut shown = 0usize;
    for entry in &report.entries {
        if entry.action != crate::cache_lib::prune_target::PruneAction::Delete {
            continue;
        }
        if shown == 0 {
            println!(
                "  {} entries:",
                if dry_run { "would delete" } else { "deleted" }
            );
        }
        println!(
            "    - {} ({})",
            entry.path.display(),
            crate::cache_lib::target_registry::human_size(entry.size_bytes),
        );
        shown += 1;
    }
    if shown == 0 {
        println!("  nothing to prune");
    }
}

#[derive(Serialize)]
struct CachePruneTargetOutput {
    schema_version: u32,
    command: &'static str,
    target_dir: String,
    dry_run: bool,
    /// True when invoked with `--keep-latest` (issue #316 aggressive
    /// per-prefix bucketing); false for the legacy
    /// `(parent_dir, prefix)` orphan-sibling prune (issue #336).
    keep_latest: bool,
    scanned: usize,
    kept: usize,
    deleted: usize,
    reclaimed_bytes: u64,
    reclaimed_human: String,
    /// Number of keep decisions whose rank came from cargo's
    /// authoritative `.fingerprint/<prefix>-<hash>/invoked.timestamp`.
    keep_decisions_from_fingerprint: usize,
    /// Number of keep decisions that fell back to the entry's own
    /// filesystem mtime (no matching fingerprint file existed).
    keep_decisions_from_mtime: usize,
    entries: Vec<CachePruneTargetEntryOutput>,
}

#[derive(Serialize)]
struct CachePruneTargetEntryOutput {
    path: String,
    prefix: String,
    hash: String,
    size_bytes: u64,
    size_human: String,
    mtime_unix: i64,
    action: &'static str,
}

/// Profile preset for `soldr cache trim-target`. The CLI maps this
/// onto the actual rule selection inside [`run_cache_trim_target_command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrimProfile {
    /// Lightweight: orphan hash-sibling prune only.
    Local,
    /// Aggressive: prune + strip recreatable noise + drop
    /// `incremental/`. The intended profile for CI cache transport.
    Ci,
}

impl TrimProfile {
    fn as_str(self) -> &'static str {
        match self {
            TrimProfile::Local => "local",
            TrimProfile::Ci => "ci",
        }
    }
}

/// Trim a cargo `target/` directory in preparation for archival.
///
/// Composition order matters:
/// 1. Refuse if a `.cargo-lock` is present (active build sentinel,
///    reused from `prune_target`'s guard).
/// 2. CI-only: remove `target/<profile>/incremental/` wholesale.
///    rustc resets the incremental session on each fresh runner and
///    the per-process cache contributes ~0% hit rate against the next
///    job's invocation. Cargo recreates an empty `incremental/` on
///    first build.
/// 3. CI-only: run [`strip_target`] with every rule on. Removes large
///    build-script stderr, `build-script-build*` binaries,
///    `examples/`/`doc/`/`tests/`, and `.dwo`/`.pdb`/`.dSYM` debug
///    sidecars under `deps/`.
/// 4. Always: run [`prune_target`] to drop orphan hash-siblings in
///    `deps/`, `.fingerprint/`, `incremental/` (if it still exists),
///    and `build/`.
///
/// Steps 2–3 are gated on the CI profile because a developer running
/// `soldr cache trim-target` locally generally wants to KEEP the
/// incremental cache and the examples/doc artifacts. Step 4 is always
/// safe (cargo never reads orphan hashes again).
pub(crate) fn run_cache_trim_target_command(
    target_dir: std::path::PathBuf,
    profile: TrimProfile,
    dry_run: bool,
    json: bool,
) -> Result<(), SoldrError> {
    let canonical = std::path::absolute(&target_dir).unwrap_or_else(|_| target_dir.clone());

    // Refuse only when a build actually HOLDS a lock, never merely because a
    // `.cargo-lock` file exists: cargo leaves that sentinel behind after every
    // build, so a presence check made trim-target refuse forever on any
    // checkout that had ever been built. `cargo_lock::probe` acquires each
    // sentinel and reports Active only when another process holds it.
    //
    // The guard is scoped to the steps that write before `prune_target` runs.
    // `prune_target` re-probes and holds its own guard around its deletions, so
    // holding ours across that call would make it see our own locks as a live
    // build (the self-contention this scoping avoids).
    let (incremental_removed, strip_report) = {
        let _cargo_locks =
            match crate::cache_lib::cargo_lock::probe(&canonical).map_err(|error| {
                SoldrError::Other(format!("cache trim-target: lock probe failed: {error}"))
            })? {
                crate::cache_lib::cargo_lock::CargoLockProbe::Idle(guard) => guard,
                crate::cache_lib::cargo_lock::CargoLockProbe::Active(lock) => {
                    return Err(SoldrError::Other(format!(
                        "cargo lock held at {} (active build); refusing to trim",
                        lock.display()
                    )));
                }
            };

        let mut incremental_removed: Vec<TrimIncrementalEntry> = Vec::new();
        if matches!(profile, TrimProfile::Ci) {
            incremental_removed = remove_incremental_dirs(&canonical, dry_run)?;
        }

        let strip_report = if matches!(profile, TrimProfile::Ci) {
            let opts = crate::cache_lib::strip_target::StripTargetOptions {
                dry_run,
                ..crate::cache_lib::strip_target::StripTargetOptions::all(canonical.clone())
            };
            Some(
                crate::cache_lib::strip_target::strip_target(&opts).map_err(|e| {
                    SoldrError::Other(format!("cache trim-target: strip failed: {e}"))
                })?,
            )
        } else {
            None
        };
        (incremental_removed, strip_report)
    };

    let prune_opts = crate::cache_lib::prune_target::PruneTargetOptions {
        target_dir: canonical.clone(),
        dry_run,
        keep_latest: false,
    };
    let prune_report = crate::cache_lib::prune_target::prune_target(&prune_opts)
        .map_err(|e| SoldrError::Other(format!("cache trim-target: prune failed: {e}")))?;

    let summary = TrimSummary {
        target_dir: canonical.clone(),
        profile,
        dry_run,
        incremental_removed,
        strip_report,
        prune_report,
    };

    if json {
        let output = build_cache_trim_target_output(&summary);
        print_json(&output)?;
    } else {
        print_cache_trim_target_text(&summary);
    }
    Ok(())
}

/// Walk every `target/<profile>/incremental/` and remove the whole
/// directory. Returns the per-profile sizes that were reclaimed (or
/// would be reclaimed under dry-run) so the report can surface them.
fn remove_incremental_dirs(
    target_dir: &std::path::Path,
    dry_run: bool,
) -> Result<Vec<TrimIncrementalEntry>, SoldrError> {
    let mut out = Vec::new();
    let read = match std::fs::read_dir(target_dir) {
        Ok(it) => it,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(SoldrError::Other(format!("read_dir failed: {err}"))),
    };
    let incremental_name = crate::cache_lib::prune_target::INCREMENTAL_SUBDIR;
    for entry in read.flatten() {
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !ft.is_dir() {
            continue;
        }
        let profile_path = entry.path();
        // Skip dotted entries (.fingerprint et al. are not profiles).
        if let Some(name) = profile_path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') {
                continue;
            }
        }
        let incremental_path = profile_path.join(incremental_name);
        if !incremental_path.is_dir() {
            continue;
        }
        let size = trim_dir_size(&incremental_path);
        if !dry_run {
            if let Err(err) = std::fs::remove_dir_all(&incremental_path) {
                return Err(SoldrError::Other(format!(
                    "failed to remove {}: {}",
                    incremental_path.display(),
                    err
                )));
            }
        }
        out.push(TrimIncrementalEntry {
            path: incremental_path,
            size_bytes: size,
        });
    }
    Ok(out)
}

fn trim_dir_size(path: &std::path::Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<std::path::PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    total
}

struct TrimIncrementalEntry {
    path: std::path::PathBuf,
    size_bytes: u64,
}

struct TrimSummary {
    target_dir: std::path::PathBuf,
    profile: TrimProfile,
    dry_run: bool,
    incremental_removed: Vec<TrimIncrementalEntry>,
    strip_report: Option<crate::cache_lib::strip_target::StripReport>,
    prune_report: crate::cache_lib::prune_target::PruneTargetReport,
}

fn build_cache_trim_target_output(summary: &TrimSummary) -> CacheTrimTargetOutput {
    let incremental_bytes: u64 = summary
        .incremental_removed
        .iter()
        .map(|e| e.size_bytes)
        .sum();
    let strip_bytes = summary
        .strip_report
        .as_ref()
        .map(|r| r.reclaimed_bytes)
        .unwrap_or(0);
    let prune_bytes = summary.prune_report.reclaimed_bytes;
    let total = incremental_bytes
        .saturating_add(strip_bytes)
        .saturating_add(prune_bytes);
    CacheTrimTargetOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache trim-target",
        target_dir: summary.target_dir.display().to_string(),
        profile: summary.profile.as_str(),
        dry_run: summary.dry_run,
        incremental_removed_count: summary.incremental_removed.len(),
        incremental_reclaimed_bytes: incremental_bytes,
        strip_deleted: summary
            .strip_report
            .as_ref()
            .map(|r| r.deleted)
            .unwrap_or(0),
        strip_reclaimed_bytes: strip_bytes,
        prune_scanned: summary.prune_report.scanned,
        prune_kept: summary.prune_report.kept,
        prune_deleted: summary.prune_report.deleted,
        prune_reclaimed_bytes: prune_bytes,
        total_reclaimed_bytes: total,
        total_reclaimed_human: crate::cache_lib::target_registry::human_size(total),
    }
}

fn print_cache_trim_target_text(summary: &TrimSummary) {
    println!("soldr cache trim-target: {}", summary.target_dir.display());
    println!(
        "  profile: {}  mode: {}",
        summary.profile.as_str(),
        if summary.dry_run {
            "dry-run (use --force to actually delete)"
        } else {
            "force"
        }
    );
    let incremental_bytes: u64 = summary
        .incremental_removed
        .iter()
        .map(|e| e.size_bytes)
        .sum();
    if !summary.incremental_removed.is_empty() {
        println!(
            "  incremental/: {} dirs, {}",
            summary.incremental_removed.len(),
            crate::cache_lib::target_registry::human_size(incremental_bytes),
        );
    }
    if let Some(strip) = &summary.strip_report {
        println!(
            "  strip: deleted={} reclaimed={}",
            strip.deleted,
            crate::cache_lib::target_registry::human_size(strip.reclaimed_bytes),
        );
    }
    println!(
        "  prune: scanned={} kept={} deleted={} reclaimed={}",
        summary.prune_report.scanned,
        summary.prune_report.kept,
        summary.prune_report.deleted,
        crate::cache_lib::target_registry::human_size(summary.prune_report.reclaimed_bytes),
    );
    let total = incremental_bytes
        .saturating_add(
            summary
                .strip_report
                .as_ref()
                .map(|r| r.reclaimed_bytes)
                .unwrap_or(0),
        )
        .saturating_add(summary.prune_report.reclaimed_bytes);
    println!(
        "  total reclaimed: {}",
        crate::cache_lib::target_registry::human_size(total),
    );
}

#[derive(Serialize)]
struct CacheTrimTargetOutput {
    schema_version: u32,
    command: &'static str,
    target_dir: String,
    profile: &'static str,
    dry_run: bool,
    incremental_removed_count: usize,
    incremental_reclaimed_bytes: u64,
    strip_deleted: usize,
    strip_reclaimed_bytes: u64,
    prune_scanned: usize,
    prune_kept: usize,
    prune_deleted: usize,
    prune_reclaimed_bytes: u64,
    total_reclaimed_bytes: u64,
    total_reclaimed_human: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn touch(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        File::create(path).unwrap();
    }

    /// Cargo leaves `.cargo-lock` behind after every build, so the file's
    /// mere presence must not be read as a live build: that check made
    /// `trim-target` unusable on every checkout that had ever been built.
    #[test]
    fn a_stale_cargo_lock_file_does_not_block_the_trim() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        touch(&target.join("release").join(".cargo-lock"));

        run_cache_trim_target_command(target, TrimProfile::Local, true, false)
            .expect("a leftover .cargo-lock file must not be treated as an active build");
    }

    /// The guard that matters is the held lock, and it still refuses.
    #[test]
    fn a_held_cargo_lock_still_refuses_the_trim() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let lock = target.join("release").join(".cargo-lock");
        touch(&lock);
        let held = File::open(&lock).unwrap();
        held.lock().expect("take the lock");

        let error = run_cache_trim_target_command(target, TrimProfile::Local, true, false)
            .expect_err("a held lock must refuse");
        assert!(
            format!("{error}").contains("active build"),
            "unexpected error: {error}"
        );
    }
}
