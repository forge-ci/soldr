#![cfg(feature = "fs")]
//! The in-place write seal's cross-host contract through the public facade
//! (zccache#1791): a sealed file refuses the owner's in-place writes while
//! `Permissions::readonly()` stays false and rename-over-replace works —
//! the combination rustc's `check_file_is_writeable` pre-check demands
//! before it renames its temp output over the existing path.

use std::io::Write as _;

use kernal_api::platform::fs::{deny_in_place_writes, in_place_writes_denied, set_readonly};

/// Assert write refusal for ordinary owners. Unix root bypasses DAC mode bits;
/// retain the seal/readonly/hardlink metadata assertions for that identity.
fn assert_owner_write_refused(path: &std::path::Path) {
    use kernal_api::platform::host::{current_process_privilege, PrivilegedIdentity};
    if current_process_privilege().expect("privilege lookup") == Some(PrivilegedIdentity::UnixRoot)
    {
        eprintln!("write-refusal assertion gated: Unix root bypasses DAC mode bits");
        return;
    }
    assert!(
        owner_write_refused(path),
        "ordinary owner write must be refused"
    );
}

/// Open the file for an in-place write and report whether the OS refused it.
fn owner_write_refused(path: &std::path::Path) -> bool {
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => false,
        Err(error) => error.kind() == std::io::ErrorKind::PermissionDenied,
    }
}

fn sealed_fixture(content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("blob.rmeta");
    std::fs::write(&path, content).expect("write fixture");
    (dir, path)
}

#[test]
fn a_sealed_file_refuses_owner_writes_but_readonly_stays_false() {
    let (_dir, path) = sealed_fixture(b"original");

    deny_in_place_writes(&path).expect("seal");

    assert!(
        !std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .readonly(),
        "`readonly()` must stay false so `check_file_is_writeable` callers pass",
    );
    assert!(
        in_place_writes_denied(&path).expect("probe"),
        "the seal must be observable",
    );
    assert_owner_write_refused(&path);
}

#[test]
fn rename_over_a_sealed_file_still_replaces_it() {
    let (dir, path) = sealed_fixture(b"original");

    deny_in_place_writes(&path).expect("seal");

    let replacement = dir.path().join("replacement");
    std::fs::write(&replacement, b"replaced").expect("write replacement");
    std::fs::rename(&replacement, &path).expect("rename-over must work on a sealed file");
    assert_eq!(
        std::fs::read(&path).expect("read replacement"),
        b"replaced",
        "the rename must have landed",
    );
}

#[test]
fn sealing_is_idempotent_and_fresh_files_start_unsealed() {
    let (_dir, path) = sealed_fixture(b"original");

    assert!(!in_place_writes_denied(&path).expect("probe on a fresh file"));
    deny_in_place_writes(&path).expect("first seal");
    deny_in_place_writes(&path).expect("second seal must be a no-op");
    assert!(in_place_writes_denied(&path).expect("probe"));
}

#[test]
fn allow_restores_in_place_writes() {
    let (_dir, path) = sealed_fixture(b"original");

    deny_in_place_writes(&path).expect("seal");
    kernal_api::platform::fs::allow_in_place_writes(&path).expect("unseal");

    assert!(!in_place_writes_denied(&path).expect("probe"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("an unsealed file must be writable again");
    file.write_all(b"rewritten").expect("in-place write");
}

#[test]
fn a_legacy_readonly_file_upgrades_to_the_seal() {
    let (_dir, path) = sealed_fixture(b"original");

    // The pre-seal mechanism: every write bit cleared / the READONLY
    // attribute set. This is exactly the state that made rustc's pre-check
    // fail (zccache#1791).
    set_readonly(&path, true).expect("legacy seal");
    assert_owner_write_refused(&path);

    deny_in_place_writes(&path).expect("upgrade");

    assert!(
        !std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .readonly(),
        "the upgrade must clear the legacy mechanism so pre-checks pass",
    );
    assert!(in_place_writes_denied(&path).expect("probe"));
    assert_owner_write_refused(&path);
}

#[test]
fn the_seal_rides_the_file_record_across_hardlinks() {
    let (dir, path) = sealed_fixture(b"original");
    let sibling = dir.path().join("sibling.rmeta");
    std::fs::hard_link(&path, &sibling).expect("hardlink");

    deny_in_place_writes(&path).expect("seal through one name");

    assert!(in_place_writes_denied(&sibling).expect("seal through hardlink"));
    assert_owner_write_refused(&sibling);
    assert!(
        !std::fs::metadata(&sibling)
            .expect("metadata")
            .permissions()
            .readonly(),
        "and `readonly()` must be false through the sibling too",
    );
}

#[test]
fn sealing_a_missing_path_is_an_error() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let missing = dir.path().join("absent");
    let error = deny_in_place_writes(&missing).expect_err("missing path must error");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}
