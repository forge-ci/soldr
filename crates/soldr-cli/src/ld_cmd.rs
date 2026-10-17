//! Standalone target linkers: `soldr ld` and `soldr link` (soldr#3806).
//!
//! Both run a raw linker for a target with its sysroot and library search
//! paths injected. They differ by the **command-line dialect** the caller
//! speaks, not by the binary that runs:
//!
//! - `soldr ld` takes GNU `ld` syntax (`-o`, `-L`, `-l`). It serves ELF
//!   (`linux-*`), Mach-O (`mac-*`, through `ld64.lld`'s ld64 dialect) and
//!   `win-x64-gnu` (`ld.lld -m i386pep`, the mingw dialect).
//! - `soldr link` takes MSVC `link.exe` syntax (`/OUT:`, `/LIBPATH:`). It
//!   serves `*-pc-windows-msvc` through `lld-link` with the xwin CRT/SDK
//!   library paths.
//!
//! This is the `clang` / `clang-cl` split: build systems choose the linker
//! dialect from the target ABI, so one verb taking both would have to guess.
//! Using the wrong verb is an error that names the right one.
//!
//! `--fuse=<linker>` reuses the `SOLDR_LINKER` parser
//! ([`crate::linker::LinkerChoice`]) so the two spellings cannot disagree, and
//! additionally accepts `lld` for the managed-LLVM default.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use crate::linker::LinkerChoice;
use crate::target_alias::TargetSurface;

/// Options shared by `soldr ld` and `soldr link`.
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct LinkerCommon {
    /// Link target (friendly alias or Rust triple)
    #[arg(long, default_value = "host", value_name = "TARGET")]
    pub(crate) target: String,

    /// Linker to use: default | lld | ld | reld | mold | rust-lld | fast
    /// (case-insensitive; overrides SOLDR_LINKER)
    #[arg(long, value_name = "LINKER")]
    pub(crate) fuse: Option<String>,

    /// Print the target sysroot instead of linking
    #[arg(long)]
    pub(crate) print_sysroot: bool,

    /// Arguments forwarded verbatim to the linker
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) args: Vec<String>,
}

#[derive(Debug, Clone, clap::Args)]
pub(crate) struct LdArgs {
    /// Print a linker shim (usable as `LD`) instead of linking
    #[arg(long)]
    pub(crate) print_ld: bool,

    #[command(flatten)]
    pub(crate) common: LinkerCommon,
}

#[derive(Debug, Clone, clap::Args)]
pub(crate) struct LinkArgs {
    /// Print a linker shim (usable as `CMAKE_LINKER`) instead of linking
    #[arg(long)]
    pub(crate) print_link: bool,

    #[command(flatten)]
    pub(crate) common: LinkerCommon,
}

/// Which argument syntax the invoked verb speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// `soldr ld`: GNU ld syntax.
    Gnu,
    /// `soldr link`: MSVC link.exe syntax.
    Msvc,
}

impl Dialect {
    fn verb(self) -> &'static str {
        match self {
            Self::Gnu => "ld",
            Self::Msvc => "link",
        }
    }

    fn surface(self) -> TargetSurface {
        match self {
            Self::Gnu => TargetSurface::Ld,
            Self::Msvc => TargetSurface::Link,
        }
    }
}

/// The linker selected by `--fuse` / `SOLDR_LINKER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fuse {
    /// Managed LLVM lld in the target's flavor.
    Lld,
    /// The GNU `ld` that ships in the target's GCC toolchain.
    GnuLd,
    Reld,
    Mold,
    RustLld,
}

fn parse_fuse(value: Option<&str>) -> Result<Fuse, SoldrError> {
    let from_env = std::env::var("SOLDR_LINKER").ok();
    let Some(raw) = value.or(from_env.as_deref()) else {
        return Ok(Fuse::Lld);
    };
    if raw.trim().eq_ignore_ascii_case("lld") {
        return Ok(Fuse::Lld);
    }
    Ok(match LinkerChoice::from_str(raw)? {
        LinkerChoice::Default => Fuse::Lld,
        LinkerChoice::Ld => Fuse::GnuLd,
        LinkerChoice::Reld => Fuse::Reld,
        LinkerChoice::Mold => Fuse::Mold,
        LinkerChoice::RustLld => Fuse::RustLld,
        // `fast` is reld wherever reld can link; elsewhere the default.
        LinkerChoice::Fast => Fuse::Reld,
    })
}

/// Target families with distinct raw-linker plumbing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    LinuxGnu,
    LinuxMusl,
    Darwin,
    WindowsGnu,
    WindowsMsvc,
}

fn family(target: &str) -> Option<Family> {
    if target.ends_with("-unknown-linux-gnu") {
        Some(Family::LinuxGnu)
    } else if target.ends_with("-unknown-linux-musl") {
        Some(Family::LinuxMusl)
    } else if matches!(target, "x86_64-apple-darwin" | "aarch64-apple-darwin") {
        Some(Family::Darwin)
    } else if target == crate::fetch::mingw_w64_gcc::MINGW_W64_GCC_TARGET {
        Some(Family::WindowsGnu)
    } else if matches!(target, "x86_64-pc-windows-msvc" | "aarch64-pc-windows-msvc") {
        Some(Family::WindowsMsvc)
    } else {
        None
    }
}

/// The verb a target's linker dialect requires.
fn required_dialect(family: Family) -> Dialect {
    if family == Family::WindowsMsvc {
        Dialect::Msvc
    } else {
        Dialect::Gnu
    }
}

/// A prepared raw linker: the binary, the flags soldr injects ahead of the
/// caller's arguments, and the sysroot it was prepared against.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedLinker {
    program: PathBuf,
    args: Vec<String>,
    sysroot: Option<PathBuf>,
}

pub(crate) async fn run_ld(args: LdArgs) -> Result<i32, SoldrError> {
    run(args.common, Dialect::Gnu, args.print_ld).await
}

pub(crate) async fn run_link(args: LinkArgs) -> Result<i32, SoldrError> {
    run(args.common, Dialect::Msvc, args.print_link).await
}

async fn run(common: LinkerCommon, dialect: Dialect, print_shim: bool) -> Result<i32, SoldrError> {
    let verb = dialect.verb();
    if (print_shim || common.print_sysroot) && !common.args.is_empty() {
        return Err(SoldrError::Other(format!(
            "soldr {verb}: linker arguments cannot be combined with --print-*"
        )));
    }
    let resolved = crate::target_alias::resolve_soldr_target(&common.target)
        .map_err(|error| error.into_soldr_error(dialect.surface()))?;
    let target = resolved.rust_triple.as_str();
    let family = family(target).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "soldr {verb} has no linker for `{target}`; supported: linux-*, linux-*-musl, \
             mac-*, win-x64-gnu (soldr ld) and win-* MSVC (soldr link)"
        ))
    })?;
    check_dialect(dialect, family, &common.target, target)?;
    let fuse = parse_fuse(common.fuse.as_deref())?;
    check_fuse(verb, family, fuse, target)?;

    let paths = SoldrPaths::new()?;
    paths.ensure_dirs()?;
    let linker = prepare(&paths, family, fuse, target).await?;

    if common.print_sysroot {
        let sysroot = linker.sysroot.as_deref().ok_or_else(|| {
            SoldrError::Other(format!("soldr {verb}: `{target}` has no separate sysroot"))
        })?;
        println!("{}", sysroot.display());
        return Ok(0);
    }
    if print_shim {
        let shim = write_linker_shim(&paths, target, verb, &linker)?;
        println!("{}", shim.display());
        return Ok(0);
    }

    let mut command = Command::new(&linker.program);
    command.args(&linker.args).args(&common.args);
    suppress_windows_console_window(&mut command);
    let status = crate::platform::wait::status_for(&mut command, crate::platform::wait::LONG_CHILD)
        .map_err(|error| {
            SoldrError::Other(format!(
                "soldr {verb}: failed to start linker {} for target {target}: {error}",
                linker.program.display()
            ))
        })?;
    Ok(status.code().unwrap_or(1))
}

/// Write `<paths.bin>/cc-driver/ld/<verb>-<target>-<digest>` that execs the
/// prepared linker with its injected flags ahead of the caller's arguments,
/// so it works as `LD` / `CMAKE_LINKER`.
fn write_linker_shim(
    paths: &SoldrPaths,
    target: &str,
    verb: &str,
    linker: &PreparedLinker,
) -> Result<PathBuf, SoldrError> {
    let body = crate::cc_clang_cross::render_driver(&linker.program, &linker.args)?;
    let digest = blake3::hash(body.as_bytes()).to_hex();
    let dir = paths.bin.join("cc-driver").join("ld");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "{verb}-{target}-{}{}",
        &digest[..16],
        crate::platform::executable::name::script_suffix()
    ));
    crate::linker_shim::write_content_addressed_shim(&path, &body)?;
    Ok(path)
}

fn check_dialect(
    dialect: Dialect,
    family: Family,
    input: &str,
    target: &str,
) -> Result<(), SoldrError> {
    let required = required_dialect(family);
    if dialect == required {
        return Ok(());
    }
    let (have, want) = match dialect {
        Dialect::Gnu => ("GNU ld", "MSVC link.exe"),
        Dialect::Msvc => ("MSVC link.exe", "GNU ld"),
    };
    Err(SoldrError::Other(format!(
        "soldr {}: speaks {have} syntax, but {target} links with {want} syntax -- \
         use `soldr {} --target {input}`",
        dialect.verb(),
        required.verb()
    )))
}

fn check_fuse(verb: &str, family: Family, fuse: Fuse, target: &str) -> Result<(), SoldrError> {
    let elf = matches!(family, Family::LinuxGnu | Family::LinuxMusl);
    let supported = match fuse {
        Fuse::Lld => true,
        Fuse::GnuLd => elf || family == Family::WindowsGnu,
        Fuse::Reld | Fuse::Mold => elf,
        Fuse::RustLld => true,
    };
    if supported {
        return Ok(());
    }
    let options = match family {
        Family::LinuxGnu | Family::LinuxMusl => "lld, ld, reld, mold, rust-lld",
        Family::WindowsGnu => "lld, ld, rust-lld",
        Family::Darwin | Family::WindowsMsvc => "lld, rust-lld",
    };
    Err(SoldrError::Other(format!(
        "soldr {verb}: --fuse={} cannot link `{target}`; supported for this target: {options}",
        fuse_name(fuse)
    )))
}

fn fuse_name(fuse: Fuse) -> &'static str {
    match fuse {
        Fuse::Lld => "lld",
        Fuse::GnuLd => "ld",
        Fuse::Reld => "reld",
        Fuse::Mold => "mold",
        Fuse::RustLld => "rust-lld",
    }
}

async fn prepare(
    paths: &SoldrPaths,
    family: Family,
    fuse: Fuse,
    target: &str,
) -> Result<PreparedLinker, SoldrError> {
    match family {
        Family::LinuxGnu | Family::LinuxMusl => prepare_elf(paths, family, fuse, target).await,
        Family::Darwin => prepare_darwin(paths, fuse, target).await,
        Family::WindowsGnu => prepare_windows_gnu(paths, fuse, target).await,
        Family::WindowsMsvc => prepare_msvc(paths, fuse, target).await,
    }
}

async fn prepare_elf(
    paths: &SoldrPaths,
    family: Family,
    fuse: Fuse,
    target: &str,
) -> Result<PreparedLinker, SoldrError> {
    let (sysroot, gnu_ld) = if family == Family::LinuxGnu {
        let toolchain = crate::fetch::gnu_linux_toolchain::ensure(paths, target).await?;
        let ld = toolchain.tool_path("ld");
        (toolchain.sysroot, ld)
    } else {
        let toolchain = crate::fetch::musl_linux_toolchain::ensure(paths, target).await?;
        let ld = toolchain.tool_path("ld");
        (toolchain.sysroot, ld)
    };
    let search = elf_search_args(&sysroot);
    let (program, mut args) = match fuse {
        Fuse::GnuLd => (gnu_ld, Vec::new()),
        Fuse::Lld => (llvm_tool(paths, "ld.lld").await?, Vec::new()),
        Fuse::Reld => (crate::fetch::reld::ensure_reld(paths).await?, Vec::new()),
        Fuse::Mold => (host_tool("mold")?, Vec::new()),
        Fuse::RustLld => (rust_lld()?, vec!["-flavor".to_string(), "gnu".to_string()]),
    };
    args.extend(search);
    Ok(PreparedLinker {
        program,
        args,
        sysroot: Some(sysroot),
    })
}

/// `--sysroot` plus the sysroot's library directories, so `-lc` and friends
/// resolve against the target, never the host.
fn elf_search_args(sysroot: &Path) -> Vec<String> {
    let mut args = vec![format!("--sysroot={}", sysroot.display())];
    for dir in ["usr/lib64", "usr/lib", "lib64", "lib"] {
        let path = sysroot.join(dir);
        if path.is_dir() {
            args.push(format!("-L{}", path.display()));
        }
    }
    args
}

async fn prepare_darwin(
    paths: &SoldrPaths,
    fuse: Fuse,
    target: &str,
) -> Result<PreparedLinker, SoldrError> {
    let sdk = crate::fetch::apple_sdk::ensure_apple_sdk(paths, Some(target)).await?;
    let (program, mut args) = match fuse {
        Fuse::RustLld => (
            rust_lld()?,
            vec!["-flavor".to_string(), "darwin".to_string()],
        ),
        _ => (llvm_tool(paths, "ld64.lld").await?, Vec::new()),
    };
    args.extend(darwin_link_args(target, &sdk));
    Ok(PreparedLinker {
        program,
        args,
        sysroot: Some(sdk),
    })
}

fn darwin_link_args(target: &str, sdk: &Path) -> Vec<String> {
    let arch = if target.starts_with("aarch64") {
        "arm64"
    } else {
        "x86_64"
    };
    let min_os = crate::blessed_build::darwin_arch::deployment_target(target);
    vec![
        "-arch".to_string(),
        arch.to_string(),
        "-platform_version".to_string(),
        "macos".to_string(),
        min_os.clone(),
        min_os,
        "-syslibroot".to_string(),
        sdk.display().to_string(),
    ]
}

async fn prepare_windows_gnu(
    paths: &SoldrPaths,
    fuse: Fuse,
    target: &str,
) -> Result<PreparedLinker, SoldrError> {
    let (bin_dir, _env) = crate::fetch::mingw_w64_gcc::prepare_win_gnu_env(paths, target).await?;
    let sysroot = bin_dir
        .parent()
        .map(crate::fetch::mingw_w64_gcc::sysroot_lib_dir)
        .and_then(|lib| lib.parent().map(Path::to_path_buf));
    match fuse {
        Fuse::GnuLd => Ok(PreparedLinker {
            program: bin_dir.join(crate::platform::executable::name::native(
                "x86_64-w64-mingw32-ld",
            )),
            args: Vec::new(),
            sysroot,
        }),
        _ => {
            let (program, mut args) = if fuse == Fuse::RustLld {
                (rust_lld()?, vec!["-flavor".to_string(), "gnu".to_string()])
            } else {
                (llvm_tool(paths, "ld.lld").await?, Vec::new())
            };
            args.extend(["-m".to_string(), "i386pep".to_string()]);
            if let Some(lib) = sysroot.as_ref().map(|root| root.join("lib")) {
                args.push(format!("-L{}", lib.display()));
            }
            Ok(PreparedLinker {
                program,
                args,
                sysroot,
            })
        }
    }
}

async fn prepare_msvc(
    paths: &SoldrPaths,
    fuse: Fuse,
    target: &str,
) -> Result<PreparedLinker, SoldrError> {
    let xwin = crate::fetch::xwin_cache::ensure_xwin_cache(paths, target).await?;
    let lib_dirs = crate::blessed_build::xwin_lib_dirs(&xwin, target);
    if lib_dirs.is_empty() {
        return Err(SoldrError::Other(format!(
            "soldr link: the xwin MSVC SDK at {} has no libraries for `{target}`",
            xwin.display()
        )));
    }
    let (program, mut args) = if fuse == Fuse::RustLld {
        (rust_lld()?, vec!["-flavor".to_string(), "link".to_string()])
    } else {
        (llvm_tool(paths, "lld-link").await?, Vec::new())
    };
    args.extend(msvc_link_args(target, &lib_dirs));
    Ok(PreparedLinker {
        program,
        args,
        sysroot: Some(xwin),
    })
}

fn msvc_link_args(target: &str, lib_dirs: &[PathBuf]) -> Vec<String> {
    let machine = if target.starts_with("aarch64") {
        "ARM64"
    } else {
        "X64"
    };
    let mut args = vec![format!("/MACHINE:{machine}")];
    args.extend(
        lib_dirs
            .iter()
            .map(|dir| format!("/LIBPATH:{}", dir.display())),
    );
    args
}

async fn llvm_tool(paths: &SoldrPaths, name: &str) -> Result<PathBuf, SoldrError> {
    let bin = crate::fetch::ensure_llvm_toolchain(paths).await?;
    Ok(bin.join(crate::platform::executable::name::native(name)))
}

fn host_tool(name: &str) -> Result<PathBuf, SoldrError> {
    crate::msvc_host::which_on_path(&crate::platform::executable::name::native(name)).ok_or_else(
        || {
            SoldrError::Other(format!(
                "--fuse={name}: `{name}` is not on PATH; soldr does not provision it"
            ))
        },
    )
}

/// rustup's bundled `rust-lld` for the active toolchain.
fn rust_lld() -> Result<PathBuf, SoldrError> {
    let rustc = crate::binaries::resolve_toolchain_binary("rustc")?;
    let sysroot = crate::platform::wait::output_for(
        Command::new(&rustc).args(["--print", "sysroot"]),
        crate::platform::wait::PROBE,
    )
    .map_err(|error| SoldrError::Other(format!("--fuse=rust-lld: {error}")))?;
    let sysroot = PathBuf::from(String::from_utf8_lossy(&sysroot.stdout).trim());
    let host = crate::fetch::cmake_tools::current_host_triple();
    let path = sysroot
        .join("lib")
        .join("rustlib")
        .join(host)
        .join("bin")
        .join(crate::platform::executable::name::native("rust-lld"));
    if path.is_file() {
        Ok(path)
    } else {
        Err(SoldrError::Other(format!(
            "--fuse=rust-lld: {} does not exist",
            path.display()
        )))
    }
}

#[cfg(test)]
#[path = "ld_cmd_tests.rs"]
mod tests;
