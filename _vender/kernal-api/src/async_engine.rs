//! The canonical asynchronous engine for every `kernal-api` client.
//!
//! Every public value in this module is owned by `kernal-api`. The current
//! implementation is Tokio, but no Tokio type, trait, module, or macro is
//! re-exported across the facade.
//!
//! [`crate::biased_race!`] covers ordered selection over two to four
//! borrowed/pinned futures. [`crate::fair_race!`] supports two to five branches
//! with a pseudorandom initial polling position. Both snapshot optional guards
//! and return tagged winners ([`BiasedRace2`]..[`BiasedRace4`],
//! [`FairRace2`]..[`FairRace5`]); neither implements pattern-mismatch disabling
//! or an `else` arm. Preserve those behaviors explicitly when migrating a
//! broader `select!` expression. Source-order priority is a semantic choice,
//! so biased selection must not substitute for a previously unordered race.
//! Both macros are implemented with `std` only and expand to `$crate` paths,
//! so no backend macro or type crosses the facade. Callers whose only need is
//! racing one operation against cancellation should reach for [`cancellable`]
//! instead. `join!`-shaped call sites are different: awaiting a fixed, small
//! set of futures unconditionally to completion has no macro-specific
//! behavior to preserve, so [`join`] covers that shape as an ordinary
//! function. Attribute macros (`#[tokio::test]`, `#[tokio::main]`) are the
//! same problem as `select!` one level up: fronting a proc macro from this
//! facade needs a `kernal-api-macros` companion crate, which is an open
//! decision tracked in the meta issue rather than something this module
//! resolves on its own.
//!
//! [`crate::task_local!`] declares poll-scoped bindings ([`TaskLocal`]) backed
//! only by `std::thread_local!`, without exposing runtime types. A binding is
//! installed around every poll and destruction of the scoped future and is
//! not inherited by work launched from inside the scope.
//!
//! [`RwLock`] is fair and write-preferring, with borrowed, owned, and blocking
//! guards that are facade-owned newtypes. [`TerminationSignal`] and
//! [`wait_for_interrupt`] are owned shutdown-signal subscriptions for launched
//! work that cannot borrow its [`Runtime`].

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::Notify as BackendNotify;

mod broadcast;
mod mutex;
mod race;
mod rw_lock;
mod signals;
mod task_local;
pub use broadcast::{
    broadcast_channel, BroadcastReceiver, BroadcastRecvError, BroadcastRecvTimeoutError,
    BroadcastSender, BroadcastTryRecvError,
};
#[cfg(feature = "event-stream")]
pub use broadcast::{BroadcastLagged, BroadcastStream};
pub use mutex::{LockHolder, Mutex, MutexGuard, MutexTryLockError, OwnedMutexGuard};
#[doc(hidden)]
pub use race::fair_start as __fair_race_start;
pub use race::{BiasedRace2, BiasedRace3, BiasedRace4, FairRace2, FairRace3, FairRace4, FairRace5};
pub use rw_lock::{
    OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, RwLockReadGuard, RwLockTryLockError,
    RwLockWriteGuard,
};
pub use signals::{wait_for_interrupt, TerminationSignal};
pub use task_local::{TaskLocal, TaskLocalAccessError, TaskLocalScope};

/// Current engine implementation, retained for diagnostics and bug reports.
pub const BACKEND_NAME: &str = "tokio";
/// Exact backend version selected by this `kernal-api` release.
pub const BACKEND_VERSION: &str = "1.53.1";

/// Handle to a task launched on the shared async engine.
///
/// Dropping a live `Task` cancels it. This is the "bounded resources,
/// cancellation, child cleanup" default `ARCHITECTURE.md` requires of every
/// new facade operation, and it is the concrete reason a facade-owned handle
/// earns its keep over a raw `JoinHandle`: a `JoinHandle` silently detaches on
/// drop, which is exactly how a client connection whose handler task keeps
/// running after the caller gives up on it goes on holding whatever
/// permits, files, or child processes that task owns.
///
/// A task meant to keep running once its handle goes out of scope --
/// a daemon accept loop, a fire-and-forget notification -- must say so
/// explicitly by calling [`Task::detach`]. `Task` is `#[must_use]` so the
/// compiler flags a bare `launch(..);` statement rather than letting it
/// silently cancel the task it just started; `let _ = launch(..);` still
/// drops (and therefore cancels) the task without a warning, so prefer
/// `.detach()` over `let _ =` when the intent is to run in the background.
#[derive(Debug)]
#[must_use = "dropping a Task cancels it; call `.detach()` to run it in the \
              background, or await/store the handle to join it"]
pub struct Task<T> {
    inner: tokio::task::JoinHandle<T>,
    detached: bool,
}

impl<T> Task<T> {
    fn new(inner: tokio::task::JoinHandle<T>) -> Self {
        Self {
            inner,
            detached: false,
        }
    }

    /// Request cancellation. Cancellation completes at the next yield point.
    ///
    /// For a task launched through [`launch_blocking`] or
    /// [`RuntimeHandle::launch_blocking`], cancellation (including the
    /// implicit cancellation on drop) cannot interrupt blocking work that is
    /// already running: the closure keeps running on its worker thread to
    /// completion, and only the join result is discarded.
    pub fn cancel(&self) {
        self.inner.abort();
    }

    /// Whether the task has completed.
    pub fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }

    /// Let the task keep running after this handle is dropped.
    ///
    /// Use this only for work that genuinely has no owner left to join or
    /// cancel it. Prefer keeping the handle -- storing it, awaiting it, or
    /// cancelling it explicitly -- wherever the caller can; `detach` is an
    /// explicit opt-out of this crate's default child-cleanup behavior, not
    /// the default itself.
    pub fn detach(mut self) {
        self.detached = true;
    }

    /// Keep the task running if this handle is later dropped, and return the
    /// handle.
    ///
    /// Unlike [`Task::detach`], the caller keeps the handle: it can still
    /// await the task's result, ask [`Task::is_finished`], or cancel it
    /// explicitly with [`Task::cancel`]. Only the implicit cancellation on
    /// drop is switched off. Use it where a result is wanted when available
    /// but the work must complete even if the waiter gives up -- a cache
    /// store that a cancelled request should not abandon half-written.
    pub fn detach_on_drop(mut self) -> Self {
        self.detached = true;
        self
    }
}

impl<T> Task<T> {
    /// Wait at most `timeout` for the task's result. An expired wait leaves
    /// the task running and this handle usable.
    ///
    /// # Errors
    ///
    /// Returns [`JoinTimedOut`] if the task did not finish in time. Requires a
    /// runtime with its timer enabled.
    pub async fn join_within(
        &mut self,
        timeout: Duration,
    ) -> Result<Result<T, TaskError>, JoinTimedOut> {
        self::timeout(timeout, &mut *self)
            .await
            .map_err(|_| JoinTimedOut)
    }
}

/// A bounded [`Task`], [`TaskGroup`] or [`join_within`] join expired before a result arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no kernal-api task result arrived before the timeout")]
pub struct JoinTimedOut;

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        if !self.detached {
            self.inner.abort();
        }
    }
}

impl<T> Future for Task<T> {
    type Output = Result<T, TaskError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.inner)
            .poll(context)
            .map(|result| result.map_err(TaskError::from_backend))
    }
}

/// Failure returned while joining an async task.
#[derive(Debug, thiserror::Error)]
#[error("{inner}")]
pub struct TaskError {
    inner: tokio::task::JoinError,
}

impl TaskError {
    fn from_backend(inner: tokio::task::JoinError) -> Self {
        Self { inner }
    }

    /// Whether cancellation ended the task.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Whether a panic ended the task.
    pub fn is_panic(&self) -> bool {
        self.inner.is_panic()
    }
}

/// Owned asynchronous runtime.
pub struct Runtime {
    inner: tokio::runtime::Runtime,
    drivers_enabled: bool,
}

impl Runtime {
    /// Register for native Ctrl+C notifications before returning.
    ///
    /// Requires a runtime built with `enable_all`; otherwise returns
    /// `Unsupported` without installing a signal handler. Registration failures
    /// propagate. The listener borrows this runtime and uses its existing driver.
    ///
    /// Registration changes process-wide handling: dropping listeners does not
    /// restore the default handler. Applications must own graceful exit policy.
    pub fn interrupt_signal(&self) -> std::io::Result<InterruptSignal<'_>> {
        if !self.drivers_enabled {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "interrupt notifications require enabled runtime drivers",
            ));
        }
        let _entered = self.inner.enter();
        Ok(InterruptSignal {
            inner: crate::platform_imp::interrupt::InterruptReceiver::new()?,
            _runtime: self,
        })
    }

    /// Register and wait for one Ctrl+C notification when first polled.
    ///
    /// For registration before polling, use [`Self::interrupt_signal`] instead.
    /// The same driver requirements and process-wide handler effects apply.
    pub async fn wait_for_interrupt(&self) -> std::io::Result<()> {
        self.interrupt_signal()?.wait().await
    }

    /// Run one future to completion on this runtime.
    #[deprecated(note = "blocks forever on a stuck future; use `run_within` (#433)")]
    pub fn run<F: Future>(&self, future: F) -> F::Output {
        self.inner.block_on(future)
    }

    /// [`Self::run`] with a deadline: block until `future` completes or
    /// `timeout` elapses, returning [`DeadlineElapsed`] on expiry.
    ///
    /// On expiry `future` is dropped, cancelling whatever it had not
    /// finished. Tasks it launched keep running on this runtime. Requires a
    /// runtime built with [`RuntimeBuilder::enable_all`]; without the timer
    /// driver the deadline cannot be armed and this panics.
    pub fn run_within<F: Future>(
        &self,
        future: F,
        timeout: Duration,
    ) -> Result<F::Output, DeadlineElapsed> {
        self.inner.block_on(self::timeout(timeout, future))
    }

    /// Obtain a clonable handle for this runtime.
    pub fn handle(&self) -> RuntimeHandle {
        RuntimeHandle {
            inner: self.inner.handle().clone(),
        }
    }
}

/// Builder for an owned asynchronous runtime.
pub struct RuntimeBuilder {
    inner: tokio::runtime::Builder,
    drivers_enabled: bool,
}

impl RuntimeBuilder {
    /// Build a runtime that executes tasks on the calling thread.
    pub fn current_thread() -> Self {
        Self {
            inner: tokio::runtime::Builder::new_current_thread(),
            drivers_enabled: false,
        }
    }

    /// Build a runtime backed by a worker pool.
    pub fn multi_thread() -> Self {
        Self {
            inner: tokio::runtime::Builder::new_multi_thread(),
            drivers_enabled: false,
        }
    }

    /// Enable the engine's I/O, time, and signal drivers.
    pub fn enable_all(mut self) -> Self {
        self.inner.enable_all();
        self.drivers_enabled = true;
        self
    }

    /// Set the number of async worker threads.
    pub fn worker_threads(mut self, count: usize) -> Self {
        self.inner.worker_threads(count);
        self
    }

    /// Cap the threads the blocking-work lane ([`launch_blocking`]) may use.
    ///
    /// The cap excludes the async worker threads. Blocking work submitted
    /// beyond it queues until a lane thread frees up.
    ///
    /// # Panics
    ///
    /// Panics if `count` is zero.
    pub fn max_blocking_threads(mut self, count: usize) -> Self {
        assert!(count > 0, "max_blocking_threads must be greater than zero");
        self.inner.max_blocking_threads(count);
        self
    }

    /// Set the runtime worker thread name.
    pub fn thread_name(mut self, name: impl Into<String>) -> Self {
        self.inner.thread_name(name.into());
        self
    }

    /// Create the configured runtime.
    pub fn build(mut self) -> std::io::Result<Runtime> {
        self.inner.build().map(|inner| Runtime {
            inner,
            drivers_enabled: self.drivers_enabled,
        })
    }
}

/// Owned subscription to native Ctrl+C notifications.
///
/// Created by [`Runtime::interrupt_signal`], not an ambient or second runtime.
/// Unix observes SIGINT; Windows observes console CTRL_C, not CTRL_BREAK,
/// close, logoff or shutdown. Notifications may coalesce and every registered
/// listener is notified; this is not an exact interrupt counter.
///
/// The borrowed runtime must be driven to deliver notifications. No timeout is
/// imposed on an intentional wait for user input; callers may use [`timeout`]
/// or cancel the wait. Dropping this subscription does not restore process-wide
/// signal handlers. It does not cancel work or select an exit code.
///
/// A listener cannot outlive the runtime that drives it:
///
/// ```compile_fail
/// let runtime = kernal_api::async_engine::RuntimeBuilder::current_thread()
///     .enable_all().build().unwrap();
/// let listener = runtime.interrupt_signal().unwrap();
/// drop(runtime);
/// drop(listener);
/// ```
pub struct InterruptSignal<'runtime> {
    inner: crate::platform_imp::interrupt::InterruptReceiver,
    _runtime: &'runtime Runtime,
}

impl InterruptSignal<'_> {
    /// Wait for one notification, or report a closed native receiver.
    ///
    /// Cancellation-safe: dropping a pending wait does not consume a later
    /// notification. A subsequent wait can observe it. Signals may coalesce.
    pub async fn wait(&mut self) -> std::io::Result<()> {
        self.inner.recv().await.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "interrupt receiver closed")
        })
    }
}

/// Clonable handle to a running asynchronous runtime.
#[derive(Clone, Debug)]
pub struct RuntimeHandle {
    inner: tokio::runtime::Handle,
}

impl PartialEq for RuntimeHandle {
    fn eq(&self, other: &Self) -> bool {
        // Tokio assigns this identifier to the live runtime, rather than to a
        // particular Handle wrapper. This fixes `current()` and cloned owned
        // handles comparing as distinct runtimes.
        self.inner.id() == other.inner.id()
    }
}

impl Eq for RuntimeHandle {}

impl RuntimeHandle {
    /// Obtain the current runtime handle.
    pub fn current() -> Result<Self, NoRuntime> {
        tokio::runtime::Handle::try_current()
            .map(|inner| Self { inner })
            .map_err(|_| NoRuntime)
    }

    /// Launch a task on this runtime.
    pub fn launch<F>(&self, future: F) -> Task<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        Task::new(self.inner.spawn(future))
    }
    /// Launch blocking work on this handle's blocking lane.
    ///
    /// Unlike the ambient helper, this preserves caller-selected runtime
    /// ownership for facilities (such as the sketch epoch clock) that need a
    /// single executor identity.
    pub fn launch_blocking<F, R>(&self, operation: F) -> Task<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        Task::new(self.inner.spawn_blocking(operation))
    }
    #[cfg(feature = "wasm-sketch-host")]
    pub(crate) fn block_on_wasm<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        // Guest child instances are owned by native threads.  They must drive
        // Wasmtime's async API through the runtime selected by the caller,
        // rather than constructing a nested Tokio runtime.  This is called
        // only from those owner threads, never from a runtime worker.
        self.inner.block_on(future)
    }
    #[cfg(feature = "wasm-sketch-host")]
    pub(crate) fn same_runtime_for_wasm(&self, other: &Self) -> bool {
        // `tokio::runtime::Id` deliberately has no public numeric conversion.
        // Preserve it as opaque backend identity and compare only through
        // Tokio's equality semantics; never hash, truncate, or expose it.
        self.inner.id() == other.inner.id()
    }
}

/// No async runtime is active on this thread.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("no kernal-api async runtime is active on this thread")]
pub struct NoRuntime;

/// Launch a `Send` task on the current runtime.
pub fn launch<F>(future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    Task::new(tokio::spawn(future))
}

/// Run blocking work without occupying an async worker thread.
pub fn launch_blocking<F, R>(operation: F) -> Task<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    Task::new(tokio::task::spawn_blocking(operation))
}

/// Run two futures concurrently on the current task, returning both outputs
/// once both have completed.
///
/// This covers the common `tokio::join!` shape of unconditionally awaiting a
/// fixed, small set of futures to completion. It has no cancellation of its
/// own: if the caller is dropped while this is pending, both `first` and
/// `second` are dropped with it, exactly as an equivalent hand-written
/// `futures::join!`/`tokio::join!` invocation would behave. Callers that need
/// to race futures against each other, rather than wait for every one of
/// them, are outside this function's scope -- see the module documentation
/// for `select!`-shaped call sites.
pub async fn join<A, B>(first: A, second: B) -> (A::Output, B::Output)
where
    A: Future,
    B: Future,
{
    tokio::join!(first, second)
}

/// [`join`] with a deadline: run both futures concurrently and return both
/// outputs, or [`JoinTimedOut`] once `timeout` elapses first.
///
/// On expiry both futures are dropped, cancelling whatever work they had not
/// finished; results that one of them already produced are discarded with
/// it.
pub async fn join_within<A, B>(
    first: A,
    second: B,
    timeout: Duration,
) -> Result<(A::Output, B::Output), JoinTimedOut>
where
    A: Future,
    B: Future,
{
    self::timeout(timeout, join(first, second))
        .await
        .map_err(|_| JoinTimedOut)
}

/// A dynamically growing collection of same-output tasks, joined as each one
/// completes.
///
/// This is the facade surface for fan-out workloads that launch a task per
/// unit of work (for example, one task per compilation unit) and then drain
/// results as they arrive rather than in launch order. Dropping the group
/// cancels every task still running in it -- the same child-cleanup default
/// [`Task`] applies to a single handle, extended to a collection of them.
#[derive(Debug)]
pub struct TaskGroup<T> {
    inner: tokio::task::JoinSet<T>,
}

impl<T: 'static> TaskGroup<T> {
    /// Create an empty task group.
    pub fn new() -> Self {
        Self {
            inner: tokio::task::JoinSet::new(),
        }
    }

    /// Launch a `Send` task into this group.
    pub fn spawn<F>(&mut self, future: F)
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.inner.spawn(future);
    }

    /// Launch blocking work into this group without occupying an async
    /// worker thread.
    pub fn spawn_blocking<F>(&mut self, operation: F)
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.inner.spawn_blocking(operation);
    }

    /// Wait for the next task in the group to complete, in completion order.
    ///
    /// Returns `None` once the group is empty.
    #[deprecated(note = "can wait forever; use `join_next_within` (#433)")]
    pub async fn join_next(&mut self) -> Option<Result<T, TaskError>> {
        self.inner
            .join_next()
            .await
            .map(|result| result.map_err(TaskError::from_backend))
    }

    /// Wait at most `timeout` for the next task to complete. An expired wait
    /// cancels nothing and loses no result.
    ///
    /// # Errors
    ///
    /// Returns [`JoinTimedOut`] if no task completed in time. Requires a
    /// runtime with its timer enabled.
    pub async fn join_next_within(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<Result<T, TaskError>>, JoinTimedOut> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = self.join_next();
        self::timeout(timeout, wait).await.map_err(|_| JoinTimedOut)
    }

    /// The number of tasks still running or awaiting collection in this
    /// group.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the group currently holds no tasks.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl<T: 'static> Default for TaskGroup<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Yield execution back to the async scheduler once.
pub async fn yield_now() {
    tokio::task::yield_now().await;
}

/// Wait for a duration without blocking an async worker.
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// Wait until a shared deadline expires. Cancelling the wait does not change
/// the deadline, so retrying uses the original expiry rather than a new budget.
pub async fn sleep_until(deadline: Deadline) {
    tokio::time::sleep_until(deadline.at).await;
}

/// A fixed-cadence timer with an immediate first tick.
///
/// Missed ticks use [`MissedTickBehavior::Burst`] by default: they are
/// delivered in a burst until the original schedule catches up. No background
/// task is spawned. Drop the timer to stop observing ticks.
#[derive(Debug)]
pub struct PeriodicTimer {
    inner: tokio::time::Interval,
}

/// How a [`PeriodicTimer`] responds when ticks are missed because the
/// consumer was busy past one or more scheduled instants.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum MissedTickBehavior {
    /// Fire every missed tick immediately until the original schedule
    /// catches up.
    #[default]
    Burst,
    /// Fire one tick now and restart the cadence from this instant.
    Delay,
    /// Fire one tick now and resume on the original schedule, dropping the
    /// ticks that were missed.
    Skip,
}

impl PeriodicTimer {
    /// Create a periodic timer in the current runtime.
    ///
    /// # Errors
    ///
    /// Returns an error outside a runtime or when `period` is zero or exceeds
    /// 365 days. The runtime must have its timer driver enabled.
    pub fn new(period: Duration) -> std::io::Result<Self> {
        if period > Duration::from_secs(365 * 24 * 60 * 60) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "periodic timer period must be positive and at most 365 days",
            ));
        }
        Self::new_unbounded(period)
    }

    /// Create a timer for any nonzero period representable by the host clock.
    ///
    /// This keeps legacy persisted configuration (such as a multi-year flush
    /// interval) intact. Prefer [`Self::new`] for new policy-bound settings.
    ///
    /// # Errors
    ///
    /// Returns an error outside a runtime, when `period` is zero, or when the
    /// first deadline would overflow the host clock.
    pub fn new_unbounded(period: Duration) -> std::io::Result<Self> {
        if period.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "periodic timer period must be positive",
            ));
        }
        RuntimeHandle::current().map_err(std::io::Error::other)?;
        if tokio::time::Instant::now().checked_add(period).is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "periodic timer period exceeds the host clock range",
            ));
        }
        let mut inner = tokio::time::interval(period);
        inner.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        Ok(Self { inner })
    }

    /// Select missed-tick behavior without changing the immediate first tick.
    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.inner.set_missed_tick_behavior(match behavior {
            MissedTickBehavior::Burst => tokio::time::MissedTickBehavior::Burst,
            MissedTickBehavior::Delay => tokio::time::MissedTickBehavior::Delay,
            MissedTickBehavior::Skip => tokio::time::MissedTickBehavior::Skip,
        });
    }

    /// The missed-tick behavior currently in effect.
    pub fn missed_tick_behavior(&self) -> MissedTickBehavior {
        match self.inner.missed_tick_behavior() {
            tokio::time::MissedTickBehavior::Burst => MissedTickBehavior::Burst,
            tokio::time::MissedTickBehavior::Delay => MissedTickBehavior::Delay,
            tokio::time::MissedTickBehavior::Skip => MissedTickBehavior::Skip,
        }
    }

    /// Wait for the next scheduled tick. Cancelling a pending call does not
    /// consume its tick; a subsequent call observes the same scheduled time.
    pub async fn tick(&mut self) {
        self.inner.tick().await;
    }
}

/// An absolute deadline that can be shared across composed async operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline {
    at: tokio::time::Instant,
}

impl Deadline {
    /// Create a deadline `duration` from now.
    pub fn after(duration: Duration) -> Self {
        Self {
            at: tokio::time::Instant::now() + duration,
        }
    }

    /// Return the remaining budget, clamped to zero after expiry.
    pub fn remaining(self) -> Duration {
        self.at
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or(Duration::ZERO)
    }

    /// Whether this deadline has elapsed.
    pub fn is_elapsed(self) -> bool {
        self.remaining().is_zero()
    }
}

/// Run `future` until it completes or the deadline expires.
pub async fn timeout<F: Future>(
    duration: Duration,
    future: F,
) -> Result<F::Output, DeadlineElapsed> {
    timeout_at(Deadline::after(duration), future).await
}

/// Run `future` until an already-composed deadline expires.
pub async fn timeout_at<F: Future>(
    deadline: Deadline,
    future: F,
) -> Result<F::Output, DeadlineElapsed> {
    tokio::time::timeout_at(deadline.at, future)
        .await
        .map_err(|_| DeadlineElapsed)
}

/// A bounded [`Mutex`] or [`RwLock`] acquisition expired before the lock
/// became available.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the kernal-api lock was not acquired before the timeout")]
pub struct LockTimedOut;

/// A bounded [`Notified`] or [`OwnedNotified`] wait expired without a
/// notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no kernal-api notification arrived before the timeout")]
pub struct NotifyTimedOut;

/// The configured deadline elapsed before an operation completed.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("the kernal-api async operation exceeded its deadline")]
pub struct DeadlineElapsed;

/// Source that can request cancellation of operations sharing its token.
#[derive(Clone, Debug)]
pub struct CancellationSource {
    state: Arc<CancellationState>,
}

#[derive(Debug)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: BackendNotify,
    // This is a deterministic regression seam for the Notify registration
    // race. It is excluded from production artifacts.
    #[cfg(test)]
    cancel_after_waiter_check: AtomicBool,
}

/// Cloneable cancellation capability for one or more async operations.
///
/// The token is an operation boundary, not a task handle: cancelling it never
/// aborts unrelated work on the runtime. Operations opt in through
/// [`cancellable`] or by awaiting [`CancellationToken::cancelled`].
#[derive(Clone, Debug)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

impl CancellationSource {
    /// Create a new, initially active cancellation domain.
    pub fn new() -> Self {
        Self {
            state: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
                notify: BackendNotify::new(),
                #[cfg(test)]
                cancel_after_waiter_check: AtomicBool::new(false),
            }),
        }
    }

    /// Return a token that can observe this source's cancellation request.
    pub fn token(&self) -> CancellationToken {
        CancellationToken {
            state: Arc::clone(&self.state),
        }
    }

    /// Request cancellation for every operation using a token from this source.
    ///
    /// This is idempotent. The request is cooperative: an operation observes it
    /// at an await point or when it explicitly checks its token.
    pub fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::AcqRel) {
            self.state.notify.notify_waiters();
        }
    }

    /// Whether this source has requested cancellation.
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }
}

impl Default for CancellationSource {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    /// Wait until cancellation is requested.
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.state.notify.notified();
            tokio::pin!(notified);
            // Register before the second state observation. `notify_waiters`
            // intentionally retains no permit, so without this a cancellation
            // between constructing `Notified` and its first poll could strand
            // this waiter forever.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            #[cfg(test)]
            if self
                .state
                .cancel_after_waiter_check
                .swap(false, Ordering::AcqRel)
            {
                self.state.cancelled.store(true, Ordering::Release);
                self.state.notify.notify_waiters();
            }
            notified.await;
        }
    }
}

/// Run an operation until it completes or `token` requests cancellation.
pub async fn cancellable<F>(token: &CancellationToken, future: F) -> Result<F::Output, Cancelled>
where
    F: Future,
{
    tokio::select! {
        biased;
        () = token.cancelled() => Err(Cancelled),
        output = future => Ok(output),
    }
}

/// An operation was cancelled through a kernal-api cancellation token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the kernal-api async operation was cancelled")]
pub struct Cancelled;

/// A reusable wakeup signal shared between async operations.
///
/// Unlike [`CancellationSource`]/[`CancellationToken`], which latch once and
/// stay latched, `Notify` models a recurring "something changed, check
/// again" event: a producer calls [`Notify::notify_one`] or
/// [`Notify::notify_waiters`] each time new state is available, and any
/// number of waiters call [`Notify::notified`] to wait for the next one.
/// Construct one and share it (typically behind an `Arc`) between the
/// producer and its waiters.
///
/// A notification permit is not queued indefinitely: only a waiter already
/// registered (via a `notified()` future that has been polled at least once
/// or [enabled](Notified::enable)) observes a given
/// `notify_one`/`notify_waiters` call. Callers that
/// need every state transition observed exactly once should encode that
/// state explicitly (an atomic flag, a channel) rather than relying on
/// notification counting.
#[derive(Debug)]
pub struct Notify {
    inner: Arc<BackendNotify>,
}

impl Notify {
    /// Create a new notification signal with no permit outstanding.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(BackendNotify::new()),
        }
    }

    /// Wake exactly one waiting caller, or store a single permit for the next
    /// caller to observe if none is currently waiting.
    pub fn notify_one(&self) {
        self.inner.notify_one();
    }

    /// Wake every caller currently waiting. Callers that begin waiting after
    /// this call are not woken by it.
    pub fn notify_waiters(&self) {
        self.inner.notify_waiters();
    }

    /// Create a future that completes on the next notification.
    ///
    /// The future exists as soon as this returns, but it joins the waiter
    /// list only when first polled or explicitly [enabled](Notified::enable).
    /// Enable it before releasing whatever lock guards the state being
    /// waited for, so a notification sent in between is not lost.
    pub fn notified(&self) -> Notified<'_> {
        Notified {
            inner: self.inner.notified(),
        }
    }

    /// [`Self::notified`] for a shared `Notify`, returning a future that owns
    /// its reference and so may outlive the borrow it was created from --
    /// stored in a struct or moved into launched work.
    pub fn owned_notified(self: Arc<Self>) -> OwnedNotified {
        OwnedNotified {
            inner: Arc::clone(&self.inner).notified_owned(),
        }
    }
}

/// Future returned by [`Notify::notified`]; completes on a notification.
#[must_use = "a notification future does nothing unless enabled or awaited"]
pub struct Notified<'a> {
    // Structurally pinned: it is never moved out of a pinned `Notified`, and
    // `Notified` has no `Drop` impl and no manual `Unpin` impl.
    inner: tokio::sync::futures::Notified<'a>,
}

impl<'a> Notified<'a> {
    fn project(self: Pin<&mut Self>) -> Pin<&mut tokio::sync::futures::Notified<'a>> {
        // SAFETY: `inner` is structurally pinned (see the field comment), so
        // projecting the pin to it upholds the pinning guarantee.
        unsafe { self.map_unchecked_mut(|notified| &mut notified.inner) }
    }

    /// Join the waiter list now, without waiting.
    ///
    /// Returns `true` when a notification has already been received, in
    /// which case awaiting the future completes immediately. Calling it again
    /// after registration only reports that state.
    pub fn enable(self: Pin<&mut Self>) -> bool {
        self.project().enable()
    }

    /// Wait for the notification for at most `timeout`. A permit already
    /// stored by [`Notify::notify_one`] satisfies even a zero timeout.
    ///
    /// # Errors
    ///
    /// Returns [`NotifyTimedOut`] if no notification arrived in time.
    /// Requires a runtime with its timer enabled.
    pub async fn wait_within(self, timeout: Duration) -> Result<(), NotifyTimedOut> {
        self::timeout(timeout, self)
            .await
            .map_err(|_| NotifyTimedOut)
    }
}

impl Future for Notified<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        self.project().poll(context)
    }
}

impl std::fmt::Debug for Notified<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Notified").finish_non_exhaustive()
    }
}

/// Future returned by [`Notify::owned_notified`]; completes on a
/// notification and keeps its `Notify` alive.
#[must_use = "a notification future does nothing unless enabled or awaited"]
pub struct OwnedNotified {
    // Structurally pinned: it is never moved out of a pinned `OwnedNotified`,
    // and `OwnedNotified` has no `Drop` impl and no manual `Unpin` impl.
    inner: tokio::sync::futures::OwnedNotified,
}

impl OwnedNotified {
    fn project(self: Pin<&mut Self>) -> Pin<&mut tokio::sync::futures::OwnedNotified> {
        // SAFETY: `inner` is structurally pinned (see the field comment), so
        // projecting the pin to it upholds the pinning guarantee.
        unsafe { self.map_unchecked_mut(|notified| &mut notified.inner) }
    }

    /// Join the waiter list now, without waiting.
    ///
    /// Returns `true` when a notification has already been received, in
    /// which case awaiting the future completes immediately. Calling it again
    /// after registration only reports that state.
    pub fn enable(self: Pin<&mut Self>) -> bool {
        self.project().enable()
    }

    /// Wait for the notification for at most `timeout`. A permit already
    /// stored by [`Notify::notify_one`] satisfies even a zero timeout.
    ///
    /// # Errors
    ///
    /// Returns [`NotifyTimedOut`] if no notification arrived in time.
    /// Requires a runtime with its timer enabled.
    pub async fn wait_within(self, timeout: Duration) -> Result<(), NotifyTimedOut> {
        self::timeout(timeout, self)
            .await
            .map_err(|_| NotifyTimedOut)
    }
}

impl Future for OwnedNotified {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        self.project().poll(context)
    }
}

impl std::fmt::Debug for OwnedNotified {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OwnedNotified")
            .finish_non_exhaustive()
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

/// Run a connection-establishment operation until it completes or its
/// connection deadline expires.
///
/// This deliberately differs from [`progress_timeout`]: it is for the period
/// before a connection exists and therefore cannot report transfer progress.
pub async fn connection_timeout<F>(
    duration: Duration,
    future: F,
) -> Result<F::Output, ConnectionDeadlineElapsed>
where
    F: Future,
{
    connection_until(Deadline::after(duration), future).await
}

/// Run a connection-establishment operation within an already-composed
/// deadline.
pub async fn connection_until<F>(
    deadline: Deadline,
    future: F,
) -> Result<F::Output, ConnectionDeadlineElapsed>
where
    F: Future,
{
    tokio::time::timeout_at(deadline.at, future)
        .await
        .map_err(|_| ConnectionDeadlineElapsed)
}

/// The configured connection-establishment deadline elapsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the kernal-api connection deadline elapsed before the operation completed")]
pub struct ConnectionDeadlineElapsed;

/// Reports caller-observable progress for one transfer or streaming operation.
///
/// Clones of this reporter, and every [`ProgressWatch`] made from it, share one
/// intentionally common progress domain. Use a new reporter for independent
/// transfers so activity on one cannot extend another's idle budget.
#[derive(Clone, Debug)]
pub struct ProgressReporter {
    state: Arc<ProgressState>,
}

#[derive(Debug)]
struct ProgressState {
    last_progress: StdMutex<tokio::time::Instant>,
    notify: BackendNotify,
}

/// Observes progress reported through a paired [`ProgressReporter`].
///
/// Clones observe the same intentionally shared progress domain as the
/// reporter and its other watches.
#[derive(Clone, Debug)]
pub struct ProgressWatch {
    state: Arc<ProgressState>,
}

impl ProgressReporter {
    /// Start a transfer-progress domain. Its initial idle window starts now.
    pub fn new() -> Self {
        Self {
            state: Arc::new(ProgressState {
                last_progress: StdMutex::new(tokio::time::Instant::now()),
                notify: BackendNotify::new(),
            }),
        }
    }

    /// Obtain a watch capability for use with [`progress_timeout`].
    pub fn watch(&self) -> ProgressWatch {
        ProgressWatch {
            state: Arc::clone(&self.state),
        }
    }

    /// Start a fresh idle window for a new transfer using this progress
    /// domain. This establishes its initial budget; it is not evidence that
    /// bytes have moved, so callers must still use [`report_progress`] for
    /// actual transfer progress.
    pub fn restart_idle_window(&self) {
        *self
            .state
            .last_progress
            .lock()
            .expect("progress state mutex must not be poisoned") = tokio::time::Instant::now();
        self.state.notify.notify_waiters();
    }

    /// Record that the caller observed forward progress.
    ///
    /// Call this only after a meaningful byte, frame, or other contractually
    /// defined unit has been consumed or produced. Merely polling an idle
    /// transport must not reset the progress deadline.
    pub fn report_progress(&self) {
        self.restart_idle_window();
    }
}

impl Default for ProgressReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressWatch {
    async fn wait_until_stalled(&self, idle: Duration) -> ProgressIdleElapsed {
        loop {
            // Register before observing the timestamp so a concurrent progress
            // report cannot be missed between the observation and the await.
            let notified = self.state.notify.notified();
            let deadline = *self
                .state
                .last_progress
                .lock()
                .expect("progress state mutex must not be poisoned")
                + idle;
            let sleep = tokio::time::sleep_until(deadline);
            tokio::pin!(sleep);

            tokio::select! {
                () = notified => continue,
                () = &mut sleep => {
                    let last_progress = *self
                        .state
                        .last_progress
                        .lock()
                        .expect("progress state mutex must not be poisoned");
                    if last_progress + idle <= tokio::time::Instant::now() {
                        return ProgressIdleElapsed;
                    }
                }
            }
        }
    }
}

/// Run an operation while caller-observable progress keeps resetting the idle
/// budget. Unlike [`timeout`], this does not impose a total transfer duration.
pub async fn progress_timeout<F>(
    idle: Duration,
    watch: &ProgressWatch,
    future: F,
) -> Result<F::Output, ProgressIdleElapsed>
where
    F: Future,
{
    tokio::select! {
        output = future => Ok(output),
        elapsed = watch.wait_until_stalled(idle) => Err(elapsed),
    }
}

/// No caller-observable progress occurred before the configured idle deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the kernal-api async operation made no progress before its idle deadline")]
pub struct ProgressIdleElapsed;

/// A counting permit pool bounding how many callers may hold a resource at
/// once.
///
/// `Semaphore` is the facade's RAII permit type: [`Semaphore::acquire`] and
/// [`Semaphore::acquire_many`] return a [`SemaphorePermit`] that releases its
/// permits automatically when dropped, matching the guard pattern
/// [`crate::platform::fs::FileLock`] uses for advisory file locks. Clones of a
/// `Semaphore` share the same permit pool -- acquiring on one clone reduces
/// what every other clone can hand out -- so cloning is how the pool is
/// shared between tasks rather than wrapping it in an `Arc` externally.
#[derive(Clone, Debug)]
pub struct Semaphore {
    inner: Arc<tokio::sync::Semaphore>,
}

impl Semaphore {
    /// The largest number of permits a single semaphore can hold.
    pub const MAX_PERMITS: usize = tokio::sync::Semaphore::MAX_PERMITS;

    /// Create a pool with `permits` available immediately.
    pub fn new(permits: usize) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Semaphore::new(permits)),
        }
    }

    /// The number of permits currently available to acquire.
    ///
    /// This is a point-in-time snapshot for diagnostics and reporting; it is
    /// not a substitute for actually acquiring a permit, since a concurrent
    /// caller may acquire or release between the read and any decision made
    /// from it.
    pub fn available_permits(&self) -> usize {
        self.inner.available_permits()
    }

    /// Acquire one owned permit immediately, or return `None` without waiting
    /// when no permit is available. Dropping it restores shared capacity.
    pub fn try_acquire(&self) -> Option<SemaphorePermit> {
        Arc::clone(&self.inner)
            .try_acquire_owned()
            .ok()
            .map(|permit| SemaphorePermit { _permit: permit })
    }

    /// Acquire one permit, waiting until one is available.
    #[deprecated(note = "can wait forever; use `acquire_within` (#433)")]
    pub async fn acquire(&self) -> SemaphorePermit {
        SemaphorePermit {
            // This crate never exposes a way to close a `Semaphore`, so the
            // only error `acquire_owned` can return -- the pool having been
            // closed -- is unreachable from safe code built on this facade.
            _permit: Arc::clone(&self.inner)
                .acquire_owned()
                .await
                .expect("kernal-api semaphore is never closed"),
        }
    }

    /// Acquire `permits` permits atomically, waiting until that many are
    /// available together.
    #[deprecated(note = "can wait forever; use `acquire_many_within` (#433)")]
    pub async fn acquire_many(&self, permits: u32) -> SemaphorePermit {
        SemaphorePermit {
            // See `acquire`: closing is unreachable through this facade.
            _permit: Arc::clone(&self.inner)
                .acquire_many_owned(permits)
                .await
                .expect("kernal-api semaphore is never closed"),
        }
    }

    /// Acquire one permit, waiting at most `timeout`. An expired wait holds
    /// no permits.
    ///
    /// # Errors
    ///
    /// Returns [`SemaphoreAcquireTimedOut`] if no permit became available in
    /// time. Requires a runtime with its timer enabled.
    pub async fn acquire_within(
        &self,
        timeout: Duration,
    ) -> Result<SemaphorePermit, SemaphoreAcquireTimedOut> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = self.acquire();
        self::timeout(timeout, wait)
            .await
            .map_err(|_| SemaphoreAcquireTimedOut)
    }

    /// Acquire `permits` permits atomically, waiting at most `timeout`. An
    /// expired wait holds none of them.
    ///
    /// # Errors
    ///
    /// As [`Self::acquire_within`].
    pub async fn acquire_many_within(
        &self,
        permits: u32,
        timeout: Duration,
    ) -> Result<SemaphorePermit, SemaphoreAcquireTimedOut> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = self.acquire_many(permits);
        self::timeout(timeout, wait)
            .await
            .map_err(|_| SemaphoreAcquireTimedOut)
    }
}

/// A bounded [`Semaphore`] acquisition expired before enough permits were
/// available.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the kernal-api semaphore had no permits before the acquisition timed out")]
pub struct SemaphoreAcquireTimedOut;

/// An RAII permit held against a [`Semaphore`]'s pool.
///
/// Dropping the permit returns its permits to the pool. This type carries no
/// reference back to the `Semaphore` it came from, so it may outlive the
/// clone that acquired it and move freely between tasks.
#[derive(Debug)]
pub struct SemaphorePermit {
    // Held purely for its `Drop`, which returns the permits to the pool. The
    // leading underscore keeps the dead-code lint from flagging a field that
    // is never read by name.
    _permit: tokio::sync::OwnedSemaphorePermit,
}

/// Sending half of a bounded, multi-producer channel created by [`channel`].
///
/// Cloning a `Sender` adds another producer; the channel closes for
/// receiving once every clone (and the original) has been dropped.
pub struct Sender<T> {
    inner: tokio::sync::mpsc::Sender<T>,
}

// Channel handles describe themselves without `T: Debug`: the payload type
// says nothing about the handle, and a derive would stop a struct that holds a
// sender of an opaque type from deriving `Debug` itself.
macro_rules! debug_channel_handle {
    ($($handle:ident),+ $(,)?) => {$(
        impl<T> std::fmt::Debug for $handle<T> {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.debug_struct(stringify!($handle)).finish_non_exhaustive()
            }
        }
    )+};
}

debug_channel_handle!(
    Sender,
    Receiver,
    UnboundedSender,
    UnboundedReceiver,
    OneshotSender,
    OneshotReceiver,
);

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Sender<T> {
    /// Send a value, waiting for capacity if the channel is currently full.
    ///
    /// # Errors
    ///
    /// Returns the value back to the caller if every [`Receiver`] has been
    /// dropped.
    #[deprecated(note = "can wait forever; use `send_within` (#433)")]
    pub async fn send(&self, value: T) -> Result<(), SendError<T>> {
        self.inner
            .send(value)
            .await
            .map_err(|error| SendError(error.0))
    }

    /// Send a value, waiting at most `timeout` for capacity. An expired wait
    /// keeps no reservation.
    ///
    /// # Errors
    ///
    /// Returns the unsent value if every [`Receiver`] has been dropped or the
    /// channel stays full for the whole timeout. Requires a runtime with its
    /// timer enabled.
    pub async fn send_within(
        &self,
        value: T,
        timeout: Duration,
    ) -> Result<(), SendTimeoutError<T>> {
        match self::timeout(timeout, self.inner.reserve()).await {
            Ok(Ok(permit)) => {
                permit.send(value);
                Ok(())
            }
            Ok(Err(_)) => Err(SendTimeoutError::Closed(value)),
            Err(_) => Err(SendTimeoutError::TimedOut(value)),
        }
    }

    /// Send from a synchronous producer, parking this thread until capacity is
    /// available or `timeout` elapses. No runtime is created, and the bounded
    /// queue is unchanged. An expired wait keeps no reservation.
    ///
    /// # Errors
    ///
    /// Returns the unsent value if the receiver closes or is dropped, if
    /// `timeout` elapses first, or if this thread has entered a runtime. Use
    /// [`Self::send`] inside a runtime.
    pub fn blocking_send(&self, value: T, timeout: Duration) -> Result<(), BlockingSendError<T>> {
        if RuntimeHandle::current().is_ok() {
            return Err(BlockingSendError::AsyncContext(value));
        }
        match park_within(self.inner.reserve(), timeout) {
            Some(Ok(permit)) => {
                permit.send(value);
                Ok(())
            }
            Some(Err(_)) => Err(BlockingSendError::Closed(value)),
            None => Err(BlockingSendError::TimedOut(value)),
        }
    }

    /// Send a value without waiting for capacity.
    ///
    /// # Errors
    ///
    /// Returns the value back to the caller if the channel is at capacity or
    /// every [`Receiver`] has been dropped.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.inner.try_send(value).map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(value) => TrySendError::Full(value),
            tokio::sync::mpsc::error::TrySendError::Closed(value) => TrySendError::Closed(value),
        })
    }

    /// Whether every [`Receiver`] for this channel has been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}

/// Receiving half of a bounded, multi-producer channel created by
/// [`channel`].
pub struct Receiver<T> {
    inner: tokio::sync::mpsc::Receiver<T>,
}

impl<T> Receiver<T> {
    /// Wait for the next value, or `None` once every [`Sender`] has been
    /// dropped and no values remain buffered.
    #[deprecated(note = "can wait forever; use `recv_within` (#433)")]
    pub async fn recv(&mut self) -> Option<T> {
        self.inner.recv().await
    }

    /// Wait at most `timeout` for the next value.
    ///
    /// # Errors
    ///
    /// Returns [`RecvTimeoutError::Disconnected`] once every [`Sender`] has
    /// been dropped and nothing remains buffered, or
    /// [`RecvTimeoutError::TimedOut`] if no value arrived in time. Requires a
    /// runtime with its timer enabled.
    pub async fn recv_within(&mut self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        match self::timeout(timeout, self.inner.recv()).await {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Err(RecvTimeoutError::Disconnected),
            Err(_) => Err(RecvTimeoutError::TimedOut),
        }
    }

    /// Take the next value without waiting.
    ///
    /// # Errors
    ///
    /// Returns [`TryRecvError::Empty`] if no value is currently buffered, or
    /// [`TryRecvError::Disconnected`] if every [`Sender`] has been dropped
    /// and no values remain buffered.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.inner.try_recv().map_err(|error| match error {
            tokio::sync::mpsc::error::TryRecvError::Empty => TryRecvError::Empty,
            tokio::sync::mpsc::error::TryRecvError::Disconnected => TryRecvError::Disconnected,
        })
    }

    /// Close the channel for sending. Values already buffered remain
    /// available to [`Receiver::recv`] and [`Receiver::try_recv`].
    pub fn close(&mut self) {
        self.inner.close();
    }
}

/// Create a bounded, multi-producer, single-consumer channel.
///
/// `capacity` bounds how many values may be buffered before [`Sender::send`]
/// waits for the receiver to catch up. This is the default channel shape for
/// new code per `ARCHITECTURE.md`'s bounded-resources contract; use
/// [`unbounded_channel`] only when the producer side must never wait for the
/// consumer.
pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
    (Sender { inner: sender }, Receiver { inner: receiver })
}

/// Sending half of an unbounded, multi-producer channel created by
/// [`unbounded_channel`].
pub struct UnboundedSender<T> {
    inner: tokio::sync::mpsc::UnboundedSender<T>,
}

impl<T> Clone for UnboundedSender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> UnboundedSender<T> {
    /// Send a value. This never waits: an unbounded channel has no capacity
    /// limit to wait for.
    ///
    /// # Errors
    ///
    /// Returns the value back to the caller if every
    /// [`UnboundedReceiver`] has been dropped.
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        self.inner.send(value).map_err(|error| SendError(error.0))
    }

    /// Whether every [`UnboundedReceiver`] for this channel has been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}

/// Receiving half of an unbounded, multi-producer channel created by
/// [`unbounded_channel`].
pub struct UnboundedReceiver<T> {
    inner: tokio::sync::mpsc::UnboundedReceiver<T>,
}

impl<T> UnboundedReceiver<T> {
    /// Wait for the next value, or `None` once every [`UnboundedSender`] has
    /// been dropped and no values remain buffered.
    #[deprecated(note = "can wait forever; use `recv_within` (#433)")]
    pub async fn recv(&mut self) -> Option<T> {
        self.inner.recv().await
    }

    /// Wait at most `timeout` for the next value.
    ///
    /// # Errors
    ///
    /// As [`Receiver::recv_within`].
    pub async fn recv_within(&mut self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        match self::timeout(timeout, self.inner.recv()).await {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Err(RecvTimeoutError::Disconnected),
            Err(_) => Err(RecvTimeoutError::TimedOut),
        }
    }

    /// Take the next value without waiting.
    ///
    /// # Errors
    ///
    /// Returns [`TryRecvError::Empty`] if no value is currently buffered, or
    /// [`TryRecvError::Disconnected`] if every [`UnboundedSender`] has been
    /// dropped and no values remain buffered.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.inner.try_recv().map_err(|error| match error {
            tokio::sync::mpsc::error::TryRecvError::Empty => TryRecvError::Empty,
            tokio::sync::mpsc::error::TryRecvError::Disconnected => TryRecvError::Disconnected,
        })
    }

    /// Close the channel for sending. Values already buffered remain
    /// available to [`UnboundedReceiver::recv`] and
    /// [`UnboundedReceiver::try_recv`].
    pub fn close(&mut self) {
        self.inner.close();
    }
}

/// Create an unbounded, multi-producer, single-consumer channel.
///
/// An unbounded channel never applies backpressure to its senders, which
/// means an unbounded producer can grow this channel's buffer without limit.
/// Prefer [`channel`]'s bounded form for new code; this exists for call sites
/// whose producer must never wait (for example, delivering a shutdown
/// command from a synchronous `Drop` implementation).
pub fn unbounded_channel<T>() -> (UnboundedSender<T>, UnboundedReceiver<T>) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    (
        UnboundedSender { inner: sender },
        UnboundedReceiver { inner: receiver },
    )
}

/// A value could not be sent because every receiver has been dropped.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("the kernal-api channel receiver has been dropped")]
pub struct SendError<T>(pub T);

/// A synchronous send failed, retaining the unsent value.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum BlockingSendError<T> {
    /// The receiving half has been closed or dropped.
    #[error("the kernal-api channel receiver is closed")]
    Closed(T),
    /// Blocking is forbidden on a thread that has entered a runtime.
    #[error("blocking channel send cannot run inside a runtime")]
    AsyncContext(T),
    /// No capacity became available before the timeout.
    #[error("the kernal-api channel stayed full until the send timed out")]
    TimedOut(T),
}

/// Drive `future` on this thread, parking between polls, until it completes
/// or `timeout` elapses (`None`). No runtime is created; an unpark that lands
/// before the park is kept by the thread token, so no wakeup is lost.
pub(crate) fn park_within<F: Future>(future: F, timeout: Duration) -> Option<F::Output> {
    let started = std::time::Instant::now();
    let waker = std::task::Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Some(output);
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return None;
        }
        std::thread::park_timeout(timeout - elapsed);
    }
}

/// Unparks a thread parked in [`park_within`] when its future can progress.
struct ThreadWaker(std::thread::Thread);

impl std::task::Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// A bounded-time send failed, retaining the unsent value.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum SendTimeoutError<T> {
    /// Every receiver for the channel has been dropped.
    #[error("the kernal-api channel receiver has been dropped")]
    Closed(T),
    /// The channel stayed full until the send timed out.
    #[error("the kernal-api channel stayed full until the send timed out")]
    TimedOut(T),
}

/// A bounded-time receive produced no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecvTimeoutError {
    /// No value arrived before the timeout; at least one sender remains.
    #[error("no value arrived on the kernal-api channel before the timeout")]
    TimedOut,
    /// Every sender has been dropped and no value remains buffered.
    #[error("the kernal-api channel sender has been dropped")]
    Disconnected,
}

/// A value could not be sent through a bounded channel without waiting.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum TrySendError<T> {
    /// The channel has no free capacity right now.
    #[error("the kernal-api channel is at capacity")]
    Full(T),
    /// Every receiver for the channel has been dropped.
    #[error("the kernal-api channel receiver has been dropped")]
    Closed(T),
}

/// No value was available from a channel without waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TryRecvError {
    /// No value is buffered right now, but at least one sender remains.
    #[error("the kernal-api channel has no value available right now")]
    Empty,
    /// Every sender has been dropped and no value remains buffered.
    #[error("the kernal-api channel sender has been dropped")]
    Disconnected,
}

/// Sending half of a single-value channel created by [`oneshot_channel`].
pub struct OneshotSender<T> {
    inner: tokio::sync::oneshot::Sender<T>,
}

impl<T> OneshotSender<T> {
    /// Send the single value this channel carries.
    ///
    /// # Errors
    ///
    /// Returns the value back to the caller if the [`OneshotReceiver`] has
    /// already been dropped.
    pub fn send(self, value: T) -> Result<(), T> {
        self.inner.send(value)
    }

    /// Whether the paired [`OneshotReceiver`] has been dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}

/// Receiving half of a single-value channel created by [`oneshot_channel`].
///
/// Awaiting a `OneshotReceiver` resolves once the paired [`OneshotSender`]
/// sends its value or is dropped; it composes with [`timeout`] and
/// [`cancellable`] like any other future.
pub struct OneshotReceiver<T> {
    inner: tokio::sync::oneshot::Receiver<T>,
}

impl<T> OneshotReceiver<T> {
    /// Take the value without waiting.
    ///
    /// # Errors
    ///
    /// Returns [`TryRecvError::Empty`] if the [`OneshotSender`] has not sent
    /// its value yet, or [`TryRecvError::Disconnected`] if it was dropped
    /// without sending one.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.inner.try_recv().map_err(|error| match error {
            tokio::sync::oneshot::error::TryRecvError::Empty => TryRecvError::Empty,
            tokio::sync::oneshot::error::TryRecvError::Closed => TryRecvError::Disconnected,
        })
    }

    /// Wait at most `timeout` for the value. An expired wait consumes
    /// nothing, so the receiver can wait again.
    ///
    /// # Errors
    ///
    /// Returns [`RecvTimeoutError::Disconnected`] if the [`OneshotSender`] was
    /// dropped without sending, or [`RecvTimeoutError::TimedOut`] if nothing
    /// arrived in time. Requires a runtime with its timer enabled.
    pub async fn recv_within(&mut self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        match self::timeout(timeout, &mut self.inner).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(RecvTimeoutError::Disconnected),
            Err(_) => Err(RecvTimeoutError::TimedOut),
        }
    }
}

impl<T> Future for OneshotReceiver<T> {
    type Output = Result<T, OneshotReceiverClosed>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.inner)
            .poll(context)
            .map(|result| result.map_err(|_| OneshotReceiverClosed))
    }
}

/// A [`OneshotSender`] was dropped without sending its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the kernal-api channel sender was dropped without sending a value")]
pub struct OneshotReceiverClosed;

/// Create a single-value, single-producer, single-consumer channel.
///
/// This is the facade's request/acknowledgement primitive: pair it with
/// [`timeout`] to bound how long a caller waits for the acknowledgement, and
/// with [`Sender`]/[`UnboundedSender`] to carry the acknowledging half of a
/// command as part of a larger message.
pub fn oneshot_channel<T>() -> (OneshotSender<T>, OneshotReceiver<T>) {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    (
        OneshotSender { inner: sender },
        OneshotReceiver { inner: receiver },
    )
}

#[cfg(feature = "tokio-console")]
pub use crate::runtime::{DiagnosticsConfig, DiagnosticsInstallError};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_runtime_launches_owned_tasks() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let answer = runtime
            .run_within(
                async { launch(async { 42_u8 }).await.unwrap() },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(answer, 42);
    }

    #[test]
    fn deadline_error_is_facade_owned() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime
            .run_within(
                timeout(Duration::ZERO, std::future::pending::<()>()),
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert!(result.is_err());
    }

    #[test]
    fn cancellation_source_unblocks_a_pending_operation() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    let source = CancellationSource::new();
                    let token = source.token();
                    let waiter = launch({
                        let token = token.clone();
                        async move { cancellable(&token, std::future::pending::<()>()).await }
                    });
                    yield_now().await;
                    source.cancel();
                    assert_eq!(waiter.await.unwrap(), Err(Cancelled));
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    fn cancellation_between_state_check_and_waiter_poll_is_not_lost() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    let source = CancellationSource::new();
                    let token = source.token();
                    token
                        .state
                        .cancel_after_waiter_check
                        .store(true, Ordering::Release);

                    token.cancelled().await;
                    assert!(source.is_cancelled());
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    fn connection_deadline_has_its_own_typed_error() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let deadline = Deadline::after(Duration::ZERO);
        assert!(deadline.is_elapsed());
        let result = runtime
            .run_within(
                connection_until(deadline, std::future::pending::<()>()),
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(result, Err(ConnectionDeadlineElapsed));
    }

    #[test]
    fn progress_keeps_an_operation_alive_beyond_one_idle_window() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    tokio::time::pause();
                    let reporter = ProgressReporter::new();
                    let watch = reporter.watch();
                    let operation = async move {
                        for _ in 0..3 {
                            // Advancing from this same operation establishes the
                            // contract sequence: report at 10/20/30 ms, each before
                            // the 15 ms idle deadline. A spawned driver can advance
                            // time before its sibling has reported and is not a
                            // reliable policy test.
                            tokio::time::advance(Duration::from_millis(10)).await;
                            reporter.report_progress();
                        }
                        7_u8
                    };

                    assert_eq!(
                        progress_timeout(Duration::from_millis(15), &watch, operation).await,
                        Ok(7)
                    );
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    fn scheduled_progress_after_its_idle_deadline_is_not_observed_progress() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    tokio::time::pause();
                    let reporter = ProgressReporter::new();
                    let watch = reporter.watch();
                    let release_progress = Arc::new(BackendNotify::new());
                    let progress_task = launch({
                        let reporter = reporter.clone();
                        let release_progress = Arc::clone(&release_progress);
                        async move {
                            release_progress.notified().await;
                            reporter.report_progress();
                        }
                    });
                    let mut result = std::pin::pin!(progress_timeout(
                        Duration::from_millis(10),
                        &watch,
                        std::future::pending::<()>(),
                    ));

                    std::future::poll_fn(|context| {
                        assert!(matches!(
                            result.as_mut().poll(context),
                            std::task::Poll::Pending
                        ));
                        std::task::Poll::Ready(())
                    })
                    .await;

                    // Retained RED characterization of the removed test fixture:
                    // work that is merely scheduled is not progress. The separate
                    // task cannot report until after the idle deadline, so the typed
                    // idle error is the correct result and not a product regression.
                    tokio::time::advance(Duration::from_millis(10)).await;
                    assert_eq!(result.await, Err(ProgressIdleElapsed));

                    release_progress.notify_one();
                    progress_task.await.unwrap();
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    fn stalled_operation_expires_after_the_progress_idle_window() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    tokio::time::pause();
                    let reporter = ProgressReporter::new();
                    let watch = reporter.watch();
                    let mut result = std::pin::pin!(progress_timeout(
                        Duration::from_millis(10),
                        &watch,
                        std::future::pending::<()>(),
                    ));

                    std::future::poll_fn(|context| {
                        assert!(matches!(
                            result.as_mut().poll(context),
                            std::task::Poll::Pending
                        ));
                        std::task::Poll::Ready(())
                    })
                    .await;
                    tokio::time::advance(Duration::from_millis(10)).await;
                    assert_eq!(result.await, Err(ProgressIdleElapsed));
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    fn dropping_a_task_cancels_it() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    tokio::time::pause();
                    let completed = Arc::new(AtomicBool::new(false));
                    let flag = Arc::clone(&completed);
                    let task = launch(async move {
                        sleep(Duration::from_millis(10)).await;
                        flag.store(true, Ordering::Release);
                    });
                    // Let the task start and register its sleep timer before it is
                    // cancelled, so this proves cancellation interrupts pending work
                    // rather than merely racing the task's own start.
                    yield_now().await;
                    drop(task);
                    tokio::time::advance(Duration::from_millis(20)).await;
                    yield_now().await;
                    assert!(
                        !completed.load(Ordering::Acquire),
                        "a dropped task must not run to completion"
                    );
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    fn a_detached_task_keeps_running_after_its_handle_is_dropped() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    tokio::time::pause();
                    let completed = Arc::new(AtomicBool::new(false));
                    let flag = Arc::clone(&completed);
                    launch(async move {
                        sleep(Duration::from_millis(10)).await;
                        flag.store(true, Ordering::Release);
                    })
                    .detach();
                    // Let the detached task register its sleep timer at T0 before
                    // advancing the clock, matching `dropping_a_task_cancels_it`'s
                    // ordering so the timer fires within this advance.
                    yield_now().await;
                    tokio::time::advance(Duration::from_millis(20)).await;
                    yield_now().await;
                    assert!(
                        completed.load(Ordering::Acquire),
                        "a detached task must keep running after its handle is dropped"
                    );
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    fn join_awaits_both_futures_concurrently() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    let (first, second) = join(async { 1_u8 }, async { 2_u8 }).await;
                    assert_eq!((first, second), (1, 2));
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    // Contract test of a form deprecated by #433.
    #[allow(deprecated)]
    fn task_group_collects_results_as_tasks_complete() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.run(async {
            let mut group = TaskGroup::new();
            for value in 0_u8..3 {
                group.spawn(async move { value });
            }
            let mut collected = Vec::new();
            while let Some(result) = group.join_next().await {
                collected.push(result.unwrap());
            }
            collected.sort_unstable();
            assert_eq!(collected, vec![0, 1, 2]);
            assert!(group.is_empty());
        });
    }

    #[test]
    fn notify_wakes_a_registered_waiter() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    let notify = Arc::new(Notify::new());
                    let waiter_notify = Arc::clone(&notify);
                    let waiter = launch(async move {
                        waiter_notify.notified().await;
                    });
                    yield_now().await;
                    notify.notify_one();
                    waiter.await.unwrap();
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    // Contract test of a form deprecated by #433.
    #[allow(deprecated)]
    fn semaphore_serializes_access_to_a_single_permit() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.run(async {
            let semaphore = Semaphore::new(1);
            assert_eq!(semaphore.available_permits(), 1);

            let first = semaphore.acquire().await;
            assert_eq!(semaphore.available_permits(), 0);

            let waiting = semaphore.clone();
            let acquired_second = launch(async move { waiting.acquire().await });
            yield_now().await;
            // The second acquire cannot complete while `first` is held.
            assert_eq!(semaphore.available_permits(), 0);

            drop(first);
            let second = acquired_second.await.unwrap();
            assert_eq!(semaphore.available_permits(), 0);

            drop(second);
            assert_eq!(semaphore.available_permits(), 1);
        });
    }

    #[test]
    fn bounded_channel_reports_capacity_and_disconnect_without_waiting() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    let (sender, mut receiver) = channel::<u8>(1);
                    sender.try_send(1).unwrap();
                    assert!(matches!(sender.try_send(2), Err(TrySendError::Full(2))));
                    assert_eq!(receiver.try_recv(), Ok(1));
                    assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
                    drop(sender);
                    assert_eq!(receiver.try_recv(), Err(TryRecvError::Disconnected));
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    // Contract test of a form deprecated by #433.
    #[allow(deprecated)]
    fn bounded_channel_delivers_values_in_order() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.run(async {
            let (sender, mut receiver) = channel::<u8>(4);
            sender.send(1).await.unwrap();
            sender.send(2).await.unwrap();
            assert_eq!(receiver.recv().await, Some(1));
            assert_eq!(receiver.recv().await, Some(2));
            drop(sender);
            assert_eq!(receiver.recv().await, None);
        });
    }

    #[test]
    // Contract test of a form deprecated by #433.
    #[allow(deprecated)]
    fn unbounded_channel_delivers_values_without_waiting() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.run(async {
            let (sender, mut receiver) = unbounded_channel::<u8>();
            sender.send(7).unwrap();
            assert_eq!(receiver.recv().await, Some(7));
            drop(sender);
            assert_eq!(receiver.recv().await, None);
        });
    }

    #[test]
    fn oneshot_channel_delivers_its_single_value() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    let (sender, receiver) = oneshot_channel::<u8>();
                    sender.send(9).unwrap();
                    assert_eq!(receiver.await, Ok(9));
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    fn oneshot_receiver_observes_a_dropped_sender() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    let (sender, receiver) = oneshot_channel::<u8>();
                    drop(sender);
                    assert_eq!(receiver.await, Err(OneshotReceiverClosed));
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }
}
