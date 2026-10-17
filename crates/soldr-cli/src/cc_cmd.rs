//! Standalone catalogue-backed C/C++ compiler driver (soldr#2335).
//!
//! `soldr cc` and `soldr c++` deliberately prepare only the native compiler
//! toolchain. Routing through `target_lifecycle::prepare_target` would also
//! install Rust std targets, provision CMake, and probe every `*-sys` library
//! override on every compiler process CMake starts.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};

#[derive(Debug, Clone, clap::Args)]
pub(crate) struct CcArgs {
    /// Compilation target (friendly alias or Rust triple, optionally `.2.17`)
    #[arg(long, default_value = "host", value_name = "TARGET")]
    pub(crate) target: String,

    /// Print the prepared C compiler path instead of invoking it
    #[arg(long, group = "print_tool")]
    pub(crate) print_cc: bool,

    /// Print the prepared C++ compiler path instead of invoking it
    #[arg(long, group = "print_tool")]
    pub(crate) print_cxx: bool,

    /// Print the prepared archiver path instead of invoking the compiler
    #[arg(long, group = "print_tool")]
    pub(crate) print_ar: bool,

    /// Print the prepared linker-driver path instead of invoking the compiler
    #[arg(long, group = "print_tool")]
    pub(crate) print_linker: bool,

    /// Print the target's SDK/sysroot root (Apple SDK, xwin root, ...) instead
    /// of invoking the compiler
    #[arg(long, group = "print_tool")]
    pub(crate) print_sysroot: bool,

    /// Print the Windows resource compiler (`llvm-rc`, `win-*` MSVC targets)
    /// for `CMAKE_RC_COMPILER` instead of invoking the compiler
    #[arg(long, group = "print_tool")]
    pub(crate) print_rc: bool,

    /// Arguments forwarded verbatim to the selected compiler
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Language {
    C,
    Cxx,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeToolchain {
    pub(crate) cc: PathBuf,
    pub(crate) cxx: PathBuf,
    pub(crate) ar: PathBuf,
    pub(crate) linker: PathBuf,
    pub(crate) bin_dir: PathBuf,
    /// The SDK / sysroot root, when the target has one.
    pub(crate) sysroot: Option<PathBuf>,
    pub(crate) compiler_args: Vec<String>,
    /// The Windows resource compiler, for MSVC targets (soldr#3810).
    pub(crate) rc: Option<PathBuf>,
}

pub(crate) async fn run(args: CcArgs, language: Language) -> Result<i32, SoldrError> {
    if requests_print(&args) && !args.args.is_empty() {
        return Err(SoldrError::Other(format!(
            "soldr {}: compiler arguments cannot be combined with --print-*",
            language.command_name()
        )));
    }

    let paths = SoldrPaths::new()?;
    let (rust_triple, tools) = prepare_for_target(&paths, &args.target, language).await?;
    let resolved_triple = rust_triple.as_str();

    if args.print_rc {
        let rc = tools.rc.as_ref().ok_or_else(|| {
            SoldrError::Other(format!(
                "soldr {}: target {resolved_triple} has no resource compiler \
                 (only win-* MSVC targets ship llvm-rc)",
                language.command_name()
            ))
        })?;
        println!("{}", rc.display());
        return Ok(0);
    }
    if args.print_sysroot {
        let sysroot = tools.sysroot.as_ref().ok_or_else(|| {
            SoldrError::Other(format!(
                "soldr {}: target {resolved_triple} has no SDK/sysroot to print",
                language.command_name()
            ))
        })?;
        println!("{}", sysroot.display());
        return Ok(0);
    }
    if let Some(path) = selected_print_path(&args, &tools) {
        println!("{}", path.display());
        return Ok(0);
    }

    let compiler = match language {
        Language::C => &tools.cc,
        Language::Cxx => &tools.cxx,
    };
    let mut command = Command::new(compiler);
    command.args(&tools.compiler_args);
    command.args(&args.args);
    command.env("PATH", path_with_prepend(&tools.bin_dir)?);
    suppress_windows_console_window(&mut command);

    let status = crate::platform::wait::status_for(&mut command, crate::platform::wait::LONG_CHILD)
        .map_err(|error| {
            SoldrError::Other(format!(
                "soldr {}: failed to start compiler {} for target {}: {error}",
                language.command_name(),
                compiler.display(),
                resolved_triple
            ))
        })?;
    Ok(status.code().unwrap_or(1))
}

fn requests_print(args: &CcArgs) -> bool {
    args.print_cc
        || args.print_cxx
        || args.print_ar
        || args.print_linker
        || args.print_sysroot
        || args.print_rc
}

/// Resolve a user-facing `--target` spelling and prepare the compiler
/// toolchain `soldr cc` / `soldr c++` (and `toolchain prepare --target`,
/// soldr#3806) expose for it. Returns the canonical Rust triple too.
pub(crate) async fn prepare_for_target(
    paths: &SoldrPaths,
    target: &str,
    language: Language,
) -> Result<(String, NativeToolchain), SoldrError> {
    let target = normalize_driver_target(target);
    let resolved = crate::target_alias::resolve_soldr_target(&target)
        .map_err(|error| error.into_soldr_error(language.target_surface()))?;
    let tools = prepare_native_toolchain(paths, &resolved.rust_triple).await?;
    Ok((resolved.rust_triple, tools))
}

impl Language {
    pub(crate) fn command_name(self) -> &'static str {
        match self {
            Self::C => "cc",
            Self::Cxx => "c++",
        }
    }

    /// soldr#3390: the [`crate::target_alias::TargetSurface`] this language's
    /// standalone compiler driver is invoked as, so an `AliasError` renders
    /// with the verb the user actually typed.
    pub(crate) fn target_surface(self) -> crate::target_alias::TargetSurface {
        match self {
            Self::C => crate::target_alias::TargetSurface::Cc,
            Self::Cxx => crate::target_alias::TargetSurface::Cxx,
        }
    }
}

fn selected_print_path<'a>(args: &CcArgs, tools: &'a NativeToolchain) -> Option<&'a Path> {
    if args.print_cc {
        Some(&tools.cc)
    } else if args.print_cxx {
        Some(&tools.cxx)
    } else if args.print_ar {
        Some(&tools.ar)
    } else if args.print_linker {
        Some(&tools.linker)
    } else {
        None
    }
}

pub(crate) async fn prepare_native_toolchain(
    paths: &SoldrPaths,
    target: &str,
) -> Result<NativeToolchain, SoldrError> {
    if target.ends_with("-unknown-linux-gnu") {
        if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
            return Err(unsupported_host(target));
        }
        let toolchain = crate::fetch::gnu_linux_toolchain::ensure(paths, target).await?;
        return Ok(NativeToolchain {
            cc: toolchain.tool_path("gcc"),
            cxx: toolchain.tool_path("g++"),
            ar: toolchain.tool_path("ar"),
            linker: toolchain.tool_path("gcc"),
            bin_dir: toolchain.bin_dir,
            sysroot: Some(toolchain.sysroot.clone()),
            compiler_args: vec![format!("--sysroot={}", toolchain.sysroot.display())],
            rc: None,
        });
    }

    if target.ends_with("-unknown-linux-musl") {
        if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
            return Err(unsupported_host(target));
        }
        let toolchain = crate::fetch::musl_linux_toolchain::ensure(paths, target).await?;
        return Ok(NativeToolchain {
            cc: toolchain.tool_path("gcc"),
            cxx: toolchain.tool_path("g++"),
            ar: toolchain.tool_path("ar"),
            linker: toolchain.tool_path("gcc"),
            bin_dir: toolchain.bin_dir,
            sysroot: Some(toolchain.sysroot.clone()),
            compiler_args: vec![format!("--sysroot={}", toolchain.sysroot.display())],
            rc: None,
        });
    }

    if target == crate::fetch::mingw_w64_gcc::MINGW_W64_GCC_TARGET {
        let (bin_dir, env) =
            crate::fetch::mingw_w64_gcc::prepare_win_gnu_env(paths, target).await?;
        let suffix = target.replace('-', "_");
        let upper = suffix.to_ascii_uppercase();
        return Ok(NativeToolchain {
            cc: env_path(&env, &format!("CC_{suffix}"))?,
            cxx: env_path(&env, &format!("CXX_{suffix}"))?,
            ar: env_path(&env, &format!("AR_{suffix}"))?,
            linker: env_path(&env, &format!("CARGO_TARGET_{upper}_LINKER"))?,
            bin_dir,
            sysroot: None,
            compiler_args: Vec::new(),
            rc: None,
        });
    }

    if crate::cc_clang_cross::handles_target(target) {
        let cross = crate::cc_clang_cross::prepare(paths, target).await?;
        return Ok(NativeToolchain {
            cc: cross.cc,
            cxx: cross.cxx,
            ar: cross.ar,
            linker: cross.linker,
            bin_dir: cross.bin_dir,
            sysroot: Some(cross.sysroot),
            compiler_args: Vec::new(),
            rc: cross.rc,
        });
    }

    Err(SoldrError::UnsupportedPlatform(format!(
        "soldr C/C++ does not yet expose a standalone compiler for `{target}`; \
         supported: GNU/Linux, musl/Linux, x86_64-pc-windows-gnu, \
         Apple Darwin and MSVC Windows targets"
    )))
}

fn env_path(env: &[(String, String)], key: &str) -> Result<PathBuf, SoldrError> {
    env.iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| PathBuf::from(value))
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "soldr C/C++: prepared toolchain did not provide required `{key}`"
            ))
        })
}

fn unsupported_host(target: &str) -> SoldrError {
    SoldrError::UnsupportedPlatform(format!(
        "the catalogue compiler for `{target}` is a Linux executable; \
         standalone `soldr cc`/`soldr c++` for this host/target pair is tracked in soldr#2319"
    ))
}

fn path_with_prepend(dir: &Path) -> Result<std::ffi::OsString, SoldrError> {
    let mut entries = vec![dir.to_path_buf()];
    if let Some(path) = std::env::var_os("PATH") {
        entries.extend(std::env::split_paths(&path).filter(|entry| entry != dir));
    }
    std::env::join_paths(entries)
        .map_err(|error| SoldrError::Other(format!("soldr C/C++: failed to prepare PATH: {error}")))
}

/// Accept the concise compiler-driver spellings used by GCC/Zig in addition
/// to Soldr's Rust triples. The target lifecycle remains keyed by canonical
/// Rust triples after this boundary.
fn normalize_driver_target(input: &str) -> String {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    for (driver, rust) in [
        ("x86_64-linux-gnu", "x86_64-unknown-linux-gnu"),
        ("aarch64-linux-gnu", "aarch64-unknown-linux-gnu"),
        ("x86_64-linux-musl", "x86_64-unknown-linux-musl"),
        ("aarch64-linux-musl", "aarch64-unknown-linux-musl"),
    ] {
        if lower == driver {
            return rust.to_string();
        }
        if let Some(suffix) = lower.strip_prefix(&format!("{driver}.")) {
            return format!("{rust}.{suffix}");
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_tool_selection_is_unambiguous() {
        let tools = NativeToolchain {
            cc: PathBuf::from("/sdk/cc"),
            cxx: PathBuf::from("/sdk/c++"),
            ar: PathBuf::from("/sdk/ar"),
            linker: PathBuf::from("/sdk/linker"),
            bin_dir: PathBuf::from("/sdk"),
            sysroot: None,
            compiler_args: Vec::new(),
            rc: None,
        };
        let args = CcArgs {
            target: "host".to_string(),
            print_cc: false,
            print_cxx: true,
            print_ar: false,
            print_linker: false,
            print_sysroot: false,
            print_rc: false,
            args: Vec::new(),
        };
        assert_eq!(
            selected_print_path(&args, &tools),
            Some(Path::new("/sdk/c++"))
        );
    }

    #[test]
    fn alias_errors_name_the_cc_surface() {
        let message = crate::target_alias::resolve_soldr_target("not-target")
            .unwrap_err()
            .into_soldr_error(Language::C.target_surface())
            .to_string();
        assert!(message.starts_with("soldr cc --target"));
        assert!(!message.contains("soldr build"));
    }

    #[test]
    fn alias_errors_name_the_cxx_surface() {
        let message = crate::target_alias::resolve_soldr_target("not-target")
            .unwrap_err()
            .into_soldr_error(Language::Cxx.target_surface())
            .to_string();
        assert!(message.starts_with("soldr c++ --target"));
        assert!(!message.contains("soldr build"));
    }

    #[test]
    fn print_request_with_compiler_args_is_detectable_early() {
        let args = CcArgs {
            target: "host".to_string(),
            print_cc: true,
            print_cxx: false,
            print_ar: false,
            print_linker: false,
            print_sysroot: false,
            print_rc: false,
            args: vec!["hello.c".to_string()],
        };
        assert!(requests_print(&args));
        assert!(!args.args.is_empty());
    }

    #[test]
    fn zig_style_gnu_target_normalizes_to_the_rust_triple() {
        assert_eq!(
            normalize_driver_target("x86_64-linux-gnu.2.17"),
            "x86_64-unknown-linux-gnu.2.17"
        );
        assert_eq!(
            normalize_driver_target("aarch64-linux-gnu"),
            "aarch64-unknown-linux-gnu"
        );
    }

    #[test]
    fn path_prefix_keeps_the_managed_bin_first() {
        let _env = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (managed, host, other) = if crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
        {
            (r"C:\managed\bin", r"C:\host\bin", r"C:\other")
        } else {
            ("/managed/bin", "/host/bin", "/other")
        };
        let _path = crate::EnvVarGuard::set("PATH", std::env::join_paths([host, other]).unwrap());
        let value = path_with_prepend(Path::new(managed)).unwrap();
        let entries = std::env::split_paths(&value).collect::<Vec<_>>();
        assert_eq!(entries.first(), Some(&PathBuf::from(managed)));
    }
}
