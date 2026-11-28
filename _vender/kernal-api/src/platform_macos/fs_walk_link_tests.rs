//! Symbolic links in a `DirectoryWalk`: how a follow-links cycle is
//! reported, and what an unfollowed link's metadata describes
//! (kernal-api#563). Native because creating a directory link is.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use crate::platform::fs::{DirectoryWalk, DirectoryWalkParallelism};

const MODES: [DirectoryWalkParallelism; 3] = [
    DirectoryWalkParallelism::Sequential,
    DirectoryWalkParallelism::SharedPool,
    DirectoryWalkParallelism::DedicatedPool { threads: 2 },
];

#[test]
fn a_followed_cycle_is_an_error_item_and_the_walk_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::write(root.join("a/b/file.txt"), "x").unwrap();
    std::fs::write(root.join("z.txt"), "z").unwrap();
    // An absolute link back to an ancestor, and a relative one.
    symlink(&root, root.join("a/b/to_root")).unwrap();
    symlink("..", root.join("a/b/to_parent")).unwrap();

    for mode in MODES {
        let items: Vec<_> = DirectoryWalk::new(root.clone())
            .follow_symbolic_links(true)
            .parallelism(mode)
            .walk()
            .collect();
        let errors: Vec<&std::io::Error> =
            items.iter().filter_map(|item| item.as_ref().err()).collect();
        let paths: BTreeSet<PathBuf> = items
            .iter()
            .filter_map(|item| item.as_ref().ok())
            .map(|entry| entry.path().to_path_buf())
            .collect();
        assert_eq!(errors.len(), 2, "{mode:?}: {errors:?}");
        for error in &errors {
            assert_eq!(error.kind(), ErrorKind::InvalidData, "{mode:?}");
            let message = error.to_string();
            assert!(message.contains("symbolic link cycle"), "{message}");
        }
        assert!(paths.contains(&root.join("a/b/file.txt")), "{mode:?}");
        assert!(paths.contains(&root.join("z.txt")), "{mode:?}");
        assert!(!paths.contains(&root.join("a/b/to_root")), "{mode:?}");
        assert!(!paths.contains(&root.join("a/b/to_parent")), "{mode:?}");
    }
}

#[test]
fn an_unfollowed_link_is_an_entry_whose_metadata_is_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir(root.join("target_dir")).unwrap();
    symlink(root.join("target_dir"), root.join("link")).unwrap();
    symlink("missing", root.join("dangling")).unwrap();

    let entries: Vec<_> = DirectoryWalk::new(root.clone())
        .walk()
        .collect::<std::io::Result<_>>()
        .unwrap();
    for name in ["link", "dangling"] {
        let entry = entries
            .iter()
            .find(|entry| entry.file_name() == name)
            .unwrap();
        assert!(entry.is_symbolic_link());
        assert!(!entry.is_directory());
        assert!(entry.metadata().unwrap().file_type().is_symlink());
    }

    let followed: Vec<_> = DirectoryWalk::new(root)
        .follow_symbolic_links(true)
        .walk()
        .filter_map(Result::ok)
        .collect();
    let link = followed
        .iter()
        .find(|entry| entry.file_name() == "link")
        .unwrap();
    assert!(link.is_directory());
    assert!(link.metadata().unwrap().is_dir());
}
