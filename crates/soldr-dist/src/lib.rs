//! One-joblet Dist transport, independent of zccache's executor API.
pub mod cas;
pub mod cli;
pub mod client;
pub mod compiler;
pub mod host_load;
pub mod proto;
pub mod sandbox;
pub mod transport;
pub mod trust;
pub mod worker;

pub use client::DistClient;
pub use proto::{DistJob, InputFile};
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, PartialEq, Eq)]
pub enum DistOutcome {
    Executed {
        exit_code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Declined {
        reason: String,
    },
}
