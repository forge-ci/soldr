//! Bounded native HTTP/1 serving on the caller's runtime.
//!
//! Applications own routing, authorization and response policy. Dropping the
//! serving future closes the listener and cancels all owned connections.

use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
#[cfg(feature = "websocket")]
use std::pin::Pin;
use std::{convert::Infallible, future::Future, io, net::SocketAddr, sync::Arc, time::Duration};

#[cfg(feature = "websocket")]
mod websocket;
#[cfg(feature = "websocket")]
pub use websocket::{
    Message as WebSocketMessage, Upgrade as WebSocketUpgrade, WebSocket, WebSocketLimits,
};

mod body;
mod diagnostics;
#[cfg(all(test, any(feature = "http-client", feature = "http-client-rustls")))]
mod h2_tests;
mod target;
mod transport;
use body::ServerBody;
use diagnostics::increment;
pub use diagnostics::{Diagnostics, Snapshot};
pub use target::QueryPairs;

/// One server-sent event. `id` is the reconnect cursor and `event` is its type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SseEvent {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
}

#[cfg(feature = "websocket")]
type UpgradeTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Shared bounded native preparation of file responses. Clones share admission;
/// use one instance for a server's routes, not one instance per request.
#[derive(Clone, Debug)]
pub struct FileResponses {
    budget: body::ReadBudget,
    timeout: Duration,
}

impl FileResponses {
    /// Configure 1..=64 outstanding preparations and a positive deadline up to
    /// one day. No runtime is constructed.
    ///
    /// # Errors
    /// Invalid limits return `InvalidInput`.
    pub fn new(max_operations: usize, timeout: Duration) -> io::Result<Self> {
        if !(1..=64).contains(&max_operations)
            || timeout.is_zero()
            || timeout > Duration::from_secs(86400)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid file preparation limits",
            ));
        }
        Ok(Self {
            budget: body::ReadBudget::new(max_operations),
            timeout,
        })
    }

    /// Inspect and open a caller-authorized regular file, preparing its streamed
    /// response off the async executor. Prefixes are limited to 1 MiB. Paths may
    /// follow symlinks; callers still own authorization and directory policy.
    /// Cancellation/deadlines cannot interrupt an in-flight native call: its
    /// permit remains held until completion. Saturation fails without queuing.
    ///
    /// # Errors
    /// Reports native failures, non-files/oversize prefixes (`InvalidInput`),
    /// capacity exhaustion (`WouldBlock`), deadlines (`TimedOut`), and missing
    /// runtime context. Requires timers enabled on the caller's runtime.
    pub async fn open(&self, path: std::path::PathBuf, prefix: Vec<u8>) -> io::Result<Response> {
        crate::async_engine::RuntimeHandle::current().map_err(io::Error::other)?;
        if prefix.len() > 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file prefix exceeds limit",
            ));
        }
        let task = self.budget.spawn(move || {
            // Avoid opening known FIFOs/devices. A concurrent path replacement
            // can still block open, so worker-held admission remains essential.
            if !std::fs::metadata(&path)?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "response path is not a regular file",
                ));
            }
            Response::file(std::fs::File::open(path)?, prefix)
        })?;
        tokio::time::timeout(self.timeout, task)
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "file preparation deadline expired")
            })?
            .map_err(io::Error::other)?
    }
}

/// Per-server resource and time limits. Body limits bound accepted values, not
/// memory already allocated by application handlers before returning a response.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum concurrently owned connections. The accept loop waits at capacity.
    /// Also bounds outstanding native file-read workers in a separate shared
    /// budget. A cancelled connection's native read retains its read slot until
    /// the OS call finishes; new file reads fail when this budget is exhausted.
    pub max_connections: usize,
    /// Maximum collected request body bytes.
    pub max_request_body_bytes: usize,
    /// Maximum accepted in-memory response body bytes.
    pub max_response_body_bytes: usize,
    /// Maximum file bytes (including an optional prefix), streamed rather than collected.
    pub max_file_bytes: u64,
    /// Maximum response frame size, from 1024 through 65536 bytes.
    pub max_stream_chunk_bytes: usize,
    /// Maximum UTF-8 SSE payload bytes before encoding (encoding adds bounded overhead).
    pub max_event_bytes: usize,
    /// HTTP/1 parser read-buffer bound (at least 8192 bytes).
    pub max_header_bytes: usize,
    /// Maximum parsed header count.
    pub max_headers: usize,
    /// Maximum accepted application response header entries, counting duplicates.
    pub max_response_headers: usize,
    /// Maximum application header bytes: name + value + four framing bytes per
    /// entry. Excludes transport-generated headers and the status line; this is
    /// an acceptance limit, not a limit on prior application allocations.
    pub max_response_header_bytes: usize,
    /// Maximum time to receive each request's headers.
    pub header_timeout: Duration,
    /// Maximum time to collect a request body.
    pub body_timeout: Duration,
    /// Maximum time for application response preparation.
    pub handler_timeout: Duration,
    /// Maximum time an attempted socket write/flush may remain without progress.
    pub write_timeout: Duration,
    /// Absolute lifetime of a connection, including response transmission.
    pub connection_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 64,
            max_request_body_bytes: 2 * 1024 * 1024,
            max_response_body_bytes: 64 * 1024 * 1024,
            max_file_bytes: 16 * 1024 * 1024 * 1024,
            max_stream_chunk_bytes: 64 * 1024,
            max_event_bytes: 64 * 1024,
            max_header_bytes: 32 * 1024,
            max_headers: 100,
            max_response_headers: 100,
            max_response_header_bytes: 32 * 1024,
            header_timeout: Duration::from_secs(10),
            body_timeout: Duration::from_secs(30),
            handler_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            connection_timeout: Duration::from_secs(3600),
        }
    }
}

impl Limits {
    fn validate(self) -> io::Result<()> {
        if self.max_connections == 0
            || self.max_connections > 65_536
            || !(8192..=1024 * 1024).contains(&self.max_header_bytes)
            || !(1..=1024).contains(&self.max_headers)
            || self.max_response_headers > 1024
            || self.max_response_header_bytes > 1024 * 1024
            || self.max_request_body_bytes > 1024 * 1024 * 1024
            || self.max_response_body_bytes > 1024 * 1024 * 1024
            || self.max_file_bytes > 1024 * 1024 * 1024 * 1024
            || !(1024..=65536).contains(&self.max_stream_chunk_bytes)
            || !(1..=1024 * 1024).contains(&self.max_event_bytes)
            || [
                self.header_timeout,
                self.body_timeout,
                self.handler_timeout,
                self.write_timeout,
                self.connection_timeout,
            ]
            .into_iter()
            .any(|d| d.is_zero() || d > Duration::from_secs(365 * 24 * 3600))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid HTTP server limits",
            ));
        }
        Ok(())
    }
}

/// A fully collected, bounded request; no implementation body type escapes.
#[derive(Debug)]
pub struct Request {
    method: String,
    #[cfg(feature = "websocket")]
    http_1_1: bool,
    target: String,
    uri: hyper::Uri,
    headers: hyper::HeaderMap,
    body: Vec<u8>,
    #[cfg(feature = "websocket")]
    upgrade: Option<hyper::upgrade::OnUpgrade>,
}

impl Request {
    /// HTTP method token.
    pub fn method(&self) -> &str {
        &self.method
    }
    /// Original request target including an optional query string.
    pub fn target(&self) -> &str {
        &self.target
    }
    /// Encoded URI path, without the query. No percent decoding or normalization.
    pub fn path(&self) -> &str {
        self.uri.path()
    }
    /// Decode the path once as UTF-8, preserving literal `+`. This does not
    /// authorize filesystem access or normalize dots, slashes, backslashes or NUL.
    ///
    /// # Errors
    /// Returns `InvalidData` for malformed escapes or non-UTF-8 bytes.
    pub fn decoded_path(&self) -> io::Result<String> {
        target::decode(self.path(), false)
    }
    /// Encoded query without `?`; absent and present-but-empty remain distinct.
    pub fn query(&self) -> Option<&str> {
        self.uri.query()
    }
    /// Iterate decoded form-query pairs without collecting them. Input size is
    /// bounded by request parsing; each decoded field is no larger than its
    /// encoded bytes. Applications own duplicate and unknown-parameter policy.
    pub fn query_pairs(&self) -> QueryPairs<'_> {
        QueryPairs::new(self.query().unwrap_or(""))
    }
    /// First matching header's raw bytes; names are case-insensitive.
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers.get(name).map(|value| value.as_bytes())
    }
    /// Accepted body bytes.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Consume this request as an RFC 6455 WebSocket upgrade. Validate route,
    /// origin, host and authorization before calling this method. The returned
    /// value owns the upgrade future; it cannot be reused as an HTTP request.
    #[cfg(feature = "websocket")]
    pub fn into_websocket(self) -> io::Result<WebSocketUpgrade> {
        websocket::Upgrade::from_request(self)
    }
}

/// An application-selected HTTP response with validated status and headers.
pub struct Response {
    status: hyper::StatusCode,
    headers: hyper::HeaderMap,
    body: ServerBody,
    #[cfg(feature = "websocket")]
    upgrade_task: Option<UpgradeTask>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &self.body)
            .finish_non_exhaustive()
    }
}

impl Default for Response {
    /// An empty 500 response for fallible application response preparation.
    fn default() -> Self {
        Self {
            status: hyper::StatusCode::INTERNAL_SERVER_ERROR,
            headers: hyper::HeaderMap::new(),
            body: ServerBody::bytes(Bytes::new()),
            #[cfg(feature = "websocket")]
            upgrade_task: None,
        }
    }
}

impl Response {
    #[cfg(feature = "websocket")]
    pub(super) fn websocket_upgrade(accept: &str, upgrade_task: UpgradeTask) -> Self {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::CONNECTION,
            hyper::header::HeaderValue::from_static("Upgrade"),
        );
        headers.insert(
            hyper::header::UPGRADE,
            hyper::header::HeaderValue::from_static("websocket"),
        );
        headers.insert(
            hyper::header::HeaderName::from_static("sec-websocket-accept"),
            hyper::header::HeaderValue::from_str(accept)
                .expect("derived WebSocket accept key is valid"),
        );
        Self {
            status: hyper::StatusCode::SWITCHING_PROTOCOLS,
            headers,
            body: ServerBody::bytes(Bytes::new()),
            upgrade_task: Some(upgrade_task),
        }
    }

    /// Construct a response. The server separately enforces its body-size limit.
    ///
    /// # Errors
    /// Rejects status values outside 200..=599; interim responses are transport-owned.
    /// Statuses 204, 205 and 304 require an empty body.
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> io::Result<Self> {
        if !(200..=599).contains(&status) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid final HTTP status",
            ));
        }
        let body = body.into();
        if matches!(status, 204 | 205 | 304) && !body.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "HTTP status does not permit response content",
            ));
        }
        Ok(Self {
            status: hyper::StatusCode::from_u16(status).map_err(io::Error::other)?,
            headers: hyper::HeaderMap::new(),
            body: ServerBody::bytes(Bytes::from(body)),
            #[cfg(feature = "websocket")]
            upgrade_task: None,
        })
    }

    /// Stream an already-opened regular file from its current position to its
    /// current length, optionally preceded by `prefix`. The caller owns file
    /// selection/authorization. Later growth is ignored; truncation is an error.
    ///
    /// Metadata and position inspection are synchronous native operations. File
    /// reads are asynchronous and bounded; cancelling does not forcibly interrupt
    /// an OS read already running in the runtime's blocking I/O pool.
    ///
    /// # Errors
    /// Rejects non-regular files, prefixes larger than 1 MiB, and native metadata
    /// or position failures. Server acceptance also applies its configured limits.
    pub fn file(file: std::fs::File, prefix: impl Into<Vec<u8>>) -> io::Result<Self> {
        let mut response = Self::new(200, Vec::new())?;
        response.body = ServerBody::file(file, prefix.into())?;
        Ok(response)
    }

    /// Encode a pull-driven sequence of SSE data events. Keepalives are comments;
    /// the next event is polled only when the transport requests another frame.
    /// Dropping the response drops its source. No producer task or queue is added.
    ///
    /// # Errors
    /// Rejects zero or greater-than-365-day keepalive periods and missing runtime
    /// context. The runtime must have its timer driver enabled.
    pub fn event_stream<S>(events: S, keepalive: Duration) -> io::Result<Self>
    where
        S: futures_core::Stream<Item = io::Result<String>> + Send + 'static,
    {
        let mut response = Self::new(200, Vec::new())?
            .with_header("content-type", "text/event-stream")?
            .with_header("cache-control", "no-cache")?;
        response.body = ServerBody::events(events, keepalive)?;
        Ok(response)
    }

    /// Encode SSE events with optional native `id:` and `event:` fields.
    /// The source remains pull-driven, and dropping the response drops it.
    /// Invalid field values cause a stream error before that event is sent.
    ///
    /// # Errors
    /// Rejects invalid keepalive periods or missing runtime context.
    pub fn sse_stream<S>(events: S, keepalive: Duration) -> io::Result<Self>
    where
        S: futures_core::Stream<Item = io::Result<SseEvent>> + Send + 'static,
    {
        let mut response = Self::new(200, Vec::new())?
            .with_header("content-type", "text/event-stream")?
            .with_header("cache-control", "no-cache")?;
        response.body = ServerBody::sse_events(events, keepalive)?;
        Ok(response)
    }

    /// Append an application header. Framing is always transport-owned.
    ///
    /// # Errors
    /// Rejects invalid syntax and connection/body-framing headers.
    pub fn with_header(mut self, name: &str, value: &str) -> io::Result<Self> {
        let name =
            hyper::header::HeaderName::from_bytes(name.as_bytes()).map_err(io::Error::other)?;
        if matches!(
            name.as_str(),
            "content-length"
                | "transfer-encoding"
                | "connection"
                | "upgrade"
                | "trailer"
                | "keep-alive"
                | "proxy-connection"
                | "te"
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "HTTP framing header is transport-owned",
            ));
        }
        let value = hyper::header::HeaderValue::from_str(value).map_err(io::Error::other)?;
        self.headers.append(name, value);
        Ok(self)
    }
}

/// A bound listener. Serving uses the current runtime and never starts another.
#[derive(Debug)]
pub struct Server {
    listener: tokio::net::TcpListener,
    limits: Limits,
    diagnostics: Diagnostics,
    response_headers: Arc<hyper::HeaderMap>,
    read_budget: body::ReadBudget,
}

impl Server {
    /// Bind a caller-selected address after validating limits.
    ///
    /// # Errors
    /// Reports invalid limits and native listener errors.
    pub async fn bind(address: SocketAddr, limits: Limits) -> io::Result<Self> {
        limits.validate()?;
        Ok(Self {
            listener: tokio::net::TcpListener::bind(address).await?,
            limits,
            diagnostics: Diagnostics::default(),
            response_headers: Arc::new(hyper::HeaderMap::new()),
            read_budget: body::ReadBudget::new(limits.max_connections),
        })
    }

    /// The bound address, including the assigned port when zero was requested.
    ///
    /// # Errors
    /// Reports a native socket inspection failure.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Shared, fixed-size counters; clone before moving this server into `serve`.
    pub fn diagnostics(&self) -> Diagnostics {
        self.diagnostics.clone()
    }

    /// Set an application-selected header on every dispatched response, replacing
    /// handler values with the same name. Includes body rejection and handler
    /// timeout responses. Does not affect errors generated by the HTTP parser
    /// before dispatch or a connection that closes without a response.
    ///
    /// No CORS policy is inferred: applications select origins, methods and
    /// preflight handling. Both configured and merged fields obey response limits.
    ///
    /// # Errors
    /// Rejects invalid/framing headers and configured headers exceeding limits.
    pub fn with_response_header(mut self, name: &str, value: &str) -> io::Result<Self> {
        let validated = Response::new(200, Vec::new())?.with_header(name, value)?;
        for (name, value) in &validated.headers {
            Arc::make_mut(&mut self.response_headers).insert(name.clone(), value.clone());
        }
        if !headers_fit(&self.response_headers, self.limits) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "server response headers exceed limits",
            ));
        }
        Ok(self)
    }

    /// Serve until cancelled or a listener error occurs. Connection tasks are
    /// owned by this future and cancelled on drop, including incomplete requests.
    ///
    /// A connection that upgrades to a WebSocket outlives its HTTP exchange:
    /// the connection task is held while the handed-off socket is in use, so it
    /// counts against `max_connections` and is not subject to
    /// `connection_timeout`, which bounds only the handshake. The upgraded
    /// connection's lifetime belongs to the handler that received it.
    ///
    /// # Errors
    /// Returns native listener errors; individual client failures are isolated.
    pub async fn serve<H, F>(self, handler: H) -> io::Result<()>
    where
        H: Fn(Request) -> F + Clone + Send + Sync + 'static,
        F: Future<Output = Response> + Send + 'static,
    {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                result = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(Err(_)) = result {
                        increment(&self.diagnostics.0.task_failures);
                    }
                }
                accepted = self.listener.accept(), if tasks.len() < self.limits.max_connections => {
                    let (socket, _) = accepted?;
                    increment(&self.diagnostics.0.accepted_connections);
                    let handler = handler.clone();
                    let limits = self.limits;
                    let diagnostics = self.diagnostics.clone();
                    let response_headers = self.response_headers.clone();
                    let read_budget = self.read_budget.clone();
                    tasks.spawn(async move {
                        let request_diagnostics = diagnostics.clone();
                        #[cfg(feature = "websocket")]
                        let (upgrades_tx, mut upgrades_rx) = tokio::sync::mpsc::unbounded_channel();
                        let service = service_fn(move |request| {
                            dispatch(
                                request,
                                handler.clone(),
                                limits,
                                request_diagnostics.clone(),
                                response_headers.clone(),
                                read_budget.clone(),
                                #[cfg(feature = "websocket")]
                                upgrades_tx.clone(),
                            )
                        });
                        let mut builder = hyper::server::conn::http1::Builder::new();
                        builder.timer(TokioTimer::new())
                            .header_read_timeout(limits.header_timeout)
                            .max_buf_size(limits.max_header_bytes)
                            .max_headers(limits.max_headers);
                        let socket = transport::ProgressIo::new(socket, limits.write_timeout);
                        let connection = builder
                            .serve_connection(TokioIo::new(socket), service)
                            .with_upgrades();
                        #[cfg(feature = "websocket")]
                        let mut upgrade_tasks = tokio::task::JoinSet::new();
                        #[cfg(feature = "websocket")]
                        let outcome = tokio::time::timeout(limits.connection_timeout, async {
                            tokio::pin!(connection);
                            loop {
                                tokio::select! {
                                    result = &mut connection => break result,
                                    Some(task) = upgrades_rx.recv() => { upgrade_tasks.spawn(task); }
                                    Some(result) = upgrade_tasks.join_next(), if !upgrade_tasks.is_empty() => {
                                        if result.is_err() { increment(&diagnostics.0.task_failures); }
                                    }
                                }
                            }
                        }).await;
                        #[cfg(not(feature = "websocket"))]
                        let outcome = tokio::time::timeout(limits.connection_timeout, connection).await;
                        #[cfg(feature = "websocket")]
                        {
                            // A successful upgrade ends the HTTP connection while
                            // the socket it handed off is still in use: with
                            // `with_upgrades()`, hyper resolves this future as
                            // soon as the `101` is written. The connection
                            // budget therefore bounds the handshake, not the
                            // lifetime of the upgraded connection, so remaining
                            // upgrade tasks are drained and joined rather than
                            // aborted. Aborting here killed every live upgrade
                            // the moment it succeeded.
                            upgrades_rx.close();
                            while let Ok(task) = upgrades_rx.try_recv() {
                                upgrade_tasks.spawn(task);
                            }
                            while let Some(result) = upgrade_tasks.join_next().await {
                                if result.is_err() {
                                    increment(&diagnostics.0.task_failures);
                                }
                            }
                        }
                        match outcome {
                            Err(_) => increment(&diagnostics.0.connection_timeouts),
                            Ok(Err(_)) => increment(&diagnostics.0.connection_errors),
                            Ok(Ok(())) => increment(&diagnostics.0.completed_connections),
                        }
                    });
                }
            }
        }
    }
}

fn empty(status: hyper::StatusCode) -> hyper::Response<ServerBody> {
    let mut response = hyper::Response::new(ServerBody::bytes(Bytes::new()));
    *response.status_mut() = status;
    response
}

async fn dispatch<H, F>(
    request: hyper::Request<Incoming>,
    handler: H,
    limits: Limits,
    diagnostics: Diagnostics,
    response_headers: Arc<hyper::HeaderMap>,
    read_budget: body::ReadBudget,
    #[cfg(feature = "websocket")] upgrades_tx: tokio::sync::mpsc::UnboundedSender<UpgradeTask>,
) -> Result<hyper::Response<ServerBody>, Infallible>
where
    H: Fn(Request) -> F,
    F: Future<Output = Response>,
{
    let mut result = dispatch_inner(
        request,
        handler,
        limits,
        diagnostics.clone(),
        #[cfg(feature = "websocket")]
        upgrades_tx,
    )
    .await?;
    result.body_mut().set_read_budget(read_budget);
    for (name, value) in response_headers.iter() {
        result.headers_mut().insert(name.clone(), value.clone());
    }
    if !headers_fit(result.headers(), limits) {
        increment(&diagnostics.0.response_rejections);
        result = empty(hyper::StatusCode::INTERNAL_SERVER_ERROR);
        *result.headers_mut() = (*response_headers).clone();
    }
    Ok(result)
}

fn headers_fit(headers: &hyper::HeaderMap, limits: Limits) -> bool {
    let bytes = headers.iter().try_fold(0usize, |total, (name, value)| {
        total
            .checked_add(name.as_str().len())?
            .checked_add(value.as_bytes().len())?
            .checked_add(4)
    });
    headers.len() <= limits.max_response_headers
        && bytes.is_some_and(|bytes| bytes <= limits.max_response_header_bytes)
}

async fn dispatch_inner<H, F>(
    request: hyper::Request<Incoming>,
    handler: H,
    limits: Limits,
    diagnostics: Diagnostics,
    #[cfg(feature = "websocket")] upgrades_tx: tokio::sync::mpsc::UnboundedSender<UpgradeTask>,
) -> Result<hyper::Response<ServerBody>, Infallible>
where
    H: Fn(Request) -> F,
    F: Future<Output = Response>,
{
    #[cfg(feature = "websocket")]
    let mut request = request;
    #[cfg(feature = "websocket")]
    let upgrade = hyper::upgrade::on(&mut request);
    let (parts, body) = request.into_parts();
    let collected = tokio::time::timeout(
        limits.body_timeout,
        Limited::new(body, limits.max_request_body_bytes).collect(),
    )
    .await;
    let body = match collected {
        Err(_) => {
            increment(&diagnostics.0.body_timeouts);
            return Ok(empty(hyper::StatusCode::REQUEST_TIMEOUT));
        }
        Ok(Err(error)) => {
            increment(&diagnostics.0.request_rejections);
            return Ok(empty(if error.is::<http_body_util::LengthLimitError>() {
                hyper::StatusCode::PAYLOAD_TOO_LARGE
            } else {
                hyper::StatusCode::BAD_REQUEST
            }));
        }
        Ok(Ok(body)) => body.to_bytes().to_vec(),
    };
    let request = Request {
        method: parts.method.to_string(),
        #[cfg(feature = "websocket")]
        http_1_1: parts.version == hyper::Version::HTTP_11,
        target: parts.uri.to_string(),
        uri: parts.uri,
        headers: parts.headers,
        body,
        #[cfg(feature = "websocket")]
        upgrade: Some(upgrade),
    };
    let mut response = match tokio::time::timeout(limits.handler_timeout, handler(request)).await {
        Ok(response) => response,
        Err(_) => {
            increment(&diagnostics.0.handler_timeouts);
            return Ok(empty(hyper::StatusCode::GATEWAY_TIMEOUT));
        }
    };
    #[cfg(feature = "websocket")]
    if let Some(upgrade_task) = response.upgrade_task.take() {
        let _ = upgrades_tx.send(upgrade_task);
    }
    if !headers_fit(&response.headers, limits) || response.body.configure(limits).is_err() {
        increment(&diagnostics.0.response_rejections);
        return Ok(empty(hyper::StatusCode::INTERNAL_SERVER_ERROR));
    }
    let mut result = hyper::Response::new(response.body);
    *result.status_mut() = response.status;
    *result.headers_mut() = response.headers;
    Ok(result)
}
