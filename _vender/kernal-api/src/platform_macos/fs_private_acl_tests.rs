//! Native tests for the held-file ACL query.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::Duration;

fn grant_acl(path: &Path, mode: &str, acl: &str) {
    let spec = crate::SpawnSpec::new("/bin/chmod")
        .arg(mode)
        .arg(acl)
        .arg(path.as_os_str());
    let output = crate::run_bounded_command(spec, Duration::from_secs(5), 8192)
        .expect("chmod must finish within the deadline");
    assert!(output.exit.is_success(), "chmod did not add the ACL");
}

#[test]
fn open_file_acl_rejects_grants_and_stays_bound_after_replacement() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let original_path = directory.path().join("artifact");
    let original = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&original_path)
        .expect("create original");
    super::ensure_no_extended_acl(&original).expect("original without ACL");

    std::fs::rename(&original_path, directory.path().join("moved")).expect("rename original");
    let replacement = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&original_path)
        .expect("create replacement");
    grant_acl(&original_path, "+a", "everyone allow read");
    super::ensure_no_extended_acl(&original).expect("original handle remains ACL-free");
    assert_eq!(
        super::ensure_no_extended_acl(&replacement)
            .expect_err("replacement ACL must be rejected")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[test]
fn open_file_acl_rejects_inherited_grants() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let child = directory.path().join("child");
    std::fs::create_dir(&child).expect("create child");
    // Test the held-file validator, not automatic inheritance on the backing
    // filesystem (Recovery's scratch filesystem does not propagate this ACL).
    // Darwin chmod +ai explicitly installs an inherited entry.
    grant_acl(&child, "+ai", "everyone allow list,search");
    let held = File::open(&child).expect("open child");
    assert_eq!(
        super::ensure_no_extended_acl(&held)
            .expect_err("inherited ACL must be rejected")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
}
