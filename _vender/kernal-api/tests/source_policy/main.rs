//! Tests that read this crate's own source text.
//!
//! One linked test binary per category, not per file: each module below was
//! its own top-level integration test, and each one statically linked this
//! crate's whole graph. Test IDs are now `source_policy::<module>::<test>`.
//! See AGENTS.md for the rule.

mod daemon_frame_v1;
mod daemon_identity;
mod facade_policy;
mod glibc_floor;
mod version_policy;
