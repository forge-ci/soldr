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
/// Returns the copied file names.
pub fn install_into(host_libs: &Path, llvm_lib: &Path) -> Result<Vec<String>, SoldrError> {
    std::fs::create_dir_all(llvm_lib)?;
    let mut copied = Vec::new();
    for entry in std::fs::read_dir(host_libs.join("lib"))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_file() || !name.contains(".so") {
            continue;
        }
        let dest = llvm_lib.join(&name);
        let staged = llvm_lib.join(format!(".{name}.soldr-tmp"));
        std::fs::copy(entry.path(), &staged)?;
        std::fs::rename(&staged, &dest)?;
        copied.push(name);
    }
    Ok(copied)
}

/// The `DT_NEEDED` entries of an ELF64 little-endian file.
///
/// Reads `PT_DYNAMIC`, then resolves `DT_STRTAB` (a virtual address)
/// through the `PT_LOAD` segments. Enough for the verification below; not a
/// general ELF reader.
pub fn elf_needed(path: &Path) -> Result<Vec<String>, SoldrError> {
    let bytes = std::fs::read(path)?;
    parse_needed(&bytes).ok_or_else(|| {
        SoldrError::Archive(format!(
            "{} is not a readable ELF64 little-endian dynamic object",
            path.display()
        ))
    })
}

fn parse_needed(b: &[u8]) -> Option<Vec<String>> {
    if b.get(..4)? != b"\x7fELF" || *b.get(4)? != 2 || *b.get(5)? != 1 {
        return None;
    }
    let u16_at = |o: usize| Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?));
    let u64_at = |o: usize| Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?));
    let phoff = usize::try_from(u64_at(0x20)?).ok()?;
    let phentsize = usize::from(u16_at(0x36)?);
    let phnum = usize::from(u16_at(0x38)?);
    let mut loads = Vec::new();
    let mut dynamic = None;
    for i in 0..phnum {
        let ph = phoff.checked_add(i.checked_mul(phentsize)?)?;
        let kind = u32_at(ph)?;
        let offset = u64_at(ph + 8)?;
        let vaddr = u64_at(ph + 16)?;
        let filesz = u64_at(ph + 32)?;
        match kind {
            1 => loads.push((vaddr, offset, filesz)),
            2 => dynamic = Some((offset, filesz)),
            _ => {}
        }
    }
    let (dyn_off, dyn_size) = dynamic?;
    let (mut strtab, mut needed) = (None, Vec::new());
    let mut at = usize::try_from(dyn_off).ok()?;
    let end = at.checked_add(usize::try_from(dyn_size).ok()?)?;
    while at + 16 <= end {
        let (tag, val) = (u64_at(at)?, u64_at(at + 8)?);
        match tag {
            0 => break,
            1 => needed.push(val),
            5 => strtab = Some(val),
            _ => {}
        }
        at += 16;
    }
    let strtab = strtab?;
    let (vaddr, offset, _) = loads
        .iter()
        .find(|(v, _, size)| strtab >= *v && strtab < v + size)?;
    let str_base = usize::try_from(strtab - vaddr + offset).ok()?;
    needed
        .into_iter()
        .map(|rel| {
            let start = str_base.checked_add(usize::try_from(rel).ok()?)?;
            let len = b.get(start..)?.iter().position(|c| *c == 0)?;
            String::from_utf8(b[start..start + len].to_vec()).ok()
        })
        .collect()
}

/// NEEDED entries that are neither glibc core nor present in `lib_dir`.
pub fn unresolved_needed(needed: &[String], lib_dir: &Path) -> Vec<String> {
    needed
        .iter()
        .filter(|name| !GLIBC_CORE.contains(&name.as_str()) && !lib_dir.join(name).is_file())
        .cloned()
        .collect()
}

/// Fail when the bundle's `lld` still needs a library the bundle lacks.
pub fn verify_lld_self_contained(bin_dir: &Path) -> Result<(), SoldrError> {
    let lld = bin_dir.join("lld");
    let lib_dir = bin_dir.parent().unwrap_or(bin_dir).join("lib");
    let missing = unresolved_needed(&elf_needed(&lld)?, &lib_dir);
    if missing.is_empty() {
        Ok(())
    } else {
        Err(SoldrError::Archive(format!(
            "managed LLVM {} still needs host libraries {missing:?}; \
             expected them in {}",
            lld.display(),
            lib_dir.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runtime host check: cfg-based host selection is reserved for the
    /// soldr-platform boundary.
    fn linux_gnu_host() -> bool {
        use crate::platform::host::facts::{libc, os, HostLibc, HostOs};
        os() == HostOs::Linux && libc() == HostLibc::Gnu
    }

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
    #[test]
    fn elf_needed_reads_a_real_binary() {
        if !linux_gnu_host() {
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let needed = elf_needed(&exe).unwrap();
        assert!(
            needed.iter().any(|n| n == "libc.so.6"),
            "expected libc.so.6 in {needed:?}"
        );
    }

    /// End to end on a real ELF standing in for `lld`: a NEEDED the bundle
    /// lacks fails verification by name, and providing it in `lib/` passes.
    #[test]
    fn verify_requires_every_non_glibc_needed_in_the_bundle_lib() {
        if !linux_gnu_host() {
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let extra: Vec<String> =
            unresolved_needed(&elf_needed(&exe).unwrap(), Path::new("/nonexistent"));
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("hardlinked/bin");
        let lib = root.path().join("hardlinked/lib");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::copy(&exe, bin.join("lld")).unwrap();
        if !extra.is_empty() {
            let err = verify_lld_self_contained(&bin).expect_err("missing libs must fail");
            assert!(err.to_string().contains(&extra[0]), "{err}");
        }
        for name in &extra {
            std::fs::write(lib.join(name), b"").unwrap();
        }
        verify_lld_self_contained(&bin).expect("bundle now provides every non-glibc NEEDED");
    }

    #[test]
    fn elf_needed_rejects_a_non_elf_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("not-elf");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        assert!(elf_needed(&path).is_err());
    }
}
