//! Host-neutral filesystem facade.
//!
//! Owns stable file identity and same-file comparison, link/reparse
//! classification, symlink creation/removal, owner-only/writable/executable
//! permissions, atomic replacement and open-running-image retirement,
//! volume identity and free-space probes, cache-to-target delivery
//! capability probing, positional I/O, and the normalization of OS-specific
//! lock/contention errors. Callers retain archive traversal, cache/hash
//! policy, retry policy, and authorization to delete or replace data.

pub mod contention;
pub mod delivery_probe;
pub mod identity;
pub mod links;
pub mod permissions;
pub mod positioned_io;
pub mod replace;
pub mod volume;

/// Whether any process still holds a file open for writing (soldr#3350).
pub mod writers {
    pub use kernal_api::platform::fs::{await_no_writers, WriterWait};
}

/// Copy-on-write clone of one file (soldr#4033): fails rather than copying
/// when the filesystem pair cannot share extents.
pub mod reflink {
    pub use kernal_api::platform::fs::reflink_file;
}
