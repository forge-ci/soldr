//! Host phone-home, compiled into the single Soldr executable.
//! The Ed25519 seed is shared with dist's iroh identity; no second key is needed.
pub mod backoff;
pub mod checks;
pub mod config;
pub mod host_identity;
pub mod proto;
pub mod runtime;
pub mod status;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
