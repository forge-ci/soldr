//! fbuild links this crate into manylinux_2_17 binaries and wheels (#395).
//!
//! A libc wrapper newer than glibc 2.17 breaks fbuild's release link, or its
//! wheel at import time on an older distro, even when the kernel call itself
//! is older. Call these through `libc::syscall(libc::SYS_…)` with a runtime
//! fallback instead (manylinux2014's kernel floor is Linux 3.10). The
//! link-time `objdump -T` check that #395 also asks for belongs in CI.

use std::path::Path;

use super::facade_policy::rust_sources;

/// Wrappers newer than glibc 2.17, with the release that added each.
const NEWER_THAN_FLOOR: &[(&str, &str)] = &[
    ("memfd_create", "2.27"),
    ("getrandom", "2.25"),
    ("copy_file_range", "2.27"),
    ("close_range", "2.34"),
    ("pidfd_open", "2.36"),
    ("pidfd_send_signal", "2.36"),
    ("pidfd_getfd", "2.36"),
    ("statx", "2.28"),
    ("renameat2", "2.28"),
    ("gettid", "2.30"),
];

fn references_wrapper(line: &str, name: &str) -> bool {
    let token = format!("libc::{name}");
    line.match_indices(&token).any(|(at, _)| {
        let suffix = &line[at + token.len()..];
        // statx also names a struct. A type argument does not link its wrapper.
        let statx_type = name == "statx"
            && line[..at].trim_end().ends_with("::<")
            && suffix.trim_start().starts_with('>');
        !statx_type && !suffix.starts_with(|next: char| next.is_ascii_alphanumeric() || next == '_')
    })
}

#[test]
fn wrapper_references_include_function_values_and_respect_identifier_boundaries() {
    for line in [
        "unsafe { libc::statx(fd, name, flags, mask, buf) }",
        "unsafe { libc::statx (fd, name, flags, mask, buf) }",
        "let wrapper = libc::statx;",
        "consume(libc::statx);",
    ] {
        assert!(references_wrapper(line, "statx"), "{line}");
    }
    for line in [
        "libc::statx_extra()",
        "libc::statx2()",
        "libc::syscall(libc::SYS_statx, fd, name, flags, mask, buf)",
    ] {
        assert!(!references_wrapper(line, "statx"), "{line}");
    }
}

#[test]
fn statx_layout_type_does_not_require_a_libc_wrapper() {
    for line in ["size_of::<libc::statx>()", "size_of::< libc::statx >()"] {
        assert!(!references_wrapper(line, "statx"), "{line}");
    }
    assert!(references_wrapper(
        "size_of::<libc::statx>(); libc::statx(fd, name, flags, mask, buf);",
        "statx"
    ));
}

#[test]
fn no_libc_wrapper_newer_than_the_glibc_2_17_floor() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for path in rust_sources(&root) {
        let text = std::fs::read_to_string(&path).expect("read source");
        for (line_number, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for (name, glibc) in NEWER_THAN_FLOOR {
                if references_wrapper(line, name) {
                    found.push(format!(
                        "{}:{}: libc::{name} needs glibc {glibc}",
                        path.display(),
                        line_number + 1
                    ));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "use libc::syscall(libc::SYS_…) with a fallback instead:\n{}",
        found.join("\n")
    );
}
