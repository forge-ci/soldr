//! Tests for off-CPU / async profiling (#647).

use prost::Message as _;

use super::*;
use crate::profile::pprof::Profile;

/// Two tasks: one mostly waiting, one mostly working.
///
/// The split is the point — a CPU profile cannot tell them apart, and an
/// off-CPU profile exists to.
fn samples() -> Vec<TaskSample> {
    vec![
        TaskSample {
            spawn_stack: vec!["main".into(), "serve".into(), "fetch_upstream".into()],
            idle_nanos: 9_000_000_000,
            busy_nanos: 1_000_000,
            scheduled_nanos: 500_000,
            polls: 12,
            wakes: 11,
            name: "upstream-0".into(),
        },
        TaskSample {
            spawn_stack: vec!["main".into(), "serve".into(), "compute".into()],
            idle_nanos: 2_000_000,
            busy_nanos: 8_000_000_000,
            scheduled_nanos: 100_000,
            polls: 3,
            wakes: 2,
            name: "compute-0".into(),
        },
    ]
}

// --- bounds ---------------------------------------------------------------

#[test]
fn a_window_over_the_cap_is_clamped_to_it() {
    // Same ceiling as CPU profiling, for the same reason: a session an
    // operator can start and forget degrades a production process
    // indefinitely.
    assert_eq!(clamp_window(Duration::from_secs(3600)), MAX_DURATION);
    assert_eq!(clamp_window(Duration::from_secs(5)), Duration::from_secs(5));
}

#[test]
fn an_adapter_is_always_given_the_clamped_window_not_the_requested_one() {
    // Otherwise the profile would describe a period the operator did not ask
    // about, while the metadata claimed the shorter one.
    let mut seen = Duration::ZERO;
    let mut adapter = CustomAdapter::new(|window: Duration| {
        seen = window;
        samples()
    });
    let _ = profile(&mut adapter, Duration::from_secs(600));
    assert_eq!(seen, MAX_DURATION);
}

// --- the adapter contract -------------------------------------------------

#[test]
fn a_custom_producer_can_drive_the_whole_pipeline() {
    // The load-bearing test for the contract: if a caller-supplied closure
    // reaches pprof without any tokio involvement, then nothing downstream is
    // tokio-shaped.
    let mut adapter = CustomAdapter::new(|_| samples());
    let collected = profile(&mut adapter, Duration::from_secs(5)).expect("custom adapter");
    assert_eq!(collected.len(), 2);

    let decoded = Profile::decode(to_pprof(&collected).as_slice()).expect("pprof must decode");
    assert_eq!(decoded.sample.len(), 2);
}

#[test]
fn an_adapter_that_produced_nothing_says_so_rather_than_returning_an_empty_profile() {
    // An empty profile and a genuinely idle program look identical once drawn.
    let mut adapter = CustomAdapter::new(|_| Vec::new());
    let error = profile(&mut adapter, Duration::from_secs(7)).expect_err("must not be empty-ok");
    assert_eq!(
        error,
        AsyncUnavailable::NoData {
            adapter: "custom",
            seconds: 7
        }
    );
    assert!(error.to_string().contains("look identical"));
}

#[cfg(feature = "tokio-console")]
#[test]
fn the_tokio_adapter_names_the_missing_instrumentation_not_just_the_refused_connection() {
    // A missing `console_subscriber::init()` is the overwhelmingly common
    // cause and is not something a connection refusal makes obvious.
    let mut adapter = AsyncEngineAdapter::default();
    let error = adapter
        .collect(Duration::from_secs(5))
        .expect_err("no subscriber is listening in a test");
    let text = error.to_string();
    assert!(text.contains("console_subscriber::init()"));
    assert!(text.contains("tokio_unstable"));
    assert!(
        text.contains("6669"),
        "the default endpoint should be named"
    );
}

// --- pprof lowering -------------------------------------------------------

#[test]
fn the_pprof_carries_all_five_value_types() {
    let decoded = Profile::decode(to_pprof(&samples()).as_slice()).expect("decode");
    let names: Vec<&str> = decoded
        .sample_type
        .iter()
        .map(|t| decoded.string_table[t.r#type as usize].as_str())
        .collect();
    assert_eq!(names, vec!["idle", "busy", "scheduled", "polls", "wakes"]);

    let units: Vec<&str> = decoded
        .sample_type
        .iter()
        .map(|t| decoded.string_table[t.unit as usize].as_str())
        .collect();
    assert_eq!(
        units,
        vec![
            "nanoseconds",
            "nanoseconds",
            "nanoseconds",
            "count",
            "count"
        ]
    );
}

#[test]
fn the_default_view_is_idle_time() {
    // Someone reaching for an off-CPU profile is asking what is *waiting*;
    // opening on busy time would show them the CPU profile they already had.
    let decoded = Profile::decode(to_pprof(&samples()).as_slice()).expect("decode");
    assert_eq!(
        decoded.string_table[decoded.default_sample_type as usize],
        "idle"
    );
}

#[test]
fn sample_values_are_in_declared_order() {
    let decoded = Profile::decode(to_pprof(&samples()).as_slice()).expect("decode");
    assert_eq!(
        decoded.sample[0].value,
        vec![9_000_000_000, 1_000_000, 500_000, 12, 11]
    );
}

#[test]
fn the_spawn_chain_is_stored_leaf_first_as_pprof_requires() {
    let decoded = Profile::decode(to_pprof(&samples()).as_slice()).expect("decode");
    let leaf = decoded
        .location
        .iter()
        .find(|l| l.id == decoded.sample[0].location_id[0])
        .expect("leaf location");
    let function = decoded
        .function
        .iter()
        .find(|f| f.id == leaf.line[0].function_id)
        .expect("leaf function");
    // The spawn chain is root-first (`main` → `serve` → `fetch_upstream`), so
    // the pprof leaf is the innermost spawn site.
    assert_eq!(
        decoded.string_table[function.name as usize],
        "fetch_upstream"
    );
}

#[test]
fn a_task_name_is_a_label_not_a_frame() {
    // As a frame it would give every task instance its own column and defeat
    // the grouping the graph exists for.
    let decoded = Profile::decode(to_pprof(&samples()).as_slice()).expect("decode");
    let label = &decoded.sample[0].label[0];
    assert_eq!(decoded.string_table[label.key as usize], "task");
    assert_eq!(decoded.string_table[label.str as usize], "upstream-0");

    let frames: Vec<&str> = decoded
        .function
        .iter()
        .map(|f| decoded.string_table[f.name as usize].as_str())
        .collect();
    assert!(!frames.contains(&"upstream-0"));
}

#[test]
fn a_task_with_no_name_gets_no_empty_label() {
    let mut sample = samples();
    sample[0].name = String::new();
    let decoded = Profile::decode(to_pprof(&sample).as_slice()).expect("decode");
    assert!(decoded.sample[0].label.is_empty());
}

#[test]
fn the_string_table_still_starts_with_the_empty_string() {
    let decoded = Profile::decode(to_pprof(&samples()).as_slice()).expect("decode");
    assert_eq!(decoded.string_table[0], "");
}

// --- collapsed / flame graph ---------------------------------------------

#[test]
fn collapsed_output_is_weighted_by_idle_time() {
    // The waiting task must dominate, even though the other one used far more
    // CPU. That inversion is the entire point of an off-CPU profile.
    let text = to_collapsed(&samples());
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "main;serve;fetch_upstream 9000000000");
    assert_eq!(lines[1], "main;serve;compute 2000000");
}

#[test]
fn a_task_that_never_waited_is_not_drawn_in_the_idle_view() {
    let samples = vec![TaskSample {
        spawn_stack: vec!["main".into()],
        idle_nanos: 0,
        busy_nanos: 5_000_000,
        ..TaskSample::default()
    }];
    assert_eq!(to_collapsed(&samples), "");
}

#[test]
fn tasks_sharing_a_spawn_site_are_folded_together() {
    // Grouping by spawn site is what makes this readable: a thousand instances
    // of one task should be one wide box, not a thousand slivers.
    let samples = vec![
        TaskSample {
            spawn_stack: vec!["main".into(), "worker".into()],
            idle_nanos: 100,
            name: "w-0".into(),
            ..TaskSample::default()
        },
        TaskSample {
            spawn_stack: vec!["main".into(), "worker".into()],
            idle_nanos: 400,
            name: "w-1".into(),
            ..TaskSample::default()
        },
    ];
    assert_eq!(to_collapsed(&samples), "main;worker 500\n");
}

#[test]
fn a_semicolon_in_a_spawn_location_cannot_forge_a_frame() {
    let samples = vec![TaskSample {
        spawn_stack: vec!["src/a;b.rs:10".into()],
        idle_nanos: 5,
        ..TaskSample::default()
    }];
    let text = to_collapsed(&samples);
    assert!(text.contains("src/a:b.rs:10"));
    assert!(!text.contains("a;b.rs"));
}

#[test]
fn a_sample_with_no_spawn_stack_is_skipped_rather_than_rooted_anonymously() {
    let samples = vec![TaskSample {
        spawn_stack: Vec::new(),
        idle_nanos: 900,
        ..TaskSample::default()
    }];
    assert_eq!(to_collapsed(&samples), "");
}

// --- unavailability -------------------------------------------------------

#[test]
fn an_unknown_adapter_lists_what_does_exist() {
    let error = AsyncUnavailable::UnknownAdapter {
        requested: "asyncio-typo".into(),
        available: "tokio, asyncio, custom".into(),
    };
    let text = error.to_string();
    assert!(text.contains("asyncio-typo"));
    assert!(text.contains("tokio, asyncio, custom"));
}
