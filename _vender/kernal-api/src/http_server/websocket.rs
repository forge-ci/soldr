//! Owned RFC 6455 upgrade and message transport for [`super::Server`].
//!
//! Route, origin, authentication and application protocol policy remain with
//! callers. This module owns only HTTP upgrade validation, bounded frames and
//! the private transport implementation.

use super::{Request, Response};
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use hyper::header;
use hyper_util::rt::TokioIo;
use std::{future::Future, io};
use tokio_tungstenite::{tungstenite, WebSocketStream};

/// Per-connection WebSocket acceptance limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSocketLimits {
    /// Maximum decoded message payload bytes.
    pub max_message_bytes: usize,
    /// Maximum single-frame payload bytes.
    pub max_frame_bytes: usize,
}

impl Default for WebSocketLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: 64 * 1024,
            max_frame_bytes: 64 * 1024,
        }
    }
}

impl WebSocketLimits {
    fn config(self) -> io::Result<tungstenite::protocol::WebSocketConfig> {
        if !(1..=64 * 1024 * 1024).contains(&self.max_message_bytes)
            || !(1..=self.max_message_bytes).contains(&self.max_frame_bytes)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid WebSocket message/frame limits",
            ));
        }
        Ok(tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(self.max_message_bytes))
            .max_frame_size(Some(self.max_frame_bytes)))
    }
}

/// Facade-owned WebSocket message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

fn from_transport(message: tungstenite::Message) -> Message {
    match message {
        tungstenite::Message::Text(text) => Message::Text(text.to_string()),
        tungstenite::Message::Binary(data) => Message::Binary(data.to_vec()),
        tungstenite::Message::Ping(data) => Message::Ping(data.to_vec()),
        tungstenite::Message::Pong(data) => Message::Pong(data.to_vec()),
        tungstenite::Message::Close(_) => Message::Close,
        tungstenite::Message::Frame(_) => Message::Close,
    }
}

fn into_transport(message: Message) -> tungstenite::Message {
    match message {
        Message::Text(text) => tungstenite::Message::Text(text.into()),
        Message::Binary(data) => tungstenite::Message::Binary(data.into()),
        Message::Ping(data) => tungstenite::Message::Ping(data.into()),
        Message::Pong(data) => tungstenite::Message::Pong(data.into()),
        Message::Close => tungstenite::Message::Close(None),
    }
}

/// An upgraded connection with bounded facade messages.
pub struct WebSocket {
    inner: WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>,
    limits: WebSocketLimits,
}

/// Write half of an upgraded connection. It retains the connection's bounded
/// outbound-message policy after [`WebSocket::split`].
pub struct WebSocketSender {
    inner: SplitSink<WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>, tungstenite::Message>,
    limits: WebSocketLimits,
}

/// Read half of an upgraded connection, returned by [`WebSocket::split`].
pub struct WebSocketReceiver {
    inner: SplitStream<WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>>,
}

impl WebSocket {
    /// Receive one message. `Ok(None)` means the peer closed cleanly.
    pub async fn receive(&mut self) -> io::Result<Option<Message>> {
        match self.inner.next().await {
            Some(Ok(message)) => Ok(Some(from_transport(message))),
            Some(Err(error)) => Err(io::Error::other(error)),
            None => Ok(None),
        }
    }

    /// Send one message, flushing it to the connection.
    pub async fn send(&mut self, message: Message) -> io::Result<()> {
        validate_outgoing(self.limits, &message)?;
        self.inner
            .send(into_transport(message))
            .await
            .map_err(io::Error::other)
    }

    /// Split this connection into independently owned read and write halves.
    /// Both halves are cancelled when their owning tasks are dropped.
    pub fn split(self) -> (WebSocketSender, WebSocketReceiver) {
        let (inner, receiver) = self.inner.split();
        (
            WebSocketSender {
                inner,
                limits: self.limits,
            },
            WebSocketReceiver { inner: receiver },
        )
    }
}

impl WebSocketSender {
    /// Send one bounded message and flush it to the peer.
    pub async fn send(&mut self, message: Message) -> io::Result<()> {
        validate_outgoing(self.limits, &message)?;
        self.inner
            .send(into_transport(message))
            .await
            .map_err(io::Error::other)
    }
}

impl WebSocketReceiver {
    /// Receive one message. `Ok(None)` means the peer closed cleanly.
    pub async fn receive(&mut self) -> io::Result<Option<Message>> {
        match self.inner.next().await {
            Some(Ok(message)) => Ok(Some(from_transport(message))),
            Some(Err(error)) => Err(io::Error::other(error)),
            None => Ok(None),
        }
    }
}

fn validate_outgoing(limits: WebSocketLimits, message: &Message) -> io::Result<()> {
    let bytes = match message {
        Message::Text(text) => text.len(),
        Message::Binary(data) | Message::Ping(data) | Message::Pong(data) => data.len(),
        Message::Close => 0,
    };
    if bytes > limits.max_message_bytes
        || bytes > limits.max_frame_bytes
        || (matches!(message, Message::Ping(_) | Message::Pong(_)) && bytes > 125)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WebSocket outgoing message exceeds configured limits",
        ));
    }
    Ok(())
}

/// A validated, one-shot WebSocket upgrade.
pub struct Upgrade {
    upgrade: hyper::upgrade::OnUpgrade,
    accept: String,
}

impl Upgrade {
    pub(super) fn from_request(request: Request) -> io::Result<Self> {
        if !request.http_1_1
            || request.method != "GET"
            || !has_token(&request.headers, header::CONNECTION, "upgrade")
            || !has_token(&request.headers, header::UPGRADE, "websocket")
            || singleton(&request.headers, "sec-websocket-version") != Some(b"13".as_slice())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "request is not a WebSocket upgrade",
            ));
        }
        let key = singleton(&request.headers, "sec-websocket-key")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing WebSocket key"))?;
        if !valid_key(key) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid WebSocket key",
            ));
        }
        let upgrade = request
            .upgrade
            .ok_or_else(|| io::Error::other("WebSocket upgrade unavailable"))?;
        Ok(Self {
            upgrade,
            accept: tungstenite::handshake::derive_accept_key(key),
        })
    }

    /// Return the switching-protocol response and run `callback` after upgrade.
    /// Callback errors are isolated to its connection and do not stop serving.
    pub fn on_upgrade<F, Fut>(self, limits: WebSocketLimits, callback: F) -> io::Result<Response>
    where
        F: FnOnce(WebSocket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let config = limits.config()?;
        let accept = self.accept;
        let task = Box::pin(async move {
            if let Ok(upgraded) = self.upgrade.await {
                let socket = WebSocket {
                    inner: WebSocketStream::from_raw_socket(
                        TokioIo::new(upgraded),
                        tungstenite::protocol::Role::Server,
                        Some(config),
                    )
                    .await,
                    limits,
                };
                callback(socket).await;
            }
        });
        Ok(Response::websocket_upgrade(&accept, task))
    }
}

fn has_token(headers: &hyper::HeaderMap, name: hyper::header::HeaderName, token: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        std::str::from_utf8(value.as_bytes()).is_ok_and(|value| {
            value
                .split(',')
                .any(|candidate| candidate.trim().eq_ignore_ascii_case(token))
        })
    })
}

fn singleton<'a>(headers: &'a hyper::HeaderMap, name: &str) -> Option<&'a [u8]> {
    let values = headers.get_all(name);
    let mut values = values.iter();
    let value = values.next()?;
    if values.next().is_some() {
        None
    } else {
        Some(value.as_bytes())
    }
}

/// RFC 6455's nonce is exactly sixteen bytes encoded as standard Base64.
/// Its canonical encoding is consequently twenty-four ASCII bytes ending in
/// two padding characters. The handshake digest consumes the original text.
fn valid_key(key: &[u8]) -> bool {
    key.len() == 24
        && key.ends_with(b"==")
        && key[..22]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_bounded_and_frame_cannot_exceed_message() {
        assert!(WebSocketLimits::default().config().is_ok());
        assert!(WebSocketLimits {
            max_message_bytes: 0,
            max_frame_bytes: 1,
        }
        .config()
        .is_err());
        assert!(WebSocketLimits {
            max_message_bytes: 10,
            max_frame_bytes: 11,
        }
        .config()
        .is_err());
    }

    #[test]
    fn upgrade_rejects_non_websocket_before_claiming_transport() {
        let request = Request {
            method: "GET".into(),
            http_1_1: true,
            target: "/terminal/ws".into(),
            uri: "/terminal/ws".parse().unwrap(),
            headers: hyper::HeaderMap::new(),
            body: Vec::new(),
            upgrade: None,
        };
        assert!(Upgrade::from_request(request).is_err());
    }

    #[test]
    fn websocket_key_must_be_one_canonical_16_byte_nonce() {
        assert!(valid_key(b"dGhlIHNhbXBsZSBub25jZQ=="));
        assert!(!valid_key(b""));
        assert!(!valid_key(b"dGhlIHNhbXBsZSBub25jZQ="));
        assert!(!valid_key(b"!!!!!!!!!!!!!!!!!!!!!!=="));
    }

    #[test]
    fn facade_messages_do_not_expose_transport_values() {
        let message = Message::Binary(vec![1, 2, 3]);
        assert_eq!(from_transport(into_transport(message.clone())), message);
    }

    #[tokio::test]
    async fn server_emits_a_real_switching_protocols_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let server = super::super::Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            super::super::Limits::default(),
        )
        .await
        .unwrap();
        let address = server.local_addr().unwrap();
        let serving = tokio::spawn(server.serve(|request| async move {
            match request.into_websocket() {
                Ok(upgrade) => upgrade
                    .on_upgrade(WebSocketLimits::default(), |_socket| async {})
                    .unwrap(),
                Err(_) => Response::new(400, Vec::new()).unwrap(),
            }
        }));
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(
                b"GET /ws HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = [0; 1024];
        let count = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client.read(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        let response = std::str::from_utf8(&response[..count]).unwrap();
        assert!(response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(response.contains("connection: Upgrade\r\n"));
        assert!(response.contains("upgrade: websocket\r\n"));
        assert!(response.contains("sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
        serving.abort();
    }

    /// A callback that does observable work must actually run.
    ///
    /// The switching-protocols response alone is not evidence of a live
    /// connection: with `with_upgrades()`, hyper's connection future resolves
    /// as soon as the `101` is written. An upgrade task that the server tears
    /// down at that point still produces a successful handshake, so a client
    /// sees an open socket followed by a close carrying no frames. This test
    /// asserts the frame, which a do-nothing callback cannot.
    #[tokio::test]
    async fn upgrade_callback_delivers_a_frame_to_the_client() {
        use futures_util::StreamExt;

        let server = super::super::Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            super::super::Limits::default(),
        )
        .await
        .unwrap();
        let address = server.local_addr().unwrap();
        let serving = tokio::spawn(server.serve(|request| async move {
            match request.into_websocket() {
                Ok(upgrade) => upgrade
                    .on_upgrade(WebSocketLimits::default(), |mut socket| async move {
                        let _ = socket.send(Message::Text("upgrade-alive".into())).await;
                        // Stay alive so the client observes the frame instead of
                        // racing an immediate close.
                        let _ = socket.receive().await;
                    })
                    .unwrap(),
                Err(_) => Response::new(400, Vec::new()).unwrap(),
            }
        }));

        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (mut client, _) = tokio_tungstenite::client_async("ws://localhost/ws", stream)
            .await
            .unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), client.next())
            .await
            .expect("upgrade callback never delivered a frame")
            .expect("connection closed before any frame arrived")
            .expect("frame error");
        match frame {
            tokio_tungstenite::tungstenite::Message::Text(text) => {
                assert_eq!(text.as_str(), "upgrade-alive");
            }
            other => panic!("expected a text frame, got {other:?}"),
        }
        serving.abort();
    }

    /// `connection_timeout` bounds the handshake, not the upgraded connection.
    ///
    /// Wrapping the upgrade task back inside the connection budget would kill
    /// every long-lived session at the budget, which for an interactive
    /// terminal means an arbitrary mid-session disconnect.
    #[tokio::test]
    async fn upgraded_connection_outlives_the_connection_timeout() {
        use futures_util::StreamExt;

        let limits = super::super::Limits {
            connection_timeout: std::time::Duration::from_millis(50),
            ..super::super::Limits::default()
        };
        let server = super::super::Server::bind("127.0.0.1:0".parse().unwrap(), limits)
            .await
            .unwrap();
        let address = server.local_addr().unwrap();
        let serving = tokio::spawn(server.serve(|request| async move {
            match request.into_websocket() {
                Ok(upgrade) => upgrade
                    .on_upgrade(WebSocketLimits::default(), |mut socket| async move {
                        // Far beyond the connection budget that already elapsed.
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        let _ = socket.send(Message::Text("still-here".into())).await;
                        let _ = socket.receive().await;
                    })
                    .unwrap(),
                Err(_) => Response::new(400, Vec::new()).unwrap(),
            }
        }));

        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (mut client, _) = tokio_tungstenite::client_async("ws://localhost/ws", stream)
            .await
            .unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), client.next())
            .await
            .expect("upgraded connection was killed by the connection timeout")
            .expect("connection closed before the frame arrived")
            .expect("frame error");
        match frame {
            tokio_tungstenite::tungstenite::Message::Text(text) => {
                assert_eq!(text.as_str(), "still-here");
            }
            other => panic!("expected a text frame, got {other:?}"),
        }
        serving.abort();
    }
}
