use super::*;
use std::time::{Duration, SystemTime};

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn same(a: &Path, b: &Path) -> bool {
    crate::platform::fs::identity::same_file(a, b)
}

/// The #4027 shape: the same toolchain in a caller home, a managed home,
/// and twice (bare + triple name) inside the managed home.
#[test]
fn duplicate_toolchains_across_homes_share_one_copy() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let caller = root.path().join("caller");
    let managed = root.path().join("managed");
    let rustc = b"rustc-binary-bytes".repeat(64);
    let dirs = [
        caller.join("toolchains/1.95.0-x86_64-unknown-linux-gnu"),
        managed.join("toolchains/nightly-2026-08-15"),
        managed.join("toolchains/nightly-2026-08-15-x86_64-unknown-linux-gnu"),
    ];
    for dir in &dirs {
        write(&dir.join("bin/rustc"), &rustc);
        write(&dir.join("lib/libstd.rlib"), b"std");
    }

    let first = link_rustup_home(&store, &caller);
    assert_eq!(first.files, 2);
    assert_eq!(first.new_objects, 2);
    assert_eq!(first.deduplicated_files, 0);

    let second = link_rustup_home(&store, &managed);
    assert_eq!(second.files, 4);
    assert_eq!(second.deduplicated_files, 4);
    assert_eq!(second.deduplicated_bytes, 2 * (rustc.len() as u64 + 3));
    assert_eq!(second.skipped_files, 0);

    for dir in &dirs {
        assert_eq!(std::fs::read(dir.join("bin/rustc")).unwrap(), rustc);
    }
    assert!(same(&dirs[0].join("bin/rustc"), &dirs[1].join("bin/rustc")));
    assert!(same(&dirs[1].join("bin/rustc"), &dirs[2].join("bin/rustc")));

    // Idempotent: a second pass changes nothing.
    let again = link_rustup_home(&store, &managed);
    assert_eq!(again.deduplicated_files, 0);
    assert_eq!(again.new_objects, 0);
}

#[test]
fn executable_bit_keeps_objects_apart() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let dir = root.path().join("toolchains/t");
    write(&dir.join("a"), b"same");
    write(&dir.join("b"), b"same");
    crate::platform::fs::permissions::make_executable(&dir.join("b")).unwrap();
    link_toolchain_dir(&store, &dir);
    if crate::platform::fs::permissions::mode(&dir.join("b")).is_some() {
        assert!(!same(&dir.join("a"), &dir.join("b")));
    }
}

#[test]
fn gc_drops_objects_no_toolchain_links() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let home = root.path().join("home");
    write(&home.join("toolchains/old/bin/rustc"), b"old");
    write(&home.join("toolchains/new/bin/rustc"), b"new");
    link_rustup_home(&store, &home);
    assert_eq!(gc_unreferenced_objects(&store), (0, 0));

    std::fs::remove_dir_all(home.join("toolchains/old")).unwrap();
    assert_eq!(gc_unreferenced_objects(&store), (1, 3));
    assert_eq!(
        std::fs::read(home.join("toolchains/new/bin/rustc")).unwrap(),
        b"new"
    );
}

#[test]
fn use_stamps_share_bare_and_triple_names() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let host = crate::pyo3_detect::host_triple();
    record_use(&store, "nightly-2026-08-15");
    let missing = root.path().join("missing");
    assert!(last_used(&store, &format!("nightly-2026-08-15-{host}"), &missing).is_some());
    assert!(last_used(&store, "1.95.0", &missing).is_none());
}

#[test]
fn unused_window() {
    let now = SystemTime::now();
    let day = Duration::from_secs(24 * 60 * 60);
    assert!(unused_for(Some(now - day * 31), 30, now));
    assert!(!unused_for(Some(now - day * 2), 30, now));
    assert!(unused_for(None, 30, now));
}
