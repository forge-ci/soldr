//! Darwin bulk enumeration, adapted from Kencho5/derust (MIT). Originally validated in forge-ci/jettison PR #1.
use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::fs_scan::{EntryKind, Metadata};
#[cfg(not(test))]
#[path = "fs_scan_bulk.rs"]
mod bulk;
#[cfg(test)]
use crate::fs_scan::bulk_parser_tests as bulk;

pub(crate) struct Dir(OwnedFd);
const OPEN: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;

fn owned(fd: i32) -> io::Result<Dir> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(Dir(unsafe { OwnedFd::from_raw_fd(fd) }))
    }
}
fn flags(follow: bool) -> i32 {
    if follow {
        0
    } else {
        libc::O_NOFOLLOW
    }
}

impl Dir {
    pub fn open(path: &Path, follow: bool) -> io::Result<Self> {
        let name = CString::new(path.as_os_str().as_bytes())?;
        owned(unsafe { libc::open(name.as_ptr(), OPEN | flags(follow)) })
    }
    pub fn open_at(&self, name: &OsStr, follow: bool) -> io::Result<Self> {
        let name = CString::new(name.as_bytes())?;
        owned(unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), OPEN | flags(follow)) })
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        let mut s = MaybeUninit::<libc::stat>::zeroed();
        if unsafe { libc::fstat(self.0.as_raw_fd(), s.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(convert(unsafe { s.assume_init() }))
    }
    pub fn read(
        &self,
        buffer: &mut [u8],
        follow: bool,
        mut visit: impl FnMut(&OsStr, io::Result<Metadata>) -> bool,
    ) -> io::Result<()> {
        let mut attrs = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: bulk::RETURNED
                | bulk::ERROR
                | bulk::NAME
                | bulk::DEVID
                | bulk::OBJTYPE
                | bulk::FILEID,
            volattr: 0,
            dirattr: 0,
            fileattr: libc::ATTR_FILE_LINKCOUNT
                | libc::ATTR_FILE_DATALENGTH
                | libc::ATTR_FILE_ALLOCSIZE,
            forkattr: 0,
        };
        loop {
            let count = unsafe {
                libc::getattrlistbulk(
                    self.0.as_raw_fd(),
                    (&raw mut attrs).cast(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                )
            };
            if count < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if count == 0 {
                return Ok(());
            }
            let mut cursor = 0usize;
            for _ in 0..count {
                let tail = buffer
                    .get(cursor..)
                    .ok_or_else(|| io::Error::other("bulk offset overflow"))?;
                let length = bulk::length(tail)?;
                let e = bulk::parse(tail)?;
                cursor = cursor
                    .checked_add(length)
                    .ok_or_else(|| io::Error::other("bulk record overflow"))?;
                if e.name == b"." || e.name == b".." {
                    continue;
                }
                let name = OsStr::from_bytes(e.name);
                let result = if e.error != 0 {
                    Err(io::Error::from_raw_os_error(e.error))
                } else {
                    self.entry(name, &e, follow)
                };
                if !visit(name, result) {
                    return Ok(());
                }
            }
        }
    }
    fn entry(&self, name: &OsStr, e: &bulk::BulkEntry<'_>, follow: bool) -> io::Result<Metadata> {
        if let (Some(1), Some(device), Some(inode), Some(links), Some(allocated), Some(apparent)) =
            (e.kind, e.device, e.inode, e.links, e.allocated, e.apparent)
        {
            return Ok(Metadata {
                device: u64::from(device),
                inode,
                links: u64::from(links),
                allocated,
                apparent,
                mount: None,
                is_dir: false,
                kind: EntryKind::File,
            });
        }
        let name = CString::new(name.as_bytes())?;
        stat(self.0.as_raw_fd(), &name, follow)
    }
}

fn convert(s: libc::stat) -> Metadata {
    Metadata {
        device: u64::from(s.st_dev as u32),
        inode: s.st_ino,
        mount: None,
        allocated: (s.st_blocks.max(0) as u64).saturating_mul(512),
        apparent: s.st_size.max(0) as u64,
        is_dir: s.st_mode & libc::S_IFMT == libc::S_IFDIR,
        links: u64::from(s.st_nlink),
        kind: match s.st_mode & libc::S_IFMT {
            libc::S_IFDIR => EntryKind::Directory,
            libc::S_IFREG => EntryKind::File,
            libc::S_IFLNK => EntryKind::Symlink,
            _ => EntryKind::Other,
        },
    }
}
fn stat(fd: i32, name: &CStr, follow: bool) -> io::Result<Metadata> {
    let mut s = MaybeUninit::<libc::stat>::zeroed();
    if unsafe {
        libc::fstatat(
            fd,
            name.as_ptr(),
            s.as_mut_ptr(),
            if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW },
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(convert(unsafe { s.assume_init() }))
}
pub(crate) fn root(path: &Path, follow: bool) -> io::Result<Metadata> {
    stat(
        libc::AT_FDCWD,
        &CString::new(path.as_os_str().as_bytes())?,
        follow,
    )
}

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "Native fixture assertions intentionally fail the test harness"
)]
mod tests {
    use super::*;
    #[test]
    fn native_bulk_matches_metadata() -> io::Result<()> {
        let path = Path::new(".");
        let dir = Dir::open(path, false)?;
        let mut found = 0;
        let mut errors = Vec::new();
        dir.read(&mut vec![0; 128 * 1024], false, |name, result| {
            match (result, root(&path.join(name), false)) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(
                        (a.identity(), a.allocated, a.apparent),
                        (b.identity(), b.allocated, b.apparent)
                    );
                    found += 1;
                }
                (a, b) => errors.push(format!("{a:?} {b:?}")),
            }
            true
        })?;
        assert!(found > 0);
        assert!(errors.is_empty(), "{errors:?}");
        Ok(())
    }
}

pub(crate) fn is_system_root(_: Metadata) -> bool {
    false
}
pub(crate) fn fast_scan(
    _: &Path,
    _: &crate::fs_scan::ScanOptions,
    _: &mut dyn FnMut(crate::fs_scan::OwnedEntry) -> bool,
) -> Result<Option<crate::fs_scan::Summary>, crate::fs_scan::ScanError> {
    Ok(None)
}
pub(crate) fn capabilities() -> crate::fs_scan::Capabilities {
    crate::fs_scan::Capabilities {
        raw_mft_supported: false,
        requires_elevation: false,
    }
}

#[cfg(test)]
#[path = "fs_scan_tests.rs"]
mod filesystem_tests;
