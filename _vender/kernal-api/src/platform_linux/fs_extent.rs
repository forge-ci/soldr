//! Linux extent-sharing observation through `FS_IOC_FIEMAP`.

use std::io;
use std::os::unix::io::AsRawFd as _;
use std::path::Path;

use crate::platform::fs::ExtentSharing;

/// `_IOWR('f', 11, struct fiemap)`.
const FS_IOC_FIEMAP: u32 = 0xC020_660B;
const FIEMAP_FLAG_SYNC: u32 = 0x1;
const FIEMAP_EXTENT_LAST: u32 = 0x1;
const FIEMAP_EXTENT_SHARED: u32 = 0x2000;
const EXTENTS_PER_CALL: usize = 32;

/// File systems that can never share blocks between files.
const EXT_SUPER_MAGIC: u32 = 0xEF53;
const TMPFS_MAGIC: u32 = 0x0102_1994;
const RAMFS_MAGIC: u32 = 0x8584_58F6;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FiemapExtent {
    fe_logical: u64,
    fe_physical: u64,
    fe_length: u64,
    fe_reserved64: [u64; 2],
    fe_flags: u32,
    fe_reserved: [u32; 3],
}

#[repr(C)]
struct FiemapRequest {
    fm_start: u64,
    fm_length: u64,
    fm_flags: u32,
    fm_mapped_extents: u32,
    fm_extent_count: u32,
    fm_reserved: u32,
    fm_extents: [FiemapExtent; EXTENTS_PER_CALL],
}

pub fn extent_sharing(path: &Path) -> io::Result<ExtentSharing> {
    let file = std::fs::File::open(path)?;
    match fiemap_sharing(&file) {
        Ok(sharing) => Ok(sharing),
        Err(error) if is_unsupported(&error) => volume_sharing(&file),
        Err(error) => Err(error),
    }
}

fn fiemap_sharing(file: &std::fs::File) -> io::Result<ExtentSharing> {
    let mut start = 0_u64;
    loop {
        let mut request = FiemapRequest {
            fm_start: start,
            fm_length: u64::MAX - start,
            fm_flags: FIEMAP_FLAG_SYNC,
            fm_mapped_extents: 0,
            fm_extent_count: EXTENTS_PER_CALL as u32,
            fm_reserved: 0,
            fm_extents: [FiemapExtent::default(); EXTENTS_PER_CALL],
        };
        // SAFETY: `request` is a live, correctly sized `struct fiemap` with
        // room for `fm_extent_count` extents, and the descriptor is open.
        let result = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                FS_IOC_FIEMAP as _,
                std::ptr::addr_of_mut!(request),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        let mapped = (request.fm_mapped_extents as usize).min(EXTENTS_PER_CALL);
        let extents = &request.fm_extents[..mapped];
        if extents
            .iter()
            .any(|extent| extent.fe_flags & FIEMAP_EXTENT_SHARED != 0)
        {
            return Ok(ExtentSharing::Shared);
        }
        let Some(last) = extents.last() else {
            return Ok(ExtentSharing::Exclusive);
        };
        if last.fe_flags & FIEMAP_EXTENT_LAST != 0 {
            return Ok(ExtentSharing::Exclusive);
        }
        let next = last.fe_logical.saturating_add(last.fe_length);
        if next <= start {
            return Ok(ExtentSharing::Exclusive);
        }
        start = next;
    }
}

fn is_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EINVAL)
    )
}

fn volume_sharing(file: &std::fs::File) -> io::Result<ExtentSharing> {
    // SAFETY: `statfs` is plain integer data written by the call below.
    let mut info: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open and `info` is live writable storage.
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut info) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // `f_type`'s width and signedness vary by architecture; magics are 32-bit.
    #[allow(clippy::unnecessary_cast, reason = "f_type width varies by target")]
    let magic = info.f_type as u32;
    Ok(match magic {
        EXT_SUPER_MAGIC | TMPFS_MAGIC | RAMFS_MAGIC => ExtentSharing::Exclusive,
        _ => ExtentSharing::Unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fiemap_request_matches_the_kernel_layout() {
        assert_eq!(std::mem::size_of::<FiemapExtent>(), 56);
        assert_eq!(
            std::mem::size_of::<FiemapRequest>(),
            32 + 56 * EXTENTS_PER_CALL
        );
    }

    #[test]
    fn a_fresh_file_on_a_non_sharing_volume_is_exclusive() {
        // /dev/shm is tmpfs on every supported Linux host.
        let Ok(dir) = tempfile::tempdir_in("/dev/shm") else {
            return;
        };
        let path = dir.path().join("fresh");
        std::fs::write(&path, [1_u8; 8192]).unwrap();
        assert_eq!(extent_sharing(&path).unwrap(), ExtentSharing::Exclusive);
    }

    #[test]
    fn a_reflinked_pair_reports_shared_where_the_volume_clones() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, vec![3_u8; 256 * 1024]).unwrap();
        let clone = dir.path().join("clone");
        if crate::platform::fs::reflink_file(&source, &clone).is_err() {
            return;
        }
        assert_eq!(extent_sharing(&source).unwrap(), ExtentSharing::Shared);
        assert_eq!(extent_sharing(&clone).unwrap(), ExtentSharing::Shared);
        eprintln!("reflink-shared-verified");
    }
}
