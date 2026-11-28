//! Linux kernel-assisted local-socket relay.

use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use interprocess::local_socket::tokio::prelude::*;
use tokio::io::{unix::AsyncFd, AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::platform_linux::ipc::AsyncStream;

const SPLICE_CHUNK_BYTES: usize = 64 * 1024;

struct Direction {
    source: AsyncFd<OwnedFd>,
    destination: AsyncFd<OwnedFd>,
    pipe_read: OwnedFd,
    pipe_write: OwnedFd,
    committed: Arc<AtomicBool>,
    failure_injection: FailureInjection,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum FailureInjection {
    #[default]
    None,
    BeforeFirstSourceSplice,
    AfterFirstSourceSplice,
}

impl Direction {
    fn prepare(
        source: RawFd,
        destination: RawFd,
        committed: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let source = AsyncFd::new(duplicate(source)?)?;
        let destination = AsyncFd::new(duplicate(destination)?)?;
        let (pipe_read, pipe_write) = pipe()?;
        Ok(Self {
            source,
            destination,
            pipe_read,
            pipe_write,
            committed,
            failure_injection: FailureInjection::None,
        })
    }

    async fn run(self) -> io::Result<u64> {
        let mut total = 0_u64;
        loop {
            let moved = loop {
                let mut ready = self.source.readable().await?;
                match ready.try_io(|fd| {
                    if total == 0
                        && self.failure_injection == FailureInjection::BeforeFirstSourceSplice
                    {
                        return Err(injected_splice_error());
                    }
                    splice_once(
                        fd.get_ref().as_raw_fd(),
                        self.pipe_write.as_raw_fd(),
                        SPLICE_CHUNK_BYTES,
                    )
                }) {
                    Ok(result) => break result?,
                    Err(_) => continue,
                }
            };
            if moved == 0 {
                // A successful EOF observation commits the splice path because
                // the following shutdown changes the peer-visible stream.
                self.committed.store(true, Ordering::Release);
                shutdown_write(self.destination.get_ref().as_raw_fd())?;
                return Ok(total);
            }
            // Source bytes now live in the private pipe. Buffered fallback
            // would replay them from the original socket, so it is forbidden.
            self.committed.store(true, Ordering::Release);
            if total == 0 && self.failure_injection == FailureInjection::AfterFirstSourceSplice {
                return Err(injected_splice_error());
            }

            let mut remaining = moved;
            while remaining != 0 {
                let written = loop {
                    let mut ready = self.destination.writable().await?;
                    match ready.try_io(|fd| {
                        splice_once(
                            self.pipe_read.as_raw_fd(),
                            fd.get_ref().as_raw_fd(),
                            remaining,
                        )
                    }) {
                        Ok(result) => break result?,
                        Err(_) => continue,
                    }
                };
                if written == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "SESSION splice destination made no progress",
                    ));
                }
                remaining -= written;
                total += written as u64;
            }
        }
    }
}

fn injected_splice_error() -> io::Error {
    io::Error::other("injected SESSION splice failure")
}

fn duplicate(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: fd is borrowed from a live socket half. fcntl either returns
    // a new independently owned descriptor or a negative error sentinel.
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicated < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a nonnegative F_DUPFD_CLOEXEC result is a fresh descriptor
        // whose ownership is transferred exactly once to OwnedFd.
        Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
    }
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    // SAFETY: fds points to writable space for both descriptors pipe2
    // initializes on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pipe2 returned two distinct owned descriptors,
    // each transferred exactly once into OwnedFd.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn splice_once(from: RawFd, to: RawFd, count: usize) -> io::Result<usize> {
    loop {
        // SAFETY: from/to are live descriptors, null offsets are required
        // for sockets/pipes, and the kernel owns both byte ranges.
        let result = unsafe {
            libc::splice(
                from,
                std::ptr::null_mut(),
                to,
                std::ptr::null_mut(),
                count,
                libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK,
            )
        };
        if result >= 0 {
            return Ok(result as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn shutdown_write(fd: RawFd) -> io::Result<()> {
    // SAFETY: fd is a live duplicated socket descriptor; shutdown borrows
    // it and SHUT_WR is a valid direction constant.
    if unsafe { libc::shutdown(fd, libc::SHUT_WR) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(
        error.kind(),
        io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
    ) {
        Ok(())
    } else {
        Err(error)
    }
}

async fn copy_one_way<R, W>(mut reader: R, mut writer: W) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let copied = tokio::io::copy(&mut reader, &mut writer).await?;
    writer.shutdown().await?;
    Ok(copied)
}

/// Relay a pair of established local sockets through Linux `splice(2)`.
///
/// Both directions prepare all duplicated descriptors and pipes before either
/// moves a byte. Preparation failure and a first splice error therefore retain
/// a safe buffered fallback; errors after bytes move or EOF shutdown begins are
/// returned without replaying data.
pub async fn relay_local_socket_session(
    client: AsyncStream,
    daemon: AsyncStream,
) -> io::Result<()> {
    let client = client.0;
    let daemon = daemon.0;
    let (client_read, client_write) = client.split();
    let (daemon_read, daemon_write) = daemon.split();

    // Linux's dispatch enums each have exactly one Unix-domain-socket variant.
    let interprocess::local_socket::tokio::RecvHalf::UdSocket(client_read) = client_read;
    let interprocess::local_socket::tokio::SendHalf::UdSocket(client_write) = client_write;
    let interprocess::local_socket::tokio::RecvHalf::UdSocket(daemon_read) = daemon_read;
    let interprocess::local_socket::tokio::SendHalf::UdSocket(daemon_write) = daemon_write;

    let committed = Arc::new(AtomicBool::new(false));
    let client_to_daemon = Direction::prepare(
        client_read.as_fd().as_raw_fd(),
        daemon_write.as_fd().as_raw_fd(),
        Arc::clone(&committed),
    );
    let daemon_to_client = Direction::prepare(
        daemon_read.as_fd().as_raw_fd(),
        client_write.as_fd().as_raw_fd(),
        Arc::clone(&committed),
    );
    let (client_to_daemon, daemon_to_client) = match (client_to_daemon, daemon_to_client) {
        (Ok(client_to_daemon), Ok(daemon_to_client)) => (client_to_daemon, daemon_to_client),
        (client_to_daemon, daemon_to_client) => {
            // Release any successfully prepared duplicates and pipe ends before
            // awaiting the fallback, especially when setup failed under FD
            // pressure.
            drop(client_to_daemon);
            drop(daemon_to_client);
            tokio::try_join!(
                copy_one_way(client_read, daemon_write),
                copy_one_way(daemon_read, client_write)
            )?;
            return Ok(());
        }
    };

    // Keep the interprocess halves alive while the duplicates relay. Their
    // Drop implementations own the stream's shutdown lifecycle.
    let original_halves = (client_read, client_write, daemon_read, daemon_write);
    let result = tokio::try_join!(client_to_daemon.run(), daemon_to_client.run());
    match result {
        Ok(_) => {
            drop(original_halves);
            Ok(())
        }
        Err(_error) if !committed.load(Ordering::Acquire) => {
            let (client_read, client_write, daemon_read, daemon_write) = original_halves;
            tokio::try_join!(
                copy_one_way(client_read, daemon_write),
                copy_one_way(daemon_read, client_write)
            )?;
            Ok(())
        }
        Err(error) => {
            drop(original_halves);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use super::{Direction, FailureInjection};

    fn direction_fixture() -> (UnixStream, UnixStream, Direction, Arc<AtomicBool>) {
        let (source_peer, source) = UnixStream::pair().expect("source pair");
        let (destination, _destination_peer) = UnixStream::pair().expect("destination pair");
        source.set_nonblocking(true).expect("nonblocking source");
        destination
            .set_nonblocking(true)
            .expect("nonblocking destination");
        let committed = Arc::new(AtomicBool::new(false));
        let direction = Direction::prepare(
            source.as_raw_fd(),
            destination.as_raw_fd(),
            Arc::clone(&committed),
        )
        .expect("prepare direction");
        (source_peer, source, direction, committed)
    }

    #[tokio::test]
    async fn injected_first_splice_failure_keeps_buffered_fallback_safe() {
        let (mut source_peer, mut source, mut direction, committed) = direction_fixture();
        source_peer.write_all(b"still-buffered").expect("write source");
        direction.failure_injection = FailureInjection::BeforeFirstSourceSplice;

        direction.run().await.expect_err("injected failure");

        assert!(!committed.load(Ordering::Acquire));
        let mut bytes = [0_u8; 14];
        source.read_exact(&mut bytes).expect("source remains unread");
        assert_eq!(&bytes, b"still-buffered");
    }

    #[tokio::test]
    async fn injected_post_progress_failure_forbids_buffered_replay() {
        let (mut source_peer, _source, mut direction, committed) = direction_fixture();
        source_peer.write_all(b"consumed").expect("write source");
        direction.failure_injection = FailureInjection::AfterFirstSourceSplice;

        direction.run().await.expect_err("injected failure");

        assert!(committed.load(Ordering::Acquire));
    }
}
