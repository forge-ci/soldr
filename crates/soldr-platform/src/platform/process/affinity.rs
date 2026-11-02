//! CPU-affinity reset for long-lived shared services (soldr#3995).
//!
//! The broker and daemon auto-start from whichever `soldr` call first finds
//! none running, and inherit that caller's affinity mask. A caller pinned to
//! one CPU then turns into a machine-wide compile limit of one, because
//! `available_parallelism()` honours the mask. A shared service resets its
//! mask once at startup instead.

pub use crate::platform_imp::process::affinity::{widen_cpu_affinity, AffinityReset};
