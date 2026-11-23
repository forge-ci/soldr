//! Register this Soldr installation with Forge CI.

use std::io::{self, Write};
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

    println!("Open {PAIR_PAGE} and enter {}", challenge.user_code);
    println!("Then sign in with GitHub. Keep this terminal open.");
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

fn expired() -> io::Error {
    io::Error::other("Pairing expired. Run soldr pair to get a new code.")
}
