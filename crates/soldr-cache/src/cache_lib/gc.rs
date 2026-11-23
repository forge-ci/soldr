//! `soldr gc` orchestration: scan the target registry, apply safety
//! guards and thresholds, surface candidates, and (optionally) delete
//! their `target/` directories.
//!
//! This module owns the policy. The CLI layer is a thin shim that
//! parses flags, builds a [`GcOptions`], and prints the
//! [`GcReport`] / decides whether to invoke [`GcPlan::apply`].

use super::target_registry::{
    current_unix_seconds, directory_size, evaluate_safety_guards, human_age, human_size,
    workspace_root_for_target, GuardOutcome, RegistryError, TargetRegistry, TargetRow,
    DEFAULT_STALE_AGE_SECONDS, DEFAULT_STALE_SIZE_BYTES,
};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const GC_LOG_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone)]
pub struct GcOptions {
    pub older_than_seconds: u64,
    pub larger_than_bytes: u64,
    pub dev_roots: Vec<PathBuf>,
    pub dry_run: bool,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            older_than_seconds: DEFAULT_STALE_AGE_SECONDS,
            larger_than_bytes: DEFAULT_STALE_SIZE_BYTES,
            dev_roots: Vec::new(),
            dry_run: false,
        }
    }
}

/// One concrete candidate `target/` directory after sizing and
/// guard evaluation.
#[derive(Debug, Clone)]
pub struct GcCandidate {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub age_seconds: i64,
    pub eligible: bool,
    pub reason: Option<String>,
    /// Whether this `target/` belongs to a linked git worktree rather
    /// than a primary checkout. Eviction takes these first *once they have
    /// gone cold* — see [`in_linked_git_worktree`] and
    /// [`WORKTREE_TIER_AGE_SECONDS`].
    pub in_worktree: bool,
}

/// Aggregated scan result for the registry.
#[derive(Debug, Clone, Default)]
pub struct GcReport {
    /// Eligible candidates (over thresholds, guards passed).
    pub candidates: Vec<GcCandidate>,
    /// Skipped entries (guards rejected, under thresholds, or
    /// missing-on-disk rows that were dropped).
    pub skipped: Vec<GcCandidate>,
    /// Number of registry rows whose path didn't exist on disk and
    /// were quietly dropped.
    pub dropped_missing: usize,
}

#[derive(Debug, Clone)]
pub struct GcDeleteOutcome {
    pub candidate: GcCandidate,
    pub removed: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GcPurgeFailure {
    pub candidate: GcCandidate,
    pub error: String,
}

#[derive(Debug, Clone, Default)]
pub struct GcPurgeSummary {
    pub selected_count: usize,
    pub succeeded_count: usize,
    pub failed_count: usize,
    pub reclaimed_bytes: u64,
    pub deleted_paths: Vec<PathBuf>,
    pub failures: Vec<GcPurgeFailure>,
}

/// Snapshot of the registry taken before any long-running work, so the
/// database handle can be released while that work happens (#1681).
pub struct RegistrySnapshot {
    /// Rows whose directory still exists on disk.
    pub rows: Vec<TargetRow>,
    /// Rows dropped because their path was gone.
    pub dropped_missing: usize,
}

/// Read the registry into an owned snapshot and **release the database
/// handle before returning** (#1681).
///
/// SQLite connections do not exclude other openers. This phase separation
/// keeps registry work and any write transaction outside directory sizing,
/// prompting, and deletion, which can take an unbounded amount of time.
/// The original #1681 fix addressed redb's now-removed exclusive-open lock.
///
/// Pruning missing rows stays inside this short phase because it is the
/// one registry write the scan needs, and it is bounded by the row
/// count rather than by disk size.
pub fn snapshot_registry(db_path: &Path) -> Result<RegistrySnapshot, RegistryError> {
    let registry = TargetRegistry::open(db_path)?;
    let snapshot = snapshot_from_registry(&registry)?;
    drop(registry);
    Ok(snapshot)
}

/// [`snapshot_registry`] against an already-open registry, for callers
/// that own the handle and manage its lifetime themselves.
pub fn snapshot_from_registry(
    registry: &TargetRegistry,
) -> Result<RegistrySnapshot, RegistryError> {
    let mut rows = Vec::new();
    let mut dropped_missing = 0usize;
    for row in registry.list()? {
        if row.path.exists() {
            rows.push(row);
        } else {
            // Drop missing-on-disk row silently per the proposal.
            let _ = registry.remove(&row.path);
            dropped_missing += 1;
        }
    }
    Ok(RegistrySnapshot {
        rows,
        dropped_missing,
    })
}

/// Scan without holding a database handle: open, snapshot, release, then
/// do the sizing and guard evaluation against the owned rows (#1681).
///
/// This is what the `soldr gc` entry points should call. [`scan`] is the
/// handle-holding equivalent, kept for callers that already have a
/// registry open.
pub fn scan_released(db_path: &Path, options: &GcOptions) -> Result<GcReport, RegistryError> {
    let snapshot = snapshot_registry(db_path)?;
    scan_snapshot(snapshot, options)
}

/// Scan an already-owned registry snapshot received from the daemon.
///
/// The caller has no database handle: the daemon remains the sole process
/// that reads or mutates `state.sqlite3`, while the CLI retains the filesystem
/// sizing and safety-guard work that must run in the caller's environment.
pub fn scan_daemon_snapshot(
    rows: Vec<TargetRow>,
    dropped_missing: usize,
    options: &GcOptions,
) -> Result<GcReport, RegistryError> {
    scan_snapshot(
        RegistrySnapshot {
            rows,
            dropped_missing,
        },
        options,
    )
}

/// Pure scan: walk the registry, drop missing rows, apply thresholds
/// and safety guards. Does not delete anything.
///
/// Holds `registry` for the whole sizing walk. Prefer [`scan_released`]
/// on any path that goes on to prompt or delete (#1681).
pub fn scan(registry: &TargetRegistry, options: &GcOptions) -> Result<GcReport, RegistryError> {
    let snapshot = snapshot_from_registry(registry)?;
    scan_snapshot(snapshot, options)
}

/// Check the allowlist and workspace lockfile age without walking the target.
/// A caller that mutates the target must also acquire its Cargo locks.
pub fn evaluate_target_metadata_guards(
    target_dir: &Path,
    workspace_root: &Path,
    dev_roots: &[PathBuf],
    stale_age_seconds: u64,
    now_unix_seconds: i64,
) -> GuardOutcome {
    if !super::target_registry::is_under_any_root(target_dir, dev_roots) {
        return GuardOutcome::Skipped(format!(
            "outside configured gc.allowlist_roots ({} roots)",
            dev_roots.len()
        ));
    }

    let cargo_lock = workspace_root.join("Cargo.lock");
    if let Ok(meta) = std::fs::metadata(&cargo_lock) {
        if let Ok(modified) = meta.modified() {
            if let Ok(elapsed) = modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
            {
                let age = now_unix_seconds.saturating_sub(elapsed);
                if (age as u64) < stale_age_seconds {
                    return GuardOutcome::Skipped(format!(
                        "Cargo.lock modified {} ago (< staleness threshold)",
                        human_age(age)
                    ));
                }
            }
        }
    }

    GuardOutcome::Eligible
}

/// How old a tracked `target/` is, taking the *most recent* of two
/// signals (soldr#2134).
///
/// `last_used` is soldr's own bookkeeping: it is stamped when a build goes
/// through the front door. That makes it a poor sole measure of whether a
/// cache is cold, because it goes stale while the directory stays hot:
///
/// * a repo built with bare `cargo` never updates it;
/// * the daemon can lose the touch outright -- `client.rs` reports
///   "target-registry touch was lost: daemon unreachable".
///
/// A target in either state looks like the *oldest* row in the registry
/// (rows are ordered by `last_used` ascending), so it is the first thing
/// eviction reaches for -- which is how a 57.7 GB actively-built cache was
/// purged while a merged worktree's 5.6 GB was kept.
///
/// Consulting the directory's mtime as well can only ever make a target
/// look *younger*, so this is strictly conservative: it can spare a cache
/// from deletion, never cause one. A missing or unreadable mtime falls back
/// to the registry value, which is the previous behaviour.
/// Exported so `soldr gc target` reports the same age eviction acts on.
/// A report that disagrees with the decision it is meant to explain is
/// how "why did it delete *that* one?" becomes unanswerable.
pub fn effective_age_seconds(path: &Path, last_used: i64, now: i64) -> i64 {
    let registry_age = now.saturating_sub(last_used);
    let Ok(metadata) = std::fs::metadata(path) else {
        return registry_age;
    };
    let Ok(modified) = metadata.modified() else {
        return registry_age;
    };
    let Ok(since_epoch) = modified.duration_since(SystemTime::UNIX_EPOCH) else {
        return registry_age;
    };
    let fs_age = now.saturating_sub(since_epoch.as_secs() as i64);
    registry_age.min(fs_age)
}

/// Whether `target_dir` belongs to a **linked git worktree** rather than
/// a primary checkout (soldr#2134).
///
/// Eviction used to be age-ordered only, which let it purge a 57.7 GB
/// actively-built cache while keeping a 5.6 GB worktree whose PR had
/// merged a day and a half earlier — the most expensive possible choice,
/// because the purged cache was rebuilt immediately and the retained one
/// will never be built again.
///
/// The signal is the one git itself uses and no tool can get wrong: a
/// linked worktree's root holds `.git` as a **file** containing
/// `gitdir: …/worktrees/<name>`, whereas a primary checkout holds `.git`
/// as a directory. That covers `.claude/worktrees/` and every other
/// layout without hardcoding a convention, and it needs no subprocess.
///
/// Deliberately *not* "is the branch merged". Ancestry is wrong under a
/// squash-merge workflow (`merge-base --is-ancestor` reports false
/// because the squash produced a different SHA), and the patch-id
/// alternatives cost a `git` invocation per candidate for a signal that
/// is only ever a tiebreak. Worktrees are ephemeral by construction, so
/// their build output is the safest thing on the volume to prefer — and
/// coldness, which is already applied within each tier, is a good enough
/// proxy for the rest.
pub fn in_linked_git_worktree(target_dir: &Path) -> bool {
    let workspace = workspace_root_for_target(target_dir);
    workspace.join(".git").is_file()
}

/// Threshold + safety-guard evaluation over an owned snapshot. Touches
/// the filesystem (sizing) but never the database, so it is safe to run
/// with no handle open.
pub fn scan_snapshot(
    snapshot: RegistrySnapshot,
    options: &GcOptions,
) -> Result<GcReport, RegistryError> {
    let now = current_unix_seconds()?;
    let mut report = GcReport {
        dropped_missing: snapshot.dropped_missing,
        ..GcReport::default()
    };

    for row in snapshot.rows {
        let age = effective_age_seconds(&row.path, row.last_used, now);
        let size = directory_size(&row.path);
        let in_worktree = in_linked_git_worktree(&row.path);

        if (age as u64) < options.older_than_seconds {
            report.skipped.push(GcCandidate {
                path: row.path,
                size_bytes: size,
                age_seconds: age,
                in_worktree,
                eligible: false,
                reason: Some(format!(
                    "younger than threshold ({} < {})",
                    human_age(age),
                    human_age(options.older_than_seconds as i64)
                )),
            });
            continue;
        }

        if size < options.larger_than_bytes {
            report.skipped.push(GcCandidate {
                path: row.path,
                size_bytes: size,
                age_seconds: age,
                in_worktree,
                eligible: false,
                reason: Some(format!(
                    "smaller than threshold ({} < {})",
                    human_size(size),
                    human_size(options.larger_than_bytes)
                )),
            });
            continue;
        }

        let workspace = workspace_root_for_target(&row.path);
        let outcome = evaluate_safety_guards(
            &row.path,
            &workspace,
            &options.dev_roots,
            options.older_than_seconds,
            now,
        );
        match outcome {
            GuardOutcome::Eligible => {
                report.candidates.push(GcCandidate {
                    path: row.path,
                    size_bytes: size,
                    age_seconds: age,
                    in_worktree,
                    eligible: true,
                    reason: None,
                });
            }
            GuardOutcome::Skipped(reason) => {
                report.skipped.push(GcCandidate {
                    path: row.path,
                    size_bytes: size,
                    age_seconds: age,
                    in_worktree,
                    eligible: false,
                    reason: Some(reason),
                });
            }
        }
    }

    order_candidates(&mut report.candidates);
    Ok(report)
}

/// How long a linked worktree must sit untouched before its `target/` is
/// promoted ahead of colder primary checkouts (soldr#2134).
///
/// The promotion is only sound for a worktree nobody will build again.
/// soldr#2156 applied it unconditionally, which is stronger than the issue
/// asked for: a worktree built moments ago would outrank a primary checkout
/// idle for hours, and on a box with a dozen live worktrees under
/// `.claude/worktrees/` that costs exactly the rebuild the issue is about.
///
/// Merge state would be the faithful signal, but the issue documents why it
/// is hard to get right (squash-merge defeats `--is-ancestor`, and the
/// reported branch was still on the remote), so it takes the fallback the
/// issue itself proposes: coldness as the proxy. Three days is well past any
/// edit-test cycle while still catching an abandoned branch.
const WORKTREE_TIER_AGE_SECONDS: i64 = 3 * 24 * 60 * 60;

/// Order eviction cheapest-to-restore first (soldr#2134).
///
/// 1. Linked-worktree targets left cold for [`WORKTREE_TIER_AGE_SECONDS`],
///    which nobody is likely to build again — deleting them is close to free.
/// 2. Then coldest first, which is what the registry order approximated
///    and `effective_age_seconds` made trustworthy.
///
/// This reorders **already-eligible** candidates only. Nothing new
/// becomes deletable: every threshold and safety guard has already run
/// by this point, so the worst case is that the same set is deleted in
/// a better order.
fn order_candidates(candidates: &mut [GcCandidate]) {
    // A worktree only earns the tier once it has gone cold; a live one is
    // ranked on age alongside everything else.
    let abandoned_worktree =
        |c: &GcCandidate| c.in_worktree && c.age_seconds >= WORKTREE_TIER_AGE_SECONDS;
    candidates.sort_by(|a, b| {
        abandoned_worktree(b)
            .cmp(&abandoned_worktree(a))
            .then(b.age_seconds.cmp(&a.age_seconds))
            // Size last, so it only breaks ties between equally cold
            // targets rather than driving the choice as it appeared to.
            .then(b.size_bytes.cmp(&a.size_bytes))
    });
}

/// Delete the `target/` dir at `path` and drop its registry row. Skips
/// the actual delete when `dry_run` is true. Returns whether the
/// directory was removed (`false` when dry-run, when missing, or when
/// the delete failed).
pub fn purge_one(
    registry: &TargetRegistry,
    path: &Path,
    dry_run: bool,
) -> Result<bool, RegistryError> {
    if dry_run {
        return Ok(false);
    }
    if !path.exists() {
        let _ = registry.remove(path);
        return Ok(false);
    }
    // Refuse anything that is merely *named* `target` (#1671). Resolution is
    // name-based, so a wrong answer here is a recursive delete of whatever
    // that directory happens to be — an enclosing repository, in the nested
    // case this issue describes. Requiring a cargo marker makes the
    // destructive step prove its subject rather than trust the name.
    if !super::target_registry::looks_like_cargo_target(path) {
        return Err(RegistryError::Io(std::io::Error::other(format!(
            "{} has no cargo target markers; refusing to delete",
            path.display()
        ))));
    }
    let _cargo_locks = match super::cargo_lock::probe(path).map_err(RegistryError::Io)? {
        super::cargo_lock::CargoLockProbe::Idle(guard) => guard,
        super::cargo_lock::CargoLockProbe::Active(lock) => {
            return Err(RegistryError::Io(std::io::Error::other(format!(
                "active cargo lock at {}; refusing to delete",
                lock.display()
            ))));
        }
    };
    let result = std::fs::remove_dir_all(path);
    match result {
        Ok(_) => {
            let _ = registry.remove(path);
            Ok(true)
        }
        Err(e) => Err(RegistryError::Io(e)),
    }
}

pub fn delete_candidate_dir(candidate: GcCandidate) -> GcDeleteOutcome {
    remove_candidate_dir(candidate, |path| std::fs::remove_dir_all(path))
}

/// Detach a target under the same guards as deletion. The caller can release
/// its build lease before recursively reclaiming the detached tree.
pub fn detach_candidate_dir(
    candidate: GcCandidate,
    root: &Path,
) -> (GcDeleteOutcome, Option<DetachedCandidate>) {
    let mut detached = None;
    let outcome = remove_candidate_dir(candidate, |path| match detach_target(path, root) {
        Ok(candidate) => {
            detached = Some(candidate);
            Ok(())
        }
        // Guarded direct deletion still frees space if a detach cannot
        // allocate an inode or if the trash is on a different volume.
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::CrossesDevices
                    | std::io::ErrorKind::StorageFull
                    | std::io::ErrorKind::QuotaExceeded
            ) =>
        {
            std::fs::remove_dir_all(path)
        }
        Err(error) => Err(error),
    });
    (outcome, detached)
}

fn detach_target(path: &Path, root: &Path) -> std::io::Result<DetachedCandidate> {
    let lock = super::trash_gc::try_trash_reclaim_lock(root, true)?.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::WouldBlock, "trash sweep is active")
    })?;
    let trash = root.join(super::trash_gc::DETACHED_TARGETS_DIR);
    std::fs::create_dir_all(&trash)?;
    super::path_safety::validate_owned_directory(root, &trash)?;
    let staging = tempfile::Builder::new().prefix("target-").tempdir_in(trash)?;
    std::fs::rename(path, staging.path().join("target"))?;
    Ok(DetachedCandidate {
        path: staging.keep(),
        _lock: lock,
    })
}

/// An owned detached target protected from concurrent trash sweeps.
pub struct DetachedCandidate {
    path: PathBuf,
    _lock: std::fs::File,
}

/// Reclaim a target that is already detached from its original build path.
pub fn reclaim_detached_candidate(
    mut outcome: GcDeleteOutcome,
    detached: DetachedCandidate,
) -> GcDeleteOutcome {
    if let Err(error) = std::fs::remove_dir_all(&detached.path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            outcome.removed = false;
            outcome.error = Some(format!(
                "reclaiming detached target {}: {error}",
                detached.path.display()
            ));
        }
    }
    outcome
}

fn remove_candidate_dir(
    candidate: GcCandidate,
    remove: impl FnOnce(&Path) -> std::io::Result<()>,
) -> GcDeleteOutcome {
    if !candidate.path.exists() {
        return GcDeleteOutcome {
            candidate,
            removed: false,
            error: None,
        };
    }

    if !candidate.path.is_dir() {
        return GcDeleteOutcome {
            error: Some(format!(
                "registered target path is not a directory: {}",
                candidate.path.display()
            )),
            candidate,
            removed: false,
        };
    }

    let _cargo_locks = match super::cargo_lock::probe(&candidate.path) {
        Ok(super::cargo_lock::CargoLockProbe::Idle(guard)) => guard,
        Ok(super::cargo_lock::CargoLockProbe::Active(lock)) => {
            return GcDeleteOutcome {
                candidate,
                removed: false,
                error: Some(format!(
                    "active cargo lock at {}; refusing to delete",
                    lock.display()
                )),
            };
        }
        Err(error) => {
            return GcDeleteOutcome {
                candidate,
                removed: false,
                error: Some(format!("cargo lock probe failed closed: {error}")),
            };
        }
    };
    if !super::target_registry::looks_like_cargo_target(&candidate.path) {
        // Same guard as `purge_one` (#1671).
        return GcDeleteOutcome {
            error: Some(format!(
                "{} has no cargo target markers; refusing to delete",
                candidate.path.display()
            )),
            candidate,
            removed: false,
        };
    }
    match remove(&candidate.path) {
        Ok(()) => GcDeleteOutcome {
            candidate,
            removed: true,
            error: None,
        },
        Err(e) => GcDeleteOutcome {
            candidate,
            removed: false,
            error: Some(e.to_string()),
        },
    }
}

pub fn apply_purge_outcomes(
    registry: &TargetRegistry,
    outcomes: Vec<GcDeleteOutcome>,
) -> Result<GcPurgeSummary, RegistryError> {
    let mut summary = GcPurgeSummary {
        selected_count: outcomes.len(),
        ..GcPurgeSummary::default()
    };

    for outcome in outcomes {
        if let Some(error) = outcome.error {
            summary.failed_count += 1;
            summary.failures.push(GcPurgeFailure {
                candidate: outcome.candidate,
                error,
            });
            continue;
        }

        let _ = registry.remove(&outcome.candidate.path)?;
        summary.succeeded_count += 1;
        if outcome.removed {
            summary.reclaimed_bytes = summary
                .reclaimed_bytes
                .saturating_add(outcome.candidate.size_bytes);
            summary.deleted_paths.push(outcome.candidate.path);
        }
    }

    Ok(summary)
}

/// Summarize delete results without touching the target registry.
///
/// Daemon-owned callers use the returned paths in one IPC removal request
/// after filesystem deletion completes.
pub fn summarize_purge_outcomes(outcomes: Vec<GcDeleteOutcome>) -> (GcPurgeSummary, Vec<PathBuf>) {
    let mut summary = GcPurgeSummary {
        selected_count: outcomes.len(),
        ..GcPurgeSummary::default()
    };
    let mut removed_rows = Vec::new();
    for outcome in outcomes {
        if let Some(error) = outcome.error {
            summary.failed_count += 1;
            summary.failures.push(GcPurgeFailure {
                candidate: outcome.candidate,
                error,
            });
        } else {
            summary.succeeded_count += 1;
            if outcome.removed {
                summary.reclaimed_bytes = summary
                    .reclaimed_bytes
                    .saturating_add(outcome.candidate.size_bytes);
                summary.deleted_paths.push(outcome.candidate.path.clone());
            }
            removed_rows.push(outcome.candidate.path);
        }
    }
    (summary, removed_rows)
}

pub fn cleanup_old_gc_logs(log_dir: &Path) -> Result<usize, RegistryError> {
    cleanup_old_gc_logs_with_retention(log_dir, GC_LOG_RETENTION)
}

pub fn cleanup_old_gc_logs_with_retention(
    log_dir: &Path,
    retention: Duration,
) -> Result<usize, RegistryError> {
    let entries = match std::fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age > retention {
            std::fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

pub fn write_gc_error_log(
    log_dir: &Path,
    args: &[String],
    failures: &[GcPurgeFailure],
) -> Result<PathBuf, RegistryError> {
    std::fs::create_dir_all(log_dir)?;
    let now = current_unix_seconds()?;
    let path = log_dir.join(format!("gc-error-{now}-{}.log", std::process::id()));
    let mut file = std::fs::File::create(&path)?;

    writeln!(file, "timestamp_unix={now}")?;
    writeln!(file, "command_args={args:?}")?;
    writeln!(file, "failure_count={}", failures.len())?;
    writeln!(file)?;

    for failure in failures {
        writeln!(file, "path={}", failure.candidate.path.display())?;
        writeln!(file, "size_bytes={}", failure.candidate.size_bytes)?;
        writeln!(file, "age_seconds={}", failure.candidate.age_seconds)?;
        writeln!(file, "error={}", failure.error)?;
        writeln!(file)?;
    }

    Ok(path)
}

/// Information for the once-per-day startup warning. Returns `None`
/// if no candidates qualify or the warning was already emitted today.
pub fn maybe_build_startup_warning(
    registry: &TargetRegistry,
    options: &GcOptions,
    marker_path: &Path,
) -> Result<Option<String>, RegistryError> {
    if !startup_warning_due(marker_path)? {
        return Ok(None);
    }

    let report = scan(registry, options)?;
    // The marker throttles the expensive registry/filesystem scan, not only
    // visible warnings. Without recording an empty successful scan, machines
    // with no stale candidates repeat the full scan before every build.
    touch_startup_warning_marker(marker_path)?;
    if report.candidates.is_empty() {
        return Ok(None);
    }

    let total_bytes: u64 = report.candidates.iter().map(|c| c.size_bytes).sum();
    let n = report.candidates.len();
    let plural = if n == 1 { "" } else { "s" };
    let message = format!(
        "soldr: {n} stale target/ dir{plural} using {} (last used > {}). Run 'soldr gc' to review.",
        human_size(total_bytes),
        human_age(options.older_than_seconds as i64),
    );

    Ok(Some(message))
}

/// Render the startup warning from a daemon-owned registry scan.
pub fn startup_warning_from_report(
    report: &GcReport,
    options: &GcOptions,
    marker_path: &Path,
) -> Result<Option<String>, RegistryError> {
    if !startup_warning_due(marker_path)? {
        return Ok(None);
    }
    touch_startup_warning_marker(marker_path)?;
    if report.candidates.is_empty() {
        return Ok(None);
    }
    let total_bytes: u64 = report
        .candidates
        .iter()
        .map(|candidate| candidate.size_bytes)
        .sum();
    let n = report.candidates.len();
    let plural = if n == 1 { "" } else { "s" };
    Ok(Some(format!(
        "soldr: {n} stale target/ dir{plural} using {} (last used > {}). Run 'soldr gc' to review.",
        human_size(total_bytes),
        human_age(options.older_than_seconds as i64),
    )))
}

/// Whether enough time has passed since the last warning to emit
/// another one. Missing marker => due now.
pub fn startup_warning_due(marker_path: &Path) -> Result<bool, RegistryError> {
    let metadata = match std::fs::metadata(marker_path) {
        Ok(m) => m,
        Err(_) => return Ok(true),
    };
    let modified = match metadata.modified() {
        Ok(t) => t,
        Err(_) => return Ok(true),
    };
    let now = SystemTime::now();
    let elapsed = now
        .duration_since(modified)
        .unwrap_or(std::time::Duration::ZERO);
    Ok(elapsed.as_secs() >= 24 * 60 * 60)
}

fn touch_startup_warning_marker(path: &Path) -> Result<(), RegistryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, current_unix_seconds()?.to_string())?;
    Ok(())
}

/// Parse human-friendly duration strings like `10d`, `4h`, `30m`,
/// `90s`. Returns seconds.
pub fn parse_duration(input: &str) -> Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration".to_string());
    }
    let (num_str, unit) = split_numeric_suffix(s);
    let value: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid duration number in {s:?}"))?;
    let multiplier: u64 = match unit.to_ascii_lowercase().as_str() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
        "d" | "day" | "days" => 86_400,
        "w" | "wk" | "wks" | "week" | "weeks" => 7 * 86_400,
        other => return Err(format!("unknown duration unit {other:?} in {s:?}")),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("duration overflow in {s:?}"))
}

/// Parse human-friendly size strings like `256M`, `1GB`, `512KiB`.
/// Returns bytes. Both decimal (KB, MB) and binary (KiB, MiB)
/// suffixes resolve to powers of 1024.
pub fn parse_size(input: &str) -> Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty size".to_string());
    }
    let (num_str, unit) = split_numeric_suffix(s);
    let value: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid size number in {s:?}"))?;
    let multiplier: u64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024_u64.pow(4),
        other => return Err(format!("unknown size unit {other:?} in {s:?}")),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size overflow in {s:?}"))
}

fn split_numeric_suffix(s: &str) -> (&str, &str) {
    let split_at = s.find(|c: char| !(c.is_ascii_digit())).unwrap_or(s.len());
    s.split_at(split_at)
}

#[cfg(test)]
#[path = "gc_tests.rs"]
mod tests;
