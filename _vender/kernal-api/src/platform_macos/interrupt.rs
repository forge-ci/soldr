//! Native Ctrl+C subscription through the existing private signal driver.

pub(crate) struct InterruptReceiver(tokio::signal::unix::Signal);

impl InterruptReceiver {
    pub(crate) fn new() -> std::io::Result<Self> {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).map(Self)
    }

    pub(crate) async fn recv(&mut self) -> Option<()> {
        self.0.recv().await
    }
}
