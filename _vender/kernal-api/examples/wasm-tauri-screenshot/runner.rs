//! Source-tree CLI for the actual Wasm screenshot application.
use kernal_api::async_engine::{CancellationSource, RuntimeBuilder};
use kernal_api::wasm::{SketchCompiler, SketchCompilerConfig, SketchModulePolicy};
use kernal_api::webview::{ExternalWebviewHost, WebviewUrlGrant};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
mod deadline;
mod status;

/// Bound on the whole in-process run. Legitimate runs take up to ~36s (30s
/// load hold plus the 5s guest sleep); the test's 90s stall guard must never
/// be what notices a hang.
const IN_PROCESS_DEADLINE: Duration = Duration::from_secs(75);
/// Exit status for an elapsed in-process deadline.
const DEADLINE_EXIT: i32 = 124;

/// Print webview trace events not yet printed. Called by the watchdog while
/// the run is live and once at completion, so a hang still shows its trace.
#[cfg(feature = "tauri-webview-test-support")]
fn emit_webview_trace(
    client: &kernal_api::webview::ExternalWebviewClient,
    printed: &std::sync::Mutex<usize>,
) -> usize {
    let mut printed = printed.lock().unwrap_or_else(|poison| poison.into_inner());
    let (events, omitted) = client.test_trace();
    for event in events.iter().skip(*printed) {
        eprint!(
            "kernal-webview-trace phase={} elapsed_us={} opcode={}",
            event.phase,
            event.elapsed.as_micros(),
            event.opcode.unwrap_or(0)
        );
        if let Some(counts) = event.observation {
            eprint!(
                " clocks={} output_jobs={}",
                counts.active_clocks, counts.active_output_jobs
            );
            eprint!(" captures={} opens={} blobs={} transfer_bytes={} backings={} resources={} operations={}", counts.active_native_captures, counts.active_native_opens, counts.live_blobs, counts.retained_transfer_capacity, counts.native_backings, counts.live_resources, counts.pending_operations);
        }
        eprintln!();
    }
    *printed = (*printed).max(events.len());
    omitted
}

fn build_guest() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let guest = repo.join("examples/wasm-tauri-screenshot/guest");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.join("target/screenshot-proof"));
    let target = std::path::absolute(target)?.join("kernal-api-wasm-tauri-guest");
    let built = target.join("wasm32-wasip1-threads/release/kernal-api-wasm-tauri-guest.wasm");
    let admitted = built.with_file_name("kernal-api-wasm-tauri-guest.admitted.wasm");
    if !Command::new("soldr")
        .current_dir(guest)
        .env("SOLDR_LINKER", "default")
        .args([
            "cargo",
            "build",
            "--locked",
            "--release",
            "--target",
            "wasm32-wasip1-threads",
            "--target-dir",
        ])
        .arg(&target)
        .status()?
        .success()
    {
        return Err("guest-build-failed".into());
    }
    std::fs::copy(built, &admitted)?;
    if !Command::new("soldr")
        .current_dir(repo)
        .args([
            "cargo",
            "run",
            "--locked",
            "--manifest-path",
            "tools/wasm-abi-generator/Cargo.toml",
            "--",
            "--embed-threaded-metadata",
        ])
        .arg(&admitted)
        .status()?
        .success()
    {
        return Err("guest-metadata-failed".into());
    }
    Ok(admitted)
}

#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let (mut url, mut output, mut module) = (None, None, None);
    let mut worker = None;
    let diagnostic_in_process = false;
    #[cfg(feature = "tauri-webview-test-support")]
    let mut diagnostic_in_process = diagnostic_in_process;
    while let Some(flag) = arguments.next() {
        #[cfg(feature = "tauri-webview-test-support")]
        if flag == "--diagnostic-in-process" && !diagnostic_in_process {
            diagnostic_in_process = true;
            continue;
        }
        let value = arguments.next().ok_or("option requires a value")?;
        match flag.to_str() {
            Some("--url") if url.is_none() => url = Some(value),
            Some("--output") if output.is_none() => output = Some(PathBuf::from(value)),
            Some("--module") if module.is_none() => module = Some(PathBuf::from(value)),
            Some("--worker") if worker.is_none() => worker = Some(PathBuf::from(value)),
            _ => return Err("usage: kernal-api-wasm-tauri --url <http(s)-url> --output <png> [--module <built-wasm>] [--worker <native-worker>]".into()),
        }
    }
    if diagnostic_in_process && worker.is_some() {
        return Err("--worker cannot be combined with --diagnostic-in-process".into());
    }
    let url = url.ok_or("--url is required")?;
    let grant = WebviewUrlGrant::new(url.to_str().ok_or("URL must be UTF-8")?)?;
    let output = std::path::absolute(output.ok_or("--output is required")?)?;
    let parent = std::fs::canonicalize(output.parent().ok_or("output needs a parent")?)?;
    let output = parent.join(output.file_name().ok_or("output needs a filename")?);
    if output.is_dir() {
        return Err("output must be a file".into());
    }
    let module = match module {
        Some(module) => module,
        None => build_guest()?,
    };
    // Bound the native module upload independently of any guest blob budget.
    use std::io::Read as _;
    const MAX_MODULE_BYTES: usize = 32 * 1024 * 1024;
    let mut bytes = Vec::new();
    std::fs::File::open(module)?
        .take((MAX_MODULE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let policy = SketchModulePolicy::threaded_rust_v1(MAX_MODULE_BYTES, 16_384)?;
    // The overall command deadline must leave room for the production
    // 30-second load/capture/close deadlines and five-second guest sleep.
    // Do not let the compiler's generic 30-second default mask a typed step
    // timeout. This remains finite and does not replace worker containment.
    let epochs = kernal_api::wasm::SketchEpochLimits::default();
    let compiler = SketchCompiler::new(SketchCompilerConfig::default().with_epoch_limits(
        kernal_api::wasm::SketchEpochLimits::new(
            Duration::from_secs(120),
            epochs.tick_interval(),
            epochs.maximum_active_registrations(),
        )?,
    )?)?;
    let sketch = std::sync::Arc::new(compiler.admit(&bytes, policy)?);
    let runtime = RuntimeBuilder::multi_thread().enable_all().build()?;
    if !diagnostic_in_process {
        use kernal_api::wasm::{SketchWorkerConfig, SketchWorkerTerminal};
        let executable = match worker {
            Some(path) => std::path::absolute(path)?,
            None => std::env::current_exe()?.with_file_name(format!(
                "kernal-wasm-worker{}",
                std::env::consts::EXE_SUFFIX
            )),
        };
        if !executable.is_file() {
            return Err("native worker missing; build both binaries with wasm-sketch-worker,tauri-webview or supply --worker".into());
        }
        let config = SketchWorkerConfig::new(executable, Duration::from_secs(2))?
            .with_webview_capture(grant, output)?;
        #[cfg(feature = "tauri-webview-test-support")]
        let trace = kernal_api::wasm::SketchWorkerTrace::default();
        #[cfg(feature = "tauri-webview-test-support")]
        let config = config.with_trace(trace.clone());
        let terminal = runtime
            .run_within(
                sketch.execute_threaded_root_contained(runtime.handle(), &config),
                Duration::from_secs(60),
            )
            .expect("screenshot runner budget");
        #[cfg(feature = "tauri-webview-test-support")]
        {
            if let Some(text) = trace.take() {
                eprint!("{text}");
            }
            let counts = sketch.worker_execution_snapshot();
            eprintln!("kernal-worker-trace terminal={} spawned={} reaped={} forced={} workers={} tasks={} leases={}", terminal.code(), counts.spawned, counts.reaped, counts.forced, counts.live_workers, counts.live_protocol_tasks, counts.pending_root_leases);
        }
        return match terminal {
            SketchWorkerTerminal::Completed(_) => Ok(()),
            SketchWorkerTerminal::Execution(error) => Err(screenshot_error(error).into()),
            other => Err(std::io::Error::other(other.code()).into()),
        };
    }
    let host = ExternalWebviewHost::new(runtime.handle())?;
    let client = host.client();
    let handle = runtime.handle();
    let stage = deadline::Stage::default();
    #[cfg(feature = "tauri-webview-test-support")]
    let printed = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let progress = {
        #[cfg(feature = "tauri-webview-test-support")]
        let (client, printed) = (client.clone(), printed.clone());
        move || {
            #[cfg(feature = "tauri-webview-test-support")]
            emit_webview_trace(&client, &printed);
        }
    };
    let watchdog = deadline::arm(
        IN_PROCESS_DEADLINE,
        Duration::from_millis(250),
        stage.clone(),
        progress,
        |message| {
            eprintln!("{message}");
            std::process::exit(DEADLINE_EXIT);
        },
    );
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let task = handle.clone().launch(async move {
        let result = sketch
            .execute_threaded_root_with_webview(
                handle,
                CancellationSource::new().token(),
                client.clone(),
                grant,
                output,
            )
            .await;
        #[cfg(feature = "tauri-webview-test-support")]
        {
            let omitted = emit_webview_trace(&client, &printed);
            drop(sketch);
            let counts = compiler.execution_limits_snapshot();
            eprintln!("kernal-webview-trace phase=trace-end omitted={} roots={} threads={} stores={} instances={} epochs={} memory_bytes={}", omitted, counts.active_root_executions(), counts.live_guest_threads(), counts.live_stores(), counts.live_instances(), counts.active_epoch_registrations(), counts.reserved_shared_memory_bytes());
        }
        let _ = sender.send(result);
        let _ = client.request_exit();
    });
    stage.enter(0);
    host.run();
    stage.enter(1);
    let result = receiver.recv_timeout(Duration::from_secs(60))?;
    stage.enter(2);
    runtime
        .run_within(task, Duration::from_secs(60))
        .expect("screenshot runner budget")?;
    watchdog.disarm();
    result.map_err(screenshot_error)?;
    Ok(())
}

fn screenshot_error(error: kernal_api::wasm::SketchExecutionError) -> std::io::Error {
    if let kernal_api::wasm::SketchExecutionError::NonzeroExit { code } = error {
        if let Some(failure) = status::Failure::from_code(code as u32) {
            debug_assert_eq!(failure.code(), code as u32);
            return std::io::Error::other(format!(
                "screenshot-{}-{}",
                failure.step.name(),
                failure.cause.name()
            ));
        }
        return std::io::Error::other(format!("nonzero-exit:{code}"));
    }
    std::io::Error::other(error.code())
}
