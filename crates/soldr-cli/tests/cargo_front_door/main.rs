//! `soldr cargo` front-door dispatch, trampoline, and rustc-wrapper integration tests.
//!
//! soldr#2934: one linked test binary per category instead of one per source
//! file. Each module below was previously its own top-level test binary, so
//! test IDs are now `<module>::<test_name>`.

#[path = "../common/mod.rs"]
mod common;

mod cli_cargo_basic;
mod cli_cargo_clippy_fixture;
mod cli_cargo_doc_routes;
mod cli_cargo_dylint_link;
mod cli_cargo_linker;
mod cli_cargo_native_cc;
mod cli_cargo_nested_cargo_guard;
mod cli_cargo_run_trampoline;
mod cli_cargo_strip_failure;
mod cli_cargo_toolchain_env;
mod cli_cargo_trampoline_workspace;
mod cli_cargo_wrappers;
mod cli_cargo_zccache_mode;
mod cli_dispatch;
mod cli_dylint_wrapper;
mod cli_exec;
mod cli_front_door_cold_start;
mod cli_reentrancy_guard;
mod cli_reentrancy_guard_canary;
mod cli_toolchain_download_guard;
mod cli_wrapper;
mod cli_wrapper_identity;
mod cli_wrapper_perf;
mod cold_first_compile;
mod fixture_daemon_isolation;
mod zccache_trampoline_gate;
