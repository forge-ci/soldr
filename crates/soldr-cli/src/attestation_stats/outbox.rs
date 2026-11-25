use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use soldr_fetch::fetch::RemoteBuildHttpClient;

use super::{Completion, Timing, ACK_HEADER};

const MAX_PENDING: usize = 4096;
const DIRECTORY: &str = "attestation-stats-outbox";
const ORIGIN: &str = "SOLDR_ATTESTATION_STATS_ORIGIN";
const TOKEN: &str = "SOLDR_ATTESTATION_WRITE_TOKEN";

pub(crate) fn remove_service_credentials(command: &mut std::process::Command) {
    command
        .env_remove(ORIGIN)
        .env_remove(TOKEN)
        .env_remove("SOLDR_ATTESTATION_READ_TOKEN");
}

pub(crate) fn start(root: &Path) {
    if let Err(error) = start_sender(root) {
        eprintln!("soldr remote: attestation stats sender unavailable: {error}");
    }
}

fn start_sender(root: &Path) -> io::Result<()> {
    let Ok(origin) = std::env::var(ORIGIN) else {
        return Ok(());
    };
    let token = std::env::var(TOKEN).map_err(io::Error::other)?;
    if token.len() < 32 || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(io::Error::other("invalid attestation write token"));
    }
    let client = match origin.strip_prefix("http://") {
        Some(address) => RemoteBuildHttpClient::new(address.parse().map_err(io::Error::other)?)?,
        None => RemoteBuildHttpClient::https_service(&origin)?,
    };
    let directory = root.join(DIRECTORY);
    fs::create_dir_all(&directory)?;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if let Err(error) = deliver(&directory, &client, &token).await {
                tracing::debug!(%error, "attestation stats delivery deferred");
            }
        }
    });
    Ok(())
}

pub(crate) fn record(
    root: &Path,
    worker: Option<String>,
    session: &str,
    action: &str,
    args: &[String],
    exit_code: i32,
    timing: Timing,
) {
    if std::env::var_os(ORIGIN).is_none() {
        return;
    }
    let candidate = crate::cargo_front_door::cargo_target_triple(args, "");
    let event = Completion {
        version: 1,
        worker: worker.unwrap_or_else(|| "unknown".into()),
        session: session.into(),
        action: action.into(),
        target: crate::core::TargetTriple::from_triple(&candidate)
            .ok()
            .map(|target| target.triple()),
        exit_code,
        timing,
    };
    if let Err(error) = enqueue(&root.join(DIRECTORY), &event) {
        eprintln!("soldr remote: could not queue attestation stats: {error}");
    }
}

pub(super) fn enqueue(directory: &Path, event: &Completion) -> io::Result<()> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join("queue.lock"))?;
    let started = std::time::Instant::now();
    loop {
        match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => break,
            Err(error)
                if error.kind() == fs2::lock_contended_error().kind()
                    && started.elapsed() < Duration::from_millis(250) =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
    if pending(directory)?.take(MAX_PENDING).count() == MAX_PENDING {
        return Err(io::Error::other("attestation stats outbox is full"));
    }
    let mut staging = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut staging, event)?;
    staging.flush()?;
    staging.as_file().sync_all()?;
    staging.persist(directory.join(format!("{}.json", event.session)))?;
    Ok(())
}

fn pending(directory: &Path) -> io::Result<impl Iterator<Item = fs::DirEntry>> {
    Ok(fs::read_dir(directory)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json")))
}

pub(super) async fn deliver(
    directory: &Path,
    client: &RemoteBuildHttpClient,
    token: &str,
) -> io::Result<()> {
    for entry in pending(directory)?.take(64) {
        let body = fs::read(entry.path())?;
        let response = tokio::time::timeout(
            Duration::from_secs(3),
            client.post_stream(
                "/v1/attestations/completions",
                token,
                "application/json",
                body,
            ),
        )
        .await
        .map_err(io::Error::other)??;
        let status = response.status().as_u16();
        let acknowledged = response
            .headers()
            .get(ACK_HEADER)
            .is_some_and(|value| value == "1");
        if acknowledged && matches!(status, 201 | 204 | 422) {
            if status == 422 {
                tracing::warn!("attestation stats server discarded an invalid or expired event");
            }
            match fs::remove_file(entry.path()) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        } else {
            return Err(io::Error::other(format!("attestation stats HTTP {status}")));
        }
    }
    Ok(())
}
