//! GitHub device authorization issued and verified by the Forge signaling service.

use std::io;
use std::time::Duration;

use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use soldr_fetch::fetch::ForgeSignalClient;
use tokio::time::{sleep, timeout_at, Instant};

pub(super) const SIGNAL: &str = ForgeSignalClient::ORIGIN;

#[derive(Serialize)]
struct Pair {
    client: String,
    runner: String,
}

#[derive(Deserialize)]
struct Grant {
    expires_in: u64,
}

#[derive(Deserialize)]
struct Challenge {
    flow: String,
    user_code: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct Poll {
    pending: bool,
    denied: bool,
    retry_after: u64,
}

#[derive(Clone)]
pub(super) struct Signal(ForgeSignalClient);

impl Signal {
    pub(super) fn new() -> io::Result<Self> {
        ForgeSignalClient::new().map(Self)
    }

    pub(super) async fn check(
        &self,
        client: EndpointId,
        runner: EndpointId,
    ) -> io::Result<Duration> {
        let pair = Pair {
            client: client.to_string(),
            runner: runner.to_string(),
        };
        let grant: Grant = self.0.post("check", &pair).await?;
        Ok(Duration::from_secs(grant.expires_in))
    }

    pub(super) async fn sign_in(&self, client: EndpointId, runner: EndpointId) -> io::Result<()> {
        if !self.check(client, runner).await?.is_zero() {
            return Ok(());
        }
        let pair = Pair {
            client: client.to_string(),
            runner: runner.to_string(),
        };
        let challenge: Challenge = self.0.post("start", &pair).await?;
        if challenge.expires_in == 0
            || challenge.expires_in > 1800
            || challenge.interval == 0
            || !valid_user_code(&challenge.user_code)
        {
            return Err(io::Error::other(
                "Forge returned an invalid GitHub device challenge",
            ));
        }
        eprintln!(
            "Open https://github.com/login/device and enter code {}",
            challenge.user_code
        );
        let expires = Instant::now() + Duration::from_secs(challenge.expires_in);
        let mut interval = Duration::from_secs(challenge.interval.min(challenge.expires_in));
        let poll = serde_json::json!({ "flow": challenge.flow });
        timeout_at(expires, async {
            loop {
                sleep(interval).await;
                let result: Poll = self.0.post("poll", &poll).await?;
                if result.denied {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "GitHub authorization denied or the GitHub user cannot use this runner",
                    ));
                }
                if !result.pending {
                    return Ok(());
                }
                interval = Duration::from_secs(result.retry_after.clamp(1, challenge.expires_in));
            }
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "GitHub authorization expired"))?
    }
}

fn valid_user_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    bytes.len() == 9
        && bytes[4] == b'-'
        && bytes[..4]
            .iter()
            .chain(&bytes[5..])
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}
