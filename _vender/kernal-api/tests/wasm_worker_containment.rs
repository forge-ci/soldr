#![cfg(feature = "wasm-sketch-worker")]

//! Real-worker containment coverage for #28, including ignored inner helpers
//! and externally controlled crash/parent-death proofs on native Windows,
//! Linux, and macOS targets.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kernal_api::async_engine::{self, CancellationSource, RuntimeBuilder, RuntimeHandle};
use kernal_api::wasm::{
    SketchCompiler, SketchCompilerConfig, SketchEpochLimits, SketchExecutionError,
    SketchExecutionLimits, SketchFuelLimits, SketchModulePolicy, SketchWorkerConfig,
    SketchWorkerStopReason, SketchWorkerTerminal, ThreadedRootOutcome,
};

#[path = "support/threaded_fixture.rs"]
mod threaded_fixture;

const OUTER_BOUND: Duration = Duration::from_secs(10);
// Process launch and the first Wasmtime initialization are intentionally
// outside the sub-second deadline used to prove forced containment below.
// Keep ordinary guest outcomes on a generous bound so a cold Windows worker
// cannot be misclassified as a deadline expiry.
const WORKER_DEADLINE: Duration = Duration::from_secs(2);
const CONTAINMENT_DEADLINE: Duration = Duration::from_secs(1);
// The externally controlled crash and parent-death proofs must acquire an
// exact live native identity before their intentional action.  Keep their
// worker deadline beyond the outer acquisition bound so normal containment
// cannot race the proof into a false success.
#[cfg(feature = "wasm-sketch-worker-test-support")]
const FAILURE_PROOF_DEADLINE: Duration = Duration::from_secs(30);
const GRACE: Duration = Duration::from_secs(1);

/// Locate the worker executable in a way that survives a cross-built archive.
///
/// `env!("CARGO_BIN_EXE_…")` is resolved at compile time, so a test archive
/// built on one host carries that host's absolute path and the worker cannot
/// be spawned anywhere else. nextest exports `NEXTEST_BIN_EXE_<name>` at run
/// time for exactly this case, so prefer it and fall back to the compile-time
/// path for an ordinary `cargo test` run.
///
/// That fallback is `option_env!` rather than `env!` because Cargo defines
/// `CARGO_BIN_EXE_…` only where it actually produces the binary: `env!` makes
/// this file impossible to compile under `cargo check --all-targets`, which is
/// the lane the boundary lints run in. A check-only lane never runs the test,
/// so the `expect` here can only be reached from a run that had the path.
fn worker_executable() -> PathBuf {
    if let Some(exported) = std::env::var_os("NEXTEST_BIN_EXE_kernal-wasm-worker") {
        return PathBuf::from(exported);
    }
    match option_env!("CARGO_BIN_EXE_kernal-wasm-worker") {
        Some(path) => PathBuf::from(path),
        None => panic!("a cargo test run supplies the worker path"),
    }
}

fn worker_config() -> SketchWorkerConfig {
    let executable = worker_executable();
    assert!(
        executable.is_absolute(),
        "Cargo supplied an absolute worker path"
    );
    SketchWorkerConfig::new(executable, GRACE).expect("explicit worker configuration")
}

fn compiler(deadline: Duration, fuel: SketchFuelLimits) -> SketchCompiler {
    let epoch = SketchEpochLimits::new(deadline, Duration::from_millis(1), 17)
        .expect("one millisecond epoch tick");
    let limits = SketchExecutionLimits::default()
        .with_fuel_limits(fuel)
        .expect("fuel limits")
        .with_epoch_limits(epoch)
        .expect("epoch limits")
        .with_blob_limits(
            kernal_api::wasm::SketchBlobLimits::new(
                64 * 1024,
                1024 * 1024,
                2 * 1024 * 1024,
                // The real threaded smoke guest owns two independent child
                // blob scopes. Each can retain an awaited write while it
                // submits a second cancellation candidate.
                2,
                2,
                4,
            )
            .unwrap()
            .with_maximum_transfer_bytes(3 * 1024 * 1024)
            .unwrap(),
        );
    SketchCompiler::new(
        SketchCompilerConfig::default()
            .with_execution_limits(limits)
            .expect("execution limits"),
    )
    .expect("compiler")
}

fn normal_fuel() -> SketchFuelLimits {
    SketchFuelLimits::default()
}

fn long_fuel() -> SketchFuelLimits {
    SketchFuelLimits::new(1_700_000_000_000, 100_000_000_000, 100_000_000_000).expect("long fuel")
}

fn tiny_fuel() -> SketchFuelLimits {
    SketchFuelLimits::new(30_000, 10_000, 10_000).expect("tiny fuel")
}

fn admit(compiler: &SketchCompiler, bytes: Vec<u8>) -> Arc<kernal_api::wasm::AdmittedSketch> {
    compiler
        .admit(
            &bytes,
            SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, 16_384).expect("policy"),
        )
        .expect("admission")
}

async fn contained(
    sketch: &Arc<kernal_api::wasm::AdmittedSketch>,
    runtime: RuntimeHandle,
    config: &SketchWorkerConfig,
    cancellation: Option<CancellationSource>,
    outer_bound: Duration,
) -> SketchWorkerTerminal {
    let token = cancellation
        .as_ref()
        .map(CancellationSource::token)
        .unwrap_or_else(|| CancellationSource::new().token());
    async_engine::timeout(
        outer_bound,
        sketch.execute_threaded_root_contained_cancellable(runtime, config, token),
    )
    .await
    .expect("worker containment exceeded outer bound")
}

async fn assert_clean(compiler: &SketchCompiler, sketch: &Arc<kernal_api::wasm::AdmittedSketch>) {
    sketch.close_threaded_root().expect("close sketch");
    async_engine::timeout(OUTER_BOUND, async {
        loop {
            let worker = sketch.worker_execution_snapshot();
            if worker.live_workers == 0
                && worker.live_protocol_tasks == 0
                && worker.pending_root_leases == 0
            {
                break;
            }
            async_engine::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("worker cleanup exceeded outer bound");
    assert_eq!(compiler.execution_limits_snapshot(), Default::default());
    let worker = sketch.worker_execution_snapshot();
    assert_eq!(worker.live_workers, 0);
    assert_eq!(worker.live_protocol_tasks, 0);
    assert_eq!(worker.pending_root_leases, 0);
}

fn run_case(
    bytes: Vec<u8>,
    deadline: Duration,
    fuel: SketchFuelLimits,
    cancel: bool,
    expected: SketchWorkerTerminal,
) {
    run_case_with_outer_bound(bytes, deadline, fuel, cancel, expected, OUTER_BOUND);
}

fn run_case_with_outer_bound(
    bytes: Vec<u8>,
    deadline: Duration,
    fuel: SketchFuelLimits,
    cancel: bool,
    expected: SketchWorkerTerminal,
    outer_bound: Duration,
) {
    run_case_checking(bytes, deadline, fuel, cancel, outer_bound, |terminal| {
        assert_eq!(terminal, &expected);
    });
}

fn run_case_checking(
    bytes: Vec<u8>,
    deadline: Duration,
    fuel: SketchFuelLimits,
    cancel: bool,
    outer_bound: Duration,
    check: impl FnOnce(&SketchWorkerTerminal),
) {
    let compiler = compiler(deadline, fuel);
    let sketch = admit(&compiler, bytes);
    let config = worker_config();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime
        .run_within(
            async {
                let source = CancellationSource::new();
                let task =
                    runtime.handle().launch({
                        let sketch = Arc::clone(&sketch);
                        let config = config.clone();
                        let source = source.clone();
                        let handle = runtime.handle();
                        async move {
                            contained(&sketch, handle, &config, Some(source), outer_bound).await
                        }
                    });
                if cancel {
                    // Let the parent finish the bounded upload and the child enter
                    // Wasm before checking cooperative cancellation. A cancellation
                    // during protocol upload intentionally exercises forced cleanup.
                    async_engine::sleep(Duration::from_millis(500)).await;
                    source.cancel();
                }
                check(&task.await.expect("contained task"));
                assert_clean(&compiler, &sketch).await;
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
#[ignore = "requires the artifact built by scripts/build-threaded-smoke"]
fn cargo_built_threaded_guest_runs_inside_killable_worker() {
    let path = std::env::var_os("KERNAL_API_THREADED_ARTIFACT_WASM")
        .expect("explicit artifact proof must supply its Cargo-built Wasm");
    let bytes = std::fs::read(path).expect("read real threaded guest");
    // Intel macOS completes the real artifact in about 13 seconds under the
    // native screenshot job. Keep this real-worker smoke bounded, but leave
    // enough room for that supported host rather than misclassifying normal
    // execution as containment expiry.
    run_case_with_outer_bound(
        bytes,
        Duration::from_secs(20),
        long_fuel(),
        false,
        SketchWorkerTerminal::Completed(ThreadedRootOutcome::Started),
        Duration::from_secs(30),
    );
}

#[test]
#[ignore = "requires the artifact built by scripts/build-threaded-smoke"]
fn cargo_built_threaded_guest_deadline_stops_and_releases_parent_state() {
    let path = std::env::var_os("KERNAL_API_THREADED_ARTIFACT_WASM")
        .expect("explicit artifact proof must supply its Cargo-built Wasm");
    // The artifact spends longer than this deadline in its two child stream
    // pressure paths. Where the epoch deadline lands decides the terminal
    // (#274): the child usually misses it and the parent forces containment,
    // but it can also stop cooperatively and report the deadline itself. Both
    // are deadline stops; either way every parent lease and protocol task must
    // be released. Forced cleanup is proven deterministically by
    // cargo_built_threaded_guest_forced_output_cleanup.
    run_case_checking(
        std::fs::read(path).expect("read real threaded guest"),
        CONTAINMENT_DEADLINE,
        long_fuel(),
        false,
        Duration::from_secs(30),
        |terminal| {
            assert!(
                matches!(
                    terminal,
                    SketchWorkerTerminal::Stopped(SketchWorkerStopReason::DeadlineExceeded)
                        | SketchWorkerTerminal::ForcedContainment {
                            trigger: SketchWorkerStopReason::DeadlineExceeded,
                        }
                ),
                "the deadline must stop the guest, cooperatively or by force: {terminal:?}"
            );
        },
    );
}

#[test]
fn real_worker_classifies_normal_and_trap() {
    run_case(
        threaded_fixture::threaded_root_wasm(None, false, false, false),
        WORKER_DEADLINE,
        normal_fuel(),
        false,
        SketchWorkerTerminal::Completed(ThreadedRootOutcome::Started),
    );
    run_case(
        threaded_fixture::threaded_root_wasm(Some(0), false, false, false),
        WORKER_DEADLINE,
        normal_fuel(),
        false,
        SketchWorkerTerminal::Completed(ThreadedRootOutcome::Exited),
    );
    run_case(
        threaded_fixture::unreachable_root_wasm(),
        WORKER_DEADLINE,
        normal_fuel(),
        false,
        SketchWorkerTerminal::Execution(SketchExecutionError::Trapped),
    );
}

#[test]
#[ignore = "requires the artifact built by scripts/build-threaded-smoke"]
fn cargo_built_threaded_guest_commits_parent_owned_output() {
    let artifact =
        std::env::var_os("KERNAL_API_THREADED_ARTIFACT_WASM").expect("threaded artifact");
    let compiler = compiler(Duration::from_secs(20), long_fuel());
    let sketch = admit(&compiler, std::fs::read(artifact).unwrap());
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("output.png");
    std::fs::write(&destination, b"original").unwrap();
    let config = worker_config()
        .with_output_destination(destination.clone())
        .unwrap();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .run_within(
            async {
                let terminal = async_engine::timeout(
                    Duration::from_secs(30),
                    sketch.execute_threaded_root_contained(runtime.handle(), &config),
                )
                .await
                .unwrap();
                assert_eq!(
                    terminal,
                    SketchWorkerTerminal::Completed(ThreadedRootOutcome::Started)
                );
                assert_clean(&compiler, &sketch).await;
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    assert_eq!(std::fs::read(&destination).unwrap(), b"guest exact output");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn real_worker_classifies_fuel_cancellation_and_deadline() {
    run_case(
        threaded_fixture::looping_root_wasm(),
        Duration::from_secs(2),
        tiny_fuel(),
        false,
        SketchWorkerTerminal::Execution(SketchExecutionError::OutOfFuel),
    );
    run_case(
        threaded_fixture::looping_root_wasm(),
        Duration::from_secs(2),
        long_fuel(),
        true,
        SketchWorkerTerminal::Stopped(SketchWorkerStopReason::Cancelled),
    );
    run_case(
        threaded_fixture::looping_root_wasm(),
        CONTAINMENT_DEADLINE,
        long_fuel(),
        false,
        SketchWorkerTerminal::Stopped(SketchWorkerStopReason::DeadlineExceeded),
    );
}

#[cfg(feature = "wasm-sketch-worker-test-support")]
#[test]
#[ignore = "requires the real threaded artifact and test-support worker"]
fn cargo_built_threaded_guest_forced_output_cleanup() {
    let artifact =
        std::env::var_os("KERNAL_API_THREADED_ARTIFACT_WASM").expect("threaded artifact");
    let compiler = compiler(Duration::from_secs(30), long_fuel());
    let sketch = admit(&compiler, std::fs::read(artifact).unwrap());
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("output.png");
    std::fs::write(&destination, b"original").unwrap();
    let config = worker_config()
        .with_output_destination(destination.clone())
        .unwrap();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .run_within(
            async {
                let source = CancellationSource::new();
                let task = runtime.handle().launch({
                    let sketch = Arc::clone(&sketch);
                    let token = source.token();
                    let handle = runtime.handle();
                    async move {
                        sketch
                            .execute_threaded_root_contained_cancellable(handle, &config, token)
                            .await
                    }
                });
                let staging = async_engine::timeout(Duration::from_secs(5), async {
                    loop {
                        if let Some(path) = std::fs::read_dir(directory.path())
                            .unwrap()
                            .filter_map(Result::ok)
                            .map(|entry| entry.path())
                            .find(|path| path.is_dir())
                        {
                            break path;
                        }
                        async_engine::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await
                .expect("parent staging was created");
                std::fs::write(staging.join(".proof-pause-output"), b"armed").unwrap();
                async_engine::timeout(Duration::from_secs(20), async {
                    while !staging.join(".proof-output-paused").is_file() {
                        async_engine::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await
                .expect("worker reached an actual partial file write");
                assert!(
                    std::fs::read_dir(&staging)
                        .unwrap()
                        .filter_map(Result::ok)
                        .any(|entry| {
                            !entry.file_name().to_string_lossy().starts_with(".proof-")
                                && std::fs::read(entry.path())
                                    .is_ok_and(|bytes| bytes == b"guest exact output")
                        }),
                    "the worker must hold a nonempty partial output before cancellation"
                );
                assert_eq!(std::fs::read(&destination).unwrap(), b"original");
                source.cancel();
                let terminal = async_engine::timeout(Duration::from_secs(10), task)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    terminal,
                    SketchWorkerTerminal::ForcedContainment {
                        trigger: SketchWorkerStopReason::Cancelled
                    }
                );
                assert_clean(&compiler, &sketch).await;
                assert!(!staging.exists());
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    assert_eq!(std::fs::read(&destination).unwrap(), b"original");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn real_worker_forces_containment_for_atomic_wait() {
    run_case(
        threaded_fixture::atomic_wait32_wasm(),
        CONTAINMENT_DEADLINE,
        long_fuel(),
        false,
        SketchWorkerTerminal::ForcedContainment {
            trigger: SketchWorkerStopReason::DeadlineExceeded,
        },
    );
}

#[test]
fn real_worker_sequential_stress_leaves_no_parent_state() {
    for _ in 0..3 {
        run_case(
            threaded_fixture::threaded_root_wasm(None, false, false, false),
            WORKER_DEADLINE,
            normal_fuel(),
            false,
            SketchWorkerTerminal::Completed(ThreadedRootOutcome::Started),
        );
        run_case(
            threaded_fixture::atomic_wait32_wasm(),
            CONTAINMENT_DEADLINE,
            long_fuel(),
            false,
            SketchWorkerTerminal::ForcedContainment {
                trigger: SketchWorkerStopReason::DeadlineExceeded,
            },
        );
        run_case(
            threaded_fixture::unreachable_root_wasm(),
            WORKER_DEADLINE,
            normal_fuel(),
            false,
            SketchWorkerTerminal::Execution(SketchExecutionError::Trapped),
        );
        // Cancellation may land during upload on a loaded two-core guest.
        // Both paths guarantee containment; this stress test proves cleanup,
        // while real_worker_classifies_fuel_cancellation_and_deadline checks
        // the cooperative terminal separately.
        run_case_checking(
            threaded_fixture::looping_root_wasm(),
            Duration::from_secs(2),
            long_fuel(),
            true,
            OUTER_BOUND,
            |terminal| {
                assert!(
                    matches!(
                        terminal,
                        SketchWorkerTerminal::Stopped(SketchWorkerStopReason::Cancelled)
                            | SketchWorkerTerminal::ForcedContainment {
                                trigger: SketchWorkerStopReason::Cancelled
                            }
                    ),
                    "unexpected cancellation terminal: {terminal:?}"
                );
            },
        );
    }
}

// These are deliberately a second, externally controlled process layer.  The
// worker's environment is explicit-empty; only this parent harness receives
// the marker paths.  Do not remove `--ignored` from the outer invocations.
#[cfg(feature = "wasm-sketch-worker-test-support")]
mod failure_proof {
    use super::*;
    use std::fs;
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    use std::process::Command;
    use std::time::Instant;
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    const MARKER: &str = "KERNAL_API_WASM_WORKER_IDENTITY_MARKER";
    const RESULT: &str = "KERNAL_API_WASM_WORKER_FAILURE_RESULT";
    const RELEASE: &str = "KERNAL_API_WASM_WORKER_FAILURE_RELEASE";

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    struct Artifacts {
        root: std::path::PathBuf,
        marker: std::path::PathBuf,
        result: std::path::PathBuf,
        release: std::path::PathBuf,
    }
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    impl Artifacts {
        fn new() -> Self {
            let unique = format!(
                "kernal-api-d4-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            );
            let root = std::env::temp_dir().join(unique);
            fs::create_dir(&root).expect("artifact directory");
            Self {
                marker: root.join("identity"),
                result: root.join("result"),
                release: root.join("release"),
                root,
            }
        }
    }
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    impl Drop for Artifacts {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[derive(Clone, Copy)]
    struct Identity {
        pid: u32,
        a: u64,
        b: u64,
    }
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn decode_marker(path: &std::path::Path) -> Option<Identity> {
        let text = fs::read_to_string(path).ok()?;
        let mut lines = text.lines();
        (lines.next()? == "kernal-api-worker-identity-v1").then_some(())?;
        let mut number = |key| -> Option<u64> { lines.next()?.strip_prefix(key)?.parse().ok() };
        let value = Identity {
            pid: number("pid=")?.try_into().ok()?,
            a: number("creation-a=")?,
            b: number("creation-b=")?,
        };
        lines.next().is_none().then_some(value)
    }
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn wait_for_marker(path: &std::path::Path) -> Identity {
        let deadline = Instant::now() + OUTER_BOUND;
        while Instant::now() < deadline {
            if path.exists() {
                if let Some(value) = decode_marker(path) {
                    return value;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("worker identity marker was not published")
    }
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    struct InnerChild(Option<std::process::Child>);
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    impl InnerChild {
        fn wait_success(&mut self) {
            let deadline = Instant::now() + OUTER_BOUND;
            let child = self.0.as_mut().expect("inner child");
            let status = loop {
                if let Some(status) = child.try_wait().expect("inner exit") {
                    break status;
                }
                assert!(Instant::now() < deadline, "inner child exceeded bound");
                std::thread::sleep(Duration::from_millis(10));
            };
            assert!(status.success());
            self.0 = None;
        }
    }
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    impl Drop for InnerChild {
        fn drop(&mut self) {
            let Some(child) = self.0.as_mut() else {
                return;
            };
            let _ = child.kill();
            let deadline = Instant::now() + OUTER_BOUND;
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn launch(inner: &str, files: &Artifacts) -> InnerChild {
        let worker = worker_executable();
        assert!(worker.is_absolute(), "real worker path must be absolute");
        InnerChild(Some(
            Command::new(std::env::current_exe().expect("test executable"))
                .args(["--exact", inner, "--ignored", "--nocapture"])
                .env(MARKER, &files.marker)
                .env(RESULT, &files.result)
                .env(RELEASE, &files.release)
                .env("KERNAL_API_D4_REAL_WORKER", worker)
                .spawn()
                .expect("inner harness"),
        ))
    }
    fn inner_crash() {
        let compiler = compiler(FAILURE_PROOF_DEADLINE, long_fuel());
        let sketch = admit(&compiler, threaded_fixture::atomic_wait32_wasm());
        let config = SketchWorkerConfig::new(
            PathBuf::from(std::env::var_os("KERNAL_API_D4_REAL_WORKER").expect("worker")),
            GRACE,
        )
        .expect("config");
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let actual = runtime
            .run_within(
                async { contained(&sketch, runtime.handle(), &config, None, OUTER_BOUND).await },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        fs::write(std::env::var_os(RESULT).expect("result"), actual.code()).expect("result");
        runtime
            .run_within(
                async { assert_clean(&compiler, &sketch).await },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }
    fn inner_parent_death() {
        let compiler = compiler(FAILURE_PROOF_DEADLINE, long_fuel());
        let sketch = admit(&compiler, threaded_fixture::atomic_wait32_wasm());
        let config = SketchWorkerConfig::new(
            PathBuf::from(std::env::var_os("KERNAL_API_D4_REAL_WORKER").expect("worker")),
            GRACE,
        )
        .expect("config");
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let handle = runtime.handle();
        let _task = runtime.handle().launch(async move {
            let _ = sketch
                .execute_threaded_root_contained_cancellable(
                    handle,
                    &config,
                    CancellationSource::new().token(),
                )
                .await;
        });
        let release = std::path::PathBuf::from(std::env::var_os(RELEASE).expect("release"));
        runtime
            .run_within(
                async {
                    let deadline = Instant::now() + OUTER_BOUND;
                    while !release.exists() && Instant::now() < deadline {
                        async_engine::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(
                        release.exists(),
                        "outer harness did not release parent-death inner process"
                    );
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        std::process::exit(0);
    }

    #[test]
    #[ignore]
    fn d4_inner_crash_exact_identity() {
        inner_crash();
    }
    #[test]
    #[ignore]
    fn d4_inner_parent_death_exact_identity() {
        inner_parent_death();
    }

    #[cfg(target_os = "linux")]
    fn linux_stat(pid: u32) -> Option<(Identity, char)> {
        let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let close = text.rfind(')')?;
        let fields: Vec<_> = text[close + 1..].split_whitespace().collect();
        let state = fields.first()?.chars().next()?;
        Some((
            Identity {
                pid,
                a: fields.get(19)?.parse().ok()?,
                b: 0,
            },
            state,
        ))
    }
    #[cfg(target_os = "linux")]
    fn linux_identity(pid: u32) -> Option<Identity> {
        linux_stat(pid).map(|(identity, _)| identity)
    }
    /// Prove this exact worker is no longer executing.
    ///
    /// The armed pidfd has already signalled, which on Linux happens at
    /// `exit_notify`: the process is dead but its `/proc` entry survives until
    /// whoever inherited it collects the zombie.  When the inner harness dies
    /// first that collector is init, so the entry can outlive the proof by an
    /// arbitrary scheduling delay.  Accept a vanished entry, a reused PID, or
    /// the `Z` state — a zombie owns no threads, no memory and no descriptors,
    /// so containment is proven in every one of those three cases.  Any other
    /// state is a live worker and still fails the proof.
    #[cfg(target_os = "linux")]
    fn exact_worker_stopped_running(identity: Identity) -> bool {
        linux_stat(identity.pid)
            .is_none_or(|(now, state)| now.a != identity.a || now.b != identity.b || state == 'Z')
    }
    #[cfg(target_os = "linux")]
    struct CloseOnlyPidFd(Option<i32>);
    #[cfg(target_os = "linux")]
    impl Drop for CloseOnlyPidFd {
        fn drop(&mut self) {
            if let Some(fd) = self.0.take() {
                unsafe {
                    libc::close(fd);
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    impl CloseOnlyPidFd {
        fn promote(mut self) -> ArmedPidFd {
            ArmedPidFd(self.0.take())
        }
    }
    #[cfg(target_os = "linux")]
    struct ArmedPidFd(Option<i32>);
    #[cfg(target_os = "linux")]
    impl ArmedPidFd {
        fn wait_gone(&mut self) {
            let fd = self.0.expect("pidfd");
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            assert!(
                unsafe { libc::poll(&mut poll, 1, OUTER_BOUND.as_millis() as i32) } > 0,
                "exact worker survived bound"
            );
            unsafe {
                libc::close(fd);
            }
            self.0 = None;
        }
    }
    #[cfg(target_os = "linux")]
    impl Drop for ArmedPidFd {
        fn drop(&mut self) {
            let Some(fd) = self.0.take() else {
                return;
            };
            let _ = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd,
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let _ = unsafe { libc::poll(&mut poll, 1, OUTER_BOUND.as_millis() as i32) };
            unsafe {
                libc::close(fd);
            }
        }
    }
    #[cfg(target_os = "linux")]
    fn pidfd_open(identity: Identity) -> ArmedPidFd {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) as i32 };
        let close_only = CloseOnlyPidFd((fd >= 0).then_some(fd));
        assert!(
            close_only.0.is_some(),
            "pidfd_open: {}",
            std::io::Error::last_os_error()
        );
        assert!(
            matches!(linux_identity(identity.pid), Some(now) if now.a == identity.a && now.b == identity.b),
            "PID was reused"
        );
        close_only.promote()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn d4_crash_reaps_exact_worker() {
        let files = Artifacts::new();
        let mut inner = launch("failure_proof::d4_inner_crash_exact_identity", &files);
        let identity = wait_for_marker(&files.marker);
        let mut fd = pidfd_open(identity);
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.0.expect("pidfd"),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            },
            0,
            "pidfd signal"
        );
        fd.wait_gone();
        inner.wait_success();
        assert_eq!(
            fs::read_to_string(&files.result).expect("result"),
            "worker-unexpected-exit"
        );
        assert!(exact_worker_stopped_running(identity));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn d4_parent_death_kills_exact_worker() {
        let files = Artifacts::new();
        let mut inner = launch(
            "failure_proof::d4_inner_parent_death_exact_identity",
            &files,
        );
        let identity = wait_for_marker(&files.marker);
        let mut fd = pidfd_open(identity);
        fs::write(&files.release, "go").expect("release");
        inner.wait_success();
        fd.wait_gone();
        assert!(exact_worker_stopped_running(identity));
    }
    #[cfg(target_os = "windows")]
    struct CloseOnlyWindowsHandle(Option<windows_sys::Win32::Foundation::HANDLE>);
    #[cfg(target_os = "windows")]
    impl Drop for CloseOnlyWindowsHandle {
        fn drop(&mut self) {
            if let Some(handle) = self.0.take() {
                unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(handle);
                }
            }
        }
    }
    #[cfg(target_os = "windows")]
    impl CloseOnlyWindowsHandle {
        fn promote(mut self) -> ArmedWindowsProcess {
            ArmedWindowsProcess(self.0.take())
        }
    }
    #[cfg(target_os = "windows")]
    struct ArmedWindowsProcess(Option<windows_sys::Win32::Foundation::HANDLE>);
    #[cfg(target_os = "windows")]
    impl ArmedWindowsProcess {
        fn handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
            self.0.expect("process handle")
        }
        fn wait_gone(&mut self) {
            use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            assert_eq!(
                unsafe { WaitForSingleObject(self.handle(), OUTER_BOUND.as_millis() as u32) },
                WAIT_OBJECT_0,
                "exact worker survived bound"
            );
            unsafe {
                CloseHandle(self.handle());
            }
            self.0 = None;
        }
    }
    #[cfg(target_os = "windows")]
    impl Drop for ArmedWindowsProcess {
        fn drop(&mut self) {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};
            let Some(process) = self.0.take() else {
                return;
            };
            let _ = unsafe { TerminateProcess(process, 1) };
            let _ = unsafe { WaitForSingleObject(process, OUTER_BOUND.as_millis() as u32) };
            unsafe {
                CloseHandle(process);
            }
        }
    }
    #[cfg(target_os = "windows")]
    fn windows_handle(identity: Identity, access: u32) -> ArmedWindowsProcess {
        use windows_sys::Win32::System::Threading::{GetProcessTimes, OpenProcess};
        let close_only =
            CloseOnlyWindowsHandle(Some(unsafe { OpenProcess(access, 0, identity.pid) }));
        if close_only.0.expect("owned handle").is_null() {
            let error = std::io::Error::last_os_error();
            panic!("OpenProcess: {error}");
        }
        assert!(
            !close_only.0.expect("owned handle").is_null(),
            "OpenProcess unexpectedly returned a null handle"
        );
        let mut creation = unsafe { std::mem::zeroed() };
        let mut exit = unsafe { std::mem::zeroed() };
        let mut kernel = unsafe { std::mem::zeroed() };
        let mut user = unsafe { std::mem::zeroed() };
        assert_ne!(
            unsafe {
                GetProcessTimes(
                    close_only.0.expect("owned handle"),
                    &mut creation,
                    &mut exit,
                    &mut kernel,
                    &mut user,
                )
            },
            0,
            "GetProcessTimes"
        );
        let created = ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
        assert_eq!((created, 0), (identity.a, identity.b), "PID was reused");
        close_only.promote()
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn d4_crash_reaps_exact_worker() {
        use windows_sys::Win32::System::Threading::{
            TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        };
        const SYNCHRONIZE: u32 = 0x0010_0000;
        let files = Artifacts::new();
        let mut inner = launch("failure_proof::d4_inner_crash_exact_identity", &files);
        let identity = wait_for_marker(&files.marker);
        let mut process = windows_handle(
            identity,
            PROCESS_TERMINATE | SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
        );
        // The long proof deadline keeps normal containment out of this
        // acquisition/action window. This exact live handle is the worker we
        // intentionally crash, then wait as proof of its disappearance.
        assert_ne!(
            unsafe { TerminateProcess(process.handle(), 1) },
            0,
            "TerminateProcess"
        );
        process.wait_gone();
        inner.wait_success();
        assert_eq!(
            fs::read_to_string(&files.result).expect("result"),
            "worker-unexpected-exit"
        );
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn d4_parent_death_kills_exact_worker() {
        use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
        const SYNCHRONIZE: u32 = 0x0010_0000;
        let files = Artifacts::new();
        let mut inner = launch(
            "failure_proof::d4_inner_parent_death_exact_identity",
            &files,
        );
        let identity = wait_for_marker(&files.marker);
        let mut process = windows_handle(identity, SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION);
        fs::write(&files.release, "go").expect("release");
        inner.wait_success();
        // Release the inner parent only after this creation-validated worker
        // handle is live; owner death must make this exact handle signal.
        process.wait_gone();
    }
    #[cfg(target_os = "macos")]
    fn macos_identity(pid: u32) -> std::io::Result<Option<Identity>> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let expected = i32::try_from(std::mem::size_of_val(&info)).expect("proc_bsdinfo size");
        // `proc_pidinfo` copies into the fully initialized stack allocation
        // above and returns the number of copied bytes. Its start timestamp is
        // stable for a process lifetime, so it prevents a reused PID from
        // satisfying this external containment proof.
        let copied = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                expected,
            )
        };
        if copied == expected {
            return Ok(Some(Identity {
                pid,
                a: info.pbi_start_tvsec,
                b: info.pbi_start_tvusec,
            }));
        }
        let error = std::io::Error::last_os_error();
        if copied == 0 && error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("proc_pidinfo copied {copied} of {expected} bytes: {error}"),
        ))
    }

    #[cfg(target_os = "macos")]
    struct MacosWorkerExitWatch {
        descriptor: i32,
        identity: Identity,
    }
    #[cfg(target_os = "macos")]
    impl MacosWorkerExitWatch {
        fn register(identity: Identity) -> Self {
            let descriptor = unsafe { libc::kqueue() };
            assert!(
                descriptor >= 0,
                "kqueue: {}",
                std::io::Error::last_os_error()
            );
            // Establish RAII ownership before any later fallible registration
            // or identity check can unwind.
            let watch = Self {
                descriptor,
                identity,
            };
            let change = libc::kevent {
                ident: identity.pid as libc::uintptr_t,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // kqueue retains this process-event registration, allowing the
            // subsequent wait to observe this lifecycle rather than a later
            // process that reuses the same numeric PID.
            assert_eq!(
                unsafe {
                    libc::kevent(
                        watch.descriptor,
                        &change,
                        1,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null(),
                    )
                },
                0,
                "kqueue registration: {}",
                std::io::Error::last_os_error()
            );
            assert!(
                matches!(
                    macos_identity(identity.pid).expect("proc_pidinfo after kqueue registration"),
                    Some(now) if now.a == identity.a && now.b == identity.b
                ),
                "worker exited or PID was reused before the parent-death action"
            );
            watch
        }
        fn wait_gone(&mut self) {
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let timeout = libc::timespec {
                tv_sec: OUTER_BOUND.as_secs() as libc::time_t,
                tv_nsec: OUTER_BOUND.subsec_nanos() as libc::c_long,
            };
            assert_eq!(
                unsafe {
                    libc::kevent(
                        self.descriptor,
                        std::ptr::null(),
                        0,
                        &mut event,
                        1,
                        &timeout,
                    )
                },
                1,
                "exact worker survived bound: {}",
                std::io::Error::last_os_error()
            );
            // Darwin declares `kevent` packed. Copy each returned field with
            // unaligned reads before asserting on the registered lifecycle.
            let event_ident = unsafe { std::ptr::addr_of!(event.ident).read_unaligned() };
            let event_filter = unsafe { std::ptr::addr_of!(event.filter).read_unaligned() };
            let event_flags = unsafe { std::ptr::addr_of!(event.fflags).read_unaligned() };
            assert_eq!(event_ident, self.identity.pid as libc::uintptr_t);
            assert_eq!(event_filter, libc::EVFILT_PROC);
            assert_ne!(event_flags & libc::NOTE_EXIT, 0);
        }
    }
    #[cfg(target_os = "macos")]
    impl Drop for MacosWorkerExitWatch {
        fn drop(&mut self) {
            let _ = unsafe { libc::close(self.descriptor) };
        }
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn d4_parent_death_kills_exact_worker() {
        let files = Artifacts::new();
        let mut inner = launch(
            "failure_proof::d4_inner_parent_death_exact_identity",
            &files,
        );
        let identity = wait_for_marker(&files.marker);
        assert!(
            matches!(
                macos_identity(identity.pid).expect("proc_pidinfo before kqueue registration"),
                Some(now) if now.a == identity.a && now.b == identity.b
            ),
            "worker exited or PID was reused before kqueue registration"
        );
        let mut worker = MacosWorkerExitWatch::register(identity);
        fs::write(&files.release, "go").expect("release");
        inner.wait_success();
        worker.wait_gone();
    }
}
