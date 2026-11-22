//! Linux handoff: receive the SESSION socket the broker passed over
//! SCM_RIGHTS.

use std::io;

use crate::platform::ipc::handoff::ReceivedFd;

/// Receive the SESSION descriptor from the broker's control
/// connection: a token prelude plus an `SCM_RIGHTS` fd, both read
/// together from the socket. The descriptor is returned with
/// `FD_CLOEXEC` set, and the token bytes for the daemon to compare
/// against the framed handoff offer.
pub fn receive_unix_descriptor(
    control: interprocess::local_socket::tokio::Stream,
) -> io::Result<(
    interprocess::local_socket::tokio::Stream,
    ReceivedFd,
    [u8; running_process::broker::server::HANDOFF_TOKEN_BYTES],
)> {
    use std::os::fd::{AsFd as _, AsRawFd as _};

    let interprocess::local_socket::tokio::Stream::UdSocket(socket) = &control;
    let socket_fd = socket.as_fd().as_raw_fd();
    let mut token = [0_u8; running_process::broker::server::HANDOFF_TOKEN_BYTES];
    let mut iov = libc::iovec {
        iov_base: token.as_mut_ptr().cast(),
        iov_len: token.len(),
    };
    let mut ancillary =
        vec![0_u8; unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) as usize }];
    let deadline =
        std::time::Instant::now() + running_process::broker::server::DEFAULT_HANDOFF_ACK_DEADLINE;
    loop {
        let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = ancillary.as_mut_ptr().cast();
        message.msg_controllen = ancillary.len() as _;
        let received = unsafe { libc::recvmsg(socket_fd, &mut message, libc::MSG_DONTWAIT) };
        if received < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            return Err(error);
        }
        if received as usize != token.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short SCM_RIGHTS token prelude",
            ));
        }
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        if header.is_null()
            || unsafe {
                (*header).cmsg_level != libc::SOL_SOCKET || (*header).cmsg_type != libc::SCM_RIGHTS
            }
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff prelude omitted SCM_RIGHTS",
            ));
        }
        let received_fd = unsafe { *libc::CMSG_DATA(header).cast::<libc::c_int>() };
        let descriptor_flags = unsafe { libc::fcntl(received_fd, libc::F_GETFD) };
        if descriptor_flags < 0
            || unsafe {
                libc::fcntl(
                    received_fd,
                    libc::F_SETFD,
                    descriptor_flags | libc::FD_CLOEXEC,
                )
            } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // The descriptor stays process-owned from `recvmsg` until the
        // caller converts it into a stream (which takes ownership and
        // closes it on drop) or closes it via `close_received_fd`.
        // Wrapping it in a temporary `OwnedFd` here would close it on
        // scope exit and hand the caller a dead number.
        return Ok((control, ReceivedFd::from_raw(received_fd as u64), token));
    }
}

/// Wrap the received descriptor into a tokio SESSION stream. Called
/// only after the daemon verified the handoff token against the framed
/// offer.
pub fn session_stream_from_received_fd(
    fd: ReceivedFd,
) -> io::Result<interprocess::local_socket::tokio::Stream> {
    use std::os::fd::FromRawFd as _;
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd.raw() as i32) };
    // The broker can hand off a blocking descriptor. Tokio requires
    // nonblocking mode before registering that descriptor with its reactor.
    let socket = std::os::unix::net::UnixStream::from(owned);
    socket.set_nonblocking(true)?;
    let owned = std::os::fd::OwnedFd::from(socket);
    let stream = interprocess::os::unix::uds_local_socket::tokio::Stream::try_from(owned)?;
    Ok(stream.into())
}

/// Unsupported on Linux: the broker hands over a descriptor, never a
/// duplicated handle. Unreachable at runtime.
pub fn named_pipe_stream_from_handle_value(
    _value: u64,
) -> io::Result<interprocess::local_socket::tokio::Stream> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "DuplicateHandle handoff does not exist on Linux",
    ))
}

/// Close a received descriptor that will never be converted into a
/// stream (the handoff failed after receipt). Restores the original
/// RAII semantics: the daemon closes the descriptor on every error
/// path instead of leaking it.
pub fn close_received_fd(fd: ReceivedFd) {
    unsafe { libc::close(fd.raw() as libc::c_int) };
}

/// Test support: play the broker for one `SCM_RIGHTS` handoff.
///
/// Connects to the daemon's handoff endpoint, creates a connected socket
/// pair standing in for the client SESSION connection, sends one end with
/// the 16-byte token prelude exactly the way the broker's
/// `try_send_scm_rights_over` does, and returns the control connection (for
/// the framed offer/ack exchange) with the client's end of the pair. Lives
/// inside the platform boundary so the daemon's handoff regression tests
/// (soldr#3102) can drive the real transport without naming descriptor
/// types outside it.
pub fn send_test_handoff_descriptor(
    handoff_endpoint: &str,
    token: &[u8; running_process::broker::server::HANDOFF_TOKEN_BYTES],
) -> io::Result<(
    interprocess::local_socket::Stream,
    interprocess::local_socket::Stream,
)> {
    use interprocess::local_socket::traits::Stream as _;
    use running_process::broker::server::handoff::{
        try_send_scm_rights_over, HandoffToken, ScmRightsAttempt, UnixFileDescriptor,
        UnixHandoffSocket,
    };
    use std::os::fd::AsRawFd as _;

    let name = running_process::broker::server::singleton_bind::wrap_socket_name(handoff_endpoint)
        .map_err(io::Error::other)?;
    let control = interprocess::os::unix::uds_local_socket::Stream::connect(name)?;
    let (daemon_side, client_side) = std::os::unix::net::UnixStream::pair()?;
    let attempt = ScmRightsAttempt::new(
        UnixFileDescriptor::new(daemon_side.as_raw_fd()),
        UnixHandoffSocket::new(handoff_endpoint),
        HandoffToken::from_bytes(*token),
    );
    try_send_scm_rights_over(control.inner().as_raw_fd(), &attempt)
        .map_err(|error| io::Error::other(error.to_string()))?;
    // The daemon now holds its own duplicate; the broker-side copy is done.
    drop(daemon_side);
    Ok((
        control.into(),
        interprocess::os::unix::uds_local_socket::Stream::from(client_side).into(),
    ))
}
