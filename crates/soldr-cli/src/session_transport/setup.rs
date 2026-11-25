//! Compile-session setup through the broker, before relaying compiler output.

use std::future::Future;

use tokio::time::Instant;

use running_process::broker::capabilities::CAP_HANDLE_PASSING;
use running_process::broker::protocol_v2::{SessionFrame, SessionStart};
use running_process::broker::session_codec::encode_session_frame;

use super::{
    broker_hello_frame, broker_hello_retryable, busy_jitter, connect_broker_with_busy_retry,
    encode_framed, io, local_session_name, read_negotiated, session_debug_enabled, session_frame,
    session_socket_path, AsyncWriteExt, Hello, Message, SessionEnvVar, SessionError, Stream,
    ENVELOPE_VERSION,
};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(super) struct HandoffTimeout(pub(super) String);

pub(super) async fn negotiate_with_proxy_retry<T, F>(
    service_name: &str,
    deadline: Instant,
    mut negotiate: impl FnMut(bool) -> F,
) -> Result<T, SessionError>
where
    F: Future<Output = Result<T, SessionError>>,
{
    match negotiate(true).await {
        Err(error)
            if Instant::now() < deadline
                && error
                    .source
                    .get_ref()
                    .is_some_and(|cause| cause.is::<HandoffTimeout>()) =>
        {
            // Negotiation dropped the first stream. The new Hello re-resolves
            // the broker endpoint and requests its existing proxy path.
            eprintln!(
                "soldr: broker handoff deadline expired (route {service_name}); retrying once over a fresh broker proxy connection"
            );
            negotiate(false).await
        }
        result => result,
    }
}

pub(super) async fn establish_session(
    service_name: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
    deadline: Instant,
) -> Result<Stream, SessionError> {
    let mut stream = negotiate_with_proxy_retry(service_name, deadline, |request_handoff| {
        negotiate_session(service_name, request_handoff)
    })
    .await?;

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

async fn negotiate_session(
    service_name: &str,
    request_handoff: bool,
) -> Result<Stream, SessionError> {
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
        client_capabilities: if request_handoff {
            CAP_HANDLE_PASSING
        } else {
            0
        },
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
    loop {
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
            Ok(_) => return Ok(candidate),
            Err(error)
                if request_handoff && hello_attempt < 3 && broker_hello_retryable(&error) =>
            {
                tokio::time::sleep(busy_jitter()).await;
            }
            Err(error) => return Err(SessionError::pre_output(error)),
        }
    }
}

pub(super) fn compile_session_start(
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
