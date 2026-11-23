//! Browser pairing and per-user credentials for Forge CI.

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use soldr_fetch::fetch::ForgeCiHttp;

use crate::core::{home_dir, SoldrError, SoldrPaths};

const PAIR_URL: &str = "https://forge-ci.com/soldr/pair";
const API_URL: &str = "/api/v1/soldr/pairings";
const SESSION_URL: &str = "/api/v1/soldr/session";
const WORKER_URL: &str = "/api/v1/soldr/worker";
const CREDENTIAL_FILE: &str = "credential.json";
const MAX_PAIRING_SECONDS: u64 = 30 * 60;

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
    github_login: Option<String>,
}

pub(crate) async fn run(command: SignCommand) -> Result<(), SoldrError> {
    let directory = home_dir()?.join(".soldr").join("forge-ci");
    match command {
        SignCommand::In => sign_in(&directory).await?,
        SignCommand::Status => match load_current(&directory, &SoldrPaths::new()?.root)? {
            Some(credential) => {
                check_session(&credential.token, credential.github_login.as_deref()).await?;
            }
            None => println!("Run soldr pair to connect this machine to Forge CI."),
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
    let paths = SoldrPaths::new().map_err(io::Error::other)?;
    let Some(credential) = load_current(&directory, &paths.root)? else {
        return Ok(None);
    };
    let (status, body) = ForgeCiHttp::new()?
        .post(WORKER_URL, Some(&credential.token))
        .await?;
    match status {
        204 => Ok(None),
        200 => Ok(Some(serde_json::from_slice(&body)?)),
        401 => Err(rejected_credential()),
        _ => Err(io::Error::other(format!(
            "Forge CI worker discovery failed (HTTP {status})."
        ))),
    }
}

async fn sign_in(directory: &Path) -> io::Result<()> {
    let client = ForgeCiHttp::new()?;
    let (status, body) = client.post(API_URL, None).await?;
    if status != 201 {
        return Err(io::Error::other(format!(
            "Forge CI could not start pairing (HTTP {status})."
        )));
    }
    let pairing: Pairing = serde_json::from_slice(&body)?;
    let code = pairing.user_code.as_bytes();
    if pairing.verification_uri != PAIR_URL
        || !(1..=MAX_PAIRING_SECONDS).contains(&pairing.expires_in)
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
    if let Some(login) = &credential.github_login {
        println!("Signed in to Forge CI as {login}.");
    } else {
        println!("Signed in to Forge CI.");
    }
    Ok(())
}

async fn check_session(token: &str, github_login: Option<&str>) -> io::Result<()> {
    let (status, _) = ForgeCiHttp::new()?.get(SESSION_URL, token).await?;
    match status {
        204 => {
            if let Some(login) = github_login {
                println!("Connected to Forge CI as {login}.");
            } else {
                println!("Connected to Forge CI.");
            }
            Ok(())
        }
        401 => Err(rejected_credential()),
        _ => Err(io::Error::other(format!(
            "Forge CI session check failed (HTTP {status})."
        ))),
    }
}

async fn poll(client: &ForgeCiHttp, pairing: &Pairing) -> io::Result<Credential> {
    let interval = Duration::from_secs(pairing.interval);
    let url = format!("{API_URL}/poll");
    loop {
        tokio::time::sleep(interval).await;
        let (status, body) = client.post(&url, Some(&pairing.device_code)).await?;
        match status {
            202 => {}
            200 => return Ok(serde_json::from_slice(&body)?),
            404 | 410 => return Err(expired()),
            _ => {
                return Err(io::Error::other(format!(
                    "Forge CI pairing failed (HTTP {status})."
                )))
            }
        }
    }
}

fn rejected_credential() -> io::Error {
    io::Error::other("Forge CI rejected the saved credential. Run soldr sign in to reconnect.")
}

fn expired() -> io::Error {
    io::Error::other("Forge CI pairing expired. Run soldr sign in to get a new code.")
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

fn load_current(directory: &Path, root: &Path) -> io::Result<Option<Credential>> {
    match std::fs::read_to_string(root.join("forge-ci-registration")) {
        Ok(token) if !token.trim().is_empty() => Ok(Some(Credential {
            token: token.trim().to_owned(),
            github_login: None,
        })),
        Ok(_) => Err(io::Error::other("Saved Forge CI registration is empty.")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => load(directory),
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
                github_login: Some("customer".into()),
            };
            save(&directory, &credential).unwrap();
            let loaded = load(&directory).unwrap().unwrap();
            assert_eq!(loaded.token, token);
            assert_eq!(loaded.github_login.as_deref(), Some("customer"));
        }
    }

    #[test]
    fn worker_uses_the_registration_from_soldr_pair() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("forge-ci");
        save(
            &directory,
            &Credential {
                token: "old credential".into(),
                github_login: Some("customer".into()),
            },
        )
        .unwrap();
        assert_eq!(
            load_current(&directory, temp.path())
                .unwrap()
                .unwrap()
                .token,
            "old credential"
        );
        std::fs::write(
            temp.path().join("forge-ci-registration"),
            "new credential\n",
        )
        .unwrap();
        let paired = load_current(&directory, temp.path()).unwrap().unwrap();
        assert_eq!(paired.token, "new credential");
        assert!(paired.github_login.is_none());
    }
}
