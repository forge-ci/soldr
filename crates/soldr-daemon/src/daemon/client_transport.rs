/// Submit one daemon request with an explicit reply timeout.
pub fn submit_request_with_timeout(
    sock_path: &Path,
    req: &Request,
    timeout: Duration,
) -> Result<Response, ClientError> {
    let context = |stage, error| request_transport_error(sock_path, req, timeout, stage, error);
    if let Some(mut stream) = connect_through_override(sock_path, timeout)
        .map_err(|error| context("connect", error))?
    {
        write_frame_sync(&mut stream, req)
            .map_err(|error| context("write request", ClientError::from(error)))?;
        return read_frame_sync(&mut stream)
            .map_err(|error| context("read reply", ClientError::from(error)));
    }
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        submit_request_windows_with_timeout(sock_path, req, timeout)
            .map_err(|error| context("named-pipe request/reply", error))
    } else {
        let mut stream = connect(sock_path, timeout)
            .map_err(|error| context("connect", error))?;
        write_frame_sync(&mut stream, req)
            .map_err(|error| context("write request", ClientError::from(error)))?;
        read_frame_sync(&mut stream)
            .map_err(|error| context("read reply", ClientError::from(error)))
    }
}

/// Add operation context after platform-specific error classification, so a
/// missing daemon, busy Windows pipe, or protocol mismatch retains its meaning.
fn request_transport_error(
    endpoint: &Path,
    request: &Request,
    timeout: Duration,
    stage: &str,
    error: ClientError,
) -> ClientError {
    let ClientError::Io(source) = error else {
        return error;
    };
    let operation = match request {
        Request::CookLookup { .. } => "cook lookup",
        Request::CookRecord { .. } => "cook record",
        Request::RemoveTargetRegistry { .. } => "target registry removal",
        _ => "daemon request",
    };
    ClientError::Io(std::io::Error::new(
        source.kind(),
        format!(
            "{operation}: {stage} at {} (timeout {} ms): {source}",
            endpoint.display(),
            timeout.as_millis(),
        ),
    ))
}

/// Whether the daemon acknowledged receipt of a fire-and-forget request
/// (soldr#2558).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptAck {
    Acknowledged,
    /// No ack within the bounded wait; carries the transport's reason.
    Unconfirmed(String),
}

/// [`submit_fire_and_forget`] without the stderr note: submit `req` and report
/// whether its receipt ack arrived. Split out so the receipt contract can be
/// asserted from a test process, where the note's stderr line cannot
/// (soldr#3169).
pub fn submit_awaiting_receipt(sock_path: &Path, req: &Request) -> Result<ReceiptAck, ClientError> {
    if let Some(mut stream) = connect_through_override(sock_path, HOT_PATH_TIMEOUT)? {
        return write_awaiting_receipt_ack(&mut stream, req);
    }
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        submit_fire_and_forget_windows(sock_path, req)
    } else {
        // `connect` floors the read timeout at 200ms, which is the ack
        // wait's bound: sub-ms on a healthy daemon (the ack precedes the
        // store write), 200ms worst case against a wedged or pre-ack
        // daemon.
        let mut stream = connect(sock_path, HOT_PATH_TIMEOUT)?;
        write_awaiting_receipt_ack(&mut stream, req)
    }
}

/// Note a hot-path submission whose receipt ack never arrived.
///
/// soldr#2785 asks which of two things happened when a target-registry row is
/// missing: the touch was never delivered, or it was delivered and the write
/// half dropped it. The daemon already answers the second -- it prints
/// `target-touch dropped` or `target-touch upsert failed` to its own stderr,
/// and `cli_gc` reads those lines back on failure. The first had no signal at
/// all: the ack exists to prove receipt, and both platforms discarded its
/// outcome, so "never acked" and "acked then lost the row" were identical
/// silence. A failing msvc run showed exactly that -- no drop, no upsert
/// failure, and no way to tell whether the frame ever arrived.
///
/// Still best-effort. A missing ack must not fail the call: an older daemon
/// that never acks is still delivering the touch everywhere it always did,
/// which is why soldr#2558 made the wait bounded rather than required. It
/// only stops being invisible.
fn note_missing_ack(req: &Request, reason: &str) {
    // Named rather than `{req:?}`: the Debug of a touch carries the full
    // target path, which is the one thing a reader already knows and the one
    // thing that makes the line long.
    eprintln!("{}", missing_ack_message(req, reason));
}

/// The line [`note_missing_ack`] prints.
///
/// Split out so it can be asserted on. The emitter cannot be: it runs in the
/// wrapper process, and a test that captured stderr in-process would be
/// testing the harness rather than the message.
pub(crate) fn missing_ack_message(req: &Request, reason: &str) -> String {
    // Named rather than `{req:?}`: the Debug of a touch carries the full
    // target path, which is the one thing a reader already knows and the one
    // thing that makes the line long.
    let request = match req {
        Request::RecordTargetTouch { .. } => "RecordTargetTouch",
        Request::CookTouch { .. } => "CookTouch",
        _ => "other",
    };
    format!(
        concat!(
            "soldr: daemon did not acknowledge receipt of {request} within the ",
            "bounded wait ({reason}); delivery is unconfirmed (soldr#2785)"
        ),
        request = request,
        reason = reason,
    )
}

/// Wrapper-side target touch. State is daemon-owned: an unavailable daemon
/// leaves the touch unrecorded rather than opening `state.sqlite3` in this process.
pub fn record_target_touch_or_fallback(paths: &SoldrPaths, target: &Path) {
    let unix_seconds = match current_unix_seconds() {
        Ok(s) => s,
        Err(_) => return,
    };
    let sock = default_sock_path(paths);
    let req = Request::RecordTargetTouch {
        path: target.display().to_string(),
        unix_seconds,
    };
    if let Err(error) = submit_fire_and_forget(&sock, &req) {
        tracing::warn!(
            event = "target_touch_daemon_unavailable",
            target = %target.display(),
            error = ?error,
            "target-registry touch was skipped because soldr-daemon is unavailable"
        );
    }
}

fn connect(sock_path: &Path, timeout: Duration) -> Result<UnixOrPipe, ClientError> {
    // AF_UNIX socket with the caller's deadline as the write timeout and a
    // read timeout of at least 200ms so a short reply deadline never starves
    // a frame read (see platform::ipc::connect::connect_unix).
    let stream = crate::platform::ipc::connect::connect_unix(sock_path, timeout, timeout)?;
    Ok(UnixOrPipe(stream))
}

pub struct UnixOrPipe(crate::platform::ipc::connect::BoxedSyncStream);

impl std::io::Read for UnixOrPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl std::io::Write for UnixOrPipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

fn windows_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
}

fn run_windows_ipc<T, F>(operation: &'static str, timeout: Duration, f: F) -> Result<T, ClientError>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    crate::platform::ipc::connect::run_in_pipe_worker(operation, timeout, f)
        .map_err(ClientError::from)
}

/// The bound on waiting for the daemon's receipt ack after a hot-path
/// write (soldr#2558). Named pipes do not share the macOS pre-accept
/// drop, but acking uniformly keeps the transports' contracts identical
/// and the wait is sub-ms against a healthy daemon (the ack precedes the
/// store write).
const HOT_PATH_ACK_TIMEOUT: Duration = Duration::from_millis(200);

// Bounded by the enclosing `run_windows_ipc` deadline (soldr#3836).
#[cfg_attr(dylint_lib = "ban_raw_sync_primitive", allow(ban_raw_sync_primitive))]
fn submit_fire_and_forget_windows(
    sock_path: &Path,
    req: &Request,
) -> Result<ReceiptAck, ClientError> {
    use tokio::time::timeout;

    let sock_path = sock_path.to_path_buf();
    let req = req.clone();
    run_windows_ipc(
        "daemon IPC hot-path write",
        HOT_PATH_TIMEOUT + HOT_PATH_ACK_TIMEOUT,
        move || {
            let runtime = windows_runtime()?;
            runtime.block_on(async move {
                let mut stream = crate::platform::ipc::connect::open_pipe_with_retry(&sock_path)
                    .await?
                    .stream;
                timeout(HOT_PATH_TIMEOUT, write_frame_async(&mut stream, &req))
                    .await
                    .map_err(|_| {
                        crate::platform::ipc::connect::pipe_timeout_error(
                            "daemon IPC hot-path write",
                            HOT_PATH_TIMEOUT,
                        )
                    })??;
                // Best-effort receipt ack (soldr#2558); an old daemon that
                // never acks costs only this bounded wait.
                //
                // soldr#2785: still best-effort, but no longer silent. The ack
                // is what proves the frame arrived, so discarding its outcome
                // made "never delivered" indistinguishable from "delivered and
                // the write half lost it" -- the two answers that issue is
                // trying to separate.
                Ok::<ReceiptAck, std::io::Error>(
                    match timeout(
                        HOT_PATH_ACK_TIMEOUT,
                        read_frame_async::<_, Response>(&mut stream),
                    )
                    .await
                    {
                        Ok(Ok(_)) => ReceiptAck::Acknowledged,
                        Ok(Err(error)) => ReceiptAck::Unconfirmed(format!("{error}")),
                        Err(_) => ReceiptAck::Unconfirmed(format!(
                            "no ack within {HOT_PATH_ACK_TIMEOUT:?}"
                        )),
                    },
                )
            })
        },
    )
}

fn submit_request_windows_with_timeout(
    sock_path: &Path,
    req: &Request,
    deadline: Duration,
) -> Result<Response, ClientError> {
    submit_request_windows_with_timeout_and_version(
        sock_path,
        req,
        deadline,
        crate::daemon::protocol::PROTOCOL_VERSION,
    )
}

// Bounded by the enclosing `run_windows_ipc` deadline (soldr#3836).
#[cfg_attr(dylint_lib = "ban_raw_sync_primitive", allow(ban_raw_sync_primitive))]
fn submit_request_windows_with_timeout_and_version(
    sock_path: &Path,
    req: &Request,
    deadline: Duration,
    protocol_version: u32,
) -> Result<Response, ClientError> {
    use tokio::time::timeout;

    let sock_path = sock_path.to_path_buf();
    let req = req.clone();
    run_windows_ipc("daemon IPC request", deadline, move || {
        let runtime = windows_runtime()?;
        runtime.block_on(async move {
            let mut stream = crate::platform::ipc::connect::open_pipe_with_retry(&sock_path)
                .await?
                .stream;
            timeout(deadline, async {
                write_frame_async_for_version(&mut stream, &req, protocol_version).await?;
                read_frame_async_for_version(&mut stream, protocol_version).await
            })
            .await
            .map_err(|_| {
                crate::platform::ipc::connect::pipe_timeout_error("daemon IPC request", deadline)
            })?
        })
    })
}

/// Returns the well-known socket path the wrapper should use. Centralized
/// here so callers don't need to import `cache_lib` directly.
pub fn default_sock_path(paths: &SoldrPaths) -> PathBuf {
    #[cfg(debug_assertions)]
    if std::env::var_os(TEST_DIRECT_CONTROL_ENV).is_some() {
        return crate::daemon::session_endpoint::resolved_control_endpoint_path(paths)
            .unwrap_or_else(|_| PathBuf::from("<missing-daemon-control-endpoint>"));
    }
    if CONTROL_CONNECTOR.get().is_some() {
        // Opaque marker only; the installed connector ignores it. Returning a
        // marker instead of deriving the private path enforces the #2476
        // boundary that user-facing clients neither learn nor dial a daemon
        // endpoint.
        return PathBuf::from("<broker-routed-daemon-control>");
    }
    crate::daemon::session_endpoint::resolved_control_endpoint_path(paths)
        .unwrap_or_else(|_| PathBuf::from("<missing-daemon-control-endpoint>"))
}
