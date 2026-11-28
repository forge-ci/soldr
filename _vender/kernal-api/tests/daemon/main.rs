//! Daemon identity, control, and registration protocols, and the broker
//! client adapter.
//!
//! One linked test binary per category, not per file: each module below was
//! its own top-level integration test, and each one statically linked this
//! crate's whole graph. Test IDs are now `daemon::<module>::<test>`.
//! See AGENTS.md for the rule.

mod broker_client;
mod broker_v2;
mod daemon_identity_control;
mod daemon_registration;
mod daemon_registration_v2;
