//! Shared-library probing through the neutral facade.

use kernal_api::platform::dylib::Library;
use kernal_api::platform::executable::native_library_name;
use std::ffi::OsStr;

#[test]
fn a_missing_library_is_not_found_with_the_hosts_reason() {
    let name = native_library_name(OsStr::new("kernal-api-no-such-library"));
    let error = Library::open(&name).expect_err("absent");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(!error.to_string().is_empty());
}

#[test]
fn a_name_with_nul_is_refused_before_the_host_sees_it() {
    let error = Library::open("lib\0name").expect_err("NUL");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}
