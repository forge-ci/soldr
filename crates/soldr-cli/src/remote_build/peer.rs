//! Relay-assisted transport for the existing loopback HTTP/2 protocol.

use std::future::Future;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use iroh::endpoint::{presets, Connection, QuicTransportConfig, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

const ALPN: &[u8] = b"soldr-remote-http/1";
use super::oauth::{Signal, SIGNAL};
const CONNECT_DEADLINE: Duration = Duration::from_secs(15);
const ACCEPTED: &[u8] = b"\x01";

fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

pub(super) fn identity(path: &Path) -> io::Result<SecretKey> {
    match std::fs::read(path) {
        Ok(bytes) => return decode_key(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    crate::platform::fs::permissions::restore_mode(file.path(), Some(0o600))?;
    file.write_all(&SecretKey::generate().to_bytes())?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.error),
    }
    decode_key(std::fs::read(path)?)
}

fn decode_key(bytes: Vec<u8>) -> io::Result<SecretKey> {
    let seed = bytes
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "expected a 32-byte peer key"))?;
    Ok(SecretKey::from_bytes(&seed))
}

pub(super) async fn loopback_listener(address: SocketAddr) -> io::Result<TcpListener> {
    if !address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Remote build HTTP listeners must bind to loopback",
        ));
    }
    TcpListener::bind(address).await
}

async fn endpoint(key: &Path, relay: RelayUrl, relay_only: bool) -> io::Result<Endpoint> {
    let keepalive = Duration::from_secs(2);
    let transport = QuicTransportConfig::builder()
        .max_idle_timeout(Some(Duration::from_secs(30).try_into().map_err(io_error)?))
        .keep_alive_interval(keepalive)
        .default_path_keep_alive_interval(keepalive)
        .build();
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(identity(key)?)
        .alpns(vec![ALPN.to_vec()])
        .clear_address_lookup()
        .relay_mode(RelayMode::custom([relay]))
        .transport_config(transport);
    if relay_only {
        builder = builder.clear_ip_transports();
    }
    builder.bind().await.map_err(io_error)
}

pub(super) async fn serve(key: &Path, target: SocketAddr) -> io::Result<()> {
    let endpoint = endpoint(key, SIGNAL.parse().map_err(io_error)?, false).await?;
    let runner = endpoint.id();
    let signal = Signal::new()?;
    eprintln!("Soldr runner peer ID: {runner}");
    accept_peers(endpoint, target, move |client| {
        let signal = signal.clone();
        async move { signal.check(client, runner).await }
    })
    .await
}

async fn accept_peers<F, A>(endpoint: Endpoint, target: SocketAddr, authorize: F) -> io::Result<()>
where
    F: Fn(EndpointId) -> A + Send + Sync + 'static,
    A: Future<Output = io::Result<Duration>> + Send,
{
    let authorize = Arc::new(authorize);
    while let Some(incoming) = endpoint.accept().await {
        let authorize = Arc::clone(&authorize);
        tokio::spawn(async move {
            let Ok(Ok(connection)) = tokio::time::timeout(CONNECT_DEADLINE, incoming).await else {
                return;
            };
            let grant = match authorize(connection.remote_id()).await {
                Ok(grant) if !grant.is_zero() => grant,
                Ok(_) => {
                    connection.close(1u32.into(), b"peer is not authorized");
                    return;
                }
                Err(error) => {
                    eprintln!("Soldr runner could not verify peer authorization: {error}");
                    connection.close(1u32.into(), b"peer authorization unavailable");
                    return;
                }
            };
            match tokio::time::timeout(grant, serve_peer(&connection, target)).await {
                Ok(Err(error)) => tracing::debug!(%error, "remote build peer disconnected"),
                Err(_) => connection.close(2u32.into(), b"GitHub authorization expired"),
                Ok(Ok(())) => {}
            }
        });
    }
    Err(io_error("remote build peer endpoint stopped"))
}

async fn serve_peer(connection: &Connection, target: SocketAddr) -> io::Result<()> {
    let mut ready = connection.open_uni().await.map_err(io_error)?;
    ready.write_all(ACCEPTED).await.map_err(io_error)?;
    ready.finish().map_err(io_error)?;
    while let Ok((send, recv)) = connection.accept_bi().await {
        tokio::spawn(async move {
            let result = async { forward(TcpStream::connect(target).await?, send, recv).await };
            if let Err(error) = result.await {
                tracing::debug!(%error, "remote runner HTTP stream ended");
            }
        });
    }
    Ok(())
}

async fn dial(endpoint: &Endpoint, runner: &EndpointAddr) -> io::Result<Connection> {
    tokio::time::timeout(CONNECT_DEADLINE, async {
        let connection = endpoint
            .connect(runner.clone(), ALPN)
            .await
            .map_err(io_error)?;
        let mut ready = connection.accept_uni().await.map_err(io_error)?;
        if ready.read_to_end(1).await.map_err(io_error)? != ACCEPTED {
            return Err(io_error(
                "remote runner sent an invalid peer acknowledgement",
            ));
        }
        Ok(connection)
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "remote runner peer connection timed out",
        )
    })?
}

pub(super) async fn connect(
    key: &Path,
    runner: EndpointId,
    listen: SocketAddr,
    relay_only: bool,
) -> io::Result<i32> {
    let relay: RelayUrl = SIGNAL.parse().map_err(io_error)?;
    let listener = loopback_listener(listen).await?;
    let endpoint = endpoint(key, relay.clone(), relay_only).await?;
    let signal = Signal::new()?;
    tokio::select! {
        result = async {
            signal.sign_in(endpoint.id(), runner).await?;
            let address = EndpointAddr::new(runner).with_relay_url(relay);
            maintain_gateway(&listener, &endpoint, &address, &signal).await
        } => result?,
        signal = tokio::signal::ctrl_c() => signal?,
    }
    endpoint.close().await;
    Ok(0)
}

async fn maintain_gateway(
    listener: &TcpListener,
    endpoint: &Endpoint,
    runner: &EndpointAddr,
    signal: &Signal,
) -> io::Result<()> {
    let mut connection = dial(endpoint, runner).await?;
    eprintln!("Soldr peer forwarding at http://{}", listener.local_addr()?);
    loop {
        gateway(listener, &connection).await?;
        eprintln!("Soldr runner peer disconnected. Reconnecting.");
        let reconnect = async {
            loop {
                match reconnect_peer(endpoint, runner, signal).await {
                    Ok(connection) => return Ok(connection),
                    Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                        return Err(error);
                    }
                    Err(error) => eprintln!("Soldr runner reconnect failed: {error}"),
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        };
        tokio::pin!(reconnect);
        connection = loop {
            tokio::select! {
                connection = &mut reconnect => break connection?,
                // Refuse requests while disconnected so an HTTP client never
                // waits in the local TCP backlog for a remote response.
                socket = listener.accept() => drop(socket?),
            }
        };
    }
}

async fn reconnect_peer(
    endpoint: &Endpoint,
    runner: &EndpointAddr,
    signal: &Signal,
) -> io::Result<Connection> {
    signal.sign_in(endpoint.id(), runner.id).await?;
    dial(endpoint, runner).await
}

async fn gateway(listener: &TcpListener, connection: &Connection) -> io::Result<()> {
    let mut paths = connection.paths_stream();
    let mut previous = None;
    loop {
        tokio::select! {
            _ = connection.closed() => return Ok(()),
            paths = paths.next() => {
                let Some(paths) = paths else { return Ok(()); };
                if let Some(path) = paths.iter().find(|path| path.is_selected()) {
                    let kind = if path.is_ip() { "direct" } else { "relay" };
                    if previous.replace(kind) != Some(kind) {
                        eprintln!("Soldr runner peer path: {kind}");
                    }
                }
            }
            socket = listener.accept() => {
                let (socket, _) = socket?;
                let connection = connection.clone();
                tokio::spawn(async move {
                    let result = async {
                        let (send, recv) = tokio::time::timeout(CONNECT_DEADLINE, connection.open_bi())
                            .await
                            .map_err(io_error)?
                            .map_err(io_error)?;
                        forward(socket, send, recv).await
                    };
                    if let Err(error) = result.await {
                        tracing::debug!(%error, "remote client HTTP stream ended");
                    }
                });
            }
        }
    }
}

async fn forward(socket: TcpStream, mut send: SendStream, mut recv: RecvStream) -> io::Result<()> {
    socket.set_nodelay(true)?;
    let (mut reader, mut writer) = socket.into_split();
    tokio::try_join!(
        async {
            tokio::io::copy(&mut reader, &mut send).await?;
            send.finish().map_err(io_error)
        },
        async {
            tokio::io::copy(&mut recv, &mut writer).await?;
            writer.shutdown().await
        }
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn direct_endpoint() -> Endpoint {
        Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::generate())
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn authorized_peer_reuses_connection_for_independent_streams() {
        let client = direct_endpoint().await;
        let runner = direct_endpoint().await;
        let runner_address = runner.addr();
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = backend.local_addr().unwrap();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = proxy.local_addr().unwrap();
        let authorized = client.id();
        tokio::spawn(accept_peers(runner, target, move |peer| async move {
            Ok(if peer == authorized {
                Duration::from_secs(60)
            } else {
                Duration::ZERO
            })
        }));

        let outsider = direct_endpoint().await;
        let rejected = dial(&outsider, &runner_address).await.unwrap_err();
        assert!(
            rejected.to_string().contains("peer is not authorized"),
            "{rejected}"
        );

        let connection = dial(&client, &runner_address).await.unwrap();
        let gateway_connection = connection.clone();
        tokio::spawn(async move { gateway(&proxy, &gateway_connection).await });
        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut incoming, _) = backend.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    incoming.read_to_end(&mut bytes).await.unwrap();
                    incoming.write_all(&bytes).await.unwrap();
                    incoming.shutdown().await.unwrap();
                });
            }
        });
        tokio::time::timeout(CONNECT_DEADLINE, async {
            tokio::join!(exchange(local, b"one"), exchange(local, b"two"));
        })
        .await
        .expect("independent TCP streams should finish in both directions");
        assert!(connection.close_reason().is_none());
    }

    async fn exchange(address: SocketAddr, data: &[u8]) {
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket.write_all(data).await.unwrap();
        socket.shutdown().await.unwrap();
        let mut reply = Vec::new();
        socket.read_to_end(&mut reply).await.unwrap();
        assert_eq!(reply, data);
    }
}
