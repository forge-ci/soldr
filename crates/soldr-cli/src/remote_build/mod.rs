//! Whole-workspace builds on a trusted runner.

mod artifacts;
mod control;
mod manifest;
mod oauth;
mod peer;
mod retention;
mod server;
mod slots;
mod snapshot;
mod wire;

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use prost::Message;
use soldr_fetch::fetch::LoopbackPostClient;

use self::wire::{Action, Blob, Session, Start};

#[derive(Args)]
pub(crate) struct RemoteArgs {
    /// File containing the same randomly generated token on client and runner.
    #[arg(long)]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: RemoteCommand,
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Create or read a persistent peer identity and print its public ID.
    PeerId {
        #[arg(long)]
        key: PathBuf,
    },
    /// Maintain a peer connection and forward local HTTP/2 connections to it.
    Connect {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        runner: iroh::EndpointId,
        /// Route through Forge even when a direct UDP path is available.
        #[arg(long)]
        relay_only: bool,
        #[arg(long, default_value = "127.0.0.1:7744")]
        listen: SocketAddr,
    },
    /// Serve remote build sessions on a loopback interface.
    Serve {
        #[arg(long, default_value = "127.0.0.1:7744")]
        listen: SocketAddr,
        #[arg(long)]
        root: PathBuf,
        /// Persistent peer key. Enables the Forge relay transport.
        #[arg(long)]
        peer_key: Option<PathBuf>,
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
        /// Accepted for compatibility. Output always streams.
        #[arg(long, hide = true)]
        stream: bool,
        #[arg(long, allow_negative_numbers = true, value_parser = crate::cache_lib::zstd_level::parse_zstd_level)]
        cache_zstd_level: Option<i32>,
        #[arg(value_enum)]
        action: BuildAction,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Fetch matching files from a legacy on-disk session into a new directory.
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
    let command = match args.command {
        RemoteCommand::PeerId { key } => {
            println!("{}", peer::identity(&key)?.public());
            return Ok(0);
        }
        RemoteCommand::Connect {
            key,
            runner,
            listen,
            relay_only,
        } => {
            return peer::connect(&key, runner, listen, relay_only).await;
        }
        command => command,
    };
    let token = read_token(args.token_file)?;
    let token = token.trim();
    match command {
        RemoteCommand::Serve {
            listen,
            root,
            slots,
            artifact_streams,
            peer_key,
        } => serve(listen, root, token, slots, artifact_streams, peer_key).await,
        RemoteCommand::Run {
            endpoint,
            attestation,
            manifest,
            fetch_out,
            fetch,
            stream: _,
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
            let client = Client::new(endpoint, token)?;
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
            client.upload_workspace(&session, snapshot.blobs).await?;
            eprintln!("Remote build session {}", session.id);
            let code = match client.finish_run(&session.id, &attestation).await {
                // A diagnostic could write to the same blocked pipe.
                Err(error) if control::terminal_blocked(&error) => return Ok(1),
                result => result?,
            };
            if let Some(out) = fetch_out {
                artifacts::download(&client, &session.id, &out, fetch).await?;
            }
            Ok(code)
        }
        RemoteCommand::Fetch {
            endpoint,
            out,
            session,
            globs,
        } => {
            let client = Client::new(endpoint, token)?;
            artifacts::download(&client, &session, &out, globs).await?;
            Ok(0)
        }
        RemoteCommand::PeerId { .. } | RemoteCommand::Connect { .. } => unreachable!(),
    }
}

fn read_token(path: Option<PathBuf>) -> io::Result<String> {
    let token_file = path.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "--token-file is required for serve, run and fetch",
        )
    })?;
    let token = std::fs::read_to_string(token_file)?;
    if token.trim().len() != 64 || !token.trim().bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::other(
            "Remote build token file must contain 64 hexadecimal characters.",
        ));
    }
    Ok(token)
}

async fn serve(
    listen: SocketAddr,
    root: PathBuf,
    token: &str,
    slots: Option<u32>,
    artifact_streams: u8,
    peer_key: Option<PathBuf>,
) -> io::Result<i32> {
    let listener = peer::loopback_listener(listen).await?;
    let address = listener.local_addr()?;
    let serve = server::serve(listener, root, token, slots, artifact_streams);
    if let Some(key) = peer_key {
        let (code, ()) = tokio::try_join!(serve, peer::serve(&key, address))?;
        Ok(code)
    } else {
        serve.await
    }
}

struct Client<'a> {
    http: LoopbackPostClient,
    token: &'a str,
}

impl<'a> Client<'a> {
    async fn upload_workspace(
        &self,
        session: &Session,
        mut blobs: std::collections::BTreeMap<String, Vec<u8>>,
    ) -> io::Result<()> {
        const TARGET_BATCH: usize = 16 * 1024 * 1024;
        const MAX_BATCH_ENTRIES: usize = 512;
        let mut batch = wire::WorkspaceBatch::default();
        let mut size = 0;
        for hash in &session.missing {
            let data = blobs
                .remove(hash)
                .ok_or_else(|| io::Error::other("runner requested an unknown source digest"))?;
            if data.len() >= TARGET_BATCH {
                let _: Blob = self.post(&format!("blob/{hash}"), &Blob { data }).await?;
                continue;
            }
            if size + data.len() > TARGET_BATCH || batch.blobs.len() == MAX_BATCH_ENTRIES {
                self.post_workspace_batch(&session.id, &batch).await?;
                batch.blobs.clear();
                size = 0;
            }
            size += data.len();
            batch.blobs.push(wire::WorkspaceBlob {
                digest: hash.clone(),
                data,
            });
        }
        if !batch.blobs.is_empty() {
            self.post_workspace_batch(&session.id, &batch).await?;
        }
        Ok(())
    }

    async fn post_workspace_batch(
        &self,
        session: &str,
        batch: &wire::WorkspaceBatch,
    ) -> io::Result<()> {
        let _: Blob = self
            .post_encoded(
                &format!("workspace/{session}"),
                wire::encode_at_level(batch, 9)?,
            )
            .await?;
        Ok(())
    }

    async fn finish_run(&self, session: &str, attestation: &std::path::Path) -> io::Result<i32> {
        let code = control::receive(self, session).await?;
        let compressed = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            self.http.get(
                &format!("/v1/sessions/{session}/artifact"),
                self.token,
                &[("path", "attestation.json")],
                wire::MAX_FRAME + 1024 * 1024,
            ),
        )
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "remote attestation download timed out",
            )
        })??;
        let bytes = wire::decompress(&compressed)?;
        if let Some(parent) = attestation
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(attestation, bytes).await?;
        Ok(code)
    }

    fn new(endpoint: SocketAddr, token: &'a str) -> io::Result<Self> {
        Ok(Self {
            http: LoopbackPostClient::with_http2(endpoint, true)?,
            token,
        })
    }

    async fn post<T: Message + Default>(
        &self,
        operation: &str,
        message: &impl Message,
    ) -> io::Result<T> {
        self.post_encoded(operation, wire::encode(message)?).await
    }

    async fn post_encoded<T: Message + Default>(
        &self,
        operation: &str,
        body: Vec<u8>,
    ) -> io::Result<T> {
        let data = self
            .http
            .post(
                &format!("/v1/{operation}"),
                self.token,
                "application/x-protobuf+zstd",
                body,
                wire::MAX_FRAME + 1024 * 1024,
            )
            .await?;
        wire::decode(&data)
    }
}
