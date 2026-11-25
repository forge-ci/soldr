//! HTTP transport for the agent signal upgrade. WebSocket framing stays with the agent.

use crate::core::SoldrError;
use reqwest::header::{
    CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_KEY,
    SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_VERSION, UPGRADE,
};

/// An HTTP connection after the signal server accepts the WebSocket upgrade.
pub type SignalWebSocketTransport = reqwest::Upgraded;

/// Open a direct HTTP/1.1 upgrade and validate the server's handshake before
/// exposing the stream to the agent protocol.
pub async fn open_signal_websocket(
    url: url::Url,
    key: &str,
    accept: &str,
) -> Result<SignalWebSocketTransport, SoldrError> {
    let response = reqwest::Client::builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| SoldrError::Network("could not create the signal HTTP client".into()))?
        .get(url)
        .header(CONNECTION, "Upgrade")
        .header(UPGRADE, "websocket")
        .header(SEC_WEBSOCKET_VERSION, "13")
        .header(SEC_WEBSOCKET_KEY, key)
        .send()
        .await
        .map_err(|_| SoldrError::Network("signal HTTP upgrade request failed".into()))?;

    if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        return Err(SoldrError::Network(format!(
            "signal WebSocket upgrade refused: {}",
            response.status()
        )));
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
            .is_some_and(|value| value == accept)
        || headers.contains_key(SEC_WEBSOCKET_EXTENSIONS)
        || headers.contains_key(SEC_WEBSOCKET_PROTOCOL)
    {
        return Err(SoldrError::Network(
            "invalid signal WebSocket upgrade response".into(),
        ));
    }

    response
        .upgrade()
        .await
        .map_err(|_| SoldrError::Network("signal WebSocket upgrade failed".into()))
}
