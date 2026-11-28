//! Filesystem capabilities: paths, directories, watching, and hashing.
//!
//! One linked test binary per category, not per file: each module below was
//! its own top-level integration test, and each one statically linked this
//! crate's whole graph. Test IDs are now `fs::<module>::<test>`.
//! See AGENTS.md for the rule.

// Aliased: the category also has a `readiness_marker` test module.
mod blake3_facade;
mod bounded_file_lock;
mod context_file_observation;
mod directory_cursor;
mod directory_walk_options;
mod directory_walk_parallelism;
mod fs_scan;
mod fs_watch_facade;
#[path = "../support/readiness_marker.rs"]
mod marker_support;
mod materialization;
mod open_acl;
mod private_regular_file_facade;
mod public_file_handle;
mod readiness_marker;
mod sha256_facade;
mod temporary_directory;
mod tree_hash;
mod user_home;
mod write_seal_facade;
