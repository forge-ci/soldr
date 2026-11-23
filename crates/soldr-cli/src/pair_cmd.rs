//! Register this Soldr installation with Forge CI.

use std::io::{self, Write};
use std::process::Command;
use std::time::Duration;

use soldr_fetch::fetch::ForgePairingClient;
use tokio::time::{timeout_at, Instant};

use crate::core::{SoldrError, SoldrPaths};

const PAIR_PAGE: &str = "https://forge-ci.com/soldr/pair";

pub(crate) async fn run() -> Result<(), SoldrError> {
    let root = SoldrPaths::new()?.root;
    std::fs::create_dir_all(&root)?;
    let mut credential = tempfile::NamedTempFile::new_in(&root)?;
    let client = ForgePairingClient::new()?;
    let started = Instant::now();
    let challenge = client.start().await?;
    if challenge.expires_in == 0
        || challenge.expires_in > 1800
        || !(1..=challenge.expires_in).contains(&challenge.interval)
        || challenge.user_code.len() != 11
        || !challenge
            .user_code
            .bytes()
            .enumerate()
            .all(|(index, byte)| {
                if index == 5 {
                    byte == b'-'
                } else {
                    byte.is_ascii_alphanumeric()
                }
            })
    {
        return Err(io::Error::other("Forge CI returned an invalid pairing challenge").into());
    }

    let url = format!("{PAIR_PAGE}#code={}", challenge.user_code);
    println!("Pairing URL: {url}");
    println!("If your browser does not open, open the URL above.");
    println!("Continue with GitHub and keep this terminal open.");
    io::stdout().flush()?;
    open_browser(&url);
    let deadline = started + Duration::from_secs(challenge.expires_in);
    let registration = timeout_at(deadline, client.wait_for_approval(&challenge))
        .await
        .map_err(|_| expired())??;
    writeln!(credential, "{}", registration)?;
    credential.as_file().sync_all()?;
    credential
        .persist(root.join("forge-ci-registration"))
        .map_err(|error| error.error)?;
    println!("Registered");
    Ok(())
}

fn open_browser(url: &str) {
    let mut opener = match std::env::consts::OS {
        "macos" => Command::new("open"),
        "windows" => {
            let mut command = Command::new("rundll32.exe");
            command.arg("url.dll,FileProtocolHandler");
            command
        }
        "linux" => Command::new("xdg-open"),
        _ => return,
    };
    // The opener needs the caller's desktop session, including Wayland and D-Bus.
    if let Err(error) = running_process::spawn_daemon_with_env_policy(
        opener.arg(url),
        running_process::EnvironmentPolicy::Inherit,
    ) {
        eprintln!("Could not open a browser: {error}");
    }
}

fn expired() -> io::Error {
    io::Error::other("Pairing expired. Run soldr pair to get a new code.")
}
