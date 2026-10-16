//! Host-neutral broker listener and accepted-stream mechanics.

/// Bind the broker listener using owner-only host security and serialized stale-endpoint recovery.
pub use crate::platform_imp::ipc::broker::bind_listener;
/// Duplicate an accepted broker stream for daemon handoff without leaking it to child processes.
pub use crate::platform_imp::ipc::broker::duplicate_stream;
/// Identify the broker endpoint file (device, inode) so ownership can be re-checked.
pub use crate::platform_imp::ipc::broker::endpoint_identity;
/// Retire the broker endpoint after listener shutdown when the host uses a filesystem socket.
pub use crate::platform_imp::ipc::broker::retire_endpoint;
/// Retire the broker endpoint only while it is still the identified file (soldr#3815).
pub use crate::platform_imp::ipc::broker::retire_endpoint_if_identity;
/// Create and close a filesystem listener so tests can exercise stale-endpoint recovery.
pub use crate::platform_imp::ipc::broker::seed_stale_endpoint;
