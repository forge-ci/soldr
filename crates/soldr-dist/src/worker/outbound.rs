//! Worker sessions: accepted from clients, or dialled to clients behind static tickets.
//! Either way one session carries every joblet the client sends.
use super::{serve_connection, State};
use crate::{transport, trust::Trust, Result};
use iroh::{Endpoint, EndpointAddr};
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;

pub(super) async fn incoming(
    state: Arc<State>,
    incoming: iroh::endpoint::Incoming,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let conn = tokio::time::timeout(Duration::from_secs(8), incoming).await??;
    if !Trust::load(&state.root.join("trusted_clients.toml"))?.contains(conn.remote_id()) {
        eprintln!("Dist rejected untrusted client {}", conn.remote_id());
        conn.close(1u32.into(), b"untrusted client");
        return Ok(());
    }
    let client = conn.remote_id();
    let result = serve_connection(state, conn, shutdown).await;
    if let Err(ref error) = result {
        eprintln!("Dist session from {client}: {error}");
    }
    result
}

pub(super) async fn dial(
    state: Arc<State>,
    endpoint: Endpoint,
    client: EndpointAddr,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        if *shutdown.borrow() {
            break;
        }
        let attempt = async {
            if !Trust::load(&state.root.join("trusted_clients.toml"))?.contains(client.id) {
                return Err(format!("untrusted configured client {}", client.id).into());
            }
            let conn = tokio::time::timeout(
                Duration::from_secs(8),
                endpoint.connect(client.clone(), transport::ALPN),
            )
            .await??;
            // Serves joblets until the client drops the session; then redial.
            serve_connection(state.clone(), conn, shutdown.clone()).await
        };
        if let Err(error) = attempt.await {
            eprintln!("Dist outbound client {}: {error}", client.id);
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
    Ok(())
}
