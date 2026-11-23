//! Browser pairing and per-user credentials for Forge CI.

use std::io;
use std::path::Path;

use serde::Deserialize;
use soldr_fetch::fetch::ForgeCiHttp;

use crate::core::{home_dir, SoldrError, SoldrPaths};

const SESSION_URL: &str = "/api/v1/soldr/session";
const WORKER_URL: &str = "/api/v1/soldr/worker";
const CREDENTIAL_FILE: &str = "credential.json";

#[derive(clap::Subcommand)]
pub(crate) enum SignCommand {
    /// Get a pairing code and approve it with GitHub.
    In,
    /// Verify the saved Forge CI account can still connect.
    Status,
}

#[derive(Deserialize)]
struct Credential {
    token: String,
    github_login: Option<String>,
}

pub(crate) async fn run(command: SignCommand) -> Result<(), SoldrError> {
    if matches!(command, SignCommand::In) {
        return crate::pair_cmd::run().await;
    }
    let directory = home_dir()?.join(".soldr").join("forge-ci");
    match load_current(&directory, &SoldrPaths::new()?.root)? {
        Some(credential) => {
            check_session(&credential.token, credential.github_login.as_deref()).await?;
        }
        None => println!("Run soldr pair to connect this machine to Forge CI."),
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

fn rejected_credential() -> io::Error {
    io::Error::other("Forge CI rejected the saved credential. Run soldr pair to reconnect.")
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
    fn worker_uses_the_registration_from_soldr_pair() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("forge-ci");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(
            directory.join(CREDENTIAL_FILE),
            r#"{"token":"old credential","github_login":"customer"}"#,
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
