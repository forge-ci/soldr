//! Process details used by the broker inventory and the on-disk lease.

use kernal_api::platform::process::{self, ProcessLiveness};
use std::path::PathBuf;

pub use crate::platform_imp::process::metadata::{
    arguments_and_environment, snapshot, start_time_unix,
};

/// Resolve the running process image through a retained liveness reference.
pub fn executable_path(pid: u32) -> Option<PathBuf> {
    process::executable_path(&ProcessLiveness::open(pid).ok()?).ok()
}

/// Process-table fields used to discover Soldr services and follow parent links.
pub struct ProcessEntry {
    /// Process identifier.
    pub pid: u32,
    /// Parent process identifier, when available.
    pub parent_pid: Option<u32>,
    /// Process name for display and candidate filtering.
    pub name: String,
    /// Whether the process is runnable, including sleeping processes.
    pub running: bool,
}
