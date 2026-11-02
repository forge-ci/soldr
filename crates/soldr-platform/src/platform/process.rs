//! Host-neutral process facade.
//!
//! Owns command configuration and console suppression, detached/background
//! spawn and process-group setup, process-tree termination and escalation,
//! PID liveness/zombie state and running-image lookup, exit/signal
//! interpretation, and Unix exec replacement versus Windows spawn-and-wait.
//! Callers retain timeout/retry policy, which program to launch, lifecycle
//! state, and diagnostic wording.

pub mod affinity;
pub mod command;
pub mod cpu_ticks;
pub mod exit;
pub mod inspect;
pub mod signal;
pub mod spawn;
pub mod spawn_exclusion;
pub mod terminate;
pub mod test_child;
