//! Connection liveness policy: keepalive, idle budget, reconnect pacing and the
//! log vocabulary for relay and peer link changes (soldr#4212).
//!
//! The path from a worker to a client is QUIC over either a direct UDP path or
//! the first-party relay (`Caddy -> iroh-relay`). Caddy 2.6 sets no timeout on
//! upgraded relay streams and the relay pings each client every ~15 s, so the
//! shortest idle timeout on the path is our own QUIC path idle timeout. The
//! keepalive stays well below it. The idle budget must also outlast a relay
//! restart (iroh reconnects to its home relay on its own, starting at 10 ms)
//! and a starved `CPUWeight=1` worker on a busy Dell: the previous 5 s budget
//! cut joblets on both.
use iroh::endpoint::{ConnectError, ConnectionError};
use std::{error::Error as StdError, time::Duration};

/// QUIC keepalive on every path: below every idle timeout on the path.
pub const KEEP_ALIVE: Duration = Duration::from_secs(2);
/// A silent path is abandoned after this; another path may still carry the connection.
pub const PATH_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// A connection survives this long with no live path, e.g. across a relay restart.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Fresh-connection retries for one joblet before it is run locally.
pub const JOBLET_RETRIES: u32 = 2;

/// True when `error` (or anything in its source chain) is transport loss: the
/// peer or path went away, as opposed to a protocol or local error. A joblet
/// that fails this way is re-leasable: outputs are published only after every
/// output is verified, so re-running it on a fresh connection is safe.
pub fn is_connection_loss(error: &(dyn StdError + 'static)) -> bool {
    chain(error).any(|e| {
        if let Some(conn) = e.downcast_ref::<ConnectionError>() {
            return match conn {
                ConnectionError::TimedOut
                | ConnectionError::Reset
                | ConnectionError::ConnectionClosed(_) => true,
                // A clean worker shutdown hands the joblet back; a joblet error does not.
                ConnectionError::ApplicationClosed(close) => {
                    close.error_code == 0u32.into() || close.reason.as_ref() == b"worker shutdown"
                }
                _ => false,
            };
        }
        e.downcast_ref::<ConnectError>().is_some()
            || e.downcast_ref::<tokio::time::error::Elapsed>().is_some()
    })
}

/// `outer: inner: root` for logs; QUIC stream errors otherwise print only
/// "connection lost" and hide whether the peer closed, reset or timed out.
pub fn describe(error: &(dyn StdError + 'static)) -> String {
    let mut parts: Vec<String> = Vec::new();
    for e in chain(error) {
        let text = e.to_string();
        if parts.last().is_none_or(|last| !last.contains(&text)) {
            parts.push(text);
        }
    }
    parts.join(": ")
}

fn chain<'a>(
    error: &'a (dyn StdError + 'static),
) -> impl Iterator<Item = &'a (dyn StdError + 'static)> {
    std::iter::successors(Some(error), |&e| e.source())
}

/// Jittered exponential backoff. The first retry waits 100 ms to 1 s; later
/// ones double up to 30 s, each drawn from the upper half of its window so
/// that many dialers (one per slot) never reconnect in lockstep.
pub fn reconnect_delay(attempt: u32) -> Duration {
    let jitter = random_unit();
    if attempt == 0 {
        return Duration::from_millis(100) + Duration::from_millis(900).mul_f64(jitter);
    }
    let ceiling = Duration::from_secs(1u64 << attempt.min(5)).min(Duration::from_secs(30));
    ceiling.mul_f64(0.5 + 0.5 * jitter)
}

fn random_unit() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    // RandomState is seeded per process and per instance; plenty for jitter.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// Log every home-relay connect and disconnect, so a relay restart shows as
/// one disconnect line and one reconnect line with the time it took.
pub fn watch_relay(endpoint: &iroh::Endpoint, role: &'static str) -> tokio::task::JoinHandle<()> {
    use iroh::Watcher;
    let mut watcher = endpoint.home_relay_status();
    let closed = endpoint.clone();
    tokio::spawn(async move {
        let mut down_since: Option<std::time::Instant> = None;
        let mut connected = false;
        loop {
            let status = watcher.get();
            let now_connected = status.iter().any(|s| s.is_connected());
            if now_connected && !connected {
                match down_since.take() {
                    Some(since) => eprintln!(
                        "Dist {role} relay reconnected after {:.1}s: {}",
                        since.elapsed().as_secs_f64(),
                        urls(&status)
                    ),
                    None => eprintln!("Dist {role} relay connected: {}", urls(&status)),
                }
            } else if !now_connected && connected {
                down_since = Some(std::time::Instant::now());
                let cause = status
                    .iter()
                    .find_map(|s| s.last_error().map(|e| format!("{e:#}")))
                    .unwrap_or_else(|| "no error reported".into());
                eprintln!("Dist {role} relay disconnected ({cause}); reconnecting");
            }
            connected = now_connected;
            tokio::select! {
                result = watcher.updated() => if result.is_err() { break },
                _ = closed.closed() => break,
            }
        }
    })
}

fn urls(status: &[iroh::endpoint::RelayStatus]) -> String {
    status
        .iter()
        .map(|s| s.url().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_retry_is_fast_and_later_ones_are_capped() {
        for _ in 0..256 {
            let first = reconnect_delay(0);
            assert!(first >= Duration::from_millis(100) && first <= Duration::from_secs(1));
            let late = reconnect_delay(20);
            assert!(late >= Duration::from_secs(15) && late <= Duration::from_secs(30));
        }
    }

    #[test]
    fn retries_are_jittered() {
        let delays: std::collections::HashSet<_> = (0..64).map(|_| reconnect_delay(1)).collect();
        assert!(
            delays.len() > 1,
            "64 dialers must not reconnect in lockstep"
        );
    }

    #[test]
    fn keepalive_is_below_every_idle_timeout() {
        assert!(KEEP_ALIVE * 3 <= PATH_IDLE_TIMEOUT);
        assert!(PATH_IDLE_TIMEOUT < IDLE_TIMEOUT);
    }

    #[test]
    fn transport_loss_is_classified_through_the_source_chain() {
        use iroh::endpoint::{ReadError, ReadExactError};
        let lost: crate::Error = Box::new(ReadExactError::ReadError(ReadError::ConnectionLost(
            ConnectionError::TimedOut,
        )));
        assert!(is_connection_loss(lost.as_ref()));
        assert_eq!(describe(lost.as_ref()), "connection lost: timed out");
        let reset: crate::Error = Box::new(ReadError::ConnectionLost(ConnectionError::Reset));
        assert!(is_connection_loss(reset.as_ref()));
        let local: crate::Error =
            Box::new(ReadError::ConnectionLost(ConnectionError::LocallyClosed));
        assert!(!is_connection_loss(local.as_ref()));
        let protocol: crate::Error = "undeclared output requested".into();
        assert!(!is_connection_loss(protocol.as_ref()));
        assert_eq!(describe(protocol.as_ref()), "undeclared output requested");
    }
}
