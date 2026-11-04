/// Stream a completed compile's captured output through `sink`, the SESSION
/// `0x5350` wire (`session_sink::SessionCompileSink`), over one embedded-zccache
/// execution (soldr#2388 Step 5/6). Owns the disconnect error attribution and
/// per-compile telemetry for the write.
pub(crate) async fn stream_compile_output<Sink>(
    sink: &mut Sink,
    body: &crate::daemon::protocol::CompileResponseBody,
    paths: &crate::core::SoldrPaths,
    compile_id: &str,
    lifecycle: Option<&crate::daemon::protocol::CompileLifecycle>,
    inner_started: std::time::Instant,
    total: std::time::Instant,
) -> std::io::Result<()>
where
    Sink: crate::daemon::compile_sink::CompileOutputSink,
{
    let stdout_len = body.stdout.len();
    let stderr_len = body.stderr.len();
    let mut stdout_chunks = 0usize;
    let mut stderr_chunks = 0usize;

    let wire_stdout_started = std::time::Instant::now();
    for chunk in body.stdout.chunks(CHUNK_BYTES) {
        if let Err(err) = sink.emit_stdout_chunk(chunk).await {
            return Err(crate::daemon::disconnect::report_reply_write_failure(
                paths,
                compile_id,
                lifecycle,
                inner_started,
                "stdout_chunk",
                err,
                body.exit_code,
            ));
        }
        stdout_chunks += 1;
        tracing::debug!(
            target: "soldr::daemon::compile_stream",
            bytes = chunk.len(),
            chunk_index = stdout_chunks - 1,
            "stdout chunk emitted",
        );
    }
    crate::daemon::compile_trace::record(
        "wire_stdout",
        wire_stdout_started.elapsed().as_micros() as u64,
        compile_id,
    );

    let wire_stderr_started = std::time::Instant::now();
    for chunk in body.stderr.chunks(CHUNK_BYTES) {
        if let Err(err) = sink.emit_stderr_chunk(chunk).await {
            return Err(crate::daemon::disconnect::report_reply_write_failure(
                paths,
                compile_id,
                lifecycle,
                inner_started,
                "stderr_chunk",
                err,
                body.exit_code,
            ));
        }
        stderr_chunks += 1;
        tracing::debug!(
            target: "soldr::daemon::compile_stream",
            bytes = chunk.len(),
            chunk_index = stderr_chunks - 1,
            "stderr chunk emitted",
        );
    }
    crate::daemon::compile_trace::record(
        "wire_stderr",
        wire_stderr_started.elapsed().as_micros() as u64,
        compile_id,
    );

    tracing::debug!(
        target: "soldr::daemon::compile_stream",
        exit_code = body.exit_code,
        cached = body.cached,
        cache_outcome = body.cache_outcome,
        stdout_bytes = stdout_len,
        stderr_bytes = stderr_len,
        stdout_chunks,
        stderr_chunks,
        "compile done — streaming reply complete",
    );
    let wire_done_started = std::time::Instant::now();
    let res = sink
        .emit_done(body.exit_code, body.cached, body.cache_outcome, compile_id)
        .await
        .map_err(|err| {
            crate::daemon::disconnect::report_reply_write_failure(
                paths,
                compile_id,
                lifecycle,
                inner_started,
                "done",
                err,
                body.exit_code,
            )
        });
    crate::daemon::compile_trace::record(
        "wire_done",
        wire_done_started.elapsed().as_micros() as u64,
        compile_id,
    );
    crate::daemon::compile_trace::record(
        "total_dispatch",
        total.elapsed().as_micros() as u64,
        compile_id,
    );
    // Co-record per-compile output bytes for cross-axis analysis.
    crate::daemon::compile_trace::record("stdout_bytes", stdout_len as u64, compile_id);
    crate::daemon::compile_trace::record("stderr_bytes", stderr_len as u64, compile_id);
    res
}

/// Monotonic per-daemon compile counter. The id is stable within one
/// daemon process and meaningless across restarts — exactly the scope
/// the `SOLDR_DAEMON_TRACE` JSONL is designed for.
pub(crate) fn next_compile_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering as AOrdering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, AOrdering::Relaxed);
    format!("c{n:08x}")
}

/// Best-effort resolver for the `~/.soldr/cache/cook/<sha256>.tar.zst`
/// path. Returns the canonical content-addressed file even when the
/// file does not yet exist on disk — the path is informational only;
/// PR 3 (`Response::CookHit` consumer) is responsible for verifying
/// the sha256 of the bytes it reads.
fn cook_artifact_path(paths: &SoldrPaths, sha256: &[u8; 32]) -> PathBuf {
    paths
        .cache
        .join("cook")
        .join(format!("{}.tar.zst", hex_lower(sha256)))
}

fn hex_lower(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}

/// Is the daemon idle enough to exit? Compiles and maintenance are activity
/// (soldr#3377): SESSION compiles arriving through the shims never touch the
/// control-request clock, so without this a build running past the idle
/// timeout lost its daemon mid-flight and every in-flight compile saw
/// "SESSION relay closed before Exit".
fn idle_exit_due(idle_for: Duration, idle_timeout: Duration, inflight_compiles: usize, maintenance_active: bool) -> bool {
    inflight_compiles == 0 && !maintenance_active && idle_for >= idle_timeout
}

async fn run_idle_watchdog(state: Arc<State>, idle_timeout: Duration) {
    loop {
        tokio::time::sleep(IDLE_POLL_INTERVAL).await;
        let inflight = crate::daemon::inflight_compiles::count();
        if inflight > 0 || state.maintenance_activity.is_active() {
            // Restart the idle clock: the timeout counts from the last
            // compile or maintenance, not from the last control request.
            state.touch_activity();
            continue;
        }
        if idle_exit_due(state.idle_for(), idle_timeout, inflight, state.maintenance_activity.is_active()) {
            // Tag the exit reason BEFORE notifying so the main task's
            // post-shutdown lifecycle JSONL emit picks `died-idle`.
            state.exit_via_idle.store(true, Ordering::Relaxed);
            state.shutdown.request();
            return;
        }
    }
}

/// Exit as soon as the process that asked for this daemon is gone.
///
/// Separate from the idle watchdog on purpose: an owned daemon may be busy
/// right up to the moment its owner dies, so idleness is the wrong signal.
/// It shares `IDLE_POLL_INTERVAL` because both are coarse liveness polls and
/// a second cadence would only add a knob.
async fn run_owner_watchdog(state: Arc<State>, owner_pid: u32) {
    loop {
        tokio::time::sleep(IDLE_POLL_INTERVAL).await;
        if state.shutdown.is_requested() {
            return;
        }
        if super::server::owner_has_exited(Some(owner_pid)) {
            // Tagged as an idle exit so the lifecycle JSONL records a chosen
            // shutdown rather than a crash. The two are the same class of
            // event: the daemon decided it had no further reason to run.
            state.exit_via_idle.store(true, Ordering::Relaxed);
            state.shutdown.request();
            return;
        }
    }
}

/// Used by the `soldr daemon` CLI to derive sockets and paths in one
/// place. Mirrors [`crate::daemon::client::default_sock_path`].
pub fn server_sock_path(paths: &SoldrPaths) -> PathBuf {
    crate::daemon::client::default_sock_path(paths)
}

#[cfg(test)]
mod idle_watchdog_tests {
    use super::*;

    #[test]
    fn a_daemon_with_compiles_in_flight_is_never_idle() {
        let timeout = Duration::from_secs(1800);
        assert!(!idle_exit_due(timeout * 10, timeout, 1, false));
        assert!(!idle_exit_due(timeout * 10, timeout, 16, false));
    }

    #[test]
    fn maintenance_keeps_native_idle_watchdog_alive() {
        let timeout = Duration::from_secs(1800);
        assert!(!idle_exit_due(timeout * 10, timeout, 0, true));
        assert!(idle_exit_due(timeout * 10, timeout, 0, false));
    }

    #[test]
    fn an_idle_daemon_exits_only_after_the_timeout() {
        let timeout = Duration::from_secs(1800);
        assert!(!idle_exit_due(timeout - Duration::from_secs(1), timeout, 0, false));
        assert!(idle_exit_due(timeout, timeout, 0, false));
    }
}
