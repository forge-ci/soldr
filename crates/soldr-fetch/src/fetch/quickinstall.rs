//! QuickInstall fallback hop (soldr#3700).
//!
//! After the GitHub Releases API hop misses (no release, no matching asset,
//! or the hop is excluded by `SOLDR_RESOLVER_ORDER`), an exact-version fetch
//! consults the cargo-quickinstall prebuilt index before giving up:
//!
//! `<base>/<crate>-<version>/<crate>-<version>-<target>.tar.gz`
//!
//! The base defaults to the `cargo-bins/cargo-quickinstall` release
//! download URL and is overridable through [`QUICKINSTALL_BASE_URL_ENV_VAR`]
//! (mirrors, and loopback fixtures in tests).
//!
//! Trust: QuickInstall publishes no checksum. A download is therefore
//! `trust: unverified` unless `SOLDR_CHECKSUMS_FILE` pins the asset; a pin
//! mismatch is always fatal and `SOLDR_TRUST_MODE=strict` refuses an
//! unpinned asset. The hop never bypasses a pin.
//!
//! A crate's own `[package.metadata.binstall]` `pkg-url` is consulted first,
//! by the [`super::binstall`] hop (soldr#3726).

use crate::core::{SoldrError, SoldrPaths, TargetTriple};

use super::stream_download::{asset_http_client, head_request, send_control_request};
use super::{archive, trust, FetchResult, ResolverOrder, VersionSpec};

/// Overrides the QuickInstall release-download base URL.
pub const QUICKINSTALL_BASE_URL_ENV_VAR: &str = "SOLDR_QUICKINSTALL_BASE_URL";

/// Upstream cargo-quickinstall release download base.
pub const DEFAULT_QUICKINSTALL_BASE_URL: &str =
    "https://github.com/cargo-bins/cargo-quickinstall/releases/download";

/// Base URL from the environment, or the upstream default.
pub fn base_url_from_env() -> String {
    std::env::var(QUICKINSTALL_BASE_URL_ENV_VAR)
        .ok()
        .map(|raw| raw.trim().trim_end_matches('/').to_string())
        .filter(|raw| !raw.is_empty())
        .unwrap_or_else(|| DEFAULT_QUICKINSTALL_BASE_URL.to_string())
}

/// Strip a monorepo tag prefix and a leading `v` from a requested tag.
pub fn normalize_version(tag: &str, tag_prefix: Option<&str>) -> String {
    let tag = tag_prefix
        .and_then(|prefix| tag.strip_prefix(prefix))
        .unwrap_or(tag);
    tag.strip_prefix('v').unwrap_or(tag).to_string()
}

/// Asset URL for `crate_name` `version` on `target`.
pub fn asset_url(base: &str, crate_name: &str, version: &str, target: &TargetTriple) -> String {
    let base = base.trim_end_matches('/');
    let triple = target.triple();
    format!("{base}/{crate_name}-{version}/{crate_name}-{version}-{triple}.tar.gz")
}

/// Try to resolve `crate_name` `version` from QuickInstall. `Ok(None)` when
/// QuickInstall has no asset for this crate/version/target (HTTP 404/410).
/// Trust is enforced against the explicit `policy`, never skipped.
pub async fn try_resolve(
    paths: &SoldrPaths,
    crate_name: &str,
    binary_names: &[&str],
    version: &str,
    target: &TargetTriple,
    base_url: &str,
    policy: (&trust::PinnedChecksumStore, trust::TrustMode),
) -> Result<Option<FetchResult>, SoldrError> {
    paths.ensure_dirs()?;
    let url = asset_url(base_url, crate_name, version, target);
    let client = asset_http_client("quickinstall probe")?;
    let probe = send_control_request(head_request(&client, &url), &url).await?;
    let status = probe.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(SoldrError::Network(format!(
            "quickinstall: HTTP {status} probing {crate_name} v{version} for {}",
            target.triple()
        )));
    }
    eprintln!(
        "soldr: {crate_name}: resolving v{version} from QuickInstall ({})",
        target.triple()
    );
    let binary_path = archive::download_and_extract_with_policy(
        paths,
        crate_name,
        version,
        &url,
        target,
        binary_names,
        policy,
    )
    .await?;
    Ok(Some(FetchResult {
        binary_path,
        version: version.to_string(),
        cached: false,
    }))
}

/// Production fallback after the API hop failed with `api_err`. Returns the
/// QuickInstall result when the hop is permitted and hits; otherwise the
/// original API error. An unreachable QuickInstall host is a miss; a trust
/// refusal, pin mismatch, or extract failure is returned as-is.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors fetch_repo_binary_once's resolver inputs plus the API error"
)]
pub(super) async fn fallback_after_api(
    order: ResolverOrder,
    api_err: SoldrError,
    paths: &SoldrPaths,
    cache_name: &str,
    binary_names: &[&str],
    version: &VersionSpec,
    tag_prefix: Option<&str>,
    target: &TargetTriple,
) -> Result<FetchResult, SoldrError> {
    let VersionSpec::Exact(tag) = version else {
        return Err(api_err);
    };
    if !order.try_quickinstall {
        return Err(api_err);
    }
    let store = trust::PinnedChecksumStore::from_env()?;
    let mode = trust::TrustMode::from_env();
    let normalized = normalize_version(tag, tag_prefix);
    match try_resolve(
        paths,
        cache_name,
        binary_names,
        &normalized,
        target,
        &base_url_from_env(),
        (&store, mode),
    )
    .await
    {
        Ok(Some(result)) => {
            super::smoke_test_or_evict(&result.binary_path, cache_name, target)?;
            Ok(result)
        }
        Ok(None) => Err(api_err),
        Err(SoldrError::Network(reason)) => {
            eprintln!("soldr: {cache_name}: QuickInstall unreachable ({reason}); keeping the release-API error");
            Err(api_err)
        }
        Err(other) => Err(other),
    }
}

#[cfg(test)]
#[path = "quickinstall_tests.rs"]
mod tests;
