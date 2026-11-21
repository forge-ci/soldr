//! Debug command surface. No discovery or zccache integration.
use crate::{
    cas::Cas,
    compiler::Compiler,
    proto::Status,
    transport,
    trust::{self, Trust},
    worker::Worker,
    DistClient, DistOutcome, Result,
};
use clap::{Args, Subcommand};
use prost::Message;
use std::{
    path::{Path, PathBuf},
    sync::{atomic::AtomicU64, Arc},
};

mod scaffold;
#[derive(Args)]
pub struct DistArgs {
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,
    #[arg(long, global = true, env = "SOLDR_DIST_RELAY_URL")]
    pub relay_url: Option<String>,
    #[command(subcommand)]
    pub command: DistCommand,
}
#[derive(Subcommand)]
pub enum DistCommand {
    /// Lend compiler capacity to explicitly trusted clients.
    Serve {
        #[arg(long, default_value_t = 1)]
        slots: usize,
        #[arg(long, default_value_t = 10737418240)]
        cas_budget: u64,
        #[arg(long)]
        host_load_file: Option<PathBuf>,
    },
    /// Run one debug compiler invocation on an explicitly trusted worker.
    Exec {
        #[arg(long)]
        worker: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// Pin a peer's authenticated Ed25519 identity.
    Trust {
        #[command(subcommand)]
        command: TrustCommand,
    },
    /// Inspect local worker snapshot and configured trust.
    Status {
        #[arg(long)]
        json: bool,
    },
}
#[derive(Subcommand)]
pub enum TrustCommand {
    /// Grant both client and worker trust (use --role for one direction).
    Add {
        node_id: String,
        #[arg(long, value_parser = ["client", "worker", "both"], default_value = "both")]
        role: String,
    },
}
pub fn default_root() -> Result<PathBuf> {
    let home = soldr_platform::host::dirs::home().ok_or("cannot resolve home directory")?;
    Ok(home.join(".soldr/dist"))
}
pub async fn run(args: DistArgs) -> Result<i32> {
    let root = args.root.map(Ok).unwrap_or_else(default_root)?;
    std::fs::create_dir_all(&root)?;
    match args.command {
        DistCommand::Serve {
            slots,
            cas_budget,
            host_load_file,
        } => {
            let relay = args
                .relay_url
                .as_deref()
                .ok_or("dist serve requires --relay-url or SOLDR_DIST_RELAY_URL")?;
            serve(&root, slots, cas_budget, host_load_file, relay).await
        }
        DistCommand::Exec { worker, argv } => {
            let _endpoint_lease = transport::EndpointLease::acquire(&root)?;
            let job = scaffold::job(&argv).await?;
            let worker = transport::parse_ticket(&worker)?;
            let relay = args
                .relay_url
                .or_else(|| worker.relay_urls().next().map(ToString::to_string));
            let endpoint = transport::endpoint_with_relay(&root, false, relay.as_deref()).await?;
            let client = DistClient::new(endpoint, worker, &root)?;
            let outcome = client.execute(job).await;
            client.endpoint.close().await;
            match outcome {
                DistOutcome::Executed {
                    exit_code,
                    stdout,
                    stderr,
                } => {
                    use std::io::Write;
                    std::io::stdout().write_all(&stdout)?;
                    std::io::stderr().write_all(&stderr)?;
                    Ok(exit_code)
                }
                DistOutcome::Declined { reason } => Err(format!("Dist declined: {reason}").into()),
            }
        }
        DistCommand::Trust {
            command: TrustCommand::Add { node_id, role },
        } => {
            let id = node_id.parse()?;
            if role != "worker" {
                Trust::add(&root.join("trusted_clients.toml"), id)?;
            }
            if role != "client" {
                Trust::add(&root.join("trusted_workers.toml"), id)?;
            }
            Ok(0)
        }
        DistCommand::Status { json } => {
            status(&root, json)?;
            Ok(0)
        }
    }
}
async fn serve(
    root: &Path,
    slots: usize,
    cas_budget: u64,
    host_load_file: Option<PathBuf>,
    relay: &str,
) -> Result<i32> {
    let _endpoint_lease = transport::EndpointLease::acquire(root)?;
    let endpoint = transport::endpoint_with_relay(root, false, Some(relay)).await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), endpoint.online())
        .await
        .map_err(|_| "dist relay did not become online within 10 seconds")?;
    let mut compilers = vec![Compiler::rustc().await?];
    if let Some(cc) = scaffold::find_program("cc") {
        compilers.push(Compiler::cc(&cc)?);
    }
    let terminate = soldr_platform::process::signal::wait_for_terminate_signal();
    println!(
        "Dist node: {}\nDist ticket: {}",
        endpoint.id(),
        transport::ticket(&endpoint.addr())?
    );
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let worker = Worker {
        endpoint,
        root: root.into(),
        compilers,
        slots,
        cas_budget,
        host_load_file,
        total_cpus: std::thread::available_parallelism()?.get(),
        uploaded_blobs: Arc::new(AtomicU64::new(0)),
    };
    let signal = tokio::spawn(async move {
        tokio::select! {
            _ = terminate => {},
            result = tokio::signal::ctrl_c() => { if let Err(e) = result { eprintln!("Dist signal: {e}"); } }
        }
        let _ = shutdown.send(true);
    });
    let result = worker.serve(receiver).await;
    signal.abort();
    result?;
    Ok(0)
}
fn status(root: &Path, json: bool) -> Result<()> {
    let path = root.join("status.pb");
    let mut status = if path.exists() {
        Status::decode(std::fs::read(path)?.as_slice())?
    } else {
        Status {
            node_id: trust::node_key(root)?.public().to_string(),
            role: "client".into(),
            ..Status::default()
        }
    };
    if status.pid != 0 && !soldr_platform::process::inspect::is_alive(status.pid as u32) {
        status.role = "client".into();
        status.running_joblets = 0;
        status.pid = 0;
    }
    status.trusted_clients = Trust::load(&root.join("trusted_clients.toml"))?.peers;
    status.trusted_workers = Trust::load(&root.join("trusted_workers.toml"))?.peers;
    status.cas_bytes = Cas::new(root, u64::MAX)?.size()?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "node_id": status.node_id, "role": status.role, "trusted_clients": status.trusted_clients, "trusted_workers": status.trusted_workers, "cas_bytes": status.cas_bytes, "running_joblets": status.running_joblets, "pid": status.pid })
        );
    } else {
        println!("Dist {} ({})\nCAS: {} bytes; running joblets: {}\nTrusted clients: {:?}\nTrusted workers: {:?}", status.node_id, status.role, status.cas_bytes, status.running_joblets, status.trusted_clients, status.trusted_workers);
    }
    Ok(())
}
