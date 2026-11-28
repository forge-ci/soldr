use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// Fixed-size, shared server diagnostics. No request contents, addresses, error
/// strings or unbounded event queue are retained. Counters saturate at u64::MAX.
#[derive(Clone, Debug, Default)]
pub struct Diagnostics(pub(super) Arc<Counters>);

#[derive(Debug, Default)]
pub(super) struct Counters {
    pub accepted_connections: AtomicU64,
    pub completed_connections: AtomicU64,
    pub connection_errors: AtomicU64,
    pub connection_timeouts: AtomicU64,
    pub task_failures: AtomicU64,
    pub request_rejections: AtomicU64,
    pub body_timeouts: AtomicU64,
    pub handler_timeouts: AtomicU64,
    pub response_rejections: AtomicU64,
}

/// A non-transactional snapshot: concurrent events may appear in different
/// fields at slightly different instants. Request and connection counts overlap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Connections accepted by the server.
    pub accepted_connections: u64,
    /// Connections ending normally (including HTTP error-status responses).
    pub completed_connections: u64,
    /// Protocol, socket, header-deadline, write-progress or streamed-body errors.
    pub connection_errors: u64,
    /// Connections exceeding their absolute lifetime.
    pub connection_timeouts: u64,
    /// Failed owned tasks, including handler panics, observed by the accept loop.
    /// Server shutdown cancellation is not counted as a task failure.
    pub task_failures: u64,
    /// Invalid or oversized request bodies, excluding body timeouts.
    pub request_rejections: u64,
    /// Request body collection deadlines.
    pub body_timeouts: u64,
    /// Application response preparation deadlines.
    pub handler_timeouts: u64,
    /// Application response header or body acceptance failures.
    pub response_rejections: u64,
}

impl Diagnostics {
    /// Read counters. The handle remains usable after the server is dropped.
    pub fn snapshot(&self) -> Snapshot {
        let c = &self.0;
        Snapshot {
            accepted_connections: c.accepted_connections.load(Ordering::Relaxed),
            completed_connections: c.completed_connections.load(Ordering::Relaxed),
            connection_errors: c.connection_errors.load(Ordering::Relaxed),
            connection_timeouts: c.connection_timeouts.load(Ordering::Relaxed),
            task_failures: c.task_failures.load(Ordering::Relaxed),
            request_rejections: c.request_rejections.load(Ordering::Relaxed),
            body_timeouts: c.body_timeouts.load(Ordering::Relaxed),
            handler_timeouts: c.handler_timeouts.load(Ordering::Relaxed),
            response_rejections: c.response_rejections.load(Ordering::Relaxed),
        }
    }
}

pub(super) fn increment(counter: &AtomicU64) {
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        Some(n.saturating_add(1))
    });
}
