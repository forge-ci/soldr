//! Toolchain-catalogue first asset resolver.
//!
//! Before consulting `api.github.com/repos/<owner>/<repo>/releases/...`
//! the release-asset resolver consults the v1 catalogue published by
//! `zackees/soldr-toolchain` and served over GitHub Pages:
//!
//! ```text
//! https://zackees.github.io/soldr-toolchain/catalogue.v1.json
//! ```
//!
//! Migration (soldr#988): the legacy manifest-branch origin and its
//! nightly job were retired in Phase 5.
//! There is no fallback to a second URL — a catalogue miss degrades
//! straight to the live GitHub Releases API. Use the catalogue's own
//! producer (`scripts/build_catalogue_v1.py` on `zackees/soldr-toolchain`)
//! to publish new entries.
//!
//! Schema (intentionally flat — one row per (owner, repo, tag, asset) —
//! so the lookup is a single linear scan and the file is grep-friendly):
//!
//! ```json
//! {
//!   "entries": [
//!     {
//!       "owner": "zackees",
//!       "repo":  "zccache",
//!       "tag":   "1.12.9",
//!       "asset": "zccache-x86_64-pc-windows-msvc.zip",
//!       "url":   "https://github.com/zackees/zccache/releases/download/1.12.9/zccache-x86_64-pc-windows-msvc.zip",
//!       "sha256": "0000...64hex"
//!     }
//!   ]
//! }
//! ```
//!
//! The index is fetched at most once per *resolved configuration* and
//! cached in a bounded, single-flight, process-wide LRU keyed on that
//! configuration (soldr#2951). Any failure — HTTP error, parse error,
//! timeout, network — degrades silently to an empty index so the
//! resolver falls back to the live GitHub Releases API. The integrity
//! invariant ("a hit MUST sha256-verify against the manifest's pin") is
//! the same trust posture as `apple_sdk` / `zig`: a mismatched download
//! is a hard error regardless of `SOLDR_TRUST_MODE`.
//!
//! ## Env-var seams
//!
//! * `SOLDR_TOOLCHAIN_ORIGIN` — override the catalogue origin (the
//!   resolver builds `{origin}/catalogue.v1.json`). See Phase 2.
//! * `SOLDR_TOOLCHAIN_CATALOGUE_URL` — override the full URL (testing
//!   + air-gapped mirrors). When set takes precedence over
//!     `SOLDR_TOOLCHAIN_ORIGIN`.
//! * `SOLDR_MANIFEST_DISABLE=1` — skip the catalogue lookup entirely;
//!   the resolver falls through to the live GitHub Releases API.
//!
//! ## Why a separate file from per-tool manifests?
//!
//! soldr-toolchain also publishes human-friendly per-tool manifests at
//! `<origin>/<tool>/manifest.json`. `catalogue.v1.json` is the flat,
//! sha-bearing index used by runtime fetches that need a single
//! `(owner, repo, tag, asset)` lookup before falling back to the live
//! GitHub Releases API.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::catalogue_model::{
    authoritative_v2_index, bind_v2_publication_state, fail_closed_v2_index, CatalogueSource,
    ManifestIndex,
};
use super::catalogue_transport::{
    cache_busted_url, materialize_catalogue_entry, verify_catalogue_asset_sha256,
};
use crate::core::{SoldrError, SoldrPaths};
use crate::platform::sync::Mutex;

/// soldr#988 Phase 2 — default origin of the v1 catalogue document
/// published by `zackees/soldr-toolchain`. The full URL we fetch is
/// `{origin}/catalogue.v1.json`. Overridable via
/// [`TOOLCHAIN_ORIGIN_ENV_VAR`].
pub const DEFAULT_TOOLCHAIN_ORIGIN: &str = "https://zackees.github.io/soldr-toolchain";

/// soldr#988 Phase 2 — env var that overrides
/// [`DEFAULT_TOOLCHAIN_ORIGIN`]. Set to an `https://` URL (no
/// trailing slash). Test seam + air-gapped-mirror seam.
pub const TOOLCHAIN_ORIGIN_ENV_VAR: &str = "SOLDR_TOOLCHAIN_ORIGIN";

/// soldr#988 Phase 5 — full-URL override for the catalogue endpoint.
/// When set takes precedence over [`TOOLCHAIN_ORIGIN_ENV_VAR`]'s
/// origin+filename composition. Test seam — lets integration tests
/// point at a one-shot HTTP listener whose path doesn't match the
/// canonical `/catalogue.v1.json`.
pub const TOOLCHAIN_CATALOGUE_URL_ENV_VAR: &str = "SOLDR_TOOLCHAIN_CATALOGUE_URL";

/// Catalogue document name. Producers on `zackees/soldr-toolchain`
/// emit this filename under the configured origin; consumers GET
/// `{origin}/{CATALOGUE_DOC_NAME}`.
pub const CATALOGUE_DOC_NAME: &str = "catalogue.v1.json";
/// Canonical multipart-aware catalogue document, tried before v1.
pub const CATALOGUE_V2_DOC_NAME: &str = "catalogue.v2.json";
/// Explicit v2 endpoint override.  The legacy URL override remains v1-only.
pub const TOOLCHAIN_CATALOGUE_V2_URL_ENV_VAR: &str = "SOLDR_TOOLCHAIN_CATALOGUE_V2_URL";
/// Highest catalogue transport capability implemented by this Soldr build.
pub const CATALOGUE_CAPABILITY: u32 = 2;
pub const MAX_CATALOGUE_PARTS: usize = 4096;
pub const MAX_CATALOGUE_ASSET_BYTES: u64 = 8 * 1024 * 1024 * 1024 * 1024;
pub const MAX_CATALOGUE_PART_BYTES: u64 = 95 * 1024 * 1024;
pub const MAX_CATALOGUE_URL_BYTES: usize = 8192;

/// Env var that, when set to a non-empty value other than `0`,
/// disables the catalogue lookup entirely. Resolution skips straight
/// to the live GitHub Releases API. Escape hatch for the rare case
/// where the published catalogue is wrong and a user needs to bypass
/// it without waiting for a soldr release. Name preserved across the
/// soldr#988 retirement so the user-facing env var stays stable.
pub const MANIFEST_DISABLE_ENV_VAR: &str = "SOLDR_MANIFEST_DISABLE";

/// Wall-clock budget for the one-shot manifest fetch. Generous enough
/// for slow networks; tight enough that an unreachable manifest cannot
/// wedge a build.
pub const MANIFEST_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Download one non-archive catalogue asset and enforce the catalogue's
/// SHA-256 pin. This is used for small metadata objects (for example the
/// Rust nightly-version map) that are published beside the catalogue.
pub async fn fetch_verified_catalogue_asset(
    paths: &SoldrPaths,
    owner: &str,
    repo: &str,
    tag: &str,
    asset: &str,
) -> Result<Vec<u8>, SoldrError> {
    // soldr#2951: one snapshot serves both the index lookup and the
    // cache-busted refresh below. Re-resolving the endpoint from the
    // environment for the refresh let it dial a *different* configuration
    // than the one that supplied the pin being re-checked, which defeats the
    // generation binding this verification exists to enforce.
    let config = CatalogueConfig::snapshot();
    let index = get_or_fetch_for(&config).await;
    let entry = index
        .lookup(owner, repo, tag, asset)
        .cloned()
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "catalogue has no asset row for {owner}/{repo} {tag}/{asset}"
            ))
        })?;
    let downloaded = materialize_catalogue_entry(paths, &entry).await?;
    if verify_catalogue_asset_sha256(&entry, downloaded.sha256()).is_ok() {
        return std::fs::read(downloaded.path()).map_err(SoldrError::from);
    }

    // The Pages asset and catalogue are updated in one assets-branch commit,
    // but CDN edges can briefly serve generations from opposite sides of that
    // commit. Refetch both objects once with cache-busters and require the new
    // catalogue digest; unverified bytes are never returned.
    let refreshed = match (index.source, &config) {
        // Once a v2 generation supplied the pin, only a refreshed v2
        // generation may replace it.  Never turn a CDN mismatch into a v1
        // request, which could mix publication generations.
        (CatalogueSource::CanonicalV2, CatalogueConfig::CanonicalV2 { v2_url, .. }) => {
            fetch_v2_index_from(&cache_busted_url(v2_url)).await?
        }
        (CatalogueSource::LegacyV1, CatalogueConfig::LegacyV1 { url }) => {
            fetch_v1_index_from(&cache_busted_url(url)).await?
        }
        // A v2 request that fell back on 404/410 is served by this snapshot's
        // own v1 URL, resolved at snapshot time precisely so the fallback
        // cannot come from a later environment state.
        (CatalogueSource::LegacyV1, CatalogueConfig::CanonicalV2 { v1_url, .. }) => {
            fetch_v1_index_from(&cache_busted_url(v1_url)).await?
        }
        // A pin can only have come from a document this configuration itself
        // fetched, so the remaining pairs mean the index and the snapshot
        // disagree. Refusing beats guessing which endpoint to dial.
        (_, CatalogueConfig::Disabled)
        | (CatalogueSource::CanonicalV2, CatalogueConfig::LegacyV1 { .. }) => {
            return Err(SoldrError::Other(format!(
                "catalogue pin for {owner}/{repo} {tag}/{asset} did not verify, and the \
                 resolved catalogue configuration cannot refresh the publication \
                 generation that supplied it"
            )))
        }
    };
    let refreshed_entry = refreshed.lookup(owner, repo, tag, asset).ok_or_else(|| {
        SoldrError::Other(format!(
            "refreshed catalogue has no asset row for {owner}/{repo} {tag}/{asset}"
        ))
    })?;
    let refreshed = materialize_catalogue_entry(paths, refreshed_entry).await?;
    verify_catalogue_asset_sha256(refreshed_entry, refreshed.sha256())?;
    std::fs::read(refreshed.path()).map_err(SoldrError::from)
}

/// The complete resolved catalogue configuration: every decision that changes
/// *what* gets fetched, captured in one value.
///
/// soldr#2951: the first fix keyed the cache on a `String` derived from the
/// environment, then dropped the lock, awaited, and read the environment
/// *again* to decide what to fetch. Between those two reads the configuration
/// could change, binding one configuration's key to another configuration's
/// bytes -- a corruption indistinguishable, from the caller's side, from a
/// stale cache. Snapshotting once removes the window rather than narrowing it:
/// downstream code is handed this value and has no way to consult the
/// environment at all.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CatalogueConfig {
    /// [`MANIFEST_DISABLE_ENV_VAR`] is truthy. Not a place you can fetch
    /// from, so it is its own identity and must never be satisfied by a
    /// URL's entry.
    Disabled,
    /// [`TOOLCHAIN_CATALOGUE_URL_ENV_VAR`] is set. Deliberately v1-only: an
    /// explicit air-gapped/test URL must never be reinterpreted as a v2
    /// document.
    LegacyV1 { url: String },
    /// The canonical path: try v2, fall back to v1 on 404/410.
    CanonicalV2 { v2_url: String, v1_url: String },
}

impl CatalogueConfig {
    /// Read each catalogue environment variable exactly once and resolve the
    /// configuration they describe.
    ///
    /// Precedence is the historical one, unchanged: disable beats everything;
    /// otherwise an explicit [`TOOLCHAIN_CATALOGUE_URL_ENV_VAR`] beats the
    /// origin composition; otherwise the origin composes both documents.
    ///
    /// `v1_url` is resolved eagerly on the [`CatalogueConfig::CanonicalV2`]
    /// arm even though only a 404/410 fallback uses it. That is the point --
    /// the fallback endpoint must come from the same snapshot as the primary,
    /// not from an environment read taken after the v2 response arrived.
    fn snapshot() -> Self {
        if disabled_via_env() {
            return Self::Disabled;
        }
        if let Some(url) = trimmed_env(TOOLCHAIN_CATALOGUE_URL_ENV_VAR) {
            return Self::LegacyV1 { url };
        }
        let origin = resolve_toolchain_origin();
        Self::CanonicalV2 {
            v2_url: trimmed_env(TOOLCHAIN_CATALOGUE_V2_URL_ENV_VAR)
                .unwrap_or_else(|| format!("{origin}/{CATALOGUE_V2_DOC_NAME}")),
            v1_url: format!("{origin}/{CATALOGUE_DOC_NAME}"),
        }
    }
}

/// The trimmed value of `name`, or `None` when it is unset or blank.
///
/// One helper rather than a chain repeated per variable, so "set but blank
/// counts as unset" is decided in exactly one place for every catalogue
/// endpoint override (soldr#2740).
fn trimmed_env(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// How many distinct configurations stay resident. Every real run uses
/// exactly one; a test binary walks through a handful.
pub const MAX_CACHED_CONFIGS: usize = 8;

/// Parsed catalogues, most-recently-used first, keyed by the configuration
/// that produced them.
///
/// soldr#2951: this was `OnceLock<Mutex<HashMap<String, &'static
/// ManifestIndex>>>` -- unbounded, with caller-selected string keys, and a
/// fresh `Box::leak` on every miss. Two concurrent misses each fetched and
/// each leaked a duplicate. `Arc` in a bounded `Vec` means an eviction
/// actually frees, and the bound is not a function of what callers ask for.
///
/// # Single-flight without a lock held across the fetch (soldr#3836)
///
/// Each configuration owns a [`CatalogueSlot`], a `OnceCell` initialised by
/// whichever caller reaches it first; concurrent callers on one configuration
/// await that one fetch and the second finds a hit. The cache lock itself is
/// a `soldr_platform::sync` critical section held only to find or insert the
/// slot -- never across [`fetch_for`], whose network retries can outlast the
/// lock watchdog's hold limit. It previously was a `crate::platform::sync::AsyncMutex` held
/// across the fetch, which serialised distinct configurations behind one
/// network round trip and would have deadlocked silently on re-entry.
///
/// Re-entry is still forbidden for one configuration: nothing reachable from
/// `fetch_for` may call [`get_or_fetch`] for the configuration it is
/// fetching, or that caller would await its own cell. Distinct
/// configurations no longer serialise against each other.
///
/// Most-recently-used first, so eviction is `truncate` and a hit is a
/// `remove` + `insert(0)`. Evicting a slot whose fetch is still in flight is
/// harmless: its waiters hold their own `Arc` and still receive the result.
type CatalogueEntries = Vec<(CatalogueConfig, CatalogueSlot)>;

/// One configuration's catalogue, fetched at most once per residency.
type CatalogueSlot = Arc<tokio::sync::OnceCell<Arc<ManifestIndex>>>;

fn promote_cached<V: Clone>(
    entries: &mut Vec<(CatalogueConfig, V)>,
    config: &CatalogueConfig,
) -> Option<V> {
    let position = entries.iter().position(|(cached, _)| cached == config)?;
    let hit = entries.remove(position);
    let value = hit.1.clone();
    entries.insert(0, hit);
    Some(value)
}

fn insert_cached<V>(entries: &mut Vec<(CatalogueConfig, V)>, config: CatalogueConfig, value: V) {
    entries.insert(0, (config, value));
    entries.truncate(MAX_CACHED_CONFIGS);
}

static CATALOGUE_CACHE: OnceLock<Mutex<CatalogueEntries>> = OnceLock::new();

fn catalogue_cache() -> &'static Mutex<CatalogueEntries> {
    CATALOGUE_CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Number of distinct configurations currently resident.
///
/// Test seam for the [`MAX_CACHED_CONFIGS`] bound, which is otherwise
/// unobservable from outside the module -- and an unobservable bound is one
/// nobody notices regressing. Production code has no reason to ask.
#[doc(hidden)]
pub async fn cached_catalogue_config_count() -> usize {
    catalogue_cache().lock().len()
}

/// True when [`MANIFEST_DISABLE_ENV_VAR`] is set to a recognised soldr "on"
/// spelling (the canonical `crate::core::flag` contract, soldr#2740/#3638).
/// Absent, empty, `0`/`false`/`no`/`off`, and any unrecognised value keep the
/// sha-pinned manifest enabled -- a typo must never route fetches to the
/// unpinned GitHub API path.
fn disabled_via_env() -> bool {
    disabled_from_value(std::env::var(MANIFEST_DISABLE_ENV_VAR).ok().as_deref())
}

/// Pure counterpart of [`disabled_via_env`] for tests.
fn disabled_from_value(value: Option<&str>) -> bool {
    value.is_some_and(crate::core::flag_value)
}

/// soldr#988 Phase 2 — resolve the catalogue origin, honoring
/// [`TOOLCHAIN_ORIGIN_ENV_VAR`]. Trailing slash is stripped so the
/// caller can append `/catalogue.v1.json` unconditionally.
pub fn resolve_toolchain_origin() -> String {
    trimmed_env(TOOLCHAIN_ORIGIN_ENV_VAR)
        .unwrap_or_else(|| DEFAULT_TOOLCHAIN_ORIGIN.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// soldr#988 — full URL of the catalogue document.
/// [`TOOLCHAIN_CATALOGUE_URL_ENV_VAR`] takes precedence (test seam +
/// air-gapped-mirror seam); otherwise we compose
/// `{resolve_toolchain_origin()}/catalogue.v1.json`.
///
/// Retained for the one-shot `soldr toolchain catalogue` verb and for
/// callers outside this module. The fetch path resolves its endpoints
/// through the module-private `CatalogueConfig::snapshot` instead, so
/// identity and fetch cannot disagree (soldr#2951).
pub fn resolve_catalogue_url() -> String {
    trimmed_env(TOOLCHAIN_CATALOGUE_URL_ENV_VAR)
        .unwrap_or_else(|| format!("{}/{}", resolve_toolchain_origin(), CATALOGUE_DOC_NAME))
}

/// The canonical v2 endpoint; unlike the legacy override this must not
/// reinterpret an explicit v1 URL as a v2 document.
pub fn resolve_catalogue_v2_url() -> String {
    trimmed_env(TOOLCHAIN_CATALOGUE_V2_URL_ENV_VAR)
        .unwrap_or_else(|| format!("{}/{}", resolve_toolchain_origin(), CATALOGUE_V2_DOC_NAME))
}

/// Fetch the catalogue body once per resolved configuration, caching the
/// parsed index in the module-private, bounded, single-flight catalogue cache.
/// On any error (disable env var, HTTP failure, network failure, timeout,
/// parse failure) caches an empty index so the rest of the process makes no
/// further network calls for that configuration and the resolver falls through
/// to the live GitHub Releases API.
///
/// Returns a shared handle to the cached [`ManifestIndex`]. `Arc<T>` derefs
/// to `T`, so a caller that only reads fields is unchanged; a caller passing
/// it to a `&ManifestIndex` parameter must bind it first.
pub async fn get_or_fetch() -> Arc<ManifestIndex> {
    // soldr#2951: one snapshot, taken before the lock and never revisited.
    // Identity and fetch are the same value, so no mutation between them can
    // bind one configuration's key to another configuration's bytes.
    let config = CatalogueConfig::snapshot();
    get_or_fetch_for(&config).await
}

/// [`get_or_fetch`] against a configuration the caller has already
/// snapshotted, so a caller needing the configuration for its own decisions
/// cannot end up with a second, different one.
async fn get_or_fetch_for(config: &CatalogueConfig) -> Arc<ManifestIndex> {
    // Move-to-front on a hit: with `MAX_CACHED_CONFIGS` this small, recency
    // is the only eviction signal worth keeping. The guard drops at the end
    // of this block, before any await (see [`CatalogueEntries`]).
    let slot = {
        let mut cache = catalogue_cache().lock();
        promote_cached(&mut cache, config).unwrap_or_else(|| {
            let slot = CatalogueSlot::default();
            insert_cached(&mut cache, config.clone(), Arc::clone(&slot));
            slot
        })
    };
    let index = slot
        .get_or_init(|| async { Arc::new(fetch_for(config).await) })
        .await;
    Arc::clone(index)
}

/// Fetch the catalogue this configuration names, degrading to an empty index.
///
/// Contains no environment reads at all: the snapshot is the only input, so
/// what is fetched cannot drift from what the cache entry is keyed on
/// (soldr#2951).
async fn fetch_for(config: &CatalogueConfig) -> ManifestIndex {
    if matches!(config, CatalogueConfig::Disabled) {
        return ManifestIndex::empty();
    }
    // soldr#2132: retry before giving up. Falling back to an empty index
    // is a *permanent* decision for this configuration -- it drops the sha256
    // pins and makes every later syslib lookup report "not yet ingested" -- so
    // a single truncated response body must not be enough to trigger it.
    // That is what failed two lanes of the v0.8.30 release build.
    let attempt = || fetch_configured_once(config);
    match super::retry::with_backoff("the soldr-toolchain catalogue", attempt).await {
        Ok(index) => index,
        Err(err) => {
            // soldr#2132 item 4. This used to be `.unwrap_or_else(|_|
            // empty())` -- the error was discarded and the process carried
            // on with no catalogue and no message. The consequence lands
            // much later and looks unrelated: syslib lookups report "not
            // yet ingested", and a missing sysroot surfaces as rustc's
            // `can't find crate for std`. Naming it here is the difference
            // between a two-minute diagnosis and an hour of one.
            eprintln!(
                "soldr: warning: the soldr-toolchain catalogue is unavailable \
                 after {} attempts: {err}",
                super::retry::FETCH_ATTEMPTS
            );
            eprintln!(
                "soldr: warning: continuing without it. sha256 pins are \
                 unavailable for this run, and catalogue-provided sysroots \
                 and tools will not resolve -- which can surface later as an \
                 apparently unrelated compile error."
            );
            ManifestIndex::empty()
        }
    }
}

/// One attempt at the configured catalogue, used by [`fetch_for`] under the
/// retry wrapper. soldr#988 Phase 5: the legacy `manifest`-branch fallback was
/// removed along with its origin and refresh workflow.
async fn fetch_configured_once(config: &CatalogueConfig) -> Result<ManifestIndex, SoldrError> {
    match config {
        // Unreachable -- [`fetch_for`] short-circuits before the retry wrapper
        // so a disabled catalogue costs no attempt and no backoff. Spelled out
        // rather than `unreachable!()` because degrading is the module's
        // universal failure posture and a panic here would be the one place
        // it does not hold.
        CatalogueConfig::Disabled => Ok(ManifestIndex::empty()),
        // This override is intentionally *only* a v1 override.  Existing
        // air-gapped and test callers must not have their explicit URL
        // reinterpreted as a v2 endpoint.
        CatalogueConfig::LegacyV1 { url } => fetch_v1_index_from(url).await,
        CatalogueConfig::CanonicalV2 { v2_url, v1_url } => {
            let safe_v2_url = super::stream_download::safe_asset_url(v2_url);
            let client =
                super::stream_download::control_http_client("the soldr-toolchain catalogue")?;
            let resp = super::stream_download::send_control_request_with_timeout(
                super::stream_download::get_request(&client, v2_url),
                &safe_v2_url,
                MANIFEST_FETCH_TIMEOUT,
            )
            .await?;
            match resp.status().as_u16() {
                status if should_fallback_to_v1(status) => fetch_v1_index_from(v1_url).await,
                status if (200..300).contains(&status) => {
                    let body = super::stream_download::read_control_text(
                        resp,
                        &safe_v2_url,
                        MANIFEST_FETCH_TIMEOUT,
                    )
                    .await?;
                    // A present v2 response is authoritative: malformed,
                    // unknown, or semantically invalid data must never
                    // silently revive v1/live API.
                    let parsed = ManifestIndex::from_v2_json(&body);
                    let state_bound =
                        parsed.is_some() && bind_v2_publication_state(&body).await.is_ok();
                    Ok(authoritative_v2_index(parsed, state_bound))
                }
                _ => Ok(fail_closed_v2_index()),
            }
        }
    }
}

pub(super) fn should_fallback_to_v1(status: u16) -> bool {
    matches!(status, 404 | 410)
}

/// HTTP-GET + JSON-parse a single index URL. Shared by both the v1
/// catalogue origin and the legacy `manifest` branch URL. The
/// `ManifestIndex` deserializer ignores unknown top-level fields
/// (e.g. v1's `schema_version`, `generated_at`, `origin`), so the
/// same struct cleanly absorbs both shapes.
async fn fetch_v1_index_from(url: &str) -> Result<ManifestIndex, SoldrError> {
    let safe_url = super::stream_download::safe_asset_url(url);
    let client = super::stream_download::control_http_client("the soldr-toolchain catalogue")?;
    let resp = super::stream_download::send_control_request_with_timeout(
        super::stream_download::get_request(&client, url),
        &safe_url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "manifest fetch {safe_url} returned HTTP {}",
            resp.status()
        )));
    }
    let body =
        super::stream_download::read_control_text(resp, &safe_url, MANIFEST_FETCH_TIMEOUT).await?;
    ManifestIndex::from_v1_json(&body)
        .ok_or_else(|| SoldrError::Other(format!("manifest {safe_url} did not parse as JSON")))
}

/// Fetch a canonical v2 document.  This never falls through to v1: it is
/// used after a v2-selected pin mismatch to keep catalogue and payload bound
/// to one publication generation.
async fn fetch_v2_index_from(url: &str) -> Result<ManifestIndex, SoldrError> {
    let safe_url = super::stream_download::safe_asset_url(url);
    let client = super::stream_download::control_http_client("the soldr-toolchain catalogue")?;
    let resp = super::stream_download::send_control_request_with_timeout(
        super::stream_download::get_request(&client, url),
        &safe_url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "manifest fetch {safe_url} returned HTTP {}",
            resp.status()
        )));
    }
    let body =
        super::stream_download::read_control_text(resp, &safe_url, MANIFEST_FETCH_TIMEOUT).await?;
    let index = ManifestIndex::from_v2_json(&body).ok_or_else(|| {
        SoldrError::Other(format!("canonical v2 manifest {safe_url} did not validate"))
    })?;
    bind_v2_publication_state(&body).await?;
    Ok(index)
}

/// soldr#988 Phase 2 — `soldr toolchain catalogue` verb. Fetches
/// the catalogue's HEAD-equivalent metadata (one cheap GET, body
/// streamed only as needed for the entry count) and prints either a
/// human-readable summary or the stable JSON form.
pub async fn run_toolchain_catalogue(json: bool) -> Result<i32, SoldrError> {
    let url = resolve_catalogue_url();
    let safe_url = super::stream_download::safe_asset_url(&url);
    let client = super::stream_download::control_http_client("the soldr-toolchain catalogue")?;
    let resp_result = super::stream_download::send_control_request_with_timeout(
        super::stream_download::get_request(&client, &url),
        &safe_url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await;

    match resp_result {
        Err(e) => {
            print_catalogue_error(&safe_url, &format!("network error: {e}"), json);
            Ok(1)
        }
        Ok(resp) => {
            let status = resp.status();
            let etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let last_modified = resp
                .headers()
                .get("last-modified")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let content_length = resp
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            if !status.is_success() {
                print_catalogue_error(
                    &safe_url,
                    &format!(
                        "HTTP {} (the catalogue file may not be published yet)",
                        status
                    ),
                    json,
                );
                return Ok(1);
            }
            let body = match super::stream_download::read_control_text(
                resp,
                &safe_url,
                MANIFEST_FETCH_TIMEOUT,
            )
            .await
            {
                Ok(b) => b,
                Err(e) => {
                    print_catalogue_error(&safe_url, &format!("body read error: {e}"), json);
                    return Ok(1);
                }
            };
            let parsed = ManifestIndex::from_json(&body);
            let n_entries = parsed.as_ref().map(|i| i.entries.len()).unwrap_or(0);
            if json {
                let payload = serde_json::json!({
                    "schema_version": 1,
                    "origin": super::stream_download::safe_asset_url(&resolve_toolchain_origin()),
                    "url": safe_url,
                    "http_status": status.as_u16(),
                    "content_length": content_length,
                    "etag": etag,
                    "last_modified": last_modified,
                    "entries": n_entries,
                });
                println!("{}", serde_json::to_string(&payload).unwrap_or_default());
            } else {
                println!("soldr-toolchain catalogue:");
                println!("  url:            {safe_url}");
                println!("  http status:    {}", status.as_u16());
                if let Some(len) = content_length {
                    println!("  content-length: {len}");
                }
                if let Some(etag) = etag {
                    println!("  etag:           {etag}");
                }
                if let Some(lm) = last_modified {
                    println!("  last-modified:  {lm}");
                }
                println!("  entries:        {n_entries}");
            }
            Ok(0)
        }
    }
}

fn print_catalogue_error(url: &str, reason: &str, json: bool) {
    if json {
        let payload = serde_json::json!({
            "schema_version": 1,
            "origin": super::stream_download::safe_asset_url(&resolve_toolchain_origin()),
            "url": url,
            "error": reason,
        });
        eprintln!("{}", serde_json::to_string(&payload).unwrap_or_default());
    } else {
        eprintln!("soldr toolchain catalogue: {url} — {reason}");
    }
}

// NOTE: there is deliberately no `reset_cache_for_tests` helper. The cache is
// keyed on the immutable `CatalogueConfig` snapshot (soldr#2951), so a test
// that points `SOLDR_TOOLCHAIN_CATALOGUE_URL` at its own one-shot server gets
// its own entry, and one that disables the catalogue gets the `Disabled`
// entry -- in any binary, under any runner. A reset hook would be a second
// way to reach the same state, and the first fix here already showed what two
// paths to one decision cost.
//
// What that does NOT buy is freedom from ordering: the key is derived from
// process environment, so a test mutating those variables still races every
// concurrent test in the same binary. That is why the integration tests take
// `common::catalogue_env::CatalogueEnvGuard`, which serialises the catalogue
// variables and restores their prior values -- including "was unset" -- on
// drop. An earlier note here claimed a fresh cache required "their own
// integration test binary"; soldr#2934 consolidated per-file binaries into
// category targets and made that premise false.
//
// The cache unit test below uses a fresh local entry list and
// never touches the process-wide cache.

#[cfg(test)]
mod lru_tests {
    use super::*;

    fn config(number: usize) -> CatalogueConfig {
        CatalogueConfig::LegacyV1 {
            url: format!("https://catalogue.invalid/{number}.json"),
        }
    }

    #[test]
    fn true_lru_evicts_oldest_and_promotes_hits() {
        let configs: Vec<_> = (0..MAX_CACHED_CONFIGS + 2).map(config).collect();
        let indexes: Vec<_> = configs
            .iter()
            .map(|_| Arc::new(ManifestIndex::empty()))
            .collect();
        let mut entries: Vec<(CatalogueConfig, Arc<ManifestIndex>)> = Vec::new();

        for position in 0..MAX_CACHED_CONFIGS {
            insert_cached(
                &mut entries,
                configs[position].clone(),
                Arc::clone(&indexes[position]),
            );
        }

        let promoted = promote_cached(&mut entries, &configs[0]).expect("oldest is resident");
        assert!(Arc::ptr_eq(&promoted, &indexes[0]));

        insert_cached(
            &mut entries,
            configs[MAX_CACHED_CONFIGS].clone(),
            Arc::clone(&indexes[MAX_CACHED_CONFIGS]),
        );
        assert!(
            promote_cached(&mut entries, &configs[1]).is_none(),
            "the untouched oldest entry must be evicted"
        );
        let promoted = promote_cached(&mut entries, &configs[0])
            .expect("the promoted oldest entry must remain resident");
        assert!(Arc::ptr_eq(&promoted, &indexes[0]));

        insert_cached(
            &mut entries,
            configs[MAX_CACHED_CONFIGS + 1].clone(),
            Arc::clone(&indexes[MAX_CACHED_CONFIGS + 1]),
        );
        assert!(
            promote_cached(&mut entries, &configs[2]).is_none(),
            "the next untouched oldest entry must be evicted"
        );
        let promoted = promote_cached(&mut entries, &configs[0])
            .expect("the promoted entry must survive the next eviction");
        assert!(Arc::ptr_eq(&promoted, &indexes[0]));
        assert_eq!(entries.len(), MAX_CACHED_CONFIGS);
    }

    #[test]
    fn manifest_disable_uses_canonical_flag_parser_soldr_3638() {
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("off"),
            Some("OFF"),
            Some(" off "),
            Some("nope"),
        ] {
            assert!(
                !disabled_from_value(off),
                "{off:?} must keep the pinned manifest enabled"
            );
        }
        for on in [Some("1"), Some("true"), Some("YES")] {
            assert!(disabled_from_value(on), "{on:?} must disable the manifest");
        }
    }
}
