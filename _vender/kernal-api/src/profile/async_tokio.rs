//! The backend subscription behind [`AsyncEngineAdapter`].
//!
//! # Why this is a separate module
//!
//! `async_profile` is the contract: `TaskSample`, the adapter trait, the pprof
//! lowering. It is deliberately free of any runtime's vocabulary. Everything
//! gRPC-shaped lives here, so the claim "nothing downstream is tokio-shaped"
//! stays checkable by looking at what this file exports — one function
//! returning `Vec<TaskSample>`.
//!
//! # What it collects
//!
//! `console-subscriber` serves a stream of `InstrumentUpdate`. The first
//! message carries the tasks that already exist; later ones carry new tasks
//! and updated per-task stats. Stats are cumulative, not deltas, so the last
//! observation of each task wins and the window is simply how long we listen.
//!
//! # Idle time is derived, not reported
//!
//! console-api reports `busy_time` and `scheduled_time`, plus the timestamps
//! bounding a task's life. Idle is what is left:
//!
//! ```text
//! idle = (dropped_at | now) - created_at - busy_time - scheduled_time
//! ```
//!
//! Clamped at zero, because the three components are sampled at slightly
//! different instants and a task polled right as the window closed can
//! otherwise produce a small negative.
//!
//! [`AsyncEngineAdapter`]: super::async_profile::AsyncEngineAdapter

use std::collections::HashMap;
use std::time::Duration;

use console_api::instrument::instrument_client::InstrumentClient;
use console_api::instrument::InstrumentRequest;

use super::async_profile::{AsyncUnavailable, TaskSample};

/// Subscribe to `endpoint` for `window` and return one sample per task.
///
/// Blocking compatibility entry point for [`super::async_profile::AsyncAdapter`].
///
/// The subscription runs on a dedicated OS thread. Creating and driving a
/// nested runtime on the caller's Tokio worker would panic, while synchronously
/// waiting on the caller's runtime could deadlock it. Async applications can
/// call [`collect_async`] directly.
pub fn collect(endpoint: &str, window: Duration) -> Result<Vec<TaskSample>, AsyncUnavailable> {
    let owned_endpoint = endpoint.to_owned();
    let error_endpoint = owned_endpoint.clone();
    std::thread::Builder::new()
        .name("kernal-api-tokio-profile".to_owned())
        .spawn(move || collect_on_dedicated_runtime(&owned_endpoint, window))
        .map_err(|error| {
            unreachable(
                &error_endpoint,
                format!("could not start the collector thread: {error}"),
            )
        })?
        .join()
        .map_err(|_| unreachable(&error_endpoint, "collector thread panicked".to_owned()))?
}

fn collect_on_dedicated_runtime(
    endpoint: &str,
    window: Duration,
) -> Result<Vec<TaskSample>, AsyncUnavailable> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| unreachable(endpoint, format!("could not start a runtime: {e}")))?;
    runtime.block_on(collect_async(endpoint, window))
}

/// Subscribe without blocking the current Tokio runtime.
pub async fn collect_async(
    endpoint: &str,
    window: Duration,
) -> Result<Vec<TaskSample>, AsyncUnavailable> {
    subscribe(endpoint, window).await
}

/// The `SourceUnreachable` this module produces, with the remediation that is
/// almost always the real cause.
fn unreachable(endpoint: &str, detail: String) -> AsyncUnavailable {
    AsyncUnavailable::SourceUnreachable {
        adapter: "tokio",
        endpoint: endpoint.to_string(),
        detail,
        remediation: "Add `console-subscriber = \"0.5\"` to the application, call \
                      `console_subscriber::init()` at startup, and build it with \
                      `RUSTFLAGS=\"--cfg tokio_unstable\"`."
            .to_string(),
    }
}

async fn subscribe(endpoint: &str, window: Duration) -> Result<Vec<TaskSample>, AsyncUnavailable> {
    let mut client = InstrumentClient::connect(endpoint.to_string())
        .await
        .map_err(|e| unreachable(endpoint, e.to_string()))?;

    let mut stream = client
        .watch_updates(InstrumentRequest {})
        .await
        .map_err(|e| unreachable(endpoint, e.to_string()))?
        .into_inner();

    // Task metadata and stats arrive in different messages, so both are
    // accumulated by task id and joined at the end.
    let mut spawned: HashMap<u64, String> = HashMap::new();
    let mut stats: HashMap<u64, console_api::tasks::Stats> = HashMap::new();

    let deadline = tokio::time::Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        // A quiet runtime sends nothing; the window is a deadline, not a
        // requirement that anything happen.
        match tokio::time::timeout(remaining, stream.message()).await {
            Ok(Ok(Some(update))) => {
                let Some(task_update) = update.task_update else {
                    continue;
                };
                for task in task_update.new_tasks {
                    if let Some(id) = task.id.as_ref().map(|id| id.id) {
                        spawned.insert(id, describe(&task));
                    }
                }
                for (id, stat) in task_update.stats_update {
                    stats.insert(id, stat);
                }
            }
            // The subscriber closed the stream: report what was collected.
            Ok(Ok(None)) => break,
            Ok(Err(e)) => return Err(unreachable(endpoint, e.to_string())),
            // Window elapsed mid-wait.
            Err(_) => break,
        }
    }

    if spawned.is_empty() && stats.is_empty() {
        return Err(AsyncUnavailable::NoData {
            adapter: "tokio",
            seconds: window.as_secs(),
        });
    }

    Ok(join(spawned, stats))
}

/// Name a task by its spawn site, falling back to its id.
///
/// The spawn location rather than a stack: an async task's stack at any
/// instant is its executor's, which is identical for every task and says
/// nothing about which task it is.
fn describe(task: &console_api::tasks::Task) -> String {
    if let Some(location) = task.location.as_ref() {
        let file = location.file.as_deref().or(location.module_path.as_deref());
        if let Some(file) = file {
            return match location.line {
                Some(line) => format!("{file}:{line}"),
                None => file.to_string(),
            };
        }
    }
    match task.id.as_ref() {
        Some(id) => format!("task#{}", id.id),
        None => "task".to_string(),
    }
}

/// Is this task part of the runtime itself rather than the application?
///
/// tokio instruments its own machinery. A multi-thread runtime parks one
/// blocking-pool worker per core, and each is a task that is idle essentially
/// all the time — so on an idle-weighted profile they occupy every top slot
/// and push the application's tasks off the graph. Measured on the fixture:
/// 16 of 19 tasks were `runtime/scheduler/multi_thread/worker.rs`, all with
/// identical idle time, above the deliberately-blocked task the profile
/// exists to find.
///
/// They are dropped rather than merged into one row: "the runtime waited"
/// is not an answer to "what is my program waiting on", at any weight.
///
/// This mirrors what the asyncio adapter does with `_LOOP_INTERNALS`; the two
/// runtimes have the same problem for the same reason.
fn is_runtime_internal(location: &str) -> bool {
    // Match the crate's own source, not merely the word "tokio" — an
    // application module called `tokio_workers.rs` is the caller's code and
    // hiding it would be worse than the noise this removes.
    let normalized = location.replace('\\', "/");
    normalized.contains("/tokio-") && normalized.contains("/src/runtime/")
}

fn join(
    spawned: HashMap<u64, String>,
    stats: HashMap<u64, console_api::tasks::Stats>,
) -> Vec<TaskSample> {
    let mut samples: Vec<TaskSample> = stats
        .into_iter()
        .filter(|(id, _)| {
            spawned
                .get(id)
                .is_none_or(|location| !is_runtime_internal(location))
        })
        .map(|(id, stat)| {
            let name = spawned
                .get(&id)
                .cloned()
                .unwrap_or_else(|| format!("task#{id}"));
            let poll = stat.poll_stats.unwrap_or_default();
            let busy = nanos(poll.busy_time.as_ref());
            let scheduled = nanos(stat.scheduled_time.as_ref());
            let lifetime = lifetime_nanos(stat.created_at.as_ref(), stat.dropped_at.as_ref());

            TaskSample {
                // One frame: the spawn site is the whole identity here.
                spawn_stack: vec![name.clone()],
                idle_nanos: (lifetime - busy - scheduled).max(0),
                busy_nanos: busy,
                scheduled_nanos: scheduled,
                polls: poll.polls as i64,
                wakes: stat.wakes as i64,
                name,
            }
        })
        .collect();
    // Deterministic order so a caller diffing two profiles sees real changes.
    samples.sort_by(|a, b| a.name.cmp(&b.name));
    samples
}

fn nanos(duration: Option<&prost_types::Duration>) -> i64 {
    duration.map_or(0, |d| d.seconds * 1_000_000_000 + i64::from(d.nanos))
}

/// How long the task has existed, in nanoseconds.
///
/// A task still running is measured to now; one already dropped to its drop
/// time, so a short-lived task is not credited with idle time it spent not
/// existing.
fn lifetime_nanos(
    created_at: Option<&prost_types::Timestamp>,
    dropped_at: Option<&prost_types::Timestamp>,
) -> i64 {
    let Some(created) = created_at else {
        return 0;
    };
    let end = match dropped_at {
        Some(dropped) => stamp_nanos(dropped),
        None => match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(now) => now.as_nanos() as i64,
            Err(_) => return 0,
        },
    };
    (end - stamp_nanos(created)).max(0)
}

fn stamp_nanos(stamp: &prost_types::Timestamp) -> i64 {
    stamp.seconds * 1_000_000_000 + i64::from(stamp.nanos)
}

#[cfg(test)]
mod tests;
