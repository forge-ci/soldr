//! Unit tests for the console-api → `TaskSample` mapping (#788).
//!
//! These drive the pure joining/derivation logic. The live subscription is
//! covered end-to-end by `tests/profile_async_tokio_test.rs`, which runs a
//! real instrumented fixture.

use std::collections::HashMap;

use console_api::tasks::{Stats, Task};
use console_api::{Id, Location, PollStats};

use super::*;

fn stamp(seconds: i64) -> prost_types::Timestamp {
    prost_types::Timestamp { seconds, nanos: 0 }
}

fn dur(seconds: i64) -> prost_types::Duration {
    prost_types::Duration { seconds, nanos: 0 }
}

/// A task that lived 10s, was busy 1s and scheduled 1s — so idle 8s.
fn stats_for(created: i64, dropped: i64, busy: i64, scheduled: i64) -> Stats {
    Stats {
        created_at: Some(stamp(created)),
        dropped_at: Some(stamp(dropped)),
        scheduled_time: Some(dur(scheduled)),
        poll_stats: Some(PollStats {
            polls: 3,
            busy_time: Some(dur(busy)),
            ..Default::default()
        }),
        wakes: 7,
        ..Default::default()
    }
}

fn task_at(id: u64, file: &str, line: u32) -> Task {
    Task {
        id: Some(Id { id }),
        location: Some(Location {
            file: Some(file.to_string()),
            line: Some(line),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn idle_is_what_is_left_after_busy_and_scheduled() {
    let mut stats = HashMap::new();
    stats.insert(1, stats_for(100, 110, 1, 1));
    let mut spawned = HashMap::new();
    spawned.insert(1, "src/main.rs:12".to_string());

    let samples = join(spawned, stats);
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].idle_nanos, 8_000_000_000);
    assert_eq!(samples[0].busy_nanos, 1_000_000_000);
    assert_eq!(samples[0].scheduled_nanos, 1_000_000_000);
    assert_eq!(samples[0].polls, 3);
    assert_eq!(samples[0].wakes, 7);
}

#[test]
fn idle_never_goes_negative() {
    // The three figures are sampled at slightly different instants, so a task
    // polled as the window closed can report more busy time than lifetime.
    // A negative idle would render as a nonsense flame graph weight.
    let mut stats = HashMap::new();
    stats.insert(1, stats_for(100, 101, 5, 5));
    let samples = join(HashMap::new(), stats);
    assert_eq!(samples[0].idle_nanos, 0);
}

#[test]
fn a_task_with_no_metadata_still_produces_a_sample() {
    // Stats can arrive for a task whose `new_tasks` message we missed, e.g.
    // one spawned before the subscription opened. Dropping it would silently
    // omit exactly the long-lived tasks a profile is most interested in.
    let mut stats = HashMap::new();
    stats.insert(42, stats_for(0, 10, 1, 0));
    let samples = join(HashMap::new(), stats);
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].name, "task#42");
}

#[test]
fn the_spawn_site_names_the_task() {
    let task = task_at(1, "src/worker.rs", 88);
    assert_eq!(describe(&task), "src/worker.rs:88");
}

#[test]
fn a_task_without_a_location_falls_back_to_its_id() {
    let task = Task {
        id: Some(Id { id: 9 }),
        ..Default::default()
    };
    assert_eq!(describe(&task), "task#9");
}

#[test]
fn samples_come_back_in_a_stable_order() {
    // Stats arrive in a HashMap, so without sorting two profiles of the same
    // program would diff as though everything moved.
    let mut stats = HashMap::new();
    stats.insert(1, stats_for(0, 10, 1, 0));
    stats.insert(2, stats_for(0, 10, 1, 0));
    stats.insert(3, stats_for(0, 10, 1, 0));
    let mut spawned = HashMap::new();
    spawned.insert(1, "c.rs:1".to_string());
    spawned.insert(2, "a.rs:1".to_string());
    spawned.insert(3, "b.rs:1".to_string());

    let names: Vec<String> = join(spawned, stats).into_iter().map(|s| s.name).collect();
    assert_eq!(names, vec!["a.rs:1", "b.rs:1", "c.rs:1"]);
}

#[test]
fn a_still_running_task_is_measured_to_now_not_to_zero() {
    // `dropped_at` is absent while the task is alive. Treating that as 0 would
    // make every live task report zero lifetime and therefore zero idle —
    // hiding precisely the tasks that are stuck.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    let mut stats = HashMap::new();
    stats.insert(
        1,
        Stats {
            created_at: Some(stamp(now - 5)),
            dropped_at: None,
            poll_stats: Some(PollStats::default()),
            ..Default::default()
        },
    );
    let samples = join(HashMap::new(), stats);
    assert!(
        samples[0].idle_nanos > 4_000_000_000,
        "a live task idle for ~5s reported {}ns",
        samples[0].idle_nanos
    );
}

#[test]
fn the_runtimes_own_worker_tasks_are_not_reported() {
    // A multi-thread runtime parks one blocking-pool worker per core, each
    // idle essentially always. Measured on the fixture: 16 of 19 tasks were
    // these, all with identical idle time, ranked above the deliberately
    // blocked task the profile exists to surface.
    let mut stats = HashMap::new();
    stats.insert(1, stats_for(0, 100, 0, 0));
    stats.insert(2, stats_for(0, 100, 1, 0));
    let mut spawned = HashMap::new();
    spawned.insert(
        1,
        "/root/.cargo/registry/src/index.crates.io-x/tokio-1.53.1/src/runtime/scheduler/multi_thread/worker.rs:514"
            .to_string(),
    );
    spawned.insert(2, "src/blocked.rs:57".to_string());

    let names: Vec<String> = join(spawned, stats).into_iter().map(|s| s.name).collect();
    assert_eq!(names, vec!["src/blocked.rs:57"]);
}

#[test]
fn a_windows_style_runtime_path_is_also_recognised() {
    // The path arrives with whatever separators the profiled process used.
    assert!(is_runtime_internal(
        r".\.cargo\registry\src\index.crates.io-x\tokio-1.53.1\src\runtime\scheduler\multi_thread\worker.rs:514"
    ));
}

#[test]
fn an_application_module_named_after_tokio_is_kept() {
    // Matching the bare word "tokio" would hide the caller's own code. Only
    // the crate's source tree counts as runtime-internal.
    assert!(!is_runtime_internal("src/tokio_workers.rs:12"));
    assert!(!is_runtime_internal("src/runtime/mine.rs:12"));
    assert!(!is_runtime_internal("/app/tokio-helpers/src/lib.rs:3"));
}

#[tokio::test(flavor = "multi_thread")]
async fn synchronous_adapter_is_safe_inside_a_tokio_runtime() {
    let result = collect("http://127.0.0.1:1", std::time::Duration::from_millis(10));
    assert!(result.is_err());
}
