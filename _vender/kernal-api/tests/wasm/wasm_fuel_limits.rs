#![cfg(feature = "wasm-sketch-host")]

use kernal_api::wasm::{SketchCompiler, SketchCompilerConfig};

/// #42 requires an explicit deterministic budget that can be partitioned by a
/// root and every potential child before either Store executes guest code.
#[test]
fn compiler_exposes_a_nonzero_root_and_child_fuel_partition() {
    let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
    let fuel = compiler.execution_limits().fuel_limits();

    assert!(fuel.total() > 0);
    assert!(fuel.root_slice() > 0);
    assert!(fuel.child_slice() > 0);
    assert!(fuel.total() >= fuel.root_slice() + fuel.child_slice());
}
