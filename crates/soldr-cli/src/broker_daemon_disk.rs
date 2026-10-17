//! Broker-owned daemon-disk reclamation (soldr#3251).
//!
//! The broker assigns every daemon generation its disk: a service
//! registration (`<services>/soldr-daemon-<hash>.servicedef.v2`) and a route
//! directory (`<routes>/soldr-daemon-<hash>/`) that holds the staged daemon
//! image. Every distinct daemon build creates both, because the service name
//! is keyed on the image hash. Until this module nothing removed either: one
//! workstation held 782 registrations and 527 route directories (27 GiB)
//! behind a single live route.
//!
//! The broker is the owner because it is the one long-lived process that both
//! creates that disk and knows which generations are live. The per-version
//! runtime sweep (`self_relocate::sweep_route_runtime_copies`) still reclaims
//! old image versions inside a route; this module reclaims whole routes and
//! their registrations.
//!
//! A route is reclaimed only when all of these hold:
//! - its service is not live (no registry entry and no route owners);
//! - it has had no activity for [`DaemonDiskPolicy::protection`];
//! - it is idle past [`DaemonDiskPolicy::idle_ttl`], or it is older than the
//!   newest [`DaemonDiskPolicy::keep_idle_routes`] idle routes.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use running_process::broker::protocol_v2::{service_definition_dir_v2, SERVICE_DEF_V2_EXTENSION};
use running_process::broker::server::BackendRegistry;

use crate::broker_reaper::RouteOwnership;
use crate::platform::sync::Mutex;

/// Every soldr daemon service name, and so every route directory and
/// registration this module may reclaim, starts with this prefix. Anything
/// else under the broker's roots belongs to someone else and is left alone.
pub(crate) const DAEMON_SERVICE_PREFIX: &str = "soldr-daemon-";

/// How often the broker walks its daemon disk. The walk touches a few
/// hundred directory entries at most, but nothing about reclaiming disk needs
/// the reaper's 30-second cadence.
pub(crate) const DAEMON_DISK_SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DaemonDiskPolicy {
    /// No route or registration with activity inside this window is
    /// reclaimed, whatever else holds.
    ///
    /// A running daemon renews its image ledger on every maintenance tick
    /// (five minutes), but a tick waits for the pass before it, and a full
    /// store scan can run for many minutes. The window has to outlast that, so
    /// a daemon this broker's registry does not list is still never touched.
    pub(crate) protection: Duration,
    /// An idle route older than this is reclaimed even when it is among the
    /// newest.
    pub(crate) idle_ttl: Duration,
    /// How many of the most recently active idle routes are kept warm inside
    /// `idle_ttl`, so returning to a recent build skips re-staging its image.
    pub(crate) keep_idle_routes: usize,
}

impl Default for DaemonDiskPolicy {
    fn default() -> Self {
        Self {
            protection: Duration::from_secs(30 * 60),
            idle_ttl: Duration::from_secs(6 * 60 * 60),
            keep_idle_routes: 4,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DaemonDiskReport {
    pub(crate) routes_removed: usize,
    pub(crate) bytes_reclaimed: u64,
    pub(crate) registrations_removed: usize,
    pub(crate) cache_stores_removed: usize,
    pub(crate) cache_bytes_reclaimed: u64,
    pub(crate) cache_stores_live: usize,
    /// Removals that failed, such as an image still open on Windows. Each is
    /// retried by a later sweep, and none stops the current one.
    pub(crate) failed: usize,
}

impl DaemonDiskReport {
    pub(crate) fn is_empty(&self) -> bool {
        self.routes_removed == 0
            && self.registrations_removed == 0
            && self.cache_stores_removed == 0
            && self.failed == 0
    }
}

/// Reclaim this broker's idle daemon disk.
///
/// The live set is snapshotted from the route owners and the backend
/// registry before any filesystem work. A route requested after the snapshot
/// has just renewed its registration and staged its image, so the protection
/// window keeps it.
pub(crate) fn sweep_broker_daemon_disk(
    route_owners: &Mutex<RouteOwnership>,
    registry: &std::sync::Mutex<BackendRegistry>,
) -> DaemonDiskReport {
    let mut live = BTreeSet::new();
    live.extend(route_owners.lock().service_names().map(str::to_owned));
    live.extend(
        crate::broker_reaper::lock_registry(registry)
            .iter()
            .map(|(key, _)| key.service_name.clone()),
    );
    sweep_daemon_disk(
        &crate::broker_launcher::routes_root(),
        &service_definition_dir_v2(),
        &live,
        SystemTime::now(),
        DaemonDiskPolicy::default(),
    )
}

pub(crate) fn sweep_daemon_disk(
    routes_root: &Path,
    services_root: &Path,
    live: &BTreeSet<String>,
    now: SystemTime,
    policy: DaemonDiskPolicy,
) -> DaemonDiskReport {
    sweep_daemon_disk_with(routes_root, services_root, live, now, policy, remove_entry)
}

/// [`sweep_daemon_disk`] with an injectable remover, so failure handling is
/// testable without platform-specific ways to make a deletion fail.
pub(crate) fn sweep_daemon_disk_with(
    routes_root: &Path,
    services_root: &Path,
    live: &BTreeSet<String>,
    now: SystemTime,
    policy: DaemonDiskPolicy,
    mut remove: impl FnMut(&Path) -> io::Result<()>,
) -> DaemonDiskReport {
    let mut report = DaemonDiskReport::default();

    // A route's service definition persists the selected Soldr root. Sweep
    // its private cache before removing that registration; a live writer lock
    // keeps the store intact and the next broker pass retries it.
    let cache = crate::broker_daemon_cache::sweep_retired_route_caches(services_root, live, now);
    report.cache_stores_removed = cache.stores_removed;
    report.cache_bytes_reclaimed = cache.bytes_reclaimed;
    report.cache_stores_live = cache.stores_live;
    report.failed += cache.failed;

    let mut idle_routes = Vec::new();
    for (service, path) in daemon_entries(routes_root, live, route_service) {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        // A real directory only: never follow a link out of the broker root.
        if !metadata.is_dir() || crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
            continue;
        }
        let idle_for = idle_duration(now, route_last_active(&path, &metadata));
        if idle_for >= policy.protection {
            idle_routes.push((idle_for, service, path));
        }
    }
    // Least idle first, so the routes kept warm are the most recently used.
    idle_routes.sort();
    for (index, (idle_for, _, path)) in idle_routes.into_iter().enumerate() {
        if index < policy.keep_idle_routes && idle_for < policy.idle_ttl {
            continue;
        }
        let bytes = crate::cache_lib::target_registry::directory_size(&path);
        match remove(&path) {
            Ok(()) => {
                report.routes_removed += 1;
                report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(bytes);
            }
            Err(_) => report.failed += 1,
        }
    }

    // Registrations run after routes, so a route reclaimed above takes its
    // registration with it in the same sweep.
    for (service, path) in daemon_entries(services_root, live, registration_service) {
        if routes_root.join(&service).exists() || cache.pending.contains(&service) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        // Renewed on every re-registration, so this is when a front door last
        // asked for the service. An unreadable time counts as just now.
        let registered = metadata.modified().unwrap_or(now);
        if idle_duration(now, registered) < policy.protection {
            continue;
        }
        match remove(&path) {
            Ok(()) => report.registrations_removed += 1,
            Err(_) => report.failed += 1,
        }
    }

    report
}

/// A route directory is named after its service.
fn route_service(name: &str) -> Option<&str> {
    Some(name)
}

/// A registration file is `<service>.servicedef.v2`.
fn registration_service(name: &str) -> Option<&str> {
    name.strip_suffix(SERVICE_DEF_V2_EXTENSION)?
        .strip_suffix('.')
}

/// Entries under `root` whose service name (derived by `service_of`) is a
/// soldr daemon that is not live. A missing root yields nothing.
fn daemon_entries<'a>(
    root: &Path,
    live: &'a BTreeSet<String>,
    service_of: fn(&str) -> Option<&str>,
) -> impl Iterator<Item = (String, std::path::PathBuf)> + 'a {
    fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(move |entry| {
            let name = entry.file_name();
            let service = service_of(name.to_str()?)?;
            (service.starts_with(DAEMON_SERVICE_PREFIX) && !live.contains(service))
                .then(|| (service.to_owned(), entry.path()))
        })
}

/// The newest sign of use for one route: its directory's modification time
/// (set when the broker creates or stages into it) or the newest `last-used`
/// ledger of its staged images (renewed by a running daemon).
fn route_last_active(route: &Path, metadata: &fs::Metadata) -> SystemTime {
    let created_or_staged = metadata.modified().unwrap_or(UNIX_EPOCH);
    let ledger = crate::self_relocate::route_image_last_used(route)
        .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds)));
    ledger.map_or(created_or_staged, |used| used.max(created_or_staged))
}

/// How long ago `then` was. A time in the future (clock skew) counts as now,
/// so skew can only protect disk, never reclaim it early.
fn idle_duration(now: SystemTime, then: SystemTime) -> Duration {
    now.duration_since(then).unwrap_or_default()
}

fn remove_entry(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
#[path = "broker_daemon_disk_tests.rs"]
mod tests;
