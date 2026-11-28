//! Writing a whole buffer to a caller-owned descriptor (Windows).

use std::io;
use std::os::windows::io::RawHandle;
use std::ptr;

use winapi::shared::minwindef::DWORD;
use winapi::um::fileapi::WriteFile;
use winapi::um::winnt::HANDLE;

use crate::platform::fs::RawDescriptor;

impl From<RawHandle> for RawDescriptor {
    fn from(handle: RawHandle) -> Self {
        RawDescriptor::from_value(handle as usize)
    }
}

/// Write every byte of `bytes`, or report why not.
///
/// `WriteFile` is allowed to write fewer bytes than asked, so this loops for
/// the same reason the POSIX side does. It differs in one detail worth
/// stating: the count is a `DWORD`, so a buffer larger than `u32::MAX` is
/// written in several calls rather than rejected.
pub fn write_all_to_descriptor(descriptor: RawDescriptor, mut bytes: &[u8]) -> io::Result<()> {
    let handle = descriptor.value() as HANDLE;
    while !bytes.is_empty() {
        let len = bytes.len().min(DWORD::MAX as usize) as DWORD;
        let mut written: DWORD = 0;
        // SAFETY: `handle` is a handle the caller owns and has asked us to
        // write to; the pointer and length describe the slice above, and
        // `written` is a valid out-parameter.
        let ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr().cast(),
                len,
                &mut written,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
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
    use std::os::windows::io::AsRawHandle as _;

    /// A whole buffer arrives, including one larger than a single write.
    #[test]
    fn every_byte_is_written() {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        let payload = vec![b'x'; 300_000];

        write_all_to_descriptor(
            RawDescriptor::from(file.as_file().as_raw_handle()),
            &payload,
        )
        .expect("write");

        let mut readback = Vec::new();
        std::fs::File::open(file.path())
            .expect("reopen")
            .read_to_end(&mut readback)
            .expect("read");
        assert_eq!(readback.len(), payload.len());
        assert!(readback.iter().all(|b| *b == b'x'));
    }

    /// Writing nothing succeeds without touching the handle.
    #[test]
    fn an_empty_write_is_not_an_error() {
        write_all_to_descriptor(RawDescriptor::from(ptr::null_mut()), &[]).expect("empty write");
    }

    /// A handle that is not open reports the host's error.
    #[test]
    fn a_closed_descriptor_reports_the_host_error() {
        let error = write_all_to_descriptor(RawDescriptor::from(ptr::null_mut()), b"x")
            .expect_err("bad handle");
        assert!(error.raw_os_error().is_some(), "the host must say why");
    }
}
