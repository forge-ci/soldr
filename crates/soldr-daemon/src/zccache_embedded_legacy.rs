//! Legacy embedded zccache cache-root migration and sweep (extracted
//! from zccache_embedded.rs to keep that file under the 1000-LOC
//! budget).

use super::*;

/// Re-home the pre-#1651 backend directory into the stable namespace.
///
/// The legacy identity hashed `(SoldrPaths::root, current_exe path)`. For a
/// normal in-place upgrade we can derive that exact path and prefer it even
/// when stale sibling identities exist. A cache restored to a different root
/// cannot recover the old root string, so it selects the uniquely most-recent
/// legacy backend instead. `soldr save` flushes the active backend immediately
/// before archiving and preserves nanosecond mtimes, making that ordering
/// durable across load. A tied newest mtime is rejected rather than silently
/// starting with an arbitrary cold cache.
pub(crate) fn migrate_legacy_cache_root(
    paths: &SoldrPaths,
    daemon_identity: &DaemonProcess,
    stable_root: &std::path::Path,
) -> Result<(), EmbeddedServiceError> {
    if stable_root.exists() {
        return Ok(());
    }

    let parent = stable_root
        .parent()
        .expect("private zccache cache root always has a parent");
    if !parent.exists() {
        return Ok(());
    }
    crate::cache_lib::path_safety::validate_owned_directory(&paths.root, parent)?;

    let exact_legacy = private_zccache_cache_root(
        paths,
        &derive_legacy_identity(paths, &daemon_identity.exe_path),
    );
    if std::fs::symlink_metadata(&exact_legacy).is_ok_and(|metadata| {
        metadata.is_dir() && !crate::cache_lib::path_safety::is_link_or_reparse(&metadata)
    }) {
        std::fs::rename(&exact_legacy, stable_root)?;
        tracing::info!(
            from = %exact_legacy.display(),
            to = %stable_root.display(),
            "migrated exact legacy embedded zccache backend"
        );
        return Ok(());
    }

    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir()
            && !crate::cache_lib::path_safety::is_link_or_reparse(&metadata)
            && is_legacy_identity_name(&entry.file_name())
        {
            candidates.push((latest_tree_mtime(&entry.path())?, entry.path()));
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let selected = select_legacy_candidate(parent, candidates)?;
    std::fs::rename(&selected, stable_root)?;
    tracing::warn!(
        from = %selected.display(),
        to = %stable_root.display(),
        "migrated most recently flushed legacy embedded zccache backend from a relocated cache"
    );
    Ok(())
}

pub(super) fn select_legacy_candidate(
    parent: &std::path::Path,
    mut candidates: Vec<(SystemTime, PathBuf)>,
) -> Result<PathBuf, EmbeddedServiceError> {
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    if candidates.len() > 1 && candidates[0].0 == candidates[1].0 {
        let newest = candidates[0].0;
        return Err(EmbeddedServiceError::AmbiguousLegacyCache {
            root: parent.to_path_buf(),
            candidates: candidates
                .into_iter()
                .take_while(|(mtime, _)| *mtime == newest)
                .map(|(_, path)| path)
                .collect(),
        });
    }
    Ok(candidates
        .into_iter()
        .next()
        .expect("caller rejects an empty legacy candidate list")
        .1)
}

pub(super) fn derive_legacy_identity(
    paths: &SoldrPaths,
    exe_path: &std::path::Path,
) -> HostIdentity {
    let mut hasher = StreamHasher::new();
    hasher.update(paths.root.as_os_str().to_string_lossy().as_bytes());
    hasher.update(exe_path.as_os_str().to_string_lossy().as_bytes());
    let id = hex::encode(&hasher.finalize().as_bytes()[..16]);
    HostIdentity {
        product: "soldr".to_string(),
        instance_id: id.clone(),
        workspace_id: id,
    }
}

fn is_legacy_identity_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn latest_tree_mtime(root: &std::path::Path) -> Result<SystemTime, std::io::Error> {
    let root_metadata = std::fs::symlink_metadata(root)?;
    if crate::cache_lib::path_safety::is_link_or_reparse(&root_metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("linked cache tree retained: {}", root.display()),
        ));
    }
    let mut newest = root_metadata.modified()?;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("linked cache entry retained: {}", entry.path().display()),
                ));
            }
            let modified = metadata.modified()?;
            newest = newest.max(modified);
            if metadata.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(newest)
}

/// Reclaim stale soldr-owned embedded generations beneath exactly one selected
/// product root.  The active stable identity and current version are always
/// protected; links and every sibling product root are ignored.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub fn sweep_legacy_cache_roots(
    paths: &SoldrPaths,
    now: SystemTime,
    max_age: std::time::Duration,
) -> LegacyCacheSweepReport {
    let zccache_root = paths.cache.join("zccache");
    let daemon_state = zccache_root.join("daemon-state");
    let current_embedded_root = embedded_cache_root(paths);
    let current_version = zccache::core::config::versioned_subdir();
    let mut report = LegacyCacheSweepReport::default();
    if !zccache_root.exists() {
        return report;
    }
    for root in [&zccache_root, &daemon_state] {
        if root.exists()
            && crate::cache_lib::path_safety::validate_owned_directory(&paths.root, root).is_err()
        {
            report.failed += 1;
            return report;
        }
    }
    let mut candidates = Vec::new();
    if daemon_state.exists() {
        match std::fs::read_dir(&daemon_state) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) if is_legacy_identity_name(&entry.file_name()) => {
                            candidates.push(entry.path());
                        }
                        Ok(_) => {}
                        Err(_) => report.failed += 1,
                    }
                }
            }
            Err(_) => report.failed += 1,
        }
    }
    // Retired `v<VERSION>` stores are swept by zccache itself (soldr#3365):
    // per-file expiry, eager purge of files no build tree links any more, and
    // `.writer.lock` liveness. Directory mtimes never gate these stores. The
    // top-level layout belongs to the removed standalone service, so every
    // version there is retired; the embedded root protects the current one.
    let mut retired = zccache::core::config::RetiredStoreSweepReport::default();
    if zccache_root.exists() {
        match std::fs::read_dir(&zccache_root) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => {
                            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                            if is_dir
                                && entry
                                    .file_name()
                                    .to_str()
                                    .is_some_and(zccache::core::config::is_version_dir_name)
                            {
                                retired.merge(
                                    &zccache::core::config::sweep_retired_version_store_with_mode(
                                        &entry.path(),
                                        max_age,
                                        now,
                                        zccache::core::config::RetiredSweepMode::Pressure,
                                    ),
                                );
                            }
                        }
                        Err(_) => report.failed += 1,
                    }
                }
            }
            Err(_) => report.failed += 1,
        }
    }
    // The singleton broker can keep several daemon image routes alive at
    // once. Each route now owns a private embedded store. An old route's
    // writer lock is the authority on whether it is live: zccache's sweep
    // leaves a live store intact and can reclaim it after the broker stops
    // that daemon and the lock is released.
    //
    // A sibling route's store is swept in `Routine` mode, never `Pressure`
    // (soldr#3882). Pressure purges every file with a single link whatever
    // its age, and a store restored from an archive (CI's Tier-2 cache) is
    // entirely single-link files. The first daemon under disk pressure --
    // in CI the bootstrap soldr -- emptied the previous run's source store
    // before its own lineage could adopt it, so every run served 0 hits.
    // Routine keeps a store whose `.last-active` marker is recent and
    // otherwise expires files by age.
    if let Ok(entries) = std::fs::read_dir(&daemon_state) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with("embedded-v1")
                || !entry.file_type().is_ok_and(|kind| kind.is_dir())
                || crate::cache_lib::path_safety::validate_owned_directory(&paths.root, &path)
                    .is_err()
            {
                continue;
            }
            if path == current_embedded_root {
                retired.merge(
                    &zccache::core::config::sweep_retired_version_stores_in_with_mode(
                        &path,
                        &current_version,
                        max_age,
                        now,
                        zccache::core::config::RetiredSweepMode::Pressure,
                    ),
                );
            } else if let Ok(stores) = std::fs::read_dir(&path) {
                let mut has_store = false;
                for store in stores.flatten() {
                    let store_path = store.path();
                    if store.file_type().is_ok_and(|kind| kind.is_dir())
                        && store
                            .file_name()
                            .to_str()
                            .is_some_and(zccache::core::config::is_version_dir_name)
                    {
                        retired.merge(
                            &zccache::core::config::sweep_retired_version_store_with_mode(
                                &store_path,
                                max_age,
                                now,
                                zccache::core::config::RetiredSweepMode::Routine,
                            ),
                        );
                        has_store |= store_path.exists();
                    }
                }
                // A generation whose store is gone leaves a shell (lineage
                // marker, logs) that otherwise accumulates one per
                // generation. The whole-root age gate below removes it once
                // idle, so a daemon still creating its root is never raced.
                if !has_store {
                    candidates.push(path);
                }
            }
        }
    }
    report.removed += retired.stores_removed;
    report.failed += retired.failed;
    report.live_retained += retired.stores_live;
    report.bytes_reclaimed = report
        .bytes_reclaimed
        .saturating_add(retired.bytes_reclaimed);

    // Pre-#1651 32-hex identity roots keep the whole-store age gate.
    for path in candidates {
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            report.failed += 1;
            continue;
        };
        if !metadata.is_dir() || crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
            report.failed += 1;
            continue;
        }
        let Ok(modified) = latest_tree_mtime(&path) else {
            report.failed += 1;
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() < max_age {
            continue;
        }
        let bytes = crate::cache_lib::target_registry::directory_size(&path);
        match remove_retired_store(&path) {
            RetiredStoreRemoval::Removed => {
                report.removed += 1;
                report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(bytes);
            }
            RetiredStoreRemoval::Live => report.live_retained += 1,
            RetiredStoreRemoval::Failed => report.failed += 1,
        }
    }
    report
}

/// Footprint of the retired version stores beside the current one (soldr#3329).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetiredStoresUsage {
    pub stores: u64,
    pub bytes: u64,
}

/// Measure every retired `v<VERSION>` store under `embedded_root`, skipping
/// `current_version_dir`. Read-only; follows no links, and a hardlinked file
/// is counted once.
pub fn measure_retired_stores(
    embedded_root: &std::path::Path,
    current_version_dir: &std::path::Path,
) -> RetiredStoresUsage {
    let mut usage = RetiredStoresUsage::default();
    let Ok(entries) = std::fs::read_dir(embedded_root) else {
        return usage;
    };
    let mut seen = std::collections::HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path == current_version_dir
            || !entry.file_type().is_ok_and(|kind| kind.is_dir())
            || !entry
                .file_name()
                .to_str()
                .is_some_and(zccache::core::config::is_version_dir_name)
            || writer_lock_is_held(&path)
        {
            continue;
        }
        usage.stores += 1;
        let mut pending = vec![path];
        while let Some(dir) = pending.pop() {
            let Ok(children) = std::fs::read_dir(&dir) else {
                continue;
            };
            for child in children.flatten() {
                let Ok(metadata) = std::fs::symlink_metadata(child.path()) else {
                    continue;
                };
                if crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
                    continue;
                }
                if metadata.is_dir() {
                    pending.push(child.path());
                } else if metadata.is_file() && first_link_seen(&mut seen, &child.path()) {
                    usage.bytes = usage.bytes.saturating_add(metadata.len());
                }
            }
        }
    }
    usage
}

/// An occupied or unreadable writer lock cannot be counted as retired.
fn writer_lock_is_held(store: &std::path::Path) -> bool {
    use fs2::FileExt as _;

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(store.join(ZCCACHE_WRITER_LOCK_FILE))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    file.try_lock_exclusive().is_err()
}

/// True the first time a file's identity is seen, so a hardlinked file is
/// counted once. Files without a stable identity are always counted.
fn first_link_seen(
    seen: &mut std::collections::HashSet<(u64, u64)>,
    path: &std::path::Path,
) -> bool {
    let Some(id) = crate::platform::fs::identity::file_identity(path) else {
        return true;
    };
    match (id.dev.or(id.volume_serial_number), id.ino.or(id.file_index)) {
        (Some(volume), Some(index)) => seen.insert((volume, index)),
        _ => true,
    }
}

enum RetiredStoreRemoval {
    Removed,
    Live,
    Failed,
}

/// Remove one retired store unless a running service still owns it (soldr#3251).
///
/// The age gate alone cannot prove a store is dead: a long-idle service of an
/// older zccache version can sit on a store whose files have not changed for
/// days. The writer lock is the proof. It is held for the whole removal, so a
/// service of that version cannot claim the store halfway through.
fn remove_retired_store(path: &std::path::Path) -> RetiredStoreRemoval {
    use fs2::FileExt;

    let lock = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.join(ZCCACHE_WRITER_LOCK_FILE))
    {
        Ok(file) => match file.try_lock_exclusive() {
            Ok(()) => Some(file),
            Err(error) if crate::cache_lib::cargo_lock::lock_is_held(&error) => {
                return RetiredStoreRemoval::Live
            }
            Err(_) => return RetiredStoreRemoval::Failed,
        },
        // A store that never ran a service, or a layout from before the lock
        // existed: nothing can own it.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return RetiredStoreRemoval::Failed,
    };
    if std::fs::remove_dir_all(path).is_ok() {
        return RetiredStoreRemoval::Removed;
    }
    // Windows will not delete a file this process holds open, so the lock file
    // itself blocks the removal above. Release the lock and retry once. The
    // remaining window is the one zccache's own version pruning accepts.
    drop(lock);
    match std::fs::remove_dir_all(path) {
        Ok(()) => RetiredStoreRemoval::Removed,
        Err(_) => RetiredStoreRemoval::Failed,
    }
}

#[cfg(test)]
mod legacy_gc_tests {
    use super::*;

    #[test]
    fn legacy_sweep_protects_current_and_sibling_roots() {
        let temp = tempfile::tempdir().unwrap();
        let owned = SoldrPaths::with_root(temp.path().join(".soldr"));
        let sibling = SoldrPaths::with_root(temp.path().join(".soldr-dev"));
        let legacy = owned
            .cache
            .join("zccache/daemon-state/0123456789abcdef0123456789abcdef");
        let embedded = embedded_cache_root(&owned);
        let current = embedded.join(zccache::core::config::versioned_subdir());
        let nested_old_version = embedded.join("v0.0.1");
        let retired_route = owned
            .cache
            .join("zccache/daemon-state/embedded-v1-soldr-daemon-retired")
            .join(zccache::core::config::versioned_subdir());
        let top_old_version = owned.cache.join("zccache/v0.0.2");
        // Top-level versions belong to the removed standalone/legacy layout,
        // even when their version text happens to equal the embedded build.
        let top_current_version = owned
            .cache
            .join("zccache")
            .join(zccache::core::config::versioned_subdir());
        let malformed = owned.cache.join("zccache/vprivate");
        let sibling_sentinel = sibling
            .cache
            .join("zccache/daemon-state/0123456789abcdef0123456789abcdef/sentinel");
        for path in [
            legacy.join("artifact"),
            current.join("artifact"),
            nested_old_version.join("artifact"),
            retired_route.join("artifact"),
            top_old_version.join("artifact"),
            top_current_version.join("artifact"),
            malformed.join("artifact"),
            sibling_sentinel.clone(),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"payload").unwrap();
        }

        let report = sweep_legacy_cache_roots(&owned, SystemTime::now(), std::time::Duration::ZERO);
        // The emptied retired route root is a shell and goes too.
        assert_eq!(report.removed, 6);
        assert!(!retired_route.parent().unwrap().exists());
        assert!(!legacy.exists());
        assert!(!nested_old_version.exists());
        assert!(!retired_route.exists());
        assert!(!top_old_version.exists());
        assert!(!top_current_version.exists());
        assert!(current.join("artifact").is_file());
        assert!(malformed.join("artifact").is_file());
        assert!(sibling_sentinel.is_file());
    }

    /// soldr#3329: the measurement counts retired stores and skips the current one.
    #[test]
    fn measure_retired_stores_skips_the_current_version_dir() {
        let temp = tempfile::tempdir().unwrap();
        let embedded = temp.path().join("embedded-v1");
        let current = embedded.join(zccache::core::config::versioned_subdir());
        let retired = embedded.join("v0.0.1");
        std::fs::create_dir_all(current.join("nested")).unwrap();
        std::fs::create_dir_all(retired.join("nested")).unwrap();
        std::fs::write(current.join("nested/artifact"), vec![0u8; 4096]).unwrap();
        std::fs::write(retired.join("nested/artifact"), vec![0u8; 1000]).unwrap();
        std::fs::write(retired.join("top"), vec![0u8; 24]).unwrap();
        std::fs::create_dir_all(embedded.join("vprivate")).unwrap();
        std::fs::write(embedded.join("vprivate/x"), b"ignored").unwrap();

        let usage = measure_retired_stores(&embedded, &current);

        assert_eq!(
            usage,
            RetiredStoresUsage {
                stores: 1,
                bytes: 1024
            }
        );
    }

    /// soldr#3251: a retired store whose writer lock is held belongs to a
    /// running service, however old its files look.
    #[test]
    fn legacy_sweep_refuses_a_store_whose_writer_lock_is_held() {
        use fs2::FileExt;

        let temp = tempfile::tempdir().unwrap();
        let owned = SoldrPaths::with_root(temp.path().join(".soldr"));
        let retired = owned
            .cache
            .join("zccache/daemon-state/embedded-v1")
            .join(zccache::core::config::versioned_subdir());
        std::fs::create_dir_all(&retired).unwrap();
        std::fs::write(retired.join("artifact"), b"payload").unwrap();
        let lock = std::fs::File::create(retired.join(ZCCACHE_WRITER_LOCK_FILE)).unwrap();
        lock.try_lock_exclusive().unwrap();

        let held = sweep_legacy_cache_roots(&owned, SystemTime::now(), std::time::Duration::ZERO);

        assert_eq!(held.live_retained, 1, "{held:?}");
        assert_eq!(held.removed, 0, "{held:?}");
        assert_eq!(held.failed, 0, "a live store is not a failure: {held:?}");
        assert!(retired.join("artifact").is_file());

        drop(lock);
        let released =
            sweep_legacy_cache_roots(&owned, SystemTime::now(), std::time::Duration::ZERO);

        // The store, then its emptied shell root.
        assert_eq!(released.removed, 2, "{released:?}");
        assert_eq!(released.live_retained, 0, "{released:?}");
        assert!(!retired.exists());
    }
}
