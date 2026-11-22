use crate::{DistOutcome, Result};
use tracing_subscriber::EnvFilter;

pub fn trace_filter() -> EnvFilter {
    EnvFilter::try_from_env("RUST_LOG")
        .or_else(|_| EnvFilter::try_from_env("SOLDR_LOG"))
        .unwrap_or_else(|_| EnvFilter::new("warn"))
}

pub(super) fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(trace_filter())
        .with_writer(std::io::stderr)
        .try_init();
}

pub(super) fn render(outcome: DistOutcome, verbose: bool, json: bool) -> Result<i32> {
    if verbose {
        eprintln!("soldr dist: {}", outcome.metrics().summary());
    }
    if json {
        println!("{}", serde_json::to_string(&outcome)?);
    }
    match outcome {
        DistOutcome::Executed {
            exit_code,
            stdout,
            stderr,
            ..
        } => {
            if !json {
                use std::io::Write;
                std::io::stdout().write_all(&stdout)?;
                std::io::stderr().write_all(&stderr)?;
            }
            Ok(exit_code)
        }
        DistOutcome::Declined { reason, .. } => {
            if json {
                Ok(1)
            } else {
                Err(format!("Dist declined: {reason}").into())
            }
        }
    }
}
