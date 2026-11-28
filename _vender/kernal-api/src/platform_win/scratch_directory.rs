//! Keep an ID-opened metadata handle during worker execution. Path-opened
//! directory handles pin ancestor renames even when delete sharing is enabled.
//! Acquire that stronger handle only for cleanup, then verify its identity
//! against the still-live original before allowing recursive removal.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::MaybeUninit;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    FileIdInfo, FileIdType, GetFileInformationByHandleEx, GetFinalPathNameByHandleW, OpenFileById,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

pub(crate) struct Anchor(File);

impl Anchor {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::other("scratch anchor is not a plain directory"));
        }
        // Fail before transferring TempDir ownership if identity queries are
        // unavailable on this filesystem. Never fall back to a stored path.
        let anchor = reopen_by_id(&file)?;
        require_same_directory(&file, &anchor)?;
        // Ownership overlaps: the object cannot disappear and have its ID
        // recycled between closing the path handle and acquiring the anchor.
        drop(file);
        Ok(Self(anchor))
    }

    pub(crate) fn remove(self) -> io::Result<()> {
        let current = current_path(&self.0)?;
        let directory = cap_std::fs::Dir::open_ambient_dir(&current, cap_std::ambient_authority())?;
        let file = directory.into_std_file();
        require_same_directory(&self.0, &file)?;
        // `file` was opened by cap-std, without FILE_SHARE_DELETE. Do not
        // construct a Dir from the permissive metadata anchor: that would
        // violate the backend's documented path-stability requirement.
        let result = cap_std::fs::Dir::from_std_file(file).remove_open_dir_all();
        drop(self);
        result
    }
}

fn reopen_by_id(original: &File) -> io::Result<File> {
    let id = super::fs::file_identity(original)?
        .ok_or_else(|| io::Error::other("scratch directory has no reopenable file identity"))?;
    let descriptor = FILE_ID_DESCRIPTOR {
        dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: FileIdType,
        Anonymous: FILE_ID_DESCRIPTOR_0 {
            FileId: id.file as i64,
        },
    };
    // SAFETY: the volume-hint handle remains live and descriptor uses the
    // matching FileId discriminator. No security pointer is supplied.
    let raw = unsafe {
        OpenFileById(
            original.as_raw_handle(),
            &descriptor,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenFileById returned a new owned, valid handle.
    Ok(unsafe { File::from_raw_handle(raw) })
}

fn require_same_directory(original: &File, candidate: &File) -> io::Result<()> {
    if identity(original)? != identity(candidate)? {
        return Err(io::Error::other(
            "scratch directory changed while acquiring cleanup handle",
        ));
    }
    Ok(())
}

fn identity(file: &File) -> io::Result<(u64, [u8; 16])> {
    let mut info = MaybeUninit::<FILE_ID_INFO>::uninit();
    // SAFETY: the borrowed handle is live and the output has the exact size
    // and alignment required for FileIdInfo. Read it only after success.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful API call initialized FILE_ID_INFO.
    let info = unsafe { info.assume_init() };
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

fn current_path(file: &File) -> io::Result<PathBuf> {
    // Maximum extended Win32 path plus its terminator. One bounded query also
    // avoids a sizing-call/requery race if an ancestor is being renamed.
    let mut path = vec![0_u16; 32_768];
    // SAFETY: the handle is live and path contains the advertised writable
    // number of UTF-16 code units. Flags zero request normalized DOS spelling.
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            path.as_mut_ptr(),
            path.len() as u32,
            0,
        )
    };
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize >= path.len() {
        return Err(io::Error::other(
            "scratch directory path exceeds Win32 limit",
        ));
    }
    Ok(PathBuf::from(OsString::from_wide(&path[..length as usize])))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Preserve the native NTFS lifecycle that established ID-opened handles
    /// avoid the ancestor pin. Exercise the production constructor directly.
    #[test]
    fn id_opened_directory_survives_ancestor_rename_and_cleans_original() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let scratch = parent.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("partial"), b"owned").unwrap();
        let by_id = Anchor::open(&scratch).unwrap();

        let moved = root.path().join("moved");
        std::fs::rename(&parent, &moved).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("keep"), b"unrelated").unwrap();
        by_id.remove().unwrap();
        assert!(!moved.join("scratch").exists());
        assert_eq!(std::fs::read(scratch.join("keep")).unwrap(), b"unrelated");
    }

    #[test]
    fn cleanup_identity_rejects_a_different_directory() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let original = Anchor::open(first.path()).unwrap();
        let replacement = Anchor::open(second.path()).unwrap();
        assert!(require_same_directory(&original.0, &replacement.0).is_err());
        assert!(require_same_directory(&original.0, &original.0).is_ok());
    }
}
