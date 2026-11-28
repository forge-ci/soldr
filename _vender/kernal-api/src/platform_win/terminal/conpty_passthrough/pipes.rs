//! Anonymous-pipe pair plumbing for ConPTY passthrough (#150 W2).
//!
//! Each ConPTY needs two anonymous pipes:
//! * **input pipe** — host writes the child's stdin; child reads it
//! * **output pipe** — child writes stdout/stderr; host reads it
//!
//! For the ConPTY-side pipe-ends to be inherited by the spawned child,
//! `CreatePipe` is called with `SECURITY_ATTRIBUTES.bInheritHandle =
//! TRUE`. Then `SetHandleInformation(host_side, HANDLE_FLAG_INHERIT,
//! 0)` strips the inheritance bit from the host-side handle so only
//! the ConPTY side leaks into the child — without this, the host-side
//! handle would also be duplicated into the child, blowing up the
//! pipe-close EOF semantics (the read side never sees EOF until *all*
//! write handles, child's and host's, close).
//!
//! 64 KiB buffer size up from the OS default (~4 KiB on Win10/11) so
//! a chatty child can't deadlock on a full pipe before the host's
//! reader thread drains it.

#![cfg(windows)]

use std::io;
use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Pipes::CreatePipe;

/// Anonymous pipe pair. The `child` side is passed to
/// `CreatePseudoConsole`; the `host` side stays in our process.
pub(super) struct PipePair {
    pub(super) host: OwnedHandle,
    pub(super) child: OwnedHandle,
}

#[derive(Copy, Clone)]
pub(super) enum PipeDirection {
    /// Child stdin: child reads, host writes.
    HostWriteChildRead,
    /// Child stdout/stderr: child writes, host reads.
    HostReadChildWrite,
}

pub(super) fn create_pipe(direction: PipeDirection) -> io::Result<PipePair> {
    // Match portable-pty / filedescriptor exactly: non-inheritable
    // handles, default OS buffer size. With `bInheritHandles = FALSE`
    // on CreateProcessW (which is what we use), inheritance doesn't
    // come into play anyway — ConPTY duplicates the handles
    // internally for its own use.
    let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
    sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
    sa.bInheritHandle = 0;
    sa.lpSecurityDescriptor = ptr::null_mut();

    let mut read_handle: HANDLE = INVALID_HANDLE_VALUE;
    let mut write_handle: HANDLE = INVALID_HANDLE_VALUE;
    let ok = unsafe { CreatePipe(&mut read_handle, &mut write_handle, &sa, 0) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: handles returned by CreatePipe are owned and unique.
    let read_owned = unsafe { OwnedHandle::from_raw_handle(read_handle as RawHandle) };
    let write_owned = unsafe { OwnedHandle::from_raw_handle(write_handle as RawHandle) };

    let (host_owned, child_owned) = match direction {
        PipeDirection::HostWriteChildRead => (write_owned, read_owned),
        PipeDirection::HostReadChildWrite => (read_owned, write_owned),
    };

    // No SetHandleInformation needed — handles are non-inheritable
    // by construction, and CreateProcessW's bInheritHandles=FALSE
    // makes inheritance moot anyway.

    Ok(PipePair {
        host: host_owned,
        child: child_owned,
    })
}
