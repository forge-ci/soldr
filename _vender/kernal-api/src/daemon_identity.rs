//! Direct-daemon identity semantics for an application-owned endpoint.
//!
//! The implementation delegates frozen v1 framing, probe validation, and
//! sidecar encoding to the private process substrate.  This module owns the
//! public vocabulary and deliberately does not choose endpoint names, product
//! protocol identifiers, or daemon lifecycle policy.

use std::path::{Path, PathBuf};

use running_process::backend_identity as backend;

#[path = "daemon_identity_control.rs"]
mod control;

pub use control::{DaemonVerifyError, VerifiedDaemon};

/// An application-selected daemon endpoint.
///
/// Both strings are retained verbatim.  In particular, this facade never
/// converts a namespaced address into a filesystem path (or vice versa).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DaemonEndpoint {
    namespace: String,
    address: String,
}

impl DaemonEndpoint {
    /// Record an endpoint chosen by the application.
    pub fn new(namespace: impl Into<String>, address: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            address: address.into(),
        }
    }

    /// Return the application-owned namespace verbatim.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Return the application-owned address verbatim.
    pub fn address(&self) -> &str {
        &self.address
    }
}

/// A normalized identity for a daemon already bound to an endpoint.
///
/// It is suitable for durable sidecars and for proving that an endpoint still
/// serves precisely this process.  The cryptographic fields have fixed width
/// so callers cannot accidentally accept a truncated digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonIdentity {
    inner: backend::DaemonProcess,
}

/// Digest material captured for a newly constructed daemon identity.
///
/// The default remains [`Self::LegacyCompatible`]. Applications with an
/// established zero-filled legacy probe field can choose [`Self::Blake3Only`]
/// to preserve that fixed wire contract and avoid a second executable read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DaemonIdentityHashPolicy {
    /// Compute BLAKE3 and the historical SHA-256 compatibility digest.
    #[default]
    LegacyCompatible,
    /// Compute BLAKE3 only and retain a zero-filled legacy digest.
    Blake3Only,
}

impl DaemonIdentityHashPolicy {
    fn into_backend(self) -> backend::DaemonIdentityHashPolicy {
        match self {
            Self::LegacyCompatible => backend::DaemonIdentityHashPolicy::LegacyCompatible,
            Self::Blake3Only => backend::DaemonIdentityHashPolicy::Blake3Only,
        }
    }
}

/// Every field of a [`DaemonIdentity`], for applications that assemble or
/// inspect one field by field.
///
/// Converting through this record is lossless in both directions:
/// `DaemonIdentity::from_record(identity.to_record()) == identity`.  An
/// application that computes its own executable digest, or a test that needs
/// a foreign or deliberately stale identity, builds one here instead of
/// capturing the current process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonIdentityRecord {
    /// Operating-system process identifier.
    pub pid: u32,
    /// Executable path the daemon was started from.
    pub executable_path: PathBuf,
    /// BLAKE3 digest of that executable.
    pub blake3_digest: [u8; 32],
    /// Historical SHA-256 probe digest; all zeroes when not computed.
    pub legacy_sha256_digest: [u8; 32],
    /// Host boot identifier observed when the daemon started.
    pub boot_id: String,
    /// Endpoint the daemon serves.
    pub endpoint: DaemonEndpoint,
    /// Unix timestamp, in milliseconds, at which the daemon started.
    pub started_at_unix_ms: u64,
    /// Idle timeout advertised by the daemon, if it has one.
    pub idle_timeout_secs: Option<u32>,
}

impl DaemonIdentity {
    /// Assemble an identity from explicit field values.
    ///
    /// No field is checked against the host; use [`Self::verify_live`] or a
    /// probe for that.
    pub fn from_record(record: DaemonIdentityRecord) -> Self {
        Self::from_backend(backend::DaemonProcess {
            pid: record.pid,
            exe_path: record.executable_path,
            exe_hash: record.blake3_digest,
            legacy_exe_sha256: record.legacy_sha256_digest,
            boot_id: record.boot_id,
            ipc_endpoint: record.endpoint.into_backend(),
            started_at_unix_ms: record.started_at_unix_ms,
            idle_timeout_secs: record.idle_timeout_secs,
        })
    }

    /// Copy every field of this identity into a plain record.
    pub fn to_record(&self) -> DaemonIdentityRecord {
        DaemonIdentityRecord {
            pid: self.inner.pid,
            executable_path: self.inner.exe_path.clone(),
            blake3_digest: self.inner.exe_hash,
            legacy_sha256_digest: self.inner.legacy_exe_sha256,
            boot_id: self.inner.boot_id.clone(),
            endpoint: self.endpoint(),
            started_at_unix_ms: self.inner.started_at_unix_ms,
            idle_timeout_secs: self.inner.idle_timeout_secs,
        }
    }

    /// Construct the identity of this process after its final endpoint has
    /// been selected.
    pub fn current_process(
        endpoint: DaemonEndpoint,
        idle_timeout_secs: Option<u32>,
    ) -> Result<Self, DaemonIdentityError> {
        Self::current_process_with_hash_policy(
            endpoint,
            idle_timeout_secs,
            DaemonIdentityHashPolicy::LegacyCompatible,
        )
    }

    /// Construct the identity of this process with an explicit legacy-digest
    /// policy.
    pub fn current_process_with_hash_policy(
        endpoint: DaemonEndpoint,
        idle_timeout_secs: Option<u32>,
        hash_policy: DaemonIdentityHashPolicy,
    ) -> Result<Self, DaemonIdentityError> {
        backend::DaemonProcess::current_process_with_hash_policy(
            endpoint.into_backend(),
            idle_timeout_secs,
            hash_policy.into_backend(),
        )
        .map(Self::from_backend)
        .map_err(DaemonIdentityError::current_process)
    }

    /// Operating-system process identifier captured in this identity.
    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    /// Executable path captured in this identity.
    pub fn executable_path(&self) -> &Path {
        &self.inner.exe_path
    }

    /// BLAKE3 digest of the executable captured in this identity.
    pub fn blake3_digest(&self) -> &[u8; 32] {
        &self.inner.exe_hash
    }

    /// SHA-256 digest retained for compatibility with stable v1 probes.
    pub fn legacy_sha256_digest(&self) -> &[u8; 32] {
        &self.inner.legacy_exe_sha256
    }

    /// Host boot identity captured when the daemon started.
    pub fn boot_id(&self) -> &str {
        &self.inner.boot_id
    }

    /// Endpoint supplied when this identity was constructed or decoded.
    pub fn endpoint(&self) -> DaemonEndpoint {
        DaemonEndpoint::from_backend(&self.inner.ipc_endpoint)
    }

    /// Unix timestamp, in milliseconds, captured when the daemon started.
    pub fn started_at_unix_ms(&self) -> u64 {
        self.inner.started_at_unix_ms
    }

    /// Idle timeout advertised by the daemon, if it has one.
    pub fn idle_timeout_secs(&self) -> Option<u32> {
        self.inner.idle_timeout_secs
    }

    /// Persist this identity atomically as an adjacent JSON sidecar.
    ///
    /// JSON is only the human-facing durable sidecar format.  It is not a
    /// daemon control-plane or IPC fallback.
    pub fn write_sidecar(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        backend::write_daemon_identity_file(path.as_ref(), &self.inner)
    }

    /// Read an identity sidecar tolerantly.
    ///
    /// Missing or malformed sidecars return `None`, allowing callers to fall
    /// back to their normal application-owned discovery path.
    pub fn read_sidecar(path: impl AsRef<Path>) -> Option<Self> {
        backend::read_daemon_identity_file(path.as_ref()).map(Self::from_backend)
    }

    /// Read an identity sidecar while retaining I/O and decoding failures.
    pub fn try_read_sidecar(path: impl AsRef<Path>) -> std::io::Result<Option<Self>> {
        backend::try_read_daemon_identity_file(path.as_ref())
            .map(|identity| identity.map(Self::from_backend))
    }

    /// Remove an identity sidecar as a best-effort clean-shutdown operation.
    pub fn remove_sidecar(path: impl AsRef<Path>) {
        backend::remove_daemon_identity_file(path.as_ref());
    }

    /// Prove, without occupying an async worker, that this identity still
    /// serves its recorded endpoint.
    ///
    /// Equivalent to [`Self::probe_endpoint`] with [`Self::endpoint`].
    pub async fn probe_same_endpoint(&self) -> ProbeSameEndpoint {
        self.probe_endpoint(&self.endpoint()).await
    }

    /// Prove, without occupying an async worker, that this identity serves
    /// `endpoint`.
    ///
    /// See [`Self::probe_endpoint_blocking`] for the checks performed.
    pub async fn probe_endpoint(&self, endpoint: &DaemonEndpoint) -> ProbeSameEndpoint {
        let endpoint = endpoint.clone();
        let expected = self.clone();
        crate::async_engine::launch_blocking(move || expected.probe_endpoint_blocking(&endpoint))
            .await
            .unwrap_or(ProbeSameEndpoint::NotCurrent)
    }

    /// Prove that this identity serves `endpoint`, blocking the caller.
    ///
    /// `endpoint` is the endpoint the caller resolved from its own
    /// configuration; it must equal the recorded endpoint exactly.  The
    /// recorded process must then verify as in [`Self::verify_live`], and
    /// the endpoint must answer the frozen v1 nonce probe with precisely this
    /// identity.  A successful probe performs the substrate's single endpoint
    /// connection and nonce round trip: it does not construct a client,
    /// derive another endpoint, or add a product-protocol exchange.  The
    /// response wait is bounded at 500 ms.
    ///
    /// **Blocking.** From asynchronous code use [`Self::probe_endpoint`].
    pub fn probe_endpoint_blocking(&self, endpoint: &DaemonEndpoint) -> ProbeSameEndpoint {
        let endpoint = endpoint.clone().into_backend();
        if backend::BackendHandle::probe(&endpoint, &self.inner).is_some() {
            ProbeSameEndpoint::Current
        } else {
            ProbeSameEndpoint::NotCurrent
        }
    }

    fn from_backend(inner: backend::DaemonProcess) -> Self {
        Self { inner }
    }
}

/// Failure while collecting the identity of the current process.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub struct DaemonIdentityError {
    detail: String,
}

impl DaemonIdentityError {
    fn current_process(error: backend::IdentityError) -> Self {
        Self {
            detail: error.to_string(),
        }
    }
}

/// Result of an endpoint identity probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeSameEndpoint {
    /// The endpoint accepted the frozen nonce proof and returned this identity.
    Current,
    /// The endpoint was unavailable, stale, or did not prove this identity.
    NotCurrent,
}

/// Application classification of the leading bytes of its legacy wire.
///
/// The caller provides this before the facade parses the v1 envelope, so an
/// application legacy header beginning with `0x01` remains application-owned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyPrefix {
    /// The bytes belong to the application's legacy wire.
    Legacy,
    /// The bytes do not belong to the application's legacy wire.
    NotLegacy,
    /// More bytes are required before the application can decide.
    NeedMoreBytes,
}

/// A facade-owned responder for identity probes and product payload envelopes.
///
/// It is sans-I/O: an application's accept loop retains its stream and buffer,
/// calls [`Self::poll`], writes a returned probe reply verbatim, and advances
/// its buffer only by the returned `consumed` count.
#[derive(Clone, Debug)]
pub struct ProbeResponder {
    daemon: backend::DaemonProcess,
    served_payload_protocols: Vec<u32>,
}

impl ProbeResponder {
    /// Create a responder for an existing daemon endpoint.
    ///
    /// Product protocol identifiers are supplied by the application; this
    /// facade neither defines nor negotiates them.
    pub fn new(
        daemon: DaemonIdentity,
        served_payload_protocols: impl IntoIterator<Item = u32>,
    ) -> Self {
        Self {
            daemon: daemon.inner,
            served_payload_protocols: served_payload_protocols.into_iter().collect(),
        }
    }

    /// Return a copy of the identity this responder proves.
    pub fn identity(&self) -> DaemonIdentity {
        DaemonIdentity::from_backend(self.daemon.clone())
    }

    /// Classify the front of a buffered application connection.
    ///
    /// The supplied [`LegacyPrefix`] is honored before every v1 frame check,
    /// preserving legacy-first behavior and the exact consumed-byte contract.
    pub fn poll(
        &self,
        buffered: &[u8],
        legacy_prefix: LegacyPrefix,
    ) -> Result<ProbeMuxResult, DaemonMuxError> {
        let mux = backend::BackendEndpointMux::new(
            self.daemon.clone(),
            &self.served_payload_protocols,
            move |_| legacy_prefix.into_backend(),
        );
        match mux.poll(buffered).map_err(DaemonMuxError::from_backend)? {
            backend::MuxPoll::NeedMoreBytes => Ok(ProbeMuxResult::NeedMoreBytes),
            backend::MuxPoll::Legacy => Ok(ProbeMuxResult::Legacy),
            backend::MuxPoll::ProbeAnswered { reply, consumed } => {
                Ok(ProbeMuxResult::ProbeReply { reply, consumed })
            }
            backend::MuxPoll::Payload { frame, consumed } => Ok(ProbeMuxResult::ProductFrame {
                frame: ProductFrame::from_backend(frame),
                consumed,
            }),
        }
    }
}

/// A product-owned v1 envelope delivered by [`ProbeResponder`].
///
/// `kind` and `payload_encoding` intentionally remain their raw numeric
/// discriminants.  This preserves unknown future values for the application
/// instead of normalizing them through an implementation enum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProductFrame {
    /// Frozen envelope version carried by the decoded frame.
    pub envelope_version: u32,
    /// Raw frame-kind discriminant.
    pub kind: i32,
    /// Application-owned product payload protocol identifier.
    pub payload_protocol: u32,
    /// Opaque product request or response bytes.
    pub payload: Vec<u8>,
    /// Correlation identifier preserved from the incoming frame.
    pub request_id: u64,
    /// Raw payload-encoding discriminant.
    pub payload_encoding: i32,
    /// Optional absolute deadline value preserved from the incoming frame.
    pub deadline_unix_ms: u64,
    /// W3C trace parent preserved from the incoming frame.
    pub traceparent: String,
    /// W3C trace state preserved from the incoming frame.
    pub tracestate: String,
}

impl ProductFrame {
    fn from_backend(frame: backend::Frame) -> Self {
        Self {
            envelope_version: frame.envelope_version,
            kind: frame.kind,
            payload_protocol: frame.payload_protocol,
            payload: frame.payload,
            request_id: frame.request_id,
            payload_encoding: frame.payload_encoding,
            deadline_unix_ms: frame.deadline_unix_ms,
            traceparent: frame.traceparent,
            tracestate: frame.tracestate,
        }
    }
}

/// Verdict returned by [`ProbeResponder::poll`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeMuxResult {
    /// The buffer cannot yet be classified or decoded.  Consume no bytes.
    NeedMoreBytes,
    /// The buffer belongs to application legacy wire.  Consume no bytes.
    Legacy,
    /// A frozen v1 nonce probe was answered.
    ProbeReply {
        /// Complete v1 wire bytes to write back verbatim.
        reply: Vec<u8>,
        /// Count of request bytes to consume from the input buffer.
        consumed: usize,
    },
    /// A complete application product frame was decoded.
    ProductFrame {
        /// Product fields preserved without protocol interpretation.
        frame: ProductFrame,
        /// Count of frame bytes to consume from the input buffer.
        consumed: usize,
    },
}

/// Connection-fatal mux failures represented without backend error types.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DaemonMuxError {
    /// The leading framing byte is not the frozen v1 version.
    #[error("unsupported daemon frame version {received}; expected {expected}")]
    UnsupportedFrameVersion {
        /// Byte received at the start of the frame.
        received: u8,
        /// Frozen v1 framing byte expected by this facade.
        expected: u8,
    },
    /// The claimed v1 frame body exceeds the 16 MiB maximum.
    #[error("daemon frame body {body_length} exceeds maximum {maximum}")]
    FrameTooLarge {
        /// Body length declared in the v1 little-endian header.
        body_length: usize,
        /// Maximum accepted body length.
        maximum: usize,
    },
    /// The frame body is not a valid v1 envelope.
    #[error("malformed daemon frame")]
    MalformedFrame,
    /// A reserved identity probe did not meet the frozen probe contract.
    #[error("malformed daemon identity probe")]
    MalformedProbe,
    /// A first-party control protocol reached a direct daemon endpoint.
    #[error("unexpected first-party payload protocol {payload_protocol:#06X}")]
    UnexpectedFirstPartyPayload {
        /// Reserved protocol identifier received on this endpoint.
        payload_protocol: u32,
    },
    /// The endpoint has no handler for this product protocol identifier.
    #[error("unserved product payload protocol {payload_protocol:#06X}")]
    UnservedProductProtocol {
        /// Application-owned protocol identifier with no configured handler.
        payload_protocol: u32,
    },
}

impl DaemonMuxError {
    fn from_backend(error: backend::MuxError) -> Self {
        match error {
            backend::MuxError::Framing(backend::FramingError::UnsupportedFramingVersion {
                got,
                expected,
            }) => Self::UnsupportedFrameVersion {
                received: got,
                expected,
            },
            backend::MuxError::Framing(backend::FramingError::FrameTooLarge {
                body_length,
                cap,
            }) => Self::FrameTooLarge {
                body_length,
                maximum: cap,
            },
            backend::MuxError::Framing(_) => Self::MalformedFrame,
            backend::MuxError::MalformedProbe(_) => Self::MalformedProbe,
            backend::MuxError::UnexpectedFirstPartyFrame { payload_protocol } => {
                Self::UnexpectedFirstPartyPayload { payload_protocol }
            }
            backend::MuxError::UnservedPayloadProtocol { payload_protocol } => {
                Self::UnservedProductProtocol { payload_protocol }
            }
        }
    }
}

impl DaemonEndpoint {
    fn from_backend(endpoint: &backend::Endpoint) -> Self {
        Self {
            namespace: endpoint.namespace_id.clone(),
            address: endpoint.path.clone(),
        }
    }

    fn into_backend(self) -> backend::Endpoint {
        backend::Endpoint {
            namespace_id: self.namespace,
            path: self.address,
        }
    }
}

impl LegacyPrefix {
    fn into_backend(self) -> backend::LegacyClassification {
        match self {
            Self::Legacy => backend::LegacyClassification::Legacy,
            Self::NotLegacy => backend::LegacyClassification::NotLegacy,
            Self::NeedMoreBytes => backend::LegacyClassification::NeedMoreBytes,
        }
    }
}
