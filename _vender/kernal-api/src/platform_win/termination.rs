//! Native graceful-termination subscription: console break, close, and
//! shutdown events through the existing private signal driver.

use std::task::{Context, Poll};

pub(crate) struct TerminationReceiver {
    break_event: tokio::signal::windows::CtrlBreak,
    close: tokio::signal::windows::CtrlClose,
    shutdown: tokio::signal::windows::CtrlShutdown,
}

impl TerminationReceiver {
    pub(crate) fn new() -> std::io::Result<Self> {
        Ok(Self {
            break_event: tokio::signal::windows::ctrl_break()?,
            close: tokio::signal::windows::ctrl_close()?,
            shutdown: tokio::signal::windows::ctrl_shutdown()?,
        })
    }

    /// Ready when any event fires; closed only once every source is closed.
    pub(crate) fn poll_recv(&mut self, context: &mut Context<'_>) -> Poll<Option<()>> {
        let polls = [
            self.break_event.poll_recv(context),
            self.close.poll_recv(context),
            self.shutdown.poll_recv(context),
        ];
        if polls.contains(&Poll::Ready(Some(()))) {
            Poll::Ready(Some(()))
        } else if polls.iter().all(|poll| *poll == Poll::Ready(None)) {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}
