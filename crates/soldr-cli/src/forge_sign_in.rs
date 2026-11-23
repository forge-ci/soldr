//! Browser pairing and per-user credentials for Forge CI.

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

use crate::core::{home_dir, SoldrError};

const PAIR_URL: &str = "https://forge-ci.com/soldr/pair";
const API_URL: &str = "https://forge-ci.com/api/v1/soldr/pairings";
const SESSION_URL: &str = "https://forge-ci.com/api/v1/soldr/session";
const WORKER_URL: &str = "https://forge-ci.com/api/v1/soldr/worker";
const CREDENTIAL_FILE: &str = "credential.json";

#[derive(clap::Subcommand)]
pub(crate) enum SignCommand {
    /// Get a pairing code and approve it with GitHub.
    In,
    /// Verify the saved Forge CI account can still connect.
    Status,
}

#[derive(Deserialize)]
struct Pairing {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize, Serialize)]
struct Credential {
    token: String,
    github_login: String,
}

pub(crate) async fn run(command: SignCommand) -> Result<(), SoldrError> {
    let directory = home_dir()?.join(".soldr").join("forge-ci");
    match command {
        SignCommand::In => sign_in(&directory).await?,
        SignCommand::Status => match load(&directory)? {
            Some(credential) => check_session(&credential).await?,
            None => println!("Run soldr sign in to pair this machine with Forge CI."),
        },
    }
    Ok(())
}

#[derive(Deserialize)]
pub(crate) struct WorkerGrant {
    pub origin: String,
    pub token: String,
}

pub(crate) async fn discover_worker() -> io::Result<Option<WorkerGrant>> {
    let directory = home_dir()
        .map_err(io::Error::other)?
        .join(".soldr")
        .join("forge-ci");
    let Some(credential) = load(&directory)? else {
        return Ok(None);
    };
    let response = client()?
        .post(WORKER_URL)
        .bearer_auth(&credential.token)
        .send()
        .await
        .map_err(network_error)?;
    match response.status() {
        StatusCode::NO_CONTENT => Ok(None),
        StatusCode::OK => response.json().await.map(Some).map_err(network_error),
        StatusCode::UNAUTHORIZED => Err(io::Error::other(
            "Forge CI rejected the saved credential. Run soldr sign in to reconnect.",
        )),
        status => Err(io::Error::other(format!(
            "Forge CI worker discovery failed ({status})."
        ))),
    }
}

fn client() -> io::Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(network_error)
}

async fn sign_in(directory: &Path) -> io::Result<()> {
    let client = client()?;
    let pairing: Pairing = client
        .post(API_URL)
        .send()
        .await
        .map_err(network_error)?
        .error_for_status()
        .map_err(network_error)?
        .json()
        .await
        .map_err(network_error)?;
    let code = pairing.user_code.as_bytes();
    if pairing.verification_uri != PAIR_URL
        || !(1..=600).contains(&pairing.expires_in)
        || !(1..pairing.expires_in).contains(&pairing.interval)
        || code.len() != 11
        || code[5] != b'-'
        || !code[..5]
            .iter()
            .chain(&code[6..])
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        return Err(io::Error::other("Forge CI returned an invalid pairing."));
    }
    println!("Open {PAIR_URL} and enter {}", pairing.user_code);
    io::stdout().flush()?;
    let credential = tokio::time::timeout(
        Duration::from_secs(pairing.expires_in),
        poll(&client, &pairing),
    )
    .await
    .map_err(|_| expired())??;
    save(directory, &credential)?;
    println!("Signed in to Forge CI as {}.", credential.github_login);
    Ok(())
}

async fn check_session(credential: &Credential) -> io::Result<()> {
    let response = client()?
        .get(SESSION_URL)
        .bearer_auth(&credential.token)
        .send()
        .await
        .map_err(network_error)?;
    match response.status() {
        StatusCode::NO_CONTENT => {
            println!("Connected to Forge CI as {}.", credential.github_login);
            Ok(())
        }
        StatusCode::UNAUTHORIZED => Err(io::Error::other(
            "Forge CI rejected the saved credential. Run soldr sign in to reconnect.",
        )),
        status => Err(io::Error::other(format!(
            "Forge CI session check failed ({status})."
        ))),
    }
}

async fn poll(client: &Client, pairing: &Pairing) -> io::Result<Credential> {
    let interval = Duration::from_secs(pairing.interval);
    let url = format!("{API_URL}/poll");
    loop {
        tokio::time::sleep(interval).await;
        let response = client
            .post(&url)
            .bearer_auth(&pairing.device_code)
            .send()
            .await
            .map_err(network_error)?;
        match response.status() {
            StatusCode::ACCEPTED => {}
            StatusCode::OK => return response.json().await.map_err(network_error),
            StatusCode::NOT_FOUND | StatusCode::GONE => return Err(expired()),
            status => {
                return Err(io::Error::other(format!(
                    "Forge CI pairing failed ({status})."
                )));
            }
        }
    }
}

fn expired() -> io::Error {
    io::Error::other("Forge CI pairing expired. Run soldr sign in to get a new code.")
}

fn network_error(error: reqwest::Error) -> io::Error {
    io::Error::other(format!(
        "Forge CI pairing request failed: {}",
        error.without_url()
    ))
}

fn save(directory: &Path, credential: &Credential) -> io::Result<()> {
    std::fs::create_dir_all(directory)?;
    running_process::broker::secure_dir::ensure_private_dir(directory)?;
    let mut staged = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut staged, credential)?;
    staged.as_file().sync_all()?;
    staged.persist(directory.join(CREDENTIAL_FILE))?;
    Ok(())
}

fn load(directory: &Path) -> io::Result<Option<Credential>> {
    match std::fs::File::open(directory.join(CREDENTIAL_FILE)) {
        Ok(file) => Ok(Some(serde_json::from_reader(file)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn sign_in_is_a_builtin() {
        let cli = crate::cli_args::Cli::try_parse_from(["soldr", "sign", "in"]).unwrap();
        assert!(matches!(
            cli.command,
            crate::cli_args::Commands::Sign {
                command: SignCommand::In
            }
        ));
    }

    #[test]
    fn credential_can_be_loaded_and_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("forge-ci");
        assert!(load(&directory).unwrap().is_none());
        for token in ["initial", "replacement"] {
            let credential = Credential {
                token: token.into(),
                github_login: "customer".into(),
            };
            save(&directory, &credential).unwrap();
            let loaded = load(&directory).unwrap().unwrap();
            assert_eq!(loaded.token, token);
            assert_eq!(loaded.github_login, "customer");
        }
    }
}
