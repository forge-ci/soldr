//! Cache surface, garbage-collection/eviction, worktree cache sharing, and
//! Windows delete-semantics integration tests.
//!
//! soldr#2934: one linked test binary per category instead of one per source
//! file. Each module below was previously its own top-level test binary, so
//! test IDs are now `<module>::<test_name>`.

#[path = "../common/mod.rs"]
mod common;

mod agent_worktree_share;
mod cli_cache;
mod cli_cache_prune;
mod cli_cache_trim;
mod cli_gc;
mod cli_gc_auto_sweep;
mod cli_gc_extras;
mod cli_gc_git_checkouts;
mod cli_gc_registry_src;
mod cli_gc_report_only;
mod cli_gc_rustup_purge;
mod cli_gc_target;
mod cli_gc_target_subtrees;
mod cook_auto_gc;
mod out_dir_cross_worktree;
mod windows_delete_semantics;
