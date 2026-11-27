use super::*;
use std::fs::{File, FileTimes};

fn candidate(root: &Path, key: &str, names: &[&str], bytes: &str) -> PathBuf {
    let store = root.join(format!("nightly-host-{key}"));
    let release = store.join("nightly-host/release");
    std::fs::create_dir_all(release.join(".fingerprint/dep")).unwrap();
    std::fs::write(release.join(".cargo-lock"), []).unwrap();
    std::fs::write(release.join(".fingerprint/dep/lib-dep"), bytes).unwrap();
    let paths: Vec<_> = names.iter().map(PathBuf::from).collect();
    record_libraries(&store, &paths).unwrap();
    std::fs::write(store.join(".last-used"), []).unwrap();
    std::fs::create_dir(store.join(".soldr-links")).unwrap();
    store
}

fn seeded_bytes(store: &Path) -> String {
    std::fs::read_to_string(store.join("nightly-host/release/.fingerprint/dep/lib-dep")).unwrap()
}

#[test]
fn seed_prefers_shared_libraries_then_recency_and_preserves_timestamps() {
    let tmp = tempfile::tempdir().unwrap();
    let best = candidate(tmp.path(), "best", &["one", "two"], "best");
    candidate(tmp.path(), "newer", &["one"], "newer");
    let new = tmp.path().join("nightly-host-new");
    seed_store(
        tmp.path(),
        &new,
        "nightly",
        "host",
        &["one".into(), "two".into()],
    )
    .unwrap();
    assert_eq!(seeded_bytes(&new), "best");
    let artifact = "nightly-host/release/.fingerprint/dep/lib-dep";
    assert_eq!(
        std::fs::metadata(best.join(artifact))
            .unwrap()
            .modified()
            .unwrap(),
        std::fs::metadata(new.join(artifact))
            .unwrap()
            .modified()
            .unwrap()
    );
    assert!(!new.join(".soldr-links").exists());
    assert!(!new.join(".last-used").exists());
    assert!(!new.join("nightly-host/release/.cargo-lock").exists());
    std::fs::write(new.join(artifact), "changed").unwrap();
    assert_eq!(
        seeded_bytes(&best),
        "best",
        "seeded files must have independent writable inodes"
    );
}

#[test]
fn seed_skips_busy_stores_and_other_toolchains_and_keeps_existing_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let busy = candidate(tmp.path(), "busy", &["one", "two"], "busy");
    let lock = File::options()
        .read(true)
        .write(true)
        .open(busy.join("nightly-host/release/.cargo-lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    candidate(tmp.path(), "ready", &["one"], "ready");
    let other = tmp.path().join("nightly-other-host-other");
    std::fs::create_dir(&other).unwrap();
    let new = tmp.path().join("nightly-host-new");
    seed_store(
        tmp.path(),
        &new,
        "nightly",
        "host",
        &["one".into(), "two".into()],
    )
    .unwrap();
    assert_eq!(seeded_bytes(&new), "ready");
    seed_store(tmp.path(), &new, "nightly", "host", &["two".into()]).unwrap();
    assert_eq!(seeded_bytes(&new), "ready");
    let absent = tmp.path().join("another-host-new");
    seed_store(tmp.path(), &absent, "another", "host", &[]).unwrap();
    assert!(!absent.exists());
}

#[test]
fn seed_uses_newest_store_when_overlap_ties_and_recognizes_legacy_libraries() {
    let tmp = tempfile::tempdir().unwrap();
    let old = candidate(tmp.path(), "old", &["one"], "old");
    File::options()
        .write(true)
        .open(old.join(".last-used"))
        .unwrap()
        .set_times(FileTimes::new().set_modified(std::time::UNIX_EPOCH))
        .unwrap();
    candidate(tmp.path(), "newest", &["one"], "newest");
    let new = tmp.path().join("nightly-host-new");
    seed_store(tmp.path(), &new, "nightly", "host", &["one".into()]).unwrap();
    assert_eq!(seeded_bytes(&new), "newest");
    let legacy = tmp.path().join("legacy/nightly-host/release");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("libone@nightly-host.so"), []).unwrap();
    assert!(candidate_names(&tmp.path().join("legacy"), "nightly-host").contains("one"));
}

#[test]
fn seeded_store_has_its_own_gc_lifetime() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("stores");
    std::fs::create_dir(&root).unwrap();
    let source = candidate(&root, "source", &["one"], "source");
    let new = root.join("nightly-host-new");
    seed_store(&root, &new, "nightly", "host", &["one".into()]).unwrap();
    let target = tmp.path().join("target");
    super::super::link_libraries_dir(&target, &new).unwrap();
    super::super::gc::register_link(&target, &new).unwrap();
    assert_eq!(
        super::super::gc::prune(&root, 0, false).unwrap(),
        vec![source]
    );
    assert_eq!(seeded_bytes(&new), "source");
    crate::platform::fs::links::remove(&target.join("dylint/libraries")).unwrap();
    assert_eq!(super::super::gc::prune(&root, 0, false).unwrap(), vec![new]);
}

#[test]
fn legacy_store_can_seed_a_new_target_aware_key() {
    let tmp = tempfile::tempdir().unwrap();
    let source = candidate(tmp.path(), "old", &["one"], "legacy");
    std::fs::rename(source, tmp.path().join("nightly-0123456789abcdef")).unwrap();
    let wrong = candidate(tmp.path(), "wrong", &["one"], "wrong triple");
    std::fs::rename(wrong.join("nightly-host"), wrong.join("nightly-other")).unwrap();
    std::fs::rename(wrong, tmp.path().join("nightly-fedcba9876543210")).unwrap();
    let new = tmp.path().join("nightly-host-new");
    seed_store(tmp.path(), &new, "nightly", "host", &["one".into()]).unwrap();
    assert_eq!(seeded_bytes(&new), "legacy");
}

#[test]
fn seeded_scripts_rerun_to_refresh_absolute_output_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let source = candidate(tmp.path(), "source", &["one"], "source");
    let fingerprints = source.join("nightly-host/release/.fingerprint/dep");
    std::fs::write(
        fingerprints.join("run-build-script-build-script-build"),
        "old-root",
    )
    .unwrap();
    std::fs::write(
        fingerprints.join("run-build-script-build-script-build.json"),
        "old-root",
    )
    .unwrap();
    std::fs::write(
        fingerprints.join("build-script-build-script-build"),
        "compiled",
    )
    .unwrap();
    let new = tmp.path().join("nightly-host-new");
    seed_store(tmp.path(), &new, "nightly", "host", &["one".into()]).unwrap();
    let fingerprints = new.join("nightly-host/release/.fingerprint/dep");
    assert!(!fingerprints
        .join("run-build-script-build-script-build")
        .exists());
    assert!(!fingerprints
        .join("run-build-script-build-script-build.json")
        .exists());
    assert!(fingerprints
        .join("build-script-build-script-build")
        .is_file());
}

#[test]
fn seed_preserves_internal_output_links_and_rejects_external_links() {
    let tmp = tempfile::tempdir().unwrap();
    let source = candidate(tmp.path(), "source", &["one"], "source");
    let artifact = source.join("nightly-host/release/.fingerprint/dep/lib-dep");
    if crate::platform::fs::links::create(&artifact.to_string_lossy(), &source.join("alias"), false)
        .is_err()
    {
        return; // Unprivileged hosts may not create links.
    }
    let new = tmp.path().join("nightly-host-new");
    seed_store(tmp.path(), &new, "nightly", "host", &["one".into()]).unwrap();
    assert_eq!(
        std::fs::read_to_string(new.join("alias")).unwrap(),
        "source"
    );
    assert!(new
        .join("alias")
        .canonicalize()
        .unwrap()
        .starts_with(new.canonicalize().unwrap()));
    let outside = tmp.path().join("outside");
    std::fs::write(&outside, "outside").unwrap();
    crate::platform::fs::links::create(&outside.to_string_lossy(), &source.join("external"), false)
        .unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir(&staging).unwrap();
    assert!(copy_tree(&source, &staging, &source, &tmp.path().join("final")).is_err());
}
