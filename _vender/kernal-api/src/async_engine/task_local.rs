//! Scoped task-local values without exposing the runtime's task-local API.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A statically declared value installed only while its scoped future polls.
///
/// `T` need only be `Send` when the scoped future crosses threads; it never
/// needs `Sync`. Bindings are deliberately not inherited by spawned work.
pub struct TaskLocal<T: 'static> {
    key: &'static std::thread::LocalKey<RefCell<Option<T>>>,
}

impl<T: 'static> TaskLocal<T> {
    #[doc(hidden)]
    pub const fn new(key: &'static std::thread::LocalKey<RefCell<Option<T>>>) -> Self {
        Self { key }
    }

    /// Scope `value` around every poll and destruction of `future`.
    ///
    /// During destruction, if this key is already borrowed by a `with`
    /// callback or its thread-local storage is unavailable, the future is
    /// dropped without installing this scope. This avoids a second panic
    /// during cleanup; any accessible outer binding remains unchanged.
    pub fn scope<F: Future>(&'static self, value: T, future: F) -> TaskLocalScope<T, F> {
        TaskLocalScope {
            local: self,
            value: Some(value),
            future: Some(Box::pin(future)),
        }
    }

    /// Access the current binding, if this task is inside [`TaskLocal::scope`].
    /// Returns `Unbound` when the thread-local storage is being destroyed or
    /// has already been destroyed. Panics raised by `access` still propagate.
    pub fn try_with<R>(
        &'static self,
        access: impl FnOnce(&T) -> R,
    ) -> Result<R, TaskLocalAccessError> {
        self.key
            .try_with(|slot| {
                let borrowed = slot
                    .try_borrow()
                    .map_err(|_| TaskLocalAccessError::Borrowed)?;
                borrowed
                    .as_ref()
                    .map(access)
                    .ok_or(TaskLocalAccessError::Unbound)
            })
            .unwrap_or(Err(TaskLocalAccessError::Unbound))
    }

    /// Access the current binding or panic when no scope installed it.
    pub fn with<R>(&'static self, access: impl FnOnce(&T) -> R) -> R {
        self.try_with(access)
            .expect("task-local value is not bound in this task")
    }
}

/// Why a task-local binding could not be read.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TaskLocalAccessError {
    /// No enclosing [`TaskLocal::scope`] installed a value, or the backing
    /// thread-local storage is being (or has been) destroyed.
    #[error("task-local value is unbound")]
    Unbound,
    /// The binding is currently mutably borrowed by an active scope poll.
    #[error("task-local value is already mutably borrowed")]
    Borrowed,
}

/// Future returned by [`TaskLocal::scope`].
pub struct TaskLocalScope<T: 'static, F: Future> {
    local: &'static TaskLocal<T>,
    value: Option<T>,
    future: Option<Pin<Box<F>>>,
}

// Only the boxed future is structurally pinned, and moving its Pin<Box<F>>
// does not move F. The scoped value is deliberately moved into TLS for each
// poll, so this type never offers a pinning guarantee for T.
impl<T: 'static, F: Future> Unpin for TaskLocalScope<T, F> {}

impl<T: 'static, F: Future> Future for TaskLocalScope<T, F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();
        this.local.key.with(|slot| {
            // Borrow before taking our value: a recursive/misbehaving poll
            // must panic with the scope still intact, not consume it first.
            let outer = {
                let mut borrowed = slot
                    .try_borrow_mut()
                    .expect("task-local slot is already borrowed");
                std::mem::replace(&mut *borrowed, this.value.take())
            };
            struct Restore<'a, T> {
                slot: &'a RefCell<Option<T>>,
                scope: &'a mut Option<T>,
                outer: Option<T>,
            }
            impl<T> Drop for Restore<'_, T> {
                fn drop(&mut self) {
                    *self.scope = self.slot.replace(self.outer.take());
                }
            }
            let restore = Restore {
                slot,
                scope: &mut this.value,
                outer,
            };
            let result = this
                .future
                .as_mut()
                .expect("polled after completion")
                .as_mut()
                .poll(cx);
            drop(restore);
            result
        })
    }
}

impl<T: 'static, F: Future> Drop for TaskLocalScope<T, F> {
    fn drop(&mut self) {
        let Some(future) = self.future.take() else {
            return;
        };
        let _ = self.local.key.try_with(|slot| {
            // During TLS teardown or an active recursive borrow, dropping the
            // future outside the binding is safer than double-panic abort.
            let outer = {
                let Ok(mut borrowed) = slot.try_borrow_mut() else {
                    drop(future);
                    return;
                };
                std::mem::replace(&mut *borrowed, self.value.take())
            };
            struct Restore<'a, T> {
                slot: &'a RefCell<Option<T>>,
                scope: &'a mut Option<T>,
                outer: Option<T>,
            }
            impl<T> Drop for Restore<'_, T> {
                fn drop(&mut self) {
                    *self.scope = self.slot.replace(self.outer.take());
                }
            }
            let restore = Restore {
                slot,
                scope: &mut self.value,
                outer,
            };
            drop(future);
            drop(restore);
        });
    }
}

/// Declare a static [`TaskLocal`] backed only by `std::thread_local!`.
///
/// The explicit storage name avoids identifier concatenation and keeps the
/// generated TLS private to the declaring module.
#[macro_export]
macro_rules! task_local {
    ($(#[$meta:meta])* $vis:vis static $name:ident : $ty:ty = $storage:ident;) => {
        ::std::thread_local! { static $storage: ::std::cell::RefCell<::std::option::Option<$ty>> = const { ::std::cell::RefCell::new(::std::option::Option::None) }; }
        $(#[$meta])* $vis static $name: $crate::async_engine::TaskLocal<$ty> =
            $crate::async_engine::TaskLocal::new(&$storage);
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    std::thread_local! { static NUMBER_SLOT: RefCell<Option<u32>> = const { RefCell::new(None) }; }
    std::thread_local! { static OTHER_SLOT: RefCell<Option<u32>> = const { RefCell::new(None) }; }
    static NUMBER: TaskLocal<u32> = TaskLocal::new(&NUMBER_SLOT);
    static OTHER: TaskLocal<u32> = TaskLocal::new(&OTHER_SLOT);

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        let waker = std::task::Waker::noop();
        future.poll(&mut Context::from_waker(waker))
    }

    #[test]
    fn nested_same_and_different_keys_restore_outer_bindings() {
        let mut outer = NUMBER.scope(
            1,
            std::future::poll_fn(|_| {
                assert_eq!(NUMBER.with(|value| *value), 1);
                let mut same = NUMBER.scope(2, std::future::ready(()));
                assert!(matches!(poll_once(Pin::new(&mut same)), Poll::Ready(())));
                assert_eq!(NUMBER.with(|value| *value), 1);
                let mut other = OTHER.scope(3, std::future::ready(()));
                assert!(matches!(poll_once(Pin::new(&mut other)), Poll::Ready(())));
                assert_eq!(
                    OTHER.try_with(|value| *value),
                    Err(TaskLocalAccessError::Unbound)
                );
                Poll::Ready(())
            }),
        );
        assert!(matches!(poll_once(Pin::new(&mut outer)), Poll::Ready(())));
        assert_eq!(
            NUMBER.try_with(|value| *value),
            Err(TaskLocalAccessError::Unbound)
        );
    }

    #[test]
    fn pending_polls_and_cancellation_do_not_leak_bindings() {
        let mut scope = NUMBER.scope(
            7,
            std::future::poll_fn(|_| {
                assert_eq!(NUMBER.with(|value| *value), 7);
                Poll::<()>::Pending
            }),
        );
        assert!(matches!(poll_once(Pin::new(&mut scope)), Poll::Pending));
        assert_eq!(
            NUMBER.try_with(|value| *value),
            Err(TaskLocalAccessError::Unbound)
        );
        drop(scope);
        assert_eq!(
            NUMBER.try_with(|value| *value),
            Err(TaskLocalAccessError::Unbound)
        );
    }

    #[test]
    fn send_but_not_sync_values_are_accepted() {
        fn assert_send<T: Send>() {}
        assert_send::<TaskLocalScope<Cell<u32>, std::future::Ready<()>>>();
        let mut scope = CELL.scope(
            Cell::new(1),
            std::future::poll_fn(|_| {
                CELL.with(|cell| cell.set(2));
                Poll::Ready(())
            }),
        );
        assert!(matches!(poll_once(Pin::new(&mut scope)), Poll::Ready(())));
    }

    std::thread_local! { static CELL_SLOT: RefCell<Option<Cell<u32>>> = const { RefCell::new(None) }; }
    static CELL: TaskLocal<Cell<u32>> = TaskLocal::new(&CELL_SLOT);

    struct TeardownProbe(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for TeardownProbe {
        fn drop(&mut self) {
            // Catch the old LocalKey::with panic so a regression is reported
            // as an assertion rather than aborting in a TLS destructor.
            let result = std::panic::catch_unwind(|| TEARDOWN.try_with(|_| ()));
            let observed = match result {
                Ok(Err(TaskLocalAccessError::Unbound)) => 1,
                Ok(_) => 2,
                Err(_) => 3,
            };
            self.0.store(observed, std::sync::atomic::Ordering::SeqCst);
        }
    }

    task_local! {
        static TEARDOWN: TeardownProbe = TEARDOWN_SLOT;
    }

    #[test]
    fn fallible_access_during_tls_destruction_returns_unbound() {
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let thread_observed = observed.clone();
        std::thread::spawn(move || {
            TEARDOWN_SLOT.with(|slot| {
                *slot.borrow_mut() = Some(TeardownProbe(thread_observed));
            });
        })
        .join()
        .expect("thread teardown completes");
        assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    task_local! {
        static MIGRATING: Cell<u32> = MIGRATING_SLOT;
    }

    #[test]
    fn binding_moves_with_future_between_threads() {
        let mut scope = MIGRATING.scope(
            Cell::new(10),
            std::future::poll_fn(|_| {
                MIGRATING.with(|value| value.set(value.get() + 1));
                Poll::<()>::Pending
            }),
        );
        assert!(poll_once(Pin::new(&mut scope)).is_pending());
        assert_eq!(
            MIGRATING.try_with(Cell::get),
            Err(TaskLocalAccessError::Unbound)
        );
        let mut scope = std::thread::spawn(move || {
            assert_eq!(
                MIGRATING.try_with(Cell::get),
                Err(TaskLocalAccessError::Unbound)
            );
            assert!(poll_once(Pin::new(&mut scope)).is_pending());
            assert_eq!(scope.value.as_ref().unwrap().get(), 12);
            assert_eq!(
                MIGRATING.try_with(Cell::get),
                Err(TaskLocalAccessError::Unbound)
            );
            scope
        })
        .join()
        .unwrap();
        assert!(poll_once(Pin::new(&mut scope)).is_pending());
        assert_eq!(scope.value.as_ref().unwrap().get(), 13);
        assert_eq!(
            MIGRATING.try_with(Cell::get),
            Err(TaskLocalAccessError::Unbound)
        );
    }

    #[test]
    fn interleaved_scopes_keep_independent_mutable_values() {
        let future = || {
            std::future::poll_fn(|_| {
                MIGRATING.with(|value| value.set(value.get() + 1));
                Poll::<()>::Pending
            })
        };
        let mut first = MIGRATING.scope(Cell::new(0), future());
        let mut second = MIGRATING.scope(Cell::new(100), future());
        for _ in 0..3 {
            assert!(poll_once(Pin::new(&mut first)).is_pending());
            assert!(poll_once(Pin::new(&mut second)).is_pending());
            assert_eq!(
                MIGRATING.try_with(Cell::get),
                Err(TaskLocalAccessError::Unbound)
            );
        }
        assert_eq!(first.value.as_ref().unwrap().get(), 3);
        assert_eq!(second.value.as_ref().unwrap().get(), 103);
    }

    #[test]
    fn panicking_future_destructor_restores_outer_binding() {
        struct PanicOnDrop;
        impl Future for PanicOnDrop {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
                Poll::Pending
            }
        }
        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                assert_eq!(NUMBER.with(|value| *value), 22);
                panic!("destructor panic");
            }
        }
        let mut outer = NUMBER.scope(
            11,
            std::future::poll_fn(|_| {
                let inner = NUMBER.scope(22, PanicOnDrop);
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(inner))).is_err()
                );
                assert_eq!(NUMBER.with(|value| *value), 11);
                Poll::Ready(())
            }),
        );
        assert!(poll_once(Pin::new(&mut outer)).is_ready());
        assert_eq!(
            NUMBER.try_with(|value| *value),
            Err(TaskLocalAccessError::Unbound)
        );
    }

    #[test]
    fn panic_poll_restores_binding() {
        let mut scope = NUMBER.scope(
            9,
            std::future::poll_fn(|_| -> Poll<()> { panic!("poll panic") }),
        );
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            poll_once(Pin::new(&mut scope))
        }));
        assert_eq!(
            NUMBER.try_with(|value| *value),
            Err(TaskLocalAccessError::Unbound)
        );
    }

    #[test]
    fn cancelled_future_destructor_observes_its_binding() {
        struct ObserveDrop(std::sync::Arc<std::sync::atomic::AtomicU32>);
        impl Future for ObserveDrop {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
                Poll::Pending
            }
        }
        impl Drop for ObserveDrop {
            fn drop(&mut self) {
                self.0.store(
                    NUMBER.with(|value| *value),
                    std::sync::atomic::Ordering::SeqCst,
                );
            }
        }
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut scope = NUMBER.scope(42, ObserveDrop(observed.clone()));
        assert!(poll_once(Pin::new(&mut scope)).is_pending());
        drop(scope);
        assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst), 42);
        assert_eq!(
            NUMBER.try_with(|value| *value),
            Err(TaskLocalAccessError::Unbound)
        );
    }

    #[test]
    fn dropping_scope_inside_a_borrowed_callback_keeps_the_outer_binding() {
        struct ReadOnDrop(std::sync::Arc<std::sync::atomic::AtomicU32>);
        impl Future for ReadOnDrop {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
                Poll::Pending
            }
        }
        impl Drop for ReadOnDrop {
            fn drop(&mut self) {
                self.0.store(
                    NUMBER.with(|value| *value),
                    std::sync::atomic::Ordering::SeqCst,
                );
            }
        }
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut outer = NUMBER.scope(
            11,
            std::future::poll_fn(|_| {
                let inner = NUMBER.scope(22, ReadOnDrop(seen.clone()));
                NUMBER.with(|value| {
                    assert_eq!(*value, 11);
                    drop(inner);
                    assert_eq!(*value, 11);
                });
                Poll::Ready(())
            }),
        );
        assert!(poll_once(Pin::new(&mut outer)).is_ready());
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 11);
        assert_eq!(
            NUMBER.try_with(|value| *value),
            Err(TaskLocalAccessError::Unbound)
        );
    }
}
