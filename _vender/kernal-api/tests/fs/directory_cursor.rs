#![cfg(feature = "fs")]

use std::fs;
use std::io::ErrorKind;

use kernal_api::platform::fs::{ContextPathKind, DirectoryCursor};

fn entries(cursor: &mut DirectoryCursor) -> Vec<kernal_api::platform::fs::DirectoryCursorEntry> {
    let mut entries = Vec::new();
    while let Some(entry) = cursor.next_entry().expect("enumerate directory") {
        entries.push(entry);
    }
    entries
}

#[test]
fn cursor_yields_owned_hidden_and_ordinary_entries_without_descending() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("plain"), b"x").unwrap();
    fs::write(root.path().join(".hidden"), b"x").unwrap();
    fs::create_dir(root.path().join("child")).unwrap();
    fs::write(root.path().join("child").join("nested"), b"x").unwrap();

    let mut cursor = DirectoryCursor::open(root.path()).unwrap();
    let entries = entries(&mut cursor);
    assert_eq!(entries.len(), 3);
    assert!(entries.iter().any(|entry| {
        entry.file_name() == "plain"
            && entry.path() == root.path().join("plain")
            && entry.kind() == ContextPathKind::RegularFile
    }));
    assert!(entries.iter().any(|entry| entry.file_name() == ".hidden"));
    assert!(entries.iter().any(|entry| {
        entry.file_name() == "child" && entry.kind() == ContextPathKind::Directory
    }));
    assert!(entries.iter().all(|entry| entry.file_name() != "nested"));
}

#[cfg(unix)]
#[test]
fn cursor_reports_links_inertly_including_dangling_links() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    symlink("missing-target", root.path().join("dangling")).unwrap();
    symlink(root.path(), root.path().join("directory-link")).unwrap();

    let mut cursor = DirectoryCursor::open(root.path()).unwrap();
    let entries = entries(&mut cursor);
    assert_eq!(entries.len(), 2);
    assert!(entries
        .iter()
        .all(|entry| entry.kind() == ContextPathKind::Symlink));
}

#[test]
fn cursor_reports_open_and_entry_errors() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(
        DirectoryCursor::open(root.path().join("missing"))
            .err()
            .expect("missing directory must fail")
            .kind(),
        ErrorKind::NotFound
    );
    let file = root.path().join("file");
    fs::write(&file, b"x").unwrap();
    assert_eq!(
        DirectoryCursor::open(&file)
            .err()
            .expect("regular file is not a directory")
            .kind(),
        ErrorKind::NotADirectory
    );
}

#[test]
fn empty_cursor_stays_at_end_of_directory() {
    let root = tempfile::tempdir().unwrap();
    let mut cursor = DirectoryCursor::open(root.path()).unwrap();
    assert!(cursor.next_entry().unwrap().is_none());
    assert!(cursor.next_entry().unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn cursor_reports_unreadable_directory_when_host_enforces_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let unreadable = root.path().join("unreadable");
    fs::create_dir(&unreadable).unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    let result = DirectoryCursor::open(&unreadable);
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    assert_eq!(
        result
            .err()
            .expect("non-privileged caller must not enumerate unreadable directory")
            .kind(),
        ErrorKind::PermissionDenied
    );
}

#[test]
fn dropping_cursor_stops_early_and_releases_the_directory() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("first"), b"x").unwrap();
    fs::write(root.path().join("second"), b"x").unwrap();

    let mut cursor = DirectoryCursor::open(root.path()).unwrap();
    assert!(cursor.next_entry().unwrap().is_some());
    drop(cursor);

    // A new bounded consumer can immediately obtain its own handle; it need
    // not drain the first cursor before that handle is released.
    let mut next_consumer = DirectoryCursor::open(root.path()).unwrap();
    assert!(next_consumer.next_entry().unwrap().is_some());
}

#[cfg(target_os = "linux")]
fn open_directory_fd_count(path: &std::path::Path) -> usize {
    fs::read_dir("/proc/self/fd")
        .unwrap()
        .flatten()
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .filter(|target| target == path)
        .count()
}

#[cfg(target_os = "linux")]
#[test]
fn retained_facade_entry_does_not_retain_the_native_directory_handle() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("entry"), b"x").unwrap();
    let before = open_directory_fd_count(root.path());

    let mut cursor = DirectoryCursor::open(root.path()).unwrap();
    assert_eq!(open_directory_fd_count(root.path()), before + 1);
    let entry = cursor.next_entry().unwrap().unwrap();
    assert_eq!(open_directory_fd_count(root.path()), before + 1);

    drop(cursor);
    assert_eq!(open_directory_fd_count(root.path()), before);
    assert_eq!(entry.file_name(), "entry");
    assert_eq!(open_directory_fd_count(root.path()), before);
}
