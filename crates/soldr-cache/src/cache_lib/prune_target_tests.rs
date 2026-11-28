//! Tests for prune_target.rs. Included by prune_target.rs via
//! `#[path = "prune_target_tests.rs"] mod tests;` so the prune_target
//! file stays within the project LOC budget.

use super::*;
use std::fs::File;
use tempfile::tempdir;

/// Convert a unix timestamp to a [`std::time::SystemTime`].
fn system_time_from_unix(seconds: u64) -> std::time::SystemTime {
    UNIX_EPOCH + std::time::Duration::from_secs(seconds)
}

fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    File::create(path).unwrap();
}

fn hold_cargo_lock(path: &Path) -> kernal_api::platform::fs::OwnedFileLock {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    kernal_api::platform::fs::try_lock_exclusive_owned(file).unwrap()
}

fn touch_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
}

/// Set the mtime on a file. This is intentionally file-only because
/// `set_modified` on a directory handle requires privileged access
/// on Windows.
fn set_mtime(path: &Path, unix_seconds: u64) {
    let when = system_time_from_unix(unix_seconds);
    let f = File::options()
        .write(true)
        .open(path)
        .expect("set_mtime: open writable handle");
    f.set_modified(when).expect("set_mtime: set_modified");
}

#[test]
fn split_hash_suffix_recognizes_directory_names() {
    let parsed = split_hash_suffix("foo-abc1234567890");
    assert_eq!(parsed, Some(("foo", "abc1234567890")));
}

#[test]
fn split_hash_suffix_recognizes_compound_names() {
    let parsed = split_hash_suffix("zccache_daemon-3ohgys91001go");
    assert_eq!(parsed, Some(("zccache_daemon", "3ohgys91001go")));
}

#[test]
fn split_hash_suffix_strips_known_extensions() {
    let parsed = split_hash_suffix("libfoo-abcdef1234567.rlib");
    assert_eq!(parsed, Some(("libfoo", "abcdef1234567")));
}

#[test]
fn split_hash_suffix_rejects_short_hashes() {
    assert!(split_hash_suffix("foo-abc123").is_none());
}

#[test]
fn split_hash_suffix_rejects_non_alphanumeric() {
    assert!(split_hash_suffix("foo-bar.baz_quux12345").is_none());
}

#[test]
fn split_hash_suffix_rejects_no_dash() {
    assert!(split_hash_suffix("no_hash_here_at_all").is_none());
}

#[test]
fn single_entry_prefix_is_kept() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    let single = target
        .join("debug")
        .join(".fingerprint")
        .join("foo-abc1234567890");
    touch_dir(&single);

    let report = prune_target(&PruneTargetOptions {
        target_dir: target,
        dry_run: false,
        keep_latest: false,
    })
    .unwrap();

    assert_eq!(report.scanned, 1);
    assert_eq!(report.kept, 1);
    assert_eq!(report.deleted, 0);
    assert!(single.exists());
}

#[test]
fn multi_entry_prefix_keeps_newest() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    let parent = target.join("debug").join("deps");
    // Use file entries (.rlib) so mtime can be pinned deterministically
    // on Windows, where set_modified on a directory requires
    // privileged access.
    let oldest = parent.join("libfoo-aaaaaaaaaaaaa.rlib");
    let middle = parent.join("libfoo-bbbbbbbbbbbbb.rlib");
    let newest = parent.join("libfoo-ccccccccccccc.rlib");
    for f in [&oldest, &middle, &newest] {
        touch(f);
    }
    set_mtime(&oldest, 1_700_000_000);
    set_mtime(&middle, 1_700_000_100);
    set_mtime(&newest, 1_700_000_200);

    let report = prune_target(&PruneTargetOptions {
        target_dir: target,
        dry_run: false,
        keep_latest: false,
    })
    .unwrap();

    assert_eq!(report.scanned, 3);
    assert_eq!(report.kept, 1);
    assert_eq!(report.deleted, 2);
    assert!(newest.exists(), "newest file must survive");
    assert!(!oldest.exists(), "oldest must be deleted");
    assert!(!middle.exists(), "middle must be deleted");
}

#[test]
fn non_matching_names_untouched() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    // No hash suffix: should never be scanned.
    let script = target.join("debug").join("build-script.txt");
    touch(&script);
    // Looks like a name with hash but lives at top level (not under
    // a known subdir): should be ignored.
    let stray = target.join("debug").join("loose-aaaaaaaaaaaaa");
    touch_dir(&stray);

    let report = prune_target(&PruneTargetOptions {
        target_dir: target,
        dry_run: false,
        keep_latest: false,
    })
    .unwrap();
    assert_eq!(report.scanned, 0, "no entries should match the scan dirs");
    assert!(script.exists());
    assert!(stray.exists());
}

#[test]
fn buckets_are_per_subdirectory() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    let fingerprint = target
        .join("debug")
        .join(".fingerprint")
        .join("foo-aaaaaaaaaaaaa");
    let deps = target
        .join("debug")
        .join("deps")
        .join("foo-bbbbbbbbbbbbb.rlib");
    let incremental = target
        .join("debug")
        .join("incremental")
        .join("foo-ccccccccccccc");
    touch_dir(&fingerprint);
    touch(&deps);
    touch_dir(&incremental);

    let report = prune_target(&PruneTargetOptions {
        target_dir: target,
        dry_run: false,
        keep_latest: false,
    })
    .unwrap();
    assert_eq!(report.scanned, 3);
    assert_eq!(report.kept, 3);
    assert_eq!(report.deleted, 0);
    assert!(fingerprint.exists());
    assert!(deps.exists());
    assert!(incremental.exists());
}

#[test]
fn dry_run_does_not_delete() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    let parent = target.join("debug").join("deps");
    let older = parent.join("libfoo-aaaaaaaaaaaaa.rlib");
    let newer = parent.join("libfoo-bbbbbbbbbbbbb.rlib");
    touch(&older);
    touch(&newer);
    set_mtime(&older, 1_700_000_000);
    set_mtime(&newer, 1_700_000_500);

    let report = prune_target(&PruneTargetOptions {
        target_dir: target,
        dry_run: true,
        keep_latest: false,
    })
    .unwrap();
    assert_eq!(report.scanned, 2);
    assert_eq!(report.kept, 1);
    assert_eq!(report.deleted, 1);
    // Files must remain on disk because we asked for dry-run.
    assert!(older.exists(), "dry-run must not delete older entry");
    assert!(newer.exists(), "dry-run must not delete newer entry");
}

#[test]
fn refuses_when_cargo_lock_is_held() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    let parent = target.join("debug").join(".fingerprint");
    let entry = parent.join("foo-aaaaaaaaaaaaa");
    touch_dir(&entry);
    let _cargo_lock = hold_cargo_lock(&target.join(".cargo-lock"));

    let err = prune_target(&PruneTargetOptions {
        target_dir: target.clone(),
        dry_run: false,
        keep_latest: false,
    })
    .expect_err("must refuse when top-level lock exists");
    assert!(format!("{err}").contains(".cargo-lock"));
    assert!(entry.exists(), "no entries may be touched when refusing");
    assert!(target.join(".cargo-lock").exists(), "lock must survive");
}

#[test]
fn refuses_when_profile_cargo_lock_is_held() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    let parent = target.join("debug").join(".fingerprint");
    let entry = parent.join("foo-aaaaaaaaaaaaa");
    touch_dir(&entry);
    let _cargo_lock = hold_cargo_lock(&target.join("debug").join(".cargo-lock"));

    let err = prune_target(&PruneTargetOptions {
        target_dir: target,
        dry_run: false,
        keep_latest: false,
    })
    .expect_err("must refuse when profile lock exists");
    assert!(format!("{err}").contains(".cargo-lock"));
    assert!(entry.exists(), "entry must survive a refusal");
}

#[test]
fn second_run_is_a_no_op() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    let parent = target.join("debug").join("deps");
    let older = parent.join("libfoo-aaaaaaaaaaaaa.rlib");
    let newer = parent.join("libfoo-bbbbbbbbbbbbb.rlib");
    touch(&older);
    touch(&newer);
    set_mtime(&older, 1_700_000_000);
    set_mtime(&newer, 1_700_000_500);

    let _ = prune_target(&PruneTargetOptions {
        target_dir: target.clone(),
        dry_run: false,
        keep_latest: false,
    })
    .unwrap();
    let second = prune_target(&PruneTargetOptions {
        target_dir: target,
        dry_run: false,
        keep_latest: false,
    })
    .unwrap();
    assert_eq!(second.scanned, 1);
    assert_eq!(second.kept, 1);
    assert_eq!(second.deleted, 0);
}

// -----------------------------------------------------------------
// Aggressive --keep-latest mode (issue #316). The bucket key drops
// from `(parent, prefix)` to `prefix`; the winning hash family is
// preserved across all subdirs. Recency rank prefers cargo's own
// `.fingerprint/<prefix>-<hash>/invoked.timestamp` mtime over the
// entry's own mtime — see entry_recency.
// -----------------------------------------------------------------

#[test]
fn keep_latest_drops_old_hash_family_across_subdirs() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    // Hash A is the older family; hash B is newer. Each lives in
    // deps/, .fingerprint/, build/.
    let deps_a = target.join("debug/deps/libfoo-aaaaaaaaaaaaa.rlib");
    let fp_a = target.join("debug/.fingerprint/foo-aaaaaaaaaaaaa/invoked.timestamp");
    let build_a = target.join("debug/build/foo-aaaaaaaaaaaaa");
    let deps_b = target.join("debug/deps/libfoo-bbbbbbbbbbbbb.rlib");
    let fp_b = target.join("debug/.fingerprint/foo-bbbbbbbbbbbbb/invoked.timestamp");
    let build_b = target.join("debug/build/foo-bbbbbbbbbbbbb");
    touch(&deps_a);
    touch(&fp_a);
    touch_dir(&build_a);
    touch(&deps_b);
    touch(&fp_b);
    touch_dir(&build_b);

    // Pin the fingerprint timestamps so the ranker prefers B over A
    // (B is newer).
    set_mtime(&fp_a, 1_700_000_000);
    set_mtime(&fp_b, 1_700_000_500);
    // Set entry mtimes to the OPPOSITE order so we can prove the
    // ranker honored the fingerprint timestamps, not the entry
    // mtimes.
    set_mtime(&deps_a, 1_700_001_000);
    set_mtime(&deps_b, 1_700_000_001);

    let report = prune_target(&PruneTargetOptions {
        target_dir: target,
        dry_run: false,
        keep_latest: true,
    })
    .unwrap();

    // All six entries scanned. Family B (deps_b + fp_b + build_b)
    // survives; family A is gone.
    assert_eq!(report.scanned, 6);
    assert_eq!(report.kept, 3);
    assert_eq!(report.deleted, 3);
    assert!(deps_b.exists());
    assert!(fp_b.exists());
    assert!(build_b.exists());
    assert!(!deps_a.exists());
    assert!(!fp_a.exists());
    assert!(!build_a.exists());
    // Authoritative-source counter ticked up.
    assert!(
        report.keep_decisions_from_fingerprint > 0,
        "fingerprint mtime should drive the rank"
    );
}

#[test]
fn keep_latest_falls_back_to_entry_mtime_when_fingerprint_missing() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    let deps_a = target.join("debug/deps/libfoo-aaaaaaaaaaaaa.rlib");
    let deps_b = target.join("debug/deps/libfoo-bbbbbbbbbbbbb.rlib");
    touch(&deps_a);
    touch(&deps_b);
    // No .fingerprint/ dir at all → forced fallback to entry mtime.
    set_mtime(&deps_a, 1_700_000_000);
    set_mtime(&deps_b, 1_700_000_500);

    let report = prune_target(&PruneTargetOptions {
        target_dir: target,
        dry_run: false,
        keep_latest: true,
    })
    .unwrap();

    assert!(deps_b.exists(), "newer entry wins");
    assert!(!deps_a.exists());
    assert!(
        report.keep_decisions_from_mtime > 0,
        "fallback path should be reflected in the counter"
    );
    assert_eq!(report.keep_decisions_from_fingerprint, 0);
}

#[test]
fn keep_latest_keeps_one_family_when_only_one_hash_exists() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    let deps = target.join("debug/deps/libfoo-aaaaaaaaaaaaa.rlib");
    let fp = target.join("debug/.fingerprint/foo-aaaaaaaaaaaaa/invoked.timestamp");
    touch(&deps);
    touch(&fp);

    let report = prune_target(&PruneTargetOptions {
        target_dir: target,
        dry_run: false,
        keep_latest: true,
    })
    .unwrap();
    assert_eq!(report.scanned, 2);
    assert_eq!(report.kept, 2);
    assert_eq!(report.deleted, 0);
    assert!(deps.exists());
    assert!(fp.exists());
}

#[test]
fn keep_latest_respects_cargo_lock_guard() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    let deps_a = target.join("debug/deps/libfoo-aaaaaaaaaaaaa.rlib");
    let deps_b = target.join("debug/deps/libfoo-bbbbbbbbbbbbb.rlib");
    touch(&deps_a);
    touch(&deps_b);
    let _cargo_lock = hold_cargo_lock(&target.join(".cargo-lock"));

    let err = prune_target(&PruneTargetOptions {
        target_dir: target.clone(),
        dry_run: false,
        keep_latest: true,
    })
    .expect_err("--keep-latest must still refuse under an active lock");
    assert!(format!("{err}").contains(".cargo-lock"));
    // Neither hash family was touched.
    assert!(deps_a.exists());
    assert!(deps_b.exists());
}

#[test]
fn entry_recency_prefers_fingerprint_timestamp() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    let deps_path = target.join("debug/deps/libfoo-aaaaaaaaaaaaa.rlib");
    let fp_path = target.join("debug/.fingerprint/foo-aaaaaaaaaaaaa/invoked.timestamp");
    touch(&deps_path);
    touch(&fp_path);
    // Entry mtime is OLDER than fingerprint mtime — we expect the
    // ranker to pick the (newer) fingerprint.
    set_mtime(&deps_path, 1_700_000_000);
    set_mtime(&fp_path, 1_700_005_000);

    let entry = PruneTargetEntry {
        path: deps_path,
        prefix: "libfoo".to_string(),
        hash: "aaaaaaaaaaaaa".to_string(),
        size_bytes: 0,
        mtime_unix: 1_700_000_000,
        action: PruneAction::Keep,
    };
    let (ts, source) = entry_recency(&entry, &target);
    assert_eq!(ts, 1_700_005_000);
    assert_eq!(source, RecencySource::FingerprintInvokedTimestamp);
}

#[test]
fn entry_recency_falls_back_to_entry_mtime() {
    let temp = tempdir().unwrap();
    let target = temp.path().to_path_buf();
    let entry = PruneTargetEntry {
        path: target.join("debug/deps/libfoo-aaaaaaaaaaaaa.rlib"),
        prefix: "libfoo".to_string(),
        hash: "aaaaaaaaaaaaa".to_string(),
        size_bytes: 0,
        // Note: the entry's own mtime field is the rank when no
        // fingerprint file exists on disk.
        mtime_unix: 1_700_000_000,
        action: PruneAction::Keep,
    };
    // No .fingerprint/ dir created.
    let (ts, source) = entry_recency(&entry, &target);
    assert_eq!(ts, 1_700_000_000);
    assert_eq!(source, RecencySource::EntryMtime);
}
