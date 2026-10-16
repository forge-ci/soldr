//! Mutual exclusion between requested-output materialization and child spawn
//! (zackees/zccache#1562, zackees/zccache#1597, zackees/soldr#3098).
//!
//! # The race
//!
//! A cache hit or staged miss can materialize a compiler output while holding
//! a writable descriptor: staged paths use a unique sibling temporary before
//! renaming it over the requested path, while legacy cache-file delivery can
//! write its requested path directly. The same daemon process spawns compiler children
//! continuously, and on POSIX every `fork` duplicates the parent's descriptor
//! table: a child forked while the copy's descriptor is open inherits that
//! write descriptor and keeps it until its own `execve` closes it
//! (`O_CLOEXEC` closes on exec, not on fork).
//!
//! `rename(2)` does not change the inode, so after the daemon closes its own
//! descriptor and publishes the path the inherited copy is still a *writable
//! descriptor on the published inode*. Cargo hard-links `build-script-build`
//! to that inode and `execve`s it; `ETXTBSY` is evaluated against the inode,
//! so the exec fails with `Text file busy` for exactly the child's
//! fork-to-exec window. Inspecting the daemon's own `/proc/self/fd` after the
//! rename cannot see the descriptor: it lives in the child's table.
//!
//! # The fix
//!
//! A process-wide `RwLock<()>`. Every child spawn holds the shared guard for
//! the duration of the spawn call (the parent returns from `spawn` only after
//! the child has exec'd or failed, so the shared guard brackets the entire
//! fork-to-exec window). Materialization holds the exclusive guard from the
//! moment the sibling temporary is opened for writing until the rename has
//! published it, so no child can be forked while a materialization descriptor
//! is open, and no descriptor can be opened while a child is between fork and
//! exec.
//!
//! The critical sections contain no `.await`: the guard is a `std` lock held
//! across a synchronous spawn or a synchronous copy + rename only.

use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

static SPAWN_MATERIALIZE_LOCK: RwLock<()> = RwLock::new(());

/// # Bounded holds (soldr#3836)
///
/// A shared hold is a fork/exec and lasts microseconds; an exclusive hold is
/// one artifact copy + rename. On 2026-10-16 a shared guard was held across a
/// blocking child wait whose completion needed another shared acquisition
/// queued behind a writer, and the daemon wedged silently for half an hour
/// with every compile stalled. So every wait for and every hold of this lock
/// is registered with its call site, and a watchdog aborts the process --
/// loudly, naming every registered site -- once a shared hold exceeds
/// [`hold_limit`], or an exclusive hold exceeds [`EXCLUSIVE_HOLD_FACTOR`]
/// times it (a multi-GiB copy on a slow disk is legitimate). A crash the
/// broker relaunches beats a host-wide wedge nobody can diagnose.
///
/// An overdue *wait* is logged once and never aborts: a deadlock always shows
/// up as an overdue hold, while a long wait alone can be a queue of
/// legitimate copies under load, and aborting then would lose every
/// in-flight compile.
const DEFAULT_HOLD_LIMIT: Duration = Duration::from_secs(15);

/// Override for [`DEFAULT_HOLD_LIMIT`], in whole seconds (slow disks, debugging).
const HOLD_LIMIT_ENV: &str = "ZCCACHE_LOCK_HOLD_LIMIT_SECS";

/// An exclusive hold spans a whole artifact copy, so it gets this many times
/// the shared budget before it counts as wedged.
const EXCLUSIVE_HOLD_FACTOR: u32 = 8;

fn hold_limit() -> Duration {
    static LIMIT: OnceLock<Duration> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var(HOLD_LIMIT_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .map_or(DEFAULT_HOLD_LIMIT, Duration::from_secs)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Waiting,
    Holding,
}

#[derive(Clone, Debug)]
struct Entry {
    id: u64,
    side: &'static str,
    phase: Phase,
    site: &'static Location<'static>,
    thread: String,
    since: Instant,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Registry of current waiters and holders. Its critical sections are a push,
/// a field update or a remove -- never a wait -- so it cannot itself wedge.
fn registry() -> &'static Mutex<Vec<Entry>> {
    static REGISTRY: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

fn with_registry<R>(f: impl FnOnce(&mut Vec<Entry>) -> R) -> R {
    let mut entries = registry().lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut entries)
}

fn register_waiting(side: &'static str, site: &'static Location<'static>) -> u64 {
    start_watchdog();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let thread = std::thread::current()
        .name()
        .map_or_else(|| format!("{:?}", std::thread::current().id()), str::to_string);
    with_registry(|entries| {
        entries.push(Entry {
            id,
            side,
            phase: Phase::Waiting,
            site,
            thread,
            since: Instant::now(),
        });
    });
    id
}

fn mark_holding(id: u64) {
    with_registry(|entries| {
        if let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) {
            entry.phase = Phase::Holding;
            entry.since = Instant::now();
        }
    });
}

fn unregister(id: u64) {
    with_registry(|entries| entries.retain(|entry| entry.id != id));
}

/// What the watchdog does about the registry at one tick.
#[derive(Debug, PartialEq, Eq)]
enum Overdue {
    /// A hold overran its budget: a deadlock or a guard held across blocking work.
    Abort(String),
    /// Only waits overran (each listed id is reported once).
    Report(Vec<u64>, String),
}

/// The watchdog's decision for `entries` at `now`, with every registered site
/// in the report. Waits already in `reported` are not reported again. Pure,
/// so the decision is unit-testable.
fn overdue_report(
    entries: &[Entry],
    now: Instant,
    limit: Duration,
    reported: &[u64],
) -> Option<Overdue> {
    let age = |entry: &Entry| now.saturating_duration_since(entry.since);
    let hold_overrun = entries.iter().any(|entry| {
        let budget = if entry.side == "exclusive" {
            limit * EXCLUSIVE_HOLD_FACTOR
        } else {
            limit
        };
        entry.phase == Phase::Holding && age(entry) > budget
    });
    let new_waits: Vec<u64> = entries
        .iter()
        .filter(|entry| {
            entry.phase == Phase::Waiting && age(entry) > limit && !reported.contains(&entry.id)
        })
        .map(|entry| entry.id)
        .collect();
    if !hold_overrun && new_waits.is_empty() {
        return None;
    }
    let what = if hold_overrun { "hold" } else { "wait" };
    let mut report = format!(
        "zccache spawn/materialization lock: a {what} overran its budget ({}s shared, \
         {}s exclusive; soldr#3836). Registered sites:",
        limit.as_secs(),
        (limit * EXCLUSIVE_HOLD_FACTOR).as_secs()
    );
    for entry in entries {
        report.push_str(&format!(
            "\n  {:?} {} for {:.1}s at {} (thread {})",
            entry.phase,
            entry.side,
            now.saturating_duration_since(entry.since).as_secs_f64(),
            entry.site,
            entry.thread,
        ));
    }
    Some(if hold_overrun {
        Overdue::Abort(report)
    } else {
        Overdue::Report(new_waits, report)
    })
}

fn start_watchdog() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("zccache-lock-watchdog".to_string())
            .spawn(|| {
                let mut reported: Vec<u64> = Vec::new();
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let decision = with_registry(|entries| {
                        reported.retain(|id| entries.iter().any(|entry| entry.id == *id));
                        overdue_report(entries, Instant::now(), hold_limit(), &reported)
                    });
                    match decision {
                        Some(Overdue::Abort(report)) => {
                            tracing::error!(event = "spawn_lock_overdue", "{report}");
                            eprintln!("{report}");
                            std::process::abort();
                        }
                        Some(Overdue::Report(ids, report)) => {
                            tracing::warn!(event = "spawn_lock_wait_overdue", "{report}");
                            reported.extend(ids);
                        }
                        None => {}
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "spawn-lock watchdog could not start");
        }
    });
}

/// Shared guard for one child spawn. Hold it for the spawn call only -- never
/// across a wait for the child (see the module docs and soldr#3836).
pub(crate) struct SpawnGuard {
    _guard: RwLockReadGuard<'static, ()>,
    id: u64,
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        unregister(self.id);
    }
}

/// Exclusive guard for one materialization copy + publish.
pub(crate) struct MaterializeGuard {
    _guard: RwLockWriteGuard<'static, ()>,
    id: u64,
}

impl Drop for MaterializeGuard {
    fn drop(&mut self) {
        unregister(self.id);
    }
}

/// Shared guard for one child spawn. Hold it for the spawn call only.
#[track_caller]
pub(crate) fn spawn_shared() -> SpawnGuard {
    let id = register_waiting("shared", Location::caller());
    let guard = SPAWN_MATERIALIZE_LOCK
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    mark_holding(id);
    SpawnGuard { _guard: guard, id }
}

/// Exclusive guard for one materialization copy + publish. Hold it from before
/// the sibling temporary is opened for writing until after the rename.
#[track_caller]
pub(crate) fn materialize_exclusive() -> MaterializeGuard {
    let id = register_waiting("exclusive", Location::caller());
    let guard = SPAWN_MATERIALIZE_LOCK
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    mark_holding(id);
    MaterializeGuard { _guard: guard, id }
}

/// Longest a publisher waits for a foreign child to exec. A fork-to-exec
/// window is normally microseconds; this bounds a pathological one.
const PUBLISH_WRITER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Return only once no process holds the just-published `path` open for
/// writing (zackees/soldr#3350).
///
/// The lock above excludes only spawns that take it. Embedded in soldr's
/// daemon, zccache shares its process with code that forks under another
/// lock or none, and such a child inherits the copy's write descriptor
/// across the rename. Waiting here, after the exclusive guard is released,
/// covers every spawner. Best effort: a timeout or an unobservable host is
/// logged, never an error.
pub(crate) fn await_publishable(path: &std::path::Path) {
    use crate::daemon_core::platform::fs::writers::{await_no_writers, WriterWait};

    match await_no_writers(path, PUBLISH_WRITER_TIMEOUT) {
        Ok(WriterWait::Clear { waited }) if !waited.is_zero() => tracing::info!(
            event = "materialize_awaited_inherited_writer",
            path = %path.display(),
            waited_us = waited.as_micros() as u64,
            "a forked child held the published output open for writing"
        ),
        Ok(WriterWait::TimedOut) => tracing::warn!(
            event = "materialize_inherited_writer_timeout",
            path = %path.display(),
            "the published output is still open for writing by another process"
        ),
        Ok(_) => {}
        Err(error) => tracing::debug!(
            event = "materialize_writer_probe_failed",
            path = %path.display(),
            %error
        ),
    }
}

#[cfg(test)]
mod watchdog_tests {
    use super::{overdue_report, Entry, Overdue, Phase};
    use std::panic::Location;
    use std::time::{Duration, Instant};

    fn entry(id: u64, phase: Phase, age: Duration, now: Instant) -> Entry {
        side_entry(id, "shared", phase, age, now)
    }

    fn side_entry(
        id: u64,
        side: &'static str,
        phase: Phase,
        age: Duration,
        now: Instant,
    ) -> Entry {
        Entry {
            id,
            side,
            phase,
            site: Location::caller(),
            thread: "t".to_string(),
            since: now - age,
        }
    }

    #[test]
    fn nothing_overdue_reports_nothing() {
        let now = Instant::now();
        let entries = [entry(1, Phase::Holding, Duration::from_secs(2), now)];
        assert!(overdue_report(&entries, now, Duration::from_secs(15), &[]).is_none());
    }

    /// soldr#3836: a guard held past the limit must be reported with every
    /// registered site, including the waiters it is blocking.
    #[test]
    fn an_overdue_hold_reports_every_registered_site() {
        let now = Instant::now();
        let entries = [
            entry(1, Phase::Holding, Duration::from_secs(20), now),
            entry(2, Phase::Waiting, Duration::from_secs(3), now),
        ];
        let Some(Overdue::Abort(report)) =
            overdue_report(&entries, now, Duration::from_secs(15), &[])
        else {
            panic!("an overdue shared hold must abort");
        };
        assert!(report.contains("Holding shared for 20."), "{report}");
        assert!(report.contains("Waiting shared for 3."), "{report}");
        assert!(report.contains("spawn_exclusion.rs"), "{report}");
    }

    /// A queue of legitimate copies can make a wait long; only a hold proves
    /// a wedge. A wait is reported once and never aborts.
    #[test]
    fn an_overdue_wait_is_reported_once_and_does_not_abort() {
        let now = Instant::now();
        let limit = Duration::from_secs(15);
        let entries = [entry(1, Phase::Waiting, Duration::from_secs(16), now)];
        let Some(Overdue::Report(ids, _)) = overdue_report(&entries, now, limit, &[]) else {
            panic!("an overdue wait must be reported, not abort");
        };
        assert_eq!(ids, vec![1]);
        assert!(overdue_report(&entries, now, limit, &ids).is_none());
    }

    /// An exclusive hold is a whole artifact copy: 60 s on a slow disk is
    /// legitimate, a hold past the exclusive budget is not.
    #[test]
    fn an_exclusive_hold_gets_the_copy_budget() {
        let now = Instant::now();
        let limit = Duration::from_secs(15);
        let copying = [side_entry(1, "exclusive", Phase::Holding, Duration::from_secs(60), now)];
        assert!(overdue_report(&copying, now, limit, &[]).is_none());
        let wedged = [side_entry(1, "exclusive", Phase::Holding, Duration::from_secs(121), now)];
        assert!(matches!(
            overdue_report(&wedged, now, limit, &[]),
            Some(Overdue::Abort(_))
        ));
    }

    #[test]
    fn guards_unregister_on_drop() {
        let before = super::with_registry(|entries| entries.len());
        {
            let _shared = super::spawn_shared();
            assert!(super::with_registry(|entries| entries.len()) > before);
        }
        assert_eq!(super::with_registry(|entries| entries.len()), before);
    }
}
