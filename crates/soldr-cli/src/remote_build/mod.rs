//! Whole-workspace builds on a trusted runner reached over an SSH tunnel.

mod artifacts;
mod manifest;
mod server;
mod slots;
mod snapshot;
mod wire;

use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use prost::Message;
use soldr_fetch::fetch::LoopbackPostClient;

use self::wire::{Action, Blob, ResultFrame, Session, Start};

#[derive(Args)]
pub(crate) struct RemoteArgs {
    /// File containing the same randomly generated token on client and runner.
    #[arg(long)]
    token_file: PathBuf,
    #[command(subcommand)]
    command: RemoteCommand,
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Serve remote build sessions on a loopback interface.
    Serve {
        #[arg(long, default_value = "127.0.0.1:7744")]
        listen: SocketAddr,
        #[arg(long)]
        root: PathBuf,
        /// Override the automatic slot count. Zero disables warm slots.
        #[arg(long, value_parser = clap::value_parser!(u32).range(0..=1024))]
        slots: Option<u32>,
        /// Maximum parallel artifact transfers advertised to each client.
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(1..=8))]
        artifact_streams: u8,
    },
    /// Run a command against the current Git commit on the runner.
    Run {
        #[arg(long, default_value = "127.0.0.1:7744")]
        endpoint: SocketAddr,
        #[arg(long, default_value = "target/attestation.json")]
        attestation: PathBuf,
        /// Optional JSON identifying a repository, user, and commit lineage.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Download matching artifacts to this new directory on the same connection.
        #[arg(long, requires = "fetch")]
        fetch_out: Option<PathBuf>,
        /// Session-relative artifact glob to download after the run. Repeat as needed.
        #[arg(long, requires = "fetch_out")]
        fetch: Vec<String>,
        #[arg(long, allow_negative_numbers = true, value_parser = crate::cache_lib::zstd_level::parse_zstd_level)]
        cache_zstd_level: Option<i32>,
        #[arg(value_enum)]
        action: BuildAction,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Fetch matching files from a finished session into a new directory.
    Fetch {
        #[arg(long, default_value = "127.0.0.1:7744")]
        endpoint: SocketAddr,
        #[arg(long)]
        out: PathBuf,
        session: String,
        #[arg(required = true)]
        globs: Vec<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum BuildAction {
    Build,
    Test,
    Dylint,
    Lint,
}

pub(crate) async fn run(args: RemoteArgs) -> io::Result<i32> {
    let token = std::fs::read_to_string(args.token_file)?;
    let token = token.trim();
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::other(
            "Remote build token file must contain 64 hexadecimal characters.",
        ));
    }
    match args.command {
        RemoteCommand::Serve {
            listen,
            root,
            slots,
            artifact_streams,
        } => server::serve(listen, root, token, slots, artifact_streams).await,
        RemoteCommand::Run {
            endpoint,
            attestation,
            manifest,
            fetch_out,
            fetch,
            cache_zstd_level,
            action,
            args,
        } => {
            let directory = std::env::current_dir()?;
            let snapshot = snapshot::capture(&directory)?;
            let manifest = manifest::read(manifest.as_deref(), &snapshot.commit)?;
            if let Some(manifest) = &manifest {
                snapshot::verify_parent(&directory, &snapshot.commit, manifest.parent())?;
            }
            let manifest_json = manifest.as_ref().map(serde_json::to_vec).transpose()?;
            let client = Client::new(endpoint, token, fetch_out.is_some())?;
            let action = match action {
                BuildAction::Build => Action::Build,
                BuildAction::Test => Action::Test,
                BuildAction::Dylint => Action::Dylint,
                BuildAction::Lint => Action::Lint,
            };
            let wants_manifest = manifest_json.is_some();
            let session: Session = client
                .post(
                    "start",
                    &Start {
                        commit: snapshot.commit,
                        files: snapshot.files,
                        action: action as i32,
                        args,
                        cache_zstd_level,
                        manifest_json,
                    },
                )
                .await?;
            if wants_manifest && !session.manifest_accepted {
                return Err(io::Error::other(
                    "remote runner does not support session manifests; upgrade the runner",
                ));
            }
            let mut blobs = snapshot.blobs;
            for hash in session.missing {
                let data = blobs
                    .remove(&hash)
                    .ok_or_else(|| io::Error::other("runner requested an unknown source digest"))?;
                let _: Blob = client.post(&format!("blob/{hash}"), &Blob { data }).await?;
            }
            eprintln!("Remote build session {}", session.id);
            let result: ResultFrame = client
                .post(&format!("run/{}", session.id), &Blob::default())
                .await?;
            if let Some(parent) = attestation
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(attestation, result.attestation)?;
            io::stdout().write_all(&result.stdout)?;
            io::stderr().write_all(&result.stderr)?;
            if let Some(out) = fetch_out {
                artifacts::download(&client, &session.id, &out, fetch).await?;
            }
            Ok(result.exit_code)
        }
        RemoteCommand::Fetch {
            endpoint,
            out,
            session,
            globs,
        } => {
            let client = Client::new(endpoint, token, true)?;
            artifacts::download(&client, &session, &out, globs).await?;
            Ok(0)
        }
    }
}

struct Client<'a> {
    http: LoopbackPostClient,
    token: &'a str,
}

impl<'a> Client<'a> {
    fn new(endpoint: SocketAddr, token: &'a str, http2: bool) -> io::Result<Self> {
        Ok(Self {
            http: LoopbackPostClient::with_http2(endpoint, http2)?,
            token,
        })
    }

    async fn post<T: Message + Default>(
        &self,
        operation: &str,
        message: &impl Message,
    ) -> io::Result<T> {
        let data = self
            .http
            .post(
                &format!("/v1/{operation}"),
                self.token,
                "application/x-protobuf+zstd",
                wire::encode(message)?,
                wire::MAX_FRAME + 1024 * 1024,
            )
            .await?;
        wire::decode(&data)
    }
}
