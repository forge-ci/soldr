//! Facade-owned broker client: backend connect and refusal classification.
//!
//! The broker implementation stays in the private `running-process` backend.
//! This module is the adapter an application uses instead of naming it: an
//! owned request, an owned connection that is an ordinary `std::io` stream,
//! and owned route, refusal-code, refusal-kind, and error values. Backend
//! errors convert privately, so an application branches on *why* a broker
//! declined without compiling against the backend's types.
//!
//! [`connect_backend`](crate::broker_client::connect_backend) is blocking.
//! Async callers run it on a blocking worker. Endpoint naming, the product payload protocol spoken over the returned
//! connection, and every fallback decision remain application policy.

use std::io::{self, Read, Write};

use running_process::broker::client as backend;

/// Canonical escape-hatch variable. When it is `1`, [`connect_backend`]
/// returns [`BrokerClientError::Disabled`] and the caller uses its direct path.
pub const DISABLE_ENV: &str = backend::RUNNING_PROCESS_DISABLE_ENV;

/// Value of [`DISABLE_ENV`] that disables the broker.
pub const DISABLE_VALUE: &str = backend::RUNNING_PROCESS_DISABLE_VALUE;

/// Upstream TEST-ONLY seam naming a fake backend endpoint.
///
/// Applications that keep a fake-backend test lane read this name. This
/// facade does not enable the substrate's test seam, so [`connect_backend`]
/// honors it only if another crate in the graph selects that seam. Never set
/// it in production.
pub const FAKE_BACKEND_ENV: &str = backend::RUNNING_PROCESS_FAKE_BACKEND_ENV;

/// How a backend connection was reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackendRoute {
    /// Connected directly to a known backend endpoint, skipping Hello. This is
    /// the cached-endpoint fast path; applications also report it for their
    /// own fake-backend test seams.
    HelloSkip,
    /// Asked the broker via Hello, then connected to the negotiated endpoint.
    BrokerNegotiated,
    /// Adopted the broker connection itself after a confirmed handoff.
    HandlePassed,
}

/// Stable broker refusal code carried on the frozen v1/v2 wire as an `i32`.
///
/// Conversion from `i32` is total: a code this build predates is preserved as
/// [`RefusalCode::Unrecognized`] rather than rejected, so a newer broker can
/// never make classification fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefusalCode {
    /// No code was set (wire value 0).
    Unspecified,
    /// The requested version is below the backend's minimum or not offered.
    VersionUnsupported,
    /// The service name is unknown to this broker.
    ServiceUnknown,
    /// The broker failed to spawn the backend.
    BackendSpawnFailed,
    /// The broker is rate-limiting this peer.
    RateLimited,
    /// The broker is shutting down.
    ShuttingDown,
    /// The broker rejected this peer.
    PeerRejected,
    /// The broker failed internally.
    Internal,
    /// The requested version is explicitly blocked (for example, yanked).
    VersionBlocked,
    /// The broker is under file-descriptor pressure.
    FdPressure,
    /// A code newer than this build, preserved verbatim.
    Unrecognized(i32),
}

impl RefusalCode {
    /// Decode a wire code. Unknown values become [`RefusalCode::Unrecognized`].
    #[must_use]
    pub const fn from_wire(code: i32) -> Self {
        match code {
            0 => Self::Unspecified,
            1 => Self::VersionUnsupported,
            2 => Self::ServiceUnknown,
            3 => Self::BackendSpawnFailed,
            4 => Self::RateLimited,
            5 => Self::ShuttingDown,
            6 => Self::PeerRejected,
            7 => Self::Internal,
            8 => Self::VersionBlocked,
            9 => Self::FdPressure,
            other => Self::Unrecognized(other),
        }
    }

    /// Encode this code as its wire value.
    #[must_use]
    pub const fn to_wire(self) -> i32 {
        match self {
            Self::Unspecified => 0,
            Self::VersionUnsupported => 1,
            Self::ServiceUnknown => 2,
            Self::BackendSpawnFailed => 3,
            Self::RateLimited => 4,
            Self::ShuttingDown => 5,
            Self::PeerRejected => 6,
            Self::Internal => 7,
            Self::VersionBlocked => 8,
            Self::FdPressure => 9,
            Self::Unrecognized(code) => code,
        }
    }
}

impl From<i32> for RefusalCode {
    fn from(code: i32) -> Self {
        Self::from_wire(code)
    }
}

impl From<RefusalCode> for i32 {
    fn from(code: RefusalCode) -> Self {
        code.to_wire()
    }
}

/// Matchable decision surface for a broker refusal.
///
/// Every code outside the five actionable kinds lands in
/// [`RefusalKind::Other`], so consumer retry logic stays exhaustive when a
/// future broker adds codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefusalKind {
    /// Requested version unsupported: upgrade or downgrade, do not retry.
    VersionUnsupported,
    /// Requested version blocked: do not retry with the same version.
    VersionBlocked,
    /// Service unknown to this broker: a configuration error.
    ServiceUnknown,
    /// Rate-limited: honor [`BrokerRefusal::retry_after_ms`].
    RateLimited,
    /// Broker shutting down: retry against a fresh broker.
    ShuttingDown,
    /// Any other code, including codes newer than this build.
    Other(RefusalCode),
}

impl RefusalKind {
    /// Classify a refusal code.
    #[must_use]
    pub const fn from_code(code: RefusalCode) -> Self {
        match code {
            RefusalCode::VersionUnsupported => Self::VersionUnsupported,
            RefusalCode::VersionBlocked => Self::VersionBlocked,
            RefusalCode::ServiceUnknown => Self::ServiceUnknown,
            RefusalCode::RateLimited => Self::RateLimited,
            RefusalCode::ShuttingDown => Self::ShuttingDown,
            other => Self::Other(other),
        }
    }
}

/// A broker's explicit refusal of a Hello.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokerRefusal {
    code: RefusalCode,
    reason: String,
    retry_after_ms: u64,
}

impl BrokerRefusal {
    /// Build a refusal, for example to exercise an application's classifier.
    #[must_use]
    pub fn new(code: RefusalCode, reason: impl Into<String>, retry_after_ms: u64) -> Self {
        Self {
            code,
            reason: reason.into(),
            retry_after_ms,
        }
    }

    /// Stable refusal code.
    #[must_use]
    pub fn code(&self) -> RefusalCode {
        self.code
    }

    /// Matchable classification of [`Self::code`].
    #[must_use]
    pub fn kind(&self) -> RefusalKind {
        RefusalKind::from_code(self.code)
    }

    /// Human-readable reason supplied by the broker.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Broker-supplied back-off hint in milliseconds (0 = no hint).
    #[must_use]
    pub fn retry_after_ms(&self) -> u64 {
        self.retry_after_ms
    }
}

/// Failure to reach a backend through the broker.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BrokerClientError {
    /// [`DISABLE_ENV`] is `1`: use the direct path. Not a broker failure.
    #[error("broker disabled via {DISABLE_ENV}={DISABLE_VALUE}; use the direct path")]
    Disabled,
    /// [`DISABLE_ENV`] held a value other than unset or `1`.
    #[error("{DISABLE_ENV} must be unset or {DISABLE_VALUE}, got {value:?}")]
    InvalidDisableValue {
        /// The rejected value.
        value: String,
    },
    /// The broker endpoint could not be reached.
    #[error("failed to connect to broker {endpoint:?}: {source}")]
    BrokerConnect {
        /// Broker endpoint that was dialed.
        endpoint: String,
        /// Underlying transport failure.
        source: io::Error,
    },
    /// The broker (or the cache) named a backend endpoint that could not be
    /// reached.
    #[error("failed to connect to negotiated backend: {0}")]
    BackendConnect(#[source] io::Error),
    /// The broker spoke and explicitly declined.
    #[error("broker refused Hello: {} ({:?}, retry_after_ms={})", .0.reason, .0.code, .0.retry_after_ms)]
    Refused(BrokerRefusal),
    /// The broker's reply was malformed, incomplete, or failed in transit.
    #[error("broker protocol failure: {detail}")]
    Protocol {
        /// Diagnostic describing the failure.
        detail: String,
    },
}

impl BrokerClientError {
    /// Build a refusal error, for example to exercise an application's classifier.
    #[must_use]
    pub fn refused(code: RefusalCode, reason: impl Into<String>, retry_after_ms: u64) -> Self {
        Self::Refused(BrokerRefusal::new(code, reason, retry_after_ms))
    }

    /// The refusal, when the broker spoke and declined.
    #[must_use]
    pub fn refusal(&self) -> Option<&BrokerRefusal> {
        match self {
            Self::Refused(refusal) => Some(refusal),
            _ => None,
        }
    }

    /// Classification of the refusal; `None` for every non-refusal failure.
    #[must_use]
    pub fn refusal_kind(&self) -> Option<RefusalKind> {
        self.refusal().map(BrokerRefusal::kind)
    }
}

fn client_error(error: backend::BrokerClientError, broker_endpoint: &str) -> BrokerClientError {
    match error {
        backend::BrokerClientError::BrokerConnect(source) => BrokerClientError::BrokerConnect {
            endpoint: broker_endpoint.to_owned(),
            source,
        },
        backend::BrokerClientError::BackendConnect(source) => {
            BrokerClientError::BackendConnect(source)
        }
        backend::BrokerClientError::Refused {
            code,
            reason,
            retry_after_ms,
        } => {
            BrokerClientError::refused(RefusalCode::from_wire(code as i32), reason, retry_after_ms)
        }
        other => BrokerClientError::Protocol {
            detail: other.to_string(),
        },
    }
}

fn route(route: backend::BackendConnectionRoute) -> BackendRoute {
    match route {
        backend::BackendConnectionRoute::HelloSkip => BackendRoute::HelloSkip,
        backend::BackendConnectionRoute::BrokerNegotiated => BackendRoute::BrokerNegotiated,
        backend::BackendConnectionRoute::HandlePassed => BackendRoute::HandlePassed,
    }
}

/// Inputs for [`connect_backend`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendRequest {
    broker_endpoint: String,
    service_name: String,
    wanted_version: String,
    self_version: String,
    cached_backend_endpoint: Option<String>,
    client_version: String,
    client_library: Option<(String, String)>,
    client_keepalive_secs: u64,
}

impl BackendRequest {
    /// Request `service_name` at `wanted_version` from the broker at
    /// `broker_endpoint`; `self_version` is the caller's own service version.
    #[must_use]
    pub fn new(
        broker_endpoint: impl Into<String>,
        service_name: impl Into<String>,
        wanted_version: impl Into<String>,
        self_version: impl Into<String>,
    ) -> Self {
        Self {
            broker_endpoint: broker_endpoint.into(),
            service_name: service_name.into(),
            wanted_version: wanted_version.into(),
            self_version: self_version.into(),
            cached_backend_endpoint: None,
            client_version: String::new(),
            client_library: None,
            client_keepalive_secs: 0,
        }
    }

    /// A previously negotiated backend endpoint. When the wanted and own
    /// versions match, it is tried first and Hello is skipped on success.
    #[must_use]
    pub fn cached_backend_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.cached_backend_endpoint = Some(endpoint.into());
        self
    }

    /// Informational client version sent in Hello.
    #[must_use]
    pub fn client_version(mut self, version: impl Into<String>) -> Self {
        self.client_version = version.into();
        self
    }

    /// Client library name and version reported for broker diagnostics.
    /// Without this, the backend's own library identity is reported.
    #[must_use]
    pub fn client_library(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.client_library = Some((name.into(), version.into()));
        self
    }

    /// Proposed keepalive interval in seconds (0 = none).
    #[must_use]
    pub fn client_keepalive_secs(mut self, seconds: u64) -> Self {
        self.client_keepalive_secs = seconds;
        self
    }

    /// Broker endpoint this request dials.
    #[must_use]
    pub fn broker_endpoint(&self) -> &str {
        &self.broker_endpoint
    }

    /// Logical service name.
    #[must_use]
    pub fn service_name(&self) -> &str {
        &self.service_name
    }
}

/// An open backend connection: an ordinary blocking byte stream.
#[derive(Debug)]
pub struct BackendConnection {
    inner: backend::BackendConnection,
}

impl BackendConnection {
    /// How the connection was reached.
    #[must_use]
    pub fn route(&self) -> BackendRoute {
        route(self.inner.route)
    }

    /// Backend endpoint that was connected, suitable as a Hello-skip cache key.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.inner.endpoint
    }

    /// Bound how long a read may block (`None` waits indefinitely).
    ///
    /// An expired read fails with [`io::ErrorKind::WouldBlock`] or
    /// [`io::ErrorKind::TimedOut`], depending on the host.
    ///
    /// # Errors
    ///
    /// Returns the host error if the timeout cannot be applied.
    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        self.inner.stream.set_recv_timeout(timeout)
    }
}

impl Read for BackendConnection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.inner.stream.read(buffer)
    }
}

impl Write for BackendConnection {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.inner.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.stream.flush()
    }
}

/// Report whether [`DISABLE_ENV`] disables the broker.
///
/// # Errors
///
/// Returns [`BrokerClientError::InvalidDisableValue`] for a value other than
/// unset or [`DISABLE_VALUE`].
pub fn broker_disabled() -> Result<bool, BrokerClientError> {
    backend::broker_disabled_by_env()
        .map_err(|error| BrokerClientError::InvalidDisableValue { value: error.value })
}

/// Reach a backend through the broker, blocking until it connects or fails.
///
/// Honors [`DISABLE_ENV`] first. With a cached endpoint and matching
/// versions the cached endpoint is tried without Hello; otherwise this sends
/// the frozen v1 Hello and connects to the negotiated backend endpoint.
///
/// # Errors
///
/// [`BrokerClientError::Disabled`] when the escape hatch is set,
/// [`BrokerClientError::Refused`] when the broker declines, and a transport
/// or protocol variant otherwise.
pub fn connect_backend(request: &BackendRequest) -> Result<BackendConnection, BrokerClientError> {
    if broker_disabled()? {
        return Err(BrokerClientError::Disabled);
    }
    let mut backend_request = backend::ConnectBackendRequest::new(
        &request.broker_endpoint,
        &request.service_name,
        &request.wanted_version,
        &request.self_version,
    );
    backend_request.cached_backend_endpoint = request.cached_backend_endpoint.as_deref();
    backend_request.client_version = &request.client_version;
    if let Some((name, version)) = &request.client_library {
        backend_request.client_lib_name = name;
        backend_request.client_lib_version = version;
    }
    backend_request.client_keepalive_secs = request.client_keepalive_secs;
    backend::connect_to_backend(backend_request)
        .map(|inner| BackendConnection { inner })
        .map_err(|error| client_error(error, &request.broker_endpoint))
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process::broker::protocol::ErrorCode;

    /// Every backend wire code converts to the same owned code, and the owned
    /// classification agrees with the backend's own `RefusalKind` mapping.
    #[test]
    fn backend_refusals_convert_to_matching_owned_codes_and_kinds() {
        for wire in 0..=30 {
            let Ok(code) = ErrorCode::try_from(wire) else {
                assert!(matches!(
                    RefusalCode::from_wire(wire),
                    RefusalCode::Unrecognized(value) if value == wire
                ));
                continue;
            };
            let expected_kind = backend::RefusalKind::from_code(code);
            let converted = client_error(
                backend::BrokerClientError::Refused {
                    code,
                    reason: "test".to_owned(),
                    retry_after_ms: 1234,
                },
                "broker",
            );
            let refusal = converted.refusal().expect("refusal stays a refusal");
            assert_eq!(refusal.code().to_wire(), wire);
            assert_eq!(refusal.reason(), "test");
            assert_eq!(refusal.retry_after_ms(), 1234);
            let owned_kind = refusal.kind();
            let agrees = match (expected_kind, owned_kind) {
                (backend::RefusalKind::VersionUnsupported, RefusalKind::VersionUnsupported)
                | (backend::RefusalKind::VersionBlocked, RefusalKind::VersionBlocked)
                | (backend::RefusalKind::ServiceUnknown, RefusalKind::ServiceUnknown)
                | (backend::RefusalKind::RateLimited, RefusalKind::RateLimited)
                | (backend::RefusalKind::ShuttingDown, RefusalKind::ShuttingDown) => true,
                (backend::RefusalKind::Other(backend_code), RefusalKind::Other(owned_code)) => {
                    backend_code as i32 == owned_code.to_wire()
                }
                _ => false,
            };
            assert!(
                agrees,
                "wire code {wire}: {expected_kind:?} vs {owned_kind:?}"
            );
        }
    }

    #[test]
    fn backend_transport_and_protocol_failures_are_not_refusals() {
        let broker = client_error(
            backend::BrokerClientError::BrokerConnect(io::Error::from(io::ErrorKind::NotFound)),
            "broker.sock",
        );
        assert!(matches!(
            &broker,
            BrokerClientError::BrokerConnect { endpoint, .. } if endpoint == "broker.sock"
        ));
        let backend_dial = client_error(
            backend::BrokerClientError::BackendConnect(io::Error::from(
                io::ErrorKind::ConnectionRefused,
            )),
            "broker.sock",
        );
        assert!(matches!(backend_dial, BrokerClientError::BackendConnect(_)));
        let protocol = client_error(
            backend::BrokerClientError::MissingHelloReplyResult,
            "broker.sock",
        );
        assert!(matches!(protocol, BrokerClientError::Protocol { .. }));
        for error in [broker, backend_dial, protocol] {
            assert_eq!(error.refusal_kind(), None);
        }
    }

    #[test]
    fn backend_routes_convert_one_to_one() {
        assert_eq!(
            route(backend::BackendConnectionRoute::HelloSkip),
            BackendRoute::HelloSkip
        );
        assert_eq!(
            route(backend::BackendConnectionRoute::BrokerNegotiated),
            BackendRoute::BrokerNegotiated
        );
        assert_eq!(
            route(backend::BackendConnectionRoute::HandlePassed),
            BackendRoute::HandlePassed
        );
    }
}
