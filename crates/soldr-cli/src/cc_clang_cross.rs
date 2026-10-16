//! `soldr cc` / `soldr c++` for the Apple and MSVC targets (soldr#3806).
//!
//! The managed LLVM, the Apple SDK and the xwin MSVC CRT + Windows SDK are
//! the ones `soldr cargo --target` already provisions (`blessed_build`); this
//! module only exposes them as a standalone compiler. `--print-cc` must name
//! ONE executable usable as `CC` / `CMAKE_C_COMPILER`, so the printed driver
//! is a content-addressed shim that carries the target and sysroot flags
//! itself, the same mechanism the linker-driver shims use. Flags that only
//! matter when linking (`-fuse-ld=lld`, library search paths) live in a clang
//! `--config` file so a `-c` compile stays clean under `-Werror` (soldr#3776).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::core::{SoldrError, SoldrPaths};

const DRIVER_FORMAT_VERSION: &str = "v1";

/// What `soldr cc --target T` needs for an Apple or MSVC target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClangCross {
    pub cc: PathBuf,
    pub cxx: PathBuf,
    pub ar: PathBuf,
    pub linker: PathBuf,
    pub bin_dir: PathBuf,
    pub sysroot: PathBuf,
}

pub(crate) fn handles_target(target: &str) -> bool {
    (target.ends_with("-apple-darwin")
        && (target.starts_with("aarch64-") || target.starts_with("x86_64-")))
        || matches!(
            target,
            "x86_64-pc-windows-msvc" | "aarch64-pc-windows-msvc"
        )
}

/// The flags a target's driver shim carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverSpec {
    /// Compile + link flags for both C and C++.
    pub common: Vec<String>,
    /// Extra flags for the C++ driver only.
    pub cxx_extra: Vec<String>,
    /// Link-only lines of the clang config file.
    pub config_lines: Vec<String>,
}

pub(crate) async fn prepare(paths: &SoldrPaths, target: &str) -> Result<ClangCross, SoldrError> {
    let bin_dir = crate::fetch::ensure_llvm_toolchain(paths).await?;
    let (sysroot, spec, ar) = if target.ends_with("-apple-darwin") {
        let sdk = crate::fetch::apple_sdk::ensure_apple_sdk(paths, Some(target)).await?;
        let spec = apple_spec(target, &sdk);
        (sdk, spec, "llvm-ar")
    } else {
        let xwin = crate::fetch::xwin_cache::ensure_xwin_cache(paths, target).await?;
        let spec = msvc_spec(target, &xwin);
        (xwin, spec, "llvm-lib")
    };
    let (cc, cxx) = write_drivers(paths, target, &bin_dir, &spec)?;
    let ar = bin_dir.join(crate::platform::executable::name::native(ar));
    if !ar.is_file() {
        return Err(SoldrError::Other(format!(
            "soldr C/C++: managed LLVM at {} has no {}",
            bin_dir.display(),
            ar.file_name().unwrap_or_default().to_string_lossy()
        )));
    }
    Ok(ClangCross {
        linker: cc.clone(),
        cc,
        cxx,
        ar,
        bin_dir,
        sysroot,
    })
}

pub(crate) fn apple_spec(target: &str, sdk: &Path) -> DriverSpec {
    let clang_target = crate::blessed_build::darwin_arch::clang_target(target);
    let min_os = crate::blessed_build::darwin_arch::deployment_target(target);
    DriverSpec {
        common: vec![
            format!("--target={clang_target}"),
            "-isysroot".to_string(),
            sdk.display().to_string(),
            format!("-mmacosx-version-min={min_os}"),
        ],
        cxx_extra: vec!["-stdlib=libc++".to_string()],
        config_lines: vec!["-fuse-ld=lld".to_string()],
    }
}

pub(crate) fn msvc_spec(target: &str, xwin: &Path) -> DriverSpec {
    // Bundles lay libs out per MS arch name (`x64`, `arm64`) or per Rust arch
    // name (`x86_64`, `aarch64`); take whichever exists, as the cargo path does.
    let arch_dirs: &[&str] = if target.starts_with("aarch64-") {
        &["arm64", "aarch64"]
    } else {
        &["x64", "x86_64"]
    };
    let include_roots: Vec<PathBuf> = [
        xwin.join("crt").join("include"),
        xwin.join("sdk").join("include").join("ucrt"),
        xwin.join("sdk").join("include").join("um"),
        xwin.join("sdk").join("include").join("shared"),
        xwin.join("sdk").join("include").join("winrt"),
        xwin.join("sdk").join("include").join("cppwinrt"),
    ]
    .into_iter()
    .filter(|dir| dir.is_dir())
    .collect();
    let mut common = vec![
        format!("--target={target}"),
        "-fms-compatibility".to_string(),
        "-fms-extensions".to_string(),
    ];
    for dir in &include_roots {
        common.push("-isystem".to_string());
        common.push(dir.display().to_string());
    }
    // soldr#3415: resolve any `#include` casing against the case-folded SDK.
    if let Ok(Some(overlay)) =
        crate::fetch::xwin_case_overlay::ensure_case_overlay(xwin, &include_roots)
    {
        common.push("-ivfsoverlay".to_string());
        common.push(overlay.display().to_string());
    }
    let mut config_lines = vec!["-fuse-ld=lld".to_string()];
    for lib in arch_dirs.iter().flat_map(|arch| {
        [
            xwin.join("crt").join("lib").join(arch),
            xwin.join("sdk").join("lib").join("um").join(arch),
            xwin.join("sdk").join("lib").join("ucrt").join(arch),
        ]
    }) {
        if lib.is_dir() {
            config_lines.push(format!("-L{}", lib.display()));
        }
    }
    DriverSpec {
        common,
        cxx_extra: Vec::new(),
        config_lines,
    }
}

fn write_drivers(
    paths: &SoldrPaths,
    target: &str,
    bin_dir: &Path,
    spec: &DriverSpec,
) -> Result<(PathBuf, PathBuf), SoldrError> {
    let clang = bin_dir.join(crate::platform::executable::name::native("clang"));
    let clangxx = bin_dir.join(crate::platform::executable::name::native("clang++"));
    for tool in [&clang, &clangxx] {
        if !tool.is_file() {
            return Err(SoldrError::Other(format!(
                "soldr C/C++: managed LLVM at {} has no {}",
                bin_dir.display(),
                tool.file_name().unwrap_or_default().to_string_lossy()
            )));
        }
    }
    let config_body = format!("{}\n", spec.config_lines.join("\n"));
    let digest = {
        let mut hasher = Sha256::new();
        hasher.update(clang.display().to_string());
        hasher.update(format!("{spec:?}"));
        format!("{:x}", hasher.finalize())
    };
    let dir = paths
        .bin
        .join("cc-driver")
        .join(DRIVER_FORMAT_VERSION)
        .join(target)
        .join(&digest[..16]);
    std::fs::create_dir_all(&dir)?;
    let config = dir.join(format!("{target}.cfg"));
    crate::linker_shim::write_content_addressed_shim(&config, &config_body)?;
    let config_arg = format!("--config={}", config.display());

    let suffix = crate::platform::executable::name::script_suffix();
    let mut written = Vec::new();
    for (name, real, extra) in [
        ("clang", &clang, &spec.cxx_extra[..0]),
        ("clang++", &clangxx, &spec.cxx_extra[..]),
    ] {
        let mut args = spec.common.clone();
        args.extend(extra.iter().cloned());
        args.push(config_arg.clone());
        let path = dir.join(format!("{target}-{name}{suffix}"));
        crate::linker_shim::write_content_addressed_shim(&path, &render_driver(real, &args)?)?;
        written.push(path);
    }
    let cxx = written.pop().expect("two drivers written");
    let cc = written.pop().expect("two drivers written");
    Ok((cc, cxx))
}

/// The driver body: execs the absolute managed clang with the target flags
/// ahead of the caller's arguments.
pub(crate) fn render_driver(clang: &Path, args: &[String]) -> Result<String, SoldrError> {
    let clang = clang.to_str().ok_or_else(|| {
        SoldrError::Other(format!(
            "clang path is not valid UTF-8: {}",
            clang.display()
        ))
    })?;
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        let quote = |value: &str| format!("\"{}\"", value.replace('%', "%%"));
        let rendered = args
            .iter()
            .map(|arg| quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        return Ok(format!("@echo off\r\n{} {rendered} %*\r\n", quote(clang)));
    }
    let rendered = args
        .iter()
        .map(|arg| crate::linker_shim::shell_single_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        "#!/bin/sh\n# generated by soldr; content-addressed cc driver (soldr#3806)\nexec {} {rendered} \"$@\"\n",
        crate::linker_shim::shell_single_quote(clang)
    ))
}

#[cfg(test)]
#[path = "cc_clang_cross_tests.rs"]
mod tests;
