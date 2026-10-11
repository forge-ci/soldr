//! `[package.metadata.binstall]` hop (soldr#3726).
//!
//! After the GitHub Releases API hop misses (or is excluded by
//! `SOLDR_RESOLVER_ORDER`), an exact-version fetch downloads the crate's
//! published `.crate` from crates.io, reads its `Cargo.toml`, and — when it
//! declares a binstall `pkg-url` (per-target `overrides` honoured) — renders
//! that template strictly ([`super::binstall_template`]) and installs the
//! asset through the same pin/trust/install-lock/staged-promote path as the
//! QuickInstall hop. Runs before QuickInstall: publisher-described assets
//! win over the third-party index, as in cargo-binstall.
//!
//! The `.crate` base defaults to `https://static.crates.io/crates` and is
//! overridable through [`CRATES_BASE_URL_ENV_VAR`]. Both the `.crate` URL
//! and the rendered `pkg-url` must be `https`.
//!
//! Trust: binstall metadata carries no checksum soldr honours (minisign
//! signing is not implemented), so an asset is `trust: unverified` unless
//! `SOLDR_CHECKSUMS_FILE` pins it; a pin mismatch is always fatal and
//! `SOLDR_TRUST_MODE=strict` refuses an unpinned asset. The `.crate` itself
//! is only read for metadata; nothing from it is executed.

use std::io::Read as _;

use crate::core::{SoldrError, SoldrPaths, TargetTriple};

use super::binstall_template::{render, BinstallMeta, TemplateVars};
use super::stream_download::{
    asset_http_client, get_request, send_asset_request, stream_response_to_temp_file,
    ASSET_HEADER_TIMEOUT, ASSET_IDLE_TIMEOUT,
};
use super::{archive, trust, FetchResult, ResolverOrder, VersionSpec};

/// Overrides the crates.io `.crate` download base URL.
pub const CRATES_BASE_URL_ENV_VAR: &str = "SOLDR_BINSTALL_CRATES_BASE_URL";

/// Upstream crates.io static download base.
pub const DEFAULT_CRATES_BASE_URL: &str = "https://static.crates.io/crates";

/// Upper bound on the `Cargo.toml` read out of a `.crate`.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Base URL from the environment, or the upstream default.
pub fn crates_base_url_from_env() -> String {
    std::env::var(CRATES_BASE_URL_ENV_VAR)
        .ok()
        .map(|raw| raw.trim().trim_end_matches('/').to_string())
        .filter(|raw| !raw.is_empty())
        .unwrap_or_else(|| DEFAULT_CRATES_BASE_URL.to_string())
}

/// Refuse anything but `https`. `allow_loopback_http` is a test seam for
/// loopback fixtures; production passes `false`.
fn require_https(raw: &str, what: &str, allow_loopback_http: bool) -> Result<(), SoldrError> {
    let parsed = url::Url::parse(raw)
        .map_err(|e| SoldrError::Other(format!("binstall: invalid {what} `{raw}`: {e}")))?;
    let loopback = matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if allow_loopback_http && loopback => Ok(()),
        scheme => Err(SoldrError::Other(format!(
            "binstall: refusing {what} `{raw}`: scheme `{scheme}` is not https"
        ))),
    }
}

/// Download the `.crate` and return its `Cargo.toml`. `Ok(None)` on 404/410.
async fn fetch_cargo_toml(
    crates_base: &str,
    crate_name: &str,
    version: &str,
    allow_loopback_http: bool,
) -> Result<Option<String>, SoldrError> {
    let base = crates_base.trim_end_matches('/');
    let url = format!("{base}/{crate_name}/{crate_name}-{version}.crate");
    require_https(&url, "crate download URL", allow_loopback_http)?;
    let client = asset_http_client("binstall crate metadata")?;
    let resp = send_asset_request(get_request(&client, &url), &url, ASSET_HEADER_TIMEOUT).await?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(SoldrError::Network(format!(
            "binstall: HTTP {status} downloading {crate_name} v{version} .crate"
        )));
    }
    let downloaded = stream_response_to_temp_file(resp, &url, ASSET_IDLE_TIMEOUT).await?;
    let gz = flate2::read::GzDecoder::new(std::fs::File::open(downloaded.path())?);
    let mut tar = tar::Archive::new(gz);
    let wanted = format!("{crate_name}-{version}/Cargo.toml");
    for entry in tar
        .entries()
        .map_err(|e| SoldrError::Archive(e.to_string()))?
    {
        let entry = entry.map_err(|e| SoldrError::Archive(e.to_string()))?;
        let path = entry
            .path()
            .map_err(|e| SoldrError::Archive(e.to_string()))?;
        if path.to_str() == Some(wanted.as_str()) {
            let mut text = String::new();
            entry.take(MAX_MANIFEST_BYTES).read_to_string(&mut text)?;
            return Ok(Some(text));
        }
    }
    Err(SoldrError::Archive(format!(
        "binstall: {crate_name} v{version} .crate has no {wanted}"
    )))
}

/// Try to resolve `crate_name` `version` from its binstall metadata.
/// `Ok(None)` when the crate/version is absent or declares no `pkg-url`
/// for `target`, or the rendered asset is 404/410.
#[expect(
    clippy::too_many_arguments,
    reason = "quickinstall::try_resolve inputs plus the https test seam"
)]
pub async fn try_resolve(
    paths: &SoldrPaths,
    crate_name: &str,
    binary_names: &[&str],
    version: &str,
    target: &TargetTriple,
    crates_base: &str,
    policy: (&trust::PinnedChecksumStore, trust::TrustMode),
    allow_loopback_http: bool,
) -> Result<Option<FetchResult>, SoldrError> {
    paths.ensure_dirs()?;
    let Some(cargo_toml) =
        fetch_cargo_toml(crates_base, crate_name, version, allow_loopback_http).await?
    else {
        return Ok(None);
    };
    let Some(meta) = BinstallMeta::from_cargo_toml(&cargo_toml, target)? else {
        return Ok(None);
    };
    let vars = TemplateVars {
        name: crate_name,
        version,
        target,
        pkg_fmt: meta.pkg_fmt,
        repo: meta.repo.as_deref(),
        bin: None,
    };
    let url = render(&meta.pkg_url, &vars)?;
    if let Some(bin_dir) = &meta.bin_dir {
        // Validated strictly; extraction locates binaries by name.
        let bin_vars = TemplateVars {
            bin: binary_names.first().copied(),
            ..vars
        };
        render(bin_dir, &bin_vars)?;
    }
    require_https(&url, "pkg-url", allow_loopback_http)?;
    let hint = meta.pkg_fmt.extract_hint()?;
    let client = asset_http_client("binstall probe")?;
    let probe = send_asset_request(
        super::stream_download::head_request(&client, &url),
        &url,
        ASSET_HEADER_TIMEOUT,
    )
    .await?;
    let status = probe.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(SoldrError::Network(format!(
            "binstall: HTTP {status} probing {crate_name} v{version} pkg-url"
        )));
    }
    eprintln!(
        "soldr: {crate_name}: resolving v{version} from [package.metadata.binstall] pkg-url ({})",
        target.triple()
    );
    let binary_path = archive::download_and_extract_with_policy_as(
        paths,
        crate_name,
        version,
        &url,
        target,
        binary_names,
        policy,
        hint,
    )
    .await?;
    Ok(Some(FetchResult {
        binary_path,
        version: version.to_string(),
        cached: false,
    }))
}

/// Production hop after the API hop missed. `Ok(None)` when the hop is not
/// permitted, the version is not exact, it misses, or the host is
/// unreachable; a trust refusal, pin mismatch, template or scheme error is
/// returned as-is.
pub(super) async fn fallback_after_api(
    order: ResolverOrder,
    paths: &SoldrPaths,
    cache_name: &str,
    binary_names: &[&str],
    version: &VersionSpec,
    tag_prefix: Option<&str>,
    target: &TargetTriple,
) -> Result<Option<FetchResult>, SoldrError> {
    let VersionSpec::Exact(tag) = version else {
        return Ok(None);
    };
    if !order.try_binstall {
        return Ok(None);
    }
    let store = trust::PinnedChecksumStore::from_env()?;
    let mode = trust::TrustMode::from_env();
    let normalized = super::quickinstall::normalize_version(tag, tag_prefix);
    match try_resolve(
        paths,
        cache_name,
        binary_names,
        &normalized,
        target,
        &crates_base_url_from_env(),
        (&store, mode),
        false,
    )
    .await
    {
        Ok(Some(result)) => {
            super::smoke_test_or_evict(&result.binary_path, cache_name, target)?;
            Ok(Some(result))
        }
        Ok(None) => Ok(None),
        Err(SoldrError::Network(reason)) => {
            eprintln!("soldr: {cache_name}: binstall metadata unreachable ({reason}); continuing");
            Ok(None)
        }
        Err(other) => Err(other),
    }
}

#[cfg(test)]
#[path = "binstall_tests.rs"]
mod tests;
