//! End-to-end tests for the SESSION endpoint per-connection handler
//! (soldr#2388 Step 6d). Both Option-A properties are proven on the **same**
//! handler over an in-memory duplex:
//!
//! - `session_endpoint_serves_real_compile_via_mux`: a real rustc compile flows
//!   client → `serve_session_connection` (mux `Payload{0x5350}` → replay →
//!   `serve_session_compile`) → embedded zccache → `SessionFrame`s, exit 0 with
//!   `cache_outcome`/`compile_id` metadata. No spawned child at the session
//!   layer.
//! - `session_endpoint_answers_backend_handle_probe`: a `BackendHandle` liveness
//!   probe on the same handler is answered (mux `ProbeAnswered`), so the SESSION
//!   endpoint stays broker-probe-compatible — the coexistence Option A requires.

use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::protocol::{
    encode_framed, try_decode_framed, Endpoint, Frame, BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
};
use running_process::broker::protocol_v2::{
    session_frame, SessionEnvVar, SessionExit, SessionFrame,
};
use running_process::broker::session_codec::{encode_session_frame, try_decode_session_frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use std::sync::Arc;

use super::{
    bind_session_listener, handoff_endpoint_path, local_session_name,
    private_control_endpoint_from_session, serve_handoff_endpoint_with_readiness,
    serve_session_connection, serve_session_endpoint_with_readiness, soldr_session_endpoint_mux,
    spawn_handoff_control_plane, CompileServiceReadiness,
};
use crate::core::SoldrPaths;
use crate::zccache_embedded::SoldrZccacheService;

/// Nonce length for a `BackendHandle` endpoint probe request
/// (`endpoint_probe_request_from_frame` requires exactly 32 bytes).
const PROBE_NONCE_BYTES: usize = 32;

// The macOS bind-then-tighten permission test and the Unix
// missing-parent bind test live in `tests/daemon_session_endpoint.rs`
// (`#![cfg(unix)]` / `#![cfg(target_os = "macos")]`) — they exercise
// host-specific bind mechanics now owned by the platform listener leaf.

#[test]
fn private_endpoints_are_sibling_names_of_the_daemon_session_path() {
    let session = "/home/me/.soldr/routes/a/soldr-daemon.session.sock";
    assert_eq!(
        private_control_endpoint_from_session(session),
        "/home/me/.soldr/routes/a/soldr-daemon.control.sock"
    );
    assert_eq!(
        handoff_endpoint_path(session),
        "/home/me/.soldr/routes/a/soldr-daemon.handoff.sock"
    );
}

fn test_daemon_identity() -> DaemonProcess {
    let endpoint = Endpoint {
        namespace_id: "shared".into(),
        path: "rpb-session-endpoint-e2e".into(),
    };
    DaemonProcess::current_process(endpoint, Some(30)).expect("current-process identity")
}

/// Read `SessionFrame`s off `client` until the terminal `Exit`.
async fn read_until_exit<R>(client: &mut R) -> (Vec<u8>, SessionExit)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut stdout = Vec::new();
    loop {
        while let Some(decoded) = try_decode_session_frame(&buf).expect("decode session frame") {
            let consumed = decoded.consumed;
            let kind = decoded.frame.kind.clone();
            buf.drain(..consumed);
            match kind {
                Some(session_frame::Kind::Stdout(b)) => stdout.extend_from_slice(&b),
                Some(session_frame::Kind::Stderr(_)) => {}
                Some(session_frame::Kind::Exit(exit)) => return (stdout, exit),
                _ => {}
            }
        }
        let n = client.read(&mut chunk).await.expect("read from handler");
        assert!(n > 0, "handler closed before sending Exit");
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[test]
fn session_endpoint_serves_real_compile_via_mux() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            // Real rustc + the repo's pinned toolchain (mirrors the 6c
            // bridge test), but driven through the endpoint's mux handler.
            let current_dir = std::env::current_dir().expect("cwd");
            let repo = current_dir
                .ancestors()
                .find(|c| c.join("rust-toolchain.toml").is_file())
                .expect("find repo rust-toolchain.toml");
            let pinned = crate::core::read_rust_toolchain_manifest(repo)
                .expect("read rust-toolchain.toml")
                .channel
                .expect("rust-toolchain.toml declares a channel");
            let rustc = crate::test_support::rustc_from_env_or_path();

            let temp = tempfile::tempdir().expect("tempdir");
            let project = temp.path().join("workspace");
            std::fs::create_dir_all(project.join("src")).expect("create src dir");
            std::fs::write(
                project.join("src/lib.rs"),
                "pub fn endpoint_answer() -> u32 { 6060 }\n",
            )
            .expect("write source");

            let args: Vec<String> = vec![
                "--edition".into(),
                "2021".into(),
                "--crate-type".into(),
                "lib".into(),
                "--crate-name".into(),
                "soldr_session_endpoint_e2e".into(),
                "--emit=metadata".into(),
                "-C".into(),
                "metadata=sep1".into(),
                "--out-dir".into(),
                "target/debug/deps".into(),
                "src/lib.rs".into(),
            ];
            let mut env: Vec<SessionEnvVar> = std::env::vars()
                .filter(|(k, _)| k != "RUSTUP_TOOLCHAIN")
                .map(|(key, value)| SessionEnvVar { key, value })
                .collect();
            env.push(SessionEnvVar {
                key: "RUSTUP_TOOLCHAIN".into(),
                value: pinned,
            });
            let start = running_process::broker::protocol_v2::SessionStart {
                program: rustc.display().to_string(),
                args,
                cwd: project.display().to_string(),
                env,
                clear_inherited_env: true,
                environment_policy: running_process::broker::protocol_v2::EnvironmentPolicy::Clear
                    as i32,
            };

            let daemon = test_daemon_identity();
            let paths = SoldrPaths::with_root(temp.path().join("root"));
            let service = SoldrZccacheService::start(&paths, &daemon)
                .await
                .expect("start embedded zccache service");
            let service = CompileServiceReadiness::ready_without_events(Arc::new(service));
            let mux = soldr_session_endpoint_mux(daemon);

            let (mut client, server) = tokio::io::duplex(1 << 20);
            let client_fut = async {
                let hello = encode_session_frame(
                    &SessionFrame {
                        kind: Some(session_frame::Kind::Start(start)),
                    },
                    0,
                )
                .expect("encode SessionStart");
                client.write_all(&hello).await.expect("send SessionStart");
                client.flush().await.expect("flush");
                read_until_exit(&mut client).await
            };

            let (handler_res, (stdout, exit)) = tokio::join!(
                serve_session_connection(server, &service, &paths, &mux),
                client_fut
            );
            handler_res.expect("handler serve ok");

            assert_eq!(
                exit.code,
                0,
                "real rustc compile must succeed through the endpoint handler; stdout={}",
                String::from_utf8_lossy(&stdout)
            );
            assert!(
                exit.metadata
                    .contains_key(crate::daemon::session_sink::META_CACHE_OUTCOME),
                "SessionExit.metadata carries cache_outcome"
            );
            assert!(
                exit.metadata
                    .contains_key(crate::daemon::session_sink::META_COMPILE_ID),
                "SessionExit.metadata carries compile_id"
            );
        });
}

#[test]
fn session_endpoint_answers_backend_handle_probe() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let daemon = test_daemon_identity();
            let mux = soldr_session_endpoint_mux(daemon);

            // Keep initialization deliberately unresolved: a broker liveness
            // probe must never await the heavyweight compile service.
            let temp = tempfile::tempdir().expect("tempdir");
            let paths = SoldrPaths::with_root(temp.path().join("root"));
            let (service, _publisher) = CompileServiceReadiness::pending_without_events();

            // Build a BackendHandle endpoint probe request: Frame{0xB232, nonce}.
            let nonce = vec![7u8; PROBE_NONCE_BYTES];
            let request_id = 42;
            let probe = Frame::request(BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, nonce)
                .with_request_id(request_id);
            let probe_bytes = encode_framed(&probe).expect("encode probe request");

            let (mut client, server) = tokio::io::duplex(1 << 16);
            let client_fut = async {
                client.write_all(&probe_bytes).await.expect("send probe");
                client.flush().await.expect("flush");
                // Read the framed reply, then hang up so the handler's next read
                // hits EOF and it returns cleanly.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let reply = loop {
                    if let Some(decoded) =
                        try_decode_framed(&buf).expect("decode framed probe reply")
                    {
                        break decoded.frame;
                    }
                    let n = client.read(&mut chunk).await.expect("read probe reply");
                    assert!(n > 0, "handler closed before answering probe");
                    buf.extend_from_slice(&chunk[..n]);
                };
                client.shutdown().await.ok();
                reply
            };

            let (handler_res, reply) = tokio::join!(
                serve_session_connection(server, &service, &paths, &mux),
                client_fut
            );
            handler_res.expect("handler serve ok");

            assert_eq!(
                reply.payload_protocol, BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
                "reply is a BackendHandle probe response"
            );
            assert_eq!(
                reply.request_id, request_id,
                "probe response echoes the request id"
            );
        });
}

/// A unique local-socket name for a test: a filesystem path under `temp` on
/// Unix, a namespaced pipe name on Windows.
fn unique_session_socket(temp: &tempfile::TempDir) -> String {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let _ = temp;
        format!("soldr-session-6d-{}-{}", std::process::id(), nanos)
    } else {
        temp.path().join("session.sock").display().to_string()
    }
}

#[test]
fn session_endpoint_accept_loop_binds_and_dispatches_probe() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use interprocess::local_socket::tokio::prelude::*;
            use interprocess::local_socket::tokio::Stream;

            let temp = tempfile::tempdir().expect("tempdir");
            let socket = unique_session_socket(&temp);
            let listener = bind_session_listener(&socket).expect("bind SESSION listener");

            let paths = SoldrPaths::with_root(temp.path().join("root"));
            let service = CompileServiceReadiness::ready_without_events(Arc::new(
                SoldrZccacheService::start(&paths, &test_daemon_identity())
                    .await
                    .expect("start embedded zccache service"),
            ));
            let mux = Arc::new(soldr_session_endpoint_mux(test_daemon_identity()));
            let server = tokio::spawn(serve_session_endpoint_with_readiness(
                listener, service, paths, mux,
            ));

            // Dial the real transport (the listener is bound before accept runs,
            // so the connect queues in the backlog).
            let name = local_session_name(&socket).expect("socket name");
            let mut client = Stream::connect(name)
                .await
                .expect("connect SESSION endpoint");

            let nonce = vec![9u8; PROBE_NONCE_BYTES];
            let request_id = 77;
            let probe = Frame::request(BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, nonce)
                .with_request_id(request_id);
            let probe_bytes = encode_framed(&probe).expect("encode probe request");
            client.write_all(&probe_bytes).await.expect("send probe");
            client.flush().await.expect("flush");

            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let reply = loop {
                if let Some(decoded) = try_decode_framed(&buf).expect("decode framed probe reply") {
                    break decoded.frame;
                }
                let n = client.read(&mut chunk).await.expect("read probe reply");
                assert!(n > 0, "endpoint closed before answering probe");
                buf.extend_from_slice(&chunk[..n]);
            };

            assert_eq!(
                reply.payload_protocol, BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
                "reply is a BackendHandle probe response"
            );
            assert_eq!(reply.request_id, request_id, "probe response echoes id");

            server.abort();
        });
}

/// Fake-broker half of one `SCM_RIGHTS` handoff: sends the descriptor plus
/// token prelude, writes the framed `HandoffOffer`, and reports how long the
/// daemon took to acknowledge -- or `None` when no ACK arrived inside the
/// broker's [`DEFAULT_HANDOFF_ACK_DEADLINE`].
///
/// [`DEFAULT_HANDOFF_ACK_DEADLINE`]: running_process::broker::server::DEFAULT_HANDOFF_ACK_DEADLINE
fn broker_handoff_ack_latency(
    handoff_endpoint: &str,
) -> std::io::Result<Option<std::time::Duration>> {
    use prost::Message as _;
    use running_process::broker::protocol::{HandoffAck, HandoffOffer};
    use running_process::broker::server::handoff::handoff_offer_frame;
    use running_process::broker::server::{DEFAULT_HANDOFF_ACK_DEADLINE, HANDOFF_TOKEN_BYTES};
    use std::io::{Read as _, Write as _};

    let token = [7u8; HANDOFF_TOKEN_BYTES];
    let started = std::time::Instant::now();
    let (mut control, _client_peer) =
        crate::platform::ipc::handoff::send_test_handoff_descriptor(handoff_endpoint, &token)?;
    let service_name = std::env::var(running_process::broker::server::BACKEND_ENV_SERVICE_NAME)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_NAME.to_string()
        });
    let offer = HandoffOffer {
        handle_value: 0,
        token: token.to_vec(),
        service_name,
        correlation_id: 41,
    };
    let bytes = encode_framed(&handoff_offer_frame(&offer)).expect("encode offer");
    control.write_all(&bytes)?;
    control.flush()?;

    // The sync stream has no read timeout; bound the wait with a channel.
    let (ack_tx, ack_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            if let Ok(Some(decoded)) = try_decode_framed(&buf) {
                let _ = ack_tx.send(decoded.frame);
                return;
            }
            match control.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    });
    match ack_rx.recv_timeout(DEFAULT_HANDOFF_ACK_DEADLINE) {
        Ok(frame) => {
            let ack = HandoffAck::decode(frame.payload.as_slice()).expect("decode ack");
            assert!(
                ack.accepted,
                "daemon rejected the handoff: {}",
                ack.error_detail
            );
            assert_eq!(ack.token, token.to_vec(), "ack echoes the offer token");
            assert_eq!(ack.correlation_id, 41, "ack echoes the correlation id");
            Ok(Some(started.elapsed()))
        }
        Err(_) => Ok(None),
    }
}

/// Holds the compile runtime's workers parked until dropped.
struct ParkedWorkers {
    release: Arc<std::sync::Barrier>,
}

impl Drop for ParkedWorkers {
    fn drop(&mut self) {
        self.release.wait();
    }
}

/// Park every worker of `runtime` in a synchronous wait -- the shape of the
/// embedded cache's store-lock and materialization stalls -- and prove it
/// took: a fresh task must fail to run within 500 ms. The workers stay parked
/// until the returned guard is dropped.
///
/// soldr#3965: deterministic. Each stall task is spawned only after the
/// previous one reported that it holds a worker, so the next one can only run
/// on a free worker (spawn order is not scheduling order, and a batch of
/// spawns can queue behind a worker that is about to block). Readiness is a
/// signal, not a polling deadline, and the release is the guard, not a sleep
/// that could expire before the test finishes on a loaded runner.
fn saturate_runtime_workers(runtime: &tokio::runtime::Runtime, workers: usize) -> ParkedWorkers {
    let release = Arc::new(std::sync::Barrier::new(workers + 1));
    for _ in 0..workers {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let release = Arc::clone(&release);
        runtime.spawn(async move {
            let _ = started_tx.send(());
            release.wait();
        });
        started_rx
            .recv()
            .expect("stall task dropped its readiness signal without starting");
    }
    let parked = ParkedWorkers { release };
    let probe = runtime.spawn(async {});
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        !probe.is_finished(),
        "premise: the compile runtime's workers must all be parked for this test to mean anything"
    );
    parked
}

/// soldr#3102: the broker gives the daemon `DEFAULT_HANDOFF_ACK_DEADLINE`
/// (5 s) to acknowledge a passed connection and relinquishes it on expiry.
/// The wrapper then fails with `Connection reset by peer` although broker,
/// daemon, and client are all alive. The daemon must therefore acknowledge
/// handoffs even when every compile-runtime worker is parked in a
/// synchronous cache-hit wait.
///
/// Negative control first: a handoff endpoint served *on* the saturated
/// compile runtime produces no ACK inside the budget -- the exact failure.
/// Then the control plane: bound on its own runtime, the same handoff is
/// acknowledged well inside the budget while the compile runtime stays
/// parked.
#[test]
fn handoff_ack_does_not_wait_for_a_saturated_compile_runtime() {
    use running_process::broker::server::DEFAULT_HANDOFF_ACK_DEADLINE;

    const WORKERS: usize = 2;
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    let (readiness, _publisher) = CompileServiceReadiness::pending_without_events();
    let mux = Arc::new(soldr_session_endpoint_mux(test_daemon_identity()));

    // The Windows transport is DuplicateHandle; the descriptor fixture
    // reports Unsupported there and the test has nothing to drive.
    let probe_endpoint = handoff_endpoint_path(&unique_session_socket(&temp));
    if matches!(
        crate::platform::ipc::handoff::send_test_handoff_descriptor(
            &probe_endpoint,
            &[0u8; running_process::broker::server::HANDOFF_TOKEN_BYTES],
        ),
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported
    ) {
        eprintln!("skipping: descriptor handoff is not the transport on this host");
        return;
    }

    let compile_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .enable_all()
        .build()
        .expect("compile runtime");

    // Negative control: the pre-#3102 arrangement, endpoint on the compile
    // runtime. Bind while the workers are free, then park them.
    let shared_endpoint = temp
        .path()
        .join("shared.handoff.sock")
        .display()
        .to_string();
    let shared_listener = compile_runtime
        .block_on(async { bind_session_listener(&shared_endpoint) })
        .expect("bind shared-runtime handoff endpoint");
    let _shared_task = compile_runtime.spawn(serve_handoff_endpoint_with_readiness(
        shared_listener,
        readiness.clone(),
        paths.clone(),
        Arc::clone(&mux),
        compile_runtime.handle().clone(),
    ));
    let parked = saturate_runtime_workers(&compile_runtime, WORKERS);
    let shared_latency =
        broker_handoff_ack_latency(&shared_endpoint).expect("drive shared handoff");
    assert!(
        shared_latency.is_none(),
        "control: a handoff served on the saturated compile runtime must miss the \
         broker's ACK deadline, got {shared_latency:?}"
    );

    // The fix: the control plane binds and acknowledges on its own runtime
    // while the compile runtime is still parked.
    let isolated_endpoint = temp
        .path()
        .join("isolated.handoff.sock")
        .display()
        .to_string();
    let control_plane = spawn_handoff_control_plane(
        isolated_endpoint.clone(),
        readiness,
        paths,
        mux,
        compile_runtime.handle().clone(),
    )
    .expect("bind control-plane handoff endpoint");
    let latency = broker_handoff_ack_latency(&isolated_endpoint)
        .expect("drive isolated handoff")
        .expect("the control plane must acknowledge inside the broker's ACK deadline");
    assert!(
        latency < DEFAULT_HANDOFF_ACK_DEADLINE / 2,
        "ACK latency {latency:?} must not track the compile runtime stall"
    );

    control_plane.abort();
    drop(parked);
    compile_runtime.shutdown_background();
}
