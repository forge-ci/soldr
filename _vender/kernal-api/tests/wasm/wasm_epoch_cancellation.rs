#![cfg(feature = "wasm-sketch-host")]

//! Contract coverage for issue #43.  The looping artifacts belong in the
//! private host tests; this integration test keeps the public facade shape
//! explicit without exposing Wasmtime objects.

use std::time::Duration;

use kernal_api::async_engine::{CancellationSource, RuntimeBuilder};
use kernal_api::wasm::{
    SketchCompiler, SketchCompilerConfig, SketchEpochLimits, SketchExecutionError,
    SketchExecutionLimits, SketchFuelLimits, SketchModulePolicy,
};

#[test]
fn epoch_limits_are_bounded_and_cancellation_is_facade_owned() {
    let limits = SketchEpochLimits::new(Duration::from_millis(10), Duration::from_millis(1), 2)
        .expect("finite nonzero epoch limits");
    assert_eq!(limits.maximum_active_registrations(), 2);

    let config = SketchCompilerConfig::default()
        .with_epoch_limits(limits)
        .expect("epoch limits are part of compiler execution policy");
    assert_eq!(config.execution_limits().epoch_limits(), limits);

    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime
        .run_within(
            async {
                let first =
                    kernal_api::async_engine::RuntimeHandle::current().expect("current runtime");
                let second =
                    kernal_api::async_engine::RuntimeHandle::current().expect("same runtime");
                assert_eq!(
                    first, second,
                    "current handles retain live-runtime identity"
                );
                let task = first.launch_blocking(|| 7_u8);
                assert_eq!(task.await.expect("blocking task"), 7);
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");

    let cancellation = CancellationSource::new();
    assert!(!cancellation.token().is_cancelled());
    cancellation.cancel();
    assert!(cancellation.token().is_cancelled());
}

#[test]
fn public_controlled_execution_cancels_a_running_compute_loop_and_cleans_up() {
    let fuel = SketchFuelLimits::new(1_700_000_000_000, 100_000_000_000, 100_000_000_000)
        .expect("high fuel leaves epoch cancellation observable");
    let epoch = SketchEpochLimits::new(Duration::from_secs(1), Duration::from_millis(1), 17)
        .expect("finite epoch policy");
    let limits = SketchExecutionLimits::default()
        .with_fuel_limits(fuel)
        .expect("fuel policy")
        .with_epoch_limits(epoch)
        .expect("epoch policy");
    let compiler = SketchCompiler::new(
        SketchCompilerConfig::default()
            .with_execution_limits(limits)
            .expect("compiler policy"),
    )
    .expect("compiler");
    let bytes = crate::threaded_fixture::looping_root_wasm();
    let sketch = compiler
        .admit(
            &bytes,
            SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, 16_384).expect("policy"),
        )
        .expect("admission");
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime
        .run_within(
            async {
                let source = CancellationSource::new();
                let task = runtime.handle().launch({
                    let sketch = sketch.clone();
                    let token = source.token();
                    let handle = runtime.handle();
                    async move {
                        sketch
                            .execute_threaded_root_cancellable(handle, token)
                            .await
                    }
                });
                kernal_api::async_engine::sleep(Duration::from_millis(2)).await;
                source.cancel();
                assert_eq!(
                    task.await.expect("execution task"),
                    Err(SketchExecutionError::Cancelled)
                );
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    sketch
        .close_threaded_root()
        .expect("close cooperative sketch");
    assert_eq!(compiler.execution_limits_snapshot(), Default::default());
}
