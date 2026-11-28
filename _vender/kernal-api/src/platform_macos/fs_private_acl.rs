//! macOS extended ACL checks on an already-open artifact.
//!
//! This check does not establish ownership, mode, ancestry, or identity.

use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;

// Darwin's public filesec_property_t value from <sys/fcntl.h>.
const FILESEC_ACL: c_int = 5;

unsafe extern "C" {
    fn filesec_init() -> *mut c_void;
    fn filesec_free(filesec: *mut c_void);
    fn filesec_query_property(filesec: *mut c_void, property: c_int, valid: *mut c_int) -> c_int;
    // x86_64 Darwin headers select the modern inode layout with this alias;
    // arm64 uses the unsuffixed symbol. See <sys/stat.h> __DARWIN_INODE64.
    #[cfg_attr(target_arch = "x86_64", link_name = "fstatx_np$INODE64")]
    fn fstatx_np(fd: c_int, stat: *mut libc::stat, filesec: *mut c_void) -> c_int;
}

struct FileSecurity(*mut c_void);

impl Drop for FileSecurity {
    fn drop(&mut self) {
        // SAFETY: the pointer came from filesec_init and is owned by this wrapper.
        unsafe { filesec_free(self.0) };
    }
}

/// Reject every present ACL and every failed security metadata query.
pub fn ensure_no_extended_acl(file: &File) -> io::Result<()> {
    // SAFETY: filesec_init takes no arguments and returns an owned object or null.
    let security = unsafe { filesec_init() };
    if security.is_null() {
        return Err(io::Error::last_os_error());
    }
    let security = FileSecurity(security);
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the descriptor and filesec are live; stat points to valid writable storage.
    if unsafe { fstatx_np(file.as_raw_fd(), stat.as_mut_ptr(), security.0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut valid = 0;
    // SAFETY: filesec is live and valid points to writable c_int storage.
    if unsafe { filesec_query_property(security.0, FILESEC_ACL, &mut valid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Apple's filesec implementation exposes the populated ACL-valid flag;
    // copyfile treats the corresponding missing property after fstatx as no ACL.
    // Reject any present ACL rather than guessing which principals are safe.
    if valid != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "artifact has an extended ACL",
        ));
    }
    Ok(())
}
