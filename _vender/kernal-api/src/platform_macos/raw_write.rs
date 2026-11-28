//! Writing a whole buffer to a caller-owned descriptor (POSIX).

use std::io;
use std::os::fd::RawFd;

use crate::platform::fs::RawDescriptor;

impl From<RawFd> for RawDescriptor {
    fn from(fd: RawFd) -> Self {
        RawDescriptor::from_value(fd as usize)
    }
}

/// Write every byte of `bytes`, or report why not.
///
/// `write(2)` is allowed to write fewer bytes than asked and to fail with
/// `EINTR` when a signal arrives mid-call. Neither is an error, and neither
/// is optional to handle: a caller that treats a short write as a whole one
/// silently truncates, which for a log tee means losing the middle of a line
/// rather than noticing anything.
pub fn write_all_to_descriptor(descriptor: RawDescriptor, mut bytes: &[u8]) -> io::Result<()> {
    let fd = descriptor.value() as RawFd;
    while !bytes.is_empty() {
        // SAFETY: `fd` is a descriptor the caller owns and has asked us to
        // write to; the pointer and length describe the slice above.
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "raw descriptor write returned zero",
            ));
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;

    /// A whole buffer arrives, including one larger than a single write.
    #[test]
    fn every_byte_is_written() {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        let payload = vec![b'x'; 300_000];

        write_all_to_descriptor(RawDescriptor::from(file.as_file().as_raw_fd()), &payload)
            .expect("write");

        let mut readback = Vec::new();
        std::fs::File::open(file.path())
            .expect("reopen")
            .read_to_end(&mut readback)
            .expect("read");
        assert_eq!(readback.len(), payload.len());
        assert!(readback.iter().all(|b| *b == b'x'));
    }

    /// Writing nothing succeeds without touching the descriptor.
    #[test]
    fn an_empty_write_is_not_an_error() {
        write_all_to_descriptor(RawDescriptor::from(-1), &[]).expect("empty write");
    }

    /// A descriptor that is not open reports the host's error.
    #[test]
    fn a_closed_descriptor_reports_the_host_error() {
        let error = write_all_to_descriptor(RawDescriptor::from(-1), b"x").expect_err("bad fd");
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }
}
