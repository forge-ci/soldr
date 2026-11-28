//! Tests for CPU profiling and its exports (#644).

use std::sync::atomic::Ordering;
use std::time::Duration;

use prost::Message as _;

use super::export::firefox::to_firefox_value;
use super::export::{to_collapsed, to_pprof_bytes, to_pprof_gzip};
use super::symbolize::{Frame, TableResolver};
use super::*;

// --- bounds ---------------------------------------------------------------

#[test]
fn a_duration_over_the_cap_is_clamped_to_it() {
    // The acceptance criterion, and the one bound that is not negotiable: an
    // unbounded session is one an operator can start, forget, and leave
    // degrading a production process.
    let request = ProfileRequest {
        hz: DEFAULT_HZ,
        duration: Duration::from_secs(300),
    };
    assert_eq!(request.clamped().duration, MAX_DURATION);
    assert!(request.was_clamped());
}

#[test]
fn a_duration_under_the_cap_is_left_alone() {
    let request = ProfileRequest {
        hz: DEFAULT_HZ,
        duration: Duration::from_secs(5),
    };
    assert_eq!(request.clamped().duration, Duration::from_secs(5));
    assert!(!request.was_clamped());
}

#[test]
fn a_frequency_outside_the_supported_range_is_clamped() {
    let too_fast = ProfileRequest {
        hz: 100_000,
        duration: Duration::from_secs(1),
    };
    assert_eq!(too_fast.clamped().hz, MAX_HZ);

    let too_slow = ProfileRequest {
        hz: 0,
        duration: Duration::from_secs(1),
    };
    assert_eq!(too_slow.clamped().hz, MIN_HZ);
}

#[test]
fn the_period_follows_the_clamped_frequency() {
    let request = ProfileRequest {
        hz: 100,
        duration: Duration::from_secs(1),
    };
    assert_eq!(request.period_nanos(), 10_000_000);

    // A zero-hz request would divide by zero if the clamp were applied after
    // the division rather than before it.
    let zero = ProfileRequest {
        hz: 0,
        duration: Duration::from_secs(1),
    };
    assert_eq!(zero.period_nanos(), 1_000_000_000);
}

#[test]
fn the_default_frequency_is_not_a_round_number() {
    // 99 rather than 100 so the sampler drifts across anything else on a
    // 100 Hz timer instead of phase-locking with it and reporting a periodic
    // artifact as a hot path.
    assert_eq!(DEFAULT_HZ, 99);
}

// --- ingest ---------------------------------------------------------------

fn sample(tid: u64, stack: &[u64]) -> RawSample {
    RawSample {
        os_tid: tid,
        since_start_nanos: 0,
        stack: stack.to_vec(),
        truncated: false,
    }
}

#[test]
fn a_full_ring_drops_and_counts_rather_than_growing() {
    // Backpressure must never reach the sampler: blocking would push the
    // profiler's cost onto the profiled program, and growing would let a slow
    // consumer turn a profile into an OOM.
    let ring = SampleRing::with_capacity(2);
    assert!(ring.push(sample(1, &[10])));
    assert!(ring.push(sample(1, &[20])));
    assert!(!ring.push(sample(1, &[30])));

    assert_eq!(ring.len(), 2);
    assert_eq!(ring.dropped(), 1);
    assert_eq!(ring.accepted(), 2);
}

#[test]
fn draining_empties_the_ring_but_not_its_lifetime_counters() {
    // `accepted` is the denominator of the fidelity figure, so it has to
    // survive a drain — otherwise a long session that drained once would
    // report a fidelity computed from the last few samples only.
    let ring = SampleRing::with_capacity(4);
    ring.push(sample(1, &[10]));
    ring.push(sample(2, &[20]));

    assert_eq!(ring.drain().len(), 2);
    assert!(ring.is_empty());
    assert_eq!(ring.accepted(), 2);
}

// --- metrics --------------------------------------------------------------

#[test]
fn coverage_overhead_and_fidelity_are_ratios_of_what_was_observed() {
    let metrics = ProfileMetrics {
        samples_captured: 90,
        samples_dropped: 10,
        threads_seen: 3,
        threads_at_start: 4,
        pause_nanos: 1_000_000,
        duration_nanos: 100_000_000,
        hz: 99,
        clamped: false,
        buffer_full: false,
    };
    assert_eq!(metrics.thread_coverage(), 0.75);
    assert_eq!(metrics.overhead_ratio(), 0.01);
    assert_eq!(metrics.fidelity(), 0.9);
}

#[test]
fn metrics_over_an_empty_session_do_not_divide_by_zero() {
    let metrics = ProfileMetrics::default();
    assert_eq!(metrics.thread_coverage(), 0.0);
    assert_eq!(metrics.overhead_ratio(), 0.0);
    // Nothing was offered, so nothing was lost.
    assert_eq!(metrics.fidelity(), 1.0);
}

// --- sessions -------------------------------------------------------------

#[test]
fn a_clamped_request_is_reported_as_clamped() {
    // The whole point of the field: an operator who typed `--hz 5000
    // --duration 300` gets a different session than they asked for, and the
    // one marker that says so must not assert that nothing was substituted.
    let session = ProfileSession::new(ProfileRequest {
        hz: 5000,
        duration: Duration::from_secs(300),
    });
    // Stopped before the first tick. The loop tests the flag on entry, so the
    // metrics are produced without running the clamped sixty seconds.
    session.stop_handle().store(true, Ordering::Relaxed);

    let metrics = session.run();
    assert!(metrics.clamped);
    assert_eq!(metrics.hz, MAX_HZ);
}

#[test]
fn a_request_already_inside_the_bounds_is_not_reported_as_clamped() {
    let session = ProfileSession::new(ProfileRequest {
        hz: DEFAULT_HZ,
        duration: Duration::from_millis(20),
    });
    session.stop_handle().store(true, Ordering::Relaxed);

    let metrics = session.run();
    assert!(!metrics.clamped);
    assert_eq!(metrics.hz, DEFAULT_HZ);
}

#[test]
fn a_session_whose_ring_is_full_stops_instead_of_sampling_into_it() {
    // The ring's budget is hz × seconds × threads, not one sample per tick,
    // so a many-threaded target exhausts it mid-window. Nothing drains it
    // until the session ends, so carrying on would suspend every sibling
    // thread on every remaining tick to produce samples that are discarded on
    // arrival -- and the profile would describe the start of the window while
    // `duration_nanos` claimed all of it.
    let session = ProfileSession::with_ring_capacity(
        ProfileRequest {
            hz: DEFAULT_HZ,
            duration: Duration::from_secs(5),
        },
        8,
    );
    for tid in 0..8 {
        assert!(session.ring().push(sample(tid, &[0x1000])));
    }

    let metrics = session.run();

    assert!(metrics.buffer_full, "a full ring must be reported as one");
    assert_eq!(metrics.samples_captured, 8);
    // Five seconds were asked for; the run must not have spent them.
    assert!(
        metrics.duration_nanos < Duration::from_secs(1).as_nanos() as u64,
        "a session with nowhere to put samples ran for {} ns",
        metrics.duration_nanos,
    );
}

/// Real thread sampling fills the small ring with far fewer threads than the
/// default capacity assumes.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
#[test]
fn a_handful_of_threads_fills_a_small_ring() {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Arc;

    const CHILD: &str = "KERNAL_SMALL_RING_TEST_PARENT";
    if let Some(parent) = std::env::var_os(CHILD) {
        assert_ne!(
            parent,
            std::ffi::OsString::from(std::process::id().to_string())
        );
    } else {
        // A profile samples every sibling in its process. Parallel libtest
        // cases add unrelated threads and mapping changes, so their unwind
        // cost can consume this fixture's sampling window (#408).
        // Isolate this one real four-worker proof; the suite stays parallel.
        let output = crate::run_bounded_command(
            crate::SpawnSpec::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "profile::tests::a_handful_of_threads_fills_a_small_ring",
                ])
                .env(CHILD, std::process::id().to_string()),
            Duration::from_secs(35),
            65536,
        )
        .expect("isolated four-worker fixture must finish");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.exit.is_success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
        return;
    }

    const WORKERS: usize = 4;

    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicUsize::new(0));
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let stop = Arc::clone(&stop);
            let ready = Arc::clone(&ready);
            std::thread::spawn(move || {
                ready.fetch_add(1, Ordering::Release);
                while !stop.load(Ordering::Acquire) {
                    // Stay sampleable without saturating the runner and starving the sampler.
                    std::thread::park_timeout(Duration::from_millis(10));
                }
            })
        })
        .collect();
    // Every worker must be on a stack of its own before the first tick, or
    // the session would be sampling fewer threads than the test claims.
    while ready.load(Ordering::Acquire) < WORKERS {
        std::thread::yield_now();
    }

    let session = ProfileSession::with_ring_capacity(
        ProfileRequest {
            hz: DEFAULT_HZ,
            // A first snapshot can take several seconds on a cold runner.
            // Leave room for another tick while retaining a bounded proof.
            duration: Duration::from_secs(25),
        },
        8,
    );
    let metrics = session.run();

    stop.store(true, Ordering::Release);
    for worker in workers {
        worker.join().unwrap();
    }

    assert!(
        metrics.buffer_full,
        "four threads must fill a ring of eight: {metrics:?}"
    );
    assert_eq!(metrics.samples_captured, 8);
}

// --- folding --------------------------------------------------------------

/// A session where `hot` dominates: 8 samples in it, 2 elsewhere.
fn hot_session() -> SessionResult {
    let frame = |name: &str| Frame {
        function: name.to_string(),
        module: "fixture".to_string(),
        relative_address: 0,
    };
    let mut samples = Vec::new();
    for _ in 0..8 {
        samples.push(ResolvedSample {
            os_tid: 1,
            // Leaf first, as captured.
            frames: vec![frame("spin_hot"), frame("main")],
            truncated: false,
        });
    }
    for _ in 0..2 {
        samples.push(ResolvedSample {
            os_tid: 1,
            frames: vec![frame("setup"), frame("main")],
            truncated: false,
        });
    }
    SessionResult {
        samples,
        metrics: ProfileMetrics {
            duration_nanos: 1_000_000_000,
            hz: 99,
            ..ProfileMetrics::default()
        },
        start_unix_nanos: 1_700_000_000_000_000_000,
        period_nanos: 10_101_010,
    }
}

#[test]
fn folding_merges_identical_stacks_and_orders_by_weight() {
    let folded = hot_session().folded();
    assert_eq!(folded.len(), 2);
    // Root first, and the hot stack first.
    assert_eq!(folded[0].0, vec!["main", "spin_hot"]);
    assert_eq!(folded[0].1, 8);
    assert_eq!(folded[1].0, vec!["main", "setup"]);
    assert_eq!(folded[1].1, 2);
}

#[test]
fn a_sample_with_no_frames_is_skipped_rather_than_folded_as_a_root() {
    let mut session = hot_session();
    session.samples.push(ResolvedSample {
        os_tid: 9,
        frames: Vec::new(),
        truncated: false,
    });
    // An empty stack folded as a root would appear as a mysterious top-level
    // entry with no name.
    assert_eq!(session.folded().len(), 2);
}

// --- collapsed export -----------------------------------------------------

#[test]
fn collapsed_output_is_the_folded_format_hottest_first() {
    let text = to_collapsed(&hot_session());
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "main;spin_hot 8");
    assert_eq!(lines[1], "main;setup 2");
}

#[test]
fn a_semicolon_in_a_frame_name_cannot_forge_a_frame() {
    // The collapsed format has no escape syntax, so a semicolon inside a name
    // would create a phantom frame and reparent everything below it.
    let session = SessionResult {
        samples: vec![ResolvedSample {
            os_tid: 1,
            frames: vec![Frame {
                function: "evil;injected".into(),
                module: String::new(),
                relative_address: 0,
            }],
            truncated: false,
        }],
        ..hot_session()
    };
    let text = to_collapsed(&session);
    assert!(text.contains("evil:injected 1"), "got {text:?}");
    assert!(!text.contains("evil;injected"));
}

// --- pprof export ---------------------------------------------------------

#[test]
fn the_pprof_string_table_starts_with_the_empty_string() {
    // A spec invariant: index 0 must be "", because 0 is also how every
    // optional string field says "unset". Anything else there mis-resolves
    // every unset field in the profile.
    let profile = super::export::pprof::build(&hot_session());
    assert_eq!(profile.string_table[0], "");
}

#[test]
fn a_pprof_profile_round_trips_and_keeps_its_hot_stack() {
    let bytes = to_pprof_bytes(&hot_session());
    let decoded = pprof::Profile::decode(bytes.as_slice()).expect("pprof must decode");

    // Two value types: a count and the wall time it stands for.
    assert_eq!(decoded.sample_type.len(), 2);
    assert_eq!(
        decoded.string_table[decoded.sample_type[0].r#type as usize],
        "samples"
    );
    assert_eq!(
        decoded.string_table[decoded.sample_type[1].unit as usize],
        "nanoseconds"
    );
    assert_eq!(decoded.period, 10_101_010);
    assert_eq!(decoded.sample.len(), 2);

    // The hot sample carries 8 samples, and 8 periods of attributed time.
    let hot = &decoded.sample[0];
    assert_eq!(hot.value[0], 8);
    assert_eq!(hot.value[1], 8 * 10_101_010);

    // pprof wants leaf-first, so the first location is the hot leaf.
    let leaf_location = decoded
        .location
        .iter()
        .find(|location| location.id == hot.location_id[0])
        .expect("leaf location");
    let function = decoded
        .function
        .iter()
        .find(|f| f.id == leaf_location.line[0].function_id)
        .expect("leaf function");
    assert_eq!(decoded.string_table[function.name as usize], "spin_hot");
}

#[test]
fn a_pprof_export_is_gzipped_and_decompresses_to_the_same_profile() {
    use std::io::Read as _;
    let gz = to_pprof_gzip(&hot_session()).expect("gzip");
    // Gzip magic, so a viewer that sniffs the file recognizes `.pb.gz`.
    assert_eq!(&gz[..2], &[0x1f, 0x8b]);

    let mut decoder = flate2::read::GzDecoder::new(gz.as_slice());
    let mut raw = Vec::new();
    decoder.read_to_end(&mut raw).expect("gunzip");
    assert_eq!(raw, to_pprof_bytes(&hot_session()));
}

// --- firefox export -------------------------------------------------------

#[test]
fn firefox_json_carries_the_tables_the_profiler_reads() {
    let value = to_firefox_value(&hot_session());

    assert!(value["meta"]["interval"].as_f64().unwrap() > 0.0);
    let thread = &value["threads"][0];

    // Column-oriented: every table declares its own length, and the profiler
    // trusts that length over the array it is attached to.
    for table in ["samples", "stackTable", "frameTable", "funcTable"] {
        let length = thread[table]["length"].as_u64().expect("length");
        assert!(length > 0, "{table} is empty");
    }
    assert_eq!(
        thread["samples"]["stack"].as_array().map(Vec::len),
        thread["samples"]["length"].as_u64().map(|n| n as usize)
    );

    // The hot stack's weight is its fold count, not one row per sample.
    let weights: Vec<u64> = thread["samples"]["weight"]
        .as_array()
        .expect("weights")
        .iter()
        .map(|w| w.as_u64().unwrap_or(0))
        .collect();
    assert_eq!(weights, vec![8, 2]);
}

#[test]
fn firefox_stack_prefixes_form_a_tree_rooted_at_null() {
    let value = to_firefox_value(&hot_session());
    let prefixes = value["threads"][0]["stackTable"]["prefix"]
        .as_array()
        .expect("prefix column")
        .clone();
    // Exactly one root: `main`, shared by both stacks. Two roots would mean
    // the shared prefix was duplicated instead of merged.
    assert_eq!(prefixes.iter().filter(|p| p.is_null()).count(), 1);
}

#[test]
fn firefox_json_is_valid_json() {
    let text = super::export::to_firefox_json(&hot_session());
    serde_json::from_str::<serde_json::Value>(&text)
        .expect("the profiler must be able to parse it");
}

// --- exports agree --------------------------------------------------------

#[test]
fn all_three_exports_agree_on_which_stack_is_hottest() {
    // They are folded from one function, so disagreement would mean an
    // encoder rearranged something it should not have.
    let session = hot_session();
    let exports = super::export::all(&session).expect("exports");

    assert!(exports.collapsed.starts_with("main;spin_hot 8"));

    let profile = pprof::Profile::decode(to_pprof_bytes(&session).as_slice()).expect("decode");
    assert_eq!(profile.sample[0].value[0], 8);

    let firefox = to_firefox_value(&session);
    assert_eq!(
        firefox["threads"][0]["samples"]["weight"][0].as_u64(),
        Some(8)
    );

    assert!(!exports.pprof_gzip.is_empty());
}

// --- symbolization --------------------------------------------------------

#[test]
fn a_resolver_names_known_addresses_and_admits_to_unknown_ones() {
    use super::symbolize::FrameResolver as _;
    let mut resolver = TableResolver::default().with(0x1000, "known_function");
    assert_eq!(resolver.resolve(0x1000).function, "known_function");
    // An unknown address becomes its own hex value rather than a guess: a
    // profile that invented plausible names would be worse than one that
    // admits it could not resolve them.
    assert_eq!(resolver.resolve(0x2000).function, "0x2000");
}

#[test]
fn symbolization_happens_after_capture_not_during_it() {
    // The structural guarantee: a session resolves from a drained ring, so a
    // resolver can be slow (it parses debug info) without that cost ever
    // landing between two samples.
    let session = ProfileSession::new(ProfileRequest {
        hz: 99,
        duration: Duration::from_millis(1),
    });
    session.ring().push(sample(7, &[0x1000, 0x2000]));

    let mut resolver = TableResolver::default()
        .with(0x1000, "leaf")
        .with(0x2000, "caller");
    let result = session.resolve(&mut resolver, ProfileMetrics::default());

    assert_eq!(result.samples.len(), 1);
    assert_eq!(result.samples[0].frames[0].function, "leaf");
    assert_eq!(result.samples[0].frames[1].function, "caller");
    // Drained: the ring is not a second copy of the profile.
    assert!(session.ring().is_empty());
}
