//! Private one-request Wasm worker. Parent supervision and forced tree reaping
//! are deliberately phase-D responsibilities.

#[path = "../wasm/worker_protocol.rs"]
mod worker_protocol;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kernal_api::async_engine::{CancellationSource, RuntimeBuilder};
use kernal_api::wasm::{
    SketchCompiler, SketchCompilerConfig, SketchEpochLimits, SketchExecutionError,
    SketchExecutionLimits, SketchFuelLimits, SketchModulePolicy, ThreadedRootOutcome,
};
use worker_protocol::{
    read_message, write_message, ExecuteMetadata, FinalCounters, Message, ModuleAssembler,
    ProtocolError, RootOutcome, TerminalDetail, TerminalKind,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("kernal-wasm-worker: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut input = io::stdin();
    let mut output = io::stdout();
    let Some((request_id, metadata, module)) = run_with_io(&mut input, &mut output)? else {
        return Ok(());
    };
    execute_request(request_id, metadata, module, &mut output)
}

/// Consume the bounded pre-execution request sequence and write only its
/// protocol-level response. Keeping this independent of stdin's cancellation
/// reader makes the framing state machine testable without a child process.
fn run_with_io<R: io::Read, W: io::Write>(
    input: &mut R,
    output: &mut W,
) -> Result<Option<(u64, ExecuteMetadata, Vec<u8>)>, String> {
    let hello = read_message(input).map_err(protocol_text)?;
    let request_id = match hello {
        Message::Hello { request_id } => request_id,
        _ => return Err("expected hello".into()),
    };
    write_message(output, &Message::HelloAck { request_id }).map_err(protocol_text)?;
    let start = read_message(input).map_err(protocol_text)?;
    let (module_len, metadata) = match start {
        Message::ExecuteStart {
            request_id: id,
            module_len,
            metadata,
        } if id == request_id => (module_len, metadata),
        _ => {
            protocol_terminal(output, request_id, "expected matching execute-start")?;
            return Ok(None);
        }
    };
    let mut assembler = ModuleAssembler::start(request_id, module_len, metadata.max_module_bytes)
        .map_err(protocol_text)?;
    let module = loop {
        let message = read_message(input).map_err(protocol_text)?;
        if let Some(module) = assembler.accept(message).map_err(protocol_text)? {
            break module;
        }
    };
    write_message(output, &Message::ExecuteAck { request_id }).map_err(protocol_text)?;
    Ok(Some((request_id, metadata, module)))
}

#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn execute_request(
    request_id: u64,
    metadata: ExecuteMetadata,
    module: Vec<u8>,
    output: &mut impl io::Write,
) -> Result<(), String> {
    #[cfg(not(feature = "tauri-webview"))]
    if metadata.webview_url.is_some() {
        return protocol_terminal(output, request_id, "native-webview-worker-unavailable");
    }
    #[cfg(feature = "tauri-webview")]
    let native_grant = match native_grant(&metadata) {
        Ok(grant) => grant,
        Err(error) => return protocol_terminal(output, request_id, error),
    };
    let (config, policy) = reconstruct(&metadata).map_err(|text| {
        protocol_terminal(output, request_id, &text)
            .err()
            .unwrap_or(text)
    })?;
    let cancellation = CancellationSource::new();
    let token = cancellation.token();
    let control_done = Arc::new(AtomicBool::new(false));
    let control_done_thread = Arc::clone(&control_done);
    let source = cancellation.clone();
    std::thread::spawn(move || {
        let mut control = io::stdin();
        match read_message(&mut control) {
            Ok(Message::Cancel { request_id: id }) if id == request_id => source.cancel(),
            Ok(_) | Err(_) => source.cancel(),
        }
        control_done_thread.store(true, Ordering::Release);
    });
    let runtime_builder = if metadata.webview_url.is_some() {
        RuntimeBuilder::multi_thread()
    } else {
        RuntimeBuilder::current_thread()
    };
    let runtime = runtime_builder
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let compiler = match SketchCompiler::new(config) {
        Ok(value) => value,
        Err(error) => return protocol_terminal(output, request_id, error.to_string().as_str()),
    };
    let sketch = match compiler.admit(&module, policy) {
        Ok(value) => Arc::new(value),
        Err(error) => return protocol_terminal(output, request_id, error.to_string().as_str()),
    };
    let execute_plain = || {
        runtime
            .run_within(
                async {
                    match metadata.staged_output {
                        Some(destination) => {
                            sketch
                                .execute_threaded_root_with_output(
                                    runtime.handle(),
                                    token.clone(),
                                    destination,
                                )
                                .await
                        }
                        None => {
                            sketch
                                .execute_threaded_root_cancellable(runtime.handle(), token.clone())
                                .await
                        }
                    }
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget")
    };
    #[cfg(not(feature = "tauri-webview"))]
    let result = execute_plain();
    #[cfg(feature = "tauri-webview")]
    let mut trace = None;
    #[cfg(feature = "tauri-webview")]
    let result = match native_grant {
        Some((grant, destination)) => execute_native(
            &runtime,
            Arc::clone(&sketch),
            token,
            grant,
            destination,
            &mut trace,
        ),
        None => execute_plain(),
    };
    let _ = sketch.close_threaded_root();
    drop(sketch);
    let counters = snapshot(&compiler);
    #[cfg(feature = "tauri-webview")]
    if let Some(mut text) = trace {
        use std::fmt::Write as _;
        let counts = compiler.execution_limits_snapshot();
        writeln!(
            text,
            " roots={} threads={} stores={} instances={} epochs={} memory_bytes={}",
            counts.active_root_executions(),
            counts.live_guest_threads(),
            counts.live_stores(),
            counts.live_instances(),
            counts.active_epoch_registrations(),
            counts.reserved_shared_memory_bytes()
        )
        .expect("format trace");
        write_message(output, &Message::Trace { request_id, text }).map_err(protocol_text)?;
    }
    let (kind, detail, diagnostic) = map_result(result);
    let (kind, detail, diagnostic) = if counters_are_zero(counters) {
        (kind, detail, diagnostic)
    } else {
        (
            TerminalKind::WorkerFailure,
            TerminalDetail::none(),
            "nonzero-worker-counters".into(),
        )
    };
    write_message(
        output,
        &Message::Terminal {
            request_id,
            kind,
            detail,
            diagnostic: bound(diagnostic),
            counters,
        },
    )
    .map_err(protocol_text)?;
    let _ = control_done.load(Ordering::Acquire);
    Ok(())
}

#[cfg(feature = "tauri-webview")]
fn native_grant(
    metadata: &ExecuteMetadata,
) -> Result<Option<(kernal_api::webview::WebviewUrlGrant, std::path::PathBuf)>, &'static str> {
    let Some(url) = metadata.webview_url.as_deref() else {
        return Ok(None);
    };
    let grant =
        kernal_api::webview::WebviewUrlGrant::new(url).map_err(|_| "invalid-native-webview-url")?;
    let destination = metadata
        .staged_output
        .clone()
        .ok_or("native-webview-output-required")?;
    Ok(Some((grant, destination)))
}

/// The UI loop stays on process main; the root runs on this worker's runtime.
/// The parent remains responsible for the hard deadline and process-tree reap.
#[cfg(feature = "tauri-webview")]
fn execute_native(
    runtime: &kernal_api::async_engine::Runtime,
    sketch: Arc<kernal_api::wasm::AdmittedSketch>,
    token: kernal_api::async_engine::CancellationToken,
    grant: kernal_api::webview::WebviewUrlGrant,
    destination: std::path::PathBuf,
    _trace: &mut Option<String>,
) -> Result<ThreadedRootOutcome, SketchExecutionError> {
    let host = kernal_api::webview::ExternalWebviewHost::new(runtime.handle())
        .map_err(|_| SketchExecutionError::WebviewGrantRejected)?;
    let client = host.client();
    #[cfg(feature = "tauri-webview-test-support")]
    let observer = client.clone();
    let handle = runtime.handle();
    let task = handle.clone().launch(async move {
        // Unwinding or dropping the task must also release the UI loop.
        struct ExitOnDrop(kernal_api::webview::ExternalWebviewClient);
        impl Drop for ExitOnDrop {
            fn drop(&mut self) {
                let _ = self.0.request_exit();
            }
        }
        let _exit = ExitOnDrop(client.clone());
        sketch
            .execute_threaded_root_with_webview(handle, token, client.clone(), grant, destination)
            .await
    });
    host.run();
    let result = runtime
        .run_within(task, std::time::Duration::from_secs(60))
        .expect("test runtime budget")
        .map_err(|_| SketchExecutionError::BlockingTaskFailed)?;
    #[cfg(feature = "tauri-webview-test-support")]
    {
        use std::fmt::Write as _;
        let (events, omitted) = observer.test_trace();
        let mut text = String::new();
        for event in events {
            write!(
                text,
                "kernal-webview-trace phase={} elapsed_us={} opcode={}",
                event.phase,
                event.elapsed.as_micros(),
                event.opcode.unwrap_or(0)
            )
            .expect("format trace");
            if let Some(counts) = event.observation {
                write!(text, " clocks={} output_jobs={} captures={} opens={} blobs={} transfer_bytes={} backings={} resources={} operations={}", counts.active_clocks, counts.active_output_jobs, counts.active_native_captures, counts.active_native_opens, counts.live_blobs, counts.retained_transfer_capacity, counts.native_backings, counts.live_resources, counts.pending_operations).expect("format trace");
            }
            text.push('\n');
        }
        write!(
            text,
            "kernal-webview-trace phase=trace-end omitted={omitted}"
        )
        .expect("format trace");
        *_trace = Some(text);
    }
    result
}

fn reconstruct(
    metadata: &ExecuteMetadata,
) -> Result<(SketchCompilerConfig, SketchModulePolicy), String> {
    let mut blob_values = [0_usize; 7];
    for (destination, source) in blob_values.iter_mut().zip(metadata.blob_limits) {
        *destination = usize::try_from(source).map_err(|_| "blob-limit-overflow")?;
    }
    let [chunk, blob, sketch, live, reads, writes, transfer] = blob_values;
    let idle_nanos = u32::try_from(metadata.blob_progress_idle_timeout_nanos)
        .map_err(|_| "blob-idle-timeout-nanos-overflow")?;
    if idle_nanos >= 1_000_000_000 {
        return Err("blob-idle-timeout-nanos-invalid".into());
    }
    let blobs = kernal_api::wasm::SketchBlobLimits::new(chunk, blob, sketch, live, reads, writes)
        .map_err(|e| e.to_string())?
        .with_maximum_transfer_bytes(transfer)
        .map_err(|e| e.to_string())?
        .with_progress_idle_timeout(Duration::new(
            metadata.blob_progress_idle_timeout_secs,
            idle_nanos,
        ))
        .map_err(|e| e.to_string())?;
    let roots =
        usize::try_from(metadata.maximum_active_roots).map_err(|_| "active-roots-overflow")?;
    let stack = usize::try_from(metadata.max_wasm_stack_bytes).map_err(|_| "stack-overflow")?;
    let threads =
        usize::try_from(metadata.max_guest_threads).map_err(|_| "thread-limit-overflow")?;
    let fuel = SketchFuelLimits::new(metadata.total_fuel, metadata.root_fuel, metadata.child_fuel)
        .map_err(|e| e.to_string())?;
    let epoch = SketchEpochLimits::new(
        Duration::from_millis(metadata.epoch_deadline_millis),
        Duration::from_millis(metadata.epoch_tick_millis),
        usize::try_from(metadata.maximum_epoch_registrations)
            .map_err(|_| "epoch-registration-overflow")?,
    )
    .map_err(|e| e.to_string())?;
    let limits = SketchExecutionLimits::new(metadata.reserved_memory_bytes, roots)
        .map_err(|e| e.to_string())?
        .with_fuel_limits(fuel)
        .map_err(|e| e.to_string())?
        .with_epoch_limits(epoch)
        .map_err(|e| e.to_string())?
        .with_blob_limits(blobs);
    let config = SketchCompilerConfig::new(stack)
        .map_err(|e| e.to_string())?
        .with_execution_limits(limits)
        .map_err(|e| e.to_string())?;
    let policy = SketchModulePolicy::threaded_rust_v1(
        usize::try_from(metadata.max_module_bytes).map_err(|_| "module-limit-overflow")?,
        metadata.max_shared_memory_pages,
    )
    .map_err(|e| e.to_string())?
    .with_max_guest_threads(threads)
    .map_err(|e| e.to_string())?;
    Ok((config, policy))
}

fn map_result(
    result: Result<ThreadedRootOutcome, SketchExecutionError>,
) -> (TerminalKind, TerminalDetail, String) {
    match result {
        Ok(ThreadedRootOutcome::Started) => (
            TerminalKind::Completed,
            TerminalDetail {
                root_outcome: RootOutcome::Started,
                ..TerminalDetail::none()
            },
            "started".into(),
        ),
        Ok(ThreadedRootOutcome::Exited) => (
            TerminalKind::Completed,
            TerminalDetail {
                root_outcome: RootOutcome::Exited,
                ..TerminalDetail::none()
            },
            "exited".into(),
        ),
        Ok(ThreadedRootOutcome::StartedWithThreadRejections(r)) => (
            TerminalKind::Completed,
            rejections(RootOutcome::StartedWithThreadRejections, r),
            "started".into(),
        ),
        Ok(ThreadedRootOutcome::ExitedWithThreadRejections(r)) => (
            TerminalKind::Completed,
            rejections(RootOutcome::ExitedWithThreadRejections, r),
            "exited".into(),
        ),
        Err(SketchExecutionError::Cancelled) => (
            TerminalKind::Cancelled,
            TerminalDetail::none(),
            "cancelled".into(),
        ),
        Err(SketchExecutionError::DeadlineExceeded) => (
            TerminalKind::DeadlineExceeded,
            TerminalDetail::none(),
            "deadline-exceeded".into(),
        ),
        Err(SketchExecutionError::OutOfFuel) => (
            TerminalKind::OutOfFuel,
            TerminalDetail::none(),
            "out-of-fuel".into(),
        ),
        Err(SketchExecutionError::Trapped) => (
            TerminalKind::Trapped,
            TerminalDetail::none(),
            "trapped".into(),
        ),
        Err(SketchExecutionError::NonzeroExit { code }) => (
            TerminalKind::NonzeroExit,
            TerminalDetail {
                status_code: Some(code),
                ..TerminalDetail::none()
            },
            "nonzero-exit".into(),
        ),
        Err(SketchExecutionError::ChildNonzeroExit { code }) => (
            TerminalKind::ChildFailure,
            TerminalDetail {
                status_code: Some(code),
                ..TerminalDetail::none()
            },
            "child-nonzero-exit".into(),
        ),
        Err(SketchExecutionError::ChildTrapped)
        | Err(SketchExecutionError::ChildPanicked)
        | Err(SketchExecutionError::ChildOutcomes { .. }) => (
            TerminalKind::ChildFailure,
            TerminalDetail::none(),
            "child-failure".into(),
        ),
        Err(error) => (
            TerminalKind::WorkerFailure,
            TerminalDetail::none(),
            error.code().into(),
        ),
    }
}
fn rejections(
    root_outcome: RootOutcome,
    value: kernal_api::wasm::ThreadSpawnRejectionSummary,
) -> TerminalDetail {
    TerminalDetail {
        root_outcome,
        capacity_rejections: value.capacity(),
        closing_rejections: value.closing(),
        fuel_rejections: value.fuel(),
        epoch_rejections: value.epoch(),
        status_code: None,
    }
}

fn snapshot(compiler: &SketchCompiler) -> FinalCounters {
    let value = compiler.execution_limits_snapshot();
    FinalCounters {
        active_roots: value.active_root_executions() as u64,
        live_stores: value.live_stores() as u64,
        live_instances: value.live_instances() as u64,
        active_epoch_registrations: value.active_epoch_registrations() as u64,
        live_threads: value.live_guest_threads() as u64,
    }
}
fn counters_are_zero(value: FinalCounters) -> bool {
    value.active_roots == 0
        && value.live_stores == 0
        && value.live_instances == 0
        && value.active_epoch_registrations == 0
        && value.live_threads == 0
}
fn bound(mut text: String) -> String {
    text.truncate(1024);
    text
}
fn protocol_text(error: ProtocolError) -> String {
    format!("protocol:{error:?}")
}
fn protocol_terminal(
    output: &mut impl io::Write,
    request_id: u64,
    text: &str,
) -> Result<(), String> {
    write_message(
        output,
        &Message::Terminal {
            request_id,
            kind: TerminalKind::ProtocolFailure,
            detail: TerminalDetail::none(),
            diagnostic: bound(text.into()),
            counters: FinalCounters {
                active_roots: 0,
                live_stores: 0,
                live_instances: 0,
                active_epoch_registrations: 0,
                live_threads: 0,
            },
        },
    )
    .map_err(protocol_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(message: &Message) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_message(&mut bytes, message).unwrap();
        bytes
    }

    #[test]
    fn reconstruction_preserves_nondefault_blob_limits_and_rejects_invalid_ones() {
        let mut metadata = metadata();
        metadata.reserved_memory_bytes = 1024 * 1024 * 1024;
        metadata.max_shared_memory_pages = 16_384;
        let (config, _) = reconstruct(&metadata).unwrap();
        assert_eq!(
            config.execution_limits().blob_limits(),
            kernal_api::wasm::SketchBlobLimits::new(4, 8, 16, 2, 3, 4)
                .unwrap()
                .with_maximum_transfer_bytes(24)
                .unwrap()
                .with_progress_idle_timeout(Duration::new(9, 123))
                .unwrap()
        );
        metadata.blob_limits[0] = 0;
        assert!(reconstruct(&metadata).is_err());
        metadata.blob_limits[0] = u64::MAX;
        assert!(reconstruct(&metadata).is_err());
        metadata.blob_limits[0] = 4;
        metadata.blob_progress_idle_timeout_secs = 0;
        metadata.blob_progress_idle_timeout_nanos = 0;
        assert!(reconstruct(&metadata).is_err());
        metadata.blob_progress_idle_timeout_nanos = 1_000_000_000;
        assert!(reconstruct(&metadata).is_err());
    }

    #[test]
    fn staging_grant_does_not_change_compiler_limit_reconstruction() {
        let mut metadata = metadata();
        metadata.reserved_memory_bytes = 1024 * 1024 * 1024;
        metadata.max_shared_memory_pages = 16_384;
        metadata.staged_output = Some(std::env::temp_dir().join("completed-output"));
        assert!(reconstruct(&metadata).is_ok());
    }

    #[test]
    #[cfg(not(feature = "tauri-webview"))]
    fn unsupported_native_authority_is_rejected_before_module_execution() {
        let mut request = metadata();
        request.webview_url = Some("https://example.test/".into());
        let mut output = Vec::new();
        execute_request(7, request, Vec::new(), &mut output).unwrap();
        assert!(
            matches!(read_message(&mut output.as_slice()).unwrap(), Message::Terminal {
            request_id: 7,
            kind: TerminalKind::ProtocolFailure,
            diagnostic,
            ..
        } if diagnostic == "native-webview-worker-unavailable")
        );
    }

    #[test]
    #[cfg(feature = "tauri-webview")]
    fn native_authority_is_revalidated_without_starting_ui_or_compiler() {
        let mut request = metadata();
        assert!(native_grant(&request).unwrap().is_none());
        for url in [
            "file:///tmp/input",
            "javascript:alert(1)",
            "",
            "https://user:password@example.test/",
        ] {
            request.webview_url = Some(url.into());
            assert!(matches!(
                native_grant(&request),
                Err("invalid-native-webview-url")
            ));
            let mut output = Vec::new();
            execute_request(7, request.clone(), Vec::new(), &mut output).unwrap();
            assert!(
                matches!(read_message(&mut output.as_slice()).unwrap(), Message::Terminal {
                kind: TerminalKind::ProtocolFailure, diagnostic, ..
            } if diagnostic == "invalid-native-webview-url")
            );
        }
        request.webview_url = Some("https://example.test/exact".into());
        assert!(matches!(
            native_grant(&request),
            Err("native-webview-output-required")
        ));
        let destination = std::env::temp_dir().join("worker-native-grant-test.png");
        request.staged_output = Some(destination.clone());
        assert_eq!(native_grant(&request).unwrap().unwrap().1, destination);
    }

    fn metadata() -> ExecuteMetadata {
        ExecuteMetadata {
            webview_url: None,
            staged_output: None,
            blob_limits: [4, 8, 16, 2, 3, 4, 24],
            blob_progress_idle_timeout_secs: 9,
            blob_progress_idle_timeout_nanos: 123,
            max_wasm_stack_bytes: 1,
            reserved_memory_bytes: 1,
            maximum_active_roots: 1,
            total_fuel: 3,
            root_fuel: 1,
            child_fuel: 1,
            epoch_deadline_millis: 1,
            epoch_tick_millis: 1,
            maximum_epoch_registrations: 1,
            max_module_bytes: 1,
            max_shared_memory_pages: 1,
            max_guest_threads: 1,
        }
    }

    #[test]
    fn in_memory_handshake_then_wrong_start_writes_typed_protocol_terminal() {
        let request_id = 41;
        let mut input = frame(&Message::Hello { request_id });
        input.extend(frame(&Message::Cancel { request_id }));
        let mut output = Vec::new();

        assert!(run_with_io(&mut Cursor::new(input), &mut output)
            .unwrap()
            .is_none());

        let mut output = Cursor::new(output);
        assert_eq!(
            read_message(&mut output).unwrap(),
            Message::HelloAck { request_id }
        );
        let Message::Terminal {
            request_id: terminal_request,
            kind,
            detail,
            counters,
            ..
        } = read_message(&mut output).unwrap()
        else {
            panic!("expected terminal");
        };
        assert_eq!(terminal_request, request_id);
        assert_eq!(kind, TerminalKind::ProtocolFailure);
        assert_eq!(detail, TerminalDetail::none());
        assert!(counters_are_zero(counters));
    }

    #[test]
    fn complete_upload_orders_execute_ack_after_hello_ack() {
        let request_id = 41;
        let mut input = frame(&Message::Hello { request_id });
        input.extend(frame(&Message::ExecuteStart {
            request_id,
            module_len: 0,
            metadata: metadata(),
        }));
        input.extend(frame(&Message::ExecuteEnd { request_id }));
        let mut output = Vec::new();

        assert!(run_with_io(&mut Cursor::new(input), &mut output)
            .expect("complete request")
            .is_some());

        let mut output = Cursor::new(output);
        assert_eq!(
            read_message(&mut output).unwrap(),
            Message::HelloAck { request_id }
        );
        assert_eq!(
            read_message(&mut output).unwrap(),
            Message::ExecuteAck { request_id }
        );
    }

    #[test]
    fn incomplete_upload_never_writes_execute_ack() {
        let request_id = 41;
        let mut input = frame(&Message::Hello { request_id });
        input.extend(frame(&Message::ExecuteStart {
            request_id,
            module_len: 1,
            metadata: metadata(),
        }));
        input.extend(frame(&Message::ExecuteEnd { request_id }));
        let mut output = Vec::new();

        assert!(run_with_io(&mut Cursor::new(input), &mut output).is_err());

        let mut output = Cursor::new(output);
        assert_eq!(
            read_message(&mut output).unwrap(),
            Message::HelloAck { request_id }
        );
        assert!(read_message(&mut output).is_err());
    }

    #[test]
    fn result_mapping_uses_typed_status_and_root_outcome_fields() {
        let (kind, detail, _) = map_result(Ok(ThreadedRootOutcome::Started));
        assert_eq!(kind, TerminalKind::Completed);
        assert_eq!(detail.root_outcome, RootOutcome::Started);
        let (kind, detail, _) = map_result(Err(SketchExecutionError::NonzeroExit { code: -7 }));
        assert_eq!(kind, TerminalKind::NonzeroExit);
        assert_eq!(detail.status_code, Some(-7));
    }
}
