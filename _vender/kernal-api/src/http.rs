//! Bounded HTTP client operations on the caller's async runtime.
//!
//! Redirects are opt-in; credentials are never inferred.
//! No proxy is ever used, including from the environment. URL hosts must be
//! ASCII as written: IDNA and percent-encoded hosts are rejected, not converted.
//! Response status interpretation and payload schemas belong to the caller.
//! Bodies preserve content-encoded wire bytes; decoding belongs to the caller.
//! HTTP/1 transport metadata has independent fixed parser ceilings. Caller
//! header limits are post-parse acceptance limits, not allocator/RSS ceilings.

use std::error::Error as _;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

// Local TLS fixtures for whichever backend is selected. rustls must refuse the
// self-signed CA used as its own leaf; both backends accept the CA-signed leaf.
#[cfg(test)]
#[path = "http_tls_tests.rs"]
mod tls_tests;

#[path = "http_url.rs"]
mod url;

use http_body_util::BodyExt;
use tower_service::Service;
use url::Target;

type RequestBody = http_body_util::Full<bytes::Bytes>;
type Transport = hyper_util::client::legacy::Client<TimedConnector, RequestBody>;

type HttpConnector = hyper_util::client::legacy::connect::HttpConnector;

// Native TLS wins when both features are enabled.
#[cfg(feature = "http-client")]
type Connector = hyper_tls::HttpsConnector<HttpConnector>;
#[cfg(all(feature = "http-client-rustls", not(feature = "http-client")))]
type Connector = hyper_rustls::HttpsConnector<HttpConnector>;

// The pool can finish a speculative connection in the background. Bound that
// attempt even after the request has checked out a different connection.
#[derive(Clone)]
struct TimedConnector {
    inner: Connector,
    timeout: Duration,
}

impl Service<hyper::Uri> for TimedConnector {
    type Response = <Connector as Service<hyper::Uri>>::Response;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_ready(cx).map_err(transport)
    }

    fn call(&mut self, uri: hyper::Uri) -> Self::Future {
        let connecting = tokio::time::timeout(self.timeout, self.inner.call(uri));
        Box::pin(async move {
            connecting
                .await
                .map_err(|_| timed_out("HTTP connect timed out"))?
                .map_err(transport)
        })
    }
}

/// TCP only; the connector never consults proxy settings.
fn tcp_connector() -> HttpConnector {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http
}

/// Platform TLS verified against the operating system's trust store.
/// `extra_root` is a test-only PEM root added to one client.
#[cfg(feature = "http-client")]
fn connector(extra_root: Option<&[u8]>, protocol: Protocol) -> io::Result<Connector> {
    let mut builder = native_tls::TlsConnector::builder();
    builder.request_alpns(match protocol {
        Protocol::Auto => &["h2", "http/1.1"],
        Protocol::Http1Only => &["http/1.1"],
        Protocol::Http2Only => &["h2"],
    });
    if let Some(pem) = extra_root {
        builder.add_root_certificate(
            native_tls::Certificate::from_pem(pem).map_err(io::Error::other)?,
        );
    }
    let tls = builder.build().map_err(io::Error::other)?;
    Ok(hyper_tls::HttpsConnector::from((
        tcp_connector(),
        tls.into(),
    )))
}

/// rustls with the ring provider and only the bundled webpki roots: no
/// OpenSSL, no platform verifier, no system trust store (#427).
/// `extra_root` is a test-only PEM root added to one client.
#[cfg(all(feature = "http-client-rustls", not(feature = "http-client")))]
fn connector(extra_root: Option<&[u8]>, protocol: Protocol) -> io::Result<Connector> {
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Some(pem) = extra_root {
        for certificate in rustls::pki_types::CertificateDer::pem_slice_iter(pem) {
            roots
                .add(certificate.map_err(io::Error::other)?)
                .map_err(io::Error::other)?;
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(io::Error::other)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let builder = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_or_http();
    Ok(match protocol {
        Protocol::Auto => builder
            .enable_http1()
            .enable_http2()
            .wrap_connector(tcp_connector()),
        Protocol::Http1Only => builder.enable_http1().wrap_connector(tcp_connector()),
        Protocol::Http2Only => builder.enable_http2().wrap_connector(tcp_connector()),
    })
}

fn timed_out(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, message)
}

/// The smaller of `phase` and what remains of the total deadline.
fn phase_budget(phase: Duration, deadline: std::time::Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(std::time::Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .map(|remaining| remaining.min(phase))
        .ok_or_else(|| timed_out("HTTP total deadline expired"))
}

/// Per-request ceilings. A client can be reused for requests with this policy.
/// All timeouts must be nonzero and at most 365 days.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Redirect hops allowed (at most 32). Zero returns 3xx without following.
    /// Cross-origin hops discard all application headers; HTTPS downgrade is
    /// rejected. POST becomes GET for 301/302/303. For 307/308, POST is replayed
    /// only within the same origin; cross-origin POST replay is rejected.
    pub max_redirects: u8,
    /// Maximum buffered request body.
    pub max_request_bytes: usize,
    /// Maximum encoded URL length before parsing.
    pub max_url_bytes: usize,
    /// Maximum response body accepted, including streaming reads.
    pub max_body_bytes: u64,
    /// Maximum accepted response header names and values in aggregate.
    /// Checked after the backend has parsed headers, not before allocation.
    pub max_header_bytes: usize,
    /// Maximum request or response header values, including duplicates.
    /// At most 1024, to stay below the private transport's header-map ceiling.
    /// The HTTP/1 response parser independently rejects more than 100 fields.
    pub max_header_count: usize,
    /// Maximum connection-establishment duration.
    pub connect_timeout: Duration,
    /// Maximum time from request start through response-body completion.
    pub total_timeout: Duration,
    /// Maximum wait for the next network read, reset on read progress.
    pub read_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_redirects: 0,
            max_request_bytes: 4 * 1024 * 1024,
            max_url_bytes: 8192,
            max_body_bytes: 4 * 1024 * 1024,
            max_header_bytes: 64 * 1024,
            max_header_count: 128,
            connect_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(120),
            read_timeout: Duration::from_secs(30),
        }
    }
}

/// Supported request operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// Retrieve a representation.
    Get,
    /// Retrieve representation metadata without a response body.
    Head,
    /// Submit an application payload.
    Post,
}

/// Borrowed application request. Body and headers are validated before copying.
pub struct Request<'a> {
    /// HTTP operation.
    pub method: Method,
    /// Absolute HTTP(S) URL without embedded credentials.
    pub url: &'a str,
    /// Application headers; connection/framing headers are transport-owned.
    pub headers: &'a [(&'a str, &'a str)],
    /// Small POST payload. GET and HEAD require an empty body.
    pub body: &'a [u8],
}

impl<'a> Request<'a> {
    /// Construct a GET with no extra headers or body.
    pub fn get(url: &'a str) -> Self {
        Self {
            method: Method::Get,
            url,
            headers: &[],
            body: &[],
        }
    }
}

/// HTTP protocol policy for a client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// Negotiate with TLS and use HTTP/1 for cleartext.
    Auto,
    /// Use HTTP/1 only.
    Http1Only,
    /// Require HTTP/2, using prior knowledge for cleartext endpoints.
    Http2Only,
}

/// Reusable HTTP transport; does not construct an executor or runtime.
#[derive(Clone)]
pub struct Client {
    transport: Transport,
    limits: Limits,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Transport failure. Backend errors never carry the request URL.
fn transport(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::other(error)
}

fn request_error(error: hyper_util::client::legacy::Error) -> io::Error {
    if error
        .source()
        .and_then(|source| source.downcast_ref::<io::Error>())
        .is_some_and(|source| source.kind() == io::ErrorKind::TimedOut)
    {
        timed_out("HTTP connect timed out")
    } else {
        transport(error)
    }
}

fn require_blocking_context() -> io::Result<()> {
    if crate::async_engine::RuntimeHandle::current().is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "blocking HTTP cannot run inside an async runtime; use the async client",
        ));
    }
    Ok(())
}

/// Blocking view of the same HTTP transport, driven by a caller-owned runtime.
/// Does not create a runtime or a background worker. Calls from an entered
/// async runtime return `WouldBlock`, rather than attempting nested execution.
pub struct BlockingClient<'runtime> {
    client: Client,
    runtime: &'runtime crate::async_engine::Runtime,
}

impl<'runtime> BlockingClient<'runtime> {
    /// Bind the client to an existing kernel runtime.
    pub fn new(
        runtime: &'runtime crate::async_engine::Runtime,
        limits: Limits,
    ) -> io::Result<Self> {
        require_blocking_context()?;
        Ok(Self {
            client: Client::new(limits)?,
            runtime,
        })
    }

    /// Issue a GET without collecting its body.
    pub fn get(&self, url: &str) -> io::Result<BlockingResponse<'runtime>> {
        self.execute(Request::get(url))
    }

    /// Execute on the supplied runtime; all async request limits also apply.
    // The request future is bounded by the client's `Limits` (connect, total
    // and read timeouts), so the deprecated unbounded bridge cannot hang (#433).
    #[allow(deprecated)]
    pub fn execute(&self, request: Request<'_>) -> io::Result<BlockingResponse<'runtime>> {
        require_blocking_context()?;
        Ok(BlockingResponse {
            response: self.runtime.run(self.client.execute(request))?,
            runtime: self.runtime,
        })
    }
}

/// Streaming blocking response tied to its caller-owned runtime.
/// Implements `Read` without collecting the full body. Drop releases the
/// response; the borrowed runtime remains owned by the caller.
pub struct BlockingResponse<'runtime> {
    response: Response,
    runtime: &'runtime crate::async_engine::Runtime,
}

impl BlockingResponse<'_> {
    /// Numeric status; non-success statuses are not transport errors.
    pub fn status(&self) -> u16 {
        self.response.status()
    }

    /// First header value, looked up case-insensitively.
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.response.header(name)
    }

    /// Collect only unread bytes, under the same cumulative body limit.
    // The request future is bounded by the client's `Limits` (connect, total
    // and read timeouts), so the deprecated unbounded bridge cannot hang (#433).
    #[allow(deprecated)]
    pub fn into_bytes(self) -> io::Result<Vec<u8>> {
        require_blocking_context()?;
        self.runtime.run(self.response.into_bytes())
    }
}

impl io::Read for BlockingResponse<'_> {
    // The request future is bounded by the client's `Limits` (connect, total
    // and read timeouts), so the deprecated unbounded bridge cannot hang (#433).
    #[allow(deprecated)]
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        require_blocking_context()?;
        self.runtime.run(self.response.read(buffer))
    }
}

impl Client {
    /// Build a transport with finite, nonzero timeouts and verified TLS.
    pub fn new(limits: Limits) -> io::Result<Self> {
        Self::with_protocol(limits, Protocol::Auto)
    }

    /// Build a transport with an explicit HTTP protocol policy.
    pub fn with_protocol(limits: Limits, protocol: Protocol) -> io::Result<Self> {
        Self::with_connector(limits, protocol, connector(None, protocol)?)
    }

    #[cfg(test)]
    pub(crate) fn with_test_root(limits: Limits, pem: &[u8]) -> io::Result<Self> {
        Self::with_connector(
            limits,
            Protocol::Auto,
            connector(Some(pem), Protocol::Auto)?,
        )
    }

    fn with_connector(
        limits: Limits,
        protocol: Protocol,
        connector: Connector,
    ) -> io::Result<Self> {
        if limits.max_redirects > 32 {
            return Err(invalid("HTTP redirect limit cannot exceed 32"));
        }
        if limits.max_header_count > 1024 {
            return Err(invalid("HTTP header count limit cannot exceed 1024"));
        }
        for timeout in [
            limits.connect_timeout,
            limits.total_timeout,
            limits.read_timeout,
        ] {
            if timeout.is_zero()
                || timeout > Duration::from_secs(365 * 24 * 60 * 60)
                || std::time::Instant::now().checked_add(timeout).is_none()
            {
                return Err(invalid(
                    "HTTP timeouts must be nonzero and at most 365 days",
                ));
            }
        }
        let transport =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .pool_timer(hyper_util::rt::TokioTimer::new())
                .pool_max_idle_per_host(8)
                .http2_max_header_list_size(64 * 1024)
                .http2_only(protocol == Protocol::Http2Only)
                .build(TimedConnector {
                    inner: connector,
                    timeout: limits.connect_timeout,
                });
        Ok(Self { transport, limits })
    }

    /// Open a WebSocket over HTTP/1.1 without following redirects.
    ///
    /// The caller supplies a fresh handshake key and its expected accept value.
    /// The returned transport carries the raw WebSocket frames.
    pub async fn open_websocket(
        url: &str,
        key: &str,
        accept: &str,
        limits: Limits,
    ) -> io::Result<
        impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + std::fmt::Debug,
    > {
        use hyper::header::{
            CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_KEY,
            SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_VERSION, UPGRADE,
        };

        let client = Self::with_protocol(limits, Protocol::Http1Only)?;
        if url.len() > limits.max_url_bytes {
            return Err(invalid("WebSocket URL exceeds limit"));
        }
        let target = Target::parse(url)?;
        let mut request = client.build_request(
            &Request {
                headers: &[
                    (SEC_WEBSOCKET_KEY.as_str(), key),
                    (SEC_WEBSOCKET_VERSION.as_str(), "13"),
                ],
                ..Request::get(url)
            },
            &target,
        )?;
        request.headers_mut().insert(
            CONNECTION,
            hyper::header::HeaderValue::from_static("Upgrade"),
        );
        request.headers_mut().insert(
            UPGRADE,
            hyper::header::HeaderValue::from_static("websocket"),
        );
        let deadline = std::time::Instant::now() + limits.total_timeout;
        let mut response = client.send(request, deadline).await?;
        if response.status() != hyper::StatusCode::SWITCHING_PROTOCOLS {
            return Err(io::Error::other(format!(
                "WebSocket upgrade refused: {}",
                response.status()
            )));
        }
        let headers = response.headers();
        client.validate_response_headers(headers)?;
        let connection_upgrade = headers.get_all(CONNECTION).iter().any(|value| {
            value.to_str().is_ok_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            })
        });
        if !connection_upgrade
            || !headers
                .get(UPGRADE)
                .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
            || !headers
                .get(SEC_WEBSOCKET_ACCEPT)
                .is_some_and(|value| value == accept)
            || headers.contains_key(SEC_WEBSOCKET_EXTENSIONS)
            || headers.contains_key(SEC_WEBSOCKET_PROTOCOL)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid WebSocket upgrade response",
            ));
        }
        let budget = phase_budget(limits.read_timeout, deadline)?;
        let upgraded = tokio::time::timeout(budget, hyper::upgrade::on(&mut response))
            .await
            .map_err(|_| timed_out("WebSocket upgrade timed out"))?
            .map_err(transport)?;
        Ok(hyper_util::rt::TokioIo::new(upgraded))
    }

    /// Issue a GET, following redirects only when explicitly enabled in limits.
    /// Drop the future or response to release the in-flight operation.
    pub async fn get(&self, url: &str) -> io::Result<Response> {
        self.execute(Request::get(url)).await
    }

    /// POST a JSON value, adding the JSON content type unless the caller set one.
    #[cfg(feature = "json")]
    pub async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        value: &crate::json::Value,
    ) -> io::Result<Response> {
        let has_content_type = headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"));
        if headers.len().saturating_add(usize::from(!has_content_type))
            > self.limits.max_header_count
        {
            return Err(invalid("HTTP request exceeds limit"));
        }
        let mut headers = headers.to_vec();
        if !has_content_type {
            headers.push(("content-type", "application/json"));
        }
        let body = crate::json::encode(value, crate::json::Layout::Compact)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.execute(Request {
            method: Method::Post,
            url,
            headers: &headers,
            body: &body,
        })
        .await
    }

    /// Send a validated request. Status interpretation belongs to the caller.
    /// Request and response headers each have an independent metadata ceiling.
    pub async fn execute(&self, request: Request<'_>) -> io::Result<Response> {
        let deadline = std::time::Instant::now()
            .checked_add(self.limits.total_timeout)
            .ok_or_else(|| invalid("HTTP deadline cannot be represented"))?;
        let mut url = std::borrow::Cow::Borrowed(request.url);
        let mut headers = std::borrow::Cow::Borrowed(request.headers);
        let mut method = request.method;
        let mut body = request.body;
        for hop in 0..=self.limits.max_redirects {
            let response = self
                .execute_once(
                    Request {
                        method,
                        url: &url,
                        headers: &headers,
                        body,
                    },
                    deadline,
                )
                .await?;
            if self.limits.max_redirects == 0 || !matches!(response.status(), 301..=303 | 307..=308)
            {
                return Ok(response);
            }
            let Some(location) = response.header("location") else {
                return Ok(response);
            };
            if hop == self.limits.max_redirects {
                return Err(invalid("HTTP redirect limit exceeded"));
            }
            if location.len() > self.limits.max_url_bytes {
                return Err(invalid("HTTP redirect URL exceeds limit"));
            }
            let location =
                std::str::from_utf8(location).map_err(|_| invalid("invalid HTTP redirect URL"))?;
            let previous = &response.target;
            let joined = previous.join(location);
            let next = Target::parse(&joined)?;
            if previous.is_https() && !next.is_https() {
                return Err(invalid("HTTP redirect would downgrade HTTPS"));
            }
            if !next.same_origin(previous) {
                // A POST payload can contain credentials even when every
                // application header has been discarded. Following redirects
                // does not grant a second origin authority to receive it.
                if method == Method::Post && matches!(response.status(), 307 | 308) {
                    return Err(invalid("HTTP redirect would replay POST across origins"));
                }
                headers = std::borrow::Cow::Borrowed(&[]);
            }
            if method == Method::Post && matches!(response.status(), 301..=303) {
                method = Method::Get;
                body = &[];
                headers.to_mut().retain(|(name, _)| {
                    !name.eq_ignore_ascii_case("content-type")
                        && !name.eq_ignore_ascii_case("content-encoding")
                });
            }
            // Drop the intermediate body before the next hop. Every response
            // has already passed the same header/body-declaration limits.
            url = std::borrow::Cow::Owned(joined);
        }
        Err(invalid("HTTP redirect limit exceeded"))
    }

    fn build_request(
        &self,
        request: &Request<'_>,
        target: &Target,
    ) -> io::Result<hyper::Request<RequestBody>> {
        let method = match request.method {
            Method::Get => hyper::Method::GET,
            Method::Head => hyper::Method::HEAD,
            Method::Post => hyper::Method::POST,
        };
        let mut headers = hyper::HeaderMap::new();
        let mut remaining_request_headers = self.limits.max_header_bytes;
        for (name, value) in request.headers {
            remaining_request_headers = remaining_request_headers
                .checked_sub(name.len())
                .and_then(|remaining| remaining.checked_sub(value.len()))
                .ok_or_else(|| invalid("HTTP request headers exceed limit"))?;
            let name = hyper::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| invalid("invalid HTTP request header name"))?;
            if matches!(
                name.as_str(),
                "host"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
                    | "keep-alive"
                    | "proxy-connection"
                    | "upgrade"
                    | "trailer"
                    | "te"
                    | "expect"
                    | "proxy-authorization"
            ) {
                return Err(invalid(
                    "HTTP framing and proxy headers are transport-owned",
                ));
            }
            let value = hyper::header::HeaderValue::from_str(value)
                .map_err(|_| invalid("invalid HTTP request header value"))?;
            headers.append(name, value);
        }
        headers
            .entry(hyper::header::ACCEPT)
            .or_insert(hyper::header::HeaderValue::from_static("*/*"));
        let body = if request.method == Method::Post {
            bytes::Bytes::copy_from_slice(request.body)
        } else {
            bytes::Bytes::new()
        };
        let mut built = hyper::Request::new(http_body_util::Full::new(body));
        *built.method_mut() = method;
        *built.uri_mut() = target.request_uri()?;
        *built.headers_mut() = headers;
        Ok(built)
    }

    async fn send(
        &self,
        mut request: hyper::Request<RequestBody>,
        deadline: std::time::Instant,
    ) -> io::Result<hyper::Response<hyper::body::Incoming>> {
        let mut connection = hyper_util::client::legacy::connect::capture_connection(&mut request);
        let connect_budget = phase_budget(self.limits.connect_timeout, deadline)?;
        let response = self.transport.request(request);
        tokio::pin!(response);
        // Pool checkout signals once a reused or newly established connection
        // is ready. Poll the response as well to surface early transport errors.
        tokio::select! {
            biased;
            result = &mut response => return result.map_err(request_error),
            _ = connection.wait_for_connection_metadata() => {}
            () = tokio::time::sleep(connect_budget) => {
                return Err(timed_out("HTTP connect timed out"));
            }
        }
        let read_budget = phase_budget(self.limits.read_timeout, deadline)?;
        tokio::time::timeout(read_budget, response)
            .await
            .map_err(|_| timed_out("HTTP response headers timed out"))?
            .map_err(request_error)
    }

    fn validate_response_headers(&self, headers: &hyper::HeaderMap) -> io::Result<()> {
        if headers.len() > self.limits.max_header_count {
            return Err(invalid("HTTP response header count exceeds limit"));
        }
        let mut remaining_headers = self.limits.max_header_bytes;
        for (name, value) in headers {
            remaining_headers = remaining_headers
                .checked_sub(name.as_str().len())
                .and_then(|remaining| remaining.checked_sub(value.as_bytes().len()))
                .ok_or_else(|| invalid("HTTP response headers exceed limit"))?;
        }
        Ok(())
    }

    async fn execute_once(
        &self,
        request: Request<'_>,
        deadline: std::time::Instant,
    ) -> io::Result<Response> {
        if request.url.len() > self.limits.max_url_bytes
            || request.body.len() > self.limits.max_request_bytes
            || request.headers.len() > self.limits.max_header_count
        {
            return Err(invalid("HTTP request exceeds limit"));
        }
        if request.method != Method::Post && !request.body.is_empty() {
            return Err(invalid("only POST supports a request body"));
        }
        let target = Target::parse(request.url)?;
        let built = self.build_request(&request, &target)?;
        let inner = self.send(built, deadline).await?;
        let (parts, body) = inner.into_parts();
        self.validate_response_headers(&parts.headers)?;
        if request.method != Method::Head
            && hyper::body::Body::size_hint(&body)
                .exact()
                .is_some_and(|size| size > self.limits.max_body_bytes)
        {
            return Err(invalid("HTTP response body exceeds limit"));
        }
        Ok(Response {
            deadline,
            read_timeout: self.limits.read_timeout,
            status: parts.status.as_u16(),
            version: if parts.version == hyper::Version::HTTP_2 {
                Version::Http2
            } else if parts.version == hyper::Version::HTTP_10 {
                Version::Http10
            } else {
                Version::Http11
            },
            headers: parts.headers,
            body,
            target,
            remaining: self.limits.max_body_bytes,
            pending: bytes::Bytes::new(),
            failed: false,
            eof: false,
        })
    }
}

/// HTTP wire protocol reported by the peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    /// HTTP/1.0.
    Http10,
    /// HTTP/1.1.
    Http11,
    /// HTTP/2 negotiated over TLS.
    Http2,
}

/// The standard reason phrase for a numeric HTTP status, if assigned.
pub fn status_reason(status: u16) -> Option<&'static str> {
    hyper::StatusCode::from_u16(status).ok()?.canonical_reason()
}

/// An owned response data chunk. Borrow its bytes without copying.
#[derive(Clone, Debug)]
pub struct Chunk(bytes::Bytes);

impl AsRef<[u8]> for Chunk {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Owned response, with backend resources released when dropped.
pub struct Response {
    deadline: std::time::Instant,
    read_timeout: Duration,
    status: u16,
    version: Version,
    headers: hyper::HeaderMap,
    body: hyper::body::Incoming,
    target: Target,
    remaining: u64,
    pending: bytes::Bytes,
    failed: bool,
    eof: bool,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Response")
            .field("status", &self.status)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl Response {
    /// First response header value, looked up case-insensitively.
    /// Returned bytes are valid only while this response is borrowed.
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers.get(name).map(|value| value.as_bytes())
    }

    /// Declared Content-Length, if present and valid.
    pub fn content_length(&self) -> Option<u64> {
        std::str::from_utf8(self.header("content-length")?)
            .ok()?
            .parse()
            .ok()
    }

    /// Numeric HTTP status. Non-success status is not a transport error.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// HTTP version used for this response.
    pub fn version(&self) -> Version {
        self.version
    }

    /// Read at most the caller's buffer length, pulling only when needed.
    /// An empty buffer returns zero without reading; otherwise zero means EOF.
    /// Errors are terminal. Drop the response to release the body after an error.
    pub async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if !self.failed && buffer.is_empty() {
            return Ok(0);
        }
        self.fill_pending().await?;
        let n = buffer.len().min(self.pending.len());
        buffer[..n].copy_from_slice(&self.pending.split_to(n));
        Ok(n)
    }

    /// Take the next unread data chunk, or return `None` at EOF.
    /// Shares the read deadline and cumulative body limit with [Self::read].
    pub async fn next_chunk(&mut self) -> io::Result<Option<Chunk>> {
        self.fill_pending().await?;
        Ok((!self.pending.is_empty()).then(|| Chunk(std::mem::take(&mut self.pending))))
    }

    async fn fill_pending(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(invalid("HTTP response body is in a failed state"));
        }
        if !self.eof && std::time::Instant::now() >= self.deadline {
            self.failed = true;
            self.pending = bytes::Bytes::new();
            return Err(timed_out("HTTP total deadline expired"));
        }
        while self.pending.is_empty() && !self.eof {
            if let Err(error) = self.pull().await {
                self.failed = true;
                return Err(error);
            }
        }
        Ok(())
    }

    /// Wait for the next body frame under the read and total deadlines.
    /// Trailers are parsed by the transport but not exposed.
    async fn pull(&mut self) -> io::Result<()> {
        let budget = phase_budget(self.read_timeout, self.deadline)?;
        match tokio::time::timeout(budget, self.body.frame()).await {
            Err(_) => Err(timed_out("HTTP response read timed out")),
            Ok(None) => {
                self.eof = true;
                Ok(())
            }
            Ok(Some(Err(error))) => Err(transport(error)),
            Ok(Some(Ok(frame))) => {
                if let Ok(chunk) = frame.into_data() {
                    self.remaining = self
                        .remaining
                        .checked_sub(chunk.len() as u64)
                        .ok_or_else(|| invalid("HTTP response body exceeds limit"))?;
                    self.pending = chunk;
                }
                Ok(())
            }
        }
    }

    /// Consume the unread part of a small response under its byte ceiling.
    /// No declared length is trusted for allocation or stream completion.
    pub async fn into_bytes(self) -> io::Result<Vec<u8>> {
        self.collect(usize::MAX).await
    }

    /// Collect and decode the unread response body using the JSON facade's limits.
    #[cfg(feature = "json")]
    pub async fn into_json(self) -> io::Result<crate::json::Value> {
        let body = self.collect(crate::json::MAX_INPUT_BYTES).await?;
        crate::json::parse(&body).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    async fn collect(mut self, max_bytes: usize) -> io::Result<Vec<u8>> {
        let mut body = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            let chunk = chunk.as_ref();
            if chunk.len() > max_bytes.saturating_sub(body.len()) {
                return Err(invalid("HTTP response body exceeds collection limit"));
            }
            body.try_reserve(chunk.len()).map_err(io::Error::other)?;
            body.extend_from_slice(chunk);
        }
        Ok(body)
    }
}
