//! Toolchain lifecycle, doctor probes, save/archive transport, and host
//! platform discovery integration tests.
//!
//! soldr#2934: one linked test binary per category instead of one per source
//! file. Each module below was previously its own top-level test binary, so
//! test IDs are now `<module>::<test_name>`.

#[path = "../common/mod.rs"]
mod common;

mod cargo_target_dir_parity;
mod cli_doctor;
mod cli_optimize;
mod cli_save;
mod cli_toolchain;
mod cli_toolchain_doctor;
mod cli_toolchain_home_boundary;
mod doctor_standalone_zccache;
mod msvc_host_discovery_windows;
mod python_compat_sysroot;
mod rustup_child_exit_guard;
mod save_auto_defender_exclude;
mod save_bench;
mod save_roundtrip;
mod source_mtime_freshness;
