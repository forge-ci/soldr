//! Facade-owned asynchronous reader/writer locking.
//!
//! The lock is fair and write-preferring: once a writer queues, later readers
//! wait behind it, so a steady stream of readers cannot starve an exclusive
//! section. Every guard is a facade-owned newtype; no backend guard type
//! crosses the facade.

use std::ops::{Deref, DerefMut};
use std::sync::Arc;

/// An asynchronous reader/writer lock with FIFO writer preference.
///
/// Borrowed guards ([`Self::read`], [`Self::write`]) tie the lease to a
/// borrow of the lock. Owned guards ([`Self::read_owned`],
/// [`Self::write_owned`]) take an `Arc<RwLock<T>>` and may be moved into
/// launched work or stored alongside the value they protect.
pub struct RwLock<T> {
    inner: Arc<tokio::sync::RwLock<T>>,
}

impl<T> RwLock<T> {
    /// Create an unlocked value.
    pub fn new(value: T) -> Self {
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(value)),
        }
    }

    /// Acquire a borrowed shared guard, queuing behind earlier writers.
    ///
    /// Cancelling the pending acquisition removes its waiter.
    #[deprecated(note = "waits forever for a stuck holder; use `read_within` (#433)")]
    pub async fn read(&self) -> RwLockReadGuard<'_, T> {
        RwLockReadGuard {
            inner: self.inner.read().await,
        }
    }

    /// Acquire a borrowed exclusive guard. Readers arriving after this writer
    /// queues wait behind it. Cancelling the pending acquisition loses its
    /// queue position.
    #[deprecated(note = "waits forever for a stuck holder; use `write_within` (#433)")]
    pub async fn write(&self) -> RwLockWriteGuard<'_, T> {
        RwLockWriteGuard {
            inner: self.inner.write().await,
        }
    }

    /// Acquire a shared borrowed guard, waiting at most `timeout`. An expired
    /// wait leaves the queue without acquiring the lock.
    ///
    /// # Errors
    ///
    /// Returns [`super::LockTimedOut`] if the lock was not acquired in time.
    /// Requires a runtime with its timer enabled.
    pub async fn read_within(
        &self,
        timeout: std::time::Duration,
    ) -> Result<RwLockReadGuard<'_, T>, super::LockTimedOut> {
        // Bounds the deprecated acquisition; inline it when that is removed (#433).
        #[allow(deprecated)]
        let acquire = self.read();
        super::timeout(timeout, acquire)
            .await
            .map_err(|_| super::LockTimedOut)
    }

    /// Acquire an exclusive borrowed guard, waiting at most `timeout`.
    ///
    /// # Errors
    ///
    /// As [`Self::read_within`].
    pub async fn write_within(
        &self,
        timeout: std::time::Duration,
    ) -> Result<RwLockWriteGuard<'_, T>, super::LockTimedOut> {
        // Bounds the deprecated acquisition; inline it when that is removed (#433).
        #[allow(deprecated)]
        let acquire = self.write();
        super::timeout(timeout, acquire)
            .await
            .map_err(|_| super::LockTimedOut)
    }

    /// Acquire an owned shared guard that keeps the lock alive.
    ///
    /// Cancelling the pending acquisition removes its waiter.
    #[deprecated(note = "waits forever for a stuck holder; use `read_owned_within` (#433)")]
    pub async fn read_owned(self: Arc<Self>) -> OwnedRwLockReadGuard<T> {
        OwnedRwLockReadGuard {
            inner: Arc::clone(&self.inner).read_owned().await,
        }
    }

    /// Acquire an owned exclusive guard that keeps the lock alive. Readers
    /// arriving after this writer queues wait behind it.
    #[deprecated(note = "waits forever for a stuck holder; use `write_owned_within` (#433)")]
    pub async fn write_owned(self: Arc<Self>) -> OwnedRwLockWriteGuard<T> {
        OwnedRwLockWriteGuard {
            inner: Arc::clone(&self.inner).write_owned().await,
        }
    }

    /// Acquire an owned shared guard, waiting at most `timeout`.
    ///
    /// # Errors
    ///
    /// As [`Self::read_within`].
    pub async fn read_owned_within(
        self: Arc<Self>,
        timeout: std::time::Duration,
    ) -> Result<OwnedRwLockReadGuard<T>, super::LockTimedOut> {
        // Bounds the deprecated acquisition; inline it when that is removed (#433).
        #[allow(deprecated)]
        let acquire = self.read_owned();
        super::timeout(timeout, acquire)
            .await
            .map_err(|_| super::LockTimedOut)
    }

    /// Acquire an owned exclusive guard, waiting at most `timeout`.
    ///
    /// # Errors
    ///
    /// As [`Self::read_within`].
    pub async fn write_owned_within(
        self: Arc<Self>,
        timeout: std::time::Duration,
    ) -> Result<OwnedRwLockWriteGuard<T>, super::LockTimedOut> {
        // Bounds the deprecated acquisition; inline it when that is removed (#433).
        #[allow(deprecated)]
        let acquire = self.write_owned();
        super::timeout(timeout, acquire)
            .await
            .map_err(|_| super::LockTimedOut)
    }

    /// Block this thread until a shared guard is available.
    ///
    /// # Panics
    ///
    /// Panics when called from inside an asynchronous execution context. Use
    /// it only from a synchronous thread or the runtime's blocking-work lane
    /// ([`super::launch_blocking`]).
    #[deprecated(note = "waits forever for a stuck holder; use `blocking_read_within` (#433)")]
    pub fn blocking_read(&self) -> RwLockReadGuard<'_, T> {
        RwLockReadGuard {
            inner: self.inner.blocking_read(),
        }
    }

    /// Block this thread until a shared guard is available or
    /// `timeout` elapses.
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
    pub fn blocking_read_within(
        &self,
        timeout: std::time::Duration,
    ) -> Result<RwLockReadGuard<'_, T>, super::LockTimedOut> {
        assert!(
            super::RuntimeHandle::current().is_err(),
            "a bounded blocking lock must not park an asynchronous execution context"
        );
        super::park_within(self.inner.read(), timeout)
            .map(|inner| RwLockReadGuard { inner })
            .ok_or(super::LockTimedOut)
    }

    /// Block this thread until an exclusive guard is available.
    ///
    /// # Panics
    ///
    /// Panics when called from inside an asynchronous execution context. Use
    /// it only from a synchronous thread or the runtime's blocking-work lane
    /// ([`super::launch_blocking`]).
    #[deprecated(note = "waits forever for a stuck holder; use `blocking_write_within` (#433)")]
    pub fn blocking_write(&self) -> RwLockWriteGuard<'_, T> {
        RwLockWriteGuard {
            inner: self.inner.blocking_write(),
        }
    }

    /// Block this thread until an exclusive guard is available or
    /// `timeout` elapses.
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
    pub fn blocking_write_within(
        &self,
        timeout: std::time::Duration,
    ) -> Result<RwLockWriteGuard<'_, T>, super::LockTimedOut> {
        assert!(
            super::RuntimeHandle::current().is_err(),
            "a bounded blocking lock must not park an asynchronous execution context"
        );
        super::park_within(self.inner.write(), timeout)
            .map(|inner| RwLockWriteGuard { inner })
            .ok_or(super::LockTimedOut)
    }

    /// Attempt an owned shared guard without waiting.
    ///
    /// This never bypasses an already queued writer: while a writer holds or
    /// waits for the lock the attempt fails.
    ///
    /// # Errors
    ///
    /// Returns [`RwLockTryLockError`] when the guard is not immediately
    /// available.
    pub fn try_read_owned(self: Arc<Self>) -> Result<OwnedRwLockReadGuard<T>, RwLockTryLockError> {
        Arc::clone(&self.inner)
            .try_read_owned()
            .map(|inner| OwnedRwLockReadGuard { inner })
            .map_err(|_| RwLockTryLockError)
    }

    /// Consume the lock and return the protected value.
    ///
    /// Returns `None` when an owned guard is still alive, because that guard
    /// keeps the value reachable.
    pub fn into_inner(self) -> Option<T> {
        Arc::into_inner(self.inner).map(tokio::sync::RwLock::into_inner)
    }
}

impl<T: Default> Default for RwLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> std::fmt::Debug for RwLock<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("RwLock").finish_non_exhaustive()
    }
}

/// Shared lease returned by [`RwLock::read`] and [`RwLock::blocking_read`].
pub struct RwLockReadGuard<'a, T> {
    inner: tokio::sync::RwLockReadGuard<'a, T>,
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

/// Exclusive lease returned by [`RwLock::write`] and [`RwLock::blocking_write`].
pub struct RwLockWriteGuard<'a, T> {
    inner: tokio::sync::RwLockWriteGuard<'a, T>,
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

/// Owned shared lease returned by [`RwLock::read_owned`].
pub struct OwnedRwLockReadGuard<T> {
    inner: tokio::sync::OwnedRwLockReadGuard<T>,
}

impl<T> Deref for OwnedRwLockReadGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

/// Owned exclusive lease returned by [`RwLock::write_owned`].
pub struct OwnedRwLockWriteGuard<T> {
    inner: tokio::sync::OwnedRwLockWriteGuard<T>,
}

impl<T> Deref for OwnedRwLockWriteGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for OwnedRwLockWriteGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

macro_rules! debug_guard {
    ($($guard:ident$(<$lifetime:lifetime>)?),+) => {$(
        impl<T: std::fmt::Debug> std::fmt::Debug for $guard<$($lifetime,)? T> {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Debug::fmt(&**self, formatter)
            }
        }
    )+};
}

debug_guard!(
    RwLockReadGuard<'_>,
    RwLockWriteGuard<'_>,
    OwnedRwLockReadGuard,
    OwnedRwLockWriteGuard
);

/// A non-waiting lock attempt found the lock busy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("rwlock is busy")]
pub struct RwLockTryLockError;
