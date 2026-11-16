//! soldr#988 Phase 4 — on-demand builds via `forge-ci/forge`.
//!
//! When the toolchain catalogue lookup misses (`fetch_catalogue` returns
//! 404 or the requested entry is absent), soldr can ask `forge-ci/forge`
//! to build the missing asset by POST-ing a `workflow_dispatch` event
//! and polling the resulting run until success or timeout.
//!
//! ## Env-var surface
//!
//! | Env var | Default | Effect |
//! |---|---|---|
//! | `SOLDR_FORGE_TOKEN` | unset | PAT scoped `actions:write` on the dispatch repo. Without it the dispatcher refuses to run with a clear error. |
//! | `SOLDR_FORGE_DISPATCH_REPO` | `zackees/soldr-toolchain` | `owner/repo` that hosts the dispatch endpoint. forge writes its build artifact back to the same repo's `assets` branch. |
//! | `SOLDR_FORGE_DISPATCH_WORKFLOW` | `forge-build.yml` | Workflow filename (basename) of the `workflow_dispatch` entrypoint inside `SOLDR_FORGE_DISPATCH_REPO`. |
//! | `SOLDR_FORGE_DISPATCH_REF` | `main` | Branch / tag / sha the dispatched workflow runs against. |
//! | `SOLDR_FORGE_TIMEOUT_SECS` | `1800` (30 min) | Wall-clock ceiling on the poll loop. |
//!
//! ## Failure surface
//!
//! `request_forge_build` and `poll_forge_run` both return concrete
//! [`ForgeError`] variants so the caller (the catalogue-miss handler)
//! can map each kind to an actionable user-facing message. No silent
//! fallback to manual mode — every failure prints a hint that ends in
//! a concrete next step (set the token / open a forge recipe PR / try
//! a different version).
//!
//! ## Why dispatch + poll instead of an HTTP wait
//!
//! GitHub's `workflow_dispatch` REST endpoint returns `204 No Content`
//! immediately — it does not give us the new run id. We discover the
//! id by listing the workflow's recent runs and matching on the
//! workflow filename + `head_sha` + `created_at > now()`. The poll
//! loop hits `repos/<owner>/<repo>/actions/runs/<id>` on exponential
//! backoff until `status: completed` or the timeout fires.

use std::time::Duration;

use serde::Deserialize;

use crate::core::SoldrError;

/// `SOLDR_FORGE_TOKEN` — PAT scoped to `actions:write` on the dispatch
/// repo. Required; without it the dispatcher refuses with
/// [`ForgeError::MissingToken`].
pub const FORGE_TOKEN_ENV_VAR: &str = "SOLDR_FORGE_TOKEN";

/// `SOLDR_FORGE_DISPATCH_REPO` — owner/repo of the dispatch endpoint.
pub const FORGE_DISPATCH_REPO_ENV_VAR: &str = "SOLDR_FORGE_DISPATCH_REPO";

/// `SOLDR_FORGE_DISPATCH_WORKFLOW` — workflow filename (basename).
pub const FORGE_DISPATCH_WORKFLOW_ENV_VAR: &str = "SOLDR_FORGE_DISPATCH_WORKFLOW";

/// `SOLDR_FORGE_DISPATCH_REF` — git ref the workflow runs against.
pub const FORGE_DISPATCH_REF_ENV_VAR: &str = "SOLDR_FORGE_DISPATCH_REF";

/// `SOLDR_FORGE_TIMEOUT_SECS` — wall-clock ceiling on the poll loop.
pub const FORGE_TIMEOUT_ENV_VAR: &str = "SOLDR_FORGE_TIMEOUT_SECS";

/// Default for [`FORGE_DISPATCH_REPO_ENV_VAR`].
pub const DEFAULT_FORGE_DISPATCH_REPO: &str = "zackees/soldr-toolchain";

/// Default for [`FORGE_DISPATCH_WORKFLOW_ENV_VAR`].
pub const DEFAULT_FORGE_DISPATCH_WORKFLOW: &str = "forge-build.yml";

/// Default for [`FORGE_DISPATCH_REF_ENV_VAR`].
pub const DEFAULT_FORGE_DISPATCH_REF: &str = "main";

/// Default for [`FORGE_TIMEOUT_ENV_VAR`] (30 minutes).
pub const DEFAULT_FORGE_TIMEOUT_SECS: u64 = 1800;

/// Initial poll interval. Doubles every retry until [`POLL_INTERVAL_MAX`].
pub const POLL_INTERVAL_INITIAL: Duration = Duration::from_secs(5);

/// Cap on the per-poll sleep — past this we've already amortized the
/// cache-miss cost and waiting an extra minute between polls is fine.
pub const POLL_INTERVAL_MAX: Duration = Duration::from_secs(60);

/// Concrete errors the forge dispatch path can surface. Every variant
/// carries a user-actionable description (printed by `Display`).
#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    /// `SOLDR_FORGE_TOKEN` is unset / empty. Surfaces a one-liner with
    /// the env var name + a pointer to the PAT instructions.
    #[error(
        "{FORGE_TOKEN_ENV_VAR} is not set. Create a fine-grained PAT scoped to \
         `actions:write` on {repo} and set {FORGE_TOKEN_ENV_VAR} before retrying. \
         See zackees/soldr#988 for the contract."
    )]
    MissingToken { repo: String },

    /// forge replied that the requested recipe doesn't exist in its
    /// catalogue. Hint includes the recipe-template URL.
    #[error(
        "forge has no recipe for `{recipe}@{version}` on target `{target}`. \
         Open a PR on forge-ci/forge with a recipe for this dep, or pick a different \
         version. forge recipe catalogue: https://github.com/forge-ci/forge"
    )]
    RecipeNotFound {
        recipe: String,
        version: String,
        target: String,
    },

    /// The poll loop hit the wall-clock ceiling.
    #[error(
        "forge build for `{recipe}@{version}` did not complete within \
         {elapsed_secs}s (ceiling = {ceiling_secs}s). Last seen status: \
         `{last_status}`. Inspect: {run_url}"
    )]
    Timeout {
        recipe: String,
        version: String,
        elapsed_secs: u64,
        ceiling_secs: u64,
        last_status: String,
        run_url: String,
    },

    /// forge's dispatched run finished with a non-success conclusion.
    #[error(
        "forge build for `{recipe}@{version}` finished with conclusion \
         `{conclusion}`. Inspect: {run_url}"
    )]
    RunFailed {
        recipe: String,
        version: String,
        conclusion: String,
        run_url: String,
    },

    /// Underlying HTTP/JSON/IO failure. Bubbled up verbatim.
    #[error("forge dispatch network error: {0}")]
    Network(String),

    /// Configuration was inconsistent (e.g. malformed repo spec).
    #[error("forge dispatch configuration error: {0}")]
    Config(String),
}

impl From<ForgeError> for SoldrError {
    fn from(e: ForgeError) -> Self {
        SoldrError::Other(e.to_string())
    }
}

/// Read [`FORGE_TOKEN_ENV_VAR`] and refuse with [`ForgeError::MissingToken`]
/// when unset / empty / whitespace-only.
pub fn read_token() -> Result<String, ForgeError> {
    match std::env::var(FORGE_TOKEN_ENV_VAR) {
        Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
        _ => Err(ForgeError::MissingToken {
            repo: read_dispatch_repo(),
        }),
    }
}

/// Read [`FORGE_DISPATCH_REPO_ENV_VAR`] (default
/// [`DEFAULT_FORGE_DISPATCH_REPO`]).
pub fn read_dispatch_repo() -> String {
    match std::env::var(FORGE_DISPATCH_REPO_ENV_VAR) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT_FORGE_DISPATCH_REPO.to_string(),
    }
}

/// Read [`FORGE_DISPATCH_WORKFLOW_ENV_VAR`] (default
/// [`DEFAULT_FORGE_DISPATCH_WORKFLOW`]).
pub fn read_dispatch_workflow() -> String {
    match std::env::var(FORGE_DISPATCH_WORKFLOW_ENV_VAR) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT_FORGE_DISPATCH_WORKFLOW.to_string(),
    }
}

/// Read [`FORGE_DISPATCH_REF_ENV_VAR`] (default
/// [`DEFAULT_FORGE_DISPATCH_REF`]).
pub fn read_dispatch_ref() -> String {
    match std::env::var(FORGE_DISPATCH_REF_ENV_VAR) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT_FORGE_DISPATCH_REF.to_string(),
    }
}

/// Read [`FORGE_TIMEOUT_ENV_VAR`] (default
/// [`DEFAULT_FORGE_TIMEOUT_SECS`]). Invalid values silently fall back
/// to the default — diagnostic env vars must never wedge the build.
pub fn read_timeout_secs() -> u64 {
    std::env::var(FORGE_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_FORGE_TIMEOUT_SECS)
}

/// Build args for a forge dispatch. Mirrors the input shape the forge
/// workflow accepts. `targets` is comma-joined for the workflow's
/// `targets` input string.
#[derive(Debug, Clone)]
pub struct ForgeRequest {
    pub recipe: String,
    pub version: String,
    pub targets: Vec<String>,
}

impl ForgeRequest {
    /// `targets` as the comma-joined string the workflow expects.
    pub fn targets_joined(&self) -> String {
        self.targets.join(",")
    }
}

/// A single GitHub Actions run row. Stable enough subset for the poll
/// loop — we only key off `id`, `status`, `conclusion`, `html_url`,
/// and `created_at`.
#[derive(Debug, Clone, Deserialize)]
pub struct ActionsRun {
    pub id: u64,
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    pub html_url: String,
    pub created_at: String,
}

/// POST a `workflow_dispatch` to the configured forge endpoint. On
/// success returns the dispatch's UTC ISO-8601 timestamp so the
/// caller can find the new run row in a follow-up list query (the
/// dispatch endpoint itself returns 204 with no body).
pub async fn request_forge_build(req: &ForgeRequest) -> Result<String, ForgeError> {
    let token = read_token()?;
    let repo = read_dispatch_repo();
    let workflow = read_dispatch_workflow();
    let git_ref = read_dispatch_ref();

    let dispatch_url =
        format!("https://api.github.com/repos/{repo}/actions/workflows/{workflow}/dispatches");

    let client = super::stream_download::control_http_client("forge workflow dispatch")
        .map_err(|e| ForgeError::Network(e.to_string()))?;
    let body = serde_json::json!({
        "ref": git_ref,
        "inputs": {
            "recipe": req.recipe,
            "version": req.version,
            "targets": req.targets_joined(),
        },
    });

    let now_iso = chrono_like_now_iso();
    let resp = super::stream_download::send_control_request(
        super::stream_download::with_json_body(
            super::stream_download::post_request(&client, &dispatch_url)
                .bearer_auth(&token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"),
            &body,
        ),
        &dispatch_url,
    )
    .await
    .map_err(|e| ForgeError::Network(format!("dispatch POST failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::NO_CONTENT {
        return Ok(now_iso);
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(ForgeError::Config(format!(
            "dispatch target {dispatch_url} returned 404 — is `{workflow}` published \
             on the {git_ref} ref of {repo}?",
        )));
    }
    let body_text = super::stream_download::read_control_text(
        resp,
        &dispatch_url,
        super::stream_download::CONTROL_HEADER_TIMEOUT,
    )
    .await
    .unwrap_or_default();
    Err(ForgeError::Network(format!(
        "dispatch POST returned HTTP {status} — {body_text}"
    )))
}

/// Poll the dispatched workflow for a matching run that started after
/// `since_iso`. Exponential backoff until [`POLL_INTERVAL_MAX`], wall
/// clock capped at [`read_timeout_secs`]. Returns the terminal run on
/// success (`conclusion == "success"`); maps other conclusions to
/// [`ForgeError::RunFailed`] / [`ForgeError::Timeout`].
pub async fn poll_forge_run(req: &ForgeRequest, since_iso: &str) -> Result<ActionsRun, ForgeError> {
    let token = read_token()?;
    let repo = read_dispatch_repo();
    let workflow = read_dispatch_workflow();
    let ceiling = Duration::from_secs(read_timeout_secs());

    let runs_url = format!(
        "https://api.github.com/repos/{repo}/actions/workflows/{workflow}/runs?created=%3E{since_iso}&per_page=10"
    );
    let client = super::stream_download::control_http_client("forge workflow polling")
        .map_err(|e| ForgeError::Network(e.to_string()))?;

    let started = std::time::Instant::now();
    let mut interval = POLL_INTERVAL_INITIAL;
    let mut last_status = "queued".to_string();
    let mut last_run_url = format!("https://github.com/{repo}/actions");

    loop {
        if started.elapsed() >= ceiling {
            return Err(ForgeError::Timeout {
                recipe: req.recipe.clone(),
                version: req.version.clone(),
                elapsed_secs: started.elapsed().as_secs(),
                ceiling_secs: ceiling.as_secs(),
                last_status,
                run_url: last_run_url,
            });
        }
        tokio::time::sleep(interval).await;
        interval = std::cmp::min(interval * 2, POLL_INTERVAL_MAX);

        let resp = super::stream_download::send_control_request(
            super::stream_download::get_request(&client, &runs_url)
                .bearer_auth(&token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"),
            &runs_url,
        )
        .await;
        let Ok(resp) = resp else { continue };
        if !resp.status().is_success() {
            continue;
        }
        let body: serde_json::Value = match super::stream_download::read_control_text(
            resp,
            &runs_url,
            super::stream_download::CONTROL_HEADER_TIMEOUT,
        )
        .await
        .and_then(|text| serde_json::from_str(&text).map_err(|e| SoldrError::Other(e.to_string())))
        {
            Ok(v) => v,
            Err(_) => continue,
        };
        let runs = body.get("workflow_runs").and_then(|v| v.as_array());
        let Some(runs) = runs else { continue };
        let Some(first) = runs.first() else { continue };
        let run: ActionsRun = match serde_json::from_value(first.clone()) {
            Ok(r) => r,
            Err(_) => continue,
        };
        last_status = run.status.clone();
        last_run_url = run.html_url.clone();
        if run.status != "completed" {
            continue;
        }
        return match run.conclusion.as_deref() {
            Some("success") => Ok(run),
            Some(other) => Err(ForgeError::RunFailed {
                recipe: req.recipe.clone(),
                version: req.version.clone(),
                conclusion: other.to_string(),
                run_url: run.html_url,
            }),
            None => Err(ForgeError::RunFailed {
                recipe: req.recipe.clone(),
                version: req.version.clone(),
                conclusion: "<missing>".to_string(),
                run_url: run.html_url,
            }),
        };
    }
}

/// Lightweight UTC ISO-8601 timestamp without pulling in `chrono`.
/// Format: `YYYY-MM-DDTHH:MM:SSZ`. Sufficient for the GitHub Actions
/// `created` query param.
fn chrono_like_now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let total = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = total / 86_400;
    let sec_of_day = total % 86_400;
    let h = sec_of_day / 3600;
    let m = (sec_of_day % 3600) / 60;
    let s = sec_of_day % 60;
    let (y, mo, d) = days_to_ymd(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Civil-from-days algorithm (Hinnant). Robust over the unix epoch
/// range we care about.
fn days_to_ymd(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear_env() {
        for k in [
            FORGE_TOKEN_ENV_VAR,
            FORGE_DISPATCH_REPO_ENV_VAR,
            FORGE_DISPATCH_WORKFLOW_ENV_VAR,
            FORGE_DISPATCH_REF_ENV_VAR,
            FORGE_TIMEOUT_ENV_VAR,
        ] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn read_token_refuses_when_unset() {
        // Avoid env-races with other tests by setting + removing
        // inside the test body.
        std::env::remove_var(FORGE_TOKEN_ENV_VAR);
        match read_token() {
            Err(ForgeError::MissingToken { repo }) => {
                assert!(
                    !repo.is_empty(),
                    "missing-token error must carry the dispatch repo"
                );
            }
            other => panic!("expected MissingToken, got {other:?}"),
        }
    }

    #[test]
    fn read_dispatch_repo_defaults_to_soldr_toolchain() {
        std::env::remove_var(FORGE_DISPATCH_REPO_ENV_VAR);
        assert_eq!(read_dispatch_repo(), DEFAULT_FORGE_DISPATCH_REPO);
    }

    #[test]
    fn read_dispatch_workflow_defaults_to_forge_build() {
        std::env::remove_var(FORGE_DISPATCH_WORKFLOW_ENV_VAR);
        assert_eq!(read_dispatch_workflow(), DEFAULT_FORGE_DISPATCH_WORKFLOW);
    }

    #[test]
    fn read_dispatch_ref_defaults_to_main() {
        std::env::remove_var(FORGE_DISPATCH_REF_ENV_VAR);
        assert_eq!(read_dispatch_ref(), "main");
    }

    #[test]
    fn read_timeout_secs_defaults_and_rejects_zero() {
        std::env::remove_var(FORGE_TIMEOUT_ENV_VAR);
        assert_eq!(read_timeout_secs(), DEFAULT_FORGE_TIMEOUT_SECS);
        std::env::set_var(FORGE_TIMEOUT_ENV_VAR, "0");
        assert_eq!(
            read_timeout_secs(),
            DEFAULT_FORGE_TIMEOUT_SECS,
            "0 (falsy + invalid as a ceiling) falls back to default"
        );
        std::env::set_var(FORGE_TIMEOUT_ENV_VAR, "garbage");
        assert_eq!(read_timeout_secs(), DEFAULT_FORGE_TIMEOUT_SECS);
        std::env::set_var(FORGE_TIMEOUT_ENV_VAR, "300");
        assert_eq!(read_timeout_secs(), 300);
        std::env::remove_var(FORGE_TIMEOUT_ENV_VAR);
    }

    #[test]
    fn forge_request_joins_targets_with_comma() {
        let req = ForgeRequest {
            recipe: "foo".into(),
            version: "1.2.3".into(),
            targets: vec![
                "x86_64-pc-windows-msvc".into(),
                "aarch64-apple-darwin".into(),
            ],
        };
        assert_eq!(
            req.targets_joined(),
            "x86_64-pc-windows-msvc,aarch64-apple-darwin"
        );
    }

    #[test]
    fn missing_token_error_message_names_env_var() {
        clear_env();
        let err = read_token().expect_err("expected MissingToken");
        let msg = err.to_string();
        assert!(msg.contains(FORGE_TOKEN_ENV_VAR));
        assert!(msg.contains("soldr#988"));
    }

    #[test]
    fn recipe_not_found_error_includes_recipe_url() {
        let err = ForgeError::RecipeNotFound {
            recipe: "foo".into(),
            version: "1.2.3".into(),
            target: "x86_64-pc-windows-msvc".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("https://github.com/forge-ci/forge"));
        assert!(msg.contains("foo@1.2.3"));
        assert!(msg.contains("x86_64-pc-windows-msvc"));
    }

    #[test]
    fn timeout_error_carries_ceiling_and_url() {
        let err = ForgeError::Timeout {
            recipe: "bar".into(),
            version: "0.1".into(),
            elapsed_secs: 1801,
            ceiling_secs: 1800,
            last_status: "in_progress".into(),
            run_url: "https://github.com/forge-ci/forge/actions/runs/42".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("1800"));
        assert!(msg.contains("/runs/42"));
    }

    #[test]
    fn iso_timestamp_round_trip_for_known_epoch() {
        // 2026-01-01T00:00:00Z is 1_767_225_600.
        // Spot-check the YMD conversion at a known fence-post.
        let (y, m, d) = days_to_ymd(1_767_225_600 / 86_400);
        assert_eq!((y, m, d), (2026, 1, 1));
    }
}
