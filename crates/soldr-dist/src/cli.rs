//! Debug command surface. No discovery or zccache integration.
use crate::{
    compiler::Compiler,
    transport,
    trust::{self, Trust},
    worker::Worker,
    DistClient, Result,
};
use clap::{Args, Subcommand};
use std::{
    path::{Path, PathBuf},
    sync::{atomic::AtomicU64, Arc},
};

mod observe;
mod scaffold;
pub use observe::trace_filter;
#[derive(Args)]
pub struct DistArgs {
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,
    #[arg(long, global = true, env = "SOLDR_DIST_RELAY_URL")]
    pub relay_url: Option<String>,
    /// Forge HTTPS relay URL, or none for direct LAN; third-party relays are forbidden.
    #[arg(long, global = true, value_parser = transport::parse_relay_selection, env = "SOLDR_DIST_RELAY")]
    pub relay: Option<String>,
    #[command(subcommand)]
    pub command: DistCommand,
}
#[derive(Subcommand)]
pub enum DistCommand {
    /// Lend compiler capacity to explicitly trusted clients.
    #[command(visible_alias = "worker")]
    Serve {
        #[arg(long, default_value_t = 1)]
        slots: usize,
        #[arg(long, default_value_t = 10737418240)]
        cas_budget: u64,
        #[arg(long)]
        host_load_file: Option<PathBuf>,
        /// Registry configuration only; registration is a later slice.
        #[arg(
            long,
            env = "SOLDR_DIST_SIGNAL_URL",
            default_value = "https://signal.forge-ci.com"
        )]
        signal_url: String,
        /// Shared Ed25519 seed; defaults to <root>/node.key.
        #[arg(long)]
        key: Option<PathBuf>,
        /// Repeat with full client tickets to establish worker-originated connections.
        #[arg(long = "client-ticket", value_name = "JSON_TICKET")]
        clients: Vec<String>,
        /// Refuse client-initiated QUIC connections and use configured clients only.
        #[arg(long, requires = "clients")]
        outbound_only: bool,
    },
    /// Run one debug compiler invocation on an explicitly trusted worker.
    Exec {
        /// One static node ID (with relay URL) or a printed JSON ticket.
        #[arg(long, env = "SOLDR_DIST_PEERS")]
        worker: String,
        /// Wait only for an authenticated worker-originated connection.
        #[arg(long, env = "SOLDR_DIST_REVERSE_ONLY")]
        reverse_only: bool,
        /// Print transport and CAS counters to stderr.
        #[arg(long)]
        verbose: bool,
        /// Emit outcome/metrics JSON, preserving streams as byte arrays.
        #[arg(long)]
        json: bool,
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
    observe::init_tracing();
    let root = args.root.map(Ok).unwrap_or_else(default_root)?;
    std::fs::create_dir_all(&root)?;
    let root = std::fs::canonicalize(root)?;
    let relay_disabled = args.relay.as_deref() == Some("none");
    let relay_url = transport::relay_config(args.relay.as_deref(), args.relay_url)?;
    match args.command {
        DistCommand::Serve {
            slots,
            cas_budget,
            host_load_file,
            signal_url,
            key,
            clients,
            outbound_only,
        } => {
            serve(
                &root,
                relay_url.as_deref(),
                ServeConfig {
                    slots,
                    cas_budget,
                    host_load_file,
                    signal_url,
                    key,
                    clients,
                    outbound_only,
                },
            )
            .await
        }
        DistCommand::Exec {
            worker,
            argv,
            reverse_only,
            verbose,
            json,
        } => {
            let identity = trust::node_key(&root)?;
            let _endpoint_lease =
                transport::EndpointLease::acquire_with_key(&root, &root.join("node.key"))?;
            let job = scaffold::job(&argv, &root).await?;
            let worker = transport::parse_worker(&worker, relay_url.as_deref(), reverse_only)?;
            let relay = if relay_disabled {
                None
            } else {
                relay_url.or_else(|| worker.relay_urls().next().map(ToString::to_string))
            };
            let endpoint =
                transport::endpoint_with_identity(identity, false, relay.as_deref()).await?;
            let client = DistClient::new(endpoint, worker, &root)?.with_reverse_only(reverse_only);
            let outcome = client.execute(job).await;
            client.endpoint.close().await;
            observe::render(outcome, verbose, json)
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
struct ServeConfig {
    slots: usize,
    cas_budget: u64,
    host_load_file: Option<PathBuf>,
    signal_url: String,
    key: Option<PathBuf>,
    clients: Vec<String>,
    outbound_only: bool,
}
async fn serve(root: &Path, relay: Option<&str>, config: ServeConfig) -> Result<i32> {
    let ServeConfig {
        slots,
        cas_budget,
        host_load_file,
        signal_url,
        key,
        clients,
        outbound_only,
    } = config;
    std::fs::create_dir_all(root.join("tmp"))?;
    let (identity, key_path) = match key {
        Some(path) => (trust::load_or_create_key(&path)?, path),
        None => (trust::node_key(root)?, root.join("node.key")),
    };
    let _endpoint_lease = transport::EndpointLease::acquire_with_key(root, &key_path)?;
    let endpoint = transport::endpoint_with_identity(identity, false, relay).await?;
    if relay.is_some() {
        tokio::time::timeout(std::time::Duration::from_secs(10), endpoint.online())
            .await
            .map_err(|_| "dist relay did not become online within 10 seconds")?;
    } else if endpoint.addr().ip_addrs().next().is_none() {
        return Err("dist direct LAN endpoint has no reachable interface addresses".into());
    }
    let mut compilers = vec![Compiler::rustc_from_soldr(root, &std::env::current_exe()?).await?];
    // Every toolchain clients use must be offered by identity; a worker that only
    // offers its own default declines every unit built with another version.
    for channel in served_toolchains(root)? {
        let compiler =
            Compiler::rustc_toolchain_from_soldr(root, &std::env::current_exe()?, &channel).await?;
        let first = |c: &Compiler| c.identity.lines().next().map(str::to_owned);
        if !compilers.iter().any(|c| first(c) == first(&compiler)) {
            eprintln!("Dist worker serves rustc toolchain {channel}");
            compilers.push(compiler);
        }
    }
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
        signal_url,
        total_cpus: std::thread::available_parallelism()?.get(),
        uploaded_blobs: Arc::new(AtomicU64::new(0)),
        clients: clients
            .iter()
            .map(|ticket| transport::parse_ticket(ticket))
            .collect::<Result<_>>()?,
        outbound_only,
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
    let status = crate::status::load(root)?;
    if json {
        println!("{}", crate::status::json(&status));
    } else {
        println!("Dist {} ({})\nBuild: {}\nSignal: {}\nCAS: {} bytes; running joblets: {}\nTrusted clients: {:?}\nTrusted workers: {:?}", status.node_id, status.role, status.build_id, status.signal_url, status.cas_bytes, status.running_joblets, status.trusted_clients, status.trusted_workers);
        if let Some(metrics) = &status.last_job {
            println!("Last job: {}", metrics.summary());
        }
    }
    Ok(())
}

/// Toolchain names listed one per line in `<root>/toolchains` (`#` comments allowed).
fn served_toolchains(root: &std::path::Path) -> crate::Result<Vec<String>> {
    let path = root.join("toolchains");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(format!("{}: {error}", path.display()).into()),
    };
    let mut channels = vec![];
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if !line
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return Err(format!("{}: invalid toolchain name {line:?}", path.display()).into());
        }
        channels.push(line.to_owned());
    }
    Ok(channels)
}

#[cfg(test)]
mod served_toolchain_tests {
    #[test]
    fn toolchain_list_skips_comments_and_rejects_paths() {
        let temp = tempfile::tempdir().unwrap();
        assert!(super::served_toolchains(temp.path()).unwrap().is_empty());
        std::fs::write(temp.path().join("toolchains"), "# client pins\n1.95.0\n\nnightly-2026-08-15 # dylint\n").unwrap();
        assert_eq!(super::served_toolchains(temp.path()).unwrap(), ["1.95.0", "nightly-2026-08-15"]);
        std::fs::write(temp.path().join("toolchains"), "../evil\n").unwrap();
        assert!(super::served_toolchains(temp.path()).is_err());
    }
}
