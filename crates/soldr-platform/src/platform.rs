//! Host-neutral capability facades.
//!
//! This file and everything under `platform/` is a facade surface: it may
//! re-export or wrap the selected concrete implementation, but it contains
//! **no host cfg of its own**. Each leaf re-exports its implementation
//! through [`crate::platform_imp`], which `lib.rs` selects exactly once.
//!
//! The namespaces (issue #2493, plus `sync` from soldr#3836):
//!
//! - [`process`] — command configuration, spawn, terminate, inspect, exit
//! - [`fs`] — identity, links, permissions, replace, volume, positioned I/O
//! - [`ipc`] — endpoint, listener, connect, peer, handoff
//! - [`executable`] — name, search, image, materialize
//! - [`host`] — facts, resources, dirs, user
//! - [`sync`] — bounded-wait locks, sync and async (critical-section and timeout flavors)

pub mod executable;
pub mod fs;
pub mod host;
pub mod ipc;
pub mod process;
pub mod sync;
pub mod wait;
