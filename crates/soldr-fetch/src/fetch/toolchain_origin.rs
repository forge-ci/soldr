//! Where the soldr-toolchain catalogue lives: the redirect manifest, its
//! on-disk cache, the built-in origin list, and per-request origin failover.
//!
//! The catalogue is served by GitHub Pages, and Pages URLs do not follow a
//! repository transfer. One transfer of `zackees/soldr-toolchain` 404'd every
//! released client for as long as it lasted, because the origin was a single
//! compiled-in constant. This module makes the origin a list that the server
//! side can change without a client release:
//!
//! 1. Explicit overrides win and skip the redirect entirely:
//!    `SOLDR_TOOLCHAIN_ORIGIN` (one origin), and the full-URL catalogue
//!    overrides handled by `catalogue_lookup`.
//! 2. Otherwise fetch the redirect manifest
//!    ([`DEFAULT_TOOLCHAIN_REDIRECT_URL`], overridable with
//!    `SOLDR_TOOLCHAIN_REDIRECT_URL`) with a short timeout. Setting
//!    `SOLDR_TOOLCHAIN_REDIRECT=0` skips the fetch and uses only the built-in
//!    list.
//! 3. A manifest is accepted only if it is JSON with `schema_version == 1`
//!    and a non-empty `toolchain.origins` list of credential-free `https://`
//!    URLs. Anything else is a fetch failure.
//! 4. The last good manifest is cached next to the catalogue cache
//!    (`<cache>/toolchain-redirect.v1.json`). On fetch failure the cached copy
//!    is used; without one, the built-in list. An unreachable manifest never
//!    fails a build.
//! 5. Effective origins = manifest origins, then [`BUILTIN_TOOLCHAIN_ORIGINS`],
//!    de-duplicated in order.
//! 6. A toolchain file is fetched by trying the origins in order, moving on
//!    only on a network error, a timeout, HTTP 404 or HTTP 5xx
//!    ([`fetch_from_origins`]). Content and integrity errors stay hard errors.
//!
//! Catalogue *content* (absolute asset URLs inside the catalogue) is not
//! rewritten here; only the location of the catalogue documents moves.
//!
//! setup-soldr implements the same rules, so the two clients agree on which
//! origin serves a run.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::core::{SoldrError, SoldrPaths};

/// The forge-ci mirror of the soldr-toolchain Pages site. First built-in
/// origin.
pub const FORGE_CI_TOOLCHAIN_ORIGIN: &str = "https://forge-ci.github.io/soldr-toolchain";

/// The original soldr-toolchain Pages site. Baked into every released client
/// before the redirect manifest existed, so it must stay reachable forever;
/// kept as the last built-in origin.
pub const LEGACY_TOOLCHAIN_ORIGIN: &str = "https://zackees.github.io/soldr-toolchain";

/// The built-in origin list, in preference order. Always appended after the
/// redirect manifest's own origins.
pub const BUILTIN_TOOLCHAIN_ORIGINS: [&str; 2] =
    [FORGE_CI_TOOLCHAIN_ORIGIN, LEGACY_TOOLCHAIN_ORIGIN];

/// The first built-in origin: what an offline-configured or redirect-disabled
/// client tries first.
pub const DEFAULT_TOOLCHAIN_ORIGIN: &str = BUILTIN_TOOLCHAIN_ORIGINS[0];

/// Where the redirect manifest is published: the root of the forge-ci org
/// Pages site, which no project Pages site can shadow.
pub const DEFAULT_TOOLCHAIN_REDIRECT_URL: &str =
    "https://forge-ci.github.io/soldr-redirect.v1.json";

/// Overrides [`DEFAULT_TOOLCHAIN_REDIRECT_URL`].
pub const TOOLCHAIN_REDIRECT_URL_ENV_VAR: &str = "SOLDR_TOOLCHAIN_REDIRECT_URL";

/// A soldr "off" spelling (`0`, `false`, `no`, `off`) disables the redirect
/// fetch; the built-in list is used as-is.
pub const TOOLCHAIN_REDIRECT_ENV_VAR: &str = "SOLDR_TOOLCHAIN_REDIRECT";

/// soldr#988 Phase 2 — overrides the whole origin list with exactly one
/// origin and skips the redirect manifest.
pub const TOOLCHAIN_ORIGIN_ENV_VAR: &str = "SOLDR_TOOLCHAIN_ORIGIN";

/// The only redirect manifest schema this client understands.
pub const REDIRECT_SCHEMA_VERSION: u64 = 1;

/// Wall-clock budget for the whole redirect manifest fetch. Short on purpose:
/// the manifest is an optimisation over the built-in list, not a dependency.
pub const REDIRECT_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// File name of the last good manifest under [`SoldrPaths::cache`], beside
/// the `catalogue-v2/` asset cache.
pub const REDIRECT_CACHE_FILE_NAME: &str = "toolchain-redirect.v1.json";

/// Upper bounds that keep a hostile or broken manifest from becoming a
/// resource problem. The real document is a few hundred bytes.
const MAX_REDIRECT_BODY_BYTES: usize = 64 * 1024;
const MAX_REDIRECT_ORIGINS: usize = 16;
const MAX_ORIGIN_URL_BYTES: usize = 2048;

/// How the origin list is decided. Resolved from the environment once (see
/// [`OriginPlan::from_env`]) so it can be part of a catalogue cache key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OriginPlan {
    /// `SOLDR_TOOLCHAIN_ORIGIN` is set: exactly this origin, no redirect.
    Explicit(String),
    /// The redirect is disabled or bypassed by another explicit override: the
    /// built-in list only.
    Builtin,
    /// Fetch the redirect manifest, falling back to the cache file, then to
    /// the built-in list.
    Redirect {
        manifest_url: String,
        cache_file: Option<PathBuf>,
    },
}

/// Where a resolved origin list came from. Reported by
/// `soldr toolchain catalogue` and asserted by tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginsSource {
    Explicit,
    Builtin,
    Manifest,
    Cache,
}

impl OriginsSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Builtin => "builtin",
            Self::Manifest => "redirect-manifest",
            Self::Cache => "redirect-cache",
        }
    }
}

/// The ordered, de-duplicated origins to try, and where they came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedOrigins {
    pub origins: Vec<String>,
    pub source: OriginsSource,
}

impl ResolvedOrigins {
    fn builtin() -> Self {
        Self {
            origins: effective_origins(&[]),
            source: OriginsSource::Builtin,
        }
    }
}

impl OriginPlan {
    /// Read the origin-related environment exactly once.
    pub fn from_env() -> Self {
        Self::from_values(
            trimmed_env(TOOLCHAIN_ORIGIN_ENV_VAR),
            std::env::var(TOOLCHAIN_REDIRECT_ENV_VAR).ok().as_deref(),
            trimmed_env(TOOLCHAIN_REDIRECT_URL_ENV_VAR),
            SoldrPaths::new()
                .ok()
                .map(|paths| redirect_cache_file(&paths)),
        )
    }

    /// Pure counterpart of [`OriginPlan::from_env`].
    pub fn from_values(
        explicit_origin: Option<String>,
        redirect_switch: Option<&str>,
        redirect_url: Option<String>,
        cache_file: Option<PathBuf>,
    ) -> Self {
        if let Some(origin) = explicit_origin {
            return Self::Explicit(strip_trailing_slashes(&origin));
        }
        if redirect_switch.is_some_and(crate::core::is_off_value) {
            return Self::Builtin;
        }
        Self::Redirect {
            manifest_url: redirect_url
                .unwrap_or_else(|| DEFAULT_TOOLCHAIN_REDIRECT_URL.to_string()),
            cache_file,
        }
    }

    /// The same plan with the redirect fetch removed. Used when another
    /// explicit override is in force: an explicit endpoint must not cause a
    /// redirect manifest request.
    pub fn without_redirect(self) -> Self {
        match self {
            Self::Redirect { .. } => Self::Builtin,
            other => other,
        }
    }

    /// Resolve the origin list. Never fails: the worst case is the built-in
    /// list.
    pub async fn resolve(&self) -> ResolvedOrigins {
        match self {
            Self::Explicit(origin) => ResolvedOrigins {
                origins: vec![origin.clone()],
                source: OriginsSource::Explicit,
            },
            Self::Builtin => ResolvedOrigins::builtin(),
            Self::Redirect {
                manifest_url,
                cache_file,
            } => resolve_redirect_origins(manifest_url, cache_file.as_deref()).await,
        }
    }
}

/// Path of the cached redirect manifest for this soldr root.
pub fn redirect_cache_file(paths: &SoldrPaths) -> PathBuf {
    paths.cache.join(REDIRECT_CACHE_FILE_NAME)
}

/// Fetch `manifest_url`, falling back to `cache_file`, then to the built-in
/// list. A good fetch refreshes the cache.
pub async fn resolve_redirect_origins(
    manifest_url: &str,
    cache_file: Option<&Path>,
) -> ResolvedOrigins {
    match fetch_redirect_manifest(manifest_url).await {
        Ok((body, origins)) => {
            if let Some(cache_file) = cache_file {
                // Best effort: a read-only cache must not cost the run its
                // freshly fetched origins.
                let _ = write_redirect_cache(cache_file, manifest_url, &body);
            }
            ResolvedOrigins {
                origins: effective_origins(&origins),
                source: OriginsSource::Manifest,
            }
        }
        Err(_) => match cache_file.and_then(|file| read_redirect_cache(file, manifest_url)) {
            Some(origins) => ResolvedOrigins {
                origins: effective_origins(&origins),
                source: OriginsSource::Cache,
            },
            None => ResolvedOrigins::builtin(),
        },
    }
}

/// One bounded attempt at the manifest. Returns the raw body (for the cache)
/// and its validated origins.
async fn fetch_redirect_manifest(manifest_url: &str) -> Result<(String, Vec<String>), SoldrError> {
    let attempt = async {
        let safe_url = super::stream_download::safe_asset_url(manifest_url);
        let client =
            super::stream_download::control_http_client("the soldr toolchain redirect manifest")?;
        let response = super::stream_download::send_control_request_with_timeout(
            super::stream_download::get_request(&client, manifest_url),
            &safe_url,
            REDIRECT_FETCH_TIMEOUT,
        )
        .await?;
        if !response.status().is_success() {
            return Err(SoldrError::Network(format!(
                "redirect manifest {safe_url} returned HTTP {}",
                response.status()
            )));
        }
        let body =
            super::stream_download::read_control_text(response, &safe_url, REDIRECT_FETCH_TIMEOUT)
                .await?;
        let origins = parse_redirect_manifest(&body).map_err(|reason| {
            SoldrError::Other(format!("redirect manifest {safe_url} rejected: {reason}"))
        })?;
        Ok((body, origins))
    };
    tokio::time::timeout(REDIRECT_FETCH_TIMEOUT, attempt)
        .await
        .map_err(|_| SoldrError::Network("redirect manifest fetch timed out".into()))?
}

/// Validate a redirect manifest and return its origins, trailing slashes
/// stripped. Any invalid origin rejects the whole document: a partially
/// trusted list is not a list the server meant to publish.
pub fn parse_redirect_manifest(body: &str) -> Result<Vec<String>, String> {
    if body.len() > MAX_REDIRECT_BODY_BYTES {
        return Err(format!("document exceeds {MAX_REDIRECT_BODY_BYTES} bytes"));
    }
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("not JSON: {error}"))?;
    let schema = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64);
    if schema != Some(REDIRECT_SCHEMA_VERSION) {
        return Err(format!(
            "schema_version must be {REDIRECT_SCHEMA_VERSION}, found {schema:?}"
        ));
    }
    let raw = value
        .get("toolchain")
        .and_then(|toolchain| toolchain.get("origins"))
        .and_then(serde_json::Value::as_array)
        .ok_or("toolchain.origins is missing or not a list")?;
    if raw.is_empty() {
        return Err("toolchain.origins is empty".into());
    }
    if raw.len() > MAX_REDIRECT_ORIGINS {
        return Err(format!(
            "toolchain.origins lists more than {MAX_REDIRECT_ORIGINS} origins"
        ));
    }
    raw.iter()
        .map(|origin| {
            origin
                .as_str()
                .ok_or_else(|| "toolchain.origins entry is not a string".to_string())
                .and_then(validate_origin)
        })
        .collect()
}

/// Accept only a bounded, credential-free, absolute `https://` origin with no
/// query or fragment, and normalise away trailing slashes.
fn validate_origin(raw: &str) -> Result<String, String> {
    if raw.is_empty() || raw.len() > MAX_ORIGIN_URL_BYTES {
        return Err("origin URL has an invalid length".into());
    }
    let url = url::Url::parse(raw).map_err(|error| format!("origin {raw:?}: {error}"))?;
    if url.scheme() != "https" {
        return Err(format!("origin {raw:?} is not https://"));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(format!("origin {raw:?} has no host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("origin URL carries credentials".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!("origin {raw:?} has a query or fragment"));
    }
    Ok(strip_trailing_slashes(raw))
}

/// Manifest origins first, then the built-in list, de-duplicated with order
/// preserved.
pub fn effective_origins(manifest_origins: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(manifest_origins.len() + 2);
    let candidates = manifest_origins
        .iter()
        .map(String::as_str)
        .chain(BUILTIN_TOOLCHAIN_ORIGINS);
    for origin in candidates {
        let origin = strip_trailing_slashes(origin);
        if !out.contains(&origin) {
            out.push(origin);
        }
    }
    out
}

fn strip_trailing_slashes(origin: &str) -> String {
    origin.trim().trim_end_matches('/').to_string()
}

/// The trimmed value of `name`, or `None` when it is unset or blank.
///
/// One helper for every toolchain endpoint override, so "set but blank counts
/// as unset" is decided in exactly one place (soldr#2740).
pub(super) fn trimmed_env(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Persist a validated manifest body, keyed by the URL that served it, so a
/// different `SOLDR_TOOLCHAIN_REDIRECT_URL` never reads another URL's copy.
/// Written through a temporary file and renamed so a reader never sees a torn
/// document.
pub fn write_redirect_cache(
    cache_file: &Path,
    manifest_url: &str,
    body: &str,
) -> Result<(), SoldrError> {
    let parent = cache_file
        .parent()
        .ok_or_else(|| SoldrError::Other("redirect cache path has no parent".into()))?;
    std::fs::create_dir_all(parent)?;
    let record = serde_json::json!({
        "manifest_url": manifest_url,
        "manifest": body,
    });
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut temporary, record.to_string().as_bytes())?;
    temporary
        .persist(cache_file)
        .map_err(|error| SoldrError::from(error.error))?;
    Ok(())
}

/// Read the cached manifest for `manifest_url`. Anything unreadable, written
/// for a different URL, or no longer valid is treated as absent.
pub fn read_redirect_cache(cache_file: &Path, manifest_url: &str) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(cache_file).ok()?;
    let record: serde_json::Value = serde_json::from_str(&text).ok()?;
    if record.get("manifest_url")?.as_str()? != manifest_url {
        return None;
    }
    parse_redirect_manifest(record.get("manifest")?.as_str()?).ok()
}

/// Why one origin could not serve a request.
#[derive(Debug)]
pub enum OriginError {
    /// The origin is unavailable (network error, timeout, HTTP 404 or 5xx):
    /// try the next one.
    Unavailable(SoldrError),
    /// The origin answered and the answer is wrong (bad content, integrity
    /// failure, a 4xx other than 404): stop here.
    Fatal(SoldrError),
}

impl OriginError {
    /// Classify an HTTP status that is not a success.
    pub fn from_status(status: u16, error: SoldrError) -> Self {
        if status_fails_over(status) {
            Self::Unavailable(error)
        } else {
            Self::Fatal(error)
        }
    }

    pub fn into_inner(self) -> SoldrError {
        match self {
            Self::Unavailable(error) | Self::Fatal(error) => error,
        }
    }
}

/// HTTP statuses that mean "this origin cannot serve it, another might".
pub fn status_fails_over(status: u16) -> bool {
    status == 404 || (500..600).contains(&status)
}

/// Try `operation` against each origin in order. The operation receives the
/// origin and whether it is the last one (so it can choose what an exhausted
/// list means). Moves on only on [`OriginError::Unavailable`]; a
/// [`OriginError::Fatal`] or the last origin's error is returned as-is.
pub async fn fetch_from_origins<T, F, Fut>(
    origins: &[String],
    what: &str,
    mut operation: F,
) -> Result<T, SoldrError>
where
    F: FnMut(String, bool) -> Fut,
    Fut: Future<Output = Result<T, OriginError>>,
{
    let Some(last_index) = origins.len().checked_sub(1) else {
        return Err(SoldrError::Other(format!(
            "no origin configured for {what}"
        )));
    };
    let mut index = 0;
    loop {
        let origin = &origins[index];
        let is_last = index == last_index;
        match operation(origin.clone(), is_last).await {
            Ok(value) => return Ok(value),
            Err(OriginError::Unavailable(error)) if !is_last => {
                eprintln!(
                    "soldr: {what} unavailable at {}: {error}; trying {}",
                    super::stream_download::safe_asset_url(origin),
                    super::stream_download::safe_asset_url(&origins[index + 1])
                );
                index += 1;
            }
            Err(error) => return Err(error.into_inner()),
        }
    }
}

#[cfg(test)]
#[path = "toolchain_origin_tests.rs"]
mod tests;
