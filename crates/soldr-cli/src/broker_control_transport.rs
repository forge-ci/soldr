//! User-facing daemon control traffic over the one stable broker listener.
//!
//! `soldr-daemon` retains a private control listener for its own lifecycle
//! implementation. The CLI installs this connector once at process startup,
//! so status, shutdown, cache metadata, build logs, GC, and session accounting
//! never derive or dial that daemon endpoint themselves.

use prost::Message as _;
use running_process::broker::protocol::{
    encode_framed, Frame, FrameKind, PayloadEncoding, ENVELOPE_VERSION, MAX_FRAME_BYTES,
    PROTOCOL_VERSION,
};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::broker_server::{
    DaemonControlTunnelReply, DaemonControlTunnelRequest, DAEMON_CONTROL_PAYLOAD_PROTOCOL,
};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) async fn relay_to_daemon(
    client: interprocess::local_socket::tokio::Stream,
    endpoint: &str,
) -> io::Result<()> {
    let endpoint = crate::daemon::session_endpoint::runtime_control_endpoint_path(
        std::path::PathBuf::from(endpoint),
    );
    let backend = crate::platform::ipc::connect::open_pipe_with_retry(&endpoint).await?;
    relay_control_stream(client, backend.stream).await
}

async fn relay_control_stream<Client, Backend>(client: Client, backend: Backend) -> io::Result<()>
where
    Client: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    Backend: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut client_rx, mut client_tx) = tokio::io::split(client);
    let (mut backend_rx, mut backend_tx) = tokio::io::split(backend);
    // Control clients leave their connection open until they receive a reply.
    // A completed direction means the other peer must be disconnected too.
    // Local sockets can acknowledge AsyncWrite::shutdown without half-closing.
    tokio::select! {
        result = tokio::io::copy(&mut client_rx, &mut backend_tx) => result.map(|_| ()),
        result = tokio::io::copy(&mut backend_rx, &mut client_tx) => result.map(|_| ()),
    }
}

pub(crate) fn install() -> Result<(), &'static str> {
    crate::daemon::client::install_control_connector(Arc::new(BrokerControlConnector))
}

struct BrokerControlConnector;

impl crate::daemon::client::ControlConnector for BrokerControlConnector {
    fn connect(
        &self,
        _endpoint_marker: &Path,
        timeout: Duration,
    ) -> io::Result<crate::daemon::client::BoxedControlStream> {
        // Route derivation may hash a freshly-built daemon image. Complete it
        // before connecting so the broker's post-connect first-frame deadline
        // measures transport responsiveness, not client-side preparation.
        let service_name = crate::daemon::backend_handle_adoption::broker_service_name()?;
        let endpoint =
            crate::broker_identity::ResolvedBrokerEndpoint::resolve().map_err(io::Error::other)?;
        // A replacement broker performs one exact BackendHandle verification
        // before it can re-adopt a surviving daemon. That verification hashes
        // the executable and can exceed the ordinary 2s status reply budget
        // for a large debug image. The route handshake gets a separate bounded
        // allowance; once accepted, restore the caller's request timeout.
        let route_timeout = route_handshake_timeout(timeout);
        let stream = crate::platform::ipc::control::connect(endpoint.bind_endpoint, route_timeout)?;
        let mut stream = negotiate_control_tunnel(stream, route_timeout, service_name)?;
        crate::platform::ipc::control::configure_timeouts(
            &mut stream,
            timeout.max(Duration::from_millis(200)),
            timeout,
        )?;
        Ok(Box::new(stream))
    }
}

fn route_handshake_timeout(request_timeout: Duration) -> Duration {
    if request_timeout <= Duration::from_millis(100) {
        request_timeout
    } else {
        request_timeout.max(Duration::from_secs(30))
    }
}

fn negotiate_control_tunnel<S: Read + Write>(
    mut stream: S,
    timeout: Duration,
    service_name: String,
) -> io::Result<S> {
    let frame = control_tunnel_frame(timeout, service_name);
    let request_id = frame.request_id;
    stream.write_all(&encode_framed(&frame).map_err(io::Error::other)?)?;
    stream.flush()?;

    validate_control_tunnel_reply(read_broker_frame(&mut stream)?, request_id)?;
    Ok(stream)
}

fn next_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed).max(1)
}

fn control_tunnel_frame(timeout: Duration, service_name: String) -> Frame {
    let host = running_process::broker::host_identity::current();
    let request = DaemonControlTunnelRequest {
        service_name,
        machine_id: host.machine_id,
        boot_id: host.boot_id,
    };
    Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Request as i32,
        payload_protocol: DAEMON_CONTROL_PAYLOAD_PROTOCOL,
        payload: request.encode_to_vec(),
        request_id: next_request_id(),
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: unix_deadline_ms(timeout),
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

fn validate_control_tunnel_reply(reply_frame: Frame, request_id: u64) -> io::Result<()> {
    if reply_frame.request_id != request_id
        || reply_frame.payload_protocol != DAEMON_CONTROL_PAYLOAD_PROTOCOL
        || FrameKind::try_from(reply_frame.kind) != Ok(FrameKind::Response)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker returned a mismatched daemon-control tunnel reply",
        ));
    }
    let reply = DaemonControlTunnelReply::decode(reply_frame.payload.as_slice())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !reply.accepted {
        return Err(io::Error::new(
            if reply.not_running {
                io::ErrorKind::NotFound
            } else {
                io::ErrorKind::PermissionDenied
            },
            reply.error_detail,
        ));
    }
    Ok(())
}

fn read_broker_frame(stream: &mut impl Read) -> io::Result<Frame> {
    let mut header = [0_u8; 5];
    stream.read_exact(&mut header)?;
    if header[0] != ENVELOPE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported broker framing version",
        ));
    }
    let len = u32::from_le_bytes(header[1..].try_into().expect("four bytes")) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker frame exceeds the maximum size",
        ));
    }
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body)?;
    Frame::decode(body.as_slice())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn unix_deadline_ms(timeout: Duration) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_add(timeout)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn control_relay_closes_backend_when_client_disconnects() {
        use std::future::{poll_fn, Future};
        use std::pin::pin;
        use std::task::Poll;
        use tokio::io::AsyncReadExt;

        let (client, accepted) = tokio::io::duplex(64);
        let (backend, mut daemon) = tokio::io::duplex(64);
        drop(client);
        {
            let mut relay = pin!(relay_control_stream(accepted, backend));
            poll_fn(|cx| {
                let Poll::Ready(result) = relay.as_mut().poll(cx) else {
                    panic!("relay waited for the daemon after the client disconnected");
                };
                Poll::Ready(result)
            })
            .await
            .expect("relay closes cleanly");
        }
        assert_eq!(daemon.read(&mut [0]).await.expect("daemon read"), 0);
    }

    #[test]
    fn control_tunnel_deadline_is_future_and_bounded() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let deadline = unix_deadline_ms(Duration::from_secs(2));
        assert!(deadline >= before + 1_900);
        assert!(deadline <= before + 2_100);
    }

    #[test]
    fn hot_path_timeout_is_not_inflated_by_route_handshake() {
        assert_eq!(
            route_handshake_timeout(Duration::from_millis(40)),
            Duration::from_millis(40)
        );
        assert_eq!(
            route_handshake_timeout(Duration::from_secs(2)),
            Duration::from_secs(30)
        );
        assert_eq!(
            route_handshake_timeout(Duration::from_secs(45)),
            Duration::from_secs(45)
        );
    }
}
