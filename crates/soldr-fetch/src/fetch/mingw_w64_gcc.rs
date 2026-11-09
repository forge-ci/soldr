//! Managed MinGW-w64 GCC bundle for Windows GNU Rust targets.
//!
//! The first supported target is `x86_64-pc-windows-gnu` on Windows
//! x64 hosts. The bundle is produced by `zackees/soldr-toolchain`
//! from the WinLibs standalone MinGW-w64 GCC zip and exposes a
//! relocatable package root with `bin/gcc.exe`, `bin/g++.exe`,
//! binutils, headers, import libraries, and runtime DLLs.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

pub const MANAGED_MINGW_W64_GCC_VERSION: &str = "15.3.0posix-14.0.0-msvcrt-r1";
pub const MINGW_W64_GCC_TOOL: &str = "mingw-w64-gcc";
pub const MINGW_W64_GCC_TARGET: &str = "x86_64-pc-windows-gnu";
pub const MINGW_W64_GCC_SLUG: &str = "windows-x64-gnu";

/// Host-neutral MinGW-w64 sysroot (soldr-toolchain#114 / soldr#2336):
/// headers, import libraries, CRT startup objects, and the gcc runtime,
/// with **no** host executables. Unlike [`MINGW_W64_GCC_TOOL`] (Windows
/// `.exe` toolchain), this bundle is consumable from any host, so a
/// non-Windows host can materialize the win-gnu link inputs. Pinned to
/// the same WinLibs release + slug as the compiler bundle so CRT/gcc/
/// mingw versions agree.
pub const MINGW_W64_SYSROOT_TOOL: &str = "mingw-w64-sysroot";

/// Linux-hosted MinGW-w64 **cross** gcc toolchain (soldr-toolchain#114
/// Phase 2 / soldr#2336). A relocatable `x86_64-w64-mingw32-{gcc,g++,ar,
/// ranlib,dlltool,windres,ld}` toolchain built from conda-forge, so a
/// Linux host can compile + link win-gnu with a real gcc driver (the
/// maintainer-chosen path — gcc kept, not zig). Published under the HOST
/// platform slug (where the toolchain runs), like `gnu-linux-toolchain`.
pub const MINGW_W64_CROSS_TOOL: &str = "mingw-w64-cross";
pub const MINGW_W64_CROSS_VERSION: &str = "mingw-w64-gcc-15.3.0";
pub const MINGW_W64_CROSS_HOST_SLUG: &str = "linux-x64-gnu";
/// Cross-tool basename prefix inside the bundle (`bin/<prefix>-gcc`).
pub const MINGW_W64_CROSS_PREFIX: &str = "x86_64-w64-mingw32";

pub fn current_host_supports_mingw_w64_gcc() -> bool {
    crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
        && crate::platform::host::facts::arch() == crate::platform::host::facts::HostArch::X86_64
}

/// The Linux-hosted mingw cross toolchain is published for the
/// `linux-x64-gnu` host only (Phase 2). Other non-Windows hosts (macOS,
/// linux-arm) have no cross bundle yet.
pub fn current_host_supports_mingw_w64_cross() -> bool {
    crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Linux
        && crate::platform::host::facts::arch() == crate::platform::host::facts::HostArch::X86_64
}

/// Catalogue URL for the managed MinGW bundle.
pub fn mingw_w64_gcc_asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for(MINGW_W64_GCC_TOOL, version, slug)
}

pub fn slug_for_target(target_triple: &str) -> Option<&'static str> {
    match target_triple {
        MINGW_W64_GCC_TARGET => Some(MINGW_W64_GCC_SLUG),
        _ => None,
    }
}

pub async fn ensure_mingw_w64_gcc(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = slug_for_target(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "managed MinGW-w64 GCC is only available for {MINGW_W64_GCC_TARGET}; got {target_triple}"
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(
        paths,
        MINGW_W64_GCC_TOOL,
        MANAGED_MINGW_W64_GCC_VERSION,
        slug,
    )
    .await
}

/// Fetch the host-neutral MinGW-w64 sysroot bundle
/// (soldr-toolchain#114). Returns the bundle root; the target sysroot
/// lives under `package/x86_64-w64-mingw32/` with the gcc runtime under
/// `package/lib/gcc/x86_64-w64-mingw32/`. Usable from any host — this is
/// the fetch a non-Windows-host win-gnu branch consumes instead of the
/// Windows-only compiler bundle.
pub async fn ensure_mingw_w64_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = slug_for_target(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "managed MinGW-w64 sysroot is only available for {MINGW_W64_GCC_TARGET}; got {target_triple}"
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(
        paths,
        MINGW_W64_SYSROOT_TOOL,
        MANAGED_MINGW_W64_GCC_VERSION,
        slug,
    )
    .await
}

/// Catalogue URL for the host-neutral sysroot bundle.
pub fn mingw_w64_sysroot_asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for(MINGW_W64_SYSROOT_TOOL, version, slug)
}

/// Fetch the Linux-hosted mingw cross gcc toolchain
/// (soldr-toolchain#114 Phase 2). Returns the bundle root; the cross
/// tools live under `bin/x86_64-w64-mingw32-*` and the toolchain carries
/// its own mingw sysroot, so the gcc driver resolves headers/CRT itself.
pub async fn ensure_mingw_w64_cross(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    super::syslib_common::ensure_syslib_bundle(
        paths,
        MINGW_W64_CROSS_TOOL,
        MINGW_W64_CROSS_VERSION,
        MINGW_W64_CROSS_HOST_SLUG,
    )
    .await
}

/// Catalogue URL for the Linux-hosted mingw cross toolchain bundle.
pub fn mingw_w64_cross_asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for(MINGW_W64_CROSS_TOOL, version, slug)
}

/// Path to a cross tool inside a materialized cross-toolchain bundle:
/// `bin/x86_64-w64-mingw32-<tool>`. The host is Linux x64 (gated by
/// [`current_host_supports_mingw_w64_cross`]), so no `.exe` suffix.
pub fn cross_tool_path(bundle_root: &Path, tool: &str) -> PathBuf {
    bin_dir(bundle_root).join(format!("{MINGW_W64_CROSS_PREFIX}-{tool}"))
}

/// Target-scoped env for a Linux-hosted win-gnu build driven by the
/// mingw **cross** toolchain. Same cc-rs / cargo keys as
/// [`env_for_target`], but pointing at the `x86_64-w64-mingw32-*`
/// cross drivers rather than a Windows-host `bin/gcc.exe`.
pub fn cross_env_for_target(bundle_root: &Path, target_triple: &str) -> Vec<(String, String)> {
    let target_u = target_triple.replace('-', "_");
    let target_u_upper = target_u.to_uppercase();

    let tool = |name: &str| {
        cross_tool_path(bundle_root, name)
            .to_string_lossy()
            .into_owned()
    };
    vec![
        (
            "MINGW_W64_CROSS_ROOT".to_string(),
            bundle_root.to_string_lossy().into_owned(),
        ),
        (
            "MINGW_W64_CROSS_BIN".to_string(),
            bin_dir(bundle_root).to_string_lossy().into_owned(),
        ),
        (format!("CC_{target_u}"), tool("gcc")),
        (format!("CXX_{target_u}"), tool("g++")),
        (format!("AR_{target_u}"), tool("ar")),
        (format!("RANLIB_{target_u}"), tool("ranlib")),
        (format!("WINDRES_{target_u}"), tool("windres")),
        (format!("DLLTOOL_{target_u}"), tool("dlltool")),
        (format!("CARGO_TARGET_{target_u_upper}_LINKER"), tool("gcc")),
    ]
}

/// Prepare the compiler/linker env for `soldr build --target
/// x86_64-pc-windows-gnu` (soldr#2336). Windows x64 host → managed
/// WinLibs gcc bundle; Linux x64 host → managed conda-forge mingw
/// **cross** toolchain (soldr-toolchain#114 Phase 2, PE-link-proven);
/// any other host → a clear error. Returns the `bin/` dir to prepend to
/// PATH plus the target-scoped cc-rs/cargo env pairs — so the blessed
/// build's host-branch stays a single call and this policy lives in the
/// fetch crate beside the bundles it selects.
pub async fn prepare_win_gnu_env(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<(PathBuf, Vec<(String, String)>), SoldrError> {
    if current_host_supports_mingw_w64_gcc() {
        let root = ensure_mingw_w64_gcc(paths, target_triple).await?;
        Ok((bin_dir(&root), env_for_target(&root, target_triple)))
    } else if current_host_supports_mingw_w64_cross() {
        let root = ensure_mingw_w64_cross(paths).await?;
        Ok((bin_dir(&root), cross_env_for_target(&root, target_triple)))
    } else {
        Err(SoldrError::UnsupportedPlatform(format!(
            "managed Windows GNU target {target_triple} requires a Windows x64 host \
             (WinLibs bundle) or a Linux x64 host (conda-forge mingw cross toolchain); \
             cargo-zigbuild is no longer used as the blessed Windows GNU fallback"
        )))
    }
}

/// The `x86_64-w64-mingw32/lib` directory inside a materialized sysroot
/// (or compiler) bundle root — where CRT startup objects (`crt2.o`,
/// `dllcrt2.o`) and import libraries live.
pub fn sysroot_lib_dir(bundle_root: &Path) -> PathBuf {
    bundle_root.join("x86_64-w64-mingw32").join("lib")
}

/// The `x86_64-w64-mingw32/include` directory inside a materialized
/// bundle root.
pub fn sysroot_include_dir(bundle_root: &Path) -> PathBuf {
    bundle_root.join("x86_64-w64-mingw32").join("include")
}

pub fn bin_dir(bundle_root: &Path) -> PathBuf {
    bundle_root.join("bin")
}

/// GCC / MinGW-w64 runtime DLLs a win-gnu binary may import. They are not
/// part of Windows, so they must sit next to the `.exe` (or on `PATH`) at
/// run time.
pub const MINGW_RUNTIME_DLLS: &[&str] = &[
    "libgcc_s_seh-1.dll",
    "libstdc++-6.dll",
    "libwinpthread-1.dll",
    "libatomic-1.dll",
    "libgomp-1.dll",
    "libquadmath-0.dll",
    "libssp-0.dll",
];

/// The runtime DLLs a materialized win-gnu bundle ships, by absolute path.
///
/// The WinLibs (Windows-host) bundle keeps them in `bin/`; the conda-forge
/// Linux cross bundle keeps them in `x86_64-w64-mingw32/{bin,lib}`. The
/// first directory holding a given DLL wins, so each name appears once.
pub fn runtime_dlls(bundle_root: &Path) -> Vec<PathBuf> {
    let target = bundle_root.join(MINGW_W64_CROSS_PREFIX);
    let dirs = [bin_dir(bundle_root), target.join("bin"), target.join("lib")];
    MINGW_RUNTIME_DLLS
        .iter()
        .filter_map(|name| {
            dirs.iter()
                .map(|dir| dir.join(name))
                .find(|path| path.is_file())
        })
        .collect()
}

/// Restore-completeness check for the managed win-gnu compiler bundle
/// (soldr#2336 gap #3). A bundle that restored only `bin/gcc.exe` but is
/// missing dlltool/windres or the sysroot import libs + headers reads as
/// "present" yet fails at link time; assert the tools AND the sysroot
/// inputs a win-gnu link actually consumes. `bundle_dir` holds the
/// `.complete` marker; `package` is its extracted payload root.
pub fn managed_restore_present(bundle_dir: &Path, package: &Path) -> bool {
    let has_tool = |tool: &str| bin_dir(package).join(exe_name(tool)).is_file();
    bundle_dir.join(".complete").is_file()
        && has_tool("gcc")
        && has_tool("dlltool")
        && has_tool("windres")
        && sysroot_lib_dir(package).join("crt2.o").is_file()
        && sysroot_lib_dir(package).join("libmsvcrt.a").is_file()
        && sysroot_include_dir(package).join("stdio.h").is_file()
}

pub fn exe_name(base: &str) -> String {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

pub fn tool_path(bundle_root: &Path, tool: &str) -> PathBuf {
    bin_dir(bundle_root).join(exe_name(tool))
}

/// Target-scoped env consumed by Cargo, rustc, cc-rs, and resource
/// build scripts for `x86_64-pc-windows-gnu`.
pub fn env_for_target(bundle_root: &Path, target_triple: &str) -> Vec<(String, String)> {
    let target_u = target_triple.replace('-', "_");
    let target_u_upper = target_u.to_uppercase();

    let tool = |name: &str| tool_path(bundle_root, name).to_string_lossy().into_owned();
    vec![
        (
            "MINGW_W64_GCC_ROOT".to_string(),
            bundle_root.to_string_lossy().into_owned(),
        ),
        (
            "MINGW_W64_GCC_BIN".to_string(),
            bin_dir(bundle_root).to_string_lossy().into_owned(),
        ),
        (format!("CC_{target_u}"), tool("gcc")),
        (format!("CXX_{target_u}"), tool("g++")),
        (format!("AR_{target_u}"), tool("ar")),
        (format!("RANLIB_{target_u}"), tool("ranlib")),
        (format!("WINDRES_{target_u}"), tool("windres")),
        // soldr#2336 gap #4: the bundle ships `dlltool`, but it was
        // never surfaced. Rust's windows-gnu link + cc-rs import-lib
        // generation (`.def` -> `.a`) both look for `DLLTOOL_<triple>`.
        (format!("DLLTOOL_{target_u}"), tool("dlltool")),
        (format!("CARGO_TARGET_{target_u_upper}_LINKER"), tool("gcc")),
    ]
}

pub fn path_with_mingw_bin(
    bundle_root: &Path,
    current_path: Option<std::ffi::OsString>,
) -> Result<String, SoldrError> {
    let mut entries = vec![bin_dir(bundle_root)];
    if let Some(path) = current_path {
        entries.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(entries)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| SoldrError::Other(format!("failed to build PATH with MinGW bin: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dlls_are_found_in_both_bundle_layouts() {
        // WinLibs layout: DLLs in bin/.
        let winlibs = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(winlibs.path().join("bin")).unwrap();
        for name in ["libstdc++-6.dll", "libwinpthread-1.dll", "gcc.exe"] {
            std::fs::write(winlibs.path().join("bin").join(name), b"").unwrap();
        }
        let found = runtime_dlls(winlibs.path());
        let names: Vec<_> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["libstdc++-6.dll", "libwinpthread-1.dll"]);
        assert!(found.iter().all(|p| p.is_absolute() && p.is_file()));

        // conda-forge cross layout: DLLs in x86_64-w64-mingw32/{lib,bin};
        // a name present in both directories is reported once (bin wins).
        let cross = tempfile::tempdir().unwrap();
        let target = cross.path().join(MINGW_W64_CROSS_PREFIX);
        std::fs::create_dir_all(target.join("lib")).unwrap();
        std::fs::create_dir_all(target.join("bin")).unwrap();
        std::fs::write(target.join("lib").join("libgcc_s_seh-1.dll"), b"").unwrap();
        std::fs::write(target.join("lib").join("libstdc++-6.dll"), b"").unwrap();
        std::fs::write(target.join("bin").join("libstdc++-6.dll"), b"").unwrap();
        assert_eq!(
            runtime_dlls(cross.path()),
            [
                target.join("lib").join("libgcc_s_seh-1.dll"),
                target.join("bin").join("libstdc++-6.dll"),
            ]
        );

        assert!(runtime_dlls(tempfile::tempdir().unwrap().path()).is_empty());
    }

    #[test]
    fn asset_url_layout_matches_catalogue() {
        let url = mingw_w64_gcc_asset_url_for(MANAGED_MINGW_W64_GCC_VERSION, MINGW_W64_GCC_SLUG);
        assert!(url.contains("/mingw-w64-gcc/"));
        assert!(url.contains("/15.3.0posix-14.0.0-msvcrt-r1/windows-x64-gnu/"));
        assert!(url.ends_with("/bundle.tar.zst"));
    }

    #[test]
    fn target_slug_scope_is_explicit() {
        assert_eq!(
            slug_for_target("x86_64-pc-windows-gnu"),
            Some("windows-x64-gnu")
        );
        assert_eq!(slug_for_target("aarch64-pc-windows-gnullvm"), None);
        assert_eq!(slug_for_target("x86_64-pc-windows-msvc"), None);
    }

    #[test]
    fn host_scope_is_windows_x64_only() {
        let expected = crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
            && crate::platform::host::facts::arch()
                == crate::platform::host::facts::HostArch::X86_64;
        assert_eq!(current_host_supports_mingw_w64_gcc(), expected);
    }

    #[test]
    fn env_for_target_sets_cargo_and_cc_rs_vars() {
        let root = Path::new("C:/soldr/mingw");
        let env = env_for_target(root, "x86_64-pc-windows-gnu");
        let lookup = |name: &str| {
            env.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        assert!(lookup("CC_x86_64_pc_windows_gnu").contains("gcc"));
        assert!(lookup("CXX_x86_64_pc_windows_gnu").contains("g++"));
        assert!(lookup("AR_x86_64_pc_windows_gnu").contains("ar"));
        assert!(lookup("WINDRES_x86_64_pc_windows_gnu").contains("windres"));
        // soldr#2336 gap #4: dlltool must be surfaced.
        assert!(lookup("DLLTOOL_x86_64_pc_windows_gnu").contains("dlltool"));
        assert!(lookup("CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER").contains("gcc"));
    }

    #[test]
    fn sysroot_asset_url_layout_matches_catalogue() {
        // soldr-toolchain#114 host-neutral sysroot lives beside the
        // compiler bundle under its own tool subtree, same version/slug.
        let url =
            mingw_w64_sysroot_asset_url_for(MANAGED_MINGW_W64_GCC_VERSION, MINGW_W64_GCC_SLUG);
        assert!(url.contains("/mingw-w64-sysroot/"));
        assert!(url.contains("/15.3.0posix-14.0.0-msvcrt-r1/windows-x64-gnu/"));
        assert!(url.ends_with("/bundle.tar.zst"));
        // The sysroot pins the SAME upstream release as the compiler so
        // CRT/gcc/mingw versions agree.
        assert_eq!(MINGW_W64_SYSROOT_TOOL, "mingw-w64-sysroot");
    }

    #[test]
    fn sysroot_dirs_resolve_under_target_triple() {
        let root = Path::new("/soldr/mingw-sysroot/package");
        let lib = sysroot_lib_dir(root);
        let include = sysroot_include_dir(root);
        assert!(lib.ends_with("x86_64-w64-mingw32/lib"));
        assert!(include.ends_with("x86_64-w64-mingw32/include"));
    }

    #[test]
    fn cross_env_points_at_mingw32_prefixed_drivers() {
        // soldr-toolchain#114 Phase 2: Linux-host cross toolchain uses
        // `bin/x86_64-w64-mingw32-*`, not a Windows `bin/gcc.exe`.
        let root = Path::new("/soldr/mingw-cross");
        let env = cross_env_for_target(root, "x86_64-pc-windows-gnu");
        let lookup = |name: &str| {
            env.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        assert!(lookup("CC_x86_64_pc_windows_gnu").contains("x86_64-w64-mingw32-gcc"));
        assert!(lookup("CXX_x86_64_pc_windows_gnu").contains("x86_64-w64-mingw32-g++"));
        assert!(lookup("AR_x86_64_pc_windows_gnu").contains("x86_64-w64-mingw32-ar"));
        assert!(lookup("DLLTOOL_x86_64_pc_windows_gnu").contains("x86_64-w64-mingw32-dlltool"));
        assert!(
            lookup("CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER").contains("x86_64-w64-mingw32-gcc")
        );
    }

    #[test]
    fn cross_asset_url_uses_host_slug() {
        let url = mingw_w64_cross_asset_url_for(MINGW_W64_CROSS_VERSION, MINGW_W64_CROSS_HOST_SLUG);
        assert!(url.contains("/mingw-w64-cross/"));
        assert!(url.contains("/mingw-w64-gcc-15.3.0/linux-x64-gnu/"));
        assert!(url.ends_with("/bundle.tar.zst"));
    }
}
