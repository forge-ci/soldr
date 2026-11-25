//! Unit coverage split from `gc.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::super::target_registry::TargetRegistry;
use super::*;
use fs2::FileExt;
use std::path::PathBuf;
use tempfile::tempdir;

fn candidate(path: &str, in_worktree: bool, age_seconds: i64, size_bytes: u64) -> GcCandidate {
    GcCandidate {
        path: PathBuf::from(path),
        size_bytes,
        age_seconds,
        in_worktree,
        eligible: true,
        reason: None,
    }
}

#[test]
fn a_linked_worktree_target_is_recognised_by_its_git_file() {
    // A linked worktree's root holds `.git` as a FILE; a primary
    // checkout holds it as a directory. That is the whole signal.
    let dir = tempdir().unwrap();
    let (worktree_root, worktree_target) = make_workspace(dir.path(), "wt", 16);
    std::fs::write(
        worktree_root.join(".git"),
        b"gitdir: /repo/.git/worktrees/wt",
    )
    .unwrap();

    let (primary_root, primary_target) = make_workspace(dir.path(), "primary", 16);
    std::fs::create_dir_all(primary_root.join(".git")).unwrap();

    assert!(in_linked_git_worktree(&worktree_target));
    assert!(!in_linked_git_worktree(&primary_target));
}

#[test]
fn a_checkout_with_no_git_at_all_is_not_a_worktree() {
    // Tarball/zip checkouts are ordinary repos as far as this is
    // concerned -- absence of `.git` must not read as "ephemeral".
    let dir = tempdir().unwrap();
    let (_, target) = make_workspace(dir.path(), "plain", 16);
    assert!(!in_linked_git_worktree(&target));
}

/// soldr#2134 clause 3: "ideally never the repo the current build
/// belongs to."
///
/// Nothing names the current build explicitly. What protects it is that
/// `effective_age_seconds` takes `registry_age.min(fs_age)`, so a target
/// written moments ago reads as age ~0 and is dropped by the
/// `older_than_seconds` gate *before* ordering ever runs.
///
/// That gate is load-bearing and easy to break by accident, because the
/// worktree-first rule added for this same issue would otherwise put a
/// just-built worktree target at the **front** of the eviction list --
/// and soldr's own workflow builds in `.claude/worktrees/`. Reordering
/// the tier ahead of the age skip would therefore reintroduce exactly
/// the "most expensive possible choice" this issue was filed about, on
/// the hottest cache on the volume. Pin the interaction.
#[test]
fn a_freshly_built_worktree_target_is_never_a_candidate() {
    let dir = tempdir().unwrap();
    let registry = TargetRegistry::open_in_memory().unwrap();
    let (worktree_root, fresh_target) = make_workspace(dir.path(), "hot-worktree", 4096);
    std::fs::write(
        worktree_root.join(".git"),
        b"gitdir: /repo/.git/worktrees/hot-worktree",
    )
    .unwrap();
    // It really is the tier the ordering prefers -- otherwise this test
    // would pass for the wrong reason.
    assert!(in_linked_git_worktree(&fresh_target));

    let now = current_unix_seconds().unwrap();
    // The registry row is ancient; only the on-disk mtime says the build
    // just touched it. That is the reported shape: `Cargo.lock` and the
    // registry can both be stale while the cache is the hottest around.
    registry
        .upsert_with_time(&fresh_target, now - 30 * 86_400)
        .unwrap();
    // Deliberately no `backdate` here: the directory keeps the mtime it
    // was just created with, which is the whole point.

    let opts = GcOptions {
        older_than_seconds: 7 * 86_400,
        larger_than_bytes: 0,
        dev_roots: vec![dir.path().to_path_buf()],
        dry_run: true,
    };
    let report = scan(&registry, &opts).unwrap();

    assert!(
        report.candidates.is_empty(),
        "a target written moments ago must not be evictable, however cold \
             its registry row and however evictable its tier: {:?}",
        report.candidates,
    );
    assert_eq!(report.skipped.len(), 1, "{report:?}");
}

#[test]
fn the_reported_case_takes_the_cold_worktree_not_the_hot_primary() {
    // The reported failure, with its real ages: a 57.7 GB cache written
    // minutes earlier was purged while a worktree idle ~2 days was kept.
    // Note the worktree is the *colder* of the two, so coldness alone
    // orders this correctly -- the tier is not what fixes the report.
    let mut candidates = vec![
        candidate("/dev/soldr/target", false, 600, 57_700_000_000),
        candidate(
            "/dev/clud/.claude/worktrees/issue-621/target",
            true,
            170_000,
            5_600_000_000,
        ),
    ];
    order_candidates(&mut candidates);
    assert!(
        candidates[0].in_worktree,
        "the cold worktree must be taken before the hot primary: {candidates:?}"
    );
}

#[test]
fn a_live_worktree_does_not_outrank_a_colder_primary_checkout() {
    // soldr#2156 promoted every worktree unconditionally, so this
    // ordering came out backwards: a worktree built 100s ago was taken
    // ahead of a primary checkout idle for hours. Both rebuild at full
    // cost, so the colder one is the cheaper loss.
    let mut candidates = vec![
        candidate("/dev/repo/target", false, 10_000, 1_000),
        candidate("/dev/repo/.claude/worktrees/live/target", true, 100, 9_000),
    ];
    order_candidates(&mut candidates);
    assert_eq!(
        candidates[0].path,
        PathBuf::from("/dev/repo/target"),
        "a worktree in active use must not be promoted over a colder \
             primary checkout: {candidates:?}"
    );
}

#[test]
fn an_abandoned_worktree_still_outranks_a_colder_primary_checkout() {
    // The tier's actual purpose, kept intact: once a worktree has gone
    // cold past the threshold it is very likely merged and will never be
    // rebuilt, so it goes first even against an older primary checkout.
    let stale = WORKTREE_TIER_AGE_SECONDS + 1;
    let mut candidates = vec![
        candidate("/dev/repo/target", false, stale * 2, 1_000),
        candidate(
            "/dev/repo/.claude/worktrees/done/target",
            true,
            stale,
            9_000,
        ),
    ];
    order_candidates(&mut candidates);
    assert!(
        candidates[0].in_worktree,
        "an abandoned worktree is still the cheapest thing to lose: \
             {candidates:?}"
    );
}

#[test]
fn within_a_tier_the_coldest_goes_first_and_size_only_breaks_ties() {
    let mut candidates = vec![
        candidate("/warm-huge", false, 100, 900),
        candidate("/cold-small", false, 9_000, 1),
        candidate("/tie-small", false, 9_000, 500),
    ];
    order_candidates(&mut candidates);
    assert_eq!(candidates[0].path, PathBuf::from("/tie-small"));
    assert_eq!(candidates[1].path, PathBuf::from("/cold-small"));
    assert_eq!(
        candidates[2].path,
        PathBuf::from("/warm-huge"),
        "size must not outrank coldness: {candidates:?}"
    );
}

fn make_workspace(root: &Path, name: &str, size_bytes: u64) -> (PathBuf, PathBuf) {
    let workspace = root.join(name);
    let target = workspace.join("target");
    std::fs::create_dir_all(&target).unwrap();
    // Cargo always writes this; the fixture omitted it, which made these
    // targets indistinguishable from any directory called `target`.
    std::fs::write(target.join("CACHEDIR.TAG"), b"Signature: 8a477f597d28d172").unwrap();
    std::fs::write(target.join("blob"), vec![0u8; size_bytes as usize]).unwrap();
    (workspace, target)
}

/// #1681: a GC pass must not hold the state-database handle across
/// its long filesystem/prompting phases.
///
/// Under SQLite this proves that a snapshot can be followed by a concurrent
/// registry write. It cannot prove connection release: holding a connection
/// without a transaction no longer excludes other writers.
#[test]
fn snapshot_releases_the_handle_before_long_work() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("state.sqlite3");
    let (_, target) = make_workspace(dir.path(), "repo-a", 512);
    {
        let registry = TargetRegistry::open(&db).unwrap();
        registry.upsert_with_time(&target, 100).unwrap();
    }

    let snapshot = snapshot_registry(&db).unwrap();
    assert_eq!(snapshot.rows.len(), 1, "the live row must be snapshotted");

    // Stand-in for the long phase: GC owns the rows now and is off
    // sizing, prompting, and deleting. A state write must still get
    // through.
    //
    // Probe from an independent connection with a bounded wait. A held
    // write transaction, rather than a held connection, would contend
    // through SQLite's busy timeout. Transaction-lock tests live in
    // state_store; this test checks the GC operation sequence.
    let (tx, rx) = std::sync::mpsc::channel();
    let probe_db = db.clone();
    let probe_target = target.clone();
    std::thread::spawn(move || {
        let _ = tx.send(
            TargetRegistry::open(&probe_db)
                .and_then(|reg| reg.upsert_with_time(&probe_target, 200)),
        );
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(result) => result.expect("concurrent state write must succeed"),
        Err(_) => panic!(
            "state.sqlite3 was still locked while GC did its filesystem work — \
                 the scan is holding its handle across the long phase (#1681)",
        ),
    }

    let opts = GcOptions {
        older_than_seconds: 0,
        larger_than_bytes: 0,
        dev_roots: vec![dir.path().to_path_buf()],
        dry_run: true,
    };
    let report = scan_snapshot(snapshot, &opts).unwrap();
    assert_eq!(
        report.candidates.len() + report.skipped.len(),
        1,
        "the snapshotted row must still be evaluated after the handle was released",
    );
}

#[test]
fn scan_drops_missing_rows_and_keeps_candidates() {
    let dir = tempdir().unwrap();
    let registry = TargetRegistry::open_in_memory().unwrap();
    let (_, target) = make_workspace(dir.path(), "repo-a", 512);
    let missing = dir.path().join("ghost").join("target");

    let now = current_unix_seconds().unwrap();
    registry
        .upsert_with_time(&target, now - 30 * 86_400)
        .unwrap();
    registry
        .upsert_with_time(&missing, now - 30 * 86_400)
        .unwrap();
    // soldr#2134: cold on disk as well, not just in the registry.
    backdate(&target, 30 * 86_400);

    let opts = GcOptions {
        older_than_seconds: 10 * 86_400,
        larger_than_bytes: 0, // include everything
        dev_roots: vec![dir.path().to_path_buf()],
        dry_run: true,
    };
    let report = scan(&registry, &opts).unwrap();
    assert_eq!(report.dropped_missing, 1);
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].path, target);
}

#[test]
fn scan_filters_by_age_and_size() {
    let dir = tempdir().unwrap();
    let registry = TargetRegistry::open_in_memory().unwrap();
    let (_, target_small) = make_workspace(dir.path(), "small", 16);
    let (_, target_big) = make_workspace(dir.path(), "big", 4096);

    let now = current_unix_seconds().unwrap();
    // Small but stale
    registry
        .upsert_with_time(&target_small, now - 30 * 86_400)
        .unwrap();
    // Big but fresh
    registry.upsert_with_time(&target_big, now - 60).unwrap();

    let opts = GcOptions {
        older_than_seconds: 10 * 86_400,
        larger_than_bytes: 1024,
        dev_roots: vec![dir.path().to_path_buf()],
        dry_run: true,
    };
    let report = scan(&registry, &opts).unwrap();
    assert!(report.candidates.is_empty());
    assert_eq!(report.skipped.len(), 2);
}

/// Backdate a directory's mtime so the filesystem agrees with a stale
/// registry stamp. Before soldr#2134 the scan read only the stamp, so
/// fixtures could leave the real mtime at "just now" without noticing;
/// now a test that means "this target is cold" has to say so on both
/// signals.
fn backdate(path: &Path, seconds_ago: u64) {
    let when = SystemTime::now() - Duration::from_secs(seconds_ago);
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(when))
        .expect("backdate target mtime");
}

#[test]
fn effective_age_takes_the_more_recent_of_registry_and_mtime() {
    // soldr#2134. The registry stamp is soldr's own bookkeeping and goes
    // stale while a directory stays hot (bare `cargo`, or a lost daemon
    // touch). Whichever signal is more recent wins, so a hot cache cannot
    // be ranked cold by a stale stamp alone.
    let dir = tempdir().unwrap();
    let (_, target) = make_workspace(dir.path(), "repo", 64);
    let now = current_unix_seconds().unwrap();

    // Stamp says 30 days idle; the directory was written just now.
    let age = effective_age_seconds(&target, now - 30 * 86_400, now);
    assert!(
        age < 3600,
        "a freshly written target must not look 30 days old, got {age}s"
    );

    // Both signals old: evaluate 60 days in the future so the filesystem
    // mtime is stale too, without needing to backdate the directory.
    let future = now + 60 * 86_400;
    let age = effective_age_seconds(&target, now - 30 * 86_400, future);
    assert!(
        age >= 59 * 86_400,
        "a genuinely cold target must still read as old, got {age}s"
    );
}

#[test]
fn scan_still_evicts_a_target_that_is_cold_by_both_signals() {
    // The control for the fix: sparing hot caches must not turn into
    // sparing everything. Stale stamp AND stale mtime => still a
    // candidate.
    let dir = tempdir().unwrap();
    let registry = TargetRegistry::open_in_memory().unwrap();
    let (_, target) = make_workspace(dir.path(), "cold", 4096);

    let now = current_unix_seconds().unwrap();
    registry
        .upsert_with_time(&target, now - 30 * 86_400)
        .unwrap();
    backdate(&target, 30 * 86_400);

    let opts = GcOptions {
        older_than_seconds: 10 * 86_400,
        larger_than_bytes: 1024,
        dev_roots: vec![dir.path().to_path_buf()],
        dry_run: true,
    };
    let report = scan(&registry, &opts).unwrap();

    assert_eq!(
        report.candidates.len(),
        1,
        "a target cold by both signals must remain evictable: {report:?}"
    );
}

#[test]
fn scan_spares_a_hot_target_whose_registry_stamp_is_stale() {
    // The reported failure: a 57.7 GB cache written minutes earlier was
    // purged because its registry row was old, while a merged worktree's
    // was kept. Age must not come from the stamp alone.
    let dir = tempdir().unwrap();
    let registry = TargetRegistry::open_in_memory().unwrap();
    let (_, target) = make_workspace(dir.path(), "hot", 4096);

    let now = current_unix_seconds().unwrap();
    registry
        .upsert_with_time(&target, now - 30 * 86_400)
        .unwrap();

    let opts = GcOptions {
        older_than_seconds: 10 * 86_400,
        larger_than_bytes: 1024,
        dev_roots: vec![dir.path().to_path_buf()],
        dry_run: true,
    };
    let report = scan(&registry, &opts).unwrap();

    assert!(
        report.candidates.is_empty(),
        "a target written moments ago must not be an eviction candidate: {:?}",
        report.candidates
    );
    assert_eq!(report.skipped.len(), 1);
}

#[test]
fn dry_run_purge_does_not_delete() {
    let dir = tempdir().unwrap();
    let registry = TargetRegistry::open_in_memory().unwrap();
    let (_, target) = make_workspace(dir.path(), "repo", 256);
    registry.upsert_with_time(&target, 100).unwrap();

    let removed = purge_one(&registry, &target, true).unwrap();
    assert!(!removed);
    assert!(target.exists(), "dry-run must not delete");
    // Registry row preserved on dry-run.
    assert!(registry.get(&target).unwrap().is_some());
}

#[test]
fn purge_refuses_a_directory_that_only_looks_like_a_target() {
    // #1671: resolution is name-based, so GC must not delete a directory
    // just because it is called `target`.
    let dir = tempdir().unwrap();
    let registry = TargetRegistry::open_in_memory().unwrap();
    let impostor = dir.path().join("some-repo").join("target");
    std::fs::create_dir_all(&impostor).unwrap();
    std::fs::write(impostor.join("important.txt"), b"not cargo output").unwrap();
    registry.upsert_with_time(&impostor, 100).unwrap();

    let result = purge_one(&registry, &impostor, false);

    assert!(result.is_err(), "must refuse a non-cargo directory");
    assert!(impostor.exists(), "the directory must survive");
    assert!(
        impostor.join("important.txt").exists(),
        "its contents must survive"
    );
}

#[test]
fn purge_deletes_directory_and_row() {
    let dir = tempdir().unwrap();
    let registry = TargetRegistry::open_in_memory().unwrap();
    let (_, target) = make_workspace(dir.path(), "repo", 256);
    registry.upsert_with_time(&target, 100).unwrap();

    let removed = purge_one(&registry, &target, false).unwrap();
    assert!(removed);
    assert!(!target.exists());
    assert!(registry.get(&target).unwrap().is_none());
}

#[test]
fn purge_outcomes_remove_only_successful_rows() {
    let dir = tempdir().unwrap();
    let registry = TargetRegistry::open_in_memory().unwrap();
    let (_, ok_target) = make_workspace(dir.path(), "ok", 256);
    let (_, failed_target) = make_workspace(dir.path(), "failed", 512);
    registry.upsert_with_time(&ok_target, 100).unwrap();
    registry.upsert_with_time(&failed_target, 100).unwrap();

    let ok_candidate = GcCandidate {
        path: ok_target.clone(),
        size_bytes: 256,
        age_seconds: 1000,
        in_worktree: false,
        eligible: true,
        reason: None,
    };
    let failed_candidate = GcCandidate {
        path: failed_target.clone(),
        size_bytes: 512,
        age_seconds: 1000,
        in_worktree: false,
        eligible: true,
        reason: None,
    };
    let summary = apply_purge_outcomes(
        &registry,
        vec![
            GcDeleteOutcome {
                candidate: ok_candidate,
                removed: true,
                error: None,
            },
            GcDeleteOutcome {
                candidate: failed_candidate,
                removed: false,
                error: Some("permission denied".to_string()),
            },
        ],
    )
    .unwrap();

    assert_eq!(summary.selected_count, 2);
    assert_eq!(summary.succeeded_count, 1);
    assert_eq!(summary.failed_count, 1);
    assert_eq!(summary.reclaimed_bytes, 256);
    assert!(registry.get(&ok_target).unwrap().is_none());
    assert!(registry.get(&failed_target).unwrap().is_some());
}

#[test]
fn daemon_purge_summary_removes_already_absent_rows_without_counting_bytes() {
    let target = PathBuf::from("/tmp/already-absent-target");
    let (summary, removed_rows) = summarize_purge_outcomes(vec![GcDeleteOutcome {
        candidate: GcCandidate {
            path: target.clone(),
            size_bytes: 512,
            age_seconds: 100,
            in_worktree: false,
            eligible: true,
            reason: None,
        },
        removed: false,
        error: None,
    }]);

    assert_eq!(summary.succeeded_count, 1);
    assert_eq!(summary.reclaimed_bytes, 0);
    assert!(summary.deleted_paths.is_empty());
    assert_eq!(removed_rows, vec![target]);
}

#[test]
fn gc_error_log_includes_failure_details() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("repo").join("target");
    let failure = GcPurgeFailure {
        candidate: GcCandidate {
            path: target.clone(),
            size_bytes: 1024,
            age_seconds: 42,
            in_worktree: false,
            eligible: true,
            reason: None,
        },
        error: "permission denied".to_string(),
    };

    let log_path = write_gc_error_log(
        dir.path(),
        &["soldr".to_string(), "gc".to_string(), "purge".to_string()],
        &[failure],
    )
    .unwrap();
    let raw = std::fs::read_to_string(log_path).unwrap();

    assert!(raw.contains("command_args="));
    assert!(raw.contains(&target.display().to_string()));
    assert!(raw.contains("size_bytes=1024"));
    assert!(raw.contains("permission denied"));
}

#[test]
fn gc_log_cleanup_removes_entries_past_retention() {
    // Use explicit mtime manipulation rather than wall-clock sleeps:
    // APFS / NTFS / ext4 all have different mtime resolutions, and a
    // sleep-based version of this test was flaking on macOS CI when
    // the cleanup wall-clock crossed the retention boundary for both
    // files. `FileTimes::set_modified` is millisecond-precise on every
    // supported platform.
    let dir = tempdir().unwrap();
    let stale = dir.path().join("stale.log");
    let fresh = dir.path().join("fresh.log");
    std::fs::write(&stale, b"old").unwrap();
    std::fs::write(&fresh, b"new").unwrap();

    let now = SystemTime::now();
    let stale_mtime = now.checked_sub(Duration::from_secs(60)).unwrap();
    let fresh_mtime = now;
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(stale_mtime)
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&fresh)
        .unwrap()
        .set_modified(fresh_mtime)
        .unwrap();

    let removed = cleanup_old_gc_logs_with_retention(dir.path(), Duration::from_secs(30))
        .expect("cleanup old logs");

    assert_eq!(removed, 1);
    assert!(!stale.exists());
    assert!(fresh.exists());
}

#[test]
fn safety_guard_short_circuits_on_active_lock() {
    let dir = tempdir().unwrap();
    let registry = TargetRegistry::open_in_memory().unwrap();
    let (_, target) = make_workspace(dir.path(), "repo", 4096);
    let cargo_lock = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(target.join(".cargo-lock"))
        .unwrap();
    cargo_lock.try_lock_exclusive().unwrap();

    let now = current_unix_seconds().unwrap();
    registry
        .upsert_with_time(&target, now - 30 * 86_400)
        .unwrap();

    let opts = GcOptions {
        older_than_seconds: 10 * 86_400,
        larger_than_bytes: 0,
        dev_roots: vec![dir.path().to_path_buf()],
        dry_run: true,
    };
    let report = scan(&registry, &opts).unwrap();
    assert!(report.candidates.is_empty());
    assert_eq!(report.skipped.len(), 1);
}

#[test]
fn parse_duration_handles_units() {
    assert_eq!(parse_duration("10d").unwrap(), 10 * 86_400);
    assert_eq!(parse_duration("4h").unwrap(), 4 * 3_600);
    assert_eq!(parse_duration("90s").unwrap(), 90);
    assert_eq!(parse_duration("3W").unwrap(), 3 * 7 * 86_400);
    assert!(parse_duration("garbage").is_err());
}

#[test]
fn parse_size_handles_units() {
    assert_eq!(parse_size("256M").unwrap(), 256 * 1024 * 1024);
    assert_eq!(parse_size("1GB").unwrap(), 1024 * 1024 * 1024);
    assert_eq!(parse_size("512KiB").unwrap(), 512 * 1024);
    assert_eq!(parse_size("42").unwrap(), 42);
    assert!(parse_size("nopes").is_err());
}

#[test]
fn startup_warning_throttle_blocks_repeats() {
    let dir = tempdir().unwrap();
    let marker = dir.path().join(".gc_warning_marker");
    // First call: due.
    assert!(startup_warning_due(&marker).unwrap());
    // After we touch it, next call should not be due.
    touch_startup_warning_marker(&marker).unwrap();
    assert!(!startup_warning_due(&marker).unwrap());
}

#[test]
fn empty_startup_scan_is_throttled() {
    let dir = tempdir().unwrap();
    let marker = dir.path().join(".gc_warning_marker");
    let registry = TargetRegistry::open_in_memory().unwrap();
    let opts = GcOptions {
        older_than_seconds: 10 * 86_400,
        larger_than_bytes: 1024,
        dev_roots: vec![dir.path().to_path_buf()],
        dry_run: true,
    };

    assert_eq!(
        maybe_build_startup_warning(&registry, &opts, &marker).unwrap(),
        None
    );
    assert!(marker.is_file(), "successful empty scan must write marker");
    assert!(!startup_warning_due(&marker).unwrap());
}

#[test]
fn detached_target_reclamation_leaves_recreated_build_path_intact() {
    let scratch = tempfile::tempdir().unwrap();
    let target = scratch.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("CACHEDIR.TAG"), b"Signature: 8a477f597d28d172").unwrap();
    let candidate = candidate(target.to_str().unwrap(), false, 60, 1);
    let (outcome, detached) = detach_candidate_dir(candidate, scratch.path());
    assert!(outcome.removed, "{:?}", outcome.error);
    let detached = detached.expect("same-volume detachment");
    assert!(!target.exists());
    let staging = detached.path.clone();
    let paths = crate::core::SoldrPaths::with_root(scratch.path().to_path_buf());
    let report = super::super::trash_gc::sweep_trash(&paths).unwrap();
    assert_eq!(report.removed, 0);
    assert!(staging.join("target/CACHEDIR.TAG").is_file());

    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("live"), b"new build").unwrap();
    let outcome = reclaim_detached_candidate(outcome, detached);
    assert!(outcome.removed, "{:?}", outcome.error);
    assert!(!staging.exists());
    assert_eq!(std::fs::read(target.join("live")).unwrap(), b"new build");
}
