//! HTTP client for Forge CI's browser-assisted Soldr registration.

use std::io;
use std::time::Duration;

use serde::Deserialize;
use tokio::time::sleep;

use super::forge_http::ForgeCiHttp;

const PAIRINGS: &str = "/api/v1/soldr/pairings";
const POLL: &str = "/api/v1/soldr/pairings/poll";

#[derive(Deserialize)]
pub struct ForgePairingChallenge {
    device_code: String,
    pub user_code: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Deserialize)]
struct Registration {
    token: String,
}

pub struct ForgePairingClient(ForgeCiHttp);

impl ForgePairingClient {
    pub fn new() -> io::Result<Self> {
        ForgeCiHttp::new().map(Self)
    }

    pub async fn start(&self) -> io::Result<ForgePairingChallenge> {
        let (status, body) = self.0.post(PAIRINGS, None).await?;
        if status != 201 {
            return Err(status_error(status));
        }
        serde_json::from_slice(&body).map_err(io::Error::other)
    }

    pub async fn wait_for_approval(&self, challenge: &ForgePairingChallenge) -> io::Result<String> {
        loop {
            sleep(Duration::from_secs(challenge.interval)).await;
            let (status, body) = self.0.post(POLL, Some(&challenge.device_code)).await?;
            match status {
                202 => {}
                200 => {
                    let registration: Registration = serde_json::from_slice(&body)?;
                    return Ok(registration.token);
                }
                404 | 410 => {
                    return Err(io::Error::other(
                        "Pairing expired. Run soldr pair to get a new code.",
                    ));
                }
                status => return Err(status_error(status)),
            }
        }
    }
}

fn status_error(status: u16) -> io::Error {
    io::Error::other(format!("Forge CI pairing failed ({status})"))
}
