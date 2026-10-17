//! zccache concurrency primitives: every wait is bounded (soldr#3836).
//!
//! Raw `std::sync` / `parking_lot` / `tokio::sync` locks can wait forever, and
//! a wait that never ends is invisible: on 2026-10-16 a lock guard held across
//! a blocking child wait wedged the soldr daemon (which hosts zccache) for half an hour while every
//! build on the host stalled, with no log line naming the culprit. The
//! `ci/check_zccache_raw_locks.py` gate (zccache is not a workspace member, so the soldr Dylint cannot see it) therefore rejects those primitives outside
//! this module, and these wrappers offer exactly two flavors:
//!
//! - **Critical section** -- [`Mutex::lock`], [`RwLock::read`],
//!   [`RwLock::write`]. The caller expects the lock promptly and holds it
//!   briefly. Every wait and every hold is registered with its call site; a
//!   watchdog aborts the process, printing every registered site, once any
//!   *hold* exceeds the hold limit (15 s by default,
//!   `ZCCACHE_LOCK_HOLD_LIMIT_SECS` to override). An overrun hold is a bug -- a
//!   deadlock or a guard held across blocking work -- and a crash that names
//!   the site beats a silent wedge. An overdue *wait* is only reported: a
//!   deadlock always shows up as an overdue hold, while a long wait alone can
//!   be a queue of brief holds under load, which must not kill the process.
//! - **Timeout** -- [`Mutex::lock_for`], [`RwLock::read_for`],
//!   [`RwLock::write_for`]. The caller can legitimately wait and has a
//!   fallback; the signature makes it handle [`LockTimeout`].
//!
//! Poisoning is recovered (`into_inner`): a panicking holder already reported
//! itself, and the protected data is still the best state available.

use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, PoisonError, TryLockError};
use std::time::{Duration, Instant};

const DEFAULT_HOLD_LIMIT: Duration = Duration::from_secs(15);
const HOLD_LIMIT_ENV: &str = "ZCCACHE_LOCK_HOLD_LIMIT_SECS";

/// The critical-section budget for any single hold (and the report threshold
/// for any single wait).
pub fn hold_limit() -> Duration {
    static LIMIT: OnceLock<Duration> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var(HOLD_LIMIT_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .map_or(DEFAULT_HOLD_LIMIT, Duration::from_secs)
    })
}

/// A timeout-flavor acquisition gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockTimeout {
    /// How long the caller waited before giving up.
    pub waited: Duration,
}

impl std::fmt::Display for LockTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lock not acquired within {:?}", self.waited)
    }
}

impl std::error::Error for LockTimeout {}

// ---------------------------------------------------------------------------
// Registry and watchdog
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Waiting,
    Holding,
}

#[derive(Clone, Debug)]
struct Entry {
    id: u64,
    kind: &'static str,
    phase: Phase,
    site: &'static Location<'static>,
    thread: String,
    since: Instant,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Current waiters and holders. Its own critical sections are a push, a field
/// update or a remove -- never a wait -- so it cannot wedge.
#[allow(clippy::disallowed_types)] // the one raw lock: the boundary's own bookkeeping
fn registry() -> &'static std::sync::Mutex<Vec<Entry>> {
    static REGISTRY: OnceLock<std::sync::Mutex<Vec<Entry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn with_registry<R>(f: impl FnOnce(&mut Vec<Entry>) -> R) -> R {
    let mut entries = registry().lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut entries)
}

fn current_thread_name() -> String {
    let thread = std::thread::current();
    thread
        .name()
        .map_or_else(|| format!("{:?}", thread.id()), str::to_string)
}

fn register_waiting(kind: &'static str, site: &'static Location<'static>) -> u64 {
    start_watchdog();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let entry = Entry {
        id,
        kind,
        phase: Phase::Waiting,
        site,
        thread: current_thread_name(),
        since: Instant::now(),
    };
    with_registry(|entries| entries.push(entry));
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
    /// A hold overran: a deadlock or a guard held across blocking work.
    Abort(String),
    /// Only waits overran (each listed id is reported once).
    Report(Vec<u64>, String),
}

/// The watchdog's decision for `entries` at `now`, with every registered
/// site in the report. Waits already in `reported` are not reported again.
/// Pure, so the decision is unit-testable.
fn overdue_report(
    entries: &[Entry],
    now: Instant,
    limit: Duration,
    reported: &[u64],
) -> Option<Overdue> {
    let overdue = |entry: &&Entry| now.saturating_duration_since(entry.since) > limit;
    let hold_overrun = entries
        .iter()
        .filter(overdue)
        .any(|entry| entry.phase == Phase::Holding);
    let new_waits: Vec<u64> = entries
        .iter()
        .filter(overdue)
        .filter(|entry| entry.phase == Phase::Waiting && !reported.contains(&entry.id))
        .map(|entry| entry.id)
        .collect();
    if !hold_overrun && new_waits.is_empty() {
        return None;
    }
    let what = if hold_overrun { "hold" } else { "wait" };
    let mut report = format!(
        "zccache: a lock {what} exceeded {}s (critical sections are expected to be brief; \
         soldr#3836). Registered sites:",
        limit.as_secs()
    );
    for entry in entries {
        report.push_str(&format!(
            "\n  {:?} {} for {:.1}s at {} (thread {})",
            entry.phase,
            entry.kind,
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
        // The watchdog only observes; if the host cannot give it a thread the
        // locks still work, just without the overrun backstop.
        let _ = std::thread::Builder::new()
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
                            eprintln!("{report}");
                            std::process::abort();
                        }
                        Some(Overdue::Report(ids, report)) => {
                            eprintln!("{report}");
                            reported.extend(ids);
                        }
                        None => {}
                    }
                }
            });
    });
}

/// Registration that unregisters on drop, so an unwinding holder never leaves
/// a stale entry for the watchdog.
struct Ticket(u64);

impl Drop for Ticket {
    fn drop(&mut self) {
        unregister(self.0);
    }
}

/// Poll `attempt` until it yields or `timeout` passes, backing off from 50 us
/// to 5 ms between attempts.
fn poll_for<G>(
    timeout: Duration,
    mut attempt: impl FnMut() -> Option<G>,
) -> Result<G, LockTimeout> {
    let start = Instant::now();
    let mut backoff = Duration::from_micros(50);
    loop {
        if let Some(guard) = attempt() {
            return Ok(guard);
        }
        let waited = start.elapsed();
        if waited >= timeout {
            return Err(LockTimeout { waited });
        }
        std::thread::sleep(backoff.min(timeout - waited));
        backoff = (backoff * 2).min(Duration::from_millis(5));
    }
}

fn try_result<G>(result: Result<G, TryLockError<G>>) -> Option<G> {
    match result {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

// ---------------------------------------------------------------------------
// Mutex
// ---------------------------------------------------------------------------

/// A mutex whose waits are bounded. See the module docs for the two flavors.
#[derive(Debug, Default)]
#[allow(clippy::disallowed_types)]
pub struct Mutex<T: ?Sized> {
    inner: std::sync::Mutex<T>,
}

/// Guard for [`Mutex`]; a critical-section guard stays registered until drop.
pub struct MutexGuard<'a, T: ?Sized> {
    guard: std::sync::MutexGuard<'a, T>,
    _ticket: Option<Ticket>,
}

impl<T> Mutex<T> {
    /// Create an unlocked mutex.
    pub const fn new(value: T) -> Self {
        Self {
            inner: std::sync::Mutex::new(value),
        }
    }

    /// Consume the mutex and return its value.
    pub fn into_inner(self) -> T {
        self.inner
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Critical section: lock, expecting to get it promptly and hold it
    /// briefly. An overrun of [`hold_limit`] aborts with every registered site.
    #[track_caller]
    pub fn lock(&self) -> MutexGuard<'_, T> {
        let ticket = Ticket(register_waiting("Mutex", Location::caller()));
        let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        mark_holding(ticket.0);
        MutexGuard {
            guard,
            _ticket: Some(ticket),
        }
    }

    /// Timeout flavor: lock within `timeout`, or report [`LockTimeout`].
    /// The returned guard is still subject to the critical-section hold limit.
    #[track_caller]
    pub fn lock_for(&self, timeout: Duration) -> Result<MutexGuard<'_, T>, LockTimeout> {
        let site = Location::caller();
        let guard = poll_for(timeout, || try_result(self.inner.try_lock()))?;
        let ticket = Ticket(register_waiting("Mutex", site));
        mark_holding(ticket.0);
        Ok(MutexGuard {
            guard,
            _ticket: Some(ticket),
        })
    }

    /// Non-blocking attempt; `None` if another holder has it.
    #[track_caller]
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        let site = Location::caller();
        let guard = try_result(self.inner.try_lock())?;
        let ticket = Ticket(register_waiting("Mutex", site));
        mark_holding(ticket.0);
        Some(MutexGuard {
            guard,
            _ticket: Some(ticket),
        })
    }

    /// Exclusive access through `&mut self`; no locking needed.
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a holder panicked. Acquisition still succeeds (poison is
    /// recovered); this is for diagnostics and tests.
    pub fn is_poisoned(&self) -> bool {
        self.inner.is_poisoned()
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

// ---------------------------------------------------------------------------
// Condvar
// ---------------------------------------------------------------------------

/// A condition variable for [`Mutex`]. Only timed waits exist: an untimed wait
/// is exactly the unbounded wait this module forbids. While parked, the
/// guard's registry entry is marked waiting and its clock restarts on every
/// wait, so a loop of short timed waits never trips the watchdog but a single
/// wait longer than [`hold_limit`] does.
#[derive(Debug, Default)]
#[allow(clippy::disallowed_types)]
pub struct Condvar {
    inner: std::sync::Condvar,
}

fn mark_waiting(id: u64) {
    with_registry(|entries| {
        if let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) {
            entry.phase = Phase::Waiting;
            entry.since = Instant::now();
        }
    });
}

impl Condvar {
    /// Create a condition variable.
    pub const fn new() -> Self {
        Self {
            inner: std::sync::Condvar::new(),
        }
    }

    /// Wait for a notification or `timeout`; returns the reacquired guard and
    /// whether the wait timed out.
    pub fn wait_timeout<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        timeout: Duration,
    ) -> (MutexGuard<'a, T>, bool) {
        self.wait_timeout_while(guard, timeout, |_| false)
    }

    /// Wait while `condition` holds, for at most `timeout` in total; returns the
    /// reacquired guard and whether it timed out with the condition still true.
    pub fn wait_timeout_while<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        timeout: Duration,
        condition: impl FnMut(&mut T) -> bool,
    ) -> (MutexGuard<'a, T>, bool) {
        let MutexGuard { guard, _ticket } = guard;
        if let Some(ticket) = &_ticket {
            mark_waiting(ticket.0);
        }
        let (inner, result) = self
            .inner
            .wait_timeout_while(guard, timeout, condition)
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(ticket) = &_ticket {
            mark_holding(ticket.0);
        }
        (
            MutexGuard {
                guard: inner,
                _ticket,
            },
            result.timed_out(),
        )
    }

    /// Wake one waiter.
    pub fn notify_one(&self) {
        self.inner.notify_one();
    }

    /// Wake every waiter.
    pub fn notify_all(&self) {
        self.inner.notify_all();
    }
}

// ---------------------------------------------------------------------------
// RwLock
// ---------------------------------------------------------------------------

/// A reader-writer lock whose waits are bounded. See the module docs.
#[derive(Debug, Default)]
#[allow(clippy::disallowed_types)]
pub struct RwLock<T: ?Sized> {
    inner: std::sync::RwLock<T>,
}

/// Shared guard for [`RwLock`].
pub struct RwLockReadGuard<'a, T: ?Sized> {
    guard: std::sync::RwLockReadGuard<'a, T>,
    _ticket: Ticket,
}

/// Exclusive guard for [`RwLock`].
pub struct RwLockWriteGuard<'a, T: ?Sized> {
    guard: std::sync::RwLockWriteGuard<'a, T>,
    _ticket: Ticket,
}

impl<T> RwLock<T> {
    /// Create an unlocked reader-writer lock.
    pub const fn new(value: T) -> Self {
        Self {
            inner: std::sync::RwLock::new(value),
        }
    }

    /// Consume the lock and return its value.
    pub fn into_inner(self) -> T {
        self.inner
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: ?Sized> RwLock<T> {
    /// Critical section, shared.
    #[track_caller]
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        let ticket = Ticket(register_waiting("RwLock(read)", Location::caller()));
        let guard = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        mark_holding(ticket.0);
        RwLockReadGuard {
            guard,
            _ticket: ticket,
        }
    }

    /// Critical section, exclusive.
    #[track_caller]
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        let ticket = Ticket(register_waiting("RwLock(write)", Location::caller()));
        let guard = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        mark_holding(ticket.0);
        RwLockWriteGuard {
            guard,
            _ticket: ticket,
        }
    }

    /// Timeout flavor, shared.
    #[track_caller]
    pub fn read_for(&self, timeout: Duration) -> Result<RwLockReadGuard<'_, T>, LockTimeout> {
        let site = Location::caller();
        let guard = poll_for(timeout, || try_result(self.inner.try_read()))?;
        let ticket = Ticket(register_waiting("RwLock(read)", site));
        mark_holding(ticket.0);
        Ok(RwLockReadGuard {
            guard,
            _ticket: ticket,
        })
    }

    /// Timeout flavor, exclusive.
    #[track_caller]
    pub fn write_for(&self, timeout: Duration) -> Result<RwLockWriteGuard<'_, T>, LockTimeout> {
        let site = Location::caller();
        let guard = poll_for(timeout, || try_result(self.inner.try_write()))?;
        let ticket = Ticket(register_waiting("RwLock(write)", site));
        mark_holding(ticket.0);
        Ok(RwLockWriteGuard {
            guard,
            _ticket: ticket,
        })
    }

    /// Exclusive access through `&mut self`; no locking needed.
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T: ?Sized> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T: ?Sized> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

// ---------------------------------------------------------------------------
// Async (tokio) locks
// ---------------------------------------------------------------------------

/// An async mutex with the same two flavors. A critical-section guard may be
/// held across `.await`, but its whole hold still counts against
/// [`hold_limit`]: an async critical section that needs longer is either a
/// guard held across slow I/O (restructure) or a wait with a fallback (use
/// [`AsyncMutex::lock_for`]).
#[derive(Debug, Default)]
pub struct AsyncMutex<T: ?Sized> {
    inner: tokio::sync::Mutex<T>,
}

/// Guard for [`AsyncMutex`].
pub struct AsyncMutexGuard<'a, T: ?Sized> {
    guard: tokio::sync::MutexGuard<'a, T>,
    _ticket: Ticket,
}

impl<T> AsyncMutex<T> {
    /// Create an unlocked async mutex.
    pub fn new(value: T) -> Self {
        Self {
            inner: tokio::sync::Mutex::new(value),
        }
    }

    /// Consume the mutex and return its value.
    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

impl<T: ?Sized> AsyncMutex<T> {
    /// Critical section (watchdog-bounded wait and hold).
    #[track_caller]
    pub fn lock(&self) -> impl std::future::Future<Output = AsyncMutexGuard<'_, T>> {
        let site = Location::caller();
        async move {
            let ticket = Ticket(register_waiting("AsyncMutex", site));
            #[allow(clippy::disallowed_methods)]
            let guard = self.inner.lock().await;
            mark_holding(ticket.0);
            AsyncMutexGuard {
                guard,
                _ticket: ticket,
            }
        }
    }

    /// Timeout flavor.
    #[track_caller]
    pub fn lock_for(
        &self,
        timeout: Duration,
    ) -> impl std::future::Future<Output = Result<AsyncMutexGuard<'_, T>, LockTimeout>> {
        let site = Location::caller();
        async move {
            let start = Instant::now();
            let guard = tokio::time::timeout(timeout, self.inner.lock())
                .await
                .map_err(|_| LockTimeout {
                    waited: start.elapsed(),
                })?;
            let ticket = Ticket(register_waiting("AsyncMutex", site));
            mark_holding(ticket.0);
            Ok(AsyncMutexGuard {
                guard,
                _ticket: ticket,
            })
        }
    }

    /// Non-blocking attempt.
    #[track_caller]
    pub fn try_lock(&self) -> Option<AsyncMutexGuard<'_, T>> {
        let site = Location::caller();
        let guard = self.inner.try_lock().ok()?;
        let ticket = Ticket(register_waiting("AsyncMutex", site));
        mark_holding(ticket.0);
        Some(AsyncMutexGuard {
            guard,
            _ticket: ticket,
        })
    }

    /// Exclusive access through `&mut self`.
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }
}

impl<T: ?Sized> Deref for AsyncMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T: ?Sized> DerefMut for AsyncMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

/// An async reader-writer lock with the same two flavors.
#[derive(Debug, Default)]
pub struct AsyncRwLock<T: ?Sized> {
    inner: tokio::sync::RwLock<T>,
}

/// Shared guard for [`AsyncRwLock`].
pub struct AsyncRwLockReadGuard<'a, T: ?Sized> {
    guard: tokio::sync::RwLockReadGuard<'a, T>,
    _ticket: Ticket,
}

/// Exclusive guard for [`AsyncRwLock`].
pub struct AsyncRwLockWriteGuard<'a, T: ?Sized> {
    guard: tokio::sync::RwLockWriteGuard<'a, T>,
    _ticket: Ticket,
}

impl<T> AsyncRwLock<T> {
    /// Create an unlocked async reader-writer lock.
    pub fn new(value: T) -> Self {
        Self {
            inner: tokio::sync::RwLock::new(value),
        }
    }
}

impl<T: ?Sized> AsyncRwLock<T> {
    /// Critical section, shared.
    #[track_caller]
    pub fn read(&self) -> impl std::future::Future<Output = AsyncRwLockReadGuard<'_, T>> {
        let site = Location::caller();
        async move {
            let ticket = Ticket(register_waiting("AsyncRwLock(read)", site));
            #[allow(clippy::disallowed_methods)]
            let guard = self.inner.read().await;
            mark_holding(ticket.0);
            AsyncRwLockReadGuard {
                guard,
                _ticket: ticket,
            }
        }
    }

    /// Critical section, exclusive.
    #[track_caller]
    pub fn write(&self) -> impl std::future::Future<Output = AsyncRwLockWriteGuard<'_, T>> {
        let site = Location::caller();
        async move {
            let ticket = Ticket(register_waiting("AsyncRwLock(write)", site));
            #[allow(clippy::disallowed_methods)]
            let guard = self.inner.write().await;
            mark_holding(ticket.0);
            AsyncRwLockWriteGuard {
                guard,
                _ticket: ticket,
            }
        }
    }

    /// Timeout flavor, shared.
    #[track_caller]
    pub fn read_for(
        &self,
        timeout: Duration,
    ) -> impl std::future::Future<Output = Result<AsyncRwLockReadGuard<'_, T>, LockTimeout>> {
        let site = Location::caller();
        async move {
            let start = Instant::now();
            let guard = tokio::time::timeout(timeout, self.inner.read())
                .await
                .map_err(|_| LockTimeout {
                    waited: start.elapsed(),
                })?;
            let ticket = Ticket(register_waiting("AsyncRwLock(read)", site));
            mark_holding(ticket.0);
            Ok(AsyncRwLockReadGuard {
                guard,
                _ticket: ticket,
            })
        }
    }

    /// Timeout flavor, exclusive.
    #[track_caller]
    pub fn write_for(
        &self,
        timeout: Duration,
    ) -> impl std::future::Future<Output = Result<AsyncRwLockWriteGuard<'_, T>, LockTimeout>> {
        let site = Location::caller();
        async move {
            let start = Instant::now();
            let guard = tokio::time::timeout(timeout, self.inner.write())
                .await
                .map_err(|_| LockTimeout {
                    waited: start.elapsed(),
                })?;
            let ticket = Ticket(register_waiting("AsyncRwLock(write)", site));
            mark_holding(ticket.0);
            Ok(AsyncRwLockWriteGuard {
                guard,
                _ticket: ticket,
            })
        }
    }
}

impl<T: ?Sized> Deref for AsyncRwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T: ?Sized> Deref for AsyncRwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T: ?Sized> DerefMut for AsyncRwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
