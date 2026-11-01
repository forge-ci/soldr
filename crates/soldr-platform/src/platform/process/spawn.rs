//! Detached/background spawn, daemon stdio plumbing, and exec-replacement
//! versus spawn-and-wait.

pub use crate::platform_imp::process::spawn::{
    daemon_command, daemon_stdio, exec_or_status, spawn_detached, spawn_holding_fork_window,
};
