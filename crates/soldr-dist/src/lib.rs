//! One-joblet Dist transport, independent of zccache's executor API.
pub mod cas;
pub mod cli;
pub mod client;
pub mod client_connections;
pub mod compiler;
mod compiler_image;
pub mod dep_info;
pub mod host_load;
pub mod metrics;
pub mod proto;
pub mod rust_inputs;
mod rust_output;
pub mod sandbox;
pub mod status;
pub mod transport;
pub mod trust;
pub mod worker;

pub use client::DistClient;
pub use proto::{DistJob, InputFile, JobMetrics};
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DistOutcome {
    Executed {
        exit_code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        metrics: JobMetrics,
    },
    Declined {
        reason: String,
        metrics: JobMetrics,
    },
}

impl DistOutcome {
    pub fn metrics(&self) -> &JobMetrics {
        match self {
            Self::Executed { metrics, .. } | Self::Declined { metrics, .. } => metrics,
        }
    }
    pub(crate) fn set_metrics(&mut self, value: JobMetrics) {
        match self {
            Self::Executed { metrics, .. } | Self::Declined { metrics, .. } => *metrics = value,
        }
    }
}
