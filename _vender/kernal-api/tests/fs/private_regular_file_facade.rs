#![cfg(all(feature = "fs", unix))]

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};

use kernal_api::platform::fs::{read_private_regular_file_bounded, MAX_PRIVATE_REGULAR_FILE_BYTES};

fn private_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("make test directory private");
    directory
}

fn private_file(directory: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = directory.path().join(name);
    fs::write(&path, bytes).expect("write test file");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("make test file private");
    path
}

#[test]
fn bounded_private_read_accepts_empty_and_exact_limit() {
    let directory = private_directory();
    assert_eq!(
        read_private_regular_file_bounded(&private_file(&directory, "empty", b""), 0).unwrap(),
        b""
    );
    assert_eq!(
        read_private_regular_file_bounded(&private_file(&directory, "exact", b"abc"), 3).unwrap(),
        b"abc"
    );
}

#[test]
fn bounded_private_read_rejects_oversize_and_insecure_permissions() {
    let directory = private_directory();
    let oversized = private_file(&directory, "oversized", b"abcd");
    assert!(read_private_regular_file_bounded(&oversized, 3).is_err());
    let insecure = private_file(&directory, "insecure", b"ok");
    fs::set_permissions(&insecure, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        read_private_regular_file_bounded(&insecure, 2)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[test]
fn bounded_private_read_rejects_an_insecure_parent_and_an_over_cap_request() {
    let directory = private_directory();
    let file = private_file(&directory, "marker", b"ok");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        read_private_regular_file_bounded(&file, 2)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        read_private_regular_file_bounded(&file, MAX_PRIVATE_REGULAR_FILE_BYTES + 1)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[test]
fn bounded_private_read_rejects_links_and_non_regular_files() {
    let directory = private_directory();
    let target = private_file(&directory, "target", b"ok");
    let link = directory.path().join("link");
    symlink(&target, &link).unwrap();
    assert!(read_private_regular_file_bounded(&link, 2).is_err());
    let dangling = directory.path().join("dangling");
    symlink(directory.path().join("missing"), &dangling).unwrap();
    assert!(read_private_regular_file_bounded(&dangling, 2).is_err());
    assert!(read_private_regular_file_bounded(directory.path(), 2).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_private_read_rejects_fifo_without_blocking() {
    let directory = private_directory();
    let fifo = directory.path().join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(read_private_regular_file_bounded(&fifo, 2).is_err());
}
