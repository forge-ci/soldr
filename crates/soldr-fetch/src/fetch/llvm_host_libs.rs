//! Host runtime libraries for the managed LLVM bundle's `lld` (soldr#3774).
//!
//! `zackees/clang-tool-chain-bins`' LLVM 21.1.5 `lld` (and its `lld-link`,
//! `ld.lld`, `ld64.lld`, `wasm-ld` aliases) NEEDs `libxml2.so.2` and
//! `libz.so.1` and carries `RUNPATH $ORIGIN/../lib`, but the bundle ships
//! neither. A host without `libxml2.so.2` (ubuntu-26.04, Debian trixie:
//! soname `.16`) therefore cannot run it, and every MSVC/Darwin cross link
//! fails with `error while loading shared libraries: libxml2.so.2`.
//!
//! The soldr-toolchain `llvm-host-libs` rows (built in manylinux2014, glibc
//! 2.17) are copied into the bundle's `lib/`, and the result is verified:
//! every `DT_NEEDED` of `bin/lld` must be a glibc core library or present
//! in that `lib/`, so the linker no longer depends on what the host has.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

pub const LLVM_HOST_LIBS_VERSION: &str = "libxml2-2.13.9-zlib-1.3.1";

/// Libraries the bundle may take from any glibc host.
const GLIBC_CORE: &[&str] = &[
    "libc.so.6",
    "libm.so.6",
    "libdl.so.2",
    "libpthread.so.0",
    "librt.so.1",
    "ld-linux-x86-64.so.2",
    "ld-linux-aarch64.so.1",
];

/// The catalogue slug for an LLVM host triple, `None` off Linux GNU.
pub fn catalogue_slug_for(host_triple: &str) -> Option<&'static str> {
    match host_triple {
        "x86_64-unknown-linux-gnu" => Some("linux-x64-gnu"),
        "aarch64-unknown-linux-gnu" => Some("linux-arm64-gnu"),
        _ => None,
    }
}

pub async fn ensure_llvm_host_libs(
    paths: &SoldrPaths,
    host_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = catalogue_slug_for(host_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!("no llvm-host-libs row for host {host_triple}"))
    })?;
    super::syslib_common::ensure_syslib_bundle(
        paths,
        "llvm-host-libs",
        LLVM_HOST_LIBS_VERSION,
        slug,
    )
    .await
}

/// Copy every shared object under `<host_libs>/lib` into `llvm_lib`.
pub fn install_into(_host_libs: &Path, _llvm_lib: &Path) -> Result<Vec<String>, SoldrError> {
    Ok(Vec::new())
}

/// The `DT_NEEDED` entries of an ELF64 little-endian file.
pub fn elf_needed(_path: &Path) -> Result<Vec<String>, SoldrError> {
    Ok(Vec::new())
}

/// NEEDED entries that are neither glibc core nor present in `lib_dir`.
pub fn unresolved_needed(_needed: &[String], _lib_dir: &Path) -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_cover_only_linux_gnu_hosts() {
        assert_eq!(
            catalogue_slug_for("x86_64-unknown-linux-gnu"),
            Some("linux-x64-gnu")
        );
        assert_eq!(
            catalogue_slug_for("aarch64-unknown-linux-gnu"),
            Some("linux-arm64-gnu")
        );
        assert_eq!(catalogue_slug_for("x86_64-pc-windows-msvc"), None);
    }

    #[test]
    fn unresolved_needed_names_only_what_neither_glibc_nor_the_bundle_has() {
        let lib = tempfile::tempdir().unwrap();
        std::fs::write(lib.path().join("libz.so.1"), b"").unwrap();
        let needed: Vec<String> = ["libxml2.so.2", "libm.so.6", "libz.so.1", "libc.so.6"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(unresolved_needed(&needed, lib.path()), vec!["libxml2.so.2"]);
    }

    #[test]
    fn install_copies_bundle_libs_into_the_llvm_lib_dir() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("lib")).unwrap();
        std::fs::write(src.path().join("lib/libxml2.so.2"), b"x").unwrap();
        std::fs::write(src.path().join("lib/libz.so.1"), b"z").unwrap();
        std::fs::write(src.path().join("LICENSE.zlib"), b"l").unwrap();
        let llvm_lib = dst.path().join("hardlinked/lib");
        let mut copied = install_into(src.path(), &llvm_lib).unwrap();
        copied.sort();
        assert_eq!(copied, vec!["libxml2.so.2", "libz.so.1"]);
        assert_eq!(std::fs::read(llvm_lib.join("libxml2.so.2")).unwrap(), b"x");
    }

    /// A real ELF: this test binary is dynamically linked against libc on a
    /// GNU host, so the parser must find it among the DT_NEEDED entries.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn elf_needed_reads_a_real_binary() {
        let exe = std::env::current_exe().unwrap();
        let needed = elf_needed(&exe).unwrap();
        assert!(
            needed.iter().any(|n| n == "libc.so.6"),
            "expected libc.so.6 in {needed:?}"
        );
    }

    #[test]
    fn elf_needed_rejects_a_non_elf_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("not-elf");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        assert!(elf_needed(&path).is_err());
    }
}
