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
//! A crate that declares no `pkg-url` is probed with binstall's default
//! `{ repo }/releases/download/...` guesses (soldr#3735).
//!
//! Trust: binstall metadata carries no checksum soldr honours, so an asset
//! is `trust: unverified` unless `SOLDR_CHECKSUMS_FILE` pins it; a pin
//! mismatch is always fatal and `SOLDR_TRUST_MODE=strict` refuses an
//! unpinned asset. A declared minisign `signing` key is enforced fail-closed
//! ([`super::binstall_signing`]) but is not a pin. The `.crate` itself is
//! only read for metadata; nothing from it is executed.

use std::io::Read as _;

use crate::core::{SoldrError, SoldrPaths, TargetTriple};

use super::binstall_signing::Verifier;
use super::binstall_template::{render, BinstallMeta, TemplateVars};
use super::stream_download::{
    asset_http_client, get_request, send_asset_request, stream_response_to_temp_file,
    DownloadedAsset, ASSET_HEADER_TIMEOUT, ASSET_IDLE_TIMEOUT,
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

fn is_gone(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE
}

/// GET `url` (https-checked) to a temp file. `Ok(None)` on 404/410.
async fn download_optional(
    url: &str,
    what: &str,
    allow_loopback_http: bool,
) -> Result<Option<DownloadedAsset>, SoldrError> {
    require_https(url, what, allow_loopback_http)?;
    let client = asset_http_client(what)?;
    let resp = send_asset_request(get_request(&client, url), url, ASSET_HEADER_TIMEOUT).await?;
    let status = resp.status();
    if is_gone(status) {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(SoldrError::Network(format!(
            "binstall: HTTP {status} downloading {what} {url}"
        )));
    }
    Ok(Some(
        stream_response_to_temp_file(resp, url, ASSET_IDLE_TIMEOUT).await?,
    ))
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
    let Some(downloaded) =
        download_optional(&url, "crate download URL", allow_loopback_http).await?
    else {
        return Ok(None);
    };
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
    let meta = BinstallMeta::from_cargo_toml(&cargo_toml, target)?;
    let guessed = meta.pkg_url.is_none();
    let candidates = meta.candidates(crate_name, version, target)?;
    if let (Some(bin_dir), Some(&(_, fmt))) = (&meta.bin_dir, candidates.first()) {
        // Validated strictly; extraction locates binaries by name.
        render(
            bin_dir,
            &TemplateVars {
                name: crate_name,
                version,
                target,
                pkg_fmt: fmt,
                repo: meta.repo.as_deref(),
                bin: binary_names.first().copied(),
                url: None,
            },
        )?;
    }
    let Some((url, pkg_fmt)) = probe_candidates(
        candidates,
        guessed,
        crate_name,
        version,
        allow_loopback_http,
    )
    .await?
    else {
        return Ok(None);
    };
    let verifier = match &meta.signing {
        Some(signing) => Some(Verifier::new(
            signing, crate_name, version, target, pkg_fmt, &url,
        )?),
        None => None,
    };
    let signature = match &verifier {
        Some(v) => {
            let sig_file = download_optional(&v.sig_url, "minisign signature", allow_loopback_http)
                .await?
                .ok_or_else(|| {
                    SoldrError::Other(format!(
                        "binstall: minisign: {crate_name}: declares signing but has no signature at {}",
                        v.sig_url
                    ))
                })?;
            Some(v.decode_signature(sig_file.path())?)
        }
        None => None,
    };
    let check = |asset: &std::path::Path| match (&verifier, &signature) {
        (Some(v), Some(sig)) => v.verify_file(asset, sig),
        _ => Ok(()),
    };
    eprintln!(
        "soldr: {crate_name}: resolving v{version} from [package.metadata.binstall] {} ({})",
        if guessed {
            "default pkg-url guess"
        } else {
            "pkg-url"
        },
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
        archive::AssetAs {
            format_hint: pkg_fmt.extract_hint(),
            verify: verifier
                .is_some()
                .then_some(&check as archive::AssetVerifier<'_>),
        },
    )
    .await?;
    Ok(Some(FetchResult {
        binary_path,
        version: version.to_string(),
        cached: false,
    }))
}

/// HEAD-probe `candidates` in order; the first 2xx wins. A declared
/// `pkg-url` that answers anything but 2xx/404/410 is a network error; a
/// guess that does is just skipped.
async fn probe_candidates(
    candidates: Vec<(String, super::binstall_template::PkgFmt)>,
    guessed: bool,
    crate_name: &str,
    version: &str,
    allow_loopback_http: bool,
) -> Result<Option<(String, super::binstall_template::PkgFmt)>, SoldrError> {
    let client = asset_http_client("binstall probe")?;
    for (url, fmt) in candidates {
        require_https(&url, "pkg-url", allow_loopback_http)?;
        let probe = send_asset_request(
            super::stream_download::head_request(&client, &url),
            &url,
            ASSET_HEADER_TIMEOUT,
        )
        .await?;
        let status = probe.status();
        if status.is_success() {
            return Ok(Some((url, fmt)));
        }
        if !guessed && !is_gone(status) {
            return Err(SoldrError::Network(format!(
                "binstall: HTTP {status} probing {crate_name} v{version} pkg-url"
            )));
        }
    }
    Ok(None)
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
