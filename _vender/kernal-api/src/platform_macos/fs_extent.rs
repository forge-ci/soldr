//! macOS extent-sharing observation.
//!
//! APFS reports each file's unshared byte count through
//! `ATTR_CMNEXT_PRIVATESIZE` (macOS 10.15+): the bytes that are not shared
//! with a clone or snapshot. A file whose private size covers its whole
//! allocation shares nothing. Volumes that do not report the attribute fall
//! back to the file system type.

use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::io::AsRawFd as _;
use std::path::Path;

use crate::platform::fs::ExtentSharing;

/// File systems that cannot clone blocks between files.
const NON_SHARING: &[&str] = &["hfs", "msdos", "exfat"];

/// `fgetattrlist` reply for `ATTR_CMN_RETURNED_ATTRS` plus
/// `ATTR_CMNEXT_PRIVATESIZE`: the length word, the returned-attribute set,
/// then the private size as an `off_t`.
#[repr(C)]
struct PrivateSizeReply {
    length: u32,
    returned: libc::attribute_set_t,
    private_size: libc::off_t,
}

pub fn extent_sharing(path: &Path) -> io::Result<ExtentSharing> {
    let file = std::fs::File::open(path)?;
    match private_size_sharing(&file) {
        Ok(Some(sharing)) => Ok(sharing),
        Ok(None) => volume_sharing(&file),
        Err(error) if is_unsupported(&error) => volume_sharing(&file),
        Err(error) => Err(error),
    }
}

/// `None` when the volume does not report a private size.
fn private_size_sharing(file: &std::fs::File) -> io::Result<Option<ExtentSharing>> {
    let mut request = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: libc::ATTR_CMNEXT_PRIVATESIZE,
    };
    // SAFETY: `PrivateSizeReply` is plain integer data written by the call.
    let mut reply: PrivateSizeReply = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open, `request` is a valid attribute list,
    // and `reply` is live writable storage of the stated size.
    let result = unsafe {
        libc::fgetattrlist(
            file.as_raw_fd(),
            std::ptr::addr_of_mut!(request).cast(),
            std::ptr::addr_of_mut!(reply).cast(),
            std::mem::size_of::<PrivateSizeReply>(),
            libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if reply.returned.forkattr & libc::ATTR_CMNEXT_PRIVATESIZE == 0 {
        return Ok(None);
    }
    let allocated = file.metadata()?.blocks().saturating_mul(512);
    let private = u64::try_from(reply.private_size).unwrap_or(0);
    Ok(Some(classify(private, allocated)))
}

fn classify(private: u64, allocated: u64) -> ExtentSharing {
    if private < allocated {
        ExtentSharing::Shared
    } else {
        ExtentSharing::Exclusive
    }
}

fn is_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL | libc::ENOTSUP | libc::EOPNOTSUPP)
    )
}

fn volume_sharing(file: &std::fs::File) -> io::Result<ExtentSharing> {
    let name = file_system_name(file)?;
    Ok(if NON_SHARING.iter().any(|fs| name.eq_ignore_ascii_case(fs)) {
        ExtentSharing::Exclusive
    } else {
        ExtentSharing::Unknown
    })
}

fn file_system_name(file: &std::fs::File) -> io::Result<String> {
    // SAFETY: `statfs` is plain data written by the call below.
    let mut info: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open and `info` is live writable storage.
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut info) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let name: Vec<u8> = info
        .f_fstypename
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8)
        .collect();
    Ok(String::from_utf8_lossy(&name).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_size_reply_matches_the_kernel_layout() {
        assert_eq!(std::mem::offset_of!(PrivateSizeReply, returned), 4);
        assert_eq!(std::mem::offset_of!(PrivateSizeReply, private_size), 24);
        assert_eq!(std::mem::size_of::<PrivateSizeReply>(), 32);
    }

    #[test]
    fn classify_calls_any_unshared_shortfall_shared() {
        assert_eq!(classify(0, 0), ExtentSharing::Exclusive);
        assert_eq!(classify(4096, 4096), ExtentSharing::Exclusive);
        assert_eq!(classify(8192, 4096), ExtentSharing::Exclusive);
        assert_eq!(classify(0, 4096), ExtentSharing::Shared);
        assert_eq!(classify(4096, 8192), ExtentSharing::Shared);
    }

    /// The runner's temporary directory is APFS, which reports a private
    /// size, so a fresh file must be proven exclusive, not `Unknown`.
    #[test]
    fn a_fresh_file_on_apfs_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh");
        std::fs::write(&path, vec![5_u8; 256 * 1024]).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let apfs = file_system_name(&file).unwrap().eq_ignore_ascii_case("apfs");
        let measured = private_size_sharing(&file).unwrap();
        assert!(!apfs || measured.is_some(), "APFS reported no private size");
        if measured.is_none() {
            return;
        }
        assert_eq!(extent_sharing(&path).unwrap(), ExtentSharing::Exclusive);
    }

    #[test]
    fn a_cloned_pair_reports_shared_where_the_volume_clones() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, vec![3_u8; 256 * 1024]).unwrap();
        let clone = dir.path().join("clone");
        let file = std::fs::File::open(&source).unwrap();
        let apfs = file_system_name(&file).unwrap().eq_ignore_ascii_case("apfs");
        let cloned = crate::platform::fs::reflink_file(&source, &clone);
        if apfs {
            cloned.as_ref().expect("APFS clonefile");
        }
        if cloned.is_err() {
            return;
        }
        assert_eq!(extent_sharing(&source).unwrap(), ExtentSharing::Shared);
        assert_eq!(extent_sharing(&clone).unwrap(), ExtentSharing::Shared);
    }
}
