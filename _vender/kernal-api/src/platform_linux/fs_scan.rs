//! All Linux syscalls live here. No pathname-based metadata in the entry loop.
use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::fs_scan::{EntryKind, Metadata};

pub(crate) struct Dir(OwnedFd);
const OPEN: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;

// Linux UAPI include/uapi/linux/stat.h: a fixed 256-byte syscall buffer.
// libc's musl targets do not expose statx, although the kernel syscall exists.
// Keep the unread timestamp/extension fields reserved, preserving their layout.
#[repr(C)]
struct Statx {
    stx_mask: u32,
    _blksize: u32,
    _attributes: u64,
    stx_nlink: u32,
    _uid: u32,
    _gid: u32,
    stx_mode: u16,
    _spare: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    _attributes_mask: u64,
    _timestamps: [u64; 8],
    _rdev: [u32; 2],
    stx_dev_major: u32,
    stx_dev_minor: u32,
    stx_mnt_id: u64,
    _extensions: [u64; 13],
}
const STATX_MNT_ID: u32 = 0x1000;
const STATX_REQUEST: u32 = 0x0705 | STATX_MNT_ID; // TYPE, NLINK, INO, SIZE, BLOCKS
const AT_STATX_DONT_SYNC: i32 = 0x4000;

fn owned(fd: i32) -> io::Result<Dir> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a successful open/openat returns a new exclusively owned fd.
        Ok(Dir(unsafe { OwnedFd::from_raw_fd(fd) }))
    }
}

impl Dir {
    pub fn open(path: &Path, follow: bool) -> io::Result<Self> {
        let name = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: NUL terminated name; returned descriptor is immediately owned.
        owned(unsafe { libc::open(name.as_ptr(), OPEN | nofollow(follow)) })
    }

    pub fn open_at(&self, name: &OsStr, follow: bool) -> io::Result<Self> {
        // Names borrow getdents' NUL-terminated record; CString is needed only for
        // directories, never for each file's statx.
        let name = CString::new(name.as_bytes())?;
        owned(unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), OPEN | nofollow(follow)) })
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        stat(self.0.as_raw_fd(), c"", libc::AT_EMPTY_PATH)
    }

    pub fn read(
        &self,
        buffer: &mut [u8],
        follow: bool,
        mut visit: impl FnMut(&OsStr, io::Result<Metadata>) -> bool,
    ) -> io::Result<()> {
        loop {
            // SAFETY: buffer is writable for its full supplied size; fd is live.
            let n = unsafe {
                libc::syscall(
                    libc::SYS_getdents64,
                    self.0.as_raw_fd(),
                    buffer.as_mut_ptr(),
                    buffer.len(),
                )
            };
            if n < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if n == 0 {
                return Ok(());
            }
            let mut cursor = 0;
            while cursor < n as usize {
                let (name, length) = dirent(&buffer[cursor..n as usize])?;
                cursor += length;
                if name == c"." || name == c".." {
                    continue;
                }
                let meta = stat(
                    self.0.as_raw_fd(),
                    name,
                    if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW },
                );
                if !visit(OsStr::from_bytes(name.to_bytes()), meta) {
                    return Ok(());
                }
            }
        }
    }
}

fn nofollow(follow: bool) -> i32 {
    if follow {
        0
    } else {
        libc::O_NOFOLLOW
    }
}

pub(crate) fn root(path: &Path, follow: bool) -> io::Result<Metadata> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    stat(
        libc::AT_FDCWD,
        &path,
        if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW },
    )
}

fn stat(fd: i32, name: &CStr, flags: i32) -> io::Result<Metadata> {
    let mut buf = MaybeUninit::<Statx>::zeroed();
    // SAFETY: SYS_statx receives a live dirfd, NUL name and writable statx.
    // Use the raw syscall to avoid a glibc >=2.28 wrapper dependency.
    let result = unsafe {
        libc::syscall(
            libc::SYS_statx,
            fd,
            name.as_ptr(),
            flags | AT_STATX_DONT_SYNC,
            STATX_REQUEST,
            buf.as_mut_ptr(),
        )
    };
    if result == 0 {
        let s = unsafe { buf.assume_init() };
        return Ok(Metadata {
            device: (u64::from(s.stx_dev_major) << 32) | u64::from(s.stx_dev_minor),
            inode: s.stx_ino,
            mount: (s.stx_mask & STATX_MNT_ID != 0).then_some(s.stx_mnt_id),
            allocated: s.stx_blocks.saturating_mul(512),
            apparent: s.stx_size,
            is_dir: u32::from(s.stx_mode) & libc::S_IFMT == libc::S_IFDIR,
            links: u64::from(s.stx_nlink),
            kind: kind(u32::from(s.stx_mode)),
        });
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOSYS) {
        return Err(error);
    }
    fallback(fd, name, flags)
}

/// `st_nlink` is `u16` on macOS, `u32` on aarch64 and `u64` on x86_64. A generic
/// conversion widens without tripping clippy's `unnecessary_cast` /
/// `useless_conversion` on the targets where the field is already `u64`.
#[inline]
fn widen_links(value: impl Into<u64>) -> u64 {
    value.into()
}

fn fallback(fd: i32, name: &CStr, flags: i32) -> io::Result<Metadata> {
    let mut buf = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstatat(fd, name.as_ptr(), buf.as_mut_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let s = unsafe { buf.assume_init() };
    Ok(Metadata {
        device: (u64::from(libc::major(s.st_dev)) << 32) | u64::from(libc::minor(s.st_dev)),
        inode: s.st_ino,
        mount: None,
        allocated: (s.st_blocks.max(0) as u64).saturating_mul(512),
        apparent: s.st_size.max(0) as u64,
        is_dir: s.st_mode & libc::S_IFMT == libc::S_IFDIR,
        links: widen_links(s.st_nlink),
        kind: kind(s.st_mode),
    })
}

fn dirent(bytes: &[u8]) -> io::Result<(&CStr, usize)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid getdents64 record");
    let len = bytes.get(16..18).ok_or_else(invalid)?;
    let length = usize::from(u16::from_ne_bytes([len[0], len[1]]));
    if length < 20 || !length.is_multiple_of(8) {
        return Err(invalid());
    }
    let name = CStr::from_bytes_until_nul(bytes.get(19..length).ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    if name.is_empty() || name.to_bytes().contains(&b'/') {
        return Err(invalid());
    }
    Ok((name, length))
}

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "Native fixture assertions intentionally fail the test harness"
)]
mod tests {
    use super::*;

    #[test]
    fn statx_kernel_layout_is_libc_independent() {
        use std::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<Statx>(), 256);
        assert_eq!(align_of::<Statx>(), 8);
        assert_eq!(offset_of!(Statx, stx_mask), 0);
        assert_eq!(offset_of!(Statx, stx_nlink), 16);
        assert_eq!(offset_of!(Statx, stx_mode), 28);
        assert_eq!(offset_of!(Statx, stx_ino), 32);
        assert_eq!(offset_of!(Statx, stx_size), 40);
        assert_eq!(offset_of!(Statx, stx_blocks), 48);
        assert_eq!(offset_of!(Statx, stx_dev_major), 136);
        assert_eq!(offset_of!(Statx, stx_dev_minor), 140);
        assert_eq!(offset_of!(Statx, stx_mnt_id), 144);
        #[cfg(target_env = "gnu")]
        {
            assert_eq!(size_of::<Statx>(), size_of::<libc::statx>());
            assert_eq!(
                STATX_REQUEST,
                libc::STATX_TYPE
                    | libc::STATX_NLINK
                    | libc::STATX_INO
                    | libc::STATX_SIZE
                    | libc::STATX_BLOCKS
                    | libc::STATX_MNT_ID
            );
            assert_eq!(AT_STATX_DONT_SYNC, libc::AT_STATX_DONT_SYNC);
        }
    }

    #[test]
    fn malformed_dirents_are_rejected() {
        for len in 0..32 {
            assert!(dirent(&vec![0; len]).is_err());
        }
        let mut bytes = vec![0; 24];
        bytes[16..18].copy_from_slice(&24u16.to_ne_bytes());
        bytes[19] = b'a';
        assert!(dirent(&bytes).is_ok_and(|(n, _)| n.to_bytes() == b"a"));
        bytes[20..].fill(b'x');
        assert!(dirent(&bytes).is_err());
    }

    #[test]
    fn fstatat_fallback_matches_statx() -> io::Result<()> {
        let dir = Dir::open(Path::new("."), false)?;
        let modern = dir.metadata()?;
        let old = fallback(dir.0.as_raw_fd(), c"", libc::AT_EMPTY_PATH)?;
        assert_eq!(modern.identity(), old.identity());
        assert_eq!((modern.links, modern.is_dir), (old.links, old.is_dir));
        assert_eq!(
            (modern.allocated, modern.apparent),
            (old.allocated, old.apparent)
        );
        Ok(())
    }

    #[test]
    fn fd_relative_walk_exceeds_path_max() -> Result<(), Box<dyn std::error::Error>> {
        use std::path::PathBuf;
        let base = std::env::current_dir()?
            .join("target/test-fixtures")
            .join(format!(
                "long-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos()
            ));
        std::fs::create_dir_all(&base)?;
        let mut parents = vec![Dir::open(&base, false)?];
        let name = CString::new(vec![b'd'; 200])?;
        let os_name = OsStr::from_bytes(name.to_bytes());
        let mut path = PathBuf::from(&base);
        for _ in 0..32 {
            let dir = parents
                .last()
                .ok_or_else(|| io::Error::other("missing parent"))?;
            if unsafe { libc::mkdirat(dir.0.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
            let next = dir.open_at(os_name, false)?;
            parents.push(next);
            path.push(os_name);
        }
        assert!(path.as_os_str().as_bytes().len() > 4096);
        let mut found = false;
        let result =
            crate::fs_scan::scan_stream(&base, &crate::fs_scan::ScanOptions::default(), |entry| {
                if base.join(entry.relative_name) == path {
                    found = true;
                }
                true
            })?;
        // Preserve the real fixture without leaving paths that break libgit2's
        // packaging status walk. Moving fd-relative subtrees splits the tested
        // long chain into short segments; no filesystem entries are deleted.
        for index in [10, 20, 30] {
            let destination = CString::new(format!("segment-{index}"))?;
            // SAFETY: both directory fds and both NUL-terminated names are live.
            if unsafe {
                libc::renameat(
                    parents[index].0.as_raw_fd(),
                    name.as_ptr(),
                    parents[0].0.as_raw_fd(),
                    destination.as_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error().into());
            }
        }
        assert!(found);
        assert_eq!(result.entries, 33);
        assert_eq!(result.errors, 0);
        Ok(())
    }

    #[test]
    fn one_filesystem_skips_followed_foreign_mount() -> Result<(), Box<dyn std::error::Error>> {
        let base = std::env::current_dir()?
            .join("target/test-fixtures")
            .join(format!(
                "mount-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos()
            ));
        std::fs::create_dir_all(&base)?;
        if root(Path::new("/proc"), true)?.same_filesystem(root(&base, false)?) {
            return Ok(()); // No foreign proc mount in this environment.
        }
        std::os::unix::fs::symlink("/proc", base.join("foreign"))?;
        let result = crate::fs_scan::scan_stream(
            &base,
            &crate::fs_scan::ScanOptions {
                follow_symlinks: true,
                one_filesystem: true,
                ..crate::fs_scan::ScanOptions::default()
            },
            |_| true,
        )?;
        assert_eq!(result.entries, 1);
        assert_eq!(result.errors, 0);
        Ok(())
    }
}

fn kind(mode: u32) -> EntryKind {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => EntryKind::Directory,
        libc::S_IFREG => EntryKind::File,
        libc::S_IFLNK => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
}

pub(crate) fn is_system_root(meta: Metadata) -> bool {
    root(Path::new("/"), true).is_ok_and(|r| r.identity() == meta.identity())
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
