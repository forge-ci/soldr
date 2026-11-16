//! Route retirement must release its private cache without touching a live
//! sibling or forgetting a store whose writer is still active.

use super::*;
use filetime::{set_file_mtime, FileTime};
use fs2::FileExt as _;
use running_process::broker::protocol_v2::ServiceDefinitionBuilder;
use std::fs::{self, OpenOptions};

fn register(services: &Path, service: &str, root: &Path, now: SystemTime) {
    let definition = ServiceDefinitionBuilder::shared_broker(service, "/bin/true")
        .label("package", "soldr")
        .label(SOLDR_ROOT_SERVICE_LABEL, root.display().to_string())
        .build();
    let path = services.join(format!("{service}.servicedef.v2"));
    fs::write(&path, definition.encode_to_vec()).unwrap();
    set_file_mtime(
        &path,
        FileTime::from_system_time(now - Duration::from_secs(7200)),
    )
    .unwrap();
}

#[test]
fn broker_retirement_retries_locked_cache_and_preserves_live_sibling() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(root.clone());
    let services = temp.path().join("services");
    let routes = temp.path().join("routes");
    fs::create_dir_all(&services).unwrap();
    fs::create_dir_all(&routes).unwrap();
    let now = SystemTime::now();
    let retiring = "soldr-daemon-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let sibling = "soldr-daemon-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    register(&services, retiring, &root, now);
    register(&services, sibling, &root, now);
    let store_for = |route: &str| {
        paths
            .cache
            .join("zccache/daemon-state")
            .join(format!("embedded-v1-{route}"))
            .join(zccache::core::config::versioned_subdir())
    };
    let retired_store = store_for(retiring);
    let live_store = store_for(sibling);
    for store in [&retired_store, &live_store] {
        fs::create_dir_all(store).unwrap();
        fs::write(store.join("artifact"), b"payload").unwrap();
    }
    let writer = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(retired_store.join(".writer.lock"))
        .unwrap();
    writer.try_lock_exclusive().unwrap();
    let live = BTreeSet::from([sibling.to_string()]);

    let first = crate::broker_daemon_disk::sweep_daemon_disk(
        &routes,
        &services,
        &live,
        now,
        crate::broker_daemon_disk::DaemonDiskPolicy::default(),
    );
    assert_eq!(first.cache_stores_live, 1);
    assert!(retired_store.exists());
    assert!(live_store.exists());
    assert!(services.join(format!("{retiring}.servicedef.v2")).exists());

    writer.unlock().unwrap();
    drop(writer);
    let second = crate::broker_daemon_disk::sweep_daemon_disk(
        &routes,
        &services,
        &live,
        now,
        crate::broker_daemon_disk::DaemonDiskPolicy::default(),
    );
    assert_eq!(second.cache_stores_removed, 1, "{second:?}");
    assert!(!retired_store.exists());
    assert!(live_store.join("artifact").exists());
    assert!(!services.join(format!("{retiring}.servicedef.v2")).exists());

    let third = crate::broker_daemon_disk::sweep_daemon_disk(
        &routes,
        &services,
        &live,
        now,
        crate::broker_daemon_disk::DaemonDiskPolicy::default(),
    );
    assert!(third.is_empty(), "{third:?}");
}

#[test]
fn rapid_image_cycles_reclaim_all_retired_stores_after_broker_restart() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(root.clone());
    let services = temp.path().join("services");
    let routes = temp.path().join("routes");
    fs::create_dir_all(&services).unwrap();
    fs::create_dir_all(&routes).unwrap();
    let now = SystemTime::now();
    let mut live = BTreeSet::new();
    let mut stores = Vec::new();
    for index in 0..10 {
        let service = format!("soldr-daemon-{index:032x}");
        register(&services, &service, &root, now);
        let store = paths
            .cache
            .join("zccache/daemon-state")
            .join(format!("embedded-v1-{service}"))
            .join(zccache::core::config::versioned_subdir());
        fs::create_dir_all(&store).unwrap();
        fs::write(store.join("artifact"), b"payload").unwrap();
        if index >= 8 {
            live.insert(service);
        }
        stores.push(store);
    }

    // Only persisted registrations and the current live set are supplied:
    // the old broker's in-memory retirement events are gone after restart.
    let report = crate::broker_daemon_disk::sweep_daemon_disk(
        &routes,
        &services,
        &live,
        now,
        crate::broker_daemon_disk::DaemonDiskPolicy::default(),
    );
    assert_eq!(report.cache_stores_removed, 8, "{report:?}");
    assert!(stores[..8].iter().all(|store| !store.exists()));
    assert!(stores[8..].iter().all(|store| store.exists()));

    let again = crate::broker_daemon_disk::sweep_daemon_disk(
        &routes,
        &services,
        &live,
        now,
        crate::broker_daemon_disk::DaemonDiskPolicy::default(),
    );
    assert!(again.is_empty(), "{again:?}");
}

#[test]
fn a_just_renewed_route_is_not_collected_from_a_stale_live_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("soldr-root");
    let services = temp.path().join("services");
    fs::create_dir_all(&services).unwrap();
    let now = SystemTime::now();
    let service = "soldr-daemon-cccccccccccccccccccccccccccccccc";
    register(&services, service, &root, now);
    let registration = services.join(format!("{service}.servicedef.v2"));
    set_file_mtime(&registration, FileTime::from_system_time(now)).unwrap();
    let store = SoldrPaths::with_root(root)
        .cache
        .join("zccache/daemon-state")
        .join(format!("embedded-v1-{service}"))
        .join(zccache::core::config::versioned_subdir());
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("artifact"), b"payload").unwrap();

    let report = sweep_retired_route_caches(&services, &BTreeSet::new(), now);
    assert_eq!(report.stores_removed, 0);
    assert!(store.join("artifact").exists());
}

/// soldr#4148 regression: a non-live route whose route directory the
/// retention policy keeps (inside `keep_idle_routes` and under `idle_ttl`)
/// keeps its cache too. Only a route the policy reclaims loses its store.
/// The local gate's tests lane stops the installed soldr's daemon and then
/// starts a source-built broker; that broker used to empty the installed
/// generation's store on its first pass, so every gate cold-compiled it.
#[test]
fn a_cache_follows_its_route_retention_not_route_liveness() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(root.clone());
    let services = temp.path().join("services");
    let routes = temp.path().join("routes");
    fs::create_dir_all(&services).unwrap();
    fs::create_dir_all(&routes).unwrap();
    let now = SystemTime::now();
    let kept = "soldr-daemon-dddddddddddddddddddddddddddddddd";
    let expired = "soldr-daemon-eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let policy = crate::broker_daemon_disk::DaemonDiskPolicy::default();
    let mut stores = Vec::new();
    for (service, idle) in [
        (kept, policy.protection + Duration::from_secs(60)),
        (expired, policy.idle_ttl + Duration::from_secs(60)),
    ] {
        register(&services, service, &root, now);
        let route = routes.join(service);
        fs::create_dir_all(&route).unwrap();
        set_file_mtime(&route, FileTime::from_system_time(now - idle)).unwrap();
        let store = paths
            .cache
            .join("zccache/daemon-state")
            .join(format!("embedded-v1-{service}"))
            .join(zccache::core::config::versioned_subdir());
        fs::create_dir_all(&store).unwrap();
        fs::write(store.join("artifact"), b"payload").unwrap();
        stores.push(store);
    }

    let report = crate::broker_daemon_disk::sweep_daemon_disk(
        &routes,
        &services,
        &BTreeSet::new(),
        now,
        policy,
    );

    assert!(
        stores[0].join("artifact").exists(),
        "a retained route's cache was swept: {report:?}"
    );
    assert!(routes.join(kept).exists());
    assert_eq!(report.cache_stores_removed, 1, "{report:?}");
    assert!(!stores[1].exists());
    assert!(!routes.join(expired).exists());
}
