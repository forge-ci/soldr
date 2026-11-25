//! Host phone-home CLI; `agent capabilities` retains its existing contract.
use crate::core::SoldrError;
use clap::Subcommand;
use soldr_agent::{config, host_identity, status::Status};
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Print the existing agent capability negotiation document
    Capabilities {
        #[arg(long)]
        json: bool,
    },
    /// Create a host key once and print its enrollment public key
    Keygen {
        #[arg(long)]
        key: Option<PathBuf>,
    },
    /// Phone home to signal and serve LAN health checks
    Run {
        #[arg(long, default_value = "/etc/soldr-agent.toml")]
        config: PathBuf,
        #[arg(long)]
        key: Option<PathBuf>,
    },
    /// Read the last connection state (updated_ms shows its freshness)
    Status {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
}
/// Materialize Soldr itself under its service process identity, never a new binary.
pub fn materialize_alias() -> Result<PathBuf, SoldrError> {
    crate::binaries::soldr_agent_binary()
}
pub async fn run(command: AgentCommand) -> Result<i32, SoldrError> {
    execute(command)
        .await
        .map_err(|e| SoldrError::Other(format!("soldr agent: {e}")))
}
async fn execute(command: AgentCommand) -> soldr_agent::Result<i32> {
    match command {
        AgentCommand::Capabilities { json } => {
            let mut args = vec!["capabilities".into()];
            if json {
                args.push("--json".into());
            }
            return crate::agent_cmd::run(&args).map_err(Into::into);
        }
        AgentCommand::Keygen { key } => {
            let path = key.unwrap_or_else(config::default_key);
            println!("{}", host_identity::load_or_create(&path)?.public_key());
        }
        AgentCommand::Status { json, state_dir } => {
            let status = Status::read(&state_dir.unwrap_or_else(config::default_state_dir))?;
            if json {
                println!("{}", serde_json::to_string(&status)?);
            } else {
                println!(
                    "{} (updated {} ms): {} — {}",
                    status.state, status.updated_ms, status.signal_url, status.detail
                );
            }
        }
        AgentCommand::Run { config: path, key } => {
            let mut config = config::Config::load(&path)?;
            if let Some(key) = key {
                config.key = key;
            }
            let key = host_identity::load_or_create(&config.key)?;
            let (stop, receiver) = tokio::sync::watch::channel(false);
            let run = soldr_agent::runtime::run(config, key, receiver);
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => result?,
                result = tokio::signal::ctrl_c() => {
                    result?;
                    stop.send(true)?;
                    run.await?;
                }
                received = crate::platform::process::signal::wait_for_terminate_signal() => {
                    if !received { return Err("failed to install agent termination handler".into()); }
                    stop.send(true)?;
                    run.await?;
                }
            }
        }
    }
    Ok(0)
}
