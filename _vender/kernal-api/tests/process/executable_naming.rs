//! `platform::executable::file_name_os`: host executable spelling that keeps
//! native string representation and never doubles an extension.

use kernal_api::platform::executable::{file_name, file_name_os};
use std::ffi::OsStr;

#[test]
fn a_bare_name_gets_exactly_the_host_suffix() {
    assert_eq!(
        file_name_os(OsStr::new("tool")),
        OsStr::new(&file_name("tool"))
    );
}

#[test]
fn an_existing_extension_is_kept() {
    for name in ["tool.exe", "tool.cmd", "tool.custom"] {
        assert_eq!(file_name_os(OsStr::new(name)), OsStr::new(name));
    }
}

#[cfg(unix)]
#[test]
fn non_unicode_bytes_survive() {
    use std::os::unix::ffi::OsStrExt;
    let name = OsStr::from_bytes(b"tool-\xff");
    assert_eq!(file_name_os(name).as_bytes(), name.as_bytes());
}

#[cfg(windows)]
#[test]
fn unpaired_surrogates_survive() {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let raw = [0x0074, 0xd800];
    let name = std::ffi::OsString::from_wide(&raw);
    let mut expected = raw.to_vec();
    expected.extend(".exe".encode_utf16());
    assert_eq!(
        file_name_os(&name).encode_wide().collect::<Vec<_>>(),
        expected
    );
}

/// The `&str` spelling keeps its original always-append contract.
#[cfg(windows)]
#[test]
fn string_naming_still_appends() {
    assert_eq!(file_name("tool.custom"), "tool.custom.exe");
    assert_eq!(
        file_name_os(OsStr::new("tool.custom")),
        OsStr::new("tool.custom")
    );
}
