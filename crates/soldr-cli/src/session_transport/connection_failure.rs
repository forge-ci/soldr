use std::io::{self, ErrorKind};
use std::time::Duration;

use super::SessionError;

/// Deadlines in the broker negotiation, before the client sends SessionStart.
#[derive(Debug, thiserror::Error)]
pub(super) enum BrokerResponseDeadline {
    #[error(
        "broker first-response deadline exceeded after {elapsed_ms}ms (limit {limit_ms}ms; raise SOLDR_BROKER_FIRST_RESPONSE_MS if the broker is only busy)"
    )]
    FirstResponse { elapsed_ms: u128, limit_ms: u128 },
    #[error("broker did not confirm either handoff ownership or same-connection proxy fallback")]
    Handoff,
}

impl BrokerResponseDeadline {
    pub(super) fn into_io(self) -> io::Error {
        io::Error::new(ErrorKind::TimedOut, self)
    }

    pub(super) fn from_io(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref()
    }
}

/// Keep transport errors separate from compiler failures at the wrapper boundary.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct ConnectionFailure {
    pub(crate) message: String,
    pub(crate) broker_deadline: bool,
}

impl ConnectionFailure {
    pub(super) fn new(err: &SessionError, service: &str, elapsed: Duration, color: bool) -> Self {
        let broker_deadline = BrokerResponseDeadline::from_io(&err.source).is_some();
        let identity = broker_deadline.then(BrokerIdentity::probe);
        Self {
            message: connection_failure_message(err, service, elapsed, color, identity.as_ref()),
            broker_deadline,
        }
    }
}

pub(super) struct BrokerIdentity {
    pub(super) running: Option<String>,
    pub(super) local: Option<String>,
}

impl BrokerIdentity {
    fn probe() -> Self {
        // The admin probe is bounded and works from direct compiler commands,
        // too, which can already be inside the command runtime.
        let running = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let endpoint = super::session_socket_path().ok()?;
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .ok()?;
                    let (instance, _) = crate::broker_spawn::broker_snapshot_at(
                        &runtime,
                        &endpoint,
                        Duration::from_secs(1),
                    )?;
                    Some(instance)
                })
                .join()
                .ok()
                .flatten()
        });
        Self {
            running,
            local: crate::broker_server::broker_image_instance_id().ok(),
        }
    }

    fn describe(&self) -> String {
        let comparison = match (&self.running, &self.local) {
            (Some(running), Some(local)) if running == local => "matches this binary",
            (Some(_), Some(_)) => "differs from this binary",
            _ => "comparison unavailable",
        };
        let running = self
            .running
            .as_deref()
            .unwrap_or("unavailable from broker status");
        let local = self.local.as_deref().unwrap_or("unavailable");
        format!("soldr: broker instance {running} ({comparison}; this binary {local})\n")
    }
}

pub(super) fn is_connection_failure(err: &SessionError) -> bool {
    use ErrorKind::{
        BrokenPipe, ConnectionAborted, ConnectionRefused, ConnectionReset, NotFound, TimedOut,
        UnexpectedEof,
    };
    err.broker_unreachable
        || (matches!(
            err.source.kind(),
            BrokenPipe
                | ConnectionAborted
                | ConnectionRefused
                | ConnectionReset
                | NotFound
                | TimedOut
                | UnexpectedEof
        ) && !err.source.to_string().contains("reply did not complete"))
}

/// Stand-alone transport failure, red on a terminal (soldr#3815).
pub(super) fn connection_failure_message(
    err: &SessionError,
    service: &str,
    elapsed: Duration,
    color: bool,
    identity: Option<&BrokerIdentity>,
) -> String {
    let what = if identity.is_some() {
        "broker route negotiation failed"
    } else if err.broker_unreachable || err.source.kind() == ErrorKind::TimedOut {
        "could not connect to the soldr daemon"
    } else {
        "lost the connection to the soldr daemon"
    };
    let diagnostic = identity.map_or_else(String::new, BrokerIdentity::describe);
    let remove_broker = if identity.is_some() {
        "soldr:   soldr broker remove\n"
    } else {
        ""
    };
    let text = format!(
        "soldr: ERROR: control-plane failure: {what} (route {service}) after {:.1}s: {}\n\
         {diagnostic}soldr: the build was interrupted by Soldr infrastructure. Recover with:\n\
         soldr:   soldr daemon status\n\
         {remove_broker}soldr:   soldr daemon stop && soldr daemon start\n\
         soldr:   soldr logs paths   (daemon and broker logs)",
        elapsed.as_secs_f64(),
        err.source
    );
    if color {
        format!("\x1b[1;31m{text}\x1b[0m")
    } else {
        text
    }
}
