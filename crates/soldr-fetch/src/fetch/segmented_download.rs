use super::stream_download::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::core::SoldrError;
use crate::platform::sync::Mutex;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio::task::JoinSet;

// Segmented download (setup-soldr feat/segmented-download-experiment, soldr#2320)
// =============================================================================

pub(crate) const SEGMENTED_DOWNLOAD_ENV_VAR: &str = "SOLDR_SEGMENTED_DOWNLOAD";
pub(crate) const SEGMENTED_DOWNLOAD_N_ENV_VAR: &str = "SOLDR_SEGMENTED_DOWNLOAD_N";
pub(crate) const CONNECT_TIMEOUT_ENV_VAR: &str = "SOLDR_DOWNLOAD_CONNECT_TIMEOUT_SECS";
pub(crate) const STALL_TIMEOUT_ENV_VAR: &str = "SOLDR_DOWNLOAD_STALL_TIMEOUT_SECS";
pub(crate) const SEGMENT_RETRIES_ENV_VAR: &str = "SOLDR_DOWNLOAD_SEGMENT_RETRIES";
pub(crate) const GLOBAL_TIMEOUT_ENV_VAR: &str = "SOLDR_DOWNLOAD_TIMEOUT_SECS";
pub(crate) const MAX_SOCKETS_ENV_VAR: &str = "SOLDR_DOWNLOAD_MAX_SOCKETS";
pub(crate) const QUICK_POOL_ENV_VAR: &str = "SOLDR_DOWNLOAD_QUICK_POOL";
pub(crate) const QUICK_THRESHOLD_ENV_VAR: &str = "SOLDR_DOWNLOAD_QUICK_THRESHOLD_BYTES";
/// Bearer token attached to every probe-free segment request. Forward-prep
/// for a future private MSVC bundle origin; none of today's public
/// catalogue/xwin-cache/release-asset origins require it.
pub(crate) const AUTH_TOKEN_ENV_VAR: &str = "SOLDR_TOOLCHAIN_AUTH_TOKEN";

const MAX_SEGMENTS: u32 = 16;
const DEFAULT_SEGMENT_COUNT: u32 = 16;
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_STALL_TIMEOUT_SECS: u64 = 30;
const DEFAULT_SEGMENT_RETRIES: u32 = 3;
const MAX_SEGMENT_RETRIES: u32 = 10;
const DEFAULT_MAX_SOCKETS: usize = 16;
const DEFAULT_QUICK_POOL_SIZE: usize = 4;
/// Same threshold doubles as the Quick-pool routing cutoff AND the
/// segmentation minimum -- see module docs "Two connection pools". Lowered
/// under `cfg(test)` so unit tests can exercise the segmented path without
/// transferring megabytes over localhost; production default is 4 MiB.
#[cfg(not(test))]
const DEFAULT_QUICK_THRESHOLD_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(test)]
const DEFAULT_QUICK_THRESHOLD_BYTES: u64 = 256;
/// Redirect-hop ceiling for the segmented client's manual redirect
/// following (see module docs "Three clocks"). Matches reqwest's own
/// default auto-redirect limit.
const MAX_REDIRECT_HOPS: u32 = 5;
/// If a whole-operation deadline (`SOLDR_DOWNLOAD_TIMEOUT_SECS`) expires
/// with less than this much budget left, a single-stream fallback attempt
/// is not "meaningful" -- surface a clear timeout error instead of
/// starting an attempt that has almost no chance to finish.
const MEANINGFUL_FALLBACK_MIN: Duration = Duration::from_secs(5);

/// Resolved segmented-download configuration. See the module docs' knob
/// table for env var names, defaults, and units.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SegmentedDownloadConfig {
    pub(crate) enabled: bool,
    pub(crate) segment_count: u32,
    pub(crate) connect_timeout: Duration,
    pub(crate) stall_timeout: Duration,
    pub(crate) segment_retries: u32,
    pub(crate) global_timeout: Option<Duration>,
}

impl SegmentedDownloadConfig {
    pub(crate) fn from_env() -> Self {
        Self {
            enabled: !opted_out(SEGMENTED_DOWNLOAD_ENV_VAR),
            segment_count: parse_segment_count(),
            connect_timeout: parse_connect_timeout(),
            stall_timeout: parse_stall_timeout(),
            segment_retries: parse_segment_retries(),
            global_timeout: parse_global_timeout(),
        }
    }
}

/// `off`/`0`/`false`/`no` (case-insensitive, trimmed) opt out; anything
/// else -- including unset -- leaves the feature on. Mirrors
/// `msvc_host::opted_out`'s value parsing (`SOLDR_MSVC_DISCOVERY=off`).
fn opted_out(var: &str) -> bool {
    match std::env::var(var) {
        Ok(v) => crate::core::is_off_value(&v),
        Err(_) => false,
    }
}

fn auth_token() -> Option<String> {
    std::env::var(AUTH_TOKEN_ENV_VAR)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

fn parse_segment_count() -> u32 {
    std::env::var(SEGMENTED_DOWNLOAD_N_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n >= 2)
        .map(|n| n.min(MAX_SEGMENTS))
        .unwrap_or(DEFAULT_SEGMENT_COUNT)
}

fn parse_connect_timeout() -> Duration {
    std::env::var(CONNECT_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS))
}

fn parse_stall_timeout() -> Duration {
    std::env::var(STALL_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_STALL_TIMEOUT_SECS))
}

fn parse_segment_retries() -> u32 {
    std::env::var(SEGMENT_RETRIES_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map(|n| n.min(MAX_SEGMENT_RETRIES))
        .unwrap_or(DEFAULT_SEGMENT_RETRIES)
}

/// `None` (disabled) unless a valid positive integer is set -- junk values
/// fail safe to disabled, never to some arbitrary enabled duration.
fn parse_global_timeout() -> Option<Duration> {
    std::env::var(GLOBAL_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
}

/// `0` is the explicit "unconditional" sentinel (module docs "Three
/// switches"), returned as `None`; a valid positive integer caps at that
/// value (`Some`); unset or unparseable fails safe to `default` --
/// capped, never unconditional, so junk input can never silently remove
/// the cap.
fn parse_pool_size_env(var: &str, default: usize) -> Option<usize> {
    match std::env::var(var) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => Some(default),
        },
        Err(_) => Some(default),
    }
}

fn parse_max_sockets() -> Option<usize> {
    parse_pool_size_env(MAX_SOCKETS_ENV_VAR, DEFAULT_MAX_SOCKETS)
}

fn parse_quick_pool_size() -> Option<usize> {
    parse_pool_size_env(QUICK_POOL_ENV_VAR, DEFAULT_QUICK_POOL_SIZE)
}

pub(crate) fn parse_quick_threshold() -> u64 {
    std::env::var(QUICK_THRESHOLD_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_QUICK_THRESHOLD_BYTES)
}

/// Whether `response`'s headers alone (no extra request) indicate a
/// resource worth segmenting: the server advertises `Accept-Ranges: bytes`
/// and a `Content-Length` strictly above `threshold` (at/below threshold
/// routes to the Quick pool instead and is never segmented -- module docs
/// "Two connection pools").
pub(crate) fn segmentable_total_len(response: &reqwest::Response, threshold: u64) -> Option<u64> {
    let accept_ranges = response
        .headers()
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("bytes"))
        .unwrap_or(false);
    if !accept_ranges {
        return None;
    }
    let total = response.content_length()?;
    (total > threshold).then_some(total)
}

// -----------------------------------------------------------------------
// Connection pools: process-wide permits with PENDING/STREAMING
// preemption for Bulk; plain FIFO for Quick.
// -----------------------------------------------------------------------

const PREEMPT_GRACE: Duration = Duration::from_secs(2);
const MAX_PREEMPTIONS: u32 = 2;
const SCHEDULER_TICK: Duration = Duration::from_secs(1);

static BULK_POOL: OnceLock<Arc<SocketPool>> = OnceLock::new();
static QUICK_POOL: OnceLock<Arc<SocketPool>> = OnceLock::new();

/// The process-wide Bulk payload-transfer pool. See module docs "Two
/// connection pools", "Permit preemption", and "Three switches" (`0` ==
/// unconditional).
pub(crate) fn bulk_pool() -> Arc<SocketPool> {
    Arc::clone(BULK_POOL.get_or_init(|| match parse_max_sockets() {
        Some(n) => SocketPool::new(n),
        None => SocketPool::unbounded(),
    }))
}

/// The admission ceiling used by the catalogue multipart coordinator.  It is
/// intentionally the same parsed setting as the Bulk socket pool; the
/// coordinator supplies fair *start order*, while `SocketPool` remains the
/// authority that owns and releases actual socket permits.
pub(crate) fn bulk_pool_capacity() -> Option<usize> {
    parse_max_sockets()
}

/// The process-wide Quick pool for control-plane requests and small
/// downloads. See module docs "Two connection pools" and "Three
/// switches" (`0` == unconditional).
pub(crate) fn quick_pool() -> Arc<SocketPool> {
    Arc::clone(QUICK_POOL.get_or_init(|| match parse_quick_pool_size() {
        Some(n) => SocketPool::new(n),
        None => SocketPool::unbounded(),
    }))
}

struct PendingEntry {
    since: tokio::time::Instant,
    preemptions: u32,
    preempt: Arc<Notify>,
}

#[derive(Default)]
struct PoolInner {
    used: usize,
    waiting: usize,
    pending: HashMap<u64, PendingEntry>,
}

/// A connection-slot pool. Unlike a plain semaphore, each held slot
/// tracks whether it is PENDING (acquired, pre-first-byte) or STREAMING
/// (first payload byte received, permanently non-preemptible) so a
/// background scheduler tick can steal a PENDING slot for a FIFO waiter
/// under contention -- Bulk uses this; Quick holders just mark themselves
/// STREAMING immediately and are never preemption candidates. See module
/// docs "Permit preemption" for the full policy.
pub(crate) struct SocketPool {
    /// `None` means unconditional (module docs "Three switches"): no
    /// bookkeeping, no scheduler tick, [`SocketPool::acquire`] returns a
    /// detached permit immediately every time.
    capacity: Option<usize>,
    inner: Mutex<PoolInner>,
    wake: Notify,
    next_id: AtomicU64,
}

impl SocketPool {
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Self::with_capacity(Some(capacity.max(1)))
    }

    /// Fully unconditional -- the literal pre-branch, no-pool-at-all
    /// behavior, not merely a very large capacity. See module docs
    /// "Three switches: slicing / capping / QoS".
    pub(crate) fn unbounded() -> Arc<Self> {
        Self::with_capacity(None)
    }

    fn with_capacity(capacity: Option<usize>) -> Arc<Self> {
        let pool = Arc::new(Self {
            capacity,
            inner: Mutex::new(PoolInner::default()),
            wake: Notify::new(),
            next_id: AtomicU64::new(0),
        });
        // No scheduler tick for an unconditional pool: nothing is ever
        // PENDING (acquire short-circuits before touching `inner`), so a
        // tick would only ever find `waiting == 0` and return -- pure
        // overhead for a pool whose entire point is "no bookkeeping".
        if pool.capacity.is_some() {
            let scheduler_pool = Arc::clone(&pool);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(SCHEDULER_TICK).await;
                    scheduler_pool.maybe_preempt();
                }
            });
        }
        pool
    }

    /// One scheduler tick: if a FIFO waiter exists, steal the oldest
    /// PENDING slot that has cleared the grace period and has not already
    /// been preempted [`MAX_PREEMPTIONS`] times. Freeing the slot itself
    /// happens when the victim's own `fetch_segment_once` observes the
    /// preempt signal and drops its [`PoolPermit`] -- this only decides
    /// *who*, then wakes it up. A pool with no PENDING holders (e.g.
    /// Quick, which never has any) is a no-op every tick.
    fn maybe_preempt(&self) {
        let mut inner = self.inner.lock();
        if inner.waiting == 0 {
            return;
        }
        let now = tokio::time::Instant::now();
        let victim = inner
            .pending
            .iter()
            .filter(|(_, e)| {
                now.saturating_duration_since(e.since) >= PREEMPT_GRACE
                    && e.preemptions < MAX_PREEMPTIONS
            })
            .min_by_key(|(_, e)| e.since)
            .map(|(&id, _)| id);
        if let Some(id) = victim {
            if let Some(entry) = inner.pending.get_mut(&id) {
                entry.preemptions += 1;
                entry.preempt.notify_one();
            }
        }
        drop(inner);
        self.wake.notify_waiters();
    }

    /// Acquire a slot, blocking FIFO-fashion while the pool is at
    /// capacity. The returned permit starts PENDING.
    pub(crate) async fn acquire(self: &Arc<Self>) -> PoolPermit {
        if self.capacity.is_none() {
            return PoolPermit {
                pool: Arc::clone(self),
                id: 0,
                preempt: Arc::new(Notify::new()),
                streaming: true,
            };
        }
        loop {
            // Register with `wake` BEFORE the capacity check. `wake` is
            // signalled with `notify_waiters()` (Drop, scheduler tick),
            // which stores no permit and only wakes waiters already
            // enabled -- so a wakeup fired between our check and our await
            // would be lost if we enabled afterwards. `enable()` closes
            // that window: any `notify_waiters()` after this line marks
            // this future ready and our `.await` returns promptly.
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut inner = self.inner.lock();
                if inner.used < self.capacity.expect("bounded pool") {
                    inner.used += 1;
                    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                    let preempt = Arc::new(Notify::new());
                    inner.pending.insert(
                        id,
                        PendingEntry {
                            since: tokio::time::Instant::now(),
                            preemptions: 0,
                            preempt: Arc::clone(&preempt),
                        },
                    );
                    return PoolPermit {
                        pool: Arc::clone(self),
                        id,
                        preempt,
                        streaming: false,
                    };
                }
                inner.waiting += 1;
            }
            notified.await;
            let mut inner = self.inner.lock();
            inner.waiting = inner.waiting.saturating_sub(1);
        }
    }

    #[cfg(test)]
    fn available(&self) -> usize {
        let inner = self.inner.lock();
        self.capacity
            .map(|capacity| capacity - inner.used)
            .unwrap_or(usize::MAX)
    }
}

/// A held connection-pool slot. Dropping it (any return path: success,
/// failure, stall-watchdog kill, or preemption) frees the slot -- this is
/// also the permit-leak guarantee: RAII means a killed/aborted segment
/// task can never hold a slot forever.
pub(crate) struct PoolPermit {
    pool: Arc<SocketPool>,
    id: u64,
    preempt: Arc<Notify>,
    streaming: bool,
}

impl PoolPermit {
    /// Transition PENDING -> STREAMING: the first payload byte is about
    /// to be read (Bulk), or this permit will never have a PENDING phase
    /// at all (Quick, single-stream drain). After this call the permit
    /// can never be preempted. Idempotent.
    pub(crate) fn mark_streaming(&mut self) {
        if self.streaming {
            return;
        }
        self.streaming = true;
        let mut inner = self.pool.inner.lock();
        inner.pending.remove(&self.id);
    }

    /// Race `fut` (a PENDING-phase operation, i.e. connect/TTFB) against
    /// this permit's preemption signal. `Err(())` means preempted --
    /// callers must not call this again after [`mark_streaming`].
    async fn race_preemption<F: std::future::Future>(&self, fut: F) -> Result<F::Output, ()> {
        tokio::select! {
            biased;
            _ = self.preempt.notified() => Err(()),
            out = fut => Ok(out),
        }
    }
}

impl Drop for PoolPermit {
    fn drop(&mut self) {
        if self.pool.capacity.is_none() {
            return;
        }
        let mut inner = self.pool.inner.lock();
        inner.used = inner.used.saturating_sub(1);
        inner.pending.remove(&self.id);
        drop(inner);
        self.pool.wake.notify_waiters();
    }
}

/// Why a segmented attempt did not produce a verified asset.
pub(crate) enum SegmentedFailure {
    /// Recoverable: the caller should drain the original, still-untouched
    /// `response` exactly as it would have before this feature existed.
    /// `safety_timeout_override`, when set, tightens the caller's
    /// remaining safety-timeout budget (only ever set when a whole-
    /// operation deadline ate into it).
    Fallback {
        reason: String,
        safety_timeout_override: Option<Duration>,
    },
    /// Fatal: a whole-operation deadline (`SOLDR_DOWNLOAD_TIMEOUT_SECS`)
    /// expired with too little budget left to attempt anything else.
    Timeout(String),
}

impl SegmentedFailure {
    fn fallback(reason: impl Into<String>) -> Self {
        SegmentedFailure::Fallback {
            reason: reason.into(),
            safety_timeout_override: None,
        }
    }
}

/// The segmented client disables reqwest's own auto-redirect
/// (`redirect::Policy::none()`) so [`send_with_hop_timeout`] can give
/// each hop its own connect/TTFB clock -- see module docs "Three clocks".
/// Small, deliberate duplication of [`asset_http_client_with_protocol`]
/// rather than a shared parameterized builder, so that function's
/// existing (auto-redirecting) behavior for every other caller stays
/// untouched.
fn segmented_http_client(
    protocol: AssetProtocol,
    connect_timeout: Duration,
) -> Result<reqwest::Client, SoldrError> {
    super::net_guard::ensure_network_allowed("segmented asset download")?;
    let mut builder = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!("soldr/{}", crate::core::version()));
    if matches!(protocol, AssetProtocol::Http1Only) {
        builder = builder.http1_only();
    }
    builder
        .build()
        .map_err(|error| SoldrError::Network(error.to_string()))
}

fn resolve_redirect_url(current: &str, location: &str) -> Result<String, String> {
    let base = url::Url::parse(current).map_err(|_| "invalid current redirect URL".to_string())?;
    let next = base
        .join(location)
        .map_err(|_| "invalid redirect target".to_string())?;
    if (base.scheme() == "https" && next.scheme() != "https")
        || next.host_str().is_none()
        || !next.username().is_empty()
        || next.password().is_some()
    {
        return Err("unsafe redirect target".to_string());
    }
    Ok(next.to_string())
}

fn same_origin(left: &str, right: &str) -> bool {
    match (url::Url::parse(left), url::Url::parse(right)) {
        (Ok(left), Ok(right)) => left.origin() == right.origin(),
        _ => false,
    }
}

/// Send one logical GET, following redirects manually so each hop gets
/// its own connect/TTFB clock (clock 1). `build_request` is called fresh
/// for every hop since a `RequestBuilder` is consumed by `.send()`.
async fn send_with_hop_timeout(
    client: &reqwest::Client,
    initial_url: &str,
    build_request: impl Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    connect_timeout: Duration,
) -> Result<reqwest::Response, String> {
    let mut url = initial_url.to_string();
    for redirects_followed in 0..=MAX_REDIRECT_HOPS {
        let req = build_request(client, &url);
        let resp = match tokio::time::timeout(connect_timeout, req.send()).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => return Err("connect/send failed".to_string()),
            Err(_) => return Err(format!("connect/TTFB exceeded {connect_timeout:?}")),
        };
        if resp.status().is_redirection() {
            if redirects_followed == MAX_REDIRECT_HOPS {
                return Err(format!("exceeded {MAX_REDIRECT_HOPS} redirect hops"));
            }
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            match location {
                Some(loc) => {
                    url = resolve_redirect_url(&url, &loc)?;
                    continue; // fresh clock next iteration -- hop resets it.
                }
                None => {
                    return Err(format!(
                        "redirect status {} with no Location header",
                        resp.status()
                    ))
                }
            }
        }
        return Ok(resp);
    }
    unreachable!("the redirect loop returns on every iteration")
}

/// Attempt N-way Range segmentation. `response` is used only for its
/// already-received headers/version (see module docs) -- its body is
/// never read here.
pub(crate) async fn try_segmented_download(
    response: &reqwest::Response,
    url: &str,
    total: u64,
    config: SegmentedDownloadConfig,
    pool: Arc<SocketPool>,
) -> Result<DownloadedAsset, SegmentedFailure> {
    let safe_url = super::stream_download::safe_asset_url(url);
    // Protocol inheritance -- see module docs "Protocol inheritance".
    let protocol = if response.version() >= reqwest::Version::HTTP_2 {
        AssetProtocol::Negotiated
    } else {
        AssetProtocol::Http1Only
    };
    let client = segmented_http_client(protocol, config.connect_timeout)
        .map_err(|e| SegmentedFailure::fallback(format!("client build failed: {e}")))?;

    let plan = compute_segments(total, config.segment_count);
    if plan.is_empty() {
        return Err(SegmentedFailure::fallback("empty segment plan"));
    }

    let named = tempfile::NamedTempFile::new_in(soldr_core::core::ensure_temp_root())
        .map_err(|e| SegmentedFailure::fallback(format!("tempfile create failed: {e}")))?;
    named
        .as_file()
        .set_len(total)
        .map_err(|e| SegmentedFailure::fallback(format!("preallocate failed: {e}")))?;
    let file = Arc::new(
        named
            .reopen()
            .map_err(|e| SegmentedFailure::fallback(format!("reopen failed: {e}")))?,
    );

    let run = run_all_segments(
        client,
        url.to_string(),
        plan,
        Arc::clone(&file),
        config,
        pool,
    );
    let outcome = match config.global_timeout {
        Some(gt) => {
            let deadline = tokio::time::Instant::now() + gt;
            match tokio::time::timeout(gt, run).await {
                Ok(inner) => inner,
                Err(_elapsed) => {
                    // `run` (and its JoinSet) is dropped here, which aborts
                    // every still-running segment task -- their PoolPermit
                    // Drop impls release their slots.
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    return if remaining >= MEANINGFUL_FALLBACK_MIN {
                        Err(SegmentedFailure::Fallback {
                            reason: format!(
                                "{GLOBAL_TIMEOUT_ENV_VAR}={gt:?} exceeded during segmented transfer"
                            ),
                            safety_timeout_override: Some(remaining),
                        })
                    } else {
                        Err(SegmentedFailure::Timeout(format!(
                            "asset download exceeded {GLOBAL_TIMEOUT_ENV_VAR}={gt:?} with no \
                             meaningful budget left to fall back to single-stream: {safe_url}"
                        )))
                    };
                }
            }
        }
        None => run.await,
    };

    let bytes = outcome.map_err(SegmentedFailure::fallback)?;
    file.sync_all()
        .map_err(|e| SegmentedFailure::fallback(format!("sync failed: {e}")))?;
    let sha256 = sha256_of_file(&file)
        .map_err(|e| SegmentedFailure::fallback(format!("hash failed: {e}")))?;
    Ok(DownloadedAsset {
        file: named,
        sha256,
        bytes,
    })
}

/// Run every planned segment concurrently. On the first segment that
/// exhausts its retry budget, aborts every other still-running segment
/// (via `JoinSet::abort_all`) and returns its failure reason -- "the
/// segmented attempt aborts and falls through to single-stream".
async fn run_all_segments(
    client: reqwest::Client,
    url: String,
    plan: Vec<(u64, u64)>,
    file: Arc<std::fs::File>,
    config: SegmentedDownloadConfig,
    pool: Arc<SocketPool>,
) -> Result<u64, String> {
    let mut set = JoinSet::new();
    for (start, end_inclusive) in plan {
        let client = client.clone();
        let url = url.clone();
        let file = Arc::clone(&file);
        let pool = Arc::clone(&pool);
        set.spawn(async move {
            fetch_segment_with_retries(&client, &url, start, end_inclusive, &file, config, pool)
                .await
        });
    }

    let mut total_bytes = 0u64;
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(n)) => total_bytes += n,
            Ok(Err(reason)) => {
                set.abort_all();
                return Err(reason);
            }
            Err(join_err) => {
                set.abort_all();
                return Err(format!("segment task panicked: {join_err}"));
            }
        }
    }
    Ok(total_bytes)
}

/// Outcome of one attempt at a segment's byte range.
#[derive(Debug, PartialEq)]
enum SegmentAttemptOutcome {
    /// Fully delivered `n` bytes this attempt.
    Completed(u64),
    /// Preempted during the PENDING (connect) phase -- zero bytes were
    /// ever at risk. Not a failure: the caller re-queues for a fresh
    /// permit without touching its retry budget.
    Preempted,
    /// Failed after writing `.0` bytes this attempt, for reason `.1`.
    Failed(u64, String),
}

/// Fetch one `[start, end_inclusive]` byte range, retrying up to
/// `config.segment_retries` additional times on any failure or stall.
/// Each retry resumes from `start + <bytes already durably written>`,
/// never re-requesting bytes already on disk. Preemptions loop back
/// immediately without incrementing the retry counter.
async fn fetch_segment_with_retries(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end_inclusive: u64,
    file: &std::fs::File,
    config: SegmentedDownloadConfig,
    pool: Arc<SocketPool>,
) -> Result<u64, String> {
    let expected = end_inclusive - start + 1;
    let mut written = 0u64;
    let mut attempt = 0u32;
    loop {
        let resume_start = start + written;
        let outcome = fetch_segment_once(
            client,
            url,
            resume_start,
            end_inclusive,
            file,
            config.stall_timeout,
            config.connect_timeout,
            &pool,
        )
        .await;
        match outcome {
            SegmentAttemptOutcome::Completed(n) => {
                written += n;
                if written >= expected {
                    return Ok(written);
                }
                // Stream ended before delivering the full range -- treat
                // like any other failure for retry-counting purposes.
                attempt += 1;
                if attempt > config.segment_retries {
                    return Err(format!(
                        "segment [{start},{end_inclusive}] incomplete after {attempt} attempt(s): {written}/{expected} bytes"
                    ));
                }
                eprintln!(
                    "soldr: segment [{start},{end_inclusive}] short read, retry {attempt}/{} (resuming at byte {})",
                    config.segment_retries,
                    start + written
                );
            }
            SegmentAttemptOutcome::Preempted => {
                eprintln!(
                    "soldr: segment [{start},{end_inclusive}] preempted before its first byte; re-queueing (retry budget untouched)"
                );
                continue;
            }
            SegmentAttemptOutcome::Failed(n, reason) => {
                written += n;
                attempt += 1;
                if attempt > config.segment_retries {
                    return Err(format!(
                        "segment [{start},{end_inclusive}] failed after {attempt} attempt(s), {written}/{expected} bytes written: {reason}"
                    ));
                }
                eprintln!(
                    "soldr: segment [{start},{end_inclusive}] retry {attempt}/{} (resuming at byte {}): {reason}",
                    config.segment_retries,
                    start + written
                );
            }
        }
    }
}

/// One attempt at fetching `[start, end_inclusive]`. Acquires a Bulk-pool
/// permit first (so pool-wait time counts against no clock), races the
/// connect phase against preemption while PENDING, transitions to
/// STREAMING before the body loop (never preemptible from there), and
/// applies the stall watchdog (clock 2) to every chunk read.
///
/// Eight narrow, independently-meaningful parameters (byte range, two
/// timeouts, the pool, etc.) rather than a bundling struct: every caller
/// (production `fetch_segment_with_retries` and several direct unit
/// tests exercising one clock/pool behavior at a time) wants to vary a
/// different subset, and a struct would just move the same argument
/// count to construction call sites instead of removing it.
#[allow(clippy::too_many_arguments)]
async fn fetch_segment_once(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end_inclusive: u64,
    file: &std::fs::File,
    stall_timeout: Duration,
    connect_timeout: Duration,
    pool: &Arc<SocketPool>,
) -> SegmentAttemptOutcome {
    let mut permit = pool.acquire().await;

    let range_value = format!("bytes={start}-{end_inclusive}");
    let token = auth_token();
    let initial_url = url.to_string();
    let build_request = |client: &reqwest::Client, request_url: &str| {
        let mut req = client
            .get(request_url)
            .header(reqwest::header::RANGE, range_value.clone());
        if same_origin(&initial_url, request_url) {
            if let Some(t) = &token {
                req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}"));
            }
        }
        req
    };

    let send_fut = send_with_hop_timeout(client, &initial_url, build_request, connect_timeout);
    let mut resp = match permit.race_preemption(send_fut).await {
        Err(()) => return SegmentAttemptOutcome::Preempted,
        Ok(Err(reason)) => return SegmentAttemptOutcome::Failed(0, reason),
        Ok(Ok(r)) => r,
    };
    // A range request MUST come back as 206 Partial Content. A 200 means
    // the server (or an edge in this client's own fresh redirect chain)
    // ignored the `Range` header and is streaming the WHOLE file; writing
    // that at this segment's offset would clobber every other segment's
    // region of the shared preallocated file. Reject anything but 206 --
    // the segment retries, and if the origin genuinely can't serve ranges
    // the whole segmented attempt exhausts and falls back to a correct
    // single stream (which handles 200 fine).
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return SegmentAttemptOutcome::Failed(
            0,
            format!("expected 206, got HTTP {}", resp.status()),
        );
    }

    // A 206 alone is not enough evidence that this is the tail we asked
    // for.  In particular, after a stalled first attempt `start` may be in
    // the middle of a segment.  Accepting a stale `Content-Range` then would
    // overwrite the already verified prefix at the wrong file offset.  Treat
    // every absent, malformed, shifted, or differently-sized range as a
    // protocol failure; the enclosing segmented attempt abandons its whole
    // temporary file and drains the original full response instead.
    let cap = end_inclusive.saturating_sub(start) + 1;
    if let Err(reason) = validate_content_range(&resp, start, end_inclusive, cap) {
        return SegmentAttemptOutcome::Failed(0, reason);
    }

    // First payload byte is imminent: PENDING -> STREAMING, permanently
    // non-preemptible from here (module docs "Permit preemption").
    permit.mark_streaming();

    // Never write more than this range's own byte count. Defense in depth
    // against a 206 that still over-streams past the requested end: a
    // misbehaving server can never spill bytes into a neighbouring
    // segment's region. `start <= end_inclusive` always holds (resume
    // never advances past the segment end), so `cap >= 1`.
    let mut offset = start;
    let mut written = 0u64;
    loop {
        let chunk = match tokio::time::timeout(stall_timeout, resp.chunk()).await {
            Ok(Ok(Some(c))) => c,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                return SegmentAttemptOutcome::Failed(written, format!("read failed: {e}"))
            }
            Err(_) => {
                return SegmentAttemptOutcome::Failed(
                    written,
                    format!("stalled: no progress for {stall_timeout:?}"),
                )
            }
        };
        let remaining = cap - written;
        let take = (chunk.len() as u64).min(remaining) as usize;
        if take > 0 {
            if let Err(e) = write_at_all(file, &chunk[..take], offset) {
                return SegmentAttemptOutcome::Failed(written, format!("write failed: {e}"));
            }
            offset += take as u64;
            written += take as u64;
        }
        if written >= cap {
            break;
        }
    }
    SegmentAttemptOutcome::Completed(written)
    // `permit` drops here on every path (RAII) -- this is the permit-leak
    // guarantee for stall-watchdog kills, preemption, and normal returns
    // alike.
}

fn validate_content_range(
    response: &reqwest::Response,
    expected_start: u64,
    expected_end: u64,
    expected_len: u64,
) -> Result<(), String> {
    let raw = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| "206 response missing or malformed Content-Range".to_string())?;
    let raw = raw
        .strip_prefix("bytes ")
        .ok_or_else(|| format!("invalid Content-Range {raw:?}"))?;
    let (range, total) = raw
        .split_once('/')
        .ok_or_else(|| format!("invalid Content-Range {raw:?}"))?;
    let (start, end) = range
        .split_once('-')
        .ok_or_else(|| format!("invalid Content-Range {raw:?}"))?;
    let start = start
        .parse::<u64>()
        .map_err(|_| format!("invalid Content-Range {raw:?}"))?;
    let end = end
        .parse::<u64>()
        .map_err(|_| format!("invalid Content-Range {raw:?}"))?;
    let total = total
        .parse::<u64>()
        .map_err(|_| format!("invalid Content-Range {raw:?}"))?;
    if start != expected_start || end != expected_end || total <= end {
        return Err(format!(
            "Content-Range bytes {start}-{end}/{total} does not match requested bytes {expected_start}-{expected_end}"
        ));
    }
    if response.content_length() != Some(expected_len) {
        return Err(format!(
            "Content-Length {:?} does not match requested range length {expected_len}",
            response.content_length()
        ));
    }
    Ok(())
}

fn sha256_of_file(file: &std::fs::File) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

// ---- positional (pwrite/seek_write) file I/O so N concurrent segment
// ---- tasks can share one file handle without a shared cursor. ----

fn write_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    crate::platform::fs::positioned_io::write_at(file, buf, offset)
}

fn write_at_all(file: &std::fs::File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        let n = write_at(file, buf, offset)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "write_at wrote 0 bytes",
            ));
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

/// Split `[0, total)` into `n` non-overlapping, contiguous segments,
/// distributing the `total % n` remainder one byte at a time to the first
/// segments. Every byte in `[0, total)` is covered by exactly one segment;
/// no segment is ever empty when `total >= n`.
fn compute_segments(total: u64, n: u32) -> Vec<(u64, u64)> {
    if total == 0 || n == 0 {
        return Vec::new();
    }
    let n = n as u64;
    let base = total / n;
    let remainder = total % n;
    let mut segments = Vec::with_capacity(n as usize);
    let mut cursor = 0u64;
    for i in 0..n {
        let len = base + if i < remainder { 1 } else { 0 };
        if len == 0 {
            continue;
        }
        let start = cursor;
        let end_inclusive = start + len - 1;
        segments.push((start, end_inclusive));
        cursor += len;
    }
    segments
}

#[cfg(test)]
#[path = "segmented_download_tests.rs"]
mod tests;
