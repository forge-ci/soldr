#![cfg(feature = "fs")]

use std::io;

/// A held-file ACL check is separate from path/owner/mode verification.
#[test]
fn held_file_acl_check_reports_absence_or_explicit_unsupported() {
    let file = tempfile::tempfile().expect("temporary file");
    match kernal_api::platform::fs::ensure_no_extended_acl(&file) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {}
        Err(error) => panic!("unexpected ACL query failure: {error}"),
    }
}

#[test]
fn acl_query_stays_bound_to_open_handle_after_rename() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let file = tempfile::NamedTempFile::new_in(directory.path()).expect("temporary file");
    match kernal_api::platform::fs::ensure_no_extended_acl(file.as_file()) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::Unsupported => return,
        Err(error) => panic!("unexpected initial ACL query failure: {error}"),
    }
    std::fs::rename(file.path(), directory.path().join("renamed")).expect("rename the open file");
    kernal_api::platform::fs::ensure_no_extended_acl(file.as_file())
        .expect("the same held file remains ACL-free after rename");
}
