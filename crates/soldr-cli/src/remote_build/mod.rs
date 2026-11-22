//! Whole-workspace builds on a trusted runner reached over an SSH tunnel.

mod server;
mod snapshot;
mod wire;

use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use prost::Message;

use self::wire::{Action, Artifacts, Blob, Fetch, ResultFrame, Session, Start};

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
    },
    /// Run a command against the current Git commit on the runner.
    Run {
        #[arg(long, default_value = "127.0.0.1:7744")]
        endpoint: SocketAddr,
        #[arg(long, default_value = "attestation.json")]
        attestation: PathBuf,
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
        RemoteCommand::Serve { listen, root } => server::serve(listen, root, token).await,
        RemoteCommand::Run {
            endpoint,
            attestation,
            cache_zstd_level,
            action,
            args,
        } => {
            let snapshot = snapshot::capture(&std::env::current_dir()?)?;
            let client = Client::new(endpoint, token)?;
            let action = match action {
                BuildAction::Build => Action::Build,
                BuildAction::Test => Action::Test,
                BuildAction::Dylint => Action::Dylint,
            };
            let session: Session = client
                .post(
                    "start",
                    &Start {
                        commit: snapshot.commit,
                        files: snapshot.files,
                        action: action as i32,
                        args,
                        cache_zstd_level,
                    },
                )
                .await?;
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
            std::fs::write(attestation, result.attestation)?;
            io::stdout().write_all(&result.stdout)?;
            io::stderr().write_all(&result.stderr)?;
            Ok(result.exit_code)
        }
        RemoteCommand::Fetch {
            endpoint,
            out,
            session,
            globs,
        } => {
            let client = Client::new(endpoint, token)?;
            let artifacts: Artifacts = client
                .post(&format!("fetch/{session}"), &Fetch { globs })
                .await?;
            std::fs::create_dir(&out)?;
            for artifact in artifacts.files {
                let path = out.join(snapshot::relative_path(&artifact.path)?);
                std::fs::create_dir_all(
                    path.parent()
                        .ok_or_else(|| io::Error::other("missing artifact parent"))?,
                )?;
                server::write_file(&path, &artifact.data, artifact.executable)?;
            }
            Ok(0)
        }
    }
}

struct Client<'a> {
    token: &'a str,
    http: reqwest::Client,
    endpoint: SocketAddr,
}

impl<'a> Client<'a> {
    fn new(endpoint: SocketAddr, token: &'a str) -> io::Result<Self> {
        if !endpoint.ip().is_loopback() {
            return Err(io::Error::other(
                "Remote build endpoint must be a loopback SSH tunnel.",
            ));
        }
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(io::Error::other)?;
        Ok(Self {
            http,
            endpoint,
            token,
        })
    }

    async fn post<T: Message + Default>(
        &self,
        operation: &str,
        message: &impl Message,
    ) -> io::Result<T> {
        let mut response = self
            .http
            .post(format!("http://{}/v1/{operation}", self.endpoint))
            .header("content-type", "application/x-protobuf+zstd")
            .bearer_auth(self.token)
            .body(wire::encode(message)?)
            .send()
            .await
            .map_err(io::Error::other)?;
        let mut data = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
            if data.len() + chunk.len() > wire::MAX_FRAME + 1024 * 1024 {
                return Err(io::Error::other("remote build response exceeds 256 MiB"));
            }
            data.extend_from_slice(&chunk);
        }
        if !response.status().is_success() {
            return Err(io::Error::other(format!(
                "remote build {}: {}",
                response.status(),
                String::from_utf8_lossy(&data)
            )));
        }
        wire::decode(&data)
    }
}
