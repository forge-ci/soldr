//! HTTP client for Forge CI's browser-assisted Soldr registration.

use std::io;
use std::time::Duration;

use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::{de::DeserializeOwned, Deserialize};
use tokio::time::sleep;

use super::stream_download::{
    asset_http_client_no_redirect, post_request, read_control_text,
    send_control_request_with_timeout,
};

const PAIRINGS: &str = "https://forge-ci.com/api/v1/soldr/pairings";
const POLL: &str = "https://forge-ci.com/api/v1/soldr/pairings/poll";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

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

pub struct ForgePairingClient(Client);

impl ForgePairingClient {
    pub fn new() -> io::Result<Self> {
        asset_http_client_no_redirect("Forge CI registration")
            .map(Self)
            .map_err(io::Error::other)
    }

    pub async fn start(&self) -> io::Result<ForgePairingChallenge> {
        let response = request(post_request(&self.0, PAIRINGS), PAIRINGS).await?;
        if response.status() != StatusCode::CREATED {
            return Err(status_error(response.status()));
        }
        decode(response, PAIRINGS).await
    }

    pub async fn wait_for_approval(&self, challenge: &ForgePairingChallenge) -> io::Result<String> {
        loop {
            sleep(Duration::from_secs(challenge.interval)).await;
            let response = request(
                post_request(&self.0, POLL).bearer_auth(&challenge.device_code),
                POLL,
            )
            .await?;
            match response.status() {
                StatusCode::ACCEPTED => {}
                StatusCode::OK => {
                    let registration: Registration = decode(response, POLL).await?;
                    return Ok(registration.token);
                }
                StatusCode::NOT_FOUND | StatusCode::GONE => {
                    return Err(io::Error::other(
                        "Pairing expired. Run soldr pair to get a new code.",
                    ));
                }
                status => return Err(status_error(status)),
            }
        }
    }
}

async fn request(builder: RequestBuilder, url: &str) -> io::Result<Response> {
    send_control_request_with_timeout(builder, url, REQUEST_TIMEOUT)
        .await
        .map_err(io::Error::other)
}

async fn decode<T: DeserializeOwned>(response: Response, url: &str) -> io::Result<T> {
    let text = read_control_text(response, url, REQUEST_TIMEOUT)
        .await
        .map_err(io::Error::other)?;
    serde_json::from_str(&text).map_err(io::Error::other)
}

fn status_error(status: StatusCode) -> io::Error {
    io::Error::other(format!("Forge CI pairing failed ({status})"))
}
