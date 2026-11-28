//! GitHub ref / release resolution for `soldr install` (soldr#2310).
//!
//! These are the network calls the install ladder makes *before* any
//! acquisition: resolve a ref to an immutable commit sha (the stable
//! cache + pin key) and resolve a release selector to a concrete tag.
//! They live in `soldr-fetch` because the bounded control-plane HTTP
//! helpers ([`super::stream_download`]) are `pub(crate)` to this crate.

use crate::core::SoldrError;

use super::stream_download::{
    control_http_client, get_request, read_control_text, send_control_request,
    CONTROL_HEADER_TIMEOUT,
};

fn control_request(
    client: &kernal_api::http::Client,
    url: &str,
    token: Option<&str>,
) -> super::stream_download::PendingRequest {
    let mut request = get_request(client, url).header("Accept", "application/vnd.github+json");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request
}

fn not_found_hint(url: &str, status: u16, token_present: bool) -> SoldrError {
    if (status == 404 || status == 403) && !token_present {
        SoldrError::Network(format!(
            "{url} returned HTTP {status}. If this is a private repo, set GITHUB_TOKEN \
             (or GH_TOKEN / SOLDR_GITHUB_TOKEN) and retry."
        ))
    } else {
        SoldrError::Network(format!("{url} failed: HTTP {status}"))
    }
}

/// Resolve `git_ref` (branch/tag/sha) to a full commit sha via
/// `GET /repos/{owner}/{repo}/commits/{ref}`.
pub async fn resolve_commit_sha(
    owner: &str,
    repo: &str,
    git_ref: &str,
    token: Option<&str>,
) -> Result<String, SoldrError> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{git_ref}");
    let client = control_http_client("install ref resolution")?;
    let resp = send_control_request(control_request(&client, &url, token), &url).await?;
    let status = resp.status();
    if !(200..300).contains(&status) {
        if status == 404 || status == 422 {
            return Err(SoldrError::Other(format!(
                "ref '{git_ref}' not found in {owner}/{repo}"
            )));
        }
        return Err(not_found_hint(&url, status, token.is_some()));
    }
    let text = read_control_text(resp, &url, CONTROL_HEADER_TIMEOUT).await?;
    let body: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| SoldrError::Other(e.to_string()))?;
    body.get("sha")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| SoldrError::Other(format!("commits API for {url} had no `sha` field")))
}

/// Resolve the latest release tag: `GET /repos/{o}/{r}/releases/latest`.
pub async fn resolve_latest_release_tag(
    owner: &str,
    repo: &str,
    token: Option<&str>,
) -> Result<String, SoldrError> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
    let client = control_http_client("install release resolution")?;
    let resp = send_control_request(control_request(&client, &url, token), &url).await?;
    let status = resp.status();
    if !(200..300).contains(&status) {
        if status == 404 {
            return Err(SoldrError::Other(format!(
                "no releases found for {owner}/{repo}"
            )));
        }
        return Err(not_found_hint(&url, status, token.is_some()));
    }
    let text = read_control_text(resp, &url, CONTROL_HEADER_TIMEOUT).await?;
    let body: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| SoldrError::Other(e.to_string()))?;
    tag_name(&body)
        .ok_or_else(|| SoldrError::Other(format!("releases API for {url} had no `tag_name`")))
}

/// Resolve a release `offset` back from latest (0 = latest, 1 = previous),
/// listing releases via `GET /repos/{o}/{r}/releases`.
pub async fn resolve_release_at_offset(
    owner: &str,
    repo: &str,
    offset: u32,
    token: Option<&str>,
) -> Result<String, SoldrError> {
    if offset == 0 {
        return resolve_latest_release_tag(owner, repo, token).await;
    }
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases?per_page=100");
    let client = control_http_client("install release resolution")?;
    let resp = send_control_request(control_request(&client, &url, token), &url).await?;
    let status = resp.status();
    if !(200..300).contains(&status) {
        return Err(not_found_hint(&url, status, token.is_some()));
    }
    let text = read_control_text(resp, &url, CONTROL_HEADER_TIMEOUT).await?;
    let body: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| SoldrError::Other(e.to_string()))?;
    let releases = body
        .as_array()
        .ok_or_else(|| SoldrError::Other(format!("releases API for {url} was not an array")))?;
    // The list endpoint returns newest-first. Skip prereleases/drafts.
    let published: Vec<&serde_json::Value> = releases
        .iter()
        .filter(|r| {
            !r.get("draft").and_then(|v| v.as_bool()).unwrap_or(false)
                && !r
                    .get("prerelease")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
        })
        .collect();
    let chosen = published.get(offset as usize).ok_or_else(|| {
        SoldrError::Other(format!(
            "release offset ~{offset} out of range: {owner}/{repo} has {} published release(s)",
            published.len()
        ))
    })?;
    tag_name(chosen)
        .ok_or_else(|| SoldrError::Other(format!("release at offset ~{offset} had no `tag_name`")))
}

fn tag_name(release: &serde_json::Value) -> Option<String> {
    release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// soldr#3697: prebuilt release-asset lane.
pub mod release;
