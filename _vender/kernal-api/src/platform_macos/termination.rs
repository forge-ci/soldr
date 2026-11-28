//! Native graceful-termination (SIGTERM) subscription through the existing
//! private signal driver.

use std::task::{Context, Poll};

pub(crate) struct TerminationReceiver(tokio::signal::unix::Signal);

impl TerminationReceiver {
    pub(crate) fn new() -> std::io::Result<Self> {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map(Self)
    }

    pub(crate) fn poll_recv(&mut self, context: &mut Context<'_>) -> Poll<Option<()>> {
        self.0.poll_recv(context)
    }
}
