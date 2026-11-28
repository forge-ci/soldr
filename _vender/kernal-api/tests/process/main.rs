//! Process spawning, sessions, identity, exit observation, and signals.
//!
//! One linked test binary per category, not per file: each module below was
//! its own top-level integration test, and each one statically linked this
//! crate's whole graph. Test IDs are now `process::<module>::<test>`.
//! See AGENTS.md for the rule.

mod ape_launch;
mod bounded_child_wait;
mod control_signal;
mod dylib_probe;
mod executable_naming;
mod foreground_command;
mod interrupt_notification;
mod native_jobserver;
mod posix_arguments;
mod process_exit_observation;
mod process_host_control;
mod process_identity;
mod process_owner_death_contract;
mod process_session;
mod process_session_control;
mod process_session_streaming;
mod process_target;
mod process_usage;
mod session_spawn_cost;
mod spawn_mode_facade;
mod sync_environment;
mod termination_signal;
mod windows_app_resources;
mod windows_spawn_cleanup;
