//! soldr-core — the foundation crate of the #1490 workspace split.
//!
//! Owns the shared types every other soldr crate reaches for: target
//! triple resolution, `~/.soldr/` layout (`SoldrPaths` / `SoldrConfig`),
//! `SoldrError`, the daemon wire schema (`core::wire`), the shared
//! Windows Defender exclusion plumbing, and shared per-test
//! watchdog. No I/O beyond config files.
//!
//! The `core` tree stays nested as `pub mod core;` (not flattened) so
//! `crate::core::…` paths inside the moved files — and
//! `soldr_cli::core::…` paths via the facade re-export — resolve
//! unchanged (#1490 mechanics rule M1).

#![allow(dead_code)]

/// Neutral host-platform facade (#2493): the single selection site lives
/// in `soldr-platform`; this crate calls only `crate::platform::…`.
pub(crate) use soldr_platform as platform;

pub mod build_log_meta;
pub mod build_provenance;
pub mod cargo_path_check;
pub mod core;
pub mod defender;
pub mod defender_probe;
pub(crate) mod elf_origin;
pub mod fuzzy_match;
pub(crate) mod macho_loader_path;
pub mod self_relocate;
pub mod startup_profile;
/// Shared test-support helpers (leaked-daemon diagnostic, process-env lock).
/// Not cfg-gated: cargo compiles the library *without* `cfg(test)`
/// when linking it into integration tests, so a `cfg(test)` gate
/// would hide the module from `tests/`. Per-test timeouts live in
/// `.config/nextest.toml`, not here (soldr#2493).
pub mod test_util;
pub mod warning_log;
