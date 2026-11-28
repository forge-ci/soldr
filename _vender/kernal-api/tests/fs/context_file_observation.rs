#![cfg(feature = "fs")]

use std::fs;
use std::io::ErrorKind;

#[cfg(unix)]
use kernal_api::platform::fs::{
    canonical_context_path, context_path_metadata_no_follow, read_context_link,
};
use kernal_api::platform::fs::{
    read_context_regular_file_bounded, ContextPathKind, MAX_CONTEXT_REGULAR_FILE_BYTES,
};

#[test]
fn bounded_context_read_accepts_ordinary_empty_exact_and_binary_files() {
    let directory = tempfile::tempdir().unwrap();
    let empty = directory.path().join("empty");
    fs::write(&empty, []).unwrap();
    assert_eq!(
        read_context_regular_file_bounded(&empty, 0).unwrap().bytes,
        b""
    );

    let exact = directory.path().join("exact");
    fs::write(&exact, b"abc").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&exact, fs::Permissions::from_mode(0o644)).unwrap();
    }
    let observation = read_context_regular_file_bounded(&exact, 3).unwrap();
    assert_eq!(observation.bytes, b"abc");
    assert_eq!(observation.metadata.kind, ContextPathKind::RegularFile);
    assert_eq!(observation.metadata.len, Some(3));
    assert!(observation.metadata.identity.is_some());

    let binary = directory.path().join("binary");
    fs::write(&binary, [0, 0xff, b'\n']).unwrap();
    assert_eq!(
        read_context_regular_file_bounded(&binary, 3).unwrap().bytes,
        [0, 0xff, b'\n']
    );
}

#[cfg(unix)]
#[test]
fn bounded_context_read_reports_unreadable_input_when_host_enforces_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().unwrap();
    let unreadable = directory.path().join("unreadable");
    fs::write(&unreadable, b"no").unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    let result = read_context_regular_file_bounded(&unreadable, 2);
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
    match result {
        Ok(_) => assert_eq!(
            unsafe { libc::geteuid() },
            0,
            "a non-root process must not read a mode-000 file",
        ),
        Err(error) => assert_eq!(error.kind(), ErrorKind::PermissionDenied),
    }
}

#[test]
fn bounded_context_read_refuses_oversize_directory_and_missing_input() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("oversized");
    fs::write(&file, b"abcd").unwrap();
    assert_eq!(
        read_context_regular_file_bounded(&file, 3)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(
        read_context_regular_file_bounded(directory.path(), 3)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        read_context_regular_file_bounded(&directory.path().join("missing"), 3)
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        read_context_regular_file_bounded(&file, MAX_CONTEXT_REGULAR_FILE_BYTES + 1)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
}

#[cfg(unix)]
#[test]
fn context_metadata_and_link_read_do_not_follow_final_symlinks() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target");
    fs::write(&target, b"ok").unwrap();
    let link = directory.path().join("link");
    symlink("target", &link).unwrap();
    assert_eq!(
        context_path_metadata_no_follow(&link).unwrap().kind,
        ContextPathKind::Symlink
    );
    assert_eq!(
        read_context_link(&link).unwrap(),
        std::path::PathBuf::from("target")
    );
    assert_eq!(
        read_context_regular_file_bounded(&link, 2)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    // Canonicalize both sides: macOS resolves its temp directory through the
    // `/var` -> `/private/var` symlink, so `target` as constructed and `target`
    // as canonicalized differ in spelling while denoting the same file. The
    // property under test is that the link resolves to the target, not how the
    // platform spells the path on the way there.
    assert_eq!(
        canonical_context_path(&link).unwrap(),
        target.canonicalize().unwrap()
    );

    let dangling = directory.path().join("dangling");
    symlink("absent", &dangling).unwrap();
    assert_eq!(
        context_path_metadata_no_follow(&dangling).unwrap().kind,
        ContextPathKind::Symlink
    );
    assert_eq!(
        read_context_link(&dangling).unwrap(),
        std::path::PathBuf::from("absent")
    );
    assert_eq!(
        canonical_context_path(&dangling).unwrap_err().kind(),
        ErrorKind::NotFound
    );

    let directory_link = directory.path().join("directory-link");
    symlink(directory.path(), &directory_link).unwrap();
    assert_eq!(
        read_context_regular_file_bounded(&directory_link, 2)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
}

#[cfg(unix)]
#[test]
fn bounded_context_read_refuses_fifo_without_blocking() {
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o644) }, 0);
    assert_eq!(
        context_path_metadata_no_follow(&fifo).unwrap().kind,
        ContextPathKind::Other
    );
    let (sender, receiver) = std::sync::mpsc::channel();
    let fifo_for_read = fifo.clone();
    std::thread::spawn(move || {
        sender
            .send(read_context_regular_file_bounded(&fifo_for_read, 2).map(|_| ()))
            .unwrap()
    });
    assert_eq!(
        receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_context_read_refuses_synthetic_regular_file_with_incoherent_length() {
    // procfs reports cmdline as a regular file with zero metadata length while
    // yielding bytes. A successful receipt must never claim that mismatch is
    // a coherent regular-file observation.
    assert_eq!(
        read_context_regular_file_bounded(std::path::Path::new("/proc/self/cmdline"), 4096)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
}

#[test]
fn context_metadata_reports_whether_a_regular_file_is_executable() {
    use kernal_api::platform::fs::{context_path_metadata_no_follow, make_executable};

    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("tool");
    fs::write(&file, b"#!/bin/sh\n").unwrap();
    assert!(!context_path_metadata_no_follow(&file).unwrap().executable);
    assert!(
        !read_context_regular_file_bounded(&file, 16)
            .unwrap()
            .metadata
            .executable
    );

    make_executable(&file).unwrap();
    // Windows has no per-file execute bit, so nothing there is executable.
    let expected = !kernal_api::platform::host::target_is_windows();
    assert_eq!(
        context_path_metadata_no_follow(&file).unwrap().executable,
        expected
    );
    assert_eq!(
        read_context_regular_file_bounded(&file, 16)
            .unwrap()
            .metadata
            .executable,
        expected
    );
    assert!(
        !context_path_metadata_no_follow(directory.path())
            .unwrap()
            .executable
    );
}

#[test]
fn an_owner_executable_private_file_stays_private() {
    use kernal_api::platform::fs::{
        context_path_metadata_no_follow, create_private_file, make_owner_executable,
        read_private_regular_file_bounded,
    };
    use std::io::Write as _;

    let windows = kernal_api::platform::host::target_is_windows();
    let directory = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let file = directory.path().join("tool");
    create_private_file(&file)
        .unwrap()
        .write_all(b"#!/bin/sh\n")
        .unwrap();
    make_owner_executable(&file).unwrap();
    assert_eq!(
        context_path_metadata_no_follow(&file).unwrap().executable,
        !windows
    );
    // Windows has no execute bit to add, and its private reads also require a
    // protected-DACL parent that this temporary directory does not have.
    if !windows {
        assert_eq!(
            read_private_regular_file_bounded(&file, 16).unwrap(),
            b"#!/bin/sh\n"
        );
    }
}
