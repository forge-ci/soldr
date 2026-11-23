//! Whole-workspace builds on a trusted runner reached over an SSH tunnel.

mod artifacts;
mod auth;
pub(crate) mod auto;
mod control;
mod manifest;
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
use soldr_fetch::fetch::RemoteBuildHttpClient;

use self::wire::{Action, Blob, Session, Start};

#[derive(Args)]
pub(crate) struct RemoteArgs {
    /// Shared token file for a runner reached over an SSH tunnel.
    #[arg(long)]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: RemoteCommand,
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Serve remote build sessions on a loopback interface.
    Serve {
        /// File containing the separate Forge CI worker grant signing key.
        #[arg(long)]
        forge_key_file: Option<PathBuf>,
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
    /// Run a command against a Git checkout on the runner.
    Run {
        #[arg(long)]
        endpoint: Option<SocketAddr>,
        #[arg(long, default_value = "target/attestation.json")]
        attestation: PathBuf,
        /// Build the current worktree, including uncommitted and nonignored files.
        #[arg(long, conflicts_with = "manifest")]
        working_tree: bool,
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
    let RemoteArgs {
        token_file,
        command,
    } = args;
    match command {
        RemoteCommand::Serve {
            forge_key_file,
            listen,
            root,
            slots,
            artifact_streams,
        } => {
            let auth = match (forge_key_file.as_deref(), token_file.as_deref()) {
                (Some(path), None) => auth::RunnerAuth::forge(path)?,
                (None, token) => auth::RunnerAuth::tunnel(&read_tunnel_token(token)?),
                (Some(_), Some(_)) => {
                    return Err(io::Error::other(
                        "Use one of --forge-key-file or --token-file.",
                    ));
                }
            };
            server::serve(listen, root, auth, slots, artifact_streams).await
        }
        RemoteCommand::Run {
            endpoint,
            attestation,
            working_tree,
            manifest,
            fetch_out,
            fetch,
            stream: _,
            cache_zstd_level,
            action,
            args,
        } => {
            let directory = std::env::current_dir()?;
            let snapshot = if working_tree {
                snapshot::capture_worktree(&directory)?
            } else {
                snapshot::capture(&directory)?
            };
            let manifest = manifest::read(manifest.as_deref(), &snapshot.commit)?;
            if let Some(manifest) = &manifest {
                snapshot::verify_parent(&directory, &snapshot.commit, manifest.parent())?;
            }
            let manifest_json = manifest.as_ref().map(serde_json::to_vec).transpose()?;
            let client = connect_for_run(token_file.as_deref(), endpoint).await?;
            let action = match action {
                BuildAction::Build => Action::Build,
                BuildAction::Test => Action::Test,
                BuildAction::Dylint => Action::Dylint,
                BuildAction::Lint => Action::Lint,
            };
            let session = client
                .start_session(
                    Start {
                        commit: snapshot.commit,
                        files: snapshot.files,
                        action: action as i32,
                        args,
                        cache_zstd_level,
                        manifest_json,
                        working_tree,
                    },
                    snapshot.blobs,
                )
                .await?;
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
            let token = read_tunnel_token(token_file.as_deref())?;
            let client = Client::new(endpoint, token)?;
            artifacts::download(&client, &session, &out, globs).await?;
            Ok(0)
        }
    }
}

fn read_tunnel_token(path: Option<&std::path::Path>) -> io::Result<String> {
    let path = path.ok_or_else(|| io::Error::other("This command requires --token-file."))?;
    let token = std::fs::read_to_string(path)?;
    let token = token.trim();
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::other(
            "Remote build token file must contain 64 hexadecimal characters.",
        ));
    }
    Ok(token.to_owned())
}

async fn connect_for_run(
    token_file: Option<&std::path::Path>,
    endpoint: Option<SocketAddr>,
) -> io::Result<Client> {
    if let Some(path) = token_file {
        let endpoint = endpoint.unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 7744)));
        return Client::new(endpoint, read_tunnel_token(Some(path))?);
    }
    if endpoint.is_some() {
        return Err(io::Error::other("--endpoint requires --token-file."));
    }
    let grant = crate::forge_sign_in::discover_worker()
        .await?
        .ok_or_else(|| {
            io::Error::other(
                "No Forge CI worker is configured. Check your account with soldr sign status.",
            )
        })?;
    Client::forge(grant)
}

struct Client {
    http: RemoteBuildHttpClient,
    token: String,
}

impl Client {
    fn forge(grant: crate::forge_sign_in::WorkerGrant) -> io::Result<Self> {
        Ok(Self {
            http: RemoteBuildHttpClient::https_worker(&grant.origin)?,
            token: grant.token,
        })
    }

    async fn start_session(
        &self,
        request: Start,
        blobs: std::collections::BTreeMap<String, Vec<u8>>,
    ) -> io::Result<Session> {
        let session: Session = self.post("start", &request).await?;
        if request.manifest_json.is_some() && !session.manifest_accepted {
            return Err(io::Error::other(
                "remote runner does not support session manifests; upgrade the runner",
            ));
        }
        if request.working_tree && !session.working_tree_accepted {
            return Err(io::Error::other(
                "remote runner does not support worktree snapshots; upgrade the runner",
            ));
        }
        self.upload_workspace(&session, blobs).await?;
        Ok(session)
    }

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
                &self.token,
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

    fn new(endpoint: SocketAddr, token: String) -> io::Result<Self> {
        Ok(Self {
            http: RemoteBuildHttpClient::with_http2(endpoint, true)?,
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
                &self.token,
                "application/x-protobuf+zstd",
                body,
                wire::MAX_FRAME + 1024 * 1024,
            )
            .await?;
        wire::decode(&data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn remote_run_accepts_forge_ci_and_tunnel_invocations() {
        for prefix in [vec![], vec!["--token-file", "token-file"]] {
            let mut cli = vec!["soldr", "remote"];
            cli.extend(prefix.iter().copied());
            cli.extend(["run", "build"]);
            let args = crate::cli_args::Cli::try_parse_from(cli).unwrap();
            let crate::cli_args::Commands::Remote(args) = args.command else {
                panic!("expected remote command");
            };
            assert_eq!(args.token_file.is_some(), !prefix.is_empty());
            assert!(matches!(args.command, RemoteCommand::Run { .. }));
        }
    }
}
