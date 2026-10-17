//! Daemon SESSION `0x5350` per-connection handler (soldr#2388 Step 6d / #2386
//! Option A).
//!
//! The audit correction (soldr#2365 comment 5233624686) rules #2386 **Option
//! A**: soldr-daemon serves SESSION `0x5350` on the **separate broker-facing
//! backend endpoint** the broker binds and hands over, running the codec-bridge
//! [`serve_session_compile`](crate::daemon::session_serve::serve_session_compile)
//! behind it. `handle_connection` (the private control endpoint) is unchanged — its
//! `Payload{0x5350} → drain_then_close` stays as the defensive default.
//!
//! That endpoint carries three traffic kinds (see running-process
//! `backend_sdk::mux`): the daemon's control wire (none here — SESSION is a
//! dedicated endpoint), `BackendHandle` **liveness probes**, and `0x5350`
//! payload frames. So this handler drives the **full**
//! [`BackendEndpointMux`]: it answers probe frames with
//! [`MuxPoll::ProbeAnswered`] and, on a `0x5350`
//! [`MuxPoll::Payload`](running_process::broker::backend_sdk::MuxPoll::Payload),
//! **takes over** the connection as a streaming SESSION compile.
//!
//! # Handoff — replay, do not drain
//!
//! `mux.poll` is request/reply and pure: it decodes the frame and reports how
//! many bytes it *would* consume, but never advances the buffer. SESSION is a
//! streaming takeover, and [`serve_session_compile`] re-reads the opening
//! `SessionStart` itself via `session_codec`. So on a `0x5350` frame this
//! handler does **not** drain — it hands the still-buffered bytes (the
//! `SessionStart` frame plus any trailing bytes) followed by the live stream to
//! [`serve_session_compile`] through a [`ReplayReader`]. The mux and
//! `session_codec` decode the byte-identical `[1][u32 len][Frame{0x5350}]`
//! envelope, so the re-read is correct and cheap. This mirrors running-process's
//! proven `session_takeover_from_buffered`, minus its child-spawn (soldr owns
//! execution via the embedded zccache service — fable5 answer-A ruling).

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::watch;

use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::backend_sdk::{BackendEndpointMux, LegacyClassification, MuxPoll};
use running_process::broker::protocol::SESSION_PAYLOAD_PROTOCOL;

use crate::core::SoldrPaths;
use crate::daemon::event_batcher::EventBatcher;
use crate::daemon::session_serve::serve_session_compile;
use crate::zccache_embedded::SoldrZccacheService;

/// Concrete legacy detector type of the SESSION mux — a plain `fn` pointer so
/// the mux (and therefore the accept-loop `Arc<..>`) has a nameable type.
pub type SessionMux = BackendEndpointMux<fn(&[u8]) -> LegacyClassification>;

type CompileServiceResult = Result<Arc<SoldrZccacheService>, Arc<str>>;

/// Awaitable publication point for the daemon's embedded compile service.
///
/// The SESSION listener is intentionally started before zccache and the
/// daemon database finish initializing. Backend-handle probes therefore stay
/// responsive during startup, while a real compile waits here instead of
/// making the broker mistake slow initialization for a dead daemon.
#[derive(Clone)]
pub(crate) struct CompileServiceReadiness {
    receiver: watch::Receiver<Option<CompileServiceResult>>,
    /// soldr#3224: the daemon's build-history event batcher. SESSION compiles
    /// record CompileStart/CompileEnd through it, which is what
    /// `soldr daemon builds` aggregates into `crate_count` / `slowest_crate`.
    /// Only `#[cfg(test)]` constructors may leave it unset, so a production
    /// daemon cannot silently stop recording build history again.
    event_batcher: Option<EventBatcher>,
}

pub(crate) struct CompileServicePublisher {
    sender: watch::Sender<Option<CompileServiceResult>>,
}

impl CompileServiceReadiness {
    pub(crate) fn pending(event_batcher: EventBatcher) -> (Self, CompileServicePublisher) {
        let (sender, receiver) = watch::channel(None);
        (
            Self {
                receiver,
                event_batcher: Some(event_batcher),
            },
            CompileServicePublisher { sender },
        )
    }

    pub(crate) fn ready(service: Arc<SoldrZccacheService>, event_batcher: EventBatcher) -> Self {
        let (sender, receiver) = watch::channel(Some(Ok(service)));
        drop(sender);
        Self {
            receiver,
            event_batcher: Some(event_batcher),
        }
    }

    /// [`Self::pending`] for tests that never compile, so need no runtime to
    /// start an [`EventBatcher`] on.
    #[cfg(test)]
    pub(crate) fn pending_without_events() -> (Self, CompileServicePublisher) {
        let (sender, receiver) = watch::channel(None);
        (
            Self {
                receiver,
                event_batcher: None,
            },
            CompileServicePublisher { sender },
        )
    }

    /// [`Self::ready`] for tests that do not assert build history.
    #[cfg(test)]
    pub(crate) fn ready_without_events(service: Arc<SoldrZccacheService>) -> Self {
        let (sender, receiver) = watch::channel(Some(Ok(service)));
        drop(sender);
        Self {
            receiver,
            event_batcher: None,
        }
    }

    /// Wait for the embedded service, bounded by [`SERVICE_READY_DEADLINE`]:
    /// if bringup hangs, every SESSION connection used to park here forever
    /// while its client gave up (soldr#3836 audit).
    async fn wait(&self) -> io::Result<Arc<SoldrZccacheService>> {
        match tokio::time::timeout(SERVICE_READY_DEADLINE, self.wait_unbounded()).await {
            Ok(result) => result,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "embedded compile service not ready after {}s; inspect `soldr daemon status` \
                     and `soldr logs paths`",
                    SERVICE_READY_DEADLINE.as_secs()
                ),
            )),
        }
    }

    async fn wait_unbounded(&self) -> io::Result<Arc<SoldrZccacheService>> {
        let mut receiver = self.receiver.clone();
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result.map_err(|message| io::Error::other(message.to_string()));
            }
            receiver.changed().await.map_err(|_| {
                io::Error::other("embedded compile service initialization ended unexpectedly")
            })?;
        }
    }
}

/// Matches the client's SESSION setup ceiling (the 120 s route ceiling).
const SERVICE_READY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

impl CompileServicePublisher {
    pub(crate) fn publish(&self, result: CompileServiceResult) {
        self.sender.send_replace(Some(result));
    }
}

/// Names an explicit soldr-owned SESSION endpoint socket path.
///
/// The broker launcher assigns this route-local endpoint when it creates the
/// daemon. It is mandatory for broker-owned production startup; tests may set
/// it explicitly to exercise the endpoint without a broker process.
pub const SOLDR_SESSION_ENDPOINT_PATH_ENV: &str = "SOLDR_SESSION_ENDPOINT_PATH";
/// Broker-assigned private endpoint for the daemon's request/response control
/// protocol. Only the broker and daemon receive this value; user-facing
/// clients tunnel through the stable broker endpoint.
pub const SOLDR_CONTROL_ENDPOINT_PATH_ENV: &str = "SOLDR_DAEMON_CONTROL_ENDPOINT_PATH";

pub fn private_control_endpoint_from_session(session_endpoint: &str) -> String {
    sibling_endpoint(session_endpoint, ".control.sock")
}

fn sibling_endpoint(session_endpoint: &str, suffix: &str) -> String {
    session_endpoint.strip_suffix(".session.sock").map_or_else(
        || format!("{session_endpoint}{suffix}"),
        |base| format!("{base}{suffix}"),
    )
}

pub fn resolved_control_endpoint_path(_paths: &SoldrPaths) -> io::Result<std::path::PathBuf> {
    let logical = std::env::var_os(SOLDR_CONTROL_ENDPOINT_PATH_ENV)
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "required daemon endpoint variable {SOLDR_CONTROL_ENDPOINT_PATH_ENV} is unset"
                ),
            )
        })?;
    Ok(runtime_control_endpoint_path(logical))
}

/// Convert a logical endpoint value into the path the transport actually
/// dials: on Windows the logical (filesystem-looking) value lives under the
/// named-pipe namespace, so the `\\.\pipe\` prefix is prepended when absent.
///
/// Public because every dialer must apply it — the daemon's own listener and
/// the CLI's env-resolved path always did, but a test harness deriving the
/// endpoint from the executable path and dialing the raw logical leaf gets
/// `CreateFile` on a relative *file* path: NotFound, reported as
/// `NotRunning` against a demonstrably live daemon. That mismatch was the
/// deterministic, Windows-only `daemon registry query: NotRunning` failure
/// on the msvc target-run lanes.
pub fn runtime_control_endpoint_path(logical: std::path::PathBuf) -> std::path::PathBuf {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        // The broker assigns a filesystem-looking logical path; the
        // Windows transport dials it under the named-pipe namespace.
        const PREFIX: &str = r"\\.\pipe\";
        let rendered = logical.to_string_lossy();
        if rendered.starts_with(PREFIX) {
            logical
        } else {
            std::path::PathBuf::from(format!("{PREFIX}{rendered}"))
        }
    } else {
        logical
    }
}

/// The mux for the SESSION endpoint: serves the `0x5350` lane and declares
/// **no** daemon control wire.
///
/// Distinct from `backend_handle_adoption::soldr_backend_endpoint_mux`, which is
/// built with `served = &[]` for the private control endpoint and MUST NOT be reused —
/// serving `0x5350` there would change `handle_connection`, which the audit
/// correction forbids.
pub fn soldr_session_endpoint_mux(daemon: DaemonProcess) -> SessionMux {
    BackendEndpointMux::new(daemon, &[SESSION_PAYLOAD_PROTOCOL], classify_never_legacy)
}

/// The SESSION endpoint has no daemon control wire: every framed connection is either a
/// `BackendHandle` probe (handled inside the mux) or a `0x5350` SESSION frame.
///
/// `mux.poll` short-circuits an empty buffer to `NeedMoreBytes` before calling
/// this detector, so it never sees an empty slice.
fn classify_never_legacy(_buf: &[u8]) -> LegacyClassification {
    LegacyClassification::NotLegacy
}

/// Serve one accepted SESSION-endpoint connection.
///
/// Drives the mux over a growing read buffer:
/// - [`MuxPoll::ProbeAnswered`] → write the reply verbatim, advance, keep
///   reading (a prober may reuse the connection or hang up);
/// - [`MuxPoll::Payload`] carrying `0x5350` → **replay** the undrained buffer +
///   the live stream into [`serve_session_compile`] and return whatever it does;
/// - clean EOF before any frame → `Ok(())`;
/// - a control-wire verdict → an error (the SESSION endpoint serves no control
///   wire).
///
/// # Errors
///
/// A transport error, a mux framing/protocol error, or an error surfaced by
/// [`serve_session_compile`]. A compile-service failure is *not* an `Err` —
/// [`serve_session_compile`] reports it in-band as a diagnostic `Stderr` + infra
/// `Exit` (no silent close).
pub(crate) async fn serve_session_connection<IO, F>(
    mut io: IO,
    service: &CompileServiceReadiness,
    paths: &SoldrPaths,
    mux: &BackendEndpointMux<F>,
) -> io::Result<()>
where
    IO: AsyncRead + AsyncWrite + Unpin,
    F: Fn(&[u8]) -> LegacyClassification,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match mux.poll(&buf).map_err(io::Error::other)? {
            MuxPoll::NeedMoreBytes => {
                let n = io.read(&mut chunk).await?;
                if n == 0 {
                    // Clean EOF. If nothing is buffered the peer just hung up
                    // (e.g. a closed probe connection); a partial frame is a
                    // truncated request and is dropped the same way.
                    return Ok(());
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            MuxPoll::ProbeAnswered { reply, consumed } => {
                io.write_all(&reply).await?;
                io.flush().await?;
                buf.drain(..consumed);
            }
            MuxPoll::Payload { frame, .. } => {
                debug_assert_eq!(frame.payload_protocol, SESSION_PAYLOAD_PROTOCOL);
                // SESSION takeover: DO NOT drain. Replay the whole buffer (the
                // SessionStart frame + any trailing bytes) so serve_session_compile
                // can re-read the opening SessionStart via session_codec.
                let replay = ReplayReader::new(buf, io);
                let event_batcher = service.event_batcher.clone();
                let service = service.wait().await?;
                return serve_session_compile(replay, &service, paths, event_batcher.as_ref())
                    .await;
            }
            MuxPoll::Legacy => {
                return Err(io::Error::other(
                    "unexpected daemon-control frame on the SESSION endpoint",
                ));
            }
        }
    }
}

/// An [`AsyncRead`] + [`AsyncWrite`] that first replays a leading byte buffer,
/// then delegates to an inner stream. Writes always go to the inner stream.
///
/// Used to hand [`serve_session_compile`] the mux's already-read bytes without
/// consuming them from the wire: the opening `SessionStart` frame is re-read
/// from `head`, and everything after is read straight from `inner`.
pub(crate) struct ReplayReader<S> {
    head: io::Cursor<Vec<u8>>,
    inner: S,
}

impl<S> ReplayReader<S> {
    pub(crate) fn new(head: Vec<u8>, inner: S) -> Self {
        Self {
            head: io::Cursor::new(head),
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for ReplayReader<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let pos = this.head.position() as usize;
        let data = this.head.get_ref();
        if pos < data.len() {
            let remaining = &data[pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.head.set_position((pos + n) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ReplayReader<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

pub fn resolved_session_endpoint_path(_paths: &SoldrPaths) -> io::Result<String> {
    std::env::var_os(SOLDR_SESSION_ENDPOINT_PATH_ENV)
        .filter(|path| !path.is_empty())
        .map(|path| path.to_string_lossy().into_owned())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "required daemon endpoint variable {SOLDR_SESSION_ENDPOINT_PATH_ENV} is unset"
                ),
            )
        })
}

/// Deterministic daemon-side handoff endpoint used only by the broker's
/// SCM_RIGHTS/DuplicateHandle transport. Clients always dial the one stable
/// broker listener.
pub fn handoff_endpoint_path(session_endpoint: &str) -> String {
    sibling_endpoint(session_endpoint, ".handoff.sock")
}

/// The handoff endpoint the control plane binds for `paths`' SESSION endpoint.
pub(crate) fn resolve_handoff_endpoint(paths: &SoldrPaths) -> io::Result<String> {
    let session_endpoint = resolved_session_endpoint_path(paths)?;
    Ok(handoff_endpoint_path(&session_endpoint))
}

/// Resolve the SESSION endpoint listener the daemon serves.
///
/// Requires the broker-provided executable-path-derived endpoint.
///
/// # Errors
///
/// Fails only if the resolved path cannot be bound (e.g. already in use).
pub(crate) fn resolve_session_listener(paths: &SoldrPaths) -> io::Result<Option<SessionListener>> {
    let path = resolved_session_endpoint_path(paths)?;
    bind_session_listener(&path).map(Some)
}

/// The tokio local-socket listener type served by the SESSION endpoint.
pub type SessionListener = interprocess::local_socket::tokio::Listener;

/// Bind a tokio local-socket SESSION listener at `socket_path`.
///
/// Resolves the platform local-socket name the same way running-process's broker
/// SESSION bind does (Unix filesystem path, Windows namespaced pipe), so the
/// daemon endpoint and the broker's relay dial name the same path identically.
pub fn bind_session_listener(socket_path: &str) -> io::Result<SessionListener> {
    // The platform ipc listener leaf owns the host mechanics: Linux
    // filesystem sockets bound with mode 0o600 (+ stale-socket reclaim),
    // macOS bind-then-tighten, Windows namespaced pipes with an
    // owner+SYSTEM SDDL.
    crate::platform::ipc::listener::bind_owner_only_listener(socket_path)
}

fn local_session_name(socket_path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    running_process::broker::server::singleton_bind::wrap_socket_name(socket_path)
        .map_err(io::Error::other)
}

/// Accept SESSION connections on `listener` and serve each through
/// [`serve_session_connection`] on its own task.
///
/// The compile service and mux are cheaply shared (`SoldrZccacheService` is an
/// `Arc` inside; the mux is immutable), so per-connection spawning never
/// serializes concurrent compiles. A per-connection error is logged and the
/// loop continues; only a fatal `accept()` error ends the loop.
///
/// # Errors
///
/// Returns the first fatal `accept()` error (the listener is unusable).
pub async fn serve_session_endpoint(
    listener: SessionListener,
    service: Arc<SoldrZccacheService>,
    event_batcher: EventBatcher,
    paths: SoldrPaths,
    mux: Arc<SessionMux>,
) -> io::Result<()> {
    serve_session_endpoint_with_readiness(
        listener,
        CompileServiceReadiness::ready(service, event_batcher),
        paths,
        mux,
    )
    .await
}

pub(crate) async fn serve_session_endpoint_with_readiness(
    listener: SessionListener,
    service: CompileServiceReadiness,
    paths: SoldrPaths,
    mux: Arc<SessionMux>,
) -> io::Result<()> {
    use interprocess::local_socket::tokio::prelude::*;

    loop {
        let stream = listener.accept().await?;
        if !session_peer_is_current_user(&stream) {
            tracing::warn!(target: "soldr::daemon", "rejected foreign SESSION endpoint peer");
            continue;
        }
        let service = service.clone();
        let paths = paths.clone();
        let mux = Arc::clone(&mux);
        tokio::spawn(async move {
            if let Err(err) = serve_session_connection(stream, &service, &paths, &mux).await {
                eprintln!("soldr-daemon: SESSION endpoint connection ended: {err}");
            }
        });
    }
}

/// The daemon's dedicated handoff control plane (soldr#3102).
///
/// The broker gives the daemon [`DEFAULT_HANDOFF_ACK_DEADLINE`] (5 s) to
/// acknowledge a passed connection and, on expiry, relinquishes the client
/// connection ("abandoned at AwaitAck stage"). The handoff tasks used to run
/// on the compile runtime, whose workers the embedded compile service parks
/// in synchronous store-lock waits and artifact materialization on the
/// cache-hit path. When every worker was parked, a freshly accepted handoff
/// could not write its ACK inside the budget: the broker dropped the
/// connection, the daemon logged `rejected broker connection handoff: Broken
/// pipe`, and the wrapper failed with `Connection reset by peer` -- with
/// broker, daemon, and client all alive. Serving the handoff endpoint on its
/// own single-threaded runtime makes ACK latency independent of compile
/// runtime saturation; only the *serving* of an adopted connection is
/// scheduled onto the compile runtime.
///
/// [`DEFAULT_HANDOFF_ACK_DEADLINE`]: running_process::broker::server::DEFAULT_HANDOFF_ACK_DEADLINE
pub(crate) struct HandoffControlPlane {
    shutdown: Arc<tokio::sync::Notify>,
}

impl HandoffControlPlane {
    /// Stop accepting handoffs. Named like `JoinHandle::abort` so callers
    /// tear the SESSION and handoff endpoints down with one idiom.
    pub(crate) fn abort(&self) {
        self.shutdown.notify_one();
    }
}

/// Bind the handoff endpoint on its own control-plane runtime and start the
/// SESSION accept loop on the current (compile) runtime.
///
/// Returns the bind error synchronously: the handoff endpoint is bound
/// before the SESSION task is spawned, so a failed bind leaves nothing
/// running, exactly as when both listeners were bound up front.
pub(crate) fn spawn_session_endpoint_servers(
    session_listener: SessionListener,
    handoff_endpoint: String,
    readiness: CompileServiceReadiness,
    paths: SoldrPaths,
    mux: Arc<SessionMux>,
) -> io::Result<(tokio::task::JoinHandle<()>, HandoffControlPlane)> {
    let handoff = spawn_handoff_control_plane(
        handoff_endpoint,
        readiness.clone(),
        paths.clone(),
        Arc::clone(&mux),
        tokio::runtime::Handle::current(),
    )?;
    let session = tokio::spawn(async move {
        if let Err(error) =
            serve_session_endpoint_with_readiness(session_listener, readiness, paths, mux).await
        {
            tracing::warn!(target: "soldr::daemon", "SESSION endpoint serve ended: {error}");
        }
    });
    Ok((session, handoff))
}

/// Start the handoff control plane: a dedicated thread running a
/// single-threaded tokio runtime that binds `handoff_endpoint`, receives
/// every broker handoff, acknowledges it, and schedules the adopted
/// connection onto `compile_runtime` for serving.
///
/// Blocks until the endpoint is bound so the caller sees bind errors
/// synchronously.
// Thread-lifetime runtime: the handoff control plane runs until stop is
// signalled (soldr#3836).
#[cfg_attr(dylint_lib = "ban_raw_sync_primitive", allow(ban_raw_sync_primitive))]
pub(crate) fn spawn_handoff_control_plane(
    handoff_endpoint: String,
    service: CompileServiceReadiness,
    paths: SoldrPaths,
    mux: Arc<SessionMux>,
    compile_runtime: tokio::runtime::Handle,
) -> io::Result<HandoffControlPlane> {
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let stop = Arc::clone(&shutdown);
    let (bound_tx, bound_rx) = std::sync::mpsc::channel::<io::Result<()>>();
    std::thread::Builder::new()
        .name("soldr-daemon-handoff".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = bound_tx.send(Err(error));
                    return;
                }
            };
            runtime.block_on(async move {
                let listener = match bind_session_listener(&handoff_endpoint) {
                    Ok(listener) => listener,
                    Err(error) => {
                        let _ = bound_tx.send(Err(error));
                        return;
                    }
                };
                let _ = bound_tx.send(Ok(()));
                tokio::select! {
                    result = serve_handoff_endpoint_with_readiness(
                        listener,
                        service,
                        paths,
                        mux,
                        compile_runtime,
                    ) => {
                        if let Err(error) = result {
                            tracing::warn!(
                                target: "soldr::daemon",
                                "handoff endpoint serve ended: {error}"
                            );
                        }
                    }
                    () = stop.notified() => {}
                }
            });
        })?;
    crate::platform::wait::recv_for(&bound_rx, crate::platform::wait::PROBE).map_err(|_| {
        io::Error::other("handoff control plane exited before binding its endpoint")
    })??;
    Ok(HandoffControlPlane { shutdown })
}

/// Accept broker-to-daemon connection handoffs on the current (control-plane)
/// runtime, acknowledge each one, and dispatch the adopted client connection
/// through the same SESSION handler as the proxy path on `compile_runtime`.
///
/// Nothing on this loop waits for the compile service: the receive, offer
/// verification, and ACK complete here, and `compile_runtime` only receives
/// the acknowledged connection. That is the property soldr#3102 depends on.
pub(crate) async fn serve_handoff_endpoint_with_readiness(
    listener: SessionListener,
    service: CompileServiceReadiness,
    paths: SoldrPaths,
    mux: Arc<SessionMux>,
    compile_runtime: tokio::runtime::Handle,
) -> io::Result<()> {
    use interprocess::local_socket::tokio::prelude::*;

    let expected_service_name =
        std::env::var(running_process::broker::server::BACKEND_ENV_SERVICE_NAME)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_NAME.to_string()
            });

    loop {
        let control = listener.accept().await?;
        if !session_peer_is_current_user(&control) {
            tracing::warn!(target: "soldr::daemon", "rejected foreign handoff endpoint peer");
            continue;
        }
        let service = service.clone();
        let paths = paths.clone();
        let mux = Arc::clone(&mux);
        let expected_service_name = expected_service_name.clone();
        let compile_runtime = compile_runtime.clone();
        tokio::spawn(async move {
            match receive_handed_off_session(control, &expected_service_name).await {
                Ok(client) => {
                    compile_runtime.spawn(async move {
                        let client = match client.into_stream() {
                            Ok(client) => client,
                            Err(error) => {
                                eprintln!(
                                    "soldr-daemon: handed-off SESSION could not be adopted: {error}"
                                );
                                return;
                            }
                        };
                        if let Err(error) =
                            serve_session_connection(client, &service, &paths, &mux).await
                        {
                            eprintln!("soldr-daemon: handed-off SESSION ended: {error}");
                        }
                    });
                }
                Err(error) => {
                    eprintln!("soldr-daemon: rejected broker connection handoff: {error}");
                }
            }
        });
    }
}

fn session_peer_is_current_user(stream: &interprocess::local_socket::tokio::Stream) -> bool {
    use running_process::broker::server::{connection, PeerCredentialPolicy};
    let Some(policy) = PeerCredentialPolicy::current_user() else {
        return false;
    };
    connection::peer_identity_from_tokio_stream(stream).is_ok_and(|peer| policy.allows(&peer))
}

async fn read_handoff_offer_async(
    stream: &mut interprocess::local_socket::tokio::Stream,
    expected_service_name: &str,
) -> io::Result<running_process::broker::protocol::HandoffOffer> {
    use prost::Message as _;
    use running_process::broker::protocol::{
        Frame, FrameKind, HandoffOffer, HANDOFF_PAYLOAD_PROTOCOL,
    };

    let body = read_envelope_body(stream).await?;
    let frame = Frame::decode(body.as_slice()).map_err(io::Error::other)?;
    if FrameKind::try_from(frame.kind) != Ok(FrameKind::Request)
        || frame.payload_protocol != HANDOFF_PAYLOAD_PROTOCOL
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff control connection did not carry a HandoffOffer request",
        ));
    }
    let offer = HandoffOffer::decode(frame.payload.as_slice()).map_err(io::Error::other)?;
    if offer.correlation_id != frame.request_id
        || offer.service_name != expected_service_name
        || offer.token.len() != running_process::broker::server::HANDOFF_TOKEN_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff offer identity, correlation, or token was invalid",
        ));
    }
    Ok(offer)
}

async fn accept_handoff_offer_async(
    stream: &mut interprocess::local_socket::tokio::Stream,
    offer: &running_process::broker::protocol::HandoffOffer,
) -> io::Result<()> {
    use prost::Message as _;
    use running_process::broker::protocol::HandoffAck;
    use tokio::io::AsyncWriteExt as _;

    let ack = HandoffAck {
        token: offer.token.clone(),
        accepted: true,
        error_detail: String::new(),
        correlation_id: offer.correlation_id,
    };
    let frame = running_process::broker::server::handoff::handoff_ack_frame(&ack);
    let bytes =
        running_process::broker::protocol::encode_framed(&frame).map_err(io::Error::other)?;
    stream.write_all(&bytes).await?;
    stream.flush().await
}

async fn read_envelope_body(
    stream: &mut interprocess::local_socket::tokio::Stream,
) -> io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;

    let mut header = [0_u8; 5];
    tokio::time::timeout(
        running_process::broker::server::DEFAULT_HANDOFF_ACK_DEADLINE,
        stream.read_exact(&mut header),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handoff offer header timed out"))??;
    if header[0] != running_process::broker::protocol::ENVELOPE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff offer used the wrong envelope version",
        ));
    }
    let len = u32::from_le_bytes(header[1..].try_into().expect("four bytes")) as usize;
    if len > running_process::broker::protocol::MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff offer exceeded the frame limit",
        ));
    }
    let mut body = vec![0_u8; len];
    tokio::time::timeout(
        running_process::broker::server::DEFAULT_HANDOFF_ACK_DEADLINE,
        stream.read_exact(&mut body),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handoff offer body timed out"))??;
    Ok(body)
}

/// A client connection the broker passed to the daemon: acknowledged on the
/// control plane, not yet registered with the runtime that will serve it.
///
/// The conversion into a tokio stream happens on the compile runtime so the
/// SESSION I/O registers with the reactor that polls it. The control
/// connection itself never leaves the control-plane runtime.
enum HandedOffClient {
    /// Unix: the `SCM_RIGHTS` descriptor, verified against the framed offer.
    Descriptor(crate::platform::ipc::handoff::ReceivedFd),
    /// Windows: the `DuplicateHandle` value carried by the offer.
    HandleValue(u64),
}

impl HandedOffClient {
    fn into_stream(self) -> io::Result<interprocess::local_socket::tokio::Stream> {
        match self {
            Self::Descriptor(fd) => {
                crate::platform::ipc::handoff::session_stream_from_received_fd(fd)
            }
            Self::HandleValue(value) => {
                crate::platform::ipc::handoff::named_pipe_stream_from_handle_value(value)
            }
        }
    }
}

async fn receive_handed_off_session(
    control: interprocess::local_socket::tokio::Stream,
    expected_service_name: &str,
) -> io::Result<HandedOffClient> {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        // DuplicateHandle transport: the offer carries the duplicated
        // pipe handle directly.
        let mut control = control;
        let offer = read_handoff_offer_async(&mut control, expected_service_name).await?;
        if offer.handle_value == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DuplicateHandle offer carried a null handle",
            ));
        }
        // A positive ACK transfers ownership; the handle becomes a tokio
        // stream on the compile runtime that serves it.
        accept_handoff_offer_async(&mut control, &offer).await?;
        Ok(HandedOffClient::HandleValue(offer.handle_value))
    } else {
        // SCM_RIGHTS transport: receive the descriptor (with its token
        // prelude) on the blocking pool, then verify the token against
        // the framed offer before trusting the descriptor.
        let (mut control, received_fd, transport_token) = tokio::task::spawn_blocking(move || {
            crate::platform::ipc::handoff::receive_unix_descriptor(control)
        })
        .await
        .map_err(|error| io::Error::other(format!("handoff receive task failed: {error}")))??;
        let offer = match read_handoff_offer_async(&mut control, expected_service_name).await {
            Ok(offer) => offer,
            Err(error) => {
                crate::platform::ipc::handoff::close_received_fd(received_fd);
                return Err(error);
            }
        };
        if offer.token.as_slice() != transport_token {
            crate::platform::ipc::handoff::close_received_fd(received_fd);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SCM_RIGHTS token did not match the framed handoff offer",
            ));
        }
        // A positive ACK transfers ownership. The descriptor becomes a
        // tokio stream on the compile runtime that serves it; an ACK the
        // broker no longer wants closes the descriptor here.
        if let Err(error) = accept_handoff_offer_async(&mut control, &offer).await {
            crate::platform::ipc::handoff::close_received_fd(received_fd);
            return Err(error);
        }
        Ok(HandedOffClient::Descriptor(received_fd))
    }
}

#[cfg(test)]
mod tests;
