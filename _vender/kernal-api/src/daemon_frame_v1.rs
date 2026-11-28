//! Frozen v1 daemon-frame semantics for application-owned connections.
//!
//! The codec retains the exact `[1][u32 LE body length][protobuf Frame]`
//! layout implemented by the private process substrate. Applications retain
//! their endpoint, product payload identifiers, and I/O policy; this module
//! only translates the frozen envelope into facade-owned values.

use running_process::frame_v1 as backend;

/// Frozen outer framing byte for a v1 daemon frame.
pub const DAEMON_FRAME_V1_VERSION: u8 = backend::ENVELOPE_VERSION;

/// Maximum decoded daemon-frame body size (16 MiB).
pub const DAEMON_FRAME_V1_MAX_BODY_BYTES: usize = backend::MAX_FRAME_BYTES;

/// A facade-owned frozen v1 daemon envelope.
///
/// `kind` and `payload_encoding` intentionally stay raw discriminants. This
/// preserves unknown additive values rather than normalizing them through a
/// backend enum. W3C trace headers are retained privately so decode followed
/// by encode keeps their original bytes; [`Self::trace_id`] and
/// [`Self::span_id`] expose their semantic IDs when present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonFrame {
    envelope_version: u32,
    kind: i32,
    payload_protocol: u32,
    payload: Vec<u8>,
    request_id: u64,
    payload_encoding: i32,
    deadline_unix_ms: u64,
    traceparent: String,
    tracestate: String,
}

/// Semantic classification of a raw daemon-frame kind discriminant.
///
/// [`Self::Unknown`] preserves an additive or product-specific raw value. Use
/// [`DaemonFrame::kind`] when forwarding it without interpretation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonFrameKind {
    /// A product request that expects a correlated response.
    Request,
    /// A response correlated to a prior request.
    Response,
    /// A daemon-originated product event.
    Event,
    /// A request to cancel an in-flight product operation.
    Cancel,
    /// A raw kind not assigned a v1 semantic classification.
    Unknown(i32),
}

/// Semantic classification of a raw daemon-frame payload encoding.
///
/// [`Self::Unknown`] preserves an additive or product-specific raw value. Use
/// [`DaemonFrame::payload_encoding`] when forwarding it without interpretation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonPayloadEncoding {
    /// The payload is unencoded opaque product bytes.
    None,
    /// The payload uses the frozen Zstandard discriminant.
    Zstd,
    /// The payload uses the frozen Snappy discriminant.
    Snappy,
    /// The payload uses the frozen LZ4 discriminant.
    Lz4,
    /// A raw encoding not assigned a v1 semantic classification.
    Unknown(i32),
}

impl DaemonFrame {
    /// Create a request with the frozen v1 defaults.
    #[must_use]
    pub fn request(payload_protocol: u32, payload: Vec<u8>) -> Self {
        Self::from_backend(backend::Frame::request(payload_protocol, payload))
    }

    /// Create the frozen response to `request`.
    ///
    /// This uses the shared v1 response constructor, preserving the request
    /// correlation ID, product protocol, and W3C trace headers exactly.
    #[must_use]
    pub fn response_to(request: &Self, payload: Vec<u8>) -> Self {
        Self::from_backend(backend::Frame::response_to(&request.to_backend(), payload))
    }

    /// Set the opaque request/response correlation identifier.
    #[must_use]
    pub fn with_request_id(mut self, request_id: u64) -> Self {
        self.request_id = request_id;
        self
    }

    /// Set the raw frozen frame-kind discriminant.
    #[must_use]
    pub fn with_raw_kind(mut self, kind: i32) -> Self {
        self.kind = kind;
        self
    }

    /// Set the raw frozen payload-encoding discriminant.
    #[must_use]
    pub fn with_raw_payload_encoding(mut self, payload_encoding: i32) -> Self {
        self.payload_encoding = payload_encoding;
        self
    }

    /// Set the absolute Unix-millisecond deadline carried in the envelope.
    #[must_use]
    pub fn with_deadline_unix_ms(mut self, deadline_unix_ms: u64) -> Self {
        self.deadline_unix_ms = deadline_unix_ms;
        self
    }

    /// Set W3C trace and span identifiers using frozen v1 trace-header form.
    ///
    /// The default W3C version and sampled flag preserve the product's
    /// existing v1 constructor behavior. Decoded headers are not regenerated,
    /// so an existing header's version, flags, and formatting remain intact.
    #[must_use]
    pub fn with_trace_context(
        mut self,
        trace_id: impl AsRef<str>,
        span_id: impl AsRef<str>,
    ) -> Self {
        self.traceparent = format!("00-{}-{}-01", trace_id.as_ref(), span_id.as_ref());
        self
    }

    /// Set the opaque W3C trace-state value.
    #[must_use]
    pub fn with_trace_state(mut self, trace_state: impl Into<String>) -> Self {
        self.tracestate = trace_state.into();
        self
    }

    /// Frozen envelope version carried inside the protobuf body.
    #[must_use]
    pub fn envelope_version(&self) -> u32 {
        self.envelope_version
    }

    /// Raw frozen frame-kind discriminant.
    #[must_use]
    pub fn kind(&self) -> i32 {
        self.kind
    }

    /// Classify [`Self::kind`] without discarding an unknown raw value.
    #[must_use]
    pub fn kind_classification(&self) -> DaemonFrameKind {
        match self.kind {
            value if value == backend::FrameKind::Request as i32 => DaemonFrameKind::Request,
            value if value == backend::FrameKind::Response as i32 => DaemonFrameKind::Response,
            value if value == backend::FrameKind::Event as i32 => DaemonFrameKind::Event,
            value if value == backend::FrameKind::Cancel as i32 => DaemonFrameKind::Cancel,
            value => DaemonFrameKind::Unknown(value),
        }
    }

    /// Application-owned payload protocol identifier.
    #[must_use]
    pub fn payload_protocol(&self) -> u32 {
        self.payload_protocol
    }

    /// Opaque application payload bytes.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Correlation identifier carried by the envelope.
    #[must_use]
    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    /// Raw frozen payload-encoding discriminant.
    #[must_use]
    pub fn payload_encoding(&self) -> i32 {
        self.payload_encoding
    }

    /// Classify [`Self::payload_encoding`] without discarding an unknown raw value.
    #[must_use]
    pub fn payload_encoding_classification(&self) -> DaemonPayloadEncoding {
        match self.payload_encoding {
            value if value == backend::PayloadEncoding::None as i32 => DaemonPayloadEncoding::None,
            value if value == backend::PayloadEncoding::Zstd as i32 => DaemonPayloadEncoding::Zstd,
            value if value == backend::PayloadEncoding::Snappy as i32 => {
                DaemonPayloadEncoding::Snappy
            }
            value if value == backend::PayloadEncoding::Lz4 as i32 => DaemonPayloadEncoding::Lz4,
            value => DaemonPayloadEncoding::Unknown(value),
        }
    }

    /// Absolute Unix-millisecond deadline carried by the envelope.
    #[must_use]
    pub fn deadline_unix_ms(&self) -> u64 {
        self.deadline_unix_ms
    }

    /// W3C trace ID, when the retained trace header carries one.
    #[must_use]
    pub fn trace_id(&self) -> Option<&str> {
        trace_ids(&self.traceparent).map(|(trace_id, _)| trace_id)
    }

    /// W3C span ID, when the retained trace header carries one.
    #[must_use]
    pub fn span_id(&self) -> Option<&str> {
        trace_ids(&self.traceparent).map(|(_, span_id)| span_id)
    }

    /// Opaque W3C trace-state retained by the envelope.
    #[must_use]
    pub fn trace_state(&self) -> &str {
        &self.tracestate
    }

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

    fn to_backend(&self) -> backend::Frame {
        backend::Frame {
            envelope_version: self.envelope_version,
            kind: self.kind,
            payload_protocol: self.payload_protocol,
            payload: self.payload.clone(),
            request_id: self.request_id,
            payload_encoding: self.payload_encoding,
            deadline_unix_ms: self.deadline_unix_ms,
            traceparent: self.traceparent.clone(),
            tracestate: self.tracestate.clone(),
        }
    }
}

/// Stateless encoder and incremental decoder for [`DaemonFrame`].
#[derive(Clone, Copy, Debug, Default)]
pub struct DaemonFrameCodec;

impl DaemonFrameCodec {
    /// Encode `frame` into its complete frozen v1 wire representation.
    pub fn encode(frame: &DaemonFrame) -> Result<Vec<u8>, DaemonFrameError> {
        backend::encode_framed(&frame.to_backend()).map_err(DaemonFrameError::from_backend)
    }

    /// Construct and encode a frozen request frame in one operation.
    pub fn encode_request(
        payload_protocol: u32,
        payload: Vec<u8>,
        request_id: u64,
    ) -> Result<Vec<u8>, DaemonFrameError> {
        Self::encode(&DaemonFrame::request(payload_protocol, payload).with_request_id(request_id))
    }

    /// Construct and encode a frozen response to `request` in one operation.
    pub fn encode_response_to(
        request: &DaemonFrame,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, DaemonFrameError> {
        Self::encode(&DaemonFrame::response_to(request, payload))
    }

    /// Incrementally decode a single frame from the front of `buffer`.
    ///
    /// Partial headers and bodies produce [`DaemonFrameDecode::NeedMoreBytes`]
    /// and consume nothing. A decoded frame reports only its own byte count,
    /// leaving any trailing application bytes for the caller.
    pub fn decode(buffer: &[u8]) -> Result<DaemonFrameDecode, DaemonFrameError> {
        match backend::try_decode_framed(buffer).map_err(DaemonFrameError::from_backend)? {
            Some(decoded) => Ok(DaemonFrameDecode::Frame {
                frame: DaemonFrame::from_backend(decoded.frame),
                consumed: decoded.consumed,
            }),
            None => Ok(DaemonFrameDecode::NeedMoreBytes),
        }
    }
}

/// Incremental daemon-frame decoding result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonFrameDecode {
    /// More bytes are necessary for either the frozen header or its body.
    NeedMoreBytes,
    /// One complete decoded frame and the exact number of input bytes it used.
    Frame {
        /// Facade-owned decoded frame.
        frame: DaemonFrame,
        /// Header plus protobuf-body bytes consumed from the input.
        consumed: usize,
    },
}

/// Errors from frozen daemon-frame encoding and decoding.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DaemonFrameError {
    /// The outer framing byte is not the v1 frozen value.
    #[error("unsupported daemon-frame version {received}; expected {expected}")]
    UnsupportedFrameVersion {
        /// Byte received before the length prefix.
        received: u8,
        /// Frozen v1 byte expected by the codec.
        expected: u8,
    },
    /// The claimed or encoded body exceeds the v1 16 MiB bound.
    #[error("daemon-frame body {body_length} exceeds maximum {maximum}")]
    FrameTooLarge {
        /// Length claimed by the little-endian outer header.
        body_length: usize,
        /// Frozen maximum accepted body length.
        maximum: usize,
    },
    /// The complete body cannot be decoded as a frozen v1 envelope.
    #[error("malformed daemon-frame body")]
    MalformedFrame,
}

impl DaemonFrameError {
    fn from_backend(error: backend::FramingError) -> Self {
        match error {
            backend::FramingError::UnsupportedFramingVersion { got, expected } => {
                Self::UnsupportedFrameVersion {
                    received: got,
                    expected,
                }
            }
            backend::FramingError::FrameTooLarge { body_length, cap } => Self::FrameTooLarge {
                body_length,
                maximum: cap,
            },
            backend::FramingError::UnexpectedEof { .. }
            | backend::FramingError::Io(_)
            | backend::FramingError::Decode(_) => Self::MalformedFrame,
        }
    }
}

/// Return whether `payload_protocol` is reserved by the shared frame codec.
#[doc(hidden)]
pub const fn is_first_party_payload_protocol(payload_protocol: u32) -> bool {
    backend::registry::is_first_party(payload_protocol)
}

/// Return whether `payload_protocol` belongs to the registered consumer range.
#[doc(hidden)]
pub const fn is_registered_payload_protocol(payload_protocol: u32) -> bool {
    backend::registry::is_registered_consumer_id(payload_protocol)
}

/// Return whether `payload_protocol` belongs to the private-use range.
#[doc(hidden)]
pub const fn is_private_payload_protocol(payload_protocol: u32) -> bool {
    backend::registry::is_private_use_id(payload_protocol)
}

/// Register a product-owned frozen daemon-frame payload protocol.
///
/// Product identifiers remain owned by applications. The macro applies the
/// same compile-time registration checks as the private shared codec without
/// exposing its crate path to a downstream consumer.
#[macro_export]
macro_rules! register_daemon_frame_payload_protocol {
    ($(#[$meta:meta])* $vis:vis const $name:ident: u32 = $value:expr;) => {
        $(#[$meta])*
        $vis const $name: u32 = $value;

        const _: () = {
            assert!(
                !$crate::daemon_frame_v1::is_first_party_payload_protocol($name),
                concat!(
                    stringify!($name),
                    " collides with a first-party daemon-frame payload protocol",
                ),
            );
            assert!(
                $crate::daemon_frame_v1::is_registered_payload_protocol($name)
                    || $crate::daemon_frame_v1::is_private_payload_protocol($name),
                concat!(
                    stringify!($name),
                    " must lie in the registered-consumer range (0x7000..=0x7EFF) ",
                    "or the private-use range (0xF000..=0xFFFF)",
                ),
            );
        };
    };
}

fn trace_ids(traceparent: &str) -> Option<(&str, &str)> {
    let mut fields = traceparent.split('-');
    let _version = fields.next()?;
    let trace_id = fields.next()?;
    let span_id = fields.next()?;
    let _flags = fields.next()?;
    if fields.next().is_some() || trace_id.is_empty() || span_id.is_empty() {
        return None;
    }
    Some((trace_id, span_id))
}
