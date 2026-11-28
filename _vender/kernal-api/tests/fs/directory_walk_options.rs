#![cfg(feature = "fs")]

//! The walkdir-parity options of a directory walk: depth limits, entry
//! filters that see files, and per-entry name, path and metadata
//! (kernal-api#563).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use kernal_api::platform::fs::{DirectoryEntry, DirectoryWalk, DirectoryWalkParallelism};

/// `root/{a.c, b.h, src/{c.c, d.txt, deep/{e.c}}}`
fn tree(root: &Path) {
    fs::create_dir_all(root.join("src/deep")).unwrap();
    fs::write(root.join("a.c"), "a").unwrap();
    fs::write(root.join("b.h"), "bb").unwrap();
    fs::write(root.join("src/c.c"), "ccc").unwrap();
    fs::write(root.join("src/d.txt"), "dddd").unwrap();
    fs::write(root.join("src/deep/e.c"), "eeeee").unwrap();
}

fn relative(root: &Path, walk: DirectoryWalk) -> BTreeSet<String> {
    walk.walk()
        .map(|entry| {
            let path = entry.expect("walk entry").into_path();
            path.strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

fn names(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

const MODES: [DirectoryWalkParallelism; 3] = [
    DirectoryWalkParallelism::Sequential,
    DirectoryWalkParallelism::SharedPool,
    DirectoryWalkParallelism::DedicatedPool { threads: 2 },
];

#[test]
fn max_depth_bounds_what_is_yielded_in_every_mode() {
    let dir = tempfile::tempdir().unwrap();
    tree(dir.path());
    let root = dir.path().to_path_buf();
    for mode in MODES {
        let at = |depth| {
            relative(
                &root,
                DirectoryWalk::new(root.clone())
                    .parallelism(mode)
                    .max_depth(depth),
            )
        };
        assert_eq!(at(0), names(&[""]), "{mode:?}");
        assert_eq!(at(1), names(&["", "a.c", "b.h", "src"]), "{mode:?}");
        assert_eq!(
            at(2),
            names(&["", "a.c", "b.h", "src", "src/c.c", "src/d.txt", "src/deep"]),
            "{mode:?}"
        );
        assert_eq!(
            at(3),
            relative(&root, DirectoryWalk::new(root.clone()).parallelism(mode))
        );
    }
}

#[test]
fn an_entry_filter_sees_files_and_prunes_rejected_directories() {
    let dir = tempfile::tempdir().unwrap();
    tree(dir.path());
    let root = dir.path().to_path_buf();
    for mode in MODES {
        let c_sources = relative(
            &root,
            DirectoryWalk::new(root.clone())
                .parallelism(mode)
                .filter_entries(|entry| {
                    entry.is_directory() || entry.path().extension().is_some_and(|ext| ext == "c")
                }),
        );
        assert_eq!(
            c_sources,
            names(&["", "a.c", "src", "src/c.c", "src/deep", "src/deep/e.c"]),
            "{mode:?}"
        );

        let without_deep = relative(
            &root,
            DirectoryWalk::new(root.clone())
                .parallelism(mode)
                .filter_entries(|entry| entry.file_name() != "deep"),
        );
        assert!(!without_deep.contains("src/deep/e.c"), "{mode:?}");
        assert!(without_deep.contains("src/d.txt"), "{mode:?}");
    }
}

#[test]
fn the_root_is_never_filtered_and_both_predicates_must_keep_an_entry() {
    let dir = tempfile::tempdir().unwrap();
    tree(dir.path());
    let root = dir.path().to_path_buf();
    let kept = relative(
        &root,
        DirectoryWalk::new(root.clone())
            .filter_entries(|entry| !entry.is_file() || entry.depth() == 1)
            .prune_directories(|path| !path.ends_with("deep")),
    );
    assert_eq!(kept, names(&["", "a.c", "b.h", "src"]));
}

#[test]
fn entries_carry_name_path_depth_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    tree(dir.path());
    let root = dir.path().to_path_buf();
    let entries: Vec<DirectoryEntry> = DirectoryWalk::new(root.clone())
        .sorted(true)
        .walk()
        .collect::<std::io::Result<_>>()
        .unwrap();

    let deep = entries
        .iter()
        .find(|entry| entry.path() == root.join("src/deep/e.c"))
        .expect("deep file");
    assert_eq!(deep.file_name(), "e.c");
    assert_eq!(deep.depth(), 3);
    let metadata = deep.metadata().unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.len(), 5);

    let src = entries
        .iter()
        .find(|entry| entry.file_name() == "src")
        .unwrap();
    assert!(src.is_directory());
    assert!(src.metadata().unwrap().is_dir());

    let root_entry = entries.iter().find(|entry| entry.depth() == 0).unwrap();
    assert_eq!(root_entry.file_name(), root.file_name().unwrap());
    assert_eq!(root_entry.clone().into_path(), root);

    let all: BTreeSet<PathBuf> = entries.into_iter().map(DirectoryEntry::into_path).collect();
    assert_eq!(all.len(), 8);
}
