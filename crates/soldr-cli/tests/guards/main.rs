//! Repository invariant guards: source-policy lints, manifest/version lockstep,
//! binary layout, and frozen-surface contract checks.
//!
//! soldr#2934: one linked test binary per category instead of one per source
//! file. Each module below was previously its own top-level test binary, so
//! test IDs are now `<module>::<test_name>`.

#[path = "../common/mod.rs"]
mod common;

mod alias_error_surface_guard;
mod build_session_order_lint;
mod canonical_targets_parity;
mod cli_ci_test;
mod cli_lint;
mod cli_startup_smoke;
mod color_predicate_guard;
mod daemon_console_policy_guard;
mod daemon_env_docs_guard;
mod daemon_state_db_ownership_guard;
mod env_lock_lint;
mod isolated_daemon_readiness_diagnostics;
mod linker_shared_resolver_lint;
mod msrv_doc_matches_manifest;
mod mtime_owner_workspace;
mod multicall_bin_layout;
mod no_panicking_argv_collection;
mod no_presence_only_soldr_flag_lint;
mod no_standalone_spawn_lint;
mod no_timed_test_guard;
mod phase5_contract;
mod piped_child_drain_lint;
mod process_isolation_lint;
mod replay_nextest;
mod runtime_alias_identity;
mod static_liblzma;
mod version_lockstep;
mod zccache_lifecycle_retirement_lint;
