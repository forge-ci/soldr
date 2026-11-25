//! Outbound WebSocket lifecycle; all transport waits have explicit deadlines.
use crate::{
    backoff::Backoff, checks, config::Config, host_identity::HostKey, proto::*, status::Status,
    Result,
};
use futures_util::{SinkExt, StreamExt};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
};
use tokio_tungstenite::{
    tungstenite::{
        handshake::{client::generate_key, derive_accept_key},
        protocol::{Role, WebSocketConfig},
        Message,
    },
    WebSocketStream,
};
type Socket = WebSocketStream<reqwest::Upgraded>;
const IO_LIMIT: Duration = Duration::from_secs(2);

pub async fn run(config: Config, key: HostKey, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    let url = config.connect_url()?.to_string();
    std::fs::create_dir_all(&config.state_dir)?;
    let listener = TcpListener::bind(config.health_bind).await?;
    let config = Arc::new(config);
    let health = health_server(listener, config.clone());
    let phone_home = async {
        let mut backoff = Backoff::default();
        loop {
            Status::write(
                &config.state_dir,
                "connecting",
                &url,
                "opening outbound WebSocket",
            )?;
            let result = session_with_status(&config, &key, &url, &mut backoff).await;
            // Errors contain no received message bodies, signatures or close reasons.
            let detail = result
                .err()
                .map_or_else(|| "signal connection closed".into(), |e| e.to_string());
            let mut random = [0; 4];
            getrandom::fill(&mut random).map_err(|_| "retry randomness unavailable")?;
            let delay = backoff.next_delay(u32::from_le_bytes(random));
            eprintln!(
                "soldr agent: warning: {url}: {detail}; retrying in {:.2}s",
                delay.as_secs_f64()
            );
            Status::write(&config.state_dir, "disconnected", &url, &detail)?;
            tokio::time::sleep(delay).await;
        }
        #[allow(unreachable_code)]
        Ok::<(), crate::Error>(())
    };
    tokio::select! {
        result = health => result?,
        result = phone_home => result?,
        _ = async { if !*shutdown.borrow() { let _ = shutdown.changed().await; } } => {}
    }
    Status::write(&config.state_dir, "stopped", &url, "agent stopped")?;
    Ok(())
}
async fn session_with_status(
    config: &Config,
    key: &HostKey,
    url: &str,
    backoff: &mut Backoff,
) -> Result<()> {
    let mut socket = connect(config, key, url).await?;
    Status::write(
        &config.state_dir,
        "connected",
        url,
        "hello sent; signal protocol has no authentication acknowledgement",
    )?;
    let started = tokio::time::Instant::now();
    let mut interval = tokio::time::interval_at(
        started + Duration::from_secs(HEARTBEAT_S),
        Duration::from_secs(HEARTBEAT_S),
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let (load, dist, _) = checks::capacity(config).await;
                send(&mut socket, &FromHost::Heartbeat { load, dist }).await?;
                if started.elapsed() >= Duration::from_secs(TTL_MS as u64 / 1000) { backoff.reset(); }
                Status::write(&config.state_dir, "connected", url, "heartbeat sent")?;
            }
            message = receive(&mut socket) => {
                match message? {
                    ToHost::Ping { id } => send(&mut socket, &FromHost::Pong { id, checks: checks::collect(config).await }).await?,
                    ToHost::Challenge { .. } => return Err("unexpected repeated challenge".into()),
                }
            }
        }
    }
}
async fn connect(config: &Config, key: &HostKey, url: &str) -> Result<Socket> {
    let handshake = async {
        let mut socket = open_socket(url).await?;
        let ToHost::Challenge { nonce } = receive(&mut socket).await? else {
            return Err("expected signal challenge".into());
        };
        if decode(&nonce).map_or(true, |n| n.len() != 32) {
            return Err("invalid challenge nonce length".into());
        }
        let hello = hello(config, key, &nonce).await?;
        send(&mut socket, &FromHost::Hello(hello)).await?;
        Ok(socket)
    };
    tokio::time::timeout(Duration::from_secs(5), handshake)
        .await
        .map_err(|_| "signal handshake timed out after 5s")?
}
fn connection_error(error: tokio_tungstenite::tungstenite::Error) -> crate::Error {
    use tokio_tungstenite::tungstenite::Error;
    match error {
        Error::Io(e) => format!("signal transport I/O failed: {e}").into(),
        Error::Tls(_) => "signal TLS handshake failed".into(),
        Error::Http(response) => {
            format!("signal WebSocket upgrade refused: {}", response.status()).into()
        }
        _ => "signal WebSocket transport failed or closed".into(),
    }
}
async fn hello(config: &Config, key: &HostKey, nonce: &str) -> Result<Hello> {
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned();
    if boot_id.is_empty() {
        return Err("Linux boot ID unavailable".into());
    }
    let uptime_s = std::fs::read_to_string("/proc/uptime")?
        .split_whitespace()
        .next()
        .and_then(|s| s.split('.').next())
        .and_then(|s| s.parse().ok())
        .ok_or("Linux uptime unavailable")?;
    let mut ips: Vec<_> = if_addrs::get_if_addrs()?
        .into_iter()
        .map(|i| i.ip())
        .filter(|ip| !ip.is_loopback())
        .collect();
    ips.sort();
    ips.dedup();
    ips.truncate(32);
    let (load, dist, _) = checks::capacity(config).await;
    Ok(Hello {
        name: config.name.clone(),
        sig: key.sign_challenge(nonce, &config.name),
        boot_id,
        role: config.role,
        ips,
        uptime_s,
        versions: Versions {
            soldr: env!("CARGO_PKG_VERSION").into(),
            agent: env!("CARGO_PKG_VERSION").into(),
            os: std::fs::read_to_string("/proc/sys/kernel/osrelease")?
                .trim()
                .into(),
        },
        load,
        dist,
    })
}
async fn send(socket: &mut Socket, message: &FromHost) -> Result<()> {
    let text = serde_json::to_string(message)?;
    if text.len() > MAX_MESSAGE {
        return Err("agent message exceeds protocol limit".into());
    }
    tokio::time::timeout(IO_LIMIT, socket.send(Message::Text(text.into())))
        .await
        .map_err(|_| "signal send timed out")?
        .map_err(connection_error)
}
async fn receive(socket: &mut Socket) -> Result<ToHost> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) if text.len() <= MAX_MESSAGE => {
                return serde_json::from_str(&text).map_err(|_| "invalid signal message".into())
            }
            Some(Ok(Message::Ping(_))) => tokio::time::timeout(IO_LIMIT, socket.flush())
                .await
                .map_err(|_| "WebSocket ping flush timed out")?
                .map_err(connection_error)?,
            Some(Ok(Message::Pong(_))) => {}
            _ => return Err("signal socket closed or sent an invalid frame".into()),
        }
    }
}
pub async fn health_server(listener: TcpListener, config: Arc<Config>) -> Result<()> {
    let permits = Arc::new(tokio::sync::Semaphore::new(8));
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let config = config.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = health_request(stream, &config).await { eprintln!("soldr agent: health request failed: {error}"); }
                });
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(error) = result { return Err(error.into()); }
            }
        }
    }
}
async fn health_request(mut stream: TcpStream, config: &Config) -> Result<()> {
    tokio::time::timeout(IO_LIMIT, async {
        let mut request = Vec::new();
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let mut buf = [0; 512];
            let n = stream.read(&mut buf).await?;
            if n == 0 || request.len() + n > 4096 { return Err("invalid health request".into()); }
            request.extend_from_slice(&buf[..n]);
        }
        let (code, body) = if request.starts_with(b"GET /healthz HTTP/1.") {
            let checks = checks::collect(config).await;
            let code = if checks.iter().all(|c| c.ok) { "200 OK" } else { "503 Service Unavailable" };
            (code, serde_json::to_string(&Health { state: HostState::Online, offline_since: None, checks })?)
        } else { ("404 Not Found", "{}".into()) };
        let response = format!("HTTP/1.1 {code}\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        stream.write_all(response.as_bytes()).await?;
        Ok::<(), crate::Error>(())
    }).await.map_err(|_| "health request timed out")?
}

/// Let reqwest own TLS and HTTP; tungstenite handles framing after the upgrade.
async fn open_socket(url: &str) -> Result<Socket> {
    use reqwest::header::{
        CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_KEY,
        SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_VERSION, UPGRADE,
    };

    let mut url = url::Url::parse(url).map_err(|_| "invalid signal URL")?;
    let scheme = match url.scheme() {
        "wss" => "https",
        "ws" => "http",
        _ => return Err("invalid signal transport scheme".into()),
    };
    url.set_scheme(scheme)
        .map_err(|()| "invalid signal URL scheme")?;

    let key = generate_key();
    let accept = derive_accept_key(key.as_bytes());
    let response = reqwest::Client::builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()?
        .get(url)
        .header(CONNECTION, "Upgrade")
        .header(UPGRADE, "websocket")
        .header(SEC_WEBSOCKET_VERSION, "13")
        .header(SEC_WEBSOCKET_KEY, key)
        .send()
        .await
        .map_err(|_| "signal HTTP upgrade request failed")?;

    if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        return Err(format!("signal WebSocket upgrade refused: {}", response.status()).into());
    }
    let headers = response.headers();
    let has_connection_upgrade = headers.get_all(CONNECTION).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
    });
    if !has_connection_upgrade
        || !headers
            .get(UPGRADE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        || !headers
            .get(SEC_WEBSOCKET_ACCEPT)
            .is_some_and(|value| value == &accept)
        || headers.contains_key(SEC_WEBSOCKET_EXTENSIONS)
        || headers.contains_key(SEC_WEBSOCKET_PROTOCOL)
    {
        return Err("invalid signal WebSocket upgrade response".into());
    }

    let stream = response
        .upgrade()
        .await
        .map_err(|_| "signal WebSocket upgrade failed")?;
    let limits = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    Ok(WebSocketStream::from_raw_socket(stream, Role::Client, Some(limits)).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_an_upgrade_with_the_wrong_accept_key() {
        use tokio_tungstenite::tungstenite::{
            handshake::server::{Request, Response},
            http::{header::SEC_WEBSOCKET_ACCEPT, HeaderValue},
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_hdr_async(stream, |_: &Request, mut response: Response| {
                response.headers_mut().insert(
                    SEC_WEBSOCKET_ACCEPT,
                    HeaderValue::from_static("invalid-accept-key"),
                );
                Ok(response)
            })
            .await
            .unwrap();
        });

        let error = tokio::time::timeout(IO_LIMIT, open_socket(&url))
            .await
            .expect("client upgrade deadline")
            .expect_err("invalid accept key must be rejected");
        assert_eq!(
            error.to_string(),
            "invalid signal WebSocket upgrade response"
        );
        server.await.unwrap();
    }
}
