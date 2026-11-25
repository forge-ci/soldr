//! Filesystem discovery of `target/` dirs the registry never recorded
//! (soldr#2700).
//!
//! The target registry is written only when a build goes through
//! soldr's cargo front door or the wrapper's fire-and-forget daemon
//! touch. Everything built by plain `cargo` is invisible to it forever,
//! which left auto-GC's tier 2 with nothing to reclaim while the volume
//! filled. This module bridges the `gc target` walker (issue #574) into
//! the registry-shaped scan the reclaimer already speaks.

use super::{daemon_registry_rows, target_walker};
use crate::core::{SoldrError, SoldrPaths};

/// Depth the auto-GC discovery walk uses, matching `gc target`'s own
/// default so the two commands see the same tree (`clud`'s worktree
/// layout already sits at depth 6).
pub(super) const AUTO_GC_DISCOVERY_MAX_DEPTH: usize = 8;

/// Synthesize registry rows for `target/` dirs the registry never saw
/// (soldr#2700).
///
/// A row only lands in the registry when a build goes through soldr's
/// cargo front door or the wrapper's daemon target-touch. Anything built
/// by plain `cargo` is invisible to it forever, and the touch is
/// fire-and-forget IPC that a down or busy daemon drops with nothing to
/// backfill it. On the reporting box that left 41 directories totaling
/// 106.4 GB unreclaimable behind a registry holding 4 rows, none of them
/// a workspace — while `soldr gc target`, which walks the filesystem,
/// listed every one.
///
/// This runs the same walk `soldr gc target` uses, so the report and the
/// reclaimer agree about what exists. Synthesized rows carry the
/// directory's own mtime as `last_used` — the value
/// `effective_age_seconds` would have clamped a real row down to anyway,
/// so an actively-built target still looks young and stays safe.
///
/// Paths already in `known` are left out: a registered target keeps its
/// recorded `last_used` instead of being re-aged from mtime.
pub(super) fn discovered_target_rows(
    roots: &[std::path::PathBuf],
    max_depth: usize,
    known: &[crate::cache_lib::target_registry::TargetRow],
) -> Vec<crate::cache_lib::target_registry::TargetRow> {
    let seen: std::collections::HashSet<&std::path::Path> =
        known.iter().map(|row| row.path.as_path()).collect();
    let mut out: Vec<crate::cache_lib::target_registry::TargetRow> = Vec::new();
    let mut emitted: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    for root in roots {
        for entry in target_walker::walk(root, max_depth) {
            if seen.contains(entry.target_dir.as_path()) {
                continue;
            }
            if !emitted.insert(entry.target_dir.clone()) {
                continue;
            }
            out.push(crate::cache_lib::target_registry::TargetRow {
                // Milliseconds to seconds. A zero mtime (unreadable) becomes
                // last_used=0, i.e. maximally old — the age threshold then
                // decides, and the safety guards still run.
                last_used: entry.last_modified_ms / 1000,
                path: entry.target_dir,
            });
        }
    }
    out
}

/// [`daemon_gc_scan`] over the registry plus `extra_rows` (soldr#2700).
///
/// `extra_rows` are filesystem-discovered targets the registry never
/// recorded. They go through the same `scan_daemon_snapshot` as registry
/// rows, so the size/age thresholds and every safety guard — including
/// the `dev_roots` allowlist — apply identically; nothing reaches the
/// deleter on a shorter path than a registered target would.
///
/// Rows already present in the registry win: the caller filters them out
/// of `extra_rows` so a registered target keeps its recorded `last_used`
/// rather than being re-aged from mtime.
pub(super) fn daemon_gc_scan_with_rows(
    paths: &SoldrPaths,
    options: &crate::cache_lib::gc::GcOptions,
    extra_rows: Vec<crate::cache_lib::target_registry::TargetRow>,
) -> Result<crate::cache_lib::gc::GcReport, SoldrError> {
    // Planning runs outside the maintenance lease. Missing rows may be
    // refreshed by a concurrent build, so this snapshot is read-only.
    let live: Vec<_> = daemon_registry_rows(paths)?
        .into_iter()
        .filter(|row| row.path.exists())
        .collect();
    let known: std::collections::HashSet<&std::path::Path> =
        live.iter().map(|row| row.path.as_path()).collect();
    let mut all: Vec<crate::cache_lib::target_registry::TargetRow> = extra_rows
        .into_iter()
        .filter(|row| !known.contains(row.path.as_path()))
        .collect();
    all.extend(live);
    // Oldest first. `scan_snapshot` preserves input order into
    // `report.candidates`, and tier 2 deletes candidates in order under
    // `BLOCK_TIER_PRUNE_BUDGET` — so this order *is* the eviction
    // priority. `TargetRegistry::list` already hands back rows sorted by
    // `last_used` ascending; re-sorting keeps that contract once
    // discovered rows are mixed in, instead of letting a fresh
    // discovered target be deleted ahead of a long-cold registered one
    // just because of which list it came from.
    all.sort_by_key(|row| row.last_used);
    crate::cache_lib::gc::scan_daemon_snapshot(all, 0, options)
        .map_err(|error| SoldrError::Other(format!("gc scan failed: {error}")))
}

pub(super) fn reclaim_candidate(
    paths: &crate::core::SoldrPaths,
    candidate: crate::cache_lib::gc::GcCandidate,
    maintenance_lease: Option<crate::cache_lib::build_active::MaintenanceLease>,
) -> crate::cache_lib::gc::GcDeleteOutcome {
    use crate::cache_lib::gc::{
        delete_candidate_dir, detach_candidate_dir, reclaim_detached_candidate,
    };
    let Some(lease) = maintenance_lease else {
        return delete_candidate_dir(candidate);
    };
    let (outcome, detached) = detach_candidate_dir(candidate, &paths.root);
    drop(lease);
    match detached {
        Some(path) => reclaim_detached_candidate(outcome, path),
        None => outcome,
    }
}

pub(super) fn target_still_cold(
    target: &std::path::Path,
    options: &crate::cache_lib::gc::GcOptions,
    rows: impl FnOnce() -> Result<
        Vec<crate::cache_lib::target_registry::TargetRow>,
        crate::core::SoldrError,
    >,
) -> bool {
    use crate::cache_lib::target_registry::{
        current_unix_seconds, workspace_root_for_target, GuardOutcome,
    };

    let (Ok(rows), Ok(now)) = (rows(), current_unix_seconds()) else {
        return false;
    };
    let last_used = rows
        .iter()
        .find(|row| row.path == target)
        .map_or(0, |row| row.last_used);
    let age = crate::cache_lib::gc::effective_age_seconds(target, last_used, now);
    age >= 0
        && age as u64 >= options.older_than_seconds
        && matches!(
            crate::cache_lib::gc::evaluate_target_metadata_guards(
                target,
                &workspace_root_for_target(target),
                &options.dev_roots,
                options.older_than_seconds,
                now,
            ),
            GuardOutcome::Eligible,
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_lib::target_registry::TargetRow;

    // -------------------------------------------------------------------
    // soldr#2700: filesystem discovery of `target/` dirs the registry
    // never recorded. The registry is only written when a build goes
    // through soldr's cargo front door or the wrapper's fire-and-forget
    // daemon touch, so plain-`cargo` builds stayed invisible to auto-GC
    // forever.
    // -------------------------------------------------------------------

    /// Lay down `<root>/<name>/{Cargo.toml, target/debug/build.bin}`.
    fn write_workspace_with_target(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        let workspace = root.join(name);
        let target = workspace.join("target");
        std::fs::create_dir_all(target.join("debug")).unwrap();
        std::fs::write(workspace.join("Cargo.toml"), b"[package]\nname = \"x\"\n").unwrap();
        std::fs::write(target.join("debug").join("build.bin"), vec![0u8; 512]).unwrap();
        target
    }

    #[test]
    fn discovered_target_rows_finds_targets_absent_from_the_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let built_by_plain_cargo = write_workspace_with_target(root, "unregistered");

        let rows = discovered_target_rows(&[root.to_path_buf()], 4, &[]);

        assert_eq!(rows.len(), 1, "expected one discovered row, got {rows:?}");
        assert_eq!(rows[0].path, built_by_plain_cargo);
    }

    #[test]
    fn discovered_target_rows_yields_to_the_registry_for_known_paths() {
        // A registered target keeps the `last_used` soldr recorded for it.
        // Re-emitting it from mtime would make an actively-built cache look
        // like whatever its directory timestamp says, which is exactly the
        // ranking confusion `effective_age_seconds` exists to prevent.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let known_target = write_workspace_with_target(root, "registered");
        write_workspace_with_target(root, "unregistered");

        let known = vec![TargetRow {
            path: known_target.clone(),
            last_used: 4242,
        }];
        let rows = discovered_target_rows(&[root.to_path_buf()], 4, &known);

        assert_eq!(rows.len(), 1, "registered target should not be re-emitted");
        assert_ne!(rows[0].path, known_target);
    }

    #[test]
    fn discovered_target_rows_dedupes_overlapping_roots() {
        // Two configured dev roots where one contains the other must not
        // produce the same target twice: a duplicated row double-counts the
        // directory's size in the tier-2 reclaim accounting.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        write_workspace_with_target(&nested, "proj");

        let rows = discovered_target_rows(&[root.to_path_buf(), nested.clone()], 4, &[]);

        assert_eq!(rows.len(), 1, "expected dedupe across roots, got {rows:?}");
    }

    #[test]
    fn discovered_target_rows_is_empty_for_a_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(discovered_target_rows(&[missing], 4, &[]).is_empty());
    }
}
