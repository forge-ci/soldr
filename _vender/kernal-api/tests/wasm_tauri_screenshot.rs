#![cfg(feature = "wasm-sketch-host")]

use kernal_api::async_engine::RuntimeBuilder;
use kernal_api::wasm::{
    SketchCompiler, SketchCompilerConfig, SketchExecutionError, SketchModuleError,
    SketchModulePolicy,
};

// Cargo check/Dylint do not necessarily build binaries or define their paths.
// Actual test execution still requires an explicit binary; no runtime fallback.
#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
fn screenshot_binary(worker: bool) -> std::path::PathBuf {
    let (runtime_key, built) = if worker {
        (
            "NEXTEST_BIN_EXE_kernal-wasm-worker",
            option_env!("CARGO_BIN_EXE_kernal-wasm-worker"),
        )
    } else {
        (
            "NEXTEST_BIN_EXE_kernal-api-wasm-tauri",
            option_env!("CARGO_BIN_EXE_kernal-api-wasm-tauri"),
        )
    };
    std::env::var_os(runtime_key)
        .map(std::path::PathBuf::from)
        .or_else(|| built.map(std::path::PathBuf::from))
        .expect("Cargo test or nextest must supply the actual screenshot binary")
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
fn configure_fixture_stream(stream: &std::net::TcpStream) -> std::io::Result<()> {
    // Accepted sockets can inherit listener nonblocking mode. Browser probes
    // can also disconnect before setup: Darwin rejects setsockopt on a shut
    // down socket with EINVAL. Callers discard that connection, not the server.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[test]
fn fixture_stream_uses_bounded_blocking_io() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    server.set_nonblocking(true).unwrap();
    configure_fixture_stream(&server).unwrap();
    let timeout = Some(std::time::Duration::from_secs(2));
    assert_eq!(server.read_timeout().unwrap(), timeout);
    assert_eq!(server.write_timeout().unwrap(), timeout);
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        client.write_all(b"request").unwrap();
    });
    let mut request = [0; 7];
    let received = server.read_exact(&mut request);
    writer.join().unwrap();
    received.unwrap();
    assert_eq!(&request, b"request");
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
fn join_fixture_thread(thread: std::thread::JoinHandle<()>) {
    if let Err(failure) = thread.join() {
        // Preserve an assertion already unwinding through fixture cleanup.
        // Otherwise the fixture failure must still fail the test itself.
        if !std::thread::panicking() {
            std::panic::resume_unwind(failure);
        }
    }
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[test]
fn fixture_thread_panic_is_reported_without_an_existing_failure() {
    assert!(std::panic::catch_unwind(|| {
        let thread = std::thread::spawn(|| panic!("fixture failure"));
        join_fixture_thread(thread);
    })
    .is_err());
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[test]
fn fixture_thread_panic_preserves_an_existing_test_failure() {
    struct JoinOnDrop(Option<std::thread::JoinHandle<()>>);
    impl Drop for JoinOnDrop {
        fn drop(&mut self) {
            join_fixture_thread(self.0.take().unwrap());
        }
    }
    let failure = std::panic::catch_unwind(|| {
        let _fixture = JoinOnDrop(Some(std::thread::spawn(|| panic!("fixture failure"))));
        panic!("original assertion failure");
    })
    .unwrap_err();
    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"original assertion failure")
    );
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[test]
#[ignore = "requires a native display and actual screenshot artifact"]
fn actual_screenshot_guest_runs_inside_containment() {
    run_contained_screenshot(ContainedScenario::Capture);
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[test]
#[ignore = "requires a native display and actual trap-after-capture artifact"]
fn actual_screenshot_guest_trap_inside_containment_preserves_output() {
    run_contained_screenshot(ContainedScenario::Trap);
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[test]
#[ignore = "requires a native display and actual block-after-capture artifact"]
fn actual_screenshot_guest_block_inside_containment_forces_reap() {
    run_contained_screenshot(ContainedScenario::Block);
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContainedScenario {
    Capture,
    Trap,
    Block,
    CancelLoad,
    CaptureQuota,
    RenamedParent,
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[test]
#[ignore = "requires a native display and actual screenshot artifact"]
fn actual_screenshot_guest_renamed_parent_cleans_staging() {
    run_contained_screenshot(ContainedScenario::RenamedParent);
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[test]
#[ignore = "requires a native display and actual screenshot artifact"]
fn actual_screenshot_guest_cancellation_inside_containment_drains_load() {
    run_contained_screenshot(ContainedScenario::CancelLoad);
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[test]
#[ignore = "requires a native display and actual screenshot artifact"]
fn actual_screenshot_guest_native_capture_quota_failure_drains_containment() {
    run_contained_screenshot(ContainedScenario::CaptureQuota);
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[expect(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn run_contained_screenshot(scenario: ContainedScenario) {
    use kernal_api::wasm::{SketchEpochLimits, SketchWorkerConfig, SketchWorkerTerminal};
    use std::io::{Read, Write};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;
    let artifact = std::env::var_os(match scenario {
        ContainedScenario::Trap => "KERNAL_API_SCREENSHOT_TRAP_ARTIFACT_WASM",
        ContainedScenario::Block => "KERNAL_API_SCREENSHOT_BLOCK_ARTIFACT_WASM",
        ContainedScenario::Capture
        | ContainedScenario::CancelLoad
        | ContainedScenario::RenamedParent
        | ContainedScenario::CaptureQuota => "KERNAL_API_SCREENSHOT_ARTIFACT_WASM",
    })
    .expect("built screenshot artifact");
    let retained = std::env::var_os("KERNAL_API_SCREENSHOT_PROOF_DIR").map(|root| {
        std::fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix(&format!("contained-{scenario:?}-"))
            .tempdir_in(root)
            .unwrap()
            .keep()
    });
    let temporary = tempfile::tempdir().unwrap();
    let proof = retained.as_deref().unwrap_or(temporary.path());
    let directory = proof.join("output");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(proof.join("process.json"), "{\"outcome\":\"started\"}\n").unwrap();
    let output = directory.join("viewport.png");
    let sentinel = directory.join("untouched");
    std::fs::write(&output, b"original").unwrap();
    std::fs::write(&sentinel, b"unchanged").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    struct Server(Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
            join_fixture_thread(self.1.take().unwrap());
        }
    }
    let stopping = Arc::clone(&stop);
    let requested = Arc::new(AtomicBool::new(false));
    let observed_request = Arc::clone(&requested);
    // Readiness signal raised right after `requested` is stored. `notify_one`
    // retains a permit, so a waiter that registers late still wakes.
    let request_signal = Arc::new(kernal_api::async_engine::Notify::new());
    let server_request_signal = Arc::clone(&request_signal);
    let original_directory = directory.clone();
    let moved_directory = proof.join("moved-output");
    let server_moved_directory = moved_directory.clone();
    let thread = std::thread::spawn(move || {
        while !stopping.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    if let Err(error) = configure_fixture_stream(&stream) {
                        eprintln!("discarding fixture connection during setup: {error}");
                        continue;
                    }
                    let mut request = [0; 4096];
                    if stream.read(&mut request).unwrap_or(0) > 0 {
                        if scenario == ContainedScenario::RenamedParent
                            && !observed_request.load(Ordering::Acquire)
                        {
                            // A native request proves the parent already
                            // staged and granted the worker's output path.
                            std::fs::rename(&original_directory, &server_moved_directory).unwrap();
                        }
                        observed_request.store(true, Ordering::Release);
                        server_request_signal.notify_one();
                        if scenario == ContainedScenario::CancelLoad {
                            while !stopping.load(Ordering::Acquire) {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            continue;
                        }
                        let page = if scenario == ContainedScenario::CaptureQuota {
                            include_str!(
                                "../examples/wasm-tauri-screenshot/fixtures/over-budget.html"
                            )
                        } else {
                            include_str!("../examples/wasm-tauri-screenshot/fixtures/viewport.html")
                        };
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("fixture server: {error}"),
            }
        }
    });
    let _server = Server(stop, Some(thread));
    let config = SketchWorkerConfig::new(screenshot_binary(true), Duration::from_secs(2))
        .unwrap()
        .with_webview_capture(
            kernal_api::webview::WebviewUrlGrant::new(&format!("http://{address}/")).unwrap(),
            output.clone(),
        )
        .unwrap();
    let trace = kernal_api::wasm::SketchWorkerTrace::default();
    let config = config.with_trace(trace.clone());
    let epochs = SketchEpochLimits::default();
    let compiler = SketchCompiler::new(
        SketchCompilerConfig::default()
            .with_epoch_limits(
                SketchEpochLimits::new(
                    Duration::from_secs(if scenario == ContainedScenario::Block {
                        20
                    } else {
                        60
                    }),
                    epochs.tick_interval(),
                    epochs.maximum_active_registrations(),
                )
                .unwrap(),
            )
            .unwrap(),
    )
    .unwrap();
    let sketch = Arc::new(
        compiler
            .admit(
                &std::fs::read(artifact).unwrap(),
                SketchModulePolicy::threaded_rust_v1(32 * 1024 * 1024, 16_384).unwrap(),
            )
            .unwrap(),
    );
    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
    let cancellation = kernal_api::async_engine::CancellationSource::new();
    let cancel_task = if scenario == ContainedScenario::CancelLoad {
        let source = cancellation.clone();
        Some(runtime.handle().launch(async move {
            // Wait for the fixture's explicit readiness signal rather than
            // polling against a fixed ceiling: webview startup on slow lanes
            // (aarch64 measured ~9.7s) outran the old ~10s poll. The 30s
            // deadline is a failure bound, well inside the 60s run_within.
            if !requested.load(Ordering::Acquire) {
                let _ = request_signal
                    .notified()
                    .wait_within(Duration::from_secs(30))
                    .await;
            }
            if requested.load(Ordering::Acquire) {
                // The response stays unfinished. The returned trace below
                // must prove the guest actually submitted its load wait.
                kernal_api::async_engine::sleep(Duration::from_millis(500)).await;
                source.cancel();
                return true;
            }
            source.cancel();
            false
        }))
    } else {
        None
    };
    let terminal = runtime
        .run_within(
            sketch.execute_threaded_root_contained_cancellable(
                runtime.handle(),
                &config,
                cancellation.token(),
            ),
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    let directory = if scenario == ContainedScenario::RenamedParent {
        assert!(!directory.exists());
        moved_directory
    } else {
        directory
    };
    let output = directory.join("viewport.png");
    let sentinel = directory.join("untouched");
    // Persist raw evidence before assertions, including failure outcomes. A
    // killed worker may never send its terminal trace; do not invent one.
    let worker_trace = trace.take();
    if let Some(trace) = &worker_trace {
        std::fs::write(proof.join("worker.trace"), trace).unwrap();
    }
    let counts = sketch.worker_execution_snapshot();
    let guest_exit_code = match &terminal {
        SketchWorkerTerminal::Execution(SketchExecutionError::NonzeroExit { code }) => {
            code.to_string()
        }
        _ => "null".to_owned(),
    };
    std::fs::write(
        proof.join("process.json"),
        format!(
            "{{\"scenario\":\"{scenario:?}\",\"os\":\"{}\",\"arch\":\"{}\",\"terminal\":\"{}\",\"guest_exit_code\":{guest_exit_code},\"worker_trace_available\":{},\"spawned\":{},\"reaped\":{},\"forced\":{},\"live_workers\":{},\"live_protocol_tasks\":{},\"pending_root_leases\":{}}}\n",
            std::env::consts::OS, std::env::consts::ARCH, terminal.code(), worker_trace.is_some(), counts.spawned, counts.reaped,
            counts.forced, counts.live_workers, counts.live_protocol_tasks, counts.pending_root_leases,
        ),
    ).unwrap();
    if let Some(task) = cancel_task {
        assert!(
            runtime
                .run_within(task, std::time::Duration::from_secs(60))
                .expect("test runtime budget")
                .unwrap(),
            "cancellation never observed a native HTTP request"
        );
    }
    if scenario == ContainedScenario::RenamedParent {
        assert_eq!(
            terminal,
            SketchWorkerTerminal::Execution(SketchExecutionError::NonzeroExit { code: 113 })
        );
        let trace = worker_trace
            .as_deref()
            .expect("renamed-parent cleanup trace");
        validate_teardown_trace(trace).unwrap();
        assert!(trace.contains("phase=capture-requested"), "{trace}");
        assert!(
            trace.lines().any(
                |line| line.starts_with("kernal-webview-trace phase=submit ")
                    && line.ends_with("opcode=10")
            ),
            "write was not attempted: {trace}"
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"original");
    } else if scenario == ContainedScenario::CancelLoad {
        assert_eq!(
            terminal,
            SketchWorkerTerminal::Stopped(kernal_api::wasm::SketchWorkerStopReason::Cancelled)
        );
        let trace = worker_trace
            .as_deref()
            .expect("cooperative cancellation must report worker cleanup");
        validate_teardown_trace(trace).unwrap();
        assert!(
            trace.lines().any(
                |line| line.starts_with("kernal-webview-trace phase=submit ")
                    && line.ends_with("opcode=15")
            ),
            "load wait was not submitted: {trace}"
        );
        assert!(!trace.contains("phase=capture-requested"));
        assert_eq!(std::fs::read(&output).unwrap(), b"original");
    } else if scenario == ContainedScenario::CaptureQuota {
        assert_eq!(
            terminal,
            SketchWorkerTerminal::Execution(SketchExecutionError::NonzeroExit { code: 97 })
        );
        let trace = worker_trace
            .as_deref()
            .expect("failed capture cleanup trace");
        validate_teardown_trace(trace).unwrap();
        assert!(trace.contains("phase=capture-requested"), "{trace}");
        assert!(
            trace.contains("phase=capture-encoded-byte-limit"),
            "{trace}"
        );
        assert!(
            !trace.lines().any(
                |line| line.starts_with("kernal-webview-trace phase=submit ")
                    && line.ends_with("opcode=10")
            ),
            "failed capture attempted output commit: {trace}"
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"original");
    } else if scenario == ContainedScenario::Block {
        assert_eq!(
            terminal,
            SketchWorkerTerminal::ForcedContainment {
                trigger: kernal_api::wasm::SketchWorkerStopReason::DeadlineExceeded,
            }
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"original");
    } else if scenario == ContainedScenario::Trap {
        assert_eq!(
            terminal,
            SketchWorkerTerminal::Execution(SketchExecutionError::Trapped)
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"original");
    } else {
        assert!(
            matches!(terminal, SketchWorkerTerminal::Completed(_)),
            "{terminal:?}"
        );
        validate_fixture_png(&std::fs::read(&output).unwrap()).unwrap();
    }
    assert_eq!(std::fs::read(sentinel).unwrap(), b"unchanged");
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
    let counts = sketch.worker_execution_snapshot();
    assert_eq!(counts.spawned, 1);
    assert_eq!(counts.reaped, 1);
    assert_eq!(
        counts.forced,
        u64::from(scenario == ContainedScenario::Block)
    );
    assert_eq!(
        (
            counts.live_workers,
            counts.live_protocol_tasks,
            counts.pending_root_leases
        ),
        (0, 0, 0)
    );
    drop(sketch);
    let counts = compiler.execution_limits_snapshot();
    assert_eq!(counts.active_root_executions(), 0);
    assert_eq!(counts.live_stores(), 0);
    assert_eq!(counts.reserved_shared_memory_bytes(), 0);
    std::fs::write(
        proof.join("validation.txt"),
        "all contained proof assertions passed\n",
    )
    .unwrap();
}

#[cfg(feature = "tauri-webview-test-support")]
#[test]
#[cfg(feature = "wasm-sketch-worker")]
fn screenshot_cli_rejects_invalid_urls_before_module_loading_or_output_changes() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("viewport.png");
    let sentinel = directory.path().join("untouched");
    let absent_module = directory.path().join("absent.wasm");
    std::fs::write(&output, b"original").unwrap();
    std::fs::write(&sentinel, b"unchanged").unwrap();
    for url in [
        "not a URL",
        "file:///not-authorized",
        "javascript:void(0)",
        "data:text/html,not-authorized",
        "tauri://localhost/",
        "http://user:password@example.test/",
        "http://[broken",
    ] {
        let result = std::process::Command::new(screenshot_binary(false))
            .args(["--url", url, "--output"])
            .arg(&output)
            .arg("--module")
            .arg(&absent_module)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert_eq!(
            String::from_utf8(result.stderr).unwrap().trim(),
            "Error: InvalidUrl"
        );
        assert!(result.stdout.is_empty());
        assert_eq!(std::fs::read(&output).unwrap(), b"original");
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }
    // Negative control: a valid URL gets past URL validation to the missing
    // module error. Neither case needs a display or grants ambient network.
    let result = std::process::Command::new(screenshot_binary(false))
        .args(["--url", "http://127.0.0.1:1/", "--output"])
        .arg(&output)
        .arg("--module")
        .arg(&absent_module)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!String::from_utf8(result.stderr)
        .unwrap()
        .contains("InvalidUrl"));
    assert_eq!(std::fs::read(&output).unwrap(), b"original");
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[cfg(feature = "tauri-webview-test-support")]
#[test]
#[ignore = "requires a native display and the actual built screenshot guest"]
#[cfg(feature = "wasm-sketch-worker")]
fn actual_screenshot_cli_runs_the_offline_guest_and_commits_only_its_output() {
    run_native_screenshot_proof(NativeScenario::Capture);
}

#[cfg(feature = "tauri-webview-test-support")]
#[test]
#[ignore = "requires a native display and the actual built screenshot guest"]
#[cfg(feature = "wasm-sketch-worker")]
fn actual_screenshot_guest_rejects_redirect_and_drains_without_output() {
    run_native_screenshot_proof(NativeScenario::Redirect);
}

#[cfg(feature = "tauri-webview-test-support")]
#[test]
#[ignore = "requires a native display, actual guest, and the real 30-second load deadline"]
#[cfg(feature = "wasm-sketch-worker")]
fn actual_screenshot_guest_times_out_and_drains_without_output() {
    run_native_screenshot_proof(NativeScenario::Timeout);
}

#[cfg(feature = "tauri-webview-test-support")]
#[test]
#[ignore = "requires a native display and the actual built screenshot guest"]
#[cfg(feature = "wasm-sketch-worker")]
fn actual_screenshot_guest_write_failure_reclaims_capture_and_preserves_files() {
    run_native_screenshot_proof(NativeScenario::MissingOutputParent);
}

#[cfg(feature = "tauri-webview-test-support")]
#[test]
#[ignore = "requires a native display and the real guest built with proof-trap-after-capture"]
#[cfg(feature = "wasm-sketch-worker")]
fn actual_screenshot_guest_trap_reclaims_completed_native_capture() {
    run_native_screenshot_proof(NativeScenario::TrapAfterCapture);
}

#[cfg(feature = "tauri-webview-test-support")]
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "wasm-sketch-worker")]
enum NativeScenario {
    Capture,
    Redirect,
    Timeout,
    MissingOutputParent,
    TrapAfterCapture,
    ContainedCapture,
    ContainedTrap,
    MissingWorker,
    PublicationFailure,
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[test]
#[ignore = "requires a native display and actual screenshot artifact"]
fn default_screenshot_cli_uses_containment_and_commits_output() {
    run_native_screenshot_proof(NativeScenario::ContainedCapture);
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[test]
#[ignore = "requires a native display and actual trap-after-capture artifact"]
fn default_screenshot_cli_uses_containment_and_preserves_output_on_trap() {
    run_native_screenshot_proof(NativeScenario::ContainedTrap);
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[test]
#[ignore = "requires the actual built screenshot artifact"]
fn default_screenshot_cli_missing_worker_never_falls_back() {
    run_native_screenshot_proof(NativeScenario::MissingWorker);
}

#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
#[test]
#[ignore = "requires a native display and actual screenshot artifact"]
fn default_screenshot_cli_publication_failure_cleans_staging() {
    run_native_screenshot_proof(NativeScenario::PublicationFailure);
}

#[cfg(feature = "tauri-webview-test-support")]
#[cfg(feature = "wasm-sketch-worker")]
#[expect(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn run_native_screenshot_proof(scenario: NativeScenario) {
    use std::io::{Read, Write};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};
    struct Server {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                join_fixture_thread(thread);
            }
        }
    }
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let artifact_variable = if matches!(
        scenario,
        NativeScenario::TrapAfterCapture | NativeScenario::ContainedTrap
    ) {
        "KERNAL_API_SCREENSHOT_TRAP_ARTIFACT_WASM"
    } else {
        "KERNAL_API_SCREENSHOT_ARTIFACT_WASM"
    };
    let builds_guest = scenario == NativeScenario::ContainedCapture;
    let artifact = (!builds_guest)
        .then(|| std::env::var_os(artifact_variable).expect("actual guest artifact required"));
    // Diagnostic logs remain outside the exact-output directory.
    let retained = std::env::var_os("KERNAL_API_SCREENSHOT_PROOF_DIR").map(|root| {
        std::fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix("screenshot-")
            .tempdir_in(root)
            .unwrap()
            .keep()
    });
    let temporary = tempfile::tempdir().unwrap();
    let proof = retained.as_deref().unwrap_or(temporary.path());
    let directory = proof.join("output");
    std::fs::create_dir(&directory).unwrap();
    let output = directory.join("viewport.png");
    let sentinel = directory.join("untouched");
    std::fs::write(&output, b"original").unwrap();
    std::fs::write(&sentinel, b"unchanged").unwrap();
    let fixture_output_directory = directory.clone();
    let preserved_directory = proof.join("preserved-output");
    let fixture_preserved_directory = preserved_directory.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let thread = std::thread::spawn(move || {
        let page = include_str!("../examples/wasm-tauri-screenshot/fixtures/viewport.html");
        let mut relocated = false;
        while !stopping.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    if let Err(error) = configure_fixture_stream(&stream) {
                        eprintln!("discarding fixture connection during setup: {error}");
                        continue;
                    }
                    let mut request = [0; 4096];
                    if stream.read(&mut request).is_ok_and(|count| count > 0) {
                        if scenario == NativeScenario::PublicationFailure && !relocated {
                            // Parent staging already exists. Preserve the original
                            // file and obstruct only final atomic publication;
                            // guest capture and its private staged write remain real.
                            let destination = fixture_output_directory.join("viewport.png");
                            std::fs::rename(
                                &destination,
                                fixture_output_directory.join("preserved-original"),
                            )
                            .unwrap();
                            std::fs::create_dir(&destination).unwrap();
                            std::fs::write(destination.join("keep"), b"obstruction").unwrap();
                            relocated = true;
                        }
                        if scenario == NativeScenario::MissingOutputParent && !relocated {
                            // Reaching this HTTP request proves URL/output grants
                            // were already installed and the guest opened its view.
                            // Preserve the files while making the granted parent
                            // unavailable to the later real output job.
                            std::fs::rename(
                                &fixture_output_directory,
                                &fixture_preserved_directory,
                            )
                            .unwrap();
                            relocated = true;
                        }
                        if scenario == NativeScenario::Timeout {
                            // Hold the real HTTP request open without finishing
                            // navigation; only the production host deadline may
                            // end this load. The fixture remains promptly stoppable.
                            while !stopping.load(Ordering::Acquire) {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                        } else if scenario == NativeScenario::Redirect {
                            let _ = write!(stream, "HTTP/1.1 302 Found\r\nLocation: tauri://localhost/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        } else {
                            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len());
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("fixture server: {error}"),
            }
        }
    });
    let _server = Server {
        stop,
        thread: Some(thread),
    };
    let start = Instant::now();
    let contained = matches!(
        scenario,
        NativeScenario::ContainedCapture
            | NativeScenario::ContainedTrap
            | NativeScenario::MissingWorker
            | NativeScenario::Redirect
            | NativeScenario::Timeout
            | NativeScenario::PublicationFailure
    );
    let mut command = std::process::Command::new(screenshot_binary(false));
    if !contained {
        command.arg("--diagnostic-in-process");
    }
    if scenario == NativeScenario::MissingWorker {
        command.arg("--worker").arg(proof.join("missing-worker"));
    }
    if let Some(artifact) = artifact {
        command.arg("--module").arg(artifact);
    }
    // The ordinary user entry point must build and admit the real guest,
    // then execute it in containment. Keep a separate bounded build allowance.
    //
    // #316: an artifact proof carries no fixed wall-clock deadline. Measured
    // over five green ARM runs plus x86, they finish in 2.7-35.6s, yet one
    // still tripped the old 90s total bound. The fixture's Timeout scenario
    // alone legitimately silences the runner for the production 30-second
    // load hold (measured gap: 30.001s), so the bound is now runner progress:
    // fail only once neither diagnostic log has grown for 90 seconds, three
    // times the longest legitimate silence. The guest-building run keeps its
    // wall-clock allowance because one amalgamated crate may compile longer
    // than any silence the stall bound would accept.
    let build_allowance = Duration::from_secs(300);
    let stall_bound = Duration::from_secs(90);
    let stdout_log = proof.join("runner.stdout.log");
    let stderr_log = proof.join("runner.stderr.log");
    let mut child = Child(
        command
            .arg("--url")
            .arg(format!("http://{address}/"))
            .arg("--output")
            .arg(&output)
            .stdout(std::fs::File::create(&stdout_log).unwrap())
            .stderr(std::fs::File::create(&stderr_log).unwrap())
            .spawn()
            .unwrap(),
    );
    let log_bytes = |path: &std::path::Path| -> u64 {
        std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
    };
    let mut output_bytes = log_bytes(&stdout_log) + log_bytes(&stderr_log);
    let mut last_progress = start;
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        let bytes = log_bytes(&stdout_log) + log_bytes(&stderr_log);
        if bytes != output_bytes {
            output_bytes = bytes;
            last_progress = Instant::now();
        }
        if builds_guest {
            if start.elapsed() >= build_allowance {
                std::fs::write(proof.join("process.json"), "{\"outcome\":\"timeout\"}\n").unwrap();
                panic!(
                    "screenshot runner exceeded its build allowance; diagnostics: {}",
                    proof.display()
                );
            }
        } else if last_progress.elapsed() >= stall_bound {
            std::fs::write(proof.join("process.json"), "{\"outcome\":\"stalled\"}\n").unwrap();
            panic!(
                "screenshot runner produced no output for {stall_bound:?}; diagnostics: {}",
                proof.display()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    std::fs::write(
        proof.join("process.json"),
        format!(
            "{{\"os\":\"{}\",\"arch\":\"{}\",\"success\":{},\"elapsed_ms\":{}}}\n",
            std::env::consts::OS,
            std::env::consts::ARCH,
            status.success(),
            start.elapsed().as_millis()
        ),
    )
    .unwrap();
    if contained {
        let trace = std::fs::read_to_string(proof.join("runner.stderr.log")).unwrap();
        if scenario == NativeScenario::MissingWorker {
            assert!(!status.success());
            assert!(trace.contains("native worker missing"), "{trace}");
            assert!(!trace.contains("kernal-webview-trace"), "{trace}");
            assert!(!trace.contains("kernal-worker-trace"), "{trace}");
            assert_eq!(std::fs::read(&output).unwrap(), b"original");
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
            assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
            return;
        }
        assert_eq!(
            status.success(),
            scenario == NativeScenario::ContainedCapture,
            "{trace}"
        );
        assert!(
            trace.contains("spawned=1 reaped=1 forced=0 workers=0 tasks=0 leases=0"),
            "{trace}"
        );
        if scenario == NativeScenario::ContainedCapture {
            validate_execution_trace(&trace).expect("contained ABI and timing trace");
            assert!(trace.contains("terminal=worker-completed"), "{trace}");
            validate_fixture_png(&std::fs::read(&output).unwrap()).unwrap();
        } else if scenario == NativeScenario::PublicationFailure {
            validate_execution_trace(&trace)
                .expect("guest completed real capture and staged output before parent failure");
            assert!(trace.contains("terminal=worker-output-commit"), "{trace}");
            assert!(output.is_dir());
            assert_eq!(std::fs::read(output.join("keep")).unwrap(), b"obstruction");
            assert_eq!(
                std::fs::read(directory.join("preserved-original")).unwrap(),
                b"original"
            );
            assert_eq!(std::fs::read_dir(&output).unwrap().count(), 1);
        } else if matches!(scenario, NativeScenario::Redirect | NativeScenario::Timeout) {
            validate_teardown_trace(&trace).expect("contained load failure cleanup trace");
            let expected = if scenario == NativeScenario::Timeout {
                assert!(
                    start.elapsed() >= Duration::from_secs(30),
                    "load deadline bypassed"
                );
                "screenshot-load-timed-out"
            } else {
                "screenshot-load-rejected"
            };
            assert!(trace.contains(expected), "{trace}");
            assert!(
                !trace.contains("phase=capture-requested"),
                "capture after load failure"
            );
            assert_eq!(std::fs::read(&output).unwrap(), b"original");
        } else {
            validate_teardown_trace(&trace).expect("contained trap cleanup trace");
            assert!(trace.contains("terminal=trapped"), "{trace}");
            assert_eq!(std::fs::read(&output).unwrap(), b"original");
        }
        assert!(std::fs::read(proof.join("runner.stdout.log"))
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().count(),
            if scenario == NativeScenario::PublicationFailure {
                3
            } else {
                2
            }
        );
        return;
    }
    if scenario != NativeScenario::Capture {
        assert!(!status.success(), "negative native scenario succeeded");
        let trace = std::fs::read_to_string(proof.join("runner.stderr.log")).unwrap();
        if scenario == NativeScenario::TrapAfterCapture {
            assert!(
                trace.contains("error: \"trapped\""),
                "expected actual Wasm trap: {trace}"
            );
            assert!(
                trace.contains("phase=capture-requested"),
                "trap bypassed native capture"
            );
            assert!(
                !trace.lines().any(
                    |line| line.starts_with("kernal-webview-trace phase=submit ")
                        && line.ends_with("opcode=10")
                ),
                "trap guest submitted output commit"
            );
            validate_teardown_trace(&trace)
                .expect("trap must reclaim captured blob and native view");
            assert_eq!(std::fs::read(&output).unwrap(), b"original");
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
            assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
            return;
        }
        if scenario == NativeScenario::MissingOutputParent {
            assert!(
                trace.contains("screenshot-write-rejected"),
                "wrong output failure: {trace}"
            );
            assert!(
                trace.contains("phase=capture-requested"),
                "write failure bypassed real capture"
            );
            validate_teardown_trace(&trace)
                .expect("write failure must reclaim captured blob and output job");
            assert!(
                !directory.exists(),
                "output job recreated its missing parent"
            );
            assert_eq!(
                std::fs::read(preserved_directory.join("viewport.png")).unwrap(),
                b"original"
            );
            assert_eq!(
                std::fs::read(preserved_directory.join("untouched")).unwrap(),
                b"unchanged"
            );
            assert_eq!(std::fs::read_dir(&preserved_directory).unwrap().count(), 2);
            return;
        }
        panic!("non-contained failure scenario needs explicit assertions");
    }
    assert!(
        status.success(),
        "actual guest runner failed: {status}; diagnostics: {}",
        proof.display()
    );
    assert!(
        start.elapsed() >= Duration::from_secs(5),
        "guest omitted its kernel wait"
    );
    let png = std::fs::read(&output).unwrap();
    assert!(png.len() >= 24 && png.len() <= 1024 * 1024);
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(&png[12..16], b"IHDR");
    let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
    assert!(width > 0 && height > 0 && u64::from(width) * u64::from(height) <= 4_000_000);
    validate_fixture_png(&png).expect("decodable native viewport with expected color regions");
    let trace = std::fs::read_to_string(proof.join("runner.stderr.log")).unwrap();
    validate_execution_trace(&trace).expect("generated ABI, native timing, and drained root trace");
    assert_eq!(std::fs::read(sentinel).unwrap(), b"unchanged");
    assert_eq!(
        std::fs::read_dir(&directory).unwrap().count(),
        2,
        "temporary output survived commit"
    );
    eprintln!(
        "{}-{}: actual Wasm screenshot {}x{}, {} PNG bytes, elapsed {:?}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        width,
        height,
        png.len(),
        start.elapsed()
    );
}

#[cfg(feature = "tauri-webview-test-support")]
fn validate_execution_trace(trace: &str) -> Result<(), &'static str> {
    fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
        line.split_ascii_whitespace().find_map(|part| {
            let (name, value) = part.split_once('=')?;
            (name == key).then_some(value)
        })
    }
    let records: Vec<_> = trace
        .lines()
        .filter(|line| line.starts_with("kernal-webview-trace "))
        .collect();
    let one = |phase: &str| -> Result<&str, &'static str> {
        let matching: Vec<_> = records
            .iter()
            .copied()
            .filter(|line| field(line, "phase") == Some(phase))
            .collect();
        if matching.len() != 1 {
            return Err("missing or duplicate lifecycle event");
        }
        Ok(matching[0])
    };
    let time = |phase| -> Result<u128, &'static str> {
        field(one(phase)?, "elapsed_us")
            .and_then(|value| value.parse().ok())
            .ok_or("invalid event timestamp")
    };
    if time("capture-requested")?
        .checked_sub(time("load-finished")?)
        .is_none_or(|delta| delta < 5_000_000)
    {
        return Err("capture preceded the five-second post-load wait");
    }
    for opcode in [13, 11, 14, 15, 12, 16, 10, 17] {
        if !records.iter().any(|line| {
            field(line, "phase") == Some("submit")
                && field(line, "opcode").and_then(|v| v.parse::<u32>().ok()) == Some(opcode)
        }) {
            return Err("missing generated operation crossing");
        }
    }
    for phase in ["poll", "yield"] {
        if !records
            .iter()
            .any(|line| field(line, "phase") == Some(phase))
        {
            return Err("missing async lifecycle crossing");
        }
    }
    validate_teardown_trace(trace)
}

#[cfg(feature = "tauri-webview-test-support")]
fn validate_teardown_trace(trace: &str) -> Result<(), &'static str> {
    fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
        line.split_ascii_whitespace().find_map(|part| {
            let (name, value) = part.split_once('=')?;
            (name == key).then_some(value)
        })
    }
    let one = |phase: &str| -> Result<&str, &'static str> {
        let matching: Vec<_> = trace
            .lines()
            .filter(|line| {
                line.starts_with("kernal-webview-trace ") && field(line, "phase") == Some(phase)
            })
            .collect();
        if matching.len() != 1 {
            return Err("missing or duplicate teardown event");
        }
        Ok(matching[0])
    };
    let drained = one("hub-drained")?;
    for counter in [
        "clocks",
        "output_jobs",
        "captures",
        "opens",
        "blobs",
        "transfer_bytes",
        "backings",
        "resources",
        "operations",
    ] {
        if field(drained, counter) != Some("0") {
            return Err("root retains native or semantic resources");
        }
    }
    let end = one("trace-end")?;
    for counter in [
        "omitted",
        "roots",
        "threads",
        "stores",
        "instances",
        "epochs",
        "memory_bytes",
    ] {
        if field(end, counter) != Some("0") {
            return Err("incomplete trace or retained Wasm state");
        }
    }
    Ok(())
}

#[cfg(feature = "tauri-webview-test-support")]
fn validate_fixture_png(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.len() > 1024 * 1024 {
        return Err("encoded image exceeds proof limit");
    }
    let mut decoder = png::Decoder::new_with_limits(
        std::io::Cursor::new(bytes),
        png::Limits {
            bytes: 32 * 1024 * 1024,
        },
    );
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|_| "invalid PNG header")?;
    let info = reader.info();
    if info.width < 4
        || info.height < 4
        || u64::from(info.width) * u64::from(info.height) > 4_000_000
    {
        return Err("invalid viewport dimensions");
    }
    let size = reader
        .output_buffer_size()
        .ok_or("decoded image exceeds proof limit")?;
    if size > 16_000_000 {
        return Err("decoded image exceeds proof limit");
    }
    let mut pixels = vec![0; size];
    let frame = reader
        .next_frame(&mut pixels)
        .map_err(|_| "invalid PNG pixels")?;
    let channels = match frame.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        _ => return Err("fixture requires RGB color regions"),
    };
    let width = frame.width as usize;
    let height = frame.height as usize;
    for y in [height / 4, height / 2, height * 3 / 4] {
        for (x, expected) in [(width / 4, [255_u8, 0, 0]), (width * 3 / 4, [0, 0, 255])] {
            let offset = (y * width + x) * channels;
            let sample = &pixels[offset..offset + channels];
            if sample[..3]
                .iter()
                .zip(expected)
                .any(|(actual, expected)| actual.abs_diff(expected) > 35)
                || (channels == 4 && sample[3] < 220)
            {
                return Err("unexpected viewport color region");
            }
        }
    }
    Ok(())
}

#[cfg(feature = "tauri-webview-test-support")]
#[test]
fn execution_trace_assertions_reject_early_capture_leaks_and_missing_crossings() {
    // Parser controls only; the ignored acceptance test still requires the
    // actual native callback and actual compiled guest to produce its trace.
    let mut trace = String::new();
    for opcode in [13, 11, 14, 15, 12, 16, 10, 17] {
        trace.push_str(&format!(
            "kernal-webview-trace phase=submit opcode={opcode}\n"
        ));
    }
    trace.push_str("kernal-webview-trace phase=poll\nkernal-webview-trace phase=yield\n");
    trace.push_str("kernal-webview-trace phase=load-finished elapsed_us=100\n");
    trace.push_str("kernal-webview-trace phase=capture-requested elapsed_us=5000100\n");
    trace.push_str("kernal-webview-trace phase=hub-drained clocks=0 output_jobs=0 captures=0 opens=0 blobs=0 transfer_bytes=0 backings=0 resources=0 operations=0\n");
    trace.push_str("kernal-webview-trace phase=trace-end omitted=0 roots=0 threads=0 stores=0 instances=0 epochs=0 memory_bytes=0\n");
    assert_eq!(validate_execution_trace(&trace), Ok(()));
    for (from, to) in [
        ("elapsed_us=5000100", "elapsed_us=5000099"),
        ("resources=0", "resources=1"),
        ("threads=0", "threads=1"),
        ("omitted=0", "omitted=1"),
        ("opcode=16", "opcode=99"),
        ("phase=yield", "phase=unknown"),
    ] {
        assert!(
            validate_execution_trace(&trace.replace(from, to)).is_err(),
            "accepted invalid trace mutation {from} -> {to}"
        );
    }
}

#[cfg(feature = "tauri-webview-test-support")]
#[test]
fn portable_png_assertions_reject_wrong_regions_and_corrupt_pixels() {
    fn fixture(swapped: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 8, 8);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            let pixels: Vec<u8> = (0..64)
                .flat_map(|index| {
                    if (index % 8 < 4) != swapped {
                        [255, 0, 0]
                    } else {
                        [0, 0, 255]
                    }
                })
                .collect();
            writer.write_image_data(&pixels).unwrap();
        }
        bytes
    }
    let valid = fixture(false);
    assert_eq!(validate_fixture_png(&valid), Ok(()));
    assert_eq!(
        validate_fixture_png(&fixture(true)),
        Err("unexpected viewport color region")
    );
    assert!(validate_fixture_png(&valid[..valid.len() / 2]).is_err());
}

#[test]
#[ignore = "build the actual screenshot guest and supply KERNAL_API_SCREENSHOT_ARTIFACT_WASM"]
fn actual_screenshot_guest_admits_but_cannot_execute_without_webview_grants() {
    let artifact = std::env::var_os("KERNAL_API_SCREENSHOT_ARTIFACT_WASM")
        .expect("explicit actual Rust screenshot artifact required");
    let bytes = std::fs::read(artifact).expect("read screenshot artifact");
    // The build script appends metadata after the real import section. Change
    // the first ABI namespace in place, preserving every section length, and
    // require rejection before compilation rather than broadening the import
    // allowlist to accommodate this smaller application.
    let mut foreign = bytes.clone();
    let offset = foreign
        .windows(b"kernal-api:v1".len())
        .position(|window| window == b"kernal-api:v1")
        .expect("actual generated ABI import namespace");
    foreign[offset..offset + b"kernal-api:v1".len()].copy_from_slice(b"evilxx-api:v1");
    let rejected = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
    assert!(matches!(
        rejected.admit(
            &foreign,
            SketchModulePolicy::threaded_rust_v1(foreign.len() + 1, 16_384).unwrap()
        ),
        Err(SketchModuleError::ForbiddenImport { .. }),
    ));
    assert_eq!(rejected.compiled_module_count(), 0);
    let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
    let policy = SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, 16_384).expect("policy");
    let sketch = compiler
        .admit(&bytes, policy)
        .expect("closed-profile admission");
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let outcome = runtime
        .run_within(
            sketch.execute_threaded_root(runtime.handle()),
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    assert_eq!(
        outcome,
        Err(SketchExecutionError::NonzeroExit { code: 17 }),
        "ungranted command must report URL-grant rejection, not trap, time out, or succeed"
    );
    assert_eq!(
        sketch.execution_limits_snapshot().active_root_executions(),
        0
    );
    assert_eq!(compiler.compiled_module_count(), 1);
}

/// Regression: the in-process runner deadline fires on a run that never
/// completes, flushes streamed progress first and names the hung stage.
#[path = "../examples/wasm-tauri-screenshot/deadline.rs"]
#[allow(dead_code)]
mod deadline;

mod runner_deadline {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    #[test]
    fn deadline_fires_on_a_run_that_never_completes_and_names_the_stage() {
        let stage = super::deadline::Stage::default();
        let ticks = Arc::new(AtomicUsize::new(0));
        let (fired, diagnostic) = mpsc::channel();
        let watchdog = {
            let ticks = ticks.clone();
            super::deadline::arm(
                Duration::from_millis(300),
                Duration::from_millis(20),
                stage.clone(),
                move || {
                    ticks.fetch_add(1, Ordering::SeqCst);
                },
                move |message| fired.send(message).unwrap(),
            )
        };
        stage.enter(0);
        // Fixture: the guarded work never completes; this channel never receives.
        let (_never_sent, never) = mpsc::channel::<()>();
        assert!(never.recv_timeout(Duration::from_millis(50)).is_err());
        let message = diagnostic
            .recv_timeout(Duration::from_secs(10))
            .expect("deadline never fired on a hung run");
        assert!(
            message.starts_with(
                "kernal-screenshot-runner deadline-elapsed stage=event-loop deadline_ms=300 "
            ),
            "{message}"
        );
        // Progress streamed during the hang and was flushed before the diagnostic.
        assert!(ticks.load(Ordering::SeqCst) >= 2, "progress did not stream");
        drop(watchdog);
    }

    #[test]
    fn disarmed_watchdog_never_fires() {
        let (fired, diagnostic) = mpsc::channel();
        let watchdog = super::deadline::arm(
            Duration::from_millis(200),
            Duration::from_millis(20),
            super::deadline::Stage::default(),
            || {},
            move |message| fired.send(message).unwrap(),
        );
        watchdog.disarm();
        assert!(diagnostic.recv_timeout(Duration::from_millis(400)).is_err());
    }

    #[test]
    fn stage_names_the_drain() {
        let stage = super::deadline::Stage::default();
        stage.enter(2);
        assert_eq!(stage.name(), "drain");
        assert_eq!(
            super::deadline::diagnostic(
                stage.name(),
                Duration::from_secs(75),
                Duration::from_millis(75_001)
            ),
            "kernal-screenshot-runner deadline-elapsed stage=drain deadline_ms=75000 elapsed_ms=75001"
        );
    }
}
