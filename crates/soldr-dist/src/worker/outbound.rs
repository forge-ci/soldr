//! Worker sessions: accepted from clients, or dialled to clients behind static tickets.
//! Either way one session carries every joblet the client sends.
use super::{serve_connection, State};
use crate::{link, transport, trust::Trust, Result};
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
        eprintln!(
            "Dist session from {client}: {}",
            link::describe(error.as_ref())
        );
    }
    result
}

pub(super) async fn dial(
    state: Arc<State>,
    endpoint: Endpoint,
    client: EndpointAddr,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut failures = 0u32;
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
        // Jittered backoff (soldr#4212): the first redial waits at most 1 s, and
        // workers that lost the same relay never reconnect in lockstep.
        let delay = match attempt.await {
            Ok(()) => {
                failures = 0;
                link::reconnect_delay(0).min(Duration::from_millis(250))
            }
            Err(error) => {
                let delay = link::reconnect_delay(failures);
                failures = failures.saturating_add(1);
                let cause = link::describe(error.as_ref());
                if link::is_connection_loss(error.as_ref()) {
                    eprintln!(
                        "Dist outbound client {} link lost ({cause}); redialing in {:.1}s",
                        client.id,
                        delay.as_secs_f64()
                    );
                } else {
                    eprintln!("Dist outbound client {}: {cause}", client.id);
                }
                delay
            }
        };
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(delay) => {},
        }
    }
    Ok(())
}
