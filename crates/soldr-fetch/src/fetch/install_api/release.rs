//! Prebuilt release-asset lane for `soldr install` (soldr#3697).
//!
//! Selection reuses the tool fetcher's asset matcher
//! ([`super::super::github::match_asset_for_binaries`]); download, sha256
//! verification, extraction and locked promotion reuse
//! [`super::super::archive::download_and_extract_with_pin`]. There is no second
//! downloader here.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths, TargetTriple};

use super::super::github::AssetInfo;
use super::super::stream_download::{
    control_http_client, get_request, read_control_text, send_control_request,
    CONTROL_HEADER_TIMEOUT,
};

/// One release asset chosen for the host triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub bytes: u64,
    /// GitHub's published `digest` (`sha256:<hex>`), lowercase hex, when
    /// the release API reports one. Used as a hard pin on download.
    pub sha256: Option<String>,
}

/// Pick the asset in a GitHub release JSON body (`GET
/// /repos/{o}/{r}/releases/tags/{tag}`) that matches `triple`. `Ok(None)`
/// means no asset fits this host, so the caller may build from source.
pub fn select_release_asset(
    release_json: &str,
    triple: &str,
    binary_names: &[&str],
) -> Result<Option<ReleaseAsset>, SoldrError> {
    let target = TargetTriple::from_triple(triple)?;
    let body: serde_json::Value = serde_json::from_str(release_json)
        .map_err(|e| SoldrError::Other(format!("install: release JSON: {e}")))?;
    let raw = body
        .get("assets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut assets = Vec::new();
    let mut extra = Vec::new();
    for item in &raw {
        let (Some(name), Some(url)) = (
            item.get("name").and_then(|v| v.as_str()),
            item.get("browser_download_url").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        assets.push(AssetInfo {
            name: name.to_string(),
            download_url: url.to_string(),
        });
        let bytes = item.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        let sha256 = item
            .get("digest")
            .and_then(|v| v.as_str())
            .and_then(|d| d.strip_prefix("sha256:"))
            .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
            .map(str::to_ascii_lowercase);
        extra.push((bytes, sha256));
    }
    let Ok(chosen) = super::super::github::match_asset_for_binaries(&assets, &target, binary_names)
    else {
        return Ok(None);
    };
    let idx = assets
        .iter()
        .position(|a| std::ptr::eq(a, chosen))
        .unwrap_or_default();
    let (bytes, sha256) = extra[idx].clone();
    Ok(Some(ReleaseAsset {
        name: chosen.name.clone(),
        url: chosen.download_url.clone(),
        bytes,
        sha256,
    }))
}

/// Look up the asset for `triple` on release `tag` of `owner/repo`.
pub async fn find_release_asset(
    owner: &str,
    repo: &str,
    tag: &str,
    triple: &str,
    binary_names: &[&str],
    token: Option<&str>,
) -> Result<Option<ReleaseAsset>, SoldrError> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/tags/{tag}");
    let client = control_http_client("install release asset lookup")?;
    let mut request = get_request(&client, &url).header("Accept", "application/vnd.github+json");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let resp = send_control_request(request, &url).await?;
    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(SoldrError::Network(format!("{url} failed: HTTP {status}")));
    }
    let text = read_control_text(resp, &url, CONTROL_HEADER_TIMEOUT).await?;
    select_release_asset(&text, triple, binary_names)
}

/// Download, verify and extract `asset` into the locked tool cache
/// (`<bin>/<cache_name>-<version>/`), returning the main binary path.
///
/// Integrity: when the asset carries a sha256 it is a hard pin (mismatch
/// is an error in every mode); otherwise `SOLDR_CHECKSUMS_FILE` /
/// `SOLDR_TRUST_MODE` apply exactly as for every other tool fetch.
pub async fn install_release_asset(
    paths: &SoldrPaths,
    cache_name: &str,
    version: &str,
    asset: &ReleaseAsset,
    triple: &str,
    binary_names: &[&str],
) -> Result<PathBuf, SoldrError> {
    paths.ensure_dirs()?;
    let target = TargetTriple::from_triple(triple)?;
    let pin = asset
        .sha256
        .as_deref()
        .map(|sha| (asset.name.as_str(), sha));
    // Explicit release installs bypass the repository-fetch retry loop.
    super::super::retry::with_backoff(cache_name, || {
        super::super::archive::download_and_extract_with_pin(
            paths,
            cache_name,
            version,
            &asset.url,
            &target,
            binary_names,
            pin,
        )
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE: &str = r#"{"tag_name":"v1.2.3","assets":[
      {"name":"foo-x86_64-pc-windows-msvc.zip","size":10,
       "browser_download_url":"https://example.invalid/w.zip"},
      {"name":"foo-x86_64-unknown-linux-gnu.tar.gz","size":42,
       "digest":"sha256:ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789",
       "browser_download_url":"https://example.invalid/l.tar.gz"},
      {"name":"foo-src.tar.gz","size":1,"browser_download_url":"https://example.invalid/s"}
    ]}"#;

    #[test]
    fn selects_host_asset_with_size_and_digest() {
        let asset = select_release_asset(RELEASE, "x86_64-unknown-linux-gnu", &["foo"])
            .unwrap()
            .expect("linux asset");
        assert_eq!(asset.name, "foo-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(asset.url, "https://example.invalid/l.tar.gz");
        assert_eq!(asset.bytes, 42);
        assert_eq!(
            asset.sha256.as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );
    }

    #[test]
    fn no_matching_asset_is_none() {
        assert_eq!(
            select_release_asset(RELEASE, "aarch64-apple-darwin", &["foo"]).unwrap(),
            None
        );
        assert_eq!(
            select_release_asset(r#"{"assets":[]}"#, "x86_64-unknown-linux-gnu", &["foo"]).unwrap(),
            None
        );
    }
}
