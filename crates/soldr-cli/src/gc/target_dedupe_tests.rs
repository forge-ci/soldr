use super::*;
use std::time::{Duration, SystemTime};

#[test]
fn classify_accepts_only_immutable_artifacts() {
    let c = |p: &str| classify_target_path(Path::new(p));
    assert_eq!(c("debug/deps/libfoo-abc.rlib"), Some(ArtifactClass::Deps));
    assert_eq!(c("debug/deps/libfoo-abc.rmeta"), Some(ArtifactClass::Deps));
    assert_eq!(
        c("x86_64-unknown-linux-gnu/release/deps/libfoo-abc.rlib"),
        Some(ArtifactClass::Deps)
    );
    assert_eq!(
        c("debug/build/foo-abc/out/gen.rs"),
        Some(ArtifactClass::BuildOut)
    );
    assert_eq!(
        c("aarch64-apple-darwin/debug/build/foo-abc/out/sub/lib.a"),
        Some(ArtifactClass::BuildOut)
    );
    // In-place-written or workspace-specific files are never touched.
    assert_eq!(c("debug/deps/foo-abc.d"), None);
    assert_eq!(c("debug/deps/foo-abc"), None);
    assert_eq!(c("debug/build/foo-abc/build-script-build"), None);
    assert_eq!(c("debug/build/foo-abc/output"), None);
    assert_eq!(c("debug/incremental/foo/x.rlib"), None);
    assert_eq!(c("debug/.fingerprint/foo/x.rlib"), None);
    assert_eq!(c("debug/libfoo.rlib"), None);
    assert_eq!(c("debug/deps/.hidden.rlib"), None);
}

fn write_old(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
    let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(old)).unwrap();
}

fn opts(store: &Path) -> DedupeOptions {
    DedupeOptions {
        store: store.to_path_buf(),
        min_age_secs: 3600,
        min_size: 16,
        dry_run: false,
    }
}

fn ino(path: &Path) -> u64 {
    identity(&std::fs::metadata(path).unwrap()).map_or(0, |id| id.index)
}

/// Hosts without inode identity (Windows) report `supported: false`.
fn host_supported() -> bool {
    identity(&std::fs::metadata(std::env::temp_dir()).unwrap()).is_some()
}

#[test]
fn identical_deps_across_worktrees_share_one_inode() {
    if !host_supported() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let payload = vec![7u8; 64 * 1024];
    let a = tmp.path().join("wt-a/target");
    let b = tmp.path().join("wt-b/target");
    let rel = "debug/deps/libdep-1234.rlib";
    write_old(&a.join(rel), &payload);
    write_old(&b.join(rel), &payload);
    // Workspace-specific file with identical bytes must stay separate.
    write_old(&a.join("debug/deps/dep-1234.d"), &payload);
    write_old(&b.join("debug/deps/dep-1234.d"), &payload);

    let report = dedupe_targets(&[a.clone(), b.clone()], &opts(&tmp.path().join("store")));

    assert!(report.supported);
    assert_eq!(report.targets_scanned, 2);
    assert_eq!(report.files_added_to_store, 1, "{report:?}");
    assert_eq!(
        report.files_hardlinked + report.files_reflinked,
        1,
        "{report:?}"
    );
    assert_eq!(report.bytes_saved, payload.len() as u64);
    assert_eq!(std::fs::read(b.join(rel)).unwrap(), payload);
    if report.files_hardlinked == 1 {
        assert_eq!(ino(&a.join(rel)), ino(&b.join(rel)));
        assert!(std::fs::metadata(b.join(rel))
            .unwrap()
            .permissions()
            .readonly());
    }
    assert_ne!(
        ino(&a.join("debug/deps/dep-1234.d")),
        ino(&b.join("debug/deps/dep-1234.d"))
    );

    // Second pass is a no-op.
    let again = dedupe_targets(&[a.clone(), b.clone()], &opts(&tmp.path().join("store")));
    assert_eq!(again.bytes_saved, 0, "{again:?}");
    assert_eq!(again.files_added_to_store, 0, "{again:?}");

    // Sharing report: a hardlinked artifact is shared, not reclaimable.
    if report.files_hardlinked == 1 {
        let sharing = measure_target_sharing(&a);
        assert_eq!(sharing.shared_bytes, payload.len() as u64);
        assert_eq!(sharing.unique_bytes, payload.len() as u64);
        assert_eq!(sharing.total_bytes, 2 * payload.len() as u64);
    }
}

#[test]
fn build_script_out_is_never_hardlinked() {
    if !host_supported() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let payload = vec![3u8; 32 * 1024];
    let a = tmp.path().join("wt-a/target");
    let b = tmp.path().join("wt-b/target");
    let rel = "debug/build/sys-abcd/out/bindings.rs";
    write_old(&a.join(rel), &payload);
    write_old(&b.join(rel), &payload);

    let report = dedupe_targets(&[a.clone(), b.clone()], &opts(&tmp.path().join("store")));

    assert_eq!(report.files_hardlinked, 0, "{report:?}");
    assert_ne!(ino(&a.join(rel)), ino(&b.join(rel)));
    // An in-place rewrite of one worktree's OUT_DIR file never leaks.
    std::fs::write(a.join(rel), b"changed").unwrap();
    assert_eq!(std::fs::read(b.join(rel)).unwrap(), payload);
}

#[test]
fn young_small_and_active_targets_are_skipped() {
    if !host_supported() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("wt-a/target");
    let b = tmp.path().join("wt-b/target");
    let rel = "debug/deps/libdep-1.rlib";
    write_old(&a.join(rel), &[1u8; 4096]);
    std::fs::create_dir_all(b.join("debug/deps")).unwrap();
    std::fs::write(b.join(rel), [1u8; 4096]).unwrap(); // fresh mtime
    write_old(&a.join("debug/deps/libtiny-1.rlib"), b"tiny");
    write_old(&b.join("debug/deps/libtiny-1.rlib"), b"tiny");

    let report = dedupe_targets(&[a.clone(), b.clone()], &opts(&tmp.path().join("store")));
    assert_eq!(report.bytes_saved, 0, "{report:?}");
    assert_ne!(ino(&a.join(rel)), ino(&b.join(rel)));

    // A live cargo build (held .cargo-lock) skips the whole target.
    let c = tmp.path().join("wt-c/target");
    write_old(&c.join(rel), &[1u8; 4096]);
    let lock_path = c.join("debug/.cargo-lock");
    let lock = std::fs::File::create(&lock_path).unwrap();
    lock.lock().unwrap();
    let report = dedupe_targets(&[a, c.clone()], &opts(&tmp.path().join("store")));
    assert_eq!(report.targets_skipped_active, 1, "{report:?}");
    drop(lock);
}

#[test]
fn dry_run_reports_without_touching_files() {
    if !host_supported() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("wt-a/target");
    let b = tmp.path().join("wt-b/target");
    let rel = "debug/deps/libdep-9.rmeta";
    write_old(&a.join(rel), &[5u8; 8192]);
    write_old(&b.join(rel), &[5u8; 8192]);
    let store = tmp.path().join("store");
    let mut o = opts(&store);
    o.dry_run = true;

    let report = dedupe_targets(&[a.clone(), b.clone()], &o);

    assert_eq!(report.bytes_saved, 8192, "{report:?}");
    assert!(!store.exists());
    assert_ne!(ino(&a.join(rel)), ino(&b.join(rel)));
}

#[test]
fn orphaned_hardlink_blobs_are_pruned() {
    if !host_supported() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("wt-a/target");
    let rel = "debug/deps/libdep-2.rlib";
    write_old(&a.join(rel), &[9u8; 8192]);
    let store = tmp.path().join("store");
    let first = dedupe_targets(std::slice::from_ref(&a), &opts(&store));
    assert_eq!(first.files_added_to_store, 1);

    std::fs::remove_dir_all(&a).unwrap();
    let second = dedupe_targets(&[], &opts(&store));
    assert_eq!(second.blobs_pruned, 1, "{second:?}");
}
