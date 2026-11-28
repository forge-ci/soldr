//! Parsing, schema, storage, and randomness capabilities.
//!
//! One linked test binary per category, not per file: each module below was
//! its own top-level integration test, and each one statically linked this
//! crate's whole graph. Test IDs are now `data::<module>::<test>`.
//! See AGENTS.md for the rule.

mod command_schema_contract;
mod config_toml;
mod json_documents;
mod secure_random;
mod source_cpp;
mod sqlite_facade;
mod text_similarity;
