//! SESSION `0x5350` client wiring for Soldr's stable singleton broker.
//!
//! The compiler wrapper dials the one broker endpoint, sends the standard v2
//! Hello, then keeps that accepted connection for the SESSION wire. The broker
//! either hands the connection directly to the daemon or proxies that same
//! connection when handle passing is unavailable.

use std::io;

/// Verbose session-transport tracing (soldr#3609: read through `core::flag`).
pub const SESSION_DEBUG_ENV_VAR: &str = "SOLDR_SESSION_DEBUG";

/// Is [`SESSION_DEBUG_ENV_VAR`] on? The one reader.
pub(crate) fn session_debug_enabled() -> bool {
    crate::core::flag(SESSION_DEBUG_ENV_VAR)
}
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use interprocess::local_socket::tokio::Stream;
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use running_process::broker::protocol::{
    encode_framed, hello_reply::Result as HelloReplyResult, try_decode_framed, FrameKind,
    HandoffAck, Hello, PayloadEncoding, CONTROL_PAYLOAD_PROTOCOL, ENVELOPE_VERSION,
};
use running_process::broker::protocol::{Frame, HelloReply};
use running_process::broker::protocol_v2::{
    session_frame, SessionEnvVar, SessionFrame, SessionStart,
};
use running_process::broker::session_codec::{encode_session_frame, try_decode_session_frame};

static NEXT_BROKER_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn broker_hello_frame(payload: Vec<u8>) -> Frame {
    let request_id = NEXT_BROKER_REQUEST_ID
        .fetch_add(1, Ordering::Relaxed)
        .max(1);
    Frame::request(CONTROL_PAYLOAD_PROTOCOL, payload).with_request_id(request_id)
}

/// Outcome of the SESSION compile hot path ([`session_hot_path`]), consumed by
/// `compile_dispatch`. All bounded retry and error attribution lives here.
pub enum SessionHotPathOutcome {
    /// SESSION served the compile; return this exit code.
    Served(i32),
    /// SESSION infrastructure failed; cacheable compiles have no alternate route.
    HardFail(io::Error),
}

/// Backend identity attested by the broker's successful route negotiation.
#[derive(Debug)]
pub(crate) struct ReadyRoute {
    pub(crate) backend_pipe: String,
    pub(crate) daemon_version: String,
}

/// Broker-unreachable errors return immediately; an existing broker gets the
/// bounded `BrokerDeadlines::route_ceiling` window to launch or reconnect the
/// requested daemon partition. Its default is 120 seconds and it is configured
/// by `SOLDR_ROUTE_ACQUIRE_CEILING_MS`.
/// Mandatory SESSION compile hot path: relay client → broker → daemon.
/// A missing broker fails immediately; an existing broker gets the bounded
/// route-acquisition ceiling to provide the requested route. Every terminal
/// infrastructure error is hard because there is no legacy acquisition path.
pub fn session_hot_path(rustc_argv: &[String]) -> SessionHotPathOutcome {
    let service_name = match crate::daemon::backend_handle_adoption::broker_service_name() {
        Ok(service_name) => service_name,
        Err(err) => {
            return SessionHotPathOutcome::HardFail(io::Error::other(format!(
                "cannot resolve the broker daemon route: {err}; invoke this compiler through a soldr build front door"
            )))
        }
    };
    // soldr#3401: a route already refused earlier in this build will be refused
    // again, deterministically -- don't dial the broker just to print it again.
    if let Ok(paths) = crate::core::SoldrPaths::new() {
        if crate::route_refusal_sentinel::already_refused(
            &paths,
            crate::wrapper_target::read_build_session_id_env(),
        ) {
            return SessionHotPathOutcome::HardFail(io::Error::other(
                crate::route_refusal::RouteRefusal {
                    text: crate::route_refusal_sentinel::REPEAT_LINE.to_string(),
                    fatal: true,
                    repeat: true,
                },
            ));
        }
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => return SessionHotPathOutcome::HardFail(err),
    };
    let cwd = cwd.display().to_string();
    let env: Vec<SessionEnvVar> = std::env::vars()
        .map(|(key, value)| SessionEnvVar { key, value })
        .collect();

    let started = std::time::Instant::now();
    let mut attempt =
        run_session_compile_for_service(&service_name, rustc_argv, cwd.clone(), env.clone());
    if let Err(err) = &attempt {
        if relay_lost_before_output(err) {
            // soldr#3377: the daemon went away before any output reached us, so
            // nothing was printed twice and the compile can be asked for once
            // more through the same broker route. Say so: a silent retry would
            // hide daemon deaths.
            eprintln!(
                "soldr: {}; the daemon closed the compile before any output -- retrying once \
                 (see `soldr logs paths` for the daemon log; soldr#3377)",
                err.source
            );
            std::thread::sleep(Duration::from_millis(250));
            attempt = run_session_compile_for_service(&service_name, rustc_argv, cwd, env);
        }
    }
    match attempt {
        Ok(outcome) => {
            if session_debug_enabled() {
                eprintln!(
                    "soldr: SESSION compile served (cache_outcome={:?})",
                    outcome.cache_outcome
                );
            }
            SessionHotPathOutcome::Served(outcome.exit_code)
        }
        Err(err) if is_connection_failure(&err) => {
            use std::io::IsTerminal;
            SessionHotPathOutcome::HardFail(io::Error::other(connection_failure_message(
                &err,
                &service_name,
                started.elapsed(),
                io::stderr().is_terminal(),
            )))
        }
        Err(err) => SessionHotPathOutcome::HardFail(err.source),
    }
}

/// Optional tighter bound on SESSION setup, in seconds. Setup includes the
/// broker launching the daemon on a cold route, so by default it gets the route
/// ceiling (#3821's 30 s cap failed slow target-run cold starts, soldr#3815).
pub(crate) const CONNECTION_LIVENESS_DEADLINE_ENV: &str = "SOLDR_CONNECT_DEADLINE_SECS";

/// The SESSION setup budget: the route ceiling, tightened only by an explicit
/// positive `SOLDR_CONNECT_DEADLINE_SECS`.
pub(crate) fn session_setup_timeout(route_ceiling: Duration, explicit: Option<&str>) -> Duration {
    explicit
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map_or(route_ceiling, |secs| {
            route_ceiling.min(Duration::from_secs(secs))
        })
}

/// Is this failure about reaching or keeping the daemon connection, as
/// opposed to the compiler's own result?
fn is_connection_failure(err: &SessionError) -> bool {
    use io::ErrorKind::{
        BrokenPipe, ConnectionAborted, ConnectionRefused, ConnectionReset, NotFound, TimedOut,
        UnexpectedEof,
    };
    if err.broker_unreachable {
        return true;
    }
    let reply_timeout = err.source.to_string().contains("reply did not complete");
    !reply_timeout
        && matches!(
            err.source.kind(),
            BrokenPipe
                | ConnectionAborted
                | ConnectionRefused
                | ConnectionReset
                | NotFound
                | TimedOut
                | UnexpectedEof
        )
}

/// The loud, stand-alone failure printed when the daemon connection cannot be
/// established or re-established in time (soldr#3815). Red on a terminal; the
/// text reads the same without colour.
fn connection_failure_message(
    err: &SessionError,
    service_name: &str,
    elapsed: Duration,
    color: bool,
) -> String {
    let what = if err.broker_unreachable || err.source.kind() == io::ErrorKind::TimedOut {
        "could not connect to the soldr daemon"
    } else {
        "lost the connection to the soldr daemon"
    };
    let text = format!(
        "soldr: ERROR: {what} (route {service_name}) after {:.1}s: {}\n\
         soldr: the compile was not completed; this build cannot continue. Recover with:\n\
         soldr:   soldr daemon status\n\
         soldr:   soldr daemon stop && soldr daemon start\n\
         soldr:   soldr logs paths   (daemon and broker logs)",
        elapsed.as_secs_f64(),
        err.source
    );
    if color {
        format!("\x1b[1;31m{text}\x1b[0m")
    } else {
        text
    }
}

/// Did the relay close before the terminal `Exit` frame and before any
/// compiler output reached this process? Only then is a single retry safe.
fn relay_lost_before_output(err: &SessionError) -> bool {
    !err.output_started
        && !err.broker_unreachable
        && err.source.kind() == io::ErrorKind::UnexpectedEof
        && err.source.to_string().contains("relay closed before Exit")
}

/// Ask the stable broker to materialize one route without starting a SESSION.
/// Used by the explicit `soldr daemon start` front door after broker
/// resurrection; dropping immediately after `Negotiated` is intentional.
pub(crate) fn ensure_broker_route(
    service_name: &str,
    timeout: std::time::Duration,
) -> io::Result<ReadyRoute> {
    let service_name = service_name.to_string();
    std::thread::scope(|scope| {
        scope
            .spawn(move || ensure_broker_route_on_runtime(&service_name, timeout))
            .join()
            .map_err(|_| io::Error::other("broker route worker panicked"))?
    })
}

fn ensure_broker_route_on_runtime(service_name: &str, timeout: Duration) -> io::Result<ReadyRoute> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    crate::platform::wait::block_on_for(
        &runtime,
        async {
            tokio::time::timeout(timeout, async {
                let endpoint = session_socket_path()?;
                let name = local_session_name(&endpoint)?;
                let mut stream = connect_broker_with_busy_retry(name).await?;
                let hello = Hello {
                    client_min_protocol: ENVELOPE_VERSION as u32,
                    client_max_protocol: ENVELOPE_VERSION as u32,
                    service_name: service_name.to_string(),
                    wanted_version:
                        crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_VERSION
                            .to_string(),
                    client_version: env!("CARGO_PKG_VERSION").to_string(),
                    request_id: format!("soldr-route-{}", std::process::id()),
                    peer_pid: std::process::id(),
                    client_lib_name: "soldr".into(),
                    client_lib_version: env!("CARGO_PKG_VERSION").to_string(),
                    peer_attestation_nonce: crate::broker_server::client_host_attestation(),
                    // Route-only probes never need to transfer their connection.
                    client_capabilities: 0,
                    ..Default::default()
                };
                let frame = broker_hello_frame(hello.encode_to_vec());
                let request_id = frame.request_id;
                let bytes = encode_framed(&frame).map_err(io::Error::other)?;
                stream.write_all(&bytes).await?;
                stream.flush().await?;
                read_negotiated(&mut stream, request_id).await
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "broker route start timed out"))?
        },
        timeout + crate::platform::wait::PROBE,
    )?
}

/// Resolve the one stable broker endpoint.
pub(crate) fn session_socket_path() -> io::Result<String> {
    crate::broker_identity::ResolvedBrokerEndpoint::resolve()
        .map(|endpoint| endpoint.bind_endpoint)
        .map_err(io::Error::other)
}

pub(crate) fn local_session_name(
    socket_path: &str,
) -> io::Result<interprocess::local_socket::Name<'_>> {
    running_process::broker::server::singleton_bind::wrap_socket_name(socket_path)
        .map_err(io::Error::other)
}

/// A SESSION compile failure, tagged with whether any compiler output was
/// already emitted locally.
///
/// `output_started` preserves diagnostic attribution: a failure after compiler
/// output began may have emitted a partial diagnostic, while a setup failure
/// did not. Both are hard failures on the mandatory SESSION route.
#[derive(Debug)]
pub struct SessionError {
    /// Whether compiler stdout/stderr was written locally before the failure.
    pub output_started: bool,
    /// Whether the broker's SESSION socket was unreachable (dial refused /
    /// absent) — i.e. there is no broker to talk to, as opposed to a broker
    /// that answered but whose daemon relay is not ready yet. This fails
    /// immediately instead of burning the route-start budget.
    pub broker_unreachable: bool,
    /// The underlying transport / protocol error.
    pub source: io::Error,
}

impl SessionError {
    pub(crate) fn pre_output(source: io::Error) -> Self {
        Self {
            output_started: false,
            broker_unreachable: false,
            source,
        }
    }

    /// The broker's SESSION socket could not be dialed — no broker is serving.
    pub(crate) fn broker_unreachable(source: io::Error) -> Self {
        Self {
            output_started: false,
            broker_unreachable: true,
            source,
        }
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SESSION compile failed (output_started={}): {}",
            self.output_started, self.source
        )
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// The result of a SESSION compile: the compiler exit code plus the daemon's
/// `cache_outcome` (`CacheOutcome` discriminant: 1=Hit, 2=Miss, 3=Error) carried
/// on the terminal `Exit` frame's metadata — `None` if the daemon did not report
/// one (e.g. an infra exit).
#[derive(Debug, Clone)]
struct SessionCompileOutcome {
    /// The compiler's exit code.
    pub exit_code: i32,
    /// The daemon's cache outcome discriminant, if reported.
    pub cache_outcome: Option<i32>,
}

fn run_session_compile_for_service(
    service_name: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> Result<SessionCompileOutcome, SessionError> {
    let run = || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(SessionError::pre_output)?;
        // The compile reply is bounded inside; this outer bound only keeps
        // the facade's every-wait-has-a-timeout guarantee.
        crate::platform::wait::block_on_for(
            &runtime,
            run_session_compile_async(service_name, rustc_argv, cwd, env),
            crate::platform::wait::LONG_CHILD,
        )
        .unwrap_or_else(|error| Err(SessionError::pre_output(error.into())))
    };

    // A Cargo wrapper arrives before Soldr constructs its command runtime, but
    // direct compiler commands are dispatched from that runtime. Tokio rejects
    // nested `block_on` calls there. Move only that direct-command bridge to a
    // short-lived ordinary thread, leaving the hot wrapper path unchanged.
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            scope.spawn(run).join().unwrap_or_else(|_| {
                Err(SessionError::pre_output(std::io::Error::other(
                    "SESSION compile worker panicked",
                )))
            })
        })
    } else {
        run()
    }
}

async fn run_session_compile_async(
    service_name: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> Result<SessionCompileOutcome, SessionError> {
    let setup_timeout = session_setup_timeout(
        crate::broker_deadlines::BrokerDeadlines::from_env().route_ceiling,
        std::env::var(CONNECTION_LIVENESS_DEADLINE_ENV)
            .ok()
            .as_deref(),
    );
    let mut stream = match tokio::time::timeout(
        setup_timeout,
        establish_session(service_name, rustc_argv, cwd, env),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            return Err(SessionError::pre_output(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "SESSION setup did not complete within {}ms",
                    setup_timeout.as_millis()
                ),
            )))
        }
    };

    let mut probe = RelayProbe::new(rustc_argv);
    probe.on_request_sent();
    pump_session_output_with_timeout(
        &mut stream,
        crate::daemon::client::compile_reply_timeout(),
        &mut probe,
    )
    .await
}

async fn establish_session(
    service_name: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> Result<Stream, SessionError> {
    // Setup — connect / Hello / negotiate / SessionStart send. Failures here
    // are tagged pre-output for precise diagnostics.
    let session_socket = session_socket_path().map_err(SessionError::broker_unreachable)?;
    if session_debug_enabled() {
        eprintln!("soldr: SESSION dialing service={service_name} socket={session_socket}");
    }
    let name = local_session_name(&session_socket).map_err(SessionError::broker_unreachable)?;
    // Negotiate the requested daemon route over the v2 control envelope.
    let hello_payload = Hello {
        client_min_protocol: ENVELOPE_VERSION as u32,
        client_max_protocol: ENVELOPE_VERSION as u32,
        service_name: service_name.to_string(),
        wanted_version: crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_VERSION
            .to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        request_id: format!("soldr-session-{}", std::process::id()),
        peer_pid: std::process::id(),
        client_lib_name: "soldr".to_string(),
        client_lib_version: env!("CARGO_PKG_VERSION").to_string(),
        peer_attestation_nonce: crate::broker_server::client_host_attestation(),
        client_capabilities: running_process::broker::capabilities::CAP_HANDLE_PASSING,
        ..Default::default()
    }
    .encode_to_vec();
    let hello_frame = broker_hello_frame(hello_payload);
    let request_id = hello_frame.request_id;
    let hello =
        encode_framed(&hello_frame).map_err(|e| SessionError::pre_output(io::Error::other(e)))?;
    // A same-version broker replacement can accept a pipe instance and close
    // it before replying to Hello. Retrying this pre-session handshake is safe:
    // no compiler request or output has crossed the connection yet.
    let mut hello_attempt = 0;
    let mut stream = loop {
        hello_attempt += 1;
        // A failed dial means no broker is bound at this socket.
        let mut candidate = connect_broker_with_busy_retry(name.clone())
            .await
            .map_err(SessionError::broker_unreachable)?;
        candidate
            .write_all(&hello)
            .await
            .map_err(SessionError::pre_output)?;
        candidate.flush().await.map_err(SessionError::pre_output)?;
        match read_negotiated(&mut candidate, request_id).await {
            Ok(_) => break candidate,
            Err(error) if hello_attempt < 3 && broker_hello_retryable(&error) => {
                tokio::time::sleep(busy_jitter()).await;
            }
            Err(error) => return Err(SessionError::pre_output(error)),
        }
    };

    // From here the connection is a transparent SESSION relay to the daemon.
    let start = compile_session_start(rustc_argv, cwd, env);
    let start_frame = encode_session_frame(
        &SessionFrame {
            kind: Some(session_frame::Kind::Start(start)),
        },
        0,
    )
    .map_err(|e| SessionError::pre_output(io::Error::other(e)))?;
    stream
        .write_all(&start_frame)
        .await
        .map_err(SessionError::pre_output)?;
    stream.flush().await.map_err(SessionError::pre_output)?;

    // Output phase — a failure after the first byte is printed is a hard error.
    Ok(stream)
}

fn compile_session_start(
    rustc_argv: &[String],
    cwd: String,
    env: Vec<running_process::broker::protocol_v2::SessionEnvVar>,
) -> SessionStart {
    SessionStart {
        program: rustc_argv.first().cloned().unwrap_or_default(),
        args: rustc_argv.get(1..).unwrap_or_default().to_vec(),
        cwd,
        env,
        clear_inherited_env: true,
        environment_policy: running_process::broker::protocol_v2::EnvironmentPolicy::Clear as i32,
    }
}

async fn connect_broker_with_busy_retry(
    name: interprocess::local_socket::Name<'_>,
) -> io::Result<Stream> {
    let started = tokio::time::Instant::now();
    let deadline = started + crate::broker_deadlines::BrokerDeadlines::from_env().busy_budget;
    loop {
        match crate::platform::ipc::connect::connect_local_socket(name.clone()).await {
            Ok(stream) => return Ok(stream),
            Err(error)
                if broker_connect_is_busy(&error, started.elapsed())
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(busy_jitter()).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn broker_connect_is_busy(error: &io::Error, elapsed: Duration) -> bool {
    // Windows briefly reports FILE_NOT_FOUND while a busy named-pipe listener
    // replaces one accepted instance with the next. One short retry window
    // absorbs that instance gap without turning a genuinely absent endpoint
    // into the full one-second busy wait.
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
        && error.raw_os_error() == Some(2)
    {
        return elapsed < Duration::from_millis(50);
    }
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::TimedOut
            | io::ErrorKind::Interrupted
    ) || matches!(error.raw_os_error(), Some(231) | Some(232) | Some(233))
}

fn broker_hello_retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::UnexpectedEof
    ) || error.to_string() == "broker closed before Hello reply"
}

fn busy_jitter() -> std::time::Duration {
    let mut random = [0_u8; 8];
    let value = if getrandom::fill(&mut random).is_ok() {
        u64::from_le_bytes(random)
    } else {
        0
    };
    std::time::Duration::from_millis(5 + value % 46)
}

/// Read and validate the broker's framed `Negotiated` reply.
async fn read_negotiated<S>(stream: &mut S, expected_request_id: u64) -> io::Result<ReadyRoute>
where
    S: tokio::io::AsyncRead + Unpin,
{
    read_negotiated_with_deadlines(
        stream,
        expected_request_id,
        crate::broker_deadlines::BrokerDeadlines::from_env(),
    )
    .await
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
async fn read_negotiated_with_deadlines<S>(
    stream: &mut S,
    expected_request_id: u64,
    deadlines: crate::broker_deadlines::BrokerDeadlines,
) -> io::Result<ReadyRoute>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use prost::Message as _;
    let started = tokio::time::Instant::now();
    let route_ceiling = started + deadlines.route_ceiling;
    let mut response_deadline = started + deadlines.first_response;
    let mut pending_handoff: Option<(Vec<u8>, u64, ReadyRoute)> = None;
    let mut last_progress_elapsed_ms = 0_u64;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        while let Some(decoded) = try_decode_framed(&buf).map_err(io::Error::other)? {
            let consumed = decoded.consumed;
            let frame = decoded.frame;
            buf.drain(..consumed);
            if frame.envelope_version != ENVELOPE_VERSION as u32
                || PayloadEncoding::try_from(frame.payload_encoding) != Ok(PayloadEncoding::None)
            {
                return Err(io::Error::other(
                    "broker returned an invalid frame envelope",
                ));
            }
            if FrameKind::try_from(frame.kind) == Ok(FrameKind::Event)
                && frame.payload_protocol == crate::broker_server::ROUTE_PROGRESS_PAYLOAD_PROTOCOL
            {
                if frame.request_id != expected_request_id {
                    return Err(io::Error::other("broker progress request id mismatch"));
                }
                let progress =
                    crate::broker_server::RouteProgress::decode(frame.payload.as_slice())
                        .map_err(io::Error::other)?;
                if progress.stage.is_empty()
                    || progress.latest_result.is_empty()
                    || progress.attempt == 0
                    || progress.elapsed_ms < last_progress_elapsed_ms
                {
                    return Err(io::Error::other("broker returned invalid route progress"));
                }
                last_progress_elapsed_ms = progress.elapsed_ms;
                if session_debug_enabled() {
                    eprintln!(
                        "soldr: broker route progress stage={} attempt={} elapsed_ms={} result={}",
                        progress.stage,
                        progress.attempt,
                        progress.elapsed_ms,
                        progress.latest_result
                    );
                }
                response_deadline = tokio::time::Instant::now() + deadlines.progress_silence;
                continue;
            }
            if FrameKind::try_from(frame.kind) == Ok(FrameKind::Event)
                && frame.payload_protocol
                    == running_process::broker::protocol::HANDOFF_PAYLOAD_PROTOCOL
            {
                let Some((expected_token, expected_correlation_id, route)) =
                    pending_handoff.as_ref()
                else {
                    return Err(io::Error::other("unexpected broker handoff-ready event"));
                };
                let ack = HandoffAck::decode(frame.payload.as_slice()).map_err(io::Error::other)?;
                if ack.token != *expected_token || ack.correlation_id != *expected_correlation_id {
                    return Err(io::Error::other(
                        "broker handoff event did not match its negotiation",
                    ));
                }
                return Ok(ReadyRoute {
                    backend_pipe: route.backend_pipe.clone(),
                    daemon_version: route.daemon_version.clone(),
                });
            }
            if FrameKind::try_from(frame.kind) != Ok(FrameKind::Response)
                || frame.payload_protocol != CONTROL_PAYLOAD_PROTOCOL
            {
                return Err(io::Error::other(format!(
                    "unexpected broker frame kind={} protocol={:#x}",
                    frame.kind, frame.payload_protocol
                )));
            }
            if frame.request_id != expected_request_id {
                return Err(io::Error::other("broker Hello reply request id mismatch"));
            }
            let reply = HelloReply::decode(frame.payload.as_slice()).map_err(io::Error::other)?;
            match reply.result {
                Some(HelloReplyResult::Negotiated(negotiated)) => {
                    let route = ReadyRoute {
                        backend_pipe: negotiated.backend_pipe.clone(),
                        daemon_version: negotiated.daemon_version.clone(),
                    };
                    if negotiated.server_capabilities
                        & running_process::broker::capabilities::CAP_HANDLE_PASSING
                        != 0
                        && !negotiated.handle_passed_token.is_empty()
                    {
                        pending_handoff = Some((
                            negotiated.handle_passed_token,
                            negotiated.connection_id,
                            route,
                        ));
                        response_deadline = tokio::time::Instant::now()
                            + running_process::broker::server::DEFAULT_HANDOFF_ACK_DEADLINE
                            + std::time::Duration::from_millis(100);
                        continue;
                    }
                    return Ok(route);
                }
                Some(HelloReplyResult::Refused(refused)) => {
                    let (wrapper, service) = crate::route_refusal::wrapper_and_service();
                    let wanted =
                        crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_VERSION;
                    let text = crate::route_refusal::describe_refusal(
                        refused.code,
                        &refused.reason,
                        refused.retry_after_ms,
                        &refused.details,
                        &crate::route_refusal::RefusalContext {
                            wrapper: &wrapper,
                            wrapper_version: env!("CARGO_PKG_VERSION"),
                            wanted_version: wanted,
                            service: service.as_deref(),
                        },
                    );
                    // soldr#3401: carry the refusal as a value so the dispatcher
                    // can tell a deterministic policy refusal from a transient one.
                    Err(io::Error::other(crate::route_refusal::RouteRefusal {
                        text,
                        fatal: crate::route_refusal::code_is_fatal(refused.code),
                        repeat: false,
                    }))
                }
                None => Err(io::Error::other("broker returned an empty HelloReply")),
            }?;
        }
        let deadline = if pending_handoff.is_some() {
            response_deadline
        } else {
            response_deadline.min(route_ceiling)
        };
        let n = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| {
                if pending_handoff.is_some() {
                    return io::Error::new(
                        io::ErrorKind::TimedOut,
                        "broker did not confirm either handoff ownership or same-connection proxy fallback",
                    );
                }
                let class = if deadline == route_ceiling {
                    "route acquisition ceiling"
                } else if response_deadline == started + deadlines.first_response {
                    "first-response deadline"
                } else {
                    "progress-silence deadline"
                };
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "broker {class} exceeded after {}ms (limit {}ms; raise {} if the broker is only busy)",
                        started.elapsed().as_millis(),
                        (deadline - started).as_millis(),
                        crate::broker_deadlines::deadline_env_var(class),
                    ),
                )
            });
        let n = n??;
        if n == 0 {
            return Err(io::Error::other("broker closed before Hello reply"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Metadata key on the terminal `Exit` frame carrying the daemon's cache
/// outcome — matches `soldr_daemon::daemon::session_sink::META_CACHE_OUTCOME`.
const META_CACHE_OUTCOME: &str = "cache_outcome";

fn mark_relayed_output(bytes: &[u8]) -> bool {
    let marked = !bytes.is_empty();
    if marked {
        crate::exit_guard::mark_spoke();
    }
    marked
}

async fn pump_session_output_with_timeout<S>(
    stream: &mut S,
    reply_timeout: std::time::Duration,
    probe: &mut RelayProbe,
) -> Result<SessionCompileOutcome, SessionError>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let output_started = Arc::new(AtomicBool::new(false));
    match tokio::time::timeout(
        reply_timeout,
        pump_session_output(stream, Arc::clone(&output_started), probe),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(SessionError {
            output_started: output_started.load(Ordering::Acquire),
            broker_unreachable: false,
            source: io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "SESSION compile reply did not complete within {}s; adjust {} only while diagnosing",
                    reply_timeout.as_secs_f64(),
                    crate::daemon::client::REPLY_TIMEOUT_ENV
                ),
            ),
        }),
    }
}

/// Pump SESSION frames from the relay: stdout/stderr to local stdio, returning
/// the compiler exit code + `cache_outcome` on the terminal `Exit` frame.
///
/// `output_started` flips to `true` the moment any stdout/stderr byte is written
/// locally; every error is tagged with it so the caller knows whether a legacy
/// fallback would double-print.
async fn pump_session_output<S>(
    stream: &mut S,
    output_seen: Arc<AtomicBool>,
    probe: &mut RelayProbe,
) -> Result<SessionCompileOutcome, SessionError>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let mut output_started = false;
    let tag = |output_started: bool, e: io::Error| SessionError {
        output_started,
        broker_unreachable: false,
        source: e,
    };
    loop {
        loop {
            match try_decode_session_frame(&buf) {
                Ok(Some(decoded)) => {
                    let consumed = decoded.consumed;
                    let kind = decoded.frame.kind.clone();
                    buf.drain(..consumed);
                    probe.on_frame();
                    match kind {
                        Some(session_frame::Kind::Stdout(b)) => {
                            output_started = true;
                            output_seen.store(true, Ordering::Release);
                            stdout
                                .write_all(&b)
                                .await
                                .map_err(|e| tag(output_started, e))?;
                            mark_relayed_output(&b);
                            probe.on_output(&b);
                            stdout.flush().await.map_err(|e| tag(output_started, e))?;
                        }
                        Some(session_frame::Kind::Stderr(b)) => {
                            output_started = true;
                            output_seen.store(true, Ordering::Release);
                            stderr
                                .write_all(&b)
                                .await
                                .map_err(|e| tag(output_started, e))?;
                            mark_relayed_output(&b);
                            probe.on_output(&b);
                            stderr.flush().await.map_err(|e| tag(output_started, e))?;
                        }
                        Some(session_frame::Kind::Exit(exit)) => {
                            probe.on_exit();
                            let cache_outcome = exit
                                .metadata
                                .get(META_CACHE_OUTCOME)
                                .and_then(|v| v.parse::<i32>().ok());
                            return Ok(SessionCompileOutcome {
                                exit_code: exit.code,
                                cache_outcome,
                            });
                        }
                        _ => {}
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(tag(output_started, io::Error::other(e))),
            }
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| tag(output_started, e))?;
        if n == 0 {
            return Err(tag(
                output_started,
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "SESSION relay closed before Exit",
                ),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Per-invocation relay timing under `SOLDR_PROFILE_STARTUP` (soldr#3800).
/// Disabled probes do nothing per frame beyond one branch. It only observes
/// the bytes the pump already forwards; it never alters them.
struct RelayProbe {
    enabled: bool,
    crate_name: String,
    base: Instant,
    req_sent: Option<Instant>,
    req_sent_unix_ms: u128,
    first_frame_ms: Option<u128>,
    metadata_frame_ms: Option<u128>,
}

impl RelayProbe {
    fn new(rustc_argv: &[String]) -> Self {
        if !crate::startup_profile::profiling_enabled() {
            return Self::disabled();
        }
        Self {
            enabled: true,
            crate_name: crate::startup_profile::crate_name_from_args(rustc_argv)
                .unwrap_or_default()
                .to_string(),
            base: crate::startup_profile::process_start(),
            req_sent: None,
            req_sent_unix_ms: 0,
            first_frame_ms: None,
            metadata_frame_ms: None,
        }
    }

    fn disabled() -> Self {
        Self {
            enabled: false,
            crate_name: String::new(),
            base: Instant::now(),
            req_sent: None,
            req_sent_unix_ms: 0,
            first_frame_ms: None,
            metadata_frame_ms: None,
        }
    }

    fn ms_since_base(&self, at: Instant) -> u128 {
        at.saturating_duration_since(self.base).as_millis()
    }

    fn on_request_sent(&mut self) {
        if self.enabled {
            self.req_sent = Some(Instant::now());
            self.req_sent_unix_ms = crate::startup_profile::unix_ms_now();
        }
    }

    fn on_frame(&mut self) {
        if self.enabled && self.first_frame_ms.is_none() {
            self.first_frame_ms = Some(self.ms_since_base(Instant::now()));
        }
    }

    fn on_output(&mut self, bytes: &[u8]) {
        if self.enabled
            && self.metadata_frame_ms.is_none()
            && crate::startup_profile::contains_metadata_emit(bytes)
        {
            self.metadata_frame_ms = Some(self.ms_since_base(Instant::now()));
        }
    }

    fn on_exit(&self) {
        if !self.enabled {
            return;
        }
        let timings = crate::startup_profile::CompileRelayTimings {
            crate_name: self.crate_name.clone(),
            req_sent_ms: self.req_sent.map_or(0, |t| self.ms_since_base(t)),
            first_frame_ms: self.first_frame_ms,
            metadata_frame_ms: self.metadata_frame_ms,
            exit_ms: self.ms_since_base(Instant::now()),
            req_sent_unix_ms: self.req_sent_unix_ms,
            exit_unix_ms: crate::startup_profile::unix_ms_now(),
        };
        crate::startup_profile::emit_line(&crate::startup_profile::format_compile_relay_line(
            &timings,
        ));
    }
}

#[cfg(test)]
#[path = "session_transport_tests.rs"]
mod tests;
