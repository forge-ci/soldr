//! Synchronous, blocking client used by the wrapper hot path and by the
//! `soldr daemon status|stop` CLI surface. All daemon calls are best
//! effort: every error variant is mapped to a `ClientError` so the
//! caller can decide whether to report a daemon-control-plane failure.

use crate::cache_lib::target_registry::current_unix_seconds;
use crate::core::SoldrPaths;
use crate::daemon::ipc::{
    read_frame_async, read_frame_async_for_version, read_frame_sync, read_frame_sync_for_version,
    write_frame_async, write_frame_async_for_version, write_frame_sync,
    write_frame_sync_for_version,
};
use crate::daemon::protocol::{
    BuildRecord, CacheFlushInfo, CompileStatsInfo, Request, Response, ShutdownAck, StatusInfo,
};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub trait ControlStream: Read + Write + Send {}

impl<T: Read + Write + Send> ControlStream for T {}

pub type BoxedControlStream = Box<dyn ControlStream>;

/// Process-local transport override installed by the soldr CLI. The daemon
/// binary never installs it, so its internal lifecycle probes can still use
/// the private control endpoint while every user-facing soldr client enters
/// through the stable broker listener.
pub trait ControlConnector: Send + Sync {
    fn connect(
        &self,
        endpoint_marker: &Path,
        timeout: Duration,
    ) -> std::io::Result<BoxedControlStream>;
}

static CONTROL_CONNECTOR: OnceLock<Arc<dyn ControlConnector>> = OnceLock::new();

/// Debug-only seam for integration tests that exercise the daemon server in
/// isolation. Production clients cannot opt out of the stable broker route.
pub const TEST_DIRECT_CONTROL_ENV: &str = "SOLDR_TEST_DIRECT_DAEMON_CONTROL";

pub fn install_control_connector(connector: Arc<dyn ControlConnector>) -> Result<(), &'static str> {
    CONTROL_CONNECTOR
        .set(connector)
        .map_err(|_| "daemon control connector is already installed")
}

fn connect_through_override(
    endpoint_marker: &Path,
    timeout: Duration,
) -> Result<Option<BoxedControlStream>, ClientError> {
    #[cfg(debug_assertions)]
    if std::env::var_os(TEST_DIRECT_CONTROL_ENV).is_some() {
        return Ok(None);
    }
    CONTROL_CONNECTOR
        .get()
        .map(|connector| {
            connector
                .connect(endpoint_marker, timeout)
                .map_err(ClientError::from)
        })
        .transpose()
}

/// 50 ms write timeout matches the plan: short enough that the wrapper
/// never blocks the hot path on a hung daemon.
const HOT_PATH_TIMEOUT: Duration = Duration::from_millis(50);

/// Slightly more generous timeout for request/response calls that need
/// to read a body back (status, shutdown). Still small enough that the
/// CLI returns quickly even if the daemon is unresponsive.
const REPLY_TIMEOUT: Duration = Duration::from_millis(2_000);
/// The embedded flush has seven individually bounded phases (pending writes,
/// index writer, and up to five persistence saves), so its IPC budget must be
/// longer than the generic status/shutdown request timeout.
const CACHE_FLUSH_REPLY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Historical protocols that must remain able to retire their daemon during
/// an in-place upgrade. v24 is the immediately preceding daemon protocol;
/// v17 is the last protocol whose shutdown acknowledgement lacked identity.
const SHUTDOWN_COMPAT_PROTOCOL_VERSIONS: &[u32] = &[24, 17];

/// Default compile-dispatch timeout — rustc may take minutes for a release
/// build of a large crate, so the default stays generous (30 minutes): a
/// stuck rustc cannot wedge the wrapper forever, but any legitimate compile
/// inside that bound runs to completion. Issue #977 Phase 5 / #980 L1.
pub const DEFAULT_REPLY_TIMEOUT_SECS: u64 = 30 * 60;

/// Env override for [`compile_reply_timeout`] (issue #1364). Lets an
/// operator fail fast on a wedged cache without waiting out the 30-minute
/// backstop, e.g. `SOLDR_COMPILE_REPLY_TIMEOUT_SECS=30`. `0`, empty, or an
/// unparseable value falls back to [`DEFAULT_REPLY_TIMEOUT_SECS`].
pub const REPLY_TIMEOUT_ENV: &str = "SOLDR_COMPILE_REPLY_TIMEOUT_SECS";

// The Windows named-pipe open policy (`ERROR_PIPE_BUSY` classification,
// `PIPE_BUSY_RETRY_LIMIT`, exponential busy backoff with jitter, and the
// time-bounded worker executor) lives in the platform ipc connect facade
// (crate::platform::ipc::connect), which owns the concrete open/connect
// implementations for every host. The daemon keeps the retry orchestration
// and surfaces `busy_retries` as `ipc_busy_retries` telemetry.

/// Compile-dispatch timeout, resolved once from the environment.
pub fn compile_reply_timeout() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| parse_reply_timeout(std::env::var(REPLY_TIMEOUT_ENV).ok().as_deref()))
}

/// Pure policy for [`compile_reply_timeout`] so the parse/fallback matrix
/// is unit-testable without mutating the process env.
fn parse_reply_timeout(value: Option<&str>) -> Duration {
    let secs = value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_REPLY_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

#[derive(Debug)]
pub enum ClientError {
    /// No daemon endpoint exists at the expected path (most common case
    /// on a fresh checkout — daemon-owned history is unavailable).
    NotRunning,
    /// Endpoint exists but the connect / read / write failed.
    Io(std::io::Error),
    /// Daemon answered something we didn't ask for (or an Error variant).
    Protocol(String),
    /// The daemon answered, but speaks a protocol this binary cannot parse
    /// (#1853).
    ///
    /// Distinct from [`Self::Protocol`] because retrying cannot help: a
    /// version-skewed daemon can never serve this client, no matter how many
    /// attempts remain. The caller must displace the daemon or fall back to a
    /// direct compile rather than burning its retry budget.
    VersionMismatch(String),
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
            && e.raw_os_error() == Some(crate::platform::ipc::connect::ERROR_PIPE_BUSY)
        {
            // The retry cap only bounds a single client call; it does not
            // change daemon liveness. Keep this out of the caller's generic
            // I/O-recovery branch, which is allowed to spawn a missing daemon.
            return ClientError::Protocol(
                "Windows named pipe remained busy after retry budget".into(),
            );
        }
        // #1853: both the version-mismatch branch and the pre-handshake
        // reject record surface as InvalidData from the frame reader. Classify
        // them here, in the shared conversion, so Unix and Windows transports
        // both get it from one place.
        if e.kind() == std::io::ErrorKind::InvalidData {
            let message = e.to_string();
            if message.contains("protocol version mismatch")
                || message.contains("daemon rejected connection")
            {
                return ClientError::VersionMismatch(message);
            }
        }
        match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                ClientError::NotRunning
            }
            _ => ClientError::Io(e),
        }
    }
}

/// Submit `req` to the daemon without surfacing any processing outcome.
/// Used for `RecordTargetTouch`. The wrapper hot path calls this and
/// ignores the result on the failure side.
///
/// soldr#2558: "fire-and-forget" no longer means write-then-close. On
/// macOS/BSD a connection closed before the server accepts it is
/// discarded together with its buffered frame, so the old shape silently
/// lost every touch that raced the daemon's accept loop. The daemon now
/// acks RECEIPT before processing, and this client waits (bounded) for
/// that ack before closing; the wait is best-effort — an old daemon that
/// never acks just costs the bounded read and the touch is delivered on
/// the platforms where it always was.
pub fn submit_fire_and_forget(sock_path: &Path, req: &Request) -> Result<(), ClientError> {
    if let ReceiptAck::Unconfirmed(reason) = submit_awaiting_receipt(sock_path, req)? {
        note_missing_ack(req, &reason);
    }
    Ok(())
}

/// Write `req` on an already-connected stream and consume its receipt ack.
///
/// The ack is bounded, never required: a daemon that predates soldr#2558 does
/// not send one, and failing here would turn every such daemon into a hard
/// error on the wrapper's per-invocation hot path. soldr#2785 only made the
/// missing ack visible; it must not promote it to a failure.
///
/// Takes the stream rather than a socket path (soldr#2955) so that contract is
/// provable against a peer the test constructs itself. It was previously
/// covered by installing a silent peer in `CONTROL_CONNECTOR`, a `OnceLock`
/// whose first writer wins for the whole process — in the consolidated
/// `daemon` test binary that stub outlived its own test and replaced the live
/// daemon connection of every sibling that ran after it.
fn write_awaiting_receipt_ack<S: Read + Write>(
    stream: &mut S,
    req: &Request,
) -> Result<ReceiptAck, ClientError> {
    write_frame_sync(stream, req)?;
    Ok(match read_frame_sync::<_, Response>(stream) {
        Ok(_) => ReceiptAck::Acknowledged,
        Err(error) => ReceiptAck::Unconfirmed(format!("{error}")),
    })
}

/// Submit `req`, wait for one `Response`, return it.
pub fn submit_request(sock_path: &Path, req: &Request) -> Result<Response, ClientError> {
    submit_request_with_timeout(sock_path, req, REPLY_TIMEOUT)
}

fn submit_request_for_version(
    sock_path: &Path,
    req: &Request,
    protocol_version: u32,
) -> Result<Response, ClientError> {
    if let Some(mut stream) = connect_through_override(sock_path, REPLY_TIMEOUT)? {
        write_frame_sync_for_version(&mut stream, req, protocol_version)?;
        return read_frame_sync_for_version(&mut stream, protocol_version)
            .map_err(ClientError::from);
    }
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        submit_request_windows_with_timeout_and_version(
            sock_path,
            req,
            REPLY_TIMEOUT,
            protocol_version,
        )
    } else {
        let mut stream = connect(sock_path, REPLY_TIMEOUT)?;
        write_frame_sync_for_version(&mut stream, req, protocol_version)?;
        let resp: Response = read_frame_sync_for_version(&mut stream, protocol_version)?;
        Ok(resp)
    }
}

/// Compatibility-only direct request. A stable broker cannot route a daemon
/// whose protocol predates its route claim, so retirement probes must dial the
/// root-local endpoint rather than re-enter the installed broker connector.
fn submit_direct_request_for_version(
    sock_path: &Path,
    req: &Request,
    protocol_version: u32,
) -> Result<Response, ClientError> {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        submit_request_windows_with_timeout_and_version(
            sock_path,
            req,
            REPLY_TIMEOUT,
            protocol_version,
        )
    } else {
        let mut stream = connect(sock_path, REPLY_TIMEOUT)?;
        write_frame_sync_for_version(&mut stream, req, protocol_version)?;
        let response: Response = read_frame_sync_for_version(&mut stream, protocol_version)?;
        Ok(response)
    }
}

pub fn status(sock_path: &Path) -> Result<StatusInfo, ClientError> {
    match submit_request(sock_path, &Request::Status)? {
        Response::Status(info) => Ok(info),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// soldr#1368: read the embedded zccache service's cumulative compile
/// counters. `soldr session start` captures a baseline and `soldr
/// session end` diffs against it to report per-session hit/miss stats —
/// replacing the removed managed `zccache session-end` subprocess.
pub fn compile_stats(sock_path: &Path) -> Result<CompileStatsInfo, ClientError> {
    match submit_request(sock_path, &Request::CompileStats)? {
        Response::CompileStats(info) => Ok(info),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// A connection-scoped reservation of the daemon's embedded compile capacity.
///
/// The stream is intentionally retained for the lease lifetime. Calling
/// [`Self::finish`] sends the release frame and waits for its acknowledgement;
/// dropping the lease closes the stream, which makes the daemon drop the same
/// server-side permit guard without polling or a separate cleanup request.
#[must_use = "dropping the lease immediately releases its resident compile capacity"]
pub struct ResidentCapacityLease {
    stream: Option<BoxedControlStream>,
    permits: u32,
}

impl std::fmt::Debug for ResidentCapacityLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResidentCapacityLease")
            .field("permits", &self.permits)
            .field("active", &self.stream.is_some())
            .finish()
    }
}

impl ResidentCapacityLease {
    pub fn permits(&self) -> u32 {
        self.permits
    }

    /// Release the reservation explicitly and wait until the daemon confirms
    /// that its server-side permit guard has been dropped.
    pub fn finish(mut self) -> Result<(), ClientError> {
        let mut stream = self
            .stream
            .take()
            .expect("an owned resident-capacity lease always has a stream");
        write_frame_sync(&mut stream, &Request::ReleaseResidentCapacity)?;
        match read_frame_sync(&mut stream)? {
            Response::Ack => Ok(()),
            Response::Error(message) => Err(ClientError::Protocol(message)),
            other => Err(ClientError::Protocol(format!(
                "unexpected resident-capacity release response: {other:?}"
            ))),
        }
    }
}

/// Acquire `permits` from the daemon's embedded compile-capacity semaphore.
/// The call returns only after the daemon holds every requested permit.
pub fn acquire_resident_capacity(
    sock_path: &Path,
    permits: u32,
) -> Result<ResidentCapacityLease, ClientError> {
    if permits == 0 {
        return Err(ClientError::Protocol(
            "resident capacity requires at least one permit".to_string(),
        ));
    }
    let timeout = compile_reply_timeout();
    let stream = if let Some(stream) = connect_through_override(sock_path, timeout)? {
        stream
    } else {
        Box::new(connect(sock_path, timeout)?) as BoxedControlStream
    };
    acquire_resident_capacity_on_stream(stream, permits)
}

fn acquire_resident_capacity_on_stream(
    mut stream: BoxedControlStream,
    permits: u32,
) -> Result<ResidentCapacityLease, ClientError> {
    if permits == 0 {
        return Err(ClientError::Protocol(
            "resident capacity requires at least one permit".to_string(),
        ));
    }
    write_frame_sync(&mut stream, &Request::AcquireResidentCapacity { permits })?;
    match read_frame_sync(&mut stream)? {
        Response::ResidentCapacityAcquired { permits: acquired } if acquired == permits => {
            Ok(ResidentCapacityLease {
                stream: Some(stream),
                permits,
            })
        }
        Response::ResidentCapacityAcquired { permits: acquired } => Err(ClientError::Protocol(
            format!("daemon acquired {acquired} resident permits; requested {permits}"),
        )),
        Response::Error(message) => Err(ClientError::Protocol(message)),
        other => Err(ClientError::Protocol(format!(
            "unexpected resident-capacity acquire response: {other:?}"
        ))),
    }
}

pub fn shutdown(sock_path: &Path) -> Result<ShutdownAck, ClientError> {
    let current_error = match submit_request(sock_path, &Request::Shutdown) {
        Ok(Response::ShuttingDown(ack)) => return Ok(ack),
        Ok(Response::Error(msg)) => return Err(ClientError::Protocol(msg)),
        Ok(other) => {
            return Err(ClientError::Protocol(format!(
                "unexpected response: {other:?}"
            )))
        }
        Err(error) => error,
    };

    for &protocol_version in SHUTDOWN_COMPAT_PROTOCOL_VERSIONS {
        // v18 rollout bridge: v17's empty shutdown Ack cannot identify its
        // responder. Read status immediately before requesting shutdown, then
        // use that identity only for waiting. Callers never signal it.
        let legacy_status = match submit_direct_request_for_version(
            sock_path,
            &Request::Status,
            protocol_version,
        ) {
            Ok(Response::Status(status)) => Some(status),
            _ => None,
        };
        match submit_direct_request_for_version(sock_path, &Request::Shutdown, protocol_version) {
            Ok(Response::ShuttingDown(ack)) if ack.pid != 0 => return Ok(ack),
            Ok(Response::ShuttingDown(_)) => {
                return legacy_status
                    .map(|status| ShutdownAck {
                        pid: status.pid,
                        generation: status.generation,
                    })
                    .ok_or_else(|| {
                        ClientError::Protocol(
                            "legacy daemon acknowledged shutdown without a responder identity"
                                .into(),
                        )
                    });
            }
            _ => {}
        }
    }
    Err(current_error)
}

/// Issue #1286 (F1): ask the daemon to checkpoint the embedded zccache
/// state (artifact index, depgraph snapshot, metadata cache) to disk
/// without shutting down. Used by `soldr save` / `soldr cache flush`
/// before archiving the cache tree.
pub fn flush_caches(sock_path: &Path) -> Result<CacheFlushInfo, ClientError> {
    // soldr#1838: the embedded flush has seven individually bounded phases
    // and can legitimately take minutes on a large cache. Report progress
    // rather than going silent for the whole 5-minute budget. There is no
    // env override for this one, hence `None`.
    let _heartbeat = super::wait_heartbeat::WaitHeartbeat::start(
        "daemon cache flush",
        CACHE_FLUSH_REPLY_TIMEOUT,
        None,
    );
    match submit_request_with_timeout(sock_path, &Request::FlushCaches, CACHE_FLUSH_REPLY_TIMEOUT)?
    {
        Response::CacheFlushed(info) => Ok(info),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

pub fn list_builds(
    sock_path: &Path,
    limit: u32,
    since_ms: Option<i64>,
) -> Result<Vec<BuildRecord>, ClientError> {
    match submit_request(sock_path, &Request::ListBuilds { limit, since_ms })? {
        Response::Builds(rows) => Ok(rows),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

pub fn list_target_registry(
    sock_path: &Path,
) -> Result<Vec<crate::daemon::protocol::TargetRegistryRow>, ClientError> {
    match submit_request(sock_path, &Request::ListTargetRegistry)? {
        Response::TargetRegistryRows(rows) => Ok(rows),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

pub fn remove_target_registry(sock_path: &Path, paths: Vec<String>) -> Result<u32, ClientError> {
    match submit_request(sock_path, &Request::RemoveTargetRegistry { paths })? {
        Response::TargetRegistryRemoved { removed } => Ok(removed),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Fetch the build-log inputs the daemon owns for `session_id`
/// (soldr#1814 slice 2a).
///
/// Returns the session's event rows plus its build record, if any. Callers
/// treat an unavailable daemon as incomplete history; they must not open the
/// daemon-owned state database themselves.
#[allow(clippy::type_complexity)]
pub fn build_log_inputs(
    sock_path: &Path,
    session_id: u64,
) -> Result<(Vec<crate::daemon::db::Event>, Option<Box<BuildRecord>>), ClientError> {
    match submit_request(sock_path, &Request::BuildLogInputs { session_id })? {
        Response::BuildLogInputs { events, record } => Ok((events, record)),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Ask the daemon whether to emit the cargo-debug-default warning for
/// `repo_root` (soldr#1814 slice 2c).
///
/// The daemon owns the throttle state, so asking it also records the repo.
/// When the daemon is unreachable the front door warns without a record and
/// asks again after the build has started the daemon (soldr#3740).
pub fn should_warn_cargo_debug_default(
    sock_path: &Path,
    repo_root: &Path,
) -> Result<bool, ClientError> {
    let req = Request::ShouldWarnCargoDebugDefault {
        repo_root: repo_root.display().to_string(),
    };
    match submit_request(sock_path, &req)? {
        Response::CargoDebugWarning { emit } => Ok(emit),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Ask the daemon to merge this build's log-history results into its record
/// (soldr#1814 slice 2d).
///
/// The daemon performs the whole read-modify-write under its own ownership of
/// the table, so callers must not also open the DB on success.
pub fn attach_build_log_history(
    sock_path: &Path,
    update: crate::daemon::protocol::BuildLogHistoryUpdate,
) -> Result<(), ClientError> {
    match submit_request(sock_path, &Request::AttachBuildLogHistory(Box::new(update)))? {
        Response::Ack => Ok(()),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

pub fn list_slow_builds(
    sock_path: &Path,
    threshold_ms: u64,
    limit: u32,
) -> Result<Vec<BuildRecord>, ClientError> {
    match submit_request(
        sock_path,
        &Request::ListSlowBuilds {
            threshold_ms,
            limit,
        },
    )? {
        Response::Builds(rows) => Ok(rows),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Acknowledged build-session lifecycle helper. The daemon only returns Ok
/// after the start record and SessionStart event are persisted.
pub fn build_session_start(
    paths: &SoldrPaths,
    session_id: u64,
    repo_root: &Path,
    started_at_ms: i64,
) -> Result<DaemonCompileLimit, ClientError> {
    let sock = default_sock_path(paths);
    match submit_request(
        &sock,
        &Request::BuildSessionStart {
            session_id,
            repo_root: repo_root.display().to_string(),
            started_at_ms,
        },
    )? {
        Response::BuildSessionStarted {
            compile_jobs,
            compile_jobs_source,
        } => Ok(DaemonCompileLimit {
            jobs: compile_jobs as usize,
            source: compile_jobs_source,
        }),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected build_session_start response: {other:?}"
        ))),
    }
}

/// The compile limit a running daemon reported (soldr#2023).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonCompileLimit {
    pub jobs: usize,
    /// Human-readable precedence tier, from `JobsSource::describe`.
    pub source: String,
}

/// Acknowledged finalization (soldr#1536): blocks until the daemon has
/// rolled the session up from its in-memory aggregate, persisted the
/// finalized BuildRecord, and flushed every staged session event to
/// redb. `Ok(())` therefore means the wrapper can trust the persisted
/// aggregate instead of re-scanning the event table; any error routes
/// the caller to the direct-redb fallback exactly like before.
pub fn build_session_end(
    paths: &SoldrPaths,
    session_id: u64,
    exit_code: i32,
    ended_at_ms: i64,
) -> Result<(), ClientError> {
    let sock = default_sock_path(paths);
    match submit_request(
        &sock,
        &Request::BuildSessionEnd {
            session_id,
            exit_code,
            ended_at_ms,
        },
    )? {
        Response::Ack => Ok(()),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected build_session_end response: {other:?}"
        ))),
    }
}

// Cook artifact lookup/record/touch is one cohesive public client surface.
// Keep it split from compile streaming just like `client_transport.rs` while
// retaining the existing `daemon::client::*` paths through lexical inclusion.
include!("client_cook.rs");

// Control-wire transports: Unix socket and Windows named pipe.
include!("client_transport.rs");
#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
