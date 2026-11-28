//! Off-CPU / async profiling via pluggable adapters (S17b / #647).
//!
//! # Why an adapter contract instead of a tokio integration
//!
//! Off-CPU time is where a program *is not running*: blocked on a lock, a
//! socket, a channel, a scheduler queue. A CPU profile is blind to all of it —
//! a request that took nine seconds waiting and one second computing looks,
//! in a CPU profile, like one second of work.
//!
//! Every runtime exposes that information differently: tokio streams task
//! updates over a gRPC endpoint, asyncio has `all_tasks()`, and an in-house
//! executor has whatever its authors instrumented. Hardwiring one of them
//! would make the other two second-class forever.
//!
//! So the contract is deliberately narrow:
//!
//! > given (source, interval, frequency) → produce a pprof for that window
//!
//! Everything streaming, runtime-specific, or protocol-shaped is contained
//! *inside* an adapter. The daemon only ever sees a pprof. That is the same
//! model Go uses for its block and mutex profiles, and it is why this rides
//! the existing pipeline (S15/S16) without a single format-specific branch in
//! the flame graph.
//!
//! # Default-off, and bounded like everything else
//!
//! An adapter subscribes to a debugging endpoint the application had to opt
//! into exposing. The window is capped at the same 60 seconds as CPU
//! profiling, for the same reason: a session an operator can start and forget
//! is one that degrades a production process indefinitely.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::profile::export::pprof::AsyncProfileBuilder;
use crate::profile::MAX_DURATION;

/// What an adapter measured for one task.
///
/// Several values rather than one, because "why is this slow" has several
/// answers and a viewer should be able to weight by whichever is being asked
/// about: idle time finds what is waiting, poll count finds what is thrashing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskSample {
    /// Where the task was spawned, and its parent chain — root first.
    ///
    /// The spawn location rather than the current stack: an async task's stack
    /// at any instant is its executor's, which is the same for every task and
    /// tells you nothing. Where it was spawned is what distinguishes it.
    pub spawn_stack: Vec<String>,
    /// Nanoseconds the task spent waiting.
    pub idle_nanos: i64,
    /// Nanoseconds the task spent running.
    pub busy_nanos: i64,
    /// Nanoseconds between being woken and being polled.
    pub scheduled_nanos: i64,
    /// Times the task was polled.
    pub polls: i64,
    /// Times the task was woken.
    pub wakes: i64,
    /// Task name or id, carried as a pprof label.
    pub name: String,
}

/// Why an async profile could not be produced.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AsyncUnavailable {
    /// No adapter is registered under that name.
    #[error("no async profiling adapter named {requested:?}; available: {available}")]
    UnknownAdapter {
        /// What was asked for.
        requested: String,
        /// What exists.
        available: String,
    },
    /// The adapter's source is not reachable.
    #[error("the {adapter} source is not reachable at {endpoint}: {detail}\n{remediation}")]
    SourceUnreachable {
        /// Adapter that failed.
        adapter: &'static str,
        /// Where it looked.
        endpoint: String,
        /// What went wrong.
        detail: String,
        /// What the operator can do.
        remediation: String,
    },
    /// The adapter produced nothing over the window.
    #[error(
        "the {adapter} source produced no task data over {seconds}s. Either nothing \
         ran, or the application is not instrumented — an empty profile and an idle \
         program look identical, so this is reported rather than drawn."
    )]
    NoData {
        /// Adapter that ran.
        adapter: &'static str,
        /// Window length.
        seconds: u64,
    },
}

/// Produces task samples for a bounded window.
///
/// The whole runtime-specific surface. An implementation may stream, poll, or
/// read a file; the daemon cannot tell and does not need to.
pub trait AsyncAdapter {
    /// Adapter name, as an operator types it.
    fn name(&self) -> &'static str;

    /// Collect over `window`, then return.
    ///
    /// Bounded by contract: an adapter that ran longer than its window would
    /// make the profile describe a period the operator did not ask about.
    fn collect(&mut self, window: Duration) -> Result<Vec<TaskSample>, AsyncUnavailable>;
}

/// Clamp a requested window to the enforced ceiling.
///
/// The same 60 seconds as CPU profiling, and for the same reason: a session
/// an operator can start and forget is one that degrades a production process
/// indefinitely.
pub fn clamp_window(requested: Duration) -> Duration {
    requested.min(MAX_DURATION)
}

/// Lower task samples to pprof.
///
/// Five value types, so a viewer can weight the same flame graph by whichever
/// question is being asked. `idle_nanos` is the default: someone reaching for
/// an off-CPU profile is asking what is *waiting*, and opening on busy time
/// would show them the CPU profile they already had.
pub fn to_pprof(samples: &[TaskSample]) -> Vec<u8> {
    let mut builder = AsyncProfileBuilder::new();
    for sample in samples {
        builder.add_sample(
            &sample.spawn_stack,
            [
                sample.idle_nanos,
                sample.busy_nanos,
                sample.scheduled_nanos,
                sample.polls,
                sample.wakes,
            ],
            &sample.name,
        );
    }
    builder.finish()
}

/// Render task samples as collapsed stacks, weighted by idle time.
///
/// Feeds the same flame graph as CPU and heap. Idle rather than busy, because
/// the whole reason to look at an off-CPU profile is to find the waiting.
pub fn to_collapsed(samples: &[TaskSample]) -> String {
    let mut folded: BTreeMap<String, i64> = BTreeMap::new();
    for sample in samples {
        if sample.idle_nanos <= 0 || sample.spawn_stack.is_empty() {
            continue;
        }
        // Semicolons replaced: the collapsed format has no escape syntax, so
        // one inside a spawn location would forge a frame.
        let stack: Vec<String> = sample
            .spawn_stack
            .iter()
            .map(|frame| frame.replace(';', ":"))
            .collect();
        *folded.entry(stack.join(";")).or_insert(0) += sample.idle_nanos;
    }

    let mut rows: Vec<(String, i64)> = folded.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.into_iter()
        .map(|(stack, nanos)| format!("{stack} {nanos}\n"))
        .collect()
}

/// The tokio adapter: accumulates `console-api` task updates over a window.
///
/// Streaming is entirely contained here — the daemon receives a finished
/// `Vec<TaskSample>` and never learns that gRPC was involved.
#[cfg(feature = "tokio-console")]
#[derive(Debug)]
pub struct AsyncEngineAdapter {
    /// Where `console-subscriber` is listening.
    pub endpoint: String,
}

#[cfg(feature = "tokio-console")]
impl Default for AsyncEngineAdapter {
    fn default() -> Self {
        Self {
            // `console-subscriber`'s documented default.
            endpoint: "http://127.0.0.1:6669".to_string(),
        }
    }
}

#[cfg(feature = "tokio-console")]
impl AsyncAdapter for AsyncEngineAdapter {
    fn name(&self) -> &'static str {
        "kernal-api-async"
    }

    fn collect(&mut self, window: Duration) -> Result<Vec<TaskSample>, AsyncUnavailable> {
        // The subscription lives in `async_tokio` so this module stays free of
        // gRPC vocabulary. On failure that call returns a `SourceUnreachable`
        // naming the missing `console_subscriber::init()` rather than just the
        // refused connection: a missing subscriber is the overwhelmingly
        // common cause and a connection error does not make it obvious.
        crate::profile::async_tokio::collect(&self.endpoint, window)
    }
}

/// An adapter over a caller-supplied producer.
///
/// The escape hatch for a runtime that is neither tokio nor asyncio. Its
/// existence is what keeps the contract honest: if a custom producer can drive
/// the whole pipeline, then nothing downstream is tokio-shaped.
pub struct CustomAdapter<F> {
    producer: F,
}

impl<F> std::fmt::Debug for CustomAdapter<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomAdapter").finish_non_exhaustive()
    }
}

impl<F> CustomAdapter<F>
where
    F: FnMut(Duration) -> Vec<TaskSample>,
{
    /// Wrap `producer` as an adapter.
    pub fn new(producer: F) -> Self {
        Self { producer }
    }
}

impl<F> AsyncAdapter for CustomAdapter<F>
where
    F: FnMut(Duration) -> Vec<TaskSample>,
{
    fn name(&self) -> &'static str {
        "custom"
    }

    fn collect(&mut self, window: Duration) -> Result<Vec<TaskSample>, AsyncUnavailable> {
        let samples = (self.producer)(window);
        if samples.is_empty() {
            // An empty profile and an idle program look identical once drawn,
            // so the difference is reported rather than rendered.
            return Err(AsyncUnavailable::NoData {
                adapter: "custom",
                seconds: window.as_secs(),
            });
        }
        Ok(samples)
    }
}

/// Run an adapter over a clamped window.
pub fn profile(
    adapter: &mut dyn AsyncAdapter,
    requested: Duration,
) -> Result<Vec<TaskSample>, AsyncUnavailable> {
    adapter.collect(clamp_window(requested))
}

#[cfg(test)]
mod tests;
