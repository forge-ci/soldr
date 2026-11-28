//! Native filesystem fixture operations; these require a Windows runner.
use crate::fs_scan::test_support::scratch;
use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows_sys::Win32::System::IO::DeviceIoControl;

pub fn mark_sparse(file: &File) -> io::Result<()> {
    let mut returned = 0;
    if unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            null(),
            0,
            null_mut(),
            0,
            &raw mut returned,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn fixture(root: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir("..", root.join("nested/loop"))?;
    // Deny file data reads to Everyone while retaining metadata access. This
    // tests scanning a file that cannot be opened for content, just like mode 0
    // on POSIX. icacls is Windows' native ACL editor, not a test double.
    let output = std::process::Command::new("icacls")
        .arg(root.join("unreadable"))
        .args(["/deny", "*S-1-1-0:(RD)"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    if File::open(root.join("unreadable")).is_ok() {
        return Err(io::Error::other("fixture ACL did not deny file reads"));
    }
    Ok(())
}

pub fn allocation_observations(root: &Path) -> io::Result<()> {
    use std::mem::{size_of, MaybeUninit};
    use windows_sys::Win32::Storage::FileSystem::{
        FileStandardInfo, GetFileInformationByHandleEx, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_READ_ATTRIBUTES, FILE_STANDARD_INFO,
    };
    for name in ["", "wide", "nested", "nested/data", "sparse"] {
        use std::os::windows::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(root.join(name))?;
        let mut info = MaybeUninit::<FILE_STANDARD_INFO>::zeroed();
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                info.as_mut_ptr().cast(),
                size_of::<FILE_STANDARD_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let info = unsafe { info.assume_init() };
        println!(
            "Windows allocation {name:?}: standard={} apparent={}",
            info.AllocationSize, info.EndOfFile
        );
        if name == "sparse" {
            println!(
                "Windows physical allocation {name:?}: {}",
                physical_allocation(&root.join(name))?
            );
        }
    }
    Ok(())
}

// GetCompressedFileSizeW measures sparse/compressed storage; for ordinary files
// it can return logical length rather than the rounded allocation size.
pub fn physical_allocation(path: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;
    let name: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut high = 0;
    let low = unsafe { GetCompressedFileSizeW(name.as_ptr(), &raw mut high) };
    if low == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(high) << 32) | u64::from(low))
}

#[test]
#[expect(
    clippy::panic_in_result_fn,
    reason = "Native regression assertions fail the harness"
)]
fn sparse_physical_allocation_matches_windows() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let root = scratch("sparse-physical")?;
    let path = root.join("sparse");
    let mut file = File::create(&path)?;
    mark_sparse(&file)?;
    file.set_len(32 * 1024 * 1024)?;
    file.write_all(b"sparse")?;
    let before_flush = physical_allocation(&path)?;
    file.sync_all()?;
    drop(file);
    let physical = physical_allocation(&path)?;
    println!("Sparse writeback: before_flush={before_flush} after_flush={physical}");
    assert!(physical > 0 && physical < 32 * 1024 * 1024);
    for threads in [1, 2, 8] {
        let summary = crate::fs_scan::scan_stream(
            &path,
            &crate::fs_scan::ScanOptions {
                threads: Some(threads),
                ..Default::default()
            },
            |_| true,
        )?;
        assert_eq!(
            (
                summary.allocated,
                summary.apparent,
                summary.entries,
                summary.errors
            ),
            (physical, 32 * 1024 * 1024, 1, 0)
        );
    }
    Ok(())
}

#[test]
#[expect(
    clippy::panic_in_result_fn,
    reason = "Native regression assertions fail the harness"
)]
fn unpaired_utf16_filename_survives() -> Result<(), Box<dyn std::error::Error>> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let root = scratch("utf16")?;
    let name = OsString::from_wide(&[98, 97, 100, 45, 0xD800]);
    let path = root.join(name);
    std::fs::write(&path, b"bytes")?;
    for threads in [1, 8] {
        let mut names = Vec::new();
        let summary = crate::fs_scan::scan_stream(
            &root,
            &crate::fs_scan::ScanOptions {
                threads: Some(threads),
                ..Default::default()
            },
            |entry| {
                if !entry.relative_name.is_empty() {
                    names.push(root.join(entry.relative_name));
                }
                true
            },
        )?;
        assert_eq!(summary.errors, 0);
        assert_eq!(names.as_slice(), std::slice::from_ref(&path));
    }
    Ok(())
}

#[test]
#[expect(
    clippy::panic_in_result_fn,
    reason = "Native fixture assertions fail the harness"
)]
fn real_tree_parallel_accounting() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let root = scratch("windows-accounting")?;
    std::fs::create_dir(root.join("wide"))?;
    std::fs::create_dir(root.join("nested"))?;
    std::fs::write(root.join("nested/data"), vec![7; 132_579])?;
    std::fs::hard_link(root.join("nested/data"), root.join("hardlink"))?;
    let mut sparse = File::create(root.join("sparse"))?;
    mark_sparse(&sparse)?;
    sparse.set_len(32 * 1024 * 1024)?;
    sparse.write_all(b"sparse")?;
    sparse.sync_all()?;
    drop(sparse);
    std::fs::write(root.join("unreadable"), b"secret")?;
    for i in 0..3000 {
        std::fs::write(root.join("wide").join(i.to_string()), [i as u8; 13])?;
    }
    fixture(&root)?;
    allocation_observations(&root)?;
    let serial = crate::fs_scan::scan_stream(&root, &Default::default(), |_| true)?;
    assert_eq!(serial.errors, 0);
    assert!(crate::fs_scan::test_support::paths(&root)?.contains(&root.join("nested/data")));
    for threads in [2, 8].into_iter().cycle().take(24) {
        let summary = crate::fs_scan::scan_stream(
            &root,
            &crate::fs_scan::ScanOptions {
                threads: Some(threads),
                ..Default::default()
            },
            |entry| {
                if entry.relative_name == std::ffi::OsStr::new("sparse") {
                    assert_eq!(
                        Some(entry.allocated),
                        physical_allocation(&root.join("sparse")).ok()
                    );
                }
                true
            },
        )?;
        assert_eq!(
            (
                serial.allocated,
                serial.apparent,
                serial.entries,
                serial.errors
            ),
            (
                summary.allocated,
                summary.apparent,
                summary.entries,
                summary.errors
            )
        );
    }
    allocation_observations(&root)?;
    Ok(())
}
