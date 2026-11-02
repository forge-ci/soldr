//! Bytes a directory tree would free if deleted (soldr#3439).
//!
//! The walk used to sum every file's apparent length, so an output hardlinked
//! from soldr's compiler cache counted in full even though deleting the target
//! copy frees nothing while the cache still names the same inode. A file with
//! several names is counted once, and only when every one of its names is
//! inside the walk; otherwise something outside still holds its blocks.
//!
//! Reflink (copy-on-write) extents cannot be detected portably, and Windows
//! does not expose a link count without opening each file, so on those
//! volumes the figure remains the apparent length -- an upper bound.

use std::collections::HashMap;
use std::path::Path;

use crate::platform::fs::identity::{hardlink_identity, HardlinkIdentity};

/// One inode's tally within a single walk.
struct SharedInode {
    len: u64,
    links: u64,
    seen: u64,
}

#[derive(Default)]
struct Footprint {
    exclusive_bytes: u64,
    files: u64,
    shared: HashMap<(u64, u64), SharedInode>,
}

impl Footprint {
    fn add_file(&mut self, metadata: &std::fs::Metadata) {
        self.files = self.files.saturating_add(1);
        match hardlink_identity(metadata) {
            Some(HardlinkIdentity { dev, index, links }) if links > 1 => {
                let entry = self.shared.entry((dev, index)).or_insert(SharedInode {
                    len: metadata.len(),
                    links,
                    seen: 0,
                });
                entry.seen = entry.seen.saturating_add(1);
            }
            _ => {
                self.exclusive_bytes = self.exclusive_bytes.saturating_add(metadata.len());
            }
        }
    }

    fn finish(self) -> (u64, u64) {
        let owned_shared = self
            .shared
            .values()
            .filter(|inode| inode.seen >= inode.links)
            .fold(0u64, |total, inode| total.saturating_add(inode.len));
        (
            self.exclusive_bytes.saturating_add(owned_shared),
            self.files,
        )
    }
}

/// `(bytes freed by deleting the tree, file count)`. Never follows symlinks;
/// unreadable entries are skipped so a partial figure is still usable by GC.
pub fn measure(path: &Path) -> (u64, u64) {
    let mut footprint = Footprint::default();
    walk(path, &mut footprint);
    footprint.finish()
}

fn walk(path: &Path, footprint: &mut Footprint) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() {
        return;
    }
    if metadata.is_file() {
        footprint.add_file(&metadata);
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        // `DirEntry::file_type` does not follow a link; `metadata` does. Using
        // the latter made the symlink guard dead and let cycles recurse (#1662).
        let Ok(entry_type) = entry.file_type() else {
            continue;
        };
        if entry_type.is_symlink() {
            continue;
        }
        if entry_type.is_dir() {
            walk(&entry.path(), footprint);
        } else if entry_type.is_file() {
            if let Ok(metadata) = entry.metadata() {
                footprint.add_file(&metadata);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows exposes no link count without a handle per file, so the
    /// dedupe tests only apply where it is reported.
    fn reports_link_counts() -> bool {
        crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows
    }

    #[test]
    fn plain_files_sum_their_lengths_and_count() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("b"), vec![0u8; 50]).unwrap();
        assert_eq!(measure(dir.path()), (150, 2));
    }

    #[test]
    fn hardlinks_wholly_inside_the_tree_count_once() {
        if !reports_link_counts() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a");
        std::fs::write(&first, vec![0u8; 1000]).unwrap();
        std::fs::hard_link(&first, dir.path().join("b")).unwrap();
        assert_eq!(measure(dir.path()), (1000, 2));
    }

    #[test]
    fn a_hardlink_shared_with_a_name_outside_the_tree_frees_nothing() {
        if !reports_link_counts() {
            return;
        }
        let outside = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let cached = outside.path().join("cached");
        std::fs::write(&cached, vec![0u8; 1000]).unwrap();
        std::fs::hard_link(&cached, tree.path().join("out")).unwrap();
        std::fs::write(tree.path().join("own"), vec![0u8; 7]).unwrap();
        assert_eq!(measure(tree.path()), (7, 2));
    }
}
