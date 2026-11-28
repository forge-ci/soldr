//! Bounded-memory response-body downloads for release and toolchain assets.
//!
//! Metadata requests intentionally retain their short response-wide deadlines.
//! Archive payloads instead use this module: a successful chunk resets the
//! idle timer, so a healthy multi-gigabyte transfer is not killed by elapsed
//! wall time, while a stalled or truncated response remains retryable.
//!
//! ## Segmented downloads (setup-soldr `feat/segmented-download-experiment`, soldr#2320)
//!
//! `stream_response_to_temp_file[_with_safety_timeout]` transparently tries
//! N-way HTTP Range segmentation before falling back to the single-stream
//! loop below. This is a deliberate design choice, not an afterthought: it
//! centralizes HTTP request construction and download policy
//! (`dylints/ban_raw_network_access` guards direct network access
//! under `crates/soldr-fetch/src/`), so folding segmentation in here
//! is what lets every existing caller — `syslib_common.rs`, `xwin_cache.rs`,
//! `archive.rs`, `llvm.rs`, `zig.rs`, `apple_sdk.rs`, `rustup_init.rs`,
//! `manifest_lookup.rs` — inherit it with **zero call-site changes**. They
//! keep calling `get_request` + `send_asset_request` +
//! `stream_response_to_temp_file` exactly as before.
//!
//! The trick: by the time a caller's already-sent `response` reaches this
//! function, its headers (`Content-Length`, `Accept-Ranges`) are already
//! available for free — no extra probe request needed. If they indicate a
//! large, range-capable resource, this module abandons that response
//! (its body is never read) and issues its own N parallel `Range` requests
//! through a freshly built client. If segmentation was never attempted, or
//! it fails for any reason, the ORIGINAL `response` — never drained, still
//! perfectly valid — is what gets streamed by the existing single-stream
//! loop below. That is the whole fallback-safety argument: segmentation can
//! only ever add a path to success, never remove the one that already
//! worked.
//!
//! ### Why default-on
//!
//! `crates/soldr-fetch/examples/dl_bench.rs` measured single-stream vs
//! segmented downloads against the real CDNs this module talks to
//! (media.githubusercontent.com LFS-style assets, GitHub release
//! signed-URL redirects): single-stream was capped at a strikingly
//! consistent ~2-2.3 MB/s on every origin tested, while segmentation
//! scaled with no plateau through N=16, landing at **8.6-10.3x** the
//! single-stream throughput and matching `aria2c -x16` within noise on
//! every URL. That is not a marginal, opt-in-only win — it is the
//! difference between the maintainer's named xwin MSVCRT pain point and a
//! four-second download. Combined with the fallback-safety argument above
//! (a segmentation failure can never make a download that used to succeed
//! start failing), the default is ON, with an opt-OUT escape hatch for the
//! rare host/proxy where it turns out not to help.
//!
//! ### Protocol inheritance (Http1Only vs Negotiated)
//!
//! Several callers pin `AssetProtocol::Http1Only` for their single-stream
//! client (`syslib_common.rs`, `llvm.rs`, `zig.rs`, `apple_sdk.rs`) as a
//! defensive fix for historical large-binary-download stalls (see
//! `git log -S http1_only` starting at PR #963); `xwin_cache.rs` never
//! carried that pin and has shipped without it since PR #1016 with no
//! reported incident. Because this module builds its OWN client for the
//! segmented request fan-out (it cannot reuse the caller's client — only
//! the caller's already-completed `response` is available), it has no
//! direct way to see which protocol the caller originally pinned. Instead
//! it infers the safe choice from `response.version()`: if the caller's
//! single connection negotiated HTTP/1.x (either because it was pinned, or
//! because that is what the server offered), the segmented fan-out also
//! pins HTTP/1-only; if it negotiated HTTP/2+, the segmented fan-out lets
//! the client negotiate freely too. This preserves every existing pin's
//! intent without any caller needing to pass its protocol choice through —
//! "whatever pin stream_download applies stays owned by stream_download."
//! `dl_bench.rs` separately confirmed empirically that Negotiated works
//! fine against all three benchmarked origins (including the exact mingw
//! bundle whose PR introduced `syslib_common.rs`'s pin), so this inference
//! is conservative-by-construction rather than load-bearing for
//! correctness today.
//!
//! ### Three clocks per segment attempt
//!
//! 1. **Connect/TTFB** (`SOLDR_DOWNLOAD_CONNECT_TIMEOUT_SECS`, default 10s):
//!    request issuance to first response byte (i.e. until headers are
//!    available). Re-armed fresh on every redirect hop — the client's
//!    auto-redirect would share one `.send()` future across every hop,
//!    making a per-hop timeout impossible, so the segmented client
//!    disables auto-redirect (`redirect::Policy::none()`) and follows
//!    hops manually (`send_with_hop_timeout`), each with its own
//!    `tokio::time::timeout`. A connect/TTFB expiry is not fatal to the
//!    segment — it goes through the normal per-segment retry path.
//! 2. **Stall watchdog** (`SOLDR_DOWNLOAD_STALL_TIMEOUT_SECS`, default
//!    30s): arms only once the body-chunk loop starts (i.e. after clock 1
//!    already ended successfully) and resets on every chunk. A slow TLS
//!    handshake or a server that takes a while to start streaming is
//!    clock 1's problem, never clock 2's.
//! 3. **Whole-operation deadline** (`SOLDR_DOWNLOAD_TIMEOUT_SECS`,
//!    disabled by default): spans every phase of one `stream_response_to_
//!    temp_file` call, segmented attempt and single-stream fallback alike.
//!
//! Permit-wait time (see "Two connection pools" below) counts against
//! NONE of these: a segment acquires its pool permit FIRST, and only then
//! starts clock 1. Otherwise a busy pool would manufacture spurious
//! connect-timeout failures purely from FIFO queueing delay.
//!
//! ### Two connection pools (process-wide, soldr#2320 addendum 3)
//!
//! Two independent, non-borrowing pools bound total concurrent payload
//! connections:
//!
//! * **Bulk** (`SOLDR_DOWNLOAD_MAX_SOCKETS`, default 16): every segment of
//!   a segmented download, plus the single-stream fallback for anything
//!   ABOVE the quick threshold. Clocks, retries, and preemption all live
//!   here.
//! * **Quick** (`SOLDR_DOWNLOAD_QUICK_POOL`, default 4): plain FIFO, no
//!   preemption. Used for (a) every control-plane request (catalogue
//!   JSON, API probes, redirect-resolution HEADs) and (b) whole downloads
//!   whose `Content-Length` is known and at or below
//!   `SOLDR_DOWNLOAD_QUICK_THRESHOLD_BYTES` (default 4 MiB) — the same
//!   threshold also doubles as the segmentation minimum, so anything
//!   small enough to route through Quick is also never segmented; there
//!   is exactly one size boundary, not two independently-tunable ones.
//!   A response with unknown size (no `Content-Length`) always routes to
//!   Bulk — conservative, since Quick's small pool would otherwise be an
//!   easy target for an unbounded transfer to starve.
//!
//! There is no cross-pool borrowing; the hard ceiling on total concurrent
//! payload+control connections is `bulk + quick`. The motivating scenario
//! this exists for: `soldr prepare` materializing several large managed
//! bundles (cmake, ninja, LLVM, xwin) saturates Bulk with long-lived
//! segment streams, while a small catalogue/manifest lookup on Quick must
//! still complete promptly instead of queueing behind them.
//!
//! ### Permit preemption ("work stealing", soldr#2320 addendum 2, Bulk only)
//!
//! A Bulk permit is PENDING from acquisition until the holder's connect
//! phase (clock 1, across every redirect hop) finishes and the first
//! payload byte is about to be read; from there it is STREAMING and can
//! never be preempted. Quick permits skip this distinction entirely —
//! they are marked STREAMING immediately on acquisition, since Quick
//! never preempts. A background scheduler tick (~1s) steals a PENDING
//! Bulk permit for a FIFO waiter only when: a waiter exists right now,
//! the victim has been PENDING for at least a ~2s grace period, and the
//! victim has been preempted fewer than 2 times (the anti-livelock guard
//! — past that it becomes permanently non-preemptible so a uniformly slow
//! network still converges instead of cycling forever). Preemption is not
//! a failure: the victim's segment re-queues FIFO for a fresh permit, its
//! retry budget is untouched, and preemptions are counted separately from
//! retries. Tail hedging (duplicate the slowest pending segment onto an
//! idle permit, first byte wins) is explicitly out of scope here — it did
//! not fall out cleanly from this state model without materially more
//! machinery, so it is left as a documented follow-up rather than rushed.
//!
//! ### Deferred (soldr#2320 addendum 4) -- recorded, not implemented here
//!
//! - **Scheduler-task lifecycle**: [`SocketPool::new`] spawns its
//!   preemption-tick task with no shutdown/cancellation handle; it lives
//!   as long as the process (or, in tests, as long as the owning
//!   single-threaded runtime). Fine for the one process-wide Bulk/Quick
//!   singleton; would need a handle if this ever became a per-request
//!   object.
//! - **`spawn_blocking` for segment writes**: `write_at`/`write_at_all`
//!   run synchronous positional file I/O directly on the async task
//!   rather than via `tokio::task::spawn_blocking`. Fine for the small,
//!   fast local writes this does today; revisit if segment counts or
//!   write sizes grow enough for this to matter.
//! - **N-knee**: phase 1 measured no plateau through N=16 (the largest
//!   tested); whether higher N keeps helping is unprobed.
//! - **Caller-side probe tokens**: [`AUTH_TOKEN_ENV_VAR`] is forward-prep
//!   only -- there is no probe-then-reuse-a-resolved-URL path (each
//!   request, including every redirect hop, resolves independently; see
//!   "Cross-origin redirects strip Authorization" below for why that
//!   matters for auth specifically).
//!
//! ### Cross-origin redirects strip `Authorization` (soldr#2320 addendum 4 item 2)
//!
//! [`send_with_hop_timeout`] compares scheme+host+port between the
//! original request and each redirect hop's resolved URL. The
//! `Authorization: Bearer` header (from [`AUTH_TOKEN_ENV_VAR`], forward-
//! prep for a future private MSVC origin) is attached only while the hop
//! stays same-origin; a redirect to a different origin never carries it
//! forward. This is the same posture the client's auto-redirect takes
//! for sensitive headers on cross-origin hops, applied manually here
//! because the segmented client disables auto-redirect (see "Three
//! clocks").
//!
//! ### Three switches, not one (soldr#2320 addendum 4 item 1)
//!
//! There are three independent knobs here, each governing a different
//! layer, and conflating them was the critical review finding on the
//! first version of this module:
//!
//! 1. **Slicing** — `SOLDR_SEGMENTED_DOWNLOAD`. Whether a large,
//!    Range-capable response gets split into N parallel requests at all.
//!    Off means every download (small or large) takes exactly one
//!    connection, exactly like before this feature existed.
//! 2. **Capping** — `SOLDR_DOWNLOAD_MAX_SOCKETS` (Bulk) /
//!    `SOLDR_DOWNLOAD_QUICK_POOL` (Quick). How many connections each pool
//!    allows concurrently. **`0` means fully unconditional** for that
//!    pool: no permit object is created, no bookkeeping, no scheduler
//!    tick, nothing to wait on -- the literal pre-branch behavior, not
//!    merely a very large number. This is independent of switch 1: even
//!    with slicing off, the single remaining connection still acquires a
//!    permit from whichever pool it routes to, so `MAX_SOCKETS=0`/
//!    `QUICK_POOL=0` are the actual "make pooling a no-op" escape hatches.
//! 3. **QoS** — Bulk-vs-Quick routing (module docs "Two connection
//!    pools") and Bulk's PENDING/STREAMING preemption (module docs
//!    "Permit preemption"). Only meaningful when capping (switch 2) is
//!    actually in effect for the pool in question; policy for how
//!    contested capacity gets allocated and reclaimed.
//!
//! ### Knobs (defaults, env vars, units — all in one place)
//!
//! | Knob | Env var | Default | Unit |
//! |---|---|---|---|
//! | Enable/disable (opt-out, switch 1) | `SOLDR_SEGMENTED_DOWNLOAD` | enabled (`off`/`0`/`false`/`no` disables, case-insensitive) | bool-ish |
//! | Segment count | `SOLDR_SEGMENTED_DOWNLOAD_N` | 16 | count, clamped to `[2, 16]` |
//! | Connect/TTFB timeout (clock 1) | `SOLDR_DOWNLOAD_CONNECT_TIMEOUT_SECS` | 10 | seconds |
//! | Stall watchdog (clock 2) | `SOLDR_DOWNLOAD_STALL_TIMEOUT_SECS` | 30 | seconds |
//! | Per-segment retry limit | `SOLDR_DOWNLOAD_SEGMENT_RETRIES` | 3 | count, clamped to `[0, 10]` |
//! | Whole-operation deadline (clock 3, opt-in) | `SOLDR_DOWNLOAD_TIMEOUT_SECS` | disabled | seconds |
//! | Bulk pool size (switch 2) | `SOLDR_DOWNLOAD_MAX_SOCKETS` | 16 | count, `0` = unconditional |
//! | Quick pool size (switch 2) | `SOLDR_DOWNLOAD_QUICK_POOL` | 4 | count, `0` = unconditional |
//! | Quick/segmentation threshold | `SOLDR_DOWNLOAD_QUICK_THRESHOLD_BYTES` | 4 MiB (4194304) | bytes |
//! | Bearer auth (forward-prep) | `SOLDR_TOOLCHAIN_AUTH_TOKEN` | unset | token string |
//!
//! Every env var fails safe to its documented default on unset or
//! unparseable input — never panics, never silently picks an unintended
//! extreme. The one exception, by design: for the two pool-size knobs,
//! `0` is not junk -- it is the explicit unconditional sentinel (see
//! "Three switches" above). Only unparseable/unset values fail safe to
//! the default (capped) size.
//!
//! ### Retry composition with `fetch::retry::with_backoff`
//!
//! Per-segment retries here are a NEW, innermost layer, entirely distinct
//! from — and never multiplying with — the existing outer
//! `with_asset_backoff`/`with_backoff` retry that callers wrap around their
//! whole `ensure_*` operation (e.g. `syslib_common::ensure_syslib_bundle`).
//! A stalled or failed segment retries its OWN byte range, resuming from
//! bytes already durably written, entirely within a single call to this
//! module — it never returns an error for that. Preemptions are cheaper
//! still: they never touch the retry counter at all (see "Permit
//! preemption"). The outer retry loop is only ever reached if the WHOLE
//! operation still fails after: segmentation was attempted and exhausted
//! all per-segment retries AND the subsequent single-stream fallback also
//! failed (its pre-existing idle/safety-timeout errors are unchanged by
//! any of this). So the layers nest without multiplying: segment-level
//! retries handle a single-connection blip cheaply and invisibly; the
//! outer loop still only re-runs the same number of times it always did,
//! for the same terminal reasons it always did.

pub(crate) mod forge_http;
mod forge_pairing;
pub use forge_pairing::ForgePairingClient;

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::core::SoldrError;
use kernal_api::hash::Sha256Hasher;
use kernal_api::http::{Client, Limits, Method, Protocol, Request, Response};

pub(crate) const ASSET_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const ASSET_HEADER_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const CONTROL_HEADER_TIMEOUT: Duration = Duration::from_secs(120);
/// Bounded JSON control requests to the Forge peer authorization service.
#[derive(Clone)]
pub struct ForgeSignalClient(Client);

impl ForgeSignalClient {
    /// First-party relay and GitHub device authorization origin.
    pub const ORIGIN: &str = "https://signal.forge-ci.com";

    /// Construct a client pinned to the Forge origin, with no redirects.
    pub fn new() -> std::io::Result<Self> {
        super::net_guard::ensure_network_allowed("Forge remote peer authorization")
            .map_err(std::io::Error::other)?;
        Client::new(Limits {
            max_body_bytes: 4096,
            total_timeout: Duration::from_secs(12),
            read_timeout: Duration::from_secs(12),
            ..Limits::default()
        })
        .map(Self)
    }

    /// Exchange typed JSON for a single authorization operation.
    pub async fn post<Payload: serde::Serialize, Reply: serde::de::DeserializeOwned>(
        &self,
        operation: &str,
        request: &Payload,
    ) -> std::io::Result<Reply> {
        if !matches!(operation, "start" | "poll" | "check") {
            return Err(std::io::Error::other(
                "invalid Forge authorization operation",
            ));
        }
        let url = format!("{}/v1/remote/{operation}", Self::ORIGIN);
        let body = serde_json::to_vec(request).map_err(std::io::Error::other)?;
        let response = self
            .0
            .execute(Request {
                method: Method::Post,
                url: &url,
                headers: &[("content-type", "application/json")],
                body: &body,
            })
            .await?;
        if !(200..300).contains(&response.status()) {
            return Err(std::io::Error::other(format!(
                "Forge peer authorization {operation}: HTTP {}",
                response.status()
            )));
        }
        serde_json::from_slice(&response.into_bytes().await?).map_err(std::io::Error::other)
    }
}

/// Limit catalogue and asset redirects to five hops.
pub(crate) const MAX_REDIRECT_HOPS: u8 = 5;

/// A final circuit breaker for an otherwise-progressing asset download.
///
/// An idle watchdog alone would permit a server to trickle bytes forever. The
/// caller retries this transient failure from a freshly-created temporary file;
/// partial files are never exposed as completed artifacts.
pub(crate) const ASSET_SAFETY_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

/// Transport compatibility policy for a remote asset host.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum AssetProtocol {
    /// Let the client negotiate the best available HTTP version.
    #[default]
    Negotiated,
    /// Retain the HTTP/1-only compatibility mode required by selected SDK CDNs.
    Http1Only,
}

/// Construct the HTTP client for bounded control-plane requests.
pub(crate) fn control_http_client(purpose: &str) -> Result<Client, SoldrError> {
    super::net_guard::ensure_network_allowed(purpose)?;
    Client::new(Limits {
        max_redirects: MAX_REDIRECT_HOPS,
        max_body_bytes: 64 * 1024 * 1024,
        total_timeout: CONTROL_HEADER_TIMEOUT,
        read_timeout: CONTROL_HEADER_TIMEOUT,
        ..Limits::default()
    })
    .map_err(|error| SoldrError::Network(error.to_string()))
}

/// An HTTP connection after the signal server accepts the WebSocket upgrade.
pub type SignalWebSocketTransport = Box<dyn SignalWebSocketIo + Send + Unpin>;

/// Async I/O for an upgraded signal connection.
pub trait SignalWebSocketIo:
    tokio::io::AsyncRead + tokio::io::AsyncWrite + std::fmt::Debug
{
}

impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + std::fmt::Debug> SignalWebSocketIo for T {}

/// Open a direct HTTP/1.1 WebSocket connection to the signal server.
pub async fn open_signal_websocket(
    url: url::Url,
    key: &str,
    accept: &str,
) -> Result<SignalWebSocketTransport, SoldrError> {
    super::net_guard::ensure_network_allowed("signal WebSocket upgrade")?;
    let stream = Client::open_websocket(url.as_str(), key, accept, Limits::default())
        .await
        .map_err(|error| {
            let detail = if error.kind() == std::io::ErrorKind::InvalidData {
                "invalid signal WebSocket upgrade response".into()
            } else {
                error.to_string()
            };
            SoldrError::Network(detail)
        })?;
    Ok(Box::new(stream))
}

/// Construct an HTTP client for streamed asset requests.
pub(crate) fn asset_http_client(purpose: &str) -> Result<Client, SoldrError> {
    asset_http_client_with_protocol(purpose, AssetProtocol::Negotiated)
}

/// Multipart catalogue URLs are bound to an exact publication path.
pub(crate) fn asset_http_client_no_redirect(purpose: &str) -> Result<Client, SoldrError> {
    asset_client(
        purpose,
        AssetProtocol::Negotiated,
        0,
        Limits::default().connect_timeout,
    )
}

pub(crate) fn asset_http_client_with_protocol(
    purpose: &str,
    protocol: AssetProtocol,
) -> Result<Client, SoldrError> {
    asset_client(
        purpose,
        protocol,
        MAX_REDIRECT_HOPS,
        Limits::default().connect_timeout,
    )
}

pub(super) fn asset_client(
    purpose: &str,
    protocol: AssetProtocol,
    max_redirects: u8,
    connect_timeout: Duration,
) -> Result<Client, SoldrError> {
    super::net_guard::ensure_network_allowed(purpose)?;
    let protocol = match protocol {
        AssetProtocol::Negotiated => Protocol::Auto,
        AssetProtocol::Http1Only => Protocol::Http1Only,
    };
    Client::with_protocol(
        Limits {
            max_redirects,
            max_body_bytes: u64::MAX,
            connect_timeout,
            // Keep the existing header/idle/safety watchdogs authoritative.
            read_timeout: ASSET_SAFETY_TIMEOUT + ASSET_IDLE_TIMEOUT,
            total_timeout: ASSET_SAFETY_TIMEOUT + ASSET_HEADER_TIMEOUT + ASSET_IDLE_TIMEOUT,
            ..Limits::default()
        },
        protocol,
    )
    .map_err(|error| SoldrError::Network(error.to_string()))
}

async fn read_forge_body(
    mut response: Response,
    limit: usize,
    size_error: &str,
    map_error: impl Fn(std::io::Error) -> std::io::Error,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.next_chunk().await.map_err(&map_error)? {
        let chunk = chunk.as_ref();
        if chunk.len() > limit - bytes.len() {
            return Err(std::io::Error::other(size_error.to_owned()));
        }
        bytes.extend_from_slice(chunk);
    }
    Ok(bytes)
}

/// Own the URL and application headers until the fetch boundary sends the request.
pub(crate) struct PendingRequest {
    client: Client,
    method: Method,
    url: String,
    headers: Vec<(&'static str, String)>,
    body: std::io::Result<Vec<u8>>,
}

impl PendingRequest {
    fn new(client: &Client, method: Method, url: &str) -> Self {
        Self {
            client: client.clone(),
            method,
            url: url.to_owned(),
            headers: vec![("user-agent", format!("soldr/{}", crate::core::version()))],
            body: Ok(Vec::new()),
        }
    }

    pub(crate) fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    pub(crate) fn bearer_auth(self, token: &str) -> Self {
        self.header("authorization", format!("Bearer {token}"))
    }

    pub(super) async fn send(self) -> std::io::Result<Response> {
        let headers: Vec<_> = self
            .headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        self.client
            .execute(Request {
                method: self.method,
                url: &self.url,
                headers: &headers,
                body: &self.body?,
            })
            .await
    }
}

pub(crate) fn get_request(client: &Client, url: &str) -> PendingRequest {
    PendingRequest::new(client, Method::Get, url)
}

/// Probe a PyPI release through soldr-fetch's sole HTTP boundary.
pub async fn pypi_has_version(pkg: &str, version: &str) -> bool {
    let url = format!("https://pypi.org/pypi/{pkg}/{version}/json");
    let Ok(client) = control_http_client("PyPI release availability") else {
        return false;
    };
    get_request(&client, &url)
        .send()
        .await
        .map(|response| (200..300).contains(&response.status()))
        .unwrap_or(false)
}

/// Build a HEAD request through the fetch boundary.
pub(crate) fn head_request(client: &Client, url: &str) -> PendingRequest {
    PendingRequest::new(client, Method::Head, url)
}

/// Build a POST request through the fetch boundary.
pub(crate) fn post_request(client: &Client, url: &str) -> PendingRequest {
    PendingRequest::new(client, Method::Post, url)
}

/// Attach a serialized JSON request body through the fetch boundary.
pub(crate) fn with_json_body<T: serde::Serialize>(
    request: PendingRequest,
    body: &T,
) -> PendingRequest {
    let mut request = request.header("content-type", "application/json");
    request.body = serde_json::to_vec(body).map_err(std::io::Error::other);
    request
}

#[derive(Debug)]
pub(crate) struct DownloadedAsset {
    pub(crate) file: tempfile::NamedTempFile,
    pub(crate) sha256: String,
    pub(crate) bytes: u64,
}

impl DownloadedAsset {
    pub(crate) fn path(&self) -> &Path {
        self.file.path()
    }

    pub(crate) fn sha256(&self) -> &str {
        &self.sha256
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
}

pub(crate) async fn stream_response_to_temp_file(
    response: Response,
    url: &str,
    idle_timeout: Duration,
) -> Result<DownloadedAsset, SoldrError> {
    stream_response_to_temp_file_with_safety_timeout(
        response,
        url,
        idle_timeout,
        ASSET_SAFETY_TIMEOUT,
    )
    .await
}

/// Drain one already-logical catalogue part.  Catalogue parts are immutable
/// content-addressed objects, not byte ranges of an asset, so segmenting them
/// again creates nested Range fanout.  Keep this separate from the legacy
/// archive path: direct release downloads retain their Range behaviour.
pub(crate) async fn stream_catalogue_part_to_temp_file(
    response: Response,
    url: &str,
    idle_timeout: Duration,
) -> Result<DownloadedAsset, SoldrError> {
    let safe_url = safe_asset_url(url);
    if !(200..300).contains(&response.status()) {
        let retry_after = response
            .header("retry-after")
            .and_then(|value| std::str::from_utf8(value).ok())
            .map(|value| format!(" Retry-After: {value}"))
            .unwrap_or_default();
        return Err(SoldrError::Network(format!(
            "catalogue part download {safe_url} failed: HTTP {}{retry_after}",
            response.status(),
        )));
    }
    stream_response_to_temp_file_inner_direct(
        response,
        &safe_url,
        idle_timeout,
        ASSET_SAFETY_TIMEOUT,
        bulk_pool(),
    )
    .await
}

/// Drain exactly one catalogue-part response into a caller-owned unverified
/// file.  This is deliberately a single-stream primitive: multipart retry
/// code may append one validated Range tail, but must never re-enter the
/// segmented asset downloader.  On an interrupted read the bytes already
/// written remain in `file`; callers decide whether their validated resume
/// state permits keeping or deleting that prefix.
pub(crate) async fn stream_catalogue_part_into_file(
    mut response: Response,
    url: &str,
    file: &mut std::fs::File,
) -> Result<u64, SoldrError> {
    let safe_url = safe_asset_url(url);
    if !(200..300).contains(&response.status()) {
        let retry_after = response
            .header("retry-after")
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|seconds| format!(" Retry-After: {seconds}"))
            .unwrap_or_default();
        return Err(SoldrError::Network(format!(
            "catalogue part download {safe_url} failed: HTTP {}{retry_after}",
            response.status(),
        )));
    }
    let mut permit = bulk_pool().acquire().await;
    permit.mark_streaming();
    let started = tokio::time::Instant::now();
    let mut written = 0u64;
    loop {
        let remaining = ASSET_SAFETY_TIMEOUT.saturating_sub(started.elapsed());
        let wait = ASSET_IDLE_TIMEOUT.min(remaining);
        let chunk = tokio::time::timeout(wait, response.next_chunk())
            .await
            .map_err(|_| stalled_download_error(&safe_url, written, ASSET_IDLE_TIMEOUT))?
            .map_err(|error| interrupted_download_error(&safe_url, written, error))?;
        let Some(chunk) = chunk else { break };
        let chunk = chunk.as_ref();
        file.write_all(chunk)?;
        written = written.saturating_add(chunk.len() as u64);
    }
    file.flush()?;
    Ok(written)
}

pub(crate) fn safe_asset_url(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        return "[invalid asset URL]".into();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

/// Stream an archive response to a temporary file, incrementally hashing every
/// chunk while enforcing independent idle-progress and total-safety deadlines.
///
/// Before doing so, tries N-way Range segmentation (see module docs) using
/// only the headers of the already-sent `response` — no extra probe request.
/// `response`'s body is never touched unless/until segmentation is skipped
/// or fails, so the single-stream path below is always exactly as safe as
/// it was before this feature existed.
pub(crate) async fn stream_response_to_temp_file_with_safety_timeout(
    response: Response,
    url: &str,
    idle_timeout: Duration,
    safety_timeout: Duration,
) -> Result<DownloadedAsset, SoldrError> {
    stream_response_to_temp_file_inner(
        response,
        url,
        idle_timeout,
        safety_timeout,
        bulk_pool(),
        quick_pool(),
    )
    .await
}

/// Test-only entry point that plumbs a caller-supplied pool through instead
/// of the process-wide singletons for both bulk and quick roles, so pool-
/// bounding and preemption behavior can be exercised deterministically
/// without depending on (or polluting) global state shared with every
/// other test in the binary.
#[cfg(test)]
pub(crate) async fn stream_response_to_temp_file_with_pool(
    response: Response,
    url: &str,
    idle_timeout: Duration,
    safety_timeout: Duration,
    pool: Arc<SocketPool>,
) -> Result<DownloadedAsset, SoldrError> {
    stream_response_to_temp_file_inner(
        response,
        url,
        idle_timeout,
        safety_timeout,
        Arc::clone(&pool),
        pool,
    )
    .await
}

async fn stream_response_to_temp_file_inner(
    response: Response,
    url: &str,
    idle_timeout: Duration,
    safety_timeout: Duration,
    bulk: Arc<SocketPool>,
    quick: Arc<SocketPool>,
) -> Result<DownloadedAsset, SoldrError> {
    let safe_url = safe_asset_url(url);
    if !(200..300).contains(&response.status()) {
        return Err(SoldrError::Network(format!(
            "asset download {safe_url} failed: HTTP {}",
            response.status()
        )));
    }

    let threshold = parse_quick_threshold();
    let mut effective_safety_timeout = safety_timeout;
    let config = SegmentedDownloadConfig::from_env();
    if config.enabled {
        if let Some(total) = segmentable_total_len(&response, threshold) {
            match try_segmented_download(&response, url, total, config, Arc::clone(&bulk)).await {
                Ok(asset) => return Ok(asset),
                Err(SegmentedFailure::Fallback {
                    reason,
                    safety_timeout_override,
                }) => {
                    eprintln!(
                        "soldr: segmented download for {safe_url} fell back to single-stream: {reason}"
                    );
                    if let Some(bound) = safety_timeout_override {
                        effective_safety_timeout = effective_safety_timeout.min(bound);
                    }
                }
                Err(SegmentedFailure::Timeout(message)) => {
                    return Err(SoldrError::Network(message));
                }
            }
        }
    }
    let safety_timeout = effective_safety_timeout;

    // Pool routing for the single-stream drain (module docs "Two
    // connection pools"): known-small -> Quick (never segmented, no
    // preemption needed); unknown or large -> Bulk, conservative. The
    // connect phase for this response already happened in the caller's
    // own `send_asset_request` before we ever saw it, so there is no
    // PENDING/preemptible phase left for us to model -- acquire and go
    // straight to STREAMING for the duration of the drain loop below.
    let pool = match response.content_length() {
        Some(total) if total <= threshold => quick,
        _ => bulk,
    };
    stream_response_to_temp_file_inner_direct(
        response,
        &safe_url,
        idle_timeout,
        safety_timeout,
        pool,
    )
    .await
}

/// The common single-request drain.  Kept below the segmented decision so a
/// catalogue part can opt into exactly this code without ever creating a
/// nested Range request.
async fn stream_response_to_temp_file_inner_direct(
    mut response: Response,
    url: &str,
    idle_timeout: Duration,
    safety_timeout: Duration,
    pool: Arc<SocketPool>,
) -> Result<DownloadedAsset, SoldrError> {
    let mut permit = pool.acquire().await;
    permit.mark_streaming();

    let mut file = tempfile::NamedTempFile::new_in(soldr_core::core::ensure_temp_root())?;
    let mut hasher = Sha256Hasher::new();
    let mut bytes = 0_u64;
    let started = tokio::time::Instant::now();

    loop {
        if started.elapsed() >= safety_timeout {
            return Err(SoldrError::Network(format!(
                "asset download exceeded its global safety ceiling of {safety_timeout:?} after {bytes} bytes: {url}"
            )));
        }
        let remaining = safety_timeout.saturating_sub(started.elapsed());
        let wait = idle_timeout.min(remaining);
        let chunk = tokio::time::timeout(wait, response.next_chunk())
            .await
            .map_err(|_| {
                if wait == remaining {
                    SoldrError::Network(format!(
                        "asset download exceeded its global safety ceiling of {safety_timeout:?} after {bytes} bytes: {url}"
                    ))
                } else {
                    stalled_download_error(url, bytes, idle_timeout)
                }
            })?
            .map_err(|error| interrupted_download_error(url, bytes, error))?;
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.as_ref();
        file.write_all(chunk)?;
        hasher.update(chunk);
        bytes = bytes.saturating_add(chunk.len() as u64);
    }
    file.flush()?;

    Ok(DownloadedAsset {
        file,
        sha256: hasher.finalize().to_hex(),
        bytes,
    })
}

pub(crate) async fn send_asset_request(
    request: PendingRequest,
    url: &str,
    header_timeout: Duration,
) -> Result<Response, SoldrError> {
    let safe_url = safe_asset_url(url);
    tokio::time::timeout(header_timeout, request.send())
        .await
        .map_err(|_| {
            SoldrError::Network(format!(
                "asset request timed out waiting for headers: {safe_url}"
            ))
        })?
        .map_err(|error| request_failure("asset", &safe_url, &error))
}

/// Send a small metadata/API request with the control-plane header deadline.
///
/// Routes through the Quick pool (module docs "Two connection pools") --
/// FIFO, no preemption, sized independently of Bulk so a small catalogue
/// fetch never queues behind busy segment streams.
pub(crate) async fn send_control_request(
    request: PendingRequest,
    url: &str,
) -> Result<Response, SoldrError> {
    send_control_request_with_timeout(request, url, CONTROL_HEADER_TIMEOUT).await
}

/// Send a control request with a caller's narrower operation-specific budget.
pub(crate) async fn send_control_request_with_timeout(
    request: PendingRequest,
    url: &str,
    header_timeout: Duration,
) -> Result<Response, SoldrError> {
    send_control_request_with_pool_inner(request, url, header_timeout, quick_pool()).await
}

/// Test-only entry point that plumbs a caller-supplied pool through.
#[cfg(test)]
pub(crate) async fn send_control_request_with_pool(
    request: PendingRequest,
    url: &str,
    header_timeout: Duration,
    pool: Arc<SocketPool>,
) -> Result<Response, SoldrError> {
    send_control_request_with_pool_inner(request, url, header_timeout, pool).await
}

async fn send_control_request_with_pool_inner(
    request: PendingRequest,
    url: &str,
    header_timeout: Duration,
    pool: Arc<SocketPool>,
) -> Result<Response, SoldrError> {
    let safe_url = safe_asset_url(url);
    // Quick permits skip PENDING/preemption entirely -- see module docs.
    let mut permit = pool.acquire().await;
    permit.mark_streaming();
    tokio::time::timeout(header_timeout, request.send())
        .await
        .map_err(|_| {
            SoldrError::Network(format!(
                "control request timed out waiting for headers: {safe_url}"
            ))
        })?
        .map_err(|error| request_failure("control", &safe_url, &error))
    // `permit` drops once headers are back; the (small, fast) body read
    // that follows via `read_control_text` is intentionally ungated.
}

/// Read a small control-plane response through the fetch boundary.
pub(crate) async fn read_control_text(
    response: Response,
    url: &str,
    body_timeout: Duration,
) -> Result<String, SoldrError> {
    let safe_url = safe_asset_url(url);
    let bytes = tokio::time::timeout(body_timeout, response.into_bytes())
        .await
        .map_err(|_| SoldrError::Network(format!("control response body timed out: {safe_url}")))?
        .map_err(|error| request_failure("control response body", &safe_url, &error))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn request_failure(kind: &str, safe_url: &str, error: &std::io::Error) -> SoldrError {
    let category = match error.kind() {
        std::io::ErrorKind::TimedOut => "timed out",
        std::io::ErrorKind::ConnectionRefused => "connection failed",
        _ => "request failed",
    };
    SoldrError::Network(format!("{kind} {category}: {safe_url}"))
}

fn stalled_download_error(url: &str, bytes: u64, idle_timeout: Duration) -> SoldrError {
    SoldrError::Network(format!(
        "asset download stalled after {bytes} bytes with no progress for {idle_timeout:?}: {url}"
    ))
}

fn interrupted_download_error(url: &str, bytes: u64, error: std::io::Error) -> SoldrError {
    let category = if error.kind() == std::io::ErrorKind::TimedOut {
        "timeout"
    } else {
        "transport error"
    };
    SoldrError::Network(format!(
        "asset download interrupted after {bytes} bytes ({category}): {url}"
    ))
}

// =============================================================================

pub(crate) use super::segmented_download::{
    bulk_pool, parse_quick_threshold, quick_pool, segmentable_total_len, try_segmented_download,
    SegmentedDownloadConfig, SegmentedFailure, SocketPool,
};

#[cfg(test)]
mod websocket_tests {
    use super::open_signal_websocket;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn websocket_upgrade_preserves_buffered_bytes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/signal", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            loop {
                line.clear();
                assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
            }
            socket
                .write_all(
                    concat!(
                        "HTTP/1.1 101 Switching Protocols\r\n",
                        "Connection: Upgrade\r\nUpgrade: websocket\r\n",
                        "Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
                        "frame",
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let mut stream = open_signal_websocket(
            url::Url::parse(&url).unwrap(),
            "dGhlIHNhbXBsZSBub25jZQ==",
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
        )
        .await
        .unwrap();
        let mut frame = [0; 5];
        stream.read_exact(&mut frame).await.unwrap();
        assert_eq!(&frame, b"frame");
        server.await.unwrap();
    }
}
