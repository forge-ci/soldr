//! Tests for [`super`] (soldr#4030).

use super::*;

fn package(root: &Path, name: &str, version: &str, dependencies: &str) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), format!(
        "[package]\nname = {name:?}\nversion = {version:?}\nedition = \"2021\"\n[dependencies]\n{dependencies}\n"
    )).unwrap();
    std::fs::write(root.join("src/lib.rs"), "// fixture\n").unwrap();
}

fn workspace(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"dylints/one\", \"lint-dep\", \"other\", \"leaf\"]\nresolver = \"2\"\n[workspace.metadata.dylint]\nlibraries = [{ path = \"dylints/*\" }]\n"
    ).unwrap();
    package(
        &root.join("dylints/one"),
        "one",
        "0.1.0",
        "lint-dep = { path = \"../../lint-dep\" }",
    );
    package(&root.join("lint-dep"), "lint-dep", "0.1.0", "");
    package(
        &root.join("other"),
        "other",
        "0.1.0",
        "leaf = { path = \"../leaf\" }",
    );
    package(&root.join("leaf"), "leaf", "0.1.0", "");
}

#[test]
fn two_worktrees_with_the_same_lints_share_one_store_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("wt a"), tmp.path().join("wt b"));
    workspace(&a);
    workspace(&b);
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
    // v2 replaces raw lockfile bytes with resolved dependency identities.
    package(&b.join("lint-dep"), "lint-dep", "0.2.0", "");
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

#[test]
fn unrelated_leaf_lockfile_change_keeps_key() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    workspace(&root);
    let store = tmp.path().join("store");
    let before = store_dir(&store, &root, "nightly").unwrap();
    let lock = std::fs::read(root.join("Cargo.lock")).unwrap();
    package(&root.join("leaf"), "leaf", "0.2.0", "");
    assert_eq!(Some(before), store_dir(&store, &root, "nightly"));
    assert_ne!(lock, std::fs::read(root.join("Cargo.lock")).unwrap());
}

#[test]
fn transitive_lint_dependency_change_changes_key() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    workspace(&root);
    let store = tmp.path().join("store");
    let before = store_dir(&store, &root, "nightly").unwrap();
    package(&root.join("lint-dep"), "lint-dep", "0.2.0", "");
    assert_ne!(Some(before), store_dir(&store, &root, "nightly"));
}

#[test]
fn target_aware_sharing_links_host_and_each_triple() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    workspace(&root);
    let paths = crate::core::SoldrPaths::with_root(tmp.path().join("soldr"));
    let target = root.join("target");
    let args = [
        "dylint",
        "--",
        "--target",
        "aarch64-unknown-linux-gnu",
        "--target=x86_64-unknown-linux-musl",
    ]
    .map(str::to_string);
    share_library_build(&paths, &root, Some(&target), "nightly", &args);
    let store = paths.root.join("dylint/libraries").canonicalize().unwrap();
    assert_eq!(
        target.join("dylint/libraries").canonicalize().unwrap(),
        store_dir(&store, &root, "nightly").unwrap()
    );
    for triple in ["aarch64-unknown-linux-gnu", "x86_64-unknown-linux-musl"] {
        let key = store_dir_for_target(&store, &root, "nightly", triple).unwrap();
        assert!(key.file_name().unwrap().to_string_lossy().contains(triple));
        assert_eq!(
            target
                .join(triple)
                .join("dylint/libraries")
                .canonicalize()
                .unwrap(),
            key
        );
    }
}

#[test]
fn config_targets_are_used_unless_cli_overrides_them() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".cargo")).unwrap();
    std::fs::write(
        tmp.path().join(".cargo/config.toml"),
        "[build]\ntarget = [\"aarch64-unknown-linux-gnu\", \"x86_64-unknown-linux-musl\"]\n",
    )
    .unwrap();
    assert_eq!(
        invocation_targets(tmp.path(), &[]).unwrap(),
        ["aarch64-unknown-linux-gnu", "x86_64-unknown-linux-musl"]
    );
    assert_eq!(
        invocation_targets(tmp.path(), &["--target=wasm32-unknown-unknown".into()]).unwrap(),
        ["wasm32-unknown-unknown"]
    );
}

#[test]
fn gc_keeps_newest_and_protects_live_links() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("store");
    for index in 0..6 {
        let key = root.join(format!("key-{index}"));
        let target = tmp.path().join(format!("target-{index}"));
        link_libraries_dir(&target, &key).unwrap();
        gc::register_link(&target, &key).unwrap();
        filetime::set_file_mtime(
            key.join(".last-used"),
            filetime::FileTime::from_unix_time(100 + index, 0),
        )
        .unwrap();
        if index != 0 {
            crate::platform::fs::links::remove(&target.join("dylint/libraries")).unwrap();
        }
    }
    let preview = gc::prune(&root, 2, true).unwrap();
    assert_eq!(preview.len(), 3);
    assert!(preview.iter().all(|p| p.exists()));
    assert_eq!(gc::prune(&root, 2, false).unwrap(), preview);
    for index in [0, 4, 5] {
        assert!(root.join(format!("key-{index}")).exists());
    }
    assert!(tmp.path().join("target-0/dylint/libraries").is_dir());
    assert_eq!(gc::run(&["gc".into(), "--help".into()]).unwrap(), 0);
}

#[test]
fn standalone_lint_workspace_is_resolved() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    workspace(&root);
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"other\", \"leaf\"]\nexclude = [\"dylints/one\", \"lint-dep\"]\n[workspace.metadata.dylint]\nlibraries = [{ path = \"dylints/*\" }]\n").unwrap();
    let manifest = root.join("dylints/one/Cargo.toml");
    let mut text = std::fs::read_to_string(&manifest).unwrap();
    text.push_str("\n[workspace]\n");
    std::fs::write(manifest, text).unwrap();
    assert!(store_dir(&tmp.path().join("store"), &root, "nightly").is_some());
}

#[test]
fn gc_reclaims_retargeted_and_deleted_targets_but_preserves_legacy_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("store");
    let target = tmp.path().join("target");
    for name in ["old", "new"] {
        let key = root.join(name);
        link_libraries_dir(&target, &key).unwrap();
        gc::register_link(&target, &key).unwrap();
    }
    std::fs::create_dir(root.join("legacy")).unwrap();
    assert_eq!(gc::prune(&root, 0, false).unwrap(), [root.join("old")]);
    assert!(root.join("new").is_dir());
    crate::platform::fs::links::remove(&target.join("dylint/libraries")).unwrap();
    assert_eq!(gc::prune(&root, 0, false).unwrap(), [root.join("new")]);
    assert!(root.join("legacy").is_dir());
}

/// A worker whose checkout roots a subtree, or who exports the repo into a
/// different directory layout, still has the same lint libraries at a
/// different depth. The store key must ignore that leading path: the build
/// tree depends on the dependency graph, not on where the checkout sits.
#[test]
fn a_subtree_checkout_at_a_different_depth_shares_the_same_store_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let flat = tmp.path().join("flat");
    workspace(&flat);

    // The same workspace rooted one level deeper: every member path gains a
    // "sub/" prefix and the declared library glob moves with it.
    let nested = tmp.path().join("subtree/repo");
    std::fs::create_dir_all(&nested).unwrap();
    workspace(&nested.join("sub"));
    std::fs::write(
        nested.join("Cargo.toml"),
        "[workspace]\nmembers = [\"sub/dylints/one\", \"sub/lint-dep\", \"sub/other\", \"sub/leaf\"]\nresolver = \"2\"\n[workspace.metadata.dylint]\nlibraries = [{ path = \"sub/dylints/*\" }]\n",
    )
    .unwrap();

    let store = tmp.path().join("store");
    assert_eq!(
        store_dir(&store, &flat, "nightly-2026-08-15"),
        store_dir(&store, &nested, "nightly-2026-08-15"),
        "the same lint set at a different checkout depth must reuse one build tree"
    );
}
