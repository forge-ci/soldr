//! Portable local-socket relay for macOS.

use crate::platform_macos::ipc::AsyncStream;

/// Relay a pair of established local sockets through Tokio's bounded buffers.
pub async fn relay_local_socket_session(
    mut client: AsyncStream,
    mut daemon: AsyncStream,
) -> std::io::Result<()> {
    tokio::io::copy_bidirectional(&mut client, &mut daemon).await?;
    Ok(())
}
