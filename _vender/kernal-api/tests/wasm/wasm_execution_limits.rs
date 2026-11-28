#![cfg(feature = "wasm-sketch-host")]

use kernal_api::wasm::{SketchCompiler, SketchCompilerConfig};

/// #41's reservation must be compiler-owned rather than an observation of a
/// particular Store.  This deliberately fails before the ledger facade exists.
#[test]
fn compiler_exposes_an_empty_global_execution_ledger_before_any_preparation() {
    let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
    let snapshot = compiler.execution_limits_snapshot();

    assert_eq!(snapshot.reserved_shared_memory_bytes(), 0);
    assert_eq!(snapshot.active_root_executions(), 0);
    assert_eq!(snapshot.live_guest_threads(), 0);
    assert_eq!(snapshot.live_stores(), 0);
    assert_eq!(snapshot.live_instances(), 0);
}
