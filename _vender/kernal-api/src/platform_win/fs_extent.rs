//! Windows extent-sharing observation.
//!
//! NTFS and FAT cannot share blocks. ReFS (including Dev Drive) can, and
//! reports each extent's reference count through
//! `FSCTL_GET_RETRIEVAL_POINTERS_AND_REFCOUNT`: a count above one means a
//! block clone or snapshot holds the extent too.

use std::io;
use std::os::windows::io::AsRawHandle as _;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    ERROR_HANDLE_EOF, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA,
    ERROR_NOT_SUPPORTED,
};
use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_GET_RETRIEVAL_POINTERS_AND_REFCOUNT, RETRIEVAL_POINTERS_AND_REFCOUNT_BUFFER,
    RETRIEVAL_POINTERS_AND_REFCOUNT_BUFFER_0, STARTING_VCN_INPUT_BUFFER,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::platform::fs::ExtentSharing;

/// File systems that cannot clone blocks between files.
const NON_SHARING: &[&str] = &["NTFS", "FAT", "FAT32", "exFAT"];
/// The block-cloning file system whose extents carry reference counts.
const REFS: &str = "ReFS";
/// Extents fetched per `DeviceIoControl` call.
const EXTENTS_PER_CALL: usize = 256;

type Header = RETRIEVAL_POINTERS_AND_REFCOUNT_BUFFER;
type Extent = RETRIEVAL_POINTERS_AND_REFCOUNT_BUFFER_0;

pub fn extent_sharing(path: &Path) -> io::Result<ExtentSharing> {
    let file = std::fs::File::open(path)?;
    let name = volume_file_system(&file)?;
    if NON_SHARING.iter().any(|fs| name.eq_ignore_ascii_case(fs)) {
        return Ok(ExtentSharing::Exclusive);
    }
    if !name.eq_ignore_ascii_case(REFS) {
        return Ok(ExtentSharing::Unknown);
    }
    match refcount_sharing(&file) {
        Ok(sharing) => Ok(sharing),
        Err(error) if is_unsupported(&error) => Ok(ExtentSharing::Unknown),
        Err(error) => Err(error),
    }
}

fn volume_file_system(file: &std::fs::File) -> io::Result<String> {
    let mut name = [0_u16; 64];
    // SAFETY: the handle is open for the call, `name` is live writable
    // storage of the stated length, and every other output is declined.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let len = name.iter().position(|&unit| unit == 0).unwrap_or(name.len());
    Ok(String::from_utf16_lossy(&name[..len]))
}

fn refcount_sharing(file: &std::fs::File) -> io::Result<ExtentSharing> {
    let extents_offset = std::mem::offset_of!(Header, Extents);
    let bytes = extents_offset + EXTENTS_PER_CALL * std::mem::size_of::<Extent>();
    // `u64` storage keeps the reply aligned for its `i64` fields.
    let mut buffer = vec![0_u64; bytes.div_ceil(8)];
    let mut starting_vcn = 0_i64;
    loop {
        let input = STARTING_VCN_INPUT_BUFFER {
            StartingVcn: starting_vcn,
        };
        let mut returned = 0_u32;
        // SAFETY: the handle is open, `input` is a live input buffer, and
        // `buffer` is live aligned writable storage of at least `bytes`.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_GET_RETRIEVAL_POINTERS_AND_REFCOUNT,
                std::ptr::addr_of!(input).cast(),
                std::mem::size_of::<STARTING_VCN_INPUT_BUFFER>() as u32,
                buffer.as_mut_ptr().cast(),
                bytes as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        let more = if ok != 0 {
            false
        } else {
            let error = io::Error::last_os_error();
            match error.raw_os_error().map(|code| code as u32) {
                // Nothing allocated at or past `starting_vcn`.
                Some(ERROR_HANDLE_EOF) => return Ok(ExtentSharing::Exclusive),
                Some(ERROR_MORE_DATA) => true,
                _ => return Err(error),
            }
        };
        let header = buffer.as_ptr().cast::<Header>();
        // SAFETY: the call wrote a header at the start of `buffer`.
        let count = unsafe { (*header).ExtentCount } as usize;
        let count = count.min(EXTENTS_PER_CALL);
        // SAFETY: `buffer` holds `EXTENTS_PER_CALL` extents after the
        // header and the call initialized the first `count` of them.
        let extents = unsafe {
            std::slice::from_raw_parts(
                buffer.as_ptr().cast::<u8>().add(extents_offset).cast::<Extent>(),
                count,
            )
        };
        if let Some(sharing) = classify(extents) {
            return Ok(sharing);
        }
        match extents.last() {
            Some(last) if more && last.NextVcn > starting_vcn => starting_vcn = last.NextVcn,
            _ => return Ok(ExtentSharing::Exclusive),
        }
    }
}

/// `Some(Shared)` when an allocated extent has another reference. Holes
/// (`Lcn == -1`) own no clusters and are skipped.
fn classify(extents: &[Extent]) -> Option<ExtentSharing> {
    extents
        .iter()
        .any(|extent| extent.Lcn != -1 && extent.ReferenceCount > 1)
        .then_some(ExtentSharing::Shared)
}

fn is_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(lcn: i64, references: u32) -> Extent {
        Extent {
            NextVcn: 0,
            Lcn: lcn,
            ReferenceCount: references,
        }
    }

    #[test]
    fn refcount_reply_matches_the_winioctl_layout() {
        assert_eq!(std::mem::offset_of!(Header, Extents), 16);
        assert_eq!(std::mem::size_of::<Extent>(), 24);
    }

    #[test]
    fn classify_calls_a_second_reference_shared_and_skips_holes() {
        assert_eq!(classify(&[]), None);
        assert_eq!(classify(&[extent(10, 1), extent(20, 1)]), None);
        assert_eq!(classify(&[extent(-1, 2)]), None);
        assert_eq!(
            classify(&[extent(10, 1), extent(20, 2)]),
            Some(ExtentSharing::Shared)
        );
    }

    /// `KERNAL_REFS_TEST_DIR` names a directory on a ReFS volume (CI
    /// formats a Dev Drive); without it the NTFS temp directory is used.
    fn test_dir() -> (tempfile::TempDir, bool) {
        match std::env::var_os("KERNAL_REFS_TEST_DIR") {
            Some(root) => (tempfile::tempdir_in(root).unwrap(), true),
            None => (tempfile::tempdir().unwrap(), false),
        }
    }

    #[test]
    fn a_fresh_file_is_exclusive_on_ntfs_and_refs() {
        let (dir, refs) = test_dir();
        let path = dir.path().join("fresh");
        std::fs::write(&path, vec![5_u8; 256 * 1024]).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let name = volume_file_system(&file).unwrap();
        if refs {
            assert!(name.eq_ignore_ascii_case(REFS), "{name}");
        }
        if name.eq_ignore_ascii_case(REFS) || NON_SHARING.contains(&name.as_str()) {
            assert_eq!(extent_sharing(&path).unwrap(), ExtentSharing::Exclusive);
        }
    }

    #[test]
    fn a_block_cloned_pair_reports_shared_on_refs() {
        let (dir, refs) = test_dir();
        let source = dir.path().join("source");
        std::fs::write(&source, vec![3_u8; 256 * 1024]).unwrap();
        let clone = dir.path().join("clone");
        let cloned = crate::platform::fs::reflink_file(&source, &clone);
        if refs {
            cloned.as_ref().expect("ReFS block clone");
        }
        if cloned.is_err() {
            return;
        }
        assert_eq!(extent_sharing(&source).unwrap(), ExtentSharing::Shared);
        assert_eq!(extent_sharing(&clone).unwrap(), ExtentSharing::Shared);
    }
}
