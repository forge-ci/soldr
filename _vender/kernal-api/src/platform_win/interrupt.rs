//! Native Ctrl+C subscription through the existing private signal driver.

pub(crate) struct InterruptReceiver(tokio::signal::windows::CtrlC);

impl InterruptReceiver {
    pub(crate) fn new() -> std::io::Result<Self> {
        tokio::signal::windows::ctrl_c().map(Self)
    }

    pub(crate) async fn recv(&mut self) -> Option<()> {
        self.0.recv().await
    }
}
