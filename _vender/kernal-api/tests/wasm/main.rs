//! Wasm sketch-host admission, limits, and threaded guest execution.
//!
//! One linked test binary per category, not per file: each module below was
//! its own top-level integration test, and each one statically linked this
//! crate's whole graph. Test IDs are now `wasm::<module>::<test>`.
//! See AGENTS.md for the rule.

mod threaded_artifact_profile;
#[path = "../support/threaded_fixture.rs"]
mod threaded_fixture;
mod threaded_root_execution;
mod wasm_admission;
mod wasm_epoch_cancellation;
mod wasm_execution_limits;
mod wasm_feature_isolation;
mod wasm_fuel_limits;
mod wasm_guest_facade;
