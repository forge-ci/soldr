//! Facade-owned broker v2 daemon lifecycle: publish, discover, and connect.
//!
//! The v2 broker (`running-process-broker-v2 --program <p>`) answers a Hello
//! for a registered service with the IPC endpoint the service's daemon
//! published in its identity file. This module owns both halves of that
//! contract so an application never names the private substrate:
//!
//! - **Daemon side.** [`backend_endpoint`] names an owner-private local
//!   endpoint for a service, [`BrokerLaunch::from_env`] recovers the endpoint
//!   a broker assigned when it launched the daemon, and [`publish`] writes the
//!   identity file at [`daemon_identity_path`] (removed again when the returned
//!   [`Publication`] drops).
//! - **Client side.** [`connect`] negotiates through a running v2 broker and
//!   dials the backend it names; when no broker is bound, or it has not yet
//!   resolved a backend, it dials the published identity directly
//!   ([`connect_published_within`]). Both routes return the same [`Connection`], an
//!   ordinary blocking byte stream.
//!
//! Service registration stays in [`crate::daemon_registration_v2`]; the
//! identity record itself is [`crate::daemon_identity::DaemonIdentity`]. The
//! broker wire, name derivation, and identity encoding stay in the substrate.
//! Payload protocol, daemon start policy, and retry policy remain with callers.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use running_process::broker::client_v2 as backend;
use running_process::broker::lifecycle::names::validate_service_name;
use running_process::broker::lifecycle::names_v2;
use running_process::broker::protocol::ErrorCode;
use running_process::broker::server::backend_launcher as launch;

use crate::daemon_identity::{DaemonEndpoint, DaemonIdentity};
use crate::platform::ipc;

/// Default deadline for the broker Hello round-trip in [`connect`].
pub const DEFAULT_HELLO_DEADLINE: Duration = backend::DEFAULT_HELLO_DEADLINE;

/// Namespace recorded in identities this module publishes.
pub const PUBLISHED_NAMESPACE: &str = "broker-v2";

/// Failure while publishing, discovering, or connecting to a v2 daemon.
#[derive(Debug, thiserror::Error)]
pub enum BrokerV2Error {
    /// The service name violates the frozen `[a-z0-9-]{1,64}` rule.
    #[error("invalid broker v2 service name: {detail}")]
    InvalidServiceName {
        /// Stable diagnostic supplied by the underlying validator.
        detail: String,
    },
    /// No v2 broker is bound for the program.
    #[error("no v2 broker bound: {0}")]
    BrokerUnavailable(#[source] io::Error),
    /// A bound broker has no service definition for the service yet.
    ///
    /// A daemon typically installs its own definition at startup, so this is
    /// the first-run state, not a policy decision: [`connect`] falls back to
    /// the published identity and [`BrokerV2Error::daemon_not_running`] holds.
    #[error("v2 broker does not know the service: {reason}")]
    ServiceUnknown {
        /// Broker-supplied reason.
        reason: String,
    },
    /// The broker declined the Hello.
    #[error("v2 broker refused: {reason}")]
    Refused {
        /// Broker-supplied reason.
        reason: String,
        /// Suggested back-off before retrying, in milliseconds (0 = none).
        retry_after_ms: u64,
    },
    /// Neither the broker nor an identity file names a backend endpoint.
    #[error("no daemon has published a backend endpoint")]
    NotPublished,
    /// The named backend endpoint did not accept a connection.
    #[error("backend connect failed: {0}")]
    BackendConnect(#[source] io::Error),
    /// Writing or reading the daemon identity failed.
    #[error("daemon identity: {0}")]
    Identity(String),
    /// Any other broker transport or protocol failure.
    #[error("v2 broker protocol: {0}")]
    Protocol(String),
}

impl BrokerV2Error {
    /// Whether the daemon is simply not running yet, so a caller may start it.
    #[must_use]
    pub fn daemon_not_running(&self) -> bool {
        matches!(
            self,
            Self::NotPublished | Self::BackendConnect(_) | Self::ServiceUnknown { .. }
        )
    }
}

fn hello_error(error: backend::BrokerV2Error) -> BrokerV2Error {
    match error {
        backend::BrokerV2Error::Dial { source, .. } => BrokerV2Error::BrokerUnavailable(source),
        backend::BrokerV2Error::Refused {
            reason, details, ..
        } if details.code == ErrorCode::ErrorServiceUnknown as i32 => {
            BrokerV2Error::ServiceUnknown { reason }
        }
        backend::BrokerV2Error::Refused {
            reason,
            retry_after_ms,
            ..
        } => BrokerV2Error::Refused {
            reason,
            retry_after_ms,
        },
        other => BrokerV2Error::Protocol(other.to_string()),
    }
}

fn checked(service: &str) -> Result<&str, BrokerV2Error> {
    validate_service_name(service).map_err(|error| BrokerV2Error::InvalidServiceName {
        detail: error.to_string(),
    })?;
    Ok(service)
}

/// The owner-private runtime directory holding v2 broker sockets and identities.
#[must_use]
pub fn runtime_dir() -> PathBuf {
    names_v2::broker_v2_runtime_dir()
}

/// Path of the identity file a daemon publishes for `service`; the v2 broker
/// reads exactly this file to resolve the backend endpoint.
pub fn daemon_identity_path(service: &str) -> Result<PathBuf, BrokerV2Error> {
    Ok(names_v2::daemon_identity_path(checked(service)?))
}

/// An owner-private local endpoint for `service`'s daemon to bind: a socket in
/// [`runtime_dir`] on Unix, a per-user named pipe on Windows.
///
/// Where the endpoint is a filesystem path, its owner-private parent
/// directory is created, so the name is ready to bind on a fresh host.
pub fn backend_endpoint(service: &str) -> Result<String, BrokerV2Error> {
    let bare = format!("{}-backend", checked(service)?);
    let endpoint = ipc::broker_endpoint_name(&bare, false)
        .map_err(|error| BrokerV2Error::Identity(error.to_string()))?;
    let parent = std::path::Path::new(&endpoint).parent();
    if let (true, Some(parent)) = (ipc::endpoint_is_filesystem_backed(), parent) {
        ipc::ensure_owner_private_directory(parent)
            .map_err(|error| BrokerV2Error::Identity(format!("{}: {error}", parent.display())))?;
    }
    Ok(endpoint)
}

/// What a v2 broker told the daemon it launched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokerLaunch {
    service_name: String,
    service_version: String,
    endpoint: String,
    namespace: String,
    instance: Option<String>,
}

impl BrokerLaunch {
    /// Recover the broker-assigned launch, or `None` when this process was not
    /// started by a broker (no backend endpoint in its environment).
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let read = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
        Some(Self {
            endpoint: read(launch::BACKEND_ENV_ENDPOINT_PATH)?,
            service_name: read(launch::BACKEND_ENV_SERVICE_NAME).unwrap_or_default(),
            service_version: read(launch::BACKEND_ENV_SERVICE_VERSION).unwrap_or_default(),
            namespace: read(launch::BACKEND_ENV_ENDPOINT_NAMESPACE)
                .unwrap_or_else(|| PUBLISHED_NAMESPACE.to_owned()),
            instance: read(launch::BACKEND_ENV_INSTANCE),
        })
    }

    /// Service name the broker launched this daemon for (empty if unset).
    #[must_use]
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    /// Service version the broker negotiated (empty if unset).
    #[must_use]
    pub fn service_version(&self) -> &str {
        &self.service_version
    }

    /// Endpoint the daemon must bind.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Endpoint namespace the broker assigned.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Explicit broker instance, when not the shared broker.
    #[must_use]
    pub fn instance(&self) -> Option<&str> {
        self.instance.as_deref()
    }
}

/// A published daemon identity; dropping it removes the identity file.
#[derive(Debug)]
pub struct Publication {
    path: PathBuf,
    identity: DaemonIdentity,
}

impl Publication {
    /// Identity file path.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The identity that was written.
    #[must_use]
    pub fn identity(&self) -> &DaemonIdentity {
        &self.identity
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        // Only remove the file while it still names this process: a successor
        // daemon may already have replaced it.
        if DaemonIdentity::read_sidecar(&self.path)
            .is_some_and(|current| current.pid() == self.identity.pid())
        {
            DaemonIdentity::remove_sidecar(&self.path);
        }
    }
}

/// Publish the current process as `service`'s daemon listening on `endpoint`.
///
/// Call this only after the endpoint is bound, so a reader never dials a
/// published endpoint that is not yet accepting.
pub fn publish(
    service: &str,
    endpoint: &str,
    idle_timeout_secs: Option<u32>,
) -> Result<Publication, BrokerV2Error> {
    let path = daemon_identity_path(service)?;
    let dir = runtime_dir();
    ipc::ensure_owner_private_directory(&dir)
        .map_err(|error| BrokerV2Error::Identity(format!("{}: {error}", dir.display())))?;
    let identity = DaemonIdentity::current_process(
        DaemonEndpoint::new(PUBLISHED_NAMESPACE, endpoint),
        idle_timeout_secs,
    )
    .map_err(|error| BrokerV2Error::Identity(error.to_string()))?;
    identity
        .write_sidecar(&path)
        .map_err(|error| BrokerV2Error::Identity(format!("{}: {error}", path.display())))?;
    Ok(Publication { path, identity })
}

/// The identity `service`'s daemon last published, if any.
pub fn published(service: &str) -> Result<Option<DaemonIdentity>, BrokerV2Error> {
    Ok(DaemonIdentity::read_sidecar(daemon_identity_path(service)?))
}

/// How a [`Connection`] was reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Route {
    /// A v2 broker negotiated the backend endpoint.
    Broker,
    /// The daemon's published identity named the endpoint directly.
    Published,
}

/// An open backend connection: an ordinary blocking byte stream.
#[derive(Debug)]
pub struct Connection {
    stream: ipc::Stream,
    route: Route,
    endpoint: String,
}

impl Connection {
    /// How the connection was reached.
    #[must_use]
    pub fn route(&self) -> Route {
        self.route
    }

    /// Backend endpoint that was dialed.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Bound blocking reads (None = wait indefinitely).
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_recv_timeout(timeout)
    }

    /// Bound blocking writes (None = wait indefinitely). Unsupported on
    /// Windows named pipes; see [`ipc::Stream::set_send_timeout`].
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_send_timeout(timeout)
    }

    /// The underlying facade IPC stream.
    #[must_use]
    pub fn into_stream(self) -> ipc::Stream {
        self.stream
    }
}

impl Read for Connection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buffer)
    }
}

impl Write for Connection {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

// `timeout: None` is reached only from the deprecated `connect_published`;
// drop that arm with it (#433).
#[allow(deprecated)]
fn dial(
    endpoint: &str,
    route: Route,
    timeout: Option<Duration>,
) -> Result<Connection, BrokerV2Error> {
    let address = ipc::Endpoint::new(endpoint.to_owned()).map_err(BrokerV2Error::BackendConnect)?;
    let stream = match timeout {
        // A Unix connect completes into the backlog without waiting, so a
        // zero budget would still dial; refuse it here instead.
        Some(timeout) if timeout.is_zero() => {
            return Err(BrokerV2Error::BackendConnect(io::Error::new(
                io::ErrorKind::TimedOut,
                "the connect deadline elapsed before the daemon was dialed",
            )))
        }
        Some(timeout) => ipc::Stream::connect_within(&address, timeout),
        None => ipc::Stream::connect(&address),
    }
    .map_err(BrokerV2Error::BackendConnect)?;
    Ok(Connection {
        stream,
        route,
        endpoint: endpoint.to_owned(),
    })
}

/// Ask `program`'s v2 broker which endpoint serves `service`, without dialing it.
///
/// Returns `Ok(None)` when the broker accepts the service but no daemon has
/// published yet.
pub fn negotiate(
    program: &str,
    service: &str,
    version_hint: &str,
    deadline: Duration,
) -> Result<Option<String>, BrokerV2Error> {
    let session = backend::connect_service_with_deadline(
        checked(program)?,
        checked(service)?,
        version_hint,
        deadline,
    )
    .map_err(hello_error)?;
    let pipe = session.negotiated().backend_pipe.clone();
    Ok((!pipe.is_empty()).then_some(pipe))
}

/// Dial the endpoint `service`'s daemon published, skipping any broker.
#[deprecated(note = "can wait forever on the dial; use `connect_published_within` (#433)")]
pub fn connect_published(service: &str) -> Result<Connection, BrokerV2Error> {
    let identity = published(service)?.ok_or(BrokerV2Error::NotPublished)?;
    dial(identity.endpoint().address(), Route::Published, None)
}

/// [`connect_published`], waiting at most `timeout` to dial the daemon.
///
/// An expired dial is [`BrokerV2Error::BackendConnect`] carrying
/// [`io::ErrorKind::TimedOut`].
pub fn connect_published_within(
    service: &str,
    timeout: Duration,
) -> Result<Connection, BrokerV2Error> {
    let identity = published(service)?.ok_or(BrokerV2Error::NotPublished)?;
    dial(
        identity.endpoint().address(),
        Route::Published,
        Some(timeout),
    )
}

/// Reach `service`'s daemon: through `program`'s v2 broker when one is bound,
/// otherwise through the daemon's published identity.
///
/// A broker refusal is returned as is, never bypassed: the broker's version
/// policy is authoritative when a broker answers. The one exception is a
/// broker that has no definition for the service yet
/// ([`BrokerV2Error::ServiceUnknown`]): it makes no policy claim, so the
/// published identity is tried.
///
/// `deadline` bounds the whole call: negotiation and then the dial to the
/// chosen endpoint share it. A dial that runs out of time is
/// [`BrokerV2Error::BackendConnect`] carrying [`io::ErrorKind::TimedOut`].
pub fn connect(
    program: &str,
    service: &str,
    version_hint: &str,
    deadline: Duration,
) -> Result<Connection, BrokerV2Error> {
    let started = std::time::Instant::now();
    let negotiated = negotiate(program, service, version_hint, deadline);
    let remaining = deadline.saturating_sub(started.elapsed());
    match negotiated {
        Ok(Some(endpoint)) => dial(&endpoint, Route::Broker, Some(remaining)),
        Ok(None)
        | Err(BrokerV2Error::BrokerUnavailable(_) | BrokerV2Error::ServiceUnknown { .. }) => {
            connect_published_within(service, remaining)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_names_follow_the_frozen_rule() {
        assert!(daemon_identity_path("ghd").is_ok());
        for bad in ["", "Upper", "has space", "a/b"] {
            assert!(matches!(
                daemon_identity_path(bad),
                Err(BrokerV2Error::InvalidServiceName { .. })
            ));
        }
    }

    #[test]
    fn identity_path_lives_in_the_runtime_dir() {
        let path = daemon_identity_path("ghd").expect("valid name");
        assert_eq!(path.parent(), Some(runtime_dir().as_path()));
    }

    #[test]
    fn backend_endpoint_is_service_scoped() {
        let a = backend_endpoint("svc-a").expect("valid name");
        let b = backend_endpoint("svc-b").expect("valid name");
        assert_ne!(a, b);
        // Spelling is host policy (macOS hashes the name to fit sun_path), so
        // assert only that the name is per-service and stable.
        assert_eq!(a, backend_endpoint("svc-a").expect("valid name"));
    }

    #[test]
    fn not_running_classification() {
        assert!(BrokerV2Error::NotPublished.daemon_not_running());
        assert!(BrokerV2Error::BackendConnect(io::ErrorKind::NotFound.into()).daemon_not_running());
        assert!(BrokerV2Error::ServiceUnknown {
            reason: String::new()
        }
        .daemon_not_running());
        assert!(!BrokerV2Error::Refused {
            reason: String::new(),
            retry_after_ms: 0
        }
        .daemon_not_running());
    }
}
