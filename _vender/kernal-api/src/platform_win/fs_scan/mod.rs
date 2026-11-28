//! Windows ABI boundary. Moved from forge-ci/jettison PR #1; see docs/fs-scan.md.
use std::ffi::{OsStr, OsString};
use std::io;
use std::mem::{size_of, MaybeUninit};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::fs_scan::{EntryKind, Metadata, ScanCapability};
#[cfg(not(test))]
mod records;
#[cfg(test)]
use crate::fs_scan::windows_record_tests as records;
use records as windows_records;
#[cfg(not(test))]
mod mft;
#[cfg(test)]
use crate::fs_scan::mft_parser_tests as mft;
use crate::fs_scan::{OwnedEntry as Entry, ScanError, ScanOptions, Summary};

mod volume;

pub(crate) fn wide(path: &OsStr) -> Vec<u16> {
    path.encode_wide().chain(Some(0)).collect()
}
fn extended(path: &Path) -> io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    Ok(OsString::from_wide(&windows_records::extended(
        &path.as_os_str().encode_wide().collect::<Vec<_>>(),
    ))
    .into())
}
pub(crate) fn open(path: &Path, access: u32, flags: u32) -> io::Result<OwnedHandle> {
    let name = wide(path.as_os_str());
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            flags,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}
fn metadata(handle: &OwnedHandle, follow: bool) -> io::Result<Metadata> {
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    let mut standard = MaybeUninit::<FILE_STANDARD_INFO>::zeroed();
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), info.as_mut_ptr()) } == 0
        || unsafe {
            GetFileInformationByHandleEx(
                handle.as_raw_handle(),
                FileStandardInfo,
                standard.as_mut_ptr().cast(),
                size_of::<FILE_STANDARD_INFO>() as u32,
            )
        } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let i = unsafe { info.assume_init() };
    let s = unsafe { standard.assume_init() };
    let reparse = i.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    let physical = if i.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        && i.dwFileAttributes & (FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_COMPRESSED) != 0
    {
        let mut compression = MaybeUninit::<FILE_COMPRESSION_INFO>::zeroed();
        if unsafe {
            GetFileInformationByHandleEx(
                handle.as_raw_handle(),
                FileCompressionInfo,
                compression.as_mut_ptr().cast(),
                size_of::<FILE_COMPRESSION_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Some(unsafe { compression.assume_init() }.CompressedFileSize)
    } else {
        None
    };
    Ok(Metadata {
        device: u64::from(i.dwVolumeSerialNumber),
        inode: (u64::from(i.nFileIndexHigh) << 32) | u64::from(i.nFileIndexLow),
        mount: None,
        allocated: windows_records::allocation(s.AllocationSize, physical)?,
        apparent: s.EndOfFile.max(0) as u64,
        is_dir: i.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 && (follow || !reparse),
        links: u64::from(i.nNumberOfLinks),
        kind: if reparse && !follow {
            EntryKind::Symlink
        } else if i.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            EntryKind::Directory
        } else {
            EntryKind::File
        },
    })
}
pub(crate) fn root(path: &Path, follow: bool) -> io::Result<Metadata> {
    let flags = FILE_FLAG_BACKUP_SEMANTICS
        | if follow {
            0
        } else {
            FILE_FLAG_OPEN_REPARSE_POINT
        };
    metadata(
        &open(&extended(path)?, FILE_READ_ATTRIBUTES, flags)?,
        follow,
    )
}

pub(crate) struct Dir {
    handle: OwnedHandle,
    path: PathBuf,
}
impl Dir {
    pub fn open(path: &Path, follow: bool) -> io::Result<Self> {
        let path = extended(path)?;
        let handle = open(
            &path,
            FILE_READ_ATTRIBUTES,
            FILE_FLAG_BACKUP_SEMANTICS
                | if follow {
                    0
                } else {
                    FILE_FLAG_OPEN_REPARSE_POINT
                },
        )?;
        Ok(Self { handle, path })
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        metadata(&self.handle, true)
    }
    pub fn open_at(&self, name: &OsStr, follow: bool) -> io::Result<Self> {
        Self::open(&self.path.join(name), follow)
    }
    pub fn read(
        &self,
        _buffer: &mut [u8],
        follow: bool,
        mut visit: impl FnMut(&OsStr, io::Result<Metadata>) -> bool,
    ) -> io::Result<()> {
        let pattern = wide(self.path.join("*").as_os_str());
        let mut data = MaybeUninit::<WIN32_FIND_DATAW>::zeroed();
        let mut raw = unsafe {
            FindFirstFileExW(
                pattern.as_ptr(),
                FindExInfoBasic,
                data.as_mut_ptr().cast(),
                FindExSearchNameMatch,
                null(),
                FIND_FIRST_EX_LARGE_FETCH,
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            raw = unsafe {
                FindFirstFileExW(
                    pattern.as_ptr(),
                    FindExInfoBasic,
                    data.as_mut_ptr().cast(),
                    FindExSearchNameMatch,
                    null(),
                    0,
                )
            };
        }
        if raw == INVALID_HANDLE_VALUE {
            // Last compatibility path; errors remain visible to the engine.
            for entry in std::fs::read_dir(&self.path)? {
                let e = entry?;
                if !visit(&e.file_name(), root(&e.path(), follow)) {
                    break;
                }
            }
            return Ok(());
        }
        struct Find(windows_sys::Win32::Foundation::HANDLE);
        impl Drop for Find {
            fn drop(&mut self) {
                unsafe {
                    FindClose(self.0);
                }
            }
        }
        let guard = Find(raw);
        loop {
            let d = unsafe { data.assume_init_ref() };
            if let Some(units) = windows_records::filename(&d.cFileName)? {
                let name = OsString::from_wide(units);
                if !visit(&name, root(&self.path.join(&name), follow)) {
                    break;
                }
            }
            if unsafe { FindNextFileW(guard.0, data.as_mut_ptr()) } == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
                    return Err(e);
                }
                break;
            }
        }
        Ok(())
    }
}

pub(crate) fn ioctl(
    handle: &OwnedHandle,
    code: u32,
    input: &[u8],
    output: &mut [u8],
) -> io::Result<usize> {
    let mut returned = 0;
    if unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            code,
            input.as_ptr().cast(),
            input.len() as u32,
            output.as_mut_ptr().cast(),
            output.len() as u32,
            &raw mut returned,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(returned as usize)
}

pub(crate) fn fast_scan(
    root: &Path,
    opts: &ScanOptions,
    sink: &mut dyn FnMut(Entry) -> bool,
) -> Result<Option<Summary>, ScanError> {
    if opts.follow_symlinks || opts.capability == ScanCapability::DirectoryWalk {
        return Ok(None);
    }
    let canonical = extended(root)?;
    // Only whole drive roots. Subdirectory requests retain their exact scope.
    let Some(drive) =
        windows_records::drive_letter(&canonical.as_os_str().encode_wide().collect::<Vec<_>>())
    else {
        return Ok(None);
    };
    let device = PathBuf::from(OsString::from_wide(&[92, 92, 46, 92, drive, 58]));
    if let Ok(volume) = open(&device, GENERIC_READ, 0) {
        if let Ok(records) = volume::mft(&volume, &canonical) {
            if let Ok(Some(summary)) = volume::emit(records, self::root(root, false)?, sink) {
                return Ok(Some(summary));
            }
        }
    }
    // USN also needs a volume handle: access=0 is attempted rather than promising
    // unprivileged access, which Windows policies and filesystem versions vary.
    if let Ok((entries, metadata_allocated)) =
        open(&device, 0, 0).and_then(|volume| volume::usn(&volume, root))
    {
        let parents: std::collections::HashMap<_, _> = entries
            .iter()
            .filter(|(_, meta)| meta.is_dir)
            .map(|(path, meta)| (path.clone(), *meta))
            .collect();
        let root_meta = self::root(root, false)?;
        let mut summary = Summary {
            metadata_allocated,
            ..Summary::default()
        };
        let mut seen = std::collections::HashSet::new();
        for (path, meta) in entries {
            if !seen.insert(meta.identity()) {
                continue;
            }
            let parent = if path == root {
                root_meta
            } else {
                *path
                    .parent()
                    .and_then(|p| parents.get(p))
                    .ok_or_else(|| io::Error::other("missing USN parent"))?
            };
            let relative = path
                .strip_prefix(root)
                .map_err(|_| io::Error::other("USN path outside scan root"))?
                .to_path_buf();
            summary.allocated += meta.allocated;
            summary.apparent += meta.apparent;
            summary.entries += 1;
            if !sink(Entry::new(relative, meta, parent.file_identity())) {
                summary.cancelled = true;
                break;
            }
        }
        return Ok(Some(summary));
    }
    Ok(None)
}

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "Native fixture assertions intentionally fail the test harness"
)]
mod tests {
    use super::*;
    #[test]
    fn native_find_matches_standard_library() -> io::Result<()> {
        let path = Path::new(".");
        let dir = Dir::open(path, false)?;
        let mut found = std::collections::HashSet::new();
        dir.read(&mut [], false, |name, result| {
            assert!(result.is_ok());
            found.insert(name.to_os_string());
            true
        })?;
        let expected: std::collections::HashSet<_> = std::fs::read_dir(path)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<_>>()?;
        assert_eq!(found, expected);
        Ok(())
    }
}

pub(crate) fn is_system_root(_: Metadata) -> bool {
    false
}
pub(crate) fn capabilities() -> crate::fs_scan::Capabilities {
    crate::fs_scan::Capabilities {
        raw_mft_supported: true,
        requires_elevation: true,
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests_filesystem;
