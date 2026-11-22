//! Broker-to-daemon connection handoff: receiving a SESSION stream the
//! broker duplicated over its transport (SCM_RIGHTS on Unix,
//! DuplicateHandle on Windows).
//!
//! The daemon keeps the framed offer/ack handshake (prost protocol
//! types); these primitives own the descriptor/handle mechanics. The
//! received descriptor is held in [`ReceivedFd`] between the receive
//! and the token verification so the daemon's neutral orchestration
//! never names an OS-specific fd type.

pub use crate::platform_imp::ipc::handoff::{
    close_received_fd, named_pipe_stream_from_handle_value, receive_unix_descriptor,
    send_test_handoff_descriptor, session_stream_from_received_fd,
};

/// A file descriptor received from the broker, held opaque so the
/// holder (the daemon handoff orchestration) can pass it through
/// without naming `OwnedFd`. Constructed and consumed only inside the
/// platform crate.
#[allow(dead_code)] // Unix-only at runtime: the Windows handoff tree never wraps a descriptor.
pub struct ReceivedFd(pub(crate) u64);

#[allow(dead_code)] // See the struct-level note.
impl ReceivedFd {
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) fn raw(&self) -> u64 {
        self.0
    }
}


#[cfg(all(test, unix))]
mod tests {
    use super::{session_stream_from_received_fd, ReceivedFd};
    use std::os::fd::IntoRawFd as _;
    use std::os::unix::net::UnixStream;

    #[test]
    fn adopts_a_blocking_session_socket_into_tokio() {
        let (socket, _peer) = UnixStream::pair().expect("create connected blocking sockets");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("create test reactor");
        let _entered = runtime.enter();
        let received = ReceivedFd::from_raw(socket.into_raw_fd() as u64);
        let _stream = session_stream_from_received_fd(received).expect("adopt broker socket");
    }
}
