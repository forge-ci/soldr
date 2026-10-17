//! soldr#3053 — process-global registry of compiles currently executing
//! inside the embedded zccache service, so an RSS-ceiling breach dump can
//! name WHICH compiles the daemon was holding, not just how much RSS it
//! held. CLAUDE.md's "Diagnosing before capping" section is explicit that
//! cgroup OOM counters alone are not sufficient evidence for a memory
//! incident; the compiler's own peak-RSS telemetry plus this registry's
//! in-flight unit list are what turn "the daemon OOMed" into "the daemon
//! OOMed while holding these N units".
//!
//! This is a process-global `static`, not a field on
//! `EmbeddedZccacheService`, because the reader
//! (`daemon::rss_ceiling::write_breach_dump`) is a free function running on
//! the watchdog task and has no handle to the service instance -- the
//! watchdog and the service are independent tasks that only share the
//! process.
//!
//! The broker runs the exact same watchdog and never calls [`register`], so
//! [`snapshot`] returning an empty list from a broker process is correct,
//! not a bug -- `BreachSummary::role` is what disambiguates "empty because
//! broker" from "empty because idle daemon" for a dump reader.
//!
//! Deliberately records only unit *identity* -- crate name and the
//! `<crate>/<metadata>` unit key -- never argv or env. A breach dump is
//! sometimes shared outside the machine that produced it, so it must never
//! carry environment secrets.

use crate::platform::sync::Mutex;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// One compile the daemon is currently holding open, as seen by a breach
/// dump reader. Ordering within a [`snapshot()`] call is oldest-first (by
/// `id`), so the unit that has been running longest -- the one most likely
/// to be responsible for sustained memory growth -- sorts to the top.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct InflightCompile {
    pub id: u64,
    pub unit_key: Option<String>,
    pub crate_name: Option<String>,
    pub started_at_ms: i64,
    pub elapsed_ms: u64,
}

/// Registry entry. Does not carry `elapsed_ms` -- that is derived at
/// [`snapshot()`] time from `started_at_ms` so the registry never needs a
/// background updater.
struct Entry {
    unit_key: Option<String>,
    crate_name: Option<String>,
    started_at_ms: i64,
}

/// Monotonic id source. Mirrors the counter shape of
/// `server_compile::next_compile_id` -- a plain `fetch_add` is sufficient
/// because uniqueness, not ordering across restarts, is all that is needed.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// `Mutex::new` and `BTreeMap::new` are both `const`, so this needs no
/// `OnceLock` lazy-init. `BTreeMap` (not `HashMap`) so `snapshot()` comes
/// back ordered by `id`, i.e. oldest compile first.
static REGISTRY: Mutex<BTreeMap<u64, Entry>> = Mutex::new(BTreeMap::new());

/// RAII handle for one registered compile. Dropping it (including via an
/// early return or a panic unwind) removes the corresponding entry, so
/// callers do not need an explicit "compile finished" call.
pub(crate) struct InflightGuard {
    id: u64,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        // Poison-proof: this can run during unwind while the process is
        // already in a bad state, and a panic here would only make things
        // worse. Degrade to a stale-but-usable map rather than panic.
        let mut map = REGISTRY.lock();
        map.remove(&self.id);
    }
}

/// Register a compile as in-flight and return a guard that un-registers it
/// on drop. Call this for the full lifetime of the compile -- hold the
/// guard, do not drop it early -- so a breach mid-compile still finds the
/// entry.
pub(crate) fn register(unit_key: Option<String>, crate_name: Option<String>) -> InflightGuard {
    let id = SEQ.fetch_add(1, Ordering::Relaxed);
    let entry = Entry {
        unit_key,
        crate_name,
        started_at_ms: unix_millis(),
    };
    // Poison-proof for the same reason as `Drop::drop`: this registry is
    // read on the breach path, i.e. exactly when the process is already
    // unhealthy, and a poisoned mutex must degrade to a stale-but-readable
    // list rather than panic the dump away.
    let mut map = REGISTRY.lock();
    map.insert(id, entry);
    InflightGuard { id }
}

/// How many compiles are registered right now. The idle watchdog reads this so
/// a daemon that is busy compiling is never mistaken for an idle one
/// (soldr#3377).
pub(crate) fn count() -> usize {
    REGISTRY.lock().len()
}

/// Snapshot every compile currently registered, oldest first. Never holds
/// the lock across I/O -- the lock is released before this function
/// returns, well before any caller writes the result to disk.
pub(crate) fn snapshot() -> Vec<InflightCompile> {
    let now_ms = unix_millis();
    // Poison-proof for the same reason as `register`/`Drop::drop`: the
    // breach path must still get a list even if some other panic already
    // poisoned this mutex.
    let map = REGISTRY.lock();
    map.iter()
        .map(|(&id, entry)| InflightCompile {
            id,
            unit_key: entry.unit_key.clone(),
            crate_name: entry.crate_name.clone(),
            started_at_ms: entry.started_at_ms,
            elapsed_ms: now_ms.saturating_sub(entry.started_at_ms).max(0) as u64,
        })
        .collect()
}

/// Identical in behaviour to the private `unix_millis` at the bottom of
/// `daemon::rss_ceiling` -- duplicated rather than shared because that
/// module's copy is private and this module must not depend on
/// `rss_ceiling` (the dependency runs the other way: `rss_ceiling` reads
/// this module's `snapshot()`).
fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is process-global and nextest/libtest run every test in
    /// a binary as threads of one process, so these tests must never assert
    /// on the total length of `snapshot()` or on it being empty -- another
    /// test's entries may be alive concurrently. Each test uses its own
    /// unique unit-key string and filters `snapshot()` down to just those.

    #[test]
    fn count_grows_while_a_compile_is_registered_and_shrinks_on_drop() {
        // Other tests register concurrently, so assert only the direction of
        // change this test itself causes.
        let guard = register(Some("t-count/1".to_string()), None);
        assert!(count() >= 1, "a registered compile must be counted");
        drop(guard);
        assert!(!snapshot()
            .iter()
            .any(|c| c.unit_key.as_deref() == Some("t-count/1")));
    }

    #[test]
    fn register_then_snapshot_lists_the_unit_and_drop_removes_it() {
        let guard = register(
            Some("t1-alpha/abc123".to_string()),
            Some("alpha".to_string()),
        );

        let found = snapshot()
            .into_iter()
            .find(|c| c.unit_key.as_deref() == Some("t1-alpha/abc123"))
            .expect("registered unit must appear in snapshot");
        assert_eq!(found.crate_name, Some("alpha".to_string()));

        drop(guard);

        let still_present = snapshot()
            .into_iter()
            .any(|c| c.unit_key.as_deref() == Some("t1-alpha/abc123"));
        assert!(
            !still_present,
            "dropped guard must remove its entry from the registry"
        );
    }

    #[test]
    fn ids_are_monotonic_and_snapshot_is_oldest_first() {
        let first = register(
            Some("t1-order-first/aaa111".to_string()),
            Some("order_first".to_string()),
        );
        let second = register(
            Some("t1-order-second/bbb222".to_string()),
            Some("order_second".to_string()),
        );

        let filtered: Vec<InflightCompile> = snapshot()
            .into_iter()
            .filter(|c| {
                c.unit_key.as_deref() == Some("t1-order-first/aaa111")
                    || c.unit_key.as_deref() == Some("t1-order-second/bbb222")
            })
            .collect();

        assert_eq!(filtered.len(), 2, "both registered units must be present");
        assert!(
            filtered[0].id < filtered[1].id,
            "snapshot() must list the oldest (first-registered) entry first"
        );
        assert_eq!(
            filtered[0].unit_key.as_deref(),
            Some("t1-order-first/aaa111")
        );

        drop(first);
        drop(second);
    }

    #[test]
    fn an_unnamed_compile_still_registers() {
        // A compile whose argv has no `--crate-name` is still worth naming
        // by elapsed time alone -- an anonymous entry must still show up so
        // a breach dump reader can see "the daemon was holding N compiles"
        // even when none of them can be named.
        let guard = register(None, None);

        let present = snapshot()
            .iter()
            .any(|c| c.unit_key.is_none() && c.crate_name.is_none() && c.id == guard.id);
        assert!(
            present,
            "an unnamed compile must still produce a snapshot entry"
        );

        drop(guard);
    }

    /// Poisons the REAL `REGISTRY`, not a local `Mutex` fixture: a test that
    /// re-implements the idiom it is checking validates a copy and cannot
    /// catch drift in the original (CLAUDE.md, "Agent Code-Smell Reporting
    /// Rule" -- that exact shape is listed as a trigger). Safe to do to a
    /// process-global here precisely because tolerating poison is the
    /// property under test: `register`, `snapshot` and `Drop` all recover
    /// with `into_inner`, so a poisoned registry stays usable for the rest of
    /// the binary. Under nextest -- the prescribed runner -- each test owns
    /// its own process anyway, so the poison does not leave this test at all.
    #[test]
    fn a_poisoned_registry_still_answers() {
        let poisoned = std::panic::catch_unwind(|| {
            let _held = REGISTRY.lock();
            panic!("deliberately poison the in-flight registry");
        });
        assert!(poisoned.is_err(), "the fixture closure must panic");
        assert!(REGISTRY.is_poisoned(), "the registry must now be poisoned");

        // The breach path must still get a list, and registration must still
        // work, with the mutex poisoned.
        let guard = register(
            Some("t1-poison/ddd444".to_string()),
            Some("poisoned_unit".to_string()),
        );
        assert!(
            snapshot()
                .iter()
                .any(|c| c.unit_key.as_deref() == Some("t1-poison/ddd444")),
            "a poisoned registry must still register and report a compile"
        );

        drop(guard);
        assert!(
            !snapshot()
                .iter()
                .any(|c| c.unit_key.as_deref() == Some("t1-poison/ddd444")),
            "a poisoned registry must still deregister on drop"
        );
    }
}
