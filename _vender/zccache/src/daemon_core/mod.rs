//! `zccache-daemon-core` — the zccache daemon subsystem (#1018 crate split).
//!
//! Extracted from the monolithic `zccache` crate. The subsystem crates are
//! re-exported under the same short aliases the daemon code uses (`core`,
//! `ipc`, …) and the `daemon` / `embedded` / `audit_writer` / `test_support`
//! module structure is preserved, so internal `crate::daemon_core::daemon::…` paths resolve
//! unchanged. The `zccache` facade re-exports these modules to keep the public
//! `zccache::daemon::…` / `zccache::embedded::…` paths stable.

// Subsystem-crate aliases (mirrors the former `zccache` facade so the moved
// daemon code's `crate::daemon_core::core` / `crate::daemon_core::ipc` / … paths keep resolving).
pub use crate::artifact as artifact;
pub use crate::audit as audit;
pub use crate::compile_trace as compile_trace;
pub use crate::compiler as compiler;
pub use crate::core as core;
pub use crate::depgraph as depgraph;
pub use crate::fingerprint as fingerprint;
pub use crate::fscache as fscache;
pub use crate::hash as hash;
pub use crate::ipc as ipc;
pub(crate) mod platform;
pub use crate::protocol as protocol;
pub use crate::watcher as watcher;

pub mod audit_writer;
pub mod daemon;
pub mod embedded;

#[cfg(feature = "test-support")]
pub mod test_support;
