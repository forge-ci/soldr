//! Purge-side surfaces for the gc taxonomy.
//!
//! Owns the per-candidate confirmation prompt, the parallel deletion
//! worker pool, the cargo_registry_src / cargo_git_checkouts purge
//! commands, and small helpers shared with the dispatch entry point
//! in `mod.rs` (`gc_total_reclaimable_bytes`, `gc_largest_candidates`,
//! `gc_candidate_output`, `resolve_gc_dev_roots`,
//! `print_gc_purge_*`).

use crate::cache::print_json;
use crate::core::{SoldrError, SoldrPaths};
use serde::Serialize;
use std::io::Write;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use super::walks::{walk_cargo_git_checkouts, walk_cargo_registry_src, walk_cargo_target_subtrees};
use super::{
    GcCandidateOutput, GcListEntryOutput, GcListKindFilter, GC_JSON_SCHEMA_VERSION,
    KIND_CARGO_GIT_CHECKOUTS, KIND_CARGO_REGISTRY_SRC, KIND_CARGO_TARGET, PURGE_SAFETY_DERIVED,
};
use crate::platform::sync::Mutex;

#[derive(Serialize)]
struct GcPurgeRegistrySrcOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    kind: &'static str,
    selected_count: usize,
    succeeded_count: usize,
    failed_count: usize,
    reclaimed_bytes: u64,
    reclaimed_human: String,
    deleted_paths: Vec<String>,
    failures: Vec<GcPurgeRegistrySrcFailure>,
}

#[derive(Serialize)]
struct GcPurgeRegistrySrcFailure {
    path: String,
    error: String,
}

/// `soldr gc purge --registry-src --all` — walk
/// `$CARGO_HOME/registry/src/<reg>/<crate>-<vers>/` and `remove_dir_all`
/// each entry. Failures are reported in the JSON summary; the command
/// exits with success even on per-dir failure so callers can scrape
/// the summary (matches the existing `gc purge` workspace-target
/// behavior, which writes failures to a log file).
pub(crate) fn run_gc_purge_registry_src_command(
    purge_all: bool,
    json: bool,
) -> Result<(), SoldrError> {
    let cargo_home = crate::core::resolve_cargo_home().ok_or_else(|| {
        SoldrError::Other(
            "could not resolve $CARGO_HOME (no env var, no home directory)".to_string(),
        )
    })?;
    let now = crate::cache_lib::target_registry::current_unix_seconds()
        .map_err(|e| SoldrError::Other(format!("gc purge --registry-src clock error: {e}")))?;
    let entries = walk_cargo_registry_src(&cargo_home, now);

    if !json {
        eprintln!(
            "soldr gc purge --registry-src: cargo_home={}",
            cargo_home.display()
        );
        eprintln!(
            "soldr gc purge --registry-src: {} crate source directory{} on disk",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" }
        );
    }

    let mut selected: Vec<&GcListEntryOutput> = Vec::new();
    for entry in &entries {
        let should_delete = purge_all
            || prompt_yes_no_default_yes(&format!(
                "soldr gc: delete {} ({}, age {}) ? [Y/n] ",
                entry.path, entry.size_human, entry.age_human,
            ));
        if should_delete {
            selected.push(entry);
        }
    }

    let mut deleted_paths: Vec<String> = Vec::new();
    let mut failures: Vec<GcPurgeRegistrySrcFailure> = Vec::new();
    let mut reclaimed_bytes: u64 = 0;
    for entry in &selected {
        let path = std::path::PathBuf::from(&entry.path);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                reclaimed_bytes = reclaimed_bytes.saturating_add(entry.size_bytes);
                deleted_paths.push(entry.path.clone());
            }
            Err(e) => failures.push(GcPurgeRegistrySrcFailure {
                path: entry.path.clone(),
                error: e.to_string(),
            }),
        }
    }

    if !json {
        eprintln!(
            "soldr gc purge --registry-src: selected {}; succeeded {}; failed {}; reclaimed {}",
            selected.len(),
            deleted_paths.len(),
            failures.len(),
            crate::cache_lib::target_registry::human_size(reclaimed_bytes),
        );
        for failure in &failures {
            eprintln!(
                "soldr gc purge --registry-src: failed to delete {}: {}",
                failure.path, failure.error
            );
        }
    } else {
        let output = GcPurgeRegistrySrcOutput {
            schema_version: GC_JSON_SCHEMA_VERSION,
            command: "gc",
            mode: "purge",
            kind: KIND_CARGO_REGISTRY_SRC,
            selected_count: selected.len(),
            succeeded_count: deleted_paths.len(),
            failed_count: failures.len(),
            reclaimed_bytes,
            reclaimed_human: crate::cache_lib::target_registry::human_size(reclaimed_bytes),
            deleted_paths,
            failures,
        };
        print_json(&output)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct GcPurgeGitCheckoutsOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    kind: &'static str,
    selected_count: usize,
    succeeded_count: usize,
    failed_count: usize,
    reclaimed_bytes: u64,
    reclaimed_human: String,
    deleted_paths: Vec<String>,
    failures: Vec<GcPurgeGitCheckoutsFailure>,
}

#[derive(Serialize)]
struct GcPurgeGitCheckoutsFailure {
    path: String,
    error: String,
}

/// `soldr gc purge --git-checkouts --all` — walk
/// `$CARGO_HOME/git/checkouts/<repo>/<commit>/` and `remove_dir_all` each
/// entry. Matches the registry-src purge contract: per-dir failures are
/// reported in the summary and the command exits 0 even if some
/// deletions failed, so scripts can scrape the structured output.
pub(crate) fn run_gc_purge_git_checkouts_command(
    purge_all: bool,
    json: bool,
) -> Result<(), SoldrError> {
    let cargo_home = crate::core::resolve_cargo_home().ok_or_else(|| {
        SoldrError::Other(
            "could not resolve $CARGO_HOME (no env var, no home directory)".to_string(),
        )
    })?;
    let now = crate::cache_lib::target_registry::current_unix_seconds()
        .map_err(|e| SoldrError::Other(format!("gc purge --git-checkouts clock error: {e}")))?;
    let entries = walk_cargo_git_checkouts(&cargo_home, now);

    if !json {
        eprintln!(
            "soldr gc purge --git-checkouts: cargo_home={}",
            cargo_home.display()
        );
        eprintln!(
            "soldr gc purge --git-checkouts: {} git checkout director{} on disk",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" }
        );
    }

    let mut selected: Vec<&GcListEntryOutput> = Vec::new();
    for entry in &entries {
        let should_delete = purge_all
            || prompt_yes_no_default_yes(&format!(
                "soldr gc: delete {} ({}, age {}) ? [Y/n] ",
                entry.path, entry.size_human, entry.age_human,
            ));
        if should_delete {
            selected.push(entry);
        }
    }

    let mut deleted_paths: Vec<String> = Vec::new();
    let mut failures: Vec<GcPurgeGitCheckoutsFailure> = Vec::new();
    let mut reclaimed_bytes: u64 = 0;
    for entry in &selected {
        let path = std::path::PathBuf::from(&entry.path);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                reclaimed_bytes = reclaimed_bytes.saturating_add(entry.size_bytes);
                deleted_paths.push(entry.path.clone());
            }
            Err(e) => failures.push(GcPurgeGitCheckoutsFailure {
                path: entry.path.clone(),
                error: e.to_string(),
            }),
        }
    }

    if !json {
        eprintln!(
            "soldr gc purge --git-checkouts: selected {}; succeeded {}; failed {}; reclaimed {}",
            selected.len(),
            deleted_paths.len(),
            failures.len(),
            crate::cache_lib::target_registry::human_size(reclaimed_bytes),
        );
        for failure in &failures {
            eprintln!(
                "soldr gc purge --git-checkouts: failed to delete {}: {}",
                failure.path, failure.error
            );
        }
    } else {
        let output = GcPurgeGitCheckoutsOutput {
            schema_version: GC_JSON_SCHEMA_VERSION,
            command: "gc",
            mode: "purge",
            kind: KIND_CARGO_GIT_CHECKOUTS,
            selected_count: selected.len(),
            succeeded_count: deleted_paths.len(),
            failed_count: failures.len(),
            reclaimed_bytes,
            reclaimed_human: crate::cache_lib::target_registry::human_size(reclaimed_bytes),
            deleted_paths,
            failures,
        };
        print_json(&output)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct GcPurgeTargetSubtreeOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    kind: &'static str,
    selected_count: usize,
    succeeded_count: usize,
    failed_count: usize,
    skipped_locked_targets: usize,
    reclaimed_bytes: u64,
    reclaimed_human: String,
    deleted_paths: Vec<String>,
    failures: Vec<GcPurgeTargetSubtreeFailure>,
}

#[derive(Serialize)]
struct GcPurgeTargetSubtreeFailure {
    path: String,
    error: String,
}

/// `soldr gc purge --target-incremental|--build-scripts|--doc|--subcommand-caches`.
/// Walks target subtree entries from the tracked target registry and deletes
/// only the selected derived kind. If any Cargo build lock is present under a
/// target, that whole target is skipped as an active-build guard.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) fn run_gc_purge_target_subtree_command(
    kind: GcListKindFilter,
    purge_all: bool,
    json: bool,
) -> Result<(), SoldrError> {
    if !kind.is_target_subtree() {
        return Err(SoldrError::Other(format!(
            "gc purge --kind {} is not an in-target subtree kind",
            kind.kind_name()
        )));
    }

    let paths = SoldrPaths::new()?;
    // Scoped so the handle is gone before the sizing walk, the prompt
    // loop, and the deletes below — this command never touches the
    // registry again, and the prompt waits on a human (#1681).
    let rows = super::daemon_registry_rows(&paths)?;
    let now = crate::cache_lib::target_registry::current_unix_seconds().map_err(|e| {
        SoldrError::Other(format!("gc purge {} clock error: {e}", kind.kind_name()))
    })?;

    let mut live_rows = Vec::new();
    let mut skipped_locked_targets = 0usize;
    for row in rows {
        if !row.path.exists() {
            continue;
        }
        if find_active_cargo_lock(&row.path).is_some() {
            skipped_locked_targets += 1;
            continue;
        }
        live_rows.push(row);
    }

    let entries = walk_cargo_target_subtrees(&live_rows, now, Some(kind));

    if !json {
        eprintln!(
            "soldr gc purge --kind {}: scanned {} tracked target dir{} ({} locked target{} skipped)",
            kind.kind_name(),
            live_rows.len(),
            if live_rows.len() == 1 { "" } else { "s" },
            skipped_locked_targets,
            if skipped_locked_targets == 1 { "" } else { "s" },
        );
        eprintln!(
            "soldr gc purge --kind {}: {} entr{} on disk",
            kind.kind_name(),
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" }
        );
    }

    let mut selected: Vec<&GcListEntryOutput> = Vec::new();
    for entry in &entries {
        let should_delete = purge_all
            || prompt_yes_no_default_yes(&format!(
                "soldr gc: delete {} ({}, age {}) ? [Y/n] ",
                entry.path, entry.size_human, entry.age_human,
            ));
        if should_delete {
            selected.push(entry);
        }
    }

    let mut deleted_paths: Vec<String> = Vec::new();
    let mut failures: Vec<GcPurgeTargetSubtreeFailure> = Vec::new();
    let mut reclaimed_bytes: u64 = 0;
    for entry in &selected {
        let path = std::path::PathBuf::from(&entry.path);
        match delete_path(&path) {
            Ok(()) => {
                reclaimed_bytes = reclaimed_bytes.saturating_add(entry.size_bytes);
                deleted_paths.push(entry.path.clone());
            }
            Err(e) => failures.push(GcPurgeTargetSubtreeFailure {
                path: entry.path.clone(),
                error: e.to_string(),
            }),
        }
    }

    if !json {
        eprintln!(
            "soldr gc purge --kind {}: selected {}; succeeded {}; failed {}; reclaimed {}",
            kind.kind_name(),
            selected.len(),
            deleted_paths.len(),
            failures.len(),
            crate::cache_lib::target_registry::human_size(reclaimed_bytes),
        );
        for failure in &failures {
            eprintln!(
                "soldr gc purge --kind {}: failed to delete {}: {}",
                kind.kind_name(),
                failure.path,
                failure.error
            );
        }
    } else {
        let output = GcPurgeTargetSubtreeOutput {
            schema_version: GC_JSON_SCHEMA_VERSION,
            command: "gc",
            mode: "purge",
            kind: kind.kind_name(),
            selected_count: selected.len(),
            succeeded_count: deleted_paths.len(),
            failed_count: failures.len(),
            skipped_locked_targets,
            reclaimed_bytes,
            reclaimed_human: crate::cache_lib::target_registry::human_size(reclaimed_bytes),
            deleted_paths,
            failures,
        };
        print_json(&output)?;
    }
    Ok(())
}

/// Prompt for, delete, and account for the selected candidates.
///
/// Takes the registry *path* rather than an open handle (#1681): the
/// prompting loop and the parallel `remove_dir_all` pool run with no
/// database open, and the handle is reacquired only for the bounded
/// row-removal phase at the end. Holding it across the deletion blocked
/// every other `state.sqlite3` opener for the duration, and the prompt is
/// unbounded — it waits on a human.
pub(super) fn run_gc_purge_candidates(
    paths: &SoldrPaths,
    candidates: &[crate::cache_lib::gc::GcCandidate],
    purge_all: bool,
    json: bool,
) -> Result<crate::cache_lib::gc::GcPurgeSummary, SoldrError> {
    let worker_count = gc_purge_worker_count();
    let (job_tx, job_rx) = mpsc::channel::<crate::cache_lib::gc::GcCandidate>();
    let (result_tx, result_rx) = mpsc::channel();
    let job_rx = Arc::new(Mutex::new(job_rx));
    let mut workers = Vec::new();
    for idx in 0..worker_count {
        let job_rx = Arc::clone(&job_rx);
        let result_tx = result_tx.clone();
        let builder = std::thread::Builder::new().name(format!("soldr-gc-{idx}"));
        workers.push(
            builder
                .spawn(move || loop {
                    // Bounded, and the queue lock is released between
                    // attempts so no worker waits on a sibling's receive.
                    let next = {
                        let rx = job_rx.lock();
                        crate::platform::wait::recv_for(&rx, std::time::Duration::from_millis(200))
                    };
                    match next {
                        Err(crate::platform::wait::WaitError::TimedOut(_)) => continue,
                        Ok(candidate) => {
                            let _ = result_tx
                                .send(crate::cache_lib::gc::delete_candidate_dir(candidate));
                        }
                        Err(_) => break,
                    }
                })
                .map_err(|e| SoldrError::Other(format!("failed to start gc worker: {e}")))?,
        );
    }
    drop(result_tx);

    let mut selected_count = 0usize;
    let mut completed_count = 0usize;
    let mut outcomes = Vec::new();

    for cand in candidates {
        let should_delete = purge_all || prompt_gc_purge_candidate(cand);
        if !should_delete {
            continue;
        }

        selected_count += 1;
        job_tx
            .send(cand.clone())
            .map_err(|e| SoldrError::Other(format!("failed to queue gc delete: {e}")))?;
    }
    drop(job_tx);

    while completed_count < selected_count {
        match result_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(outcome) => {
                completed_count += 1;
                outcomes.push(outcome);
                if !json {
                    print_gc_purge_progress(completed_count, selected_count);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !json {
                    print_gc_purge_progress(completed_count, selected_count);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    for worker in workers {
        crate::platform::wait::join_for(worker, crate::platform::wait::LONG_CHILD)
            .map_err(|_| SoldrError::Other("gc worker panicked or did not finish".to_string()))?;
    }

    if !json && selected_count > 0 {
        eprintln!();
    }

    let (summary, removed_rows) = crate::cache_lib::gc::summarize_purge_outcomes(outcomes);
    super::daemon_remove_registry_rows(paths, removed_rows)?;
    Ok(summary)
}

fn gc_purge_worker_count() -> usize {
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    gc_purge_worker_count_for(available)
}

pub(crate) fn gc_purge_worker_count_for(available_parallelism: usize) -> usize {
    available_parallelism.clamp(1, 4)
}

fn print_gc_purge_progress(completed: usize, selected: usize) {
    eprint!("\rsoldr gc purge: deleting selected targets {completed}/{selected}");
    let _ = std::io::stderr().flush();
}

pub(super) fn gc_total_reclaimable_bytes(candidates: &[crate::cache_lib::gc::GcCandidate]) -> u64 {
    candidates.iter().map(|c| c.size_bytes).sum()
}

pub(super) fn print_gc_summary(
    db_path: &std::path::Path,
    report: &crate::cache_lib::gc::GcReport,
    total_reclaimable_bytes: u64,
) {
    println!("soldr gc: registry: {}", db_path.display());
    println!(
        "soldr gc: eligible: {} target dir{}; reclaimable: {}",
        report.candidates.len(),
        if report.candidates.len() == 1 {
            ""
        } else {
            "s"
        },
        crate::cache_lib::target_registry::human_size(total_reclaimable_bytes)
    );
    println!(
        "soldr gc: skipped: {}; dropped missing rows: {}",
        report.skipped.len(),
        report.dropped_missing
    );

    // soldr#2134 asks for merged-worktree targets to be reclaimed
    // *eagerly*, on the grounds that their build output has no future
    // value at any level of disk pressure. Deleting outside pressure is
    // a wider change than the ordering work in #2156/#2166 and wants its
    // own argument, but the opportunity is worth surfacing: a worktree
    // total sitting in the report is what lets someone decide to run
    // `gc purge` before the volume gets tight, rather than finding out
    // when a build blocks.
    let (worktree_count, worktree_bytes) = report
        .candidates
        .iter()
        .filter(|candidate| candidate.in_worktree)
        .fold((0usize, 0u64), |(count, bytes), candidate| {
            (count + 1, bytes.saturating_add(candidate.size_bytes))
        });
    if worktree_count > 0 {
        println!(
            "soldr gc: of those, {worktree_count} in linked worktree{} ({}) — \
             cheapest to reclaim, and taken first",
            if worktree_count == 1 { "" } else { "s" },
            crate::cache_lib::target_registry::human_size(worktree_bytes)
        );
    }

    if report.candidates.is_empty() {
        println!("soldr gc: nothing to reclaim.");
    } else {
        // soldr#2134: show what `gc purge` will actually take, in the
        // order it will take it. This used to list the five *largest*
        // candidates, which read as "these go first" and was wrong:
        // eviction orders worktree -> coldest -> size, and size is the
        // last key. `report.candidates` is already in eviction order
        // (`cache_lib::gc::order_candidates` runs inside the scan).
        //
        // The JSON keeps its `largest_candidates` field — that name is
        // accurate and something may consume it — but a human reading
        // this is about to run `gc purge`, and the question they have
        // is which directory is next, not which is biggest.
        println!("soldr gc: next to be reclaimed (eviction order):");
        for cand in report.candidates.iter().take(5) {
            // soldr#4033: hardlinked bytes are only freed when the last
            // link goes, so show what deleting this target really frees.
            let sharing = super::target_dedupe::measure_target_sharing(&cand.path);
            println!(
                "  {}{}  size={}  shared={}  unique={}  last_used={}",
                cand.path.display(),
                if cand.in_worktree {
                    "  [linked worktree]"
                } else {
                    ""
                },
                crate::cache_lib::target_registry::human_size(cand.size_bytes),
                crate::cache_lib::target_registry::human_size(sharing.shared_bytes),
                crate::cache_lib::target_registry::human_size(sharing.unique_bytes),
                crate::cache_lib::target_registry::human_age(cand.age_seconds),
            );
        }
        println!("Run 'soldr gc purge' to delete eligible target directories.");
    }
}

pub(super) fn print_gc_purge_scan(
    db_path: &std::path::Path,
    report: &crate::cache_lib::gc::GcReport,
    total_reclaimable_bytes: u64,
) {
    eprintln!(
        "soldr gc purge: scanned registry at {} ({} candidate dir{}, {} skipped, {} dropped missing, {} reclaimable)",
        db_path.display(),
        report.candidates.len(),
        if report.candidates.len() == 1 { "" } else { "s" },
        report.skipped.len(),
        report.dropped_missing,
        crate::cache_lib::target_registry::human_size(total_reclaimable_bytes)
    );

    if report.candidates.is_empty() {
        eprintln!("soldr gc purge: nothing to delete.");
    } else {
        eprintln!("soldr gc purge: candidates");
        for cand in &report.candidates {
            eprintln!(
                "  {}  size={}  age={}",
                cand.path.display(),
                crate::cache_lib::target_registry::human_size(cand.size_bytes),
                crate::cache_lib::target_registry::human_age(cand.age_seconds),
            );
        }
    }
}

pub(super) fn print_gc_purge_result(
    summary: &crate::cache_lib::gc::GcPurgeSummary,
    error_log_path: Option<&std::path::Path>,
) {
    eprintln!(
        "soldr gc purge: selected {}; succeeded {}; failed {}; reclaimed {}",
        summary.selected_count,
        summary.succeeded_count,
        summary.failed_count,
        crate::cache_lib::target_registry::human_size(summary.reclaimed_bytes)
    );
    if let Some(path) = error_log_path {
        eprintln!(
            "soldr gc purge: detailed deletion errors written to {}",
            path.display()
        );
    }
}

pub(super) fn gc_largest_candidates(
    candidates: &[crate::cache_lib::gc::GcCandidate],
    limit: usize,
) -> Vec<crate::cache_lib::gc::GcCandidate> {
    let mut largest = candidates.to_vec();
    largest.sort_by(|a, b| {
        b.size_bytes
            .cmp(&a.size_bytes)
            .then_with(|| a.path.cmp(&b.path))
    });
    largest.truncate(limit);
    largest
}

pub(super) fn gc_candidate_output(c: crate::cache_lib::gc::GcCandidate) -> GcCandidateOutput {
    GcCandidateOutput {
        path: c.path.display().to_string(),
        size_human: crate::cache_lib::target_registry::human_size(c.size_bytes),
        size_bytes: c.size_bytes,
        age_human: crate::cache_lib::target_registry::human_age(c.age_seconds),
        age_seconds: c.age_seconds,
        eligible: c.eligible,
        reason: c.reason,
        kind: KIND_CARGO_TARGET,
        purge_safety: PURGE_SAFETY_DERIVED,
    }
}

fn prompt_gc_purge_candidate(cand: &crate::cache_lib::gc::GcCandidate) -> bool {
    prompt_yes_no_default_yes(&format!(
        "soldr gc: delete {} ({}, age {}) ? [Y/n] ",
        cand.path.display(),
        crate::cache_lib::target_registry::human_size(cand.size_bytes),
        crate::cache_lib::target_registry::human_age(cand.age_seconds),
    ))
}

pub(super) fn prompt_yes_no_default_yes(prompt: &str) -> bool {
    use std::io::{BufRead, Write};
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return false;
    }
    parse_gc_purge_answer(&line)
}

pub(crate) fn parse_gc_purge_answer(input: &str) -> bool {
    matches!(input.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes")
}

fn find_active_cargo_lock(target_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    match crate::cache_lib::cargo_lock::probe(target_dir) {
        Ok(crate::cache_lib::cargo_lock::CargoLockProbe::Active(path)) => Some(path),
        Ok(crate::cache_lib::cargo_lock::CargoLockProbe::Idle(_)) => None,
        Err(_) => Some(target_dir.join(".cargo-lock")),
    }
}

fn delete_path(path: &std::path::Path) -> Result<(), std::io::Error> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if metadata.is_dir() {
        match crate::cache_lib::cargo_lock::probe(path) {
            Ok(crate::cache_lib::cargo_lock::CargoLockProbe::Idle(_guard)) => {
                std::fs::remove_dir_all(path)
            }
            Ok(crate::cache_lib::cargo_lock::CargoLockProbe::Active(lock)) => {
                Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!(
                        "active cargo lock at {}; refusing to delete",
                        lock.display()
                    ),
                ))
            }
            Err(error) => Err(std::io::Error::other(format!(
                "cargo lock probe failed closed: {error}"
            ))),
        }
    } else {
        std::fs::remove_file(path)
    }
}

/// Resolve the configured `gc.allowlist_roots`, falling back to
/// `~/dev` when unset.
pub(super) fn resolve_gc_dev_roots(
    paths: &SoldrPaths,
) -> Result<Vec<std::path::PathBuf>, crate::core::SoldrError> {
    let config = paths
        .load_config()
        .map_err(|error| crate::core::SoldrError::Other(error.to_string()))?;
    let configured = config
        .gc
        .allowlist_roots
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| !r.trim().is_empty())
        .map(|r| crate::core::expand_user_home(&r))
        .collect::<Vec<_>>();
    if !configured.is_empty() {
        return Ok(configured);
    }
    if let Ok(home) = crate::core::user_home_dir() {
        return Ok(default_gc_dev_roots(&home));
    }
    Ok(Vec::new())
}

/// Default GC allowlist when `[gc] allowlist_roots` is unset: `~/dev` plus
/// the agent worktree root `~/.clud/worktrees` (soldr#4031), where idle
/// parallel-agent worktrees accumulate full `target/` dirs.
pub(super) fn default_gc_dev_roots(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    vec![home.join("dev"), home.join(".clud").join("worktrees")]
}
