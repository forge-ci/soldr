//! Adopt a previous daemon generation's compiler store (soldr#3568 follow-up).
//!
//! Each daemon image owns a private store, `daemon-state/embedded-v1-<route>`,
//! and the route name follows the image hash. A new soldr image therefore
//! started with an empty store even when the previous generation's store sat
//! beside it, idle: every soldr upgrade discarded the cache, and CI's Tier-2
//! `zccache-unit` restore, which brings back the previous commit's generation,
//! served 0 hits (full CI run 39980588657: 0 HIT on every ci-test stage).
//!
//! A generation whose store does not exist yet takes over the largest idle
//! sibling store of the same zccache version. "Idle" means no live daemon
//! holds its writer lock, and the lock is held across the rename so no daemon
//! can claim the store mid-move. The store is content-addressed and every
//! entry is keyed by its compiler identity, so reusing it across soldr images
//! is as safe as one generation reusing its own store across restarts.

use super::*;

pub(crate) fn adopt_sibling_store(
    paths: &SoldrPaths,
    stable_root: &std::path::Path,
) -> Result<(), EmbeddedServiceError> {
    if stable_root.exists() {
        return Ok(());
    }
    let Some(parent) = stable_root.parent().filter(|parent| parent.exists()) else {
        return Ok(());
    };
    crate::cache_lib::path_safety::validate_owned_directory(&paths.root, parent)?;
    for sibling in sibling_stores_largest_first(parent, stable_root)? {
        let version_root = sibling.join(zccache::core::config::versioned_subdir());
        let Ok(lock) = zccache::core::cache_root_lock::CacheRootWriterLock::acquire(&version_root)
        else {
            continue; // A live daemon owns it; try the next largest.
        };
        // Windows refuses to rename a directory with an open handle inside;
        // retry after releasing. A daemon that claims the store in between
        // holds its own handle there, so the retry then fails the same way.
        let renamed = std::fs::rename(&sibling, stable_root).or_else(|_| {
            lock.release();
            std::fs::rename(&sibling, stable_root)
        });
        drop(lock);
        match renamed {
            Ok(()) => {
                tracing::info!(
                    from = %sibling.display(),
                    to = %stable_root.display(),
                    "adopted an idle sibling generation's embedded zccache store"
                );
                return Ok(());
            }
            Err(error) => tracing::debug!(
                %error,
                from = %sibling.display(),
                "could not adopt a sibling generation's embedded zccache store"
            ),
        }
    }
    Ok(())
}

/// Sibling generation stores for this zccache version, largest artifact index
/// first: the most to offer. Size, not age, because a store just created by
/// another generation is the newest and the emptiest. Every candidate is
/// returned, not just the largest: in CI the largest is often the bootstrap
/// soldr's own store, still held by its live daemon, while the previous
/// commit's idle store sits right behind it (soldr#3882).
fn sibling_stores_largest_first(
    parent: &std::path::Path,
    stable_root: &std::path::Path,
) -> std::io::Result<Vec<std::path::PathBuf>> {
    let version = zccache::core::config::versioned_subdir();
    let mut stores = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let path = entry?.path();
        let is_generation = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("embedded-v1-"));
        if !is_generation || path == stable_root {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
            continue;
        }
        let Ok(index) = std::fs::metadata(path.join(&version).join("index.bin")) else {
            continue;
        };
        stores.push((index.len(), path));
    }
    stores.sort_by_key(|(size, _)| std::cmp::Reverse(*size));
    Ok(stores.into_iter().map(|(_, path)| path).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(parent: &std::path::Path, name: &str, index_bytes: usize) -> std::path::PathBuf {
        let root = parent.join(name);
        let version = root.join(zccache::core::config::versioned_subdir());
        std::fs::create_dir_all(&version).unwrap();
        std::fs::write(version.join("index.bin"), vec![0_u8; index_bytes]).unwrap();
        root
    }

    fn fixture() -> (tempfile::TempDir, SoldrPaths, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
        let parent = paths.cache.join("zccache").join("daemon-state");
        std::fs::create_dir_all(&parent).unwrap();
        (temp, paths, parent)
    }

    #[test]
    fn a_new_generation_adopts_the_largest_idle_sibling_store() {
        let (_temp, paths, parent) = fixture();
        store(&parent, "embedded-v1-soldr-daemon-old", 900);
        store(&parent, "embedded-v1-soldr-daemon-bootstrap", 10);
        let current = parent.join("embedded-v1-soldr-daemon-new");
        adopt_sibling_store(&paths, &current).unwrap();
        let version = zccache::core::config::versioned_subdir();
        assert_eq!(
            std::fs::metadata(current.join(&version).join("index.bin"))
                .unwrap()
                .len(),
            900
        );
        assert!(!parent.join("embedded-v1-soldr-daemon-old").exists());
        assert!(parent.join("embedded-v1-soldr-daemon-bootstrap").exists());
    }

    #[test]
    fn a_store_held_by_a_live_daemon_is_never_adopted() {
        let (_temp, paths, parent) = fixture();
        let live = store(&parent, "embedded-v1-soldr-daemon-live", 900);
        let version_root = live.join(zccache::core::config::versioned_subdir());
        let _held =
            zccache::core::cache_root_lock::CacheRootWriterLock::acquire(&version_root).unwrap();
        let current = parent.join("embedded-v1-soldr-daemon-new");
        adopt_sibling_store(&paths, &current).unwrap();
        assert!(!current.exists());
        assert!(live.exists());
    }

    #[test]
    fn a_live_largest_store_does_not_block_adopting_the_next_idle_one() {
        let (_temp, paths, parent) = fixture();
        let live = store(&parent, "embedded-v1-soldr-daemon-bootstrap", 900);
        store(&parent, "embedded-v1-soldr-daemon-previous", 500);
        let version = zccache::core::config::versioned_subdir();
        let _held =
            zccache::core::cache_root_lock::CacheRootWriterLock::acquire(&live.join(&version))
                .unwrap();
        let current = parent.join("embedded-v1-soldr-daemon-new");
        adopt_sibling_store(&paths, &current).unwrap();
        assert_eq!(
            std::fs::metadata(current.join(&version).join("index.bin"))
                .unwrap()
                .len(),
            500
        );
        assert!(live.exists());
        assert!(!parent.join("embedded-v1-soldr-daemon-previous").exists());
    }

    #[test]
    fn an_existing_store_is_kept() {
        let (_temp, paths, parent) = fixture();
        store(&parent, "embedded-v1-soldr-daemon-old", 900);
        let current = store(&parent, "embedded-v1-soldr-daemon-new", 5);
        adopt_sibling_store(&paths, &current).unwrap();
        assert!(parent.join("embedded-v1-soldr-daemon-old").exists());
        let version = zccache::core::config::versioned_subdir();
        assert_eq!(
            std::fs::metadata(current.join(&version).join("index.bin"))
                .unwrap()
                .len(),
            5
        );
    }
}
