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
