//! Public zccache crate.
//!
//! Release packaging rewrites this facade into a self-contained crate so
//! crates.io consumers only depend on `zccache`, while git and path consumers
//! still get parallel compilation from the internal workspace crates.

pub mod artifact;
pub mod audit;
pub mod cli_core;
pub mod compile_trace;
pub mod compiler;
pub mod core;
mod daemon_core;
pub mod depgraph;
#[cfg(feature = "download")]
pub mod download;
#[cfg(feature = "download-protocol")]
pub mod download_protocol;
pub mod fingerprint;
pub mod fscache;
#[cfg(feature = "gha")]
pub mod gha;
pub mod hash;
pub mod ipc;
pub mod protocol;
#[cfg(feature = "symbols")]
pub mod symbols;
pub mod watcher;
/// Bounded-wait locks: every wait and hold is watched (soldr#3836).
pub mod sync;
/// Timeout-only blocking waits (soldr#3836).
pub mod wait;
#[cfg(feature = "mimalloc-allocator")]
pub use mimalloc_pprof;
/// Issue zccache#926 - durable audit JSONL writer for the embedded service.
pub use daemon_core::audit_writer;
#[cfg(feature = "ci")]
pub mod ci;
#[cfg(feature = "cli")]
pub use cli_core::cli;
#[cfg(feature = "formatter")]
pub use cli_core::formatter;
#[cfg(feature = "symbols")]
#[doc(hidden)]
pub mod dev_daemon_identity;
pub use daemon_core::daemon;
#[cfg(feature = "download-client")]
pub use cli_core::download_client;
#[cfg(feature = "download-daemon")]
pub use cli_core::download_daemon;
#[cfg(feature = "download-daemon-entry")]
#[doc(hidden)]
pub mod download_daemon_entry;
pub use daemon_core::embedded;

#[cfg(feature = "test-support")]
pub use daemon_core::test_support;
