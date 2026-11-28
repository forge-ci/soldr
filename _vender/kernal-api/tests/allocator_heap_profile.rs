//! Proves `kernal_api::allocator::dump_to_vec` against a real sampled
//! allocation, the way zccache's `heap_profile_test.rs` proved
//! `mimalloc_pprof::prof::dump_proto_to_vec` before this facade existed.
//!
//! An integration test is a separate final executable, so its allocator
//! declaration also guards the documented downstream linkage pattern: the
//! embedding binary declares `#[global_allocator]`, this crate supplies the
//! type and the profiler controls.

#![cfg(feature = "allocator")]

use kernal_api::allocator::{
    dump_file, dump_to_vec, is_enabled, live_sample_count, start, stats, stop, Allocator,
};

#[global_allocator]
static GLOBAL: Allocator = Allocator::new();

#[test]
fn dump_to_vec_captures_a_sampled_allocation() {
    if is_enabled() {
        stop();
    }

    // A dormant dump still serializes the pprof scaffolding — sample types,
    // mapping, string table — so a non-empty buffer proves nothing on its own.
    // Measure that floor first and require the sampled dump to exceed it.
    let baseline = dump_to_vec().len();

    assert!(start(1), "heap profiler should start exactly once");

    let retained = vec![0x5a_u8; 1024 * 1024];
    std::hint::black_box(&retained);

    assert!(
        live_sample_count() > 0,
        "retained allocation was not sampled"
    );

    let snapshot = dump_to_vec();

    // The owned counter snapshot mirrors the sampler and carries the exact
    // allocator counters alongside it.
    let counters = stats();
    assert!(counters.enabled());
    assert_eq!(counters.sample_rate(), 1);
    assert!(counters.live_samples() > 0);
    assert!(counters.live_bytes() >= retained.len());
    assert!(counters.dropped_samples() >= counters.stack_table_overflows());
    let heap = counters.heap();
    assert!(heap.committed() >= retained.len(), "{heap:?}");
    assert!(heap.reserved() >= heap.committed(), "{heap:?}");

    // The text dump is the legacy heap-profile format.
    let directory = tempfile::tempdir().unwrap();
    let text_path = directory.path().join("heap.prof");
    dump_file(&text_path).unwrap();
    let text = std::fs::read_to_string(&text_path).unwrap();
    assert!(text.starts_with("heap profile:"), "{text:.80}");
    assert!(text.contains("MAPPED_LIBRARIES:"));
    stop();
    assert!(!stats().enabled());

    assert!(
        snapshot.len() > baseline,
        "pprof snapshot ({} bytes) must carry the sampled allocation beyond \
         the dormant scaffolding ({baseline} bytes)",
        snapshot.len()
    );
}
