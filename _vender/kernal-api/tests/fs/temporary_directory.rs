#![cfg(feature = "fs")]

use kernal_api::platform::fs::TemporaryDirectory;

// Issue #182: generic ownership belongs in the kernel, not cache policy.
#[test]
fn owned_directory_cleanup_and_transfer() {
    let parent = TemporaryDirectory::new().unwrap();
    let child = TemporaryDirectory::in_directory(parent.path(), "stage-").unwrap();
    let child_path = child.path().to_path_buf();
    assert_eq!(child_path.parent(), Some(parent.path()));
    std::fs::write(child_path.join("payload"), b"data").unwrap();
    drop(child);
    assert!(!child_path.exists());

    let retained = TemporaryDirectory::in_directory(parent.path(), "keep-").unwrap();
    let retained_path = retained.persist();
    assert!(retained_path.is_dir());
    parent.close().unwrap();
    assert!(!retained_path.exists());
}

#[test]
fn invalid_prefixes_create_nothing() {
    let parent = TemporaryDirectory::new().unwrap();
    for prefix in [
        "../escape",
        "/absolute",
        "a/b",
        "a\\b",
        "..",
        ".",
        "bad\0name",
        "C:",
    ] {
        assert_eq!(
            TemporaryDirectory::in_directory(parent.path(), prefix)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    assert!(TemporaryDirectory::in_directory(parent.path(), &"x".repeat(129)).is_err());
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
}

#[test]
fn creation_never_reuses_a_live_directory_and_missing_parent_fails() {
    let parent = TemporaryDirectory::new().unwrap();
    let first = TemporaryDirectory::in_directory(parent.path(), "stage-").unwrap();
    let second = TemporaryDirectory::in_directory(parent.path(), "stage-").unwrap();
    assert_ne!(first.path(), second.path());
    assert!(first.path().is_absolute());
    assert_eq!(
        TemporaryDirectory::in_directory(&parent.path().join("missing"), "stage-")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn relative_parent_cleanup_survives_working_directory_change() {
    const PROBE: &str = "KERNAL_TEMP_CWD_PROBE";
    if std::env::var_os(PROBE).is_some() {
        let owned = TemporaryDirectory::in_directory(std::path::Path::new("."), "cwd-").unwrap();
        let path = owned.path().to_path_buf();
        assert!(path.is_absolute());
        std::env::set_current_dir("..").unwrap();
        owned.close().unwrap();
        assert!(!path.exists());
        return;
    }
    // Isolate process-global cwd changes from concurrently running tests.
    let parent = TemporaryDirectory::new().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "temporary_directory::relative_parent_cleanup_survives_working_directory_change",
        ])
        .env(PROBE, "1")
        .current_dir(parent.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn private_permissions_and_cleanup_do_not_follow_child_symlinks() {
    use std::os::unix::{fs::symlink, fs::PermissionsExt};
    let target = TemporaryDirectory::new().unwrap();
    std::fs::write(target.path().join("keep"), b"keep").unwrap();
    let owned = TemporaryDirectory::new().unwrap();
    assert_eq!(
        std::fs::metadata(owned.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    symlink(target.path(), owned.path().join("link")).unwrap();
    owned.close().unwrap();
    assert_eq!(std::fs::read(target.path().join("keep")).unwrap(), b"keep");
}

#[cfg(windows)]
#[test]
fn explicit_cleanup_reports_an_exclusively_open_file() {
    use std::os::windows::fs::OpenOptionsExt;
    let owned = TemporaryDirectory::new().unwrap();
    let path = owned.path().to_path_buf();
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .share_mode(0)
        .open(path.join("held"))
        .unwrap();
    let result = owned.close();
    drop(file);
    // Recover our exact test-owned directory after releasing the Windows handle.
    if path.exists() {
        std::fs::remove_dir_all(&path).unwrap();
    }
    assert!(result.is_err());
}
