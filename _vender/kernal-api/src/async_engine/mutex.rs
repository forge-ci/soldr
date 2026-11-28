//! Facade-owned asynchronous exclusive locking.
//!
//! The lock is fair: waiters acquire in the order their acquisitions first
//! queued, and it does not poison when a holder panics. Every guard is a
//! facade-owned newtype; no backend guard type crosses the facade.

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// An asynchronous mutual-exclusion lock with FIFO waiter order.
///
/// Borrowed guards ([`Self::lock`], [`Self::try_lock`],
/// [`Self::blocking_lock`]) tie the lease to a borrow of the lock. Owned
/// guards ([`Self::lock_owned`], [`Self::try_lock_owned`]) take an
/// `Arc<Mutex<T>>` and may be moved into launched work or stored alongside
/// the value they protect.
///
/// An owned guard retains the very `Arc<Mutex<T>>` it was acquired through,
/// not merely the protected storage. A registry that keys locks by
/// `Weak<Mutex<T>>` therefore keeps observing a held lock after every other
/// strong handle has been dropped: `Weak::upgrade` succeeds for as long as
/// any owned guard is alive, so a second caller receives the same lock and
/// waits for it instead of minting an unrelated one.
pub struct Mutex<T> {
    inner: Arc<tokio::sync::Mutex<T>>,
    holder: HolderSlot,
}

/// Who holds a [`Mutex`] and since when, for diagnosing a wait that
/// expired (#433). A snapshot: the holder may release the lock right after
/// it is taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockHolder {
    location: &'static Location<'static>,
    since: Instant,
}

impl LockHolder {
    /// The source location of the call that acquired the lock.
    pub fn location(&self) -> &'static Location<'static> {
        self.location
    }

    /// How long the lock has been held, as of now.
    pub fn held_for(&self) -> Duration {
        self.since.elapsed()
    }
}

/// The current holder of one lock. Its critical sections only copy a small
/// value in or out, so they cannot themselves wait.
#[derive(Default)]
struct HolderSlot(std::sync::Mutex<Option<LockHolder>>);

impl HolderSlot {
    fn set(&self, location: &'static Location<'static>) {
        let holder = LockHolder {
            location,
            since: Instant::now(),
        };
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(holder);
    }

    fn clear(&self) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    fn get(&self) -> Option<LockHolder> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<T> Mutex<T> {
    /// Create an unlocked value.
    pub fn new(value: T) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(value)),
            holder: HolderSlot::default(),
        }
    }

    /// The current holder, if the lock is held: where it was acquired and
    /// for how long. Use it to report a stuck holder after
    /// [`Self::lock_within`] expires.
    pub fn holder(&self) -> Option<LockHolder> {
        self.holder.get()
    }

    fn borrowed<'a>(
        &'a self,
        inner: tokio::sync::MutexGuard<'a, T>,
        at: &'static Location<'static>,
    ) -> MutexGuard<'a, T> {
        self.holder.set(at);
        MutexGuard {
            inner,
            holder: &self.holder,
        }
    }

    fn owned(
        self: Arc<Self>,
        inner: tokio::sync::OwnedMutexGuard<T>,
        at: &'static Location<'static>,
    ) -> OwnedMutexGuard<T> {
        self.holder.set(at);
        OwnedMutexGuard {
            inner,
            _owner: self,
        }
    }

    /// Acquire a borrowed guard, queuing behind earlier waiters.
    ///
    /// Cancelling the pending acquisition loses its queue position without
    /// acquiring the lock.
    #[deprecated(note = "waits forever for a stuck holder; use `lock_within` (#433)")]
    #[track_caller]
    pub fn lock(&self) -> impl Future<Output = MutexGuard<'_, T>> + '_ {
        let at = Location::caller();
        async move { self.borrowed(self.inner.lock().await, at) }
    }

    /// Acquire an owned guard that keeps this `Arc<Mutex<T>>` alive until the
    /// guard is dropped.
    ///
    /// Cancelling the pending acquisition loses its queue position without
    /// acquiring the lock.
    #[deprecated(note = "waits forever for a stuck holder; use `lock_owned_within` (#433)")]
    #[track_caller]
    pub fn lock_owned(self: Arc<Self>) -> impl Future<Output = OwnedMutexGuard<T>> {
        let at = Location::caller();
        async move {
            let inner = Arc::clone(&self.inner).lock_owned().await;
            self.owned(inner, at)
        }
    }

    /// Acquire a borrowed guard, waiting at most `timeout`. An expired wait
    /// leaves the queue without acquiring the lock.
    ///
    /// # Errors
    ///
    /// Returns [`super::LockTimedOut`] if the lock was not acquired in time.
    /// Requires a runtime with its timer enabled.
    #[track_caller]
    pub fn lock_within(
        &self,
        timeout: Duration,
    ) -> impl Future<Output = Result<MutexGuard<'_, T>, super::LockTimedOut>> + '_ {
        let at = Location::caller();
        async move {
            let inner = super::timeout(timeout, self.inner.lock())
                .await
                .map_err(|_| super::LockTimedOut)?;
            Ok(self.borrowed(inner, at))
        }
    }

    /// Acquire an owned guard, waiting at most `timeout`.
    ///
    /// # Errors
    ///
    /// As [`Self::lock_within`].
    #[track_caller]
    pub fn lock_owned_within(
        self: Arc<Self>,
        timeout: Duration,
    ) -> impl Future<Output = Result<OwnedMutexGuard<T>, super::LockTimedOut>> {
        let at = Location::caller();
        async move {
            let inner = super::timeout(timeout, Arc::clone(&self.inner).lock_owned())
                .await
                .map_err(|_| super::LockTimedOut)?;
            Ok(self.owned(inner, at))
        }
    }

    /// Acquire a borrowed guard without waiting.
    ///
    /// This never bypasses an already queued waiter.
    ///
    /// # Errors
    ///
    /// Returns [`MutexTryLockError`] when the lock is held or contended.
    #[track_caller]
    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, MutexTryLockError> {
        let inner = self.inner.try_lock().map_err(|_| MutexTryLockError)?;
        Ok(self.borrowed(inner, Location::caller()))
    }

    /// Acquire an owned guard without waiting.
    ///
    /// This never bypasses an already queued waiter. On success the guard
    /// keeps this `Arc<Mutex<T>>` alive, exactly as [`Self::lock_owned`] does.
    ///
    /// # Errors
    ///
    /// Returns [`MutexTryLockError`] when the lock is held or contended.
    #[track_caller]
    pub fn try_lock_owned(self: Arc<Self>) -> Result<OwnedMutexGuard<T>, MutexTryLockError> {
        let inner = Arc::clone(&self.inner)
            .try_lock_owned()
            .map_err(|_| MutexTryLockError)?;
        Ok(self.owned(inner, Location::caller()))
    }

    /// Block this thread until a borrowed guard is available.
    ///
    /// # Panics
    ///
    /// Panics when called from inside an asynchronous execution context. Use
    /// it only from a synchronous thread or the runtime's blocking-work lane
    /// ([`super::launch_blocking`]).
    #[deprecated(note = "waits forever for a stuck holder; use `blocking_lock_within` (#433)")]
    #[track_caller]
    pub fn blocking_lock(&self) -> MutexGuard<'_, T> {
        self.borrowed(self.inner.blocking_lock(), Location::caller())
    }

    /// Block this thread until a borrowed guard is available or `timeout`
    /// elapses.
    ///
    /// # Errors
    ///
    /// Returns [`super::LockTimedOut`] if the guard is not available within
    /// `timeout`; no guard or queue position is kept.
    ///
    /// # Panics
    ///
    /// Panics when called from inside an asynchronous execution context, like
    /// the unbounded form.
    #[track_caller]
    pub fn blocking_lock_within(
        &self,
        timeout: Duration,
    ) -> Result<MutexGuard<'_, T>, super::LockTimedOut> {
        assert!(
            super::RuntimeHandle::current().is_err(),
            "a bounded blocking lock must not park an asynchronous execution context"
        );
        let inner = super::park_within(self.inner.lock(), timeout).ok_or(super::LockTimedOut)?;
        Ok(self.borrowed(inner, Location::caller()))
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> std::fmt::Debug for Mutex<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Mutex").finish_non_exhaustive()
    }
}

/// Exclusive lease returned by [`Mutex::lock`], [`Mutex::try_lock`], and
/// [`Mutex::blocking_lock`]. Dropping it releases the lock.
pub struct MutexGuard<'a, T> {
    inner: tokio::sync::MutexGuard<'a, T>,
    holder: &'a HolderSlot,
}

// `drop` runs before the fields drop, so the record is cleared while the
// lock is still held and never erases the next holder's.
impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.holder.clear();
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

/// Owned exclusive lease returned by [`Mutex::lock_owned`] and
/// [`Mutex::try_lock_owned`]. Dropping it releases the lock.
///
/// The guard holds a strong reference to the `Arc<Mutex<T>>` it was acquired
/// through, so that facade handle stays alive -- and every `Weak` to it stays
/// upgradable -- while the lock is held.
pub struct OwnedMutexGuard<T> {
    // Declared first so it drops first: the lock is released before the
    // facade handle it was acquired through.
    inner: tokio::sync::OwnedMutexGuard<T>,
    _owner: Arc<Mutex<T>>,
}

impl<T> Drop for OwnedMutexGuard<T> {
    fn drop(&mut self) {
        self._owner.holder.clear();
    }
}

impl<T> Deref for OwnedMutexGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for OwnedMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, formatter)
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for OwnedMutexGuard<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, formatter)
    }
}

/// A non-waiting lock attempt found the mutex held or contended.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("mutex is busy")]
pub struct MutexTryLockError;
