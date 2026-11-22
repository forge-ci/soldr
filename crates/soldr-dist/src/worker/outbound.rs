//! Worker-originated warm connections for clients behind static tickets.
use super::{handle, handle_job, write_status, State};
use crate::{transport, trust::Trust, Result};
use iroh::{Endpoint, EndpointAddr};
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;

pub(super) async fn incoming(state: Arc<State>, incoming: iroh::endpoint::Incoming) -> Result<()> {
    let conn = tokio::time::timeout(Duration::from_secs(8), incoming).await??;
    if !Trust::load(&state.root.join("trusted_clients.toml"))?.contains(conn.remote_id()) {
        eprintln!("Dist rejected untrusted client {}", conn.remote_id());
        conn.close(1u32.into(), b"untrusted client");
        return Ok(());
    }
    let result = handle(&state, &conn).await;
    if let Err(ref error) = result {
        eprintln!("Dist job from {}: {error}", conn.remote_id());
    }
    if result.is_ok() {
        let _ = tokio::time::timeout(Duration::from_secs(8), conn.closed()).await;
    }
    match &result {
        Ok(()) => conn.close(0u32.into(), b"job complete"),
        Err(error) => {
            let reason = error.to_string();
            conn.close(1u32.into(), &reason.as_bytes()[..reason.len().min(200)]);
        }
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
            let streams = tokio::select! {
                result = conn.accept_bi() => result?,
                _ = shutdown.changed() => { conn.close(0u32.into(), b"worker shutdown"); return Ok(()); },
            };
            // Once a joblet starts, the parent serve deadline drains it rather than
            // cancelling it immediately on SIGTERM. Trust is rechecked per joblet.
            if !Trust::load(&state.root.join("trusted_clients.toml"))?.contains(conn.remote_id()) {
                conn.close(1u32.into(), b"client trust revoked");
                return Err("client trust revoked".into());
            }
            let result = handle_job(&state, &conn, streams).await;
            if result.is_ok() {
                let _ = tokio::time::timeout(Duration::from_secs(8), conn.closed()).await;
            }
            conn.close(0u32.into(), b"joblet complete");
            write_status(&state)?;
            state.cas.evict()?;
            result
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
