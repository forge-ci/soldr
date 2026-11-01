//! Tests for [`super`] (soldr#4030).

use super::*;

fn workspace(root: &Path, lint_lock: &str) {
    std::fs::create_dir_all(root.join("dylints/one/src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\n[workspace.metadata.dylint]\nlibraries = [{ path = \"dylints/*\" }]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("dylints/one/Cargo.toml"),
        "[package]\nname = \"one\"\n",
    )
    .unwrap();
    std::fs::write(root.join("dylints/one/Cargo.lock"), lint_lock).unwrap();
    std::fs::write(root.join("dylints/one/src/lib.rs"), "// v1\n").unwrap();
}

#[test]
fn two_worktrees_with_the_same_lints_share_one_store_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("wt-a"), tmp.path().join("wt-b"));
    workspace(&a, "lock");
    workspace(&b, "lock");
    let store = tmp.path().join("store");
    let dir_a = store_dir(&store, &a, "nightly-2026-08-15").unwrap();
    assert_eq!(
        Some(dir_a.clone()),
        store_dir(&store, &b, "nightly-2026-08-15")
    );
    assert!(dir_a.starts_with(&store));

    // A lint source edit rebuilds in place; it is not a new store entry.
    std::fs::write(b.join("dylints/one/src/lib.rs"), "// v2\n").unwrap();
    assert_eq!(
        Some(dir_a.clone()),
        store_dir(&store, &b, "nightly-2026-08-15")
    );
    // A dependency or toolchain change is.
    assert_ne!(
        Some(dir_a.clone()),
        store_dir(&store, &b, "nightly-2026-09-01")
    );
    std::fs::write(b.join("dylints/one/Cargo.lock"), "other").unwrap();
    assert_ne!(Some(dir_a), store_dir(&store, &b, "nightly-2026-08-15"));
}

#[test]
fn no_declared_libraries_means_no_store() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    assert_eq!(
        None,
        store_dir(&tmp.path().join("s"), tmp.path(), "nightly-2026-08-15")
    );
}

#[test]
fn an_existing_per_worktree_build_moves_into_an_empty_store() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("target");
    let local = target.join("dylint/libraries/tc/release");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("libone.so"), "built").unwrap();
    let store = tmp.path().join("store/tc-key");

    if link_libraries_dir(&target, &store).is_err() {
        return; // host cannot create directory links (unprivileged Windows)
    }

    assert_eq!(
        std::fs::read_to_string(store.join("tc/release/libone.so")).unwrap(),
        "built"
    );
    let link = target.join("dylint/libraries");
    assert!(crate::platform::fs::links::is_link_or_reparse(
        &std::fs::symlink_metadata(&link).unwrap()
    ));
    assert!(link.join("tc/release/libone.so").is_file());
    // Idempotent.
    link_libraries_dir(&target, &store).unwrap();
}

#[test]
fn a_redundant_copy_is_dropped_when_the_store_is_populated() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store/tc-key");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("shared"), "x").unwrap();
    let target = tmp.path().join("target");
    std::fs::create_dir_all(target.join("dylint/libraries/dup")).unwrap();

    if link_libraries_dir(&target, &store).is_err() {
        return;
    }

    let link = target.join("dylint/libraries");
    assert!(link.join("shared").is_file());
    assert!(!store.join("dup").exists());
}
