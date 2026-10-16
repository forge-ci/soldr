//! Content-addressed Linux clang driver shims, and reld path injection.
//!
//! Split out of `linker.rs` to stay under the per-file line ceiling
//! (soldr#3276): this module has one cohesive job -- materializing a
//! driver-only executable shim so a Linux clang driver argument never enters
//! Cargo's `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`, which would silently replace a
//! project's own `[build] rustflags` (soldr#3277) -- plus the reld-specific
//! path substitution that runs just before a shim is materialized.

use crate::core::SoldrError;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::linker::{target_kind, LinkerInjection, TargetKind};

pub(crate) mod rustc_lld;

/// Path-valued override: presence of a path is the legitimate meaning, so this
/// is deliberately a presence check, not a `core::flag` boolean.
const LLVM_DIR_ENV_VAR: &str = "SOLDR_LLVM_DIR";

const LINKER_SHIM_FORMAT_VERSION: &str = "v1";
static LINKER_SHIM_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Replace the bare `reld` token in an explicit linker injection with a
/// verified absolute executable path resolved before Cargo starts.
pub fn inject_resolved_reld(
    injection: &mut LinkerInjection,
    reld: &Path,
) -> Result<(), SoldrError> {
    let reld = reld.to_str().ok_or_else(|| {
        SoldrError::Other(format!(
            "managed reld path is not valid UTF-8: {}",
            reld.display()
        ))
    })?;
    let mut replaced = false;
    if injection.linker.as_deref() == Some("reld") {
        injection.linker = Some(reld.to_string());
        replaced = true;
    }
    if let Some(flags) = injection.rustflags.as_mut() {
        if flags.contains("--ld-path=reld") {
            *flags = flags.replacen("--ld-path=reld", &format!("--ld-path={reld}"), 1);
            replaced = true;
        }
    }
    if replaced {
        Ok(())
    } else {
        Err(SoldrError::Other(
            "explicit reld linker selection produced no reld injection".to_string(),
        ))
    }
}

/// Move a Linux or Apple clang driver argument out of target-scoped rustflags and into
/// a content-addressed executable shim.
///
/// Cargo does not merge `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` with
/// `[build] rustflags`; the environment value replaces the project setting.
/// A linker shim carries the driver-only argument without entering Cargo's
/// rustflags precedence at all. Its content digest is embedded in the path so
/// the existing `CARGO_TARGET_*_LINKER` cache-key input changes whenever the
/// generated command changes.
pub fn materialize_linker_driver_shim(
    paths: &crate::core::SoldrPaths,
    target: &str,
    injection: &mut LinkerInjection,
    clang: Option<&Path>,
) -> Result<(), SoldrError> {
    let Some(driver_arg) = shim_driver_arg(target, injection) else {
        return Ok(());
    };
    let driver_arg = driver_arg.to_string();

    let body = render_linker_driver_shim(clang, &driver_arg)?;
    let path = write_linker_shim(paths, "clang", &body)?;

    injection.linker = Some(path.to_string_lossy().into_owned());
    injection.rustflags = None;
    Ok(())
}

/// The `CARGO_TARGET_*_LINKER` value to hand Cargo for `linker`.
///
/// soldr#3743 (Perf Matrix `cold-tar-untar-warm` at 0% hits): a generated
/// shim lives under the cache root, and Cargo passes this value to every
/// rustc as `-C linker=...`, which is part of the compile's cache key. An
/// absolute shim path therefore made every unit miss after `soldr load`
/// restored the cache under a different root (and across worktrees and CI
/// runners). A shim is passed by file name instead, with its directory put on
/// the cargo child's `PATH`: the name still carries the shim's content digest,
/// so the key changes exactly when the generated command does, and nothing
/// root-specific enters it. Any other linker value is returned unchanged.
pub(crate) fn linker_value_for_command(
    command: &mut std::process::Command,
    linker: &str,
) -> Result<String, SoldrError> {
    let path = Path::new(linker);
    let in_shim_dir = path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .is_some_and(|name| name == "linker-shims");
    let (Some(dir), Some(name), true) = (path.parent(), path.file_name(), in_shim_dir) else {
        return Ok(linker.to_string());
    };
    prepend_command_path(command, dir)?;
    Ok(name.to_string_lossy().into_owned())
}

/// Put `dir` first on `command`'s `PATH` (its own value when set, else this
/// process's).
pub(crate) fn prepend_command_path(
    command: &mut std::process::Command,
    dir: &Path,
) -> Result<(), SoldrError> {
    let existing = command
        .get_envs()
        .find(|(key, _)| *key == std::ffi::OsStr::new("PATH"))
        .map(|(_, value)| value.map(std::ffi::OsStr::to_os_string))
        .unwrap_or_else(|| std::env::var_os("PATH"));
    let mut entries = vec![dir.to_path_buf()];
    if let Some(existing) = existing {
        entries.extend(std::env::split_paths(&existing));
    }
    let joined = std::env::join_paths(entries)
        .map_err(|error| SoldrError::Other(format!("invalid PATH: {error}")))?;
    command.env("PATH", joined);
    Ok(())
}

/// Write `body` to `<bin>/linker-shims/v1/<stem>-<digest>`, the content
/// digest in the name so the `CARGO_TARGET_*_LINKER` cache-key input changes
/// whenever the generated command does.
fn write_linker_shim(
    paths: &crate::core::SoldrPaths,
    stem: &str,
    body: &str,
) -> Result<PathBuf, SoldrError> {
    let digest = format!("{:x}", Sha256::digest(body.as_bytes()));
    let dir = paths
        .bin
        .join("linker-shims")
        .join(LINKER_SHIM_FORMAT_VERSION);
    std::fs::create_dir_all(&dir)?;
    let suffix = crate::platform::executable::name::script_suffix();
    let path = dir.join(format!("{stem}-{}{suffix}", &digest[..20]));
    write_content_addressed_shim(&path, body)?;
    Ok(path)
}

/// The clang driver argument that a shim would carry, or `None` when this
/// target/injection needs no shim.
fn shim_driver_arg<'a>(target: &str, injection: &'a LinkerInjection) -> Option<&'a str> {
    if !matches!(target_kind(target), TargetKind::Linux | TargetKind::Apple)
        || injection.linker.as_deref() != Some("clang")
    {
        return None;
    }
    injection.rustflags.as_deref()?.strip_prefix("-C link-arg=")
}

/// [`materialize_linker_driver_shim`] after resolving the `clang` it will
/// exec (soldr#3430): a managed LLVM already on disk, else the `clang` on the
/// search path, else the catalogued LLVM fetched just in time. The shim names
/// that absolute path, so a host without a system clang gets one clear error
/// from soldr here rather than `exec: clang: not found` (exit 127) from some
/// dependency's build script.
///
/// `rustc_supplies_lld` (see [`rustc_lld`], soldr#3797) says rustc's own
/// bundled lld will serve the link, so a system clang without an `lld` is
/// still a usable driver and nothing is fetched.
pub async fn materialize_linker_driver_shim_resolving(
    paths: &crate::core::SoldrPaths,
    target: &str,
    injection: &mut LinkerInjection,
    rustc_supplies_lld: bool,
) -> Result<(), SoldrError> {
    let Some(driver_arg) = shim_driver_arg(target, injection) else {
        return materialize_linker_driver_shim(paths, target, injection, None);
    };
    let clang = resolve_driver_clang(
        paths,
        LldNeed::new(driver_arg_needs_lld(driver_arg), rustc_supplies_lld),
    )
    .await?;
    let search = std::env::var_os("PATH");
    let on_path = |name: &str| {
        search
            .as_deref()
            .and_then(|search| crate::exec_cmd::find_on_path(name, search))
    };
    if let Some(cc) = host_cc_fallback(
        target,
        crate::platform::host::facts::triple(),
        linux_host(),
        &clang,
        on_path("clang").as_deref(),
        on_path("cc"),
    ) {
        return materialize_host_cc_shim(paths, injection, &cc);
    }
    materialize_linker_driver_shim(paths, target, injection, Some(&clang))
}

/// The host `cc` that should drive a native Linux link instead of `clang`,
/// or `None` to keep `clang`.
///
/// A NixOS host with only the gcc cc-wrapper on the search path has no
/// system clang for [`prefer_host_wrapper_clang`] to yield to, and the
/// managed clang cannot see the C runtime there (`cannot open Scrt1.o`,
/// `unable to find library -lc`): glibc and `libgcc_s` live only under
/// `/nix/store`, known to the wrapper alone. The host `cc` then drives the
/// link with its own default linker -- what users did by hand with
/// `CARGO_TARGET_<TRIPLE>_LINKER=$(which cc)`. The fast-linker argument is
/// dropped because gcc does not accept clang's `--ld-path=`.
///
/// Only when every condition holds: a Linux host, a native (host-triple)
/// target -- a cross target never links through the host compiler -- a
/// clang that is not the system clang, that ran and could not find the
/// runtime, and a host `cc` that ran and could. An FHS host (Debian,
/// Ubuntu, Fedora) keeps the clang. A system clang chosen by
/// [`prefer_host_wrapper_clang`] is never demoted: the NixOS clang-wrapper
/// answers `-print-file-name=libgcc_s.so` with the bare name (it adds that
/// search path only when linking) yet links fine.
fn host_cc_fallback(
    target: &str,
    host_triple: &str,
    linux_host: bool,
    clang: &Path,
    system_clang: Option<&Path>,
    host_cc: Option<PathBuf>,
) -> Option<PathBuf> {
    if !linux_host || target != host_triple || system_clang == Some(clang) {
        return None;
    }
    let cc = host_cc?;
    if probe_host_runtime(clang) != Some(false) {
        return None;
    }
    (probe_host_runtime(&cc) == Some(true)).then_some(cc)
}

/// A content-addressed shim that execs the host `cc` with the link argv
/// unchanged (see [`host_cc_fallback`]).
fn materialize_host_cc_shim(
    paths: &crate::core::SoldrPaths,
    injection: &mut LinkerInjection,
    cc: &Path,
) -> Result<(), SoldrError> {
    let cc = cc.to_str().ok_or_else(|| {
        SoldrError::Other(format!("cc path is not valid UTF-8: {}", cc.display()))
    })?;
    let body = format!(
        "#!/bin/sh\n# generated by soldr; content-addressed host cc linker driver\nexec {} \"$@\"\n",
        shell_single_quote(cc)
    );
    let path = write_linker_shim(paths, "cc", &body)?;
    injection.linker = Some(path.to_string_lossy().into_owned());
    injection.rustflags = None;
    Ok(())
}

fn windows_host() -> bool {
    crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
}

/// Does this driver argument make clang look for `lld`? (`-fuse-ld=lld`, the
/// Fast path's fallback when reld is unavailable.) A `--ld-path=<reld>` argument
/// names its own linker.
fn driver_arg_needs_lld(driver_arg: &str) -> bool {
    driver_arg.starts_with("-fuse-ld=lld")
}

/// Is an `lld` driver reachable from this `clang`: beside it, or on the search
/// path? A system clang without one fails the link one step later with
/// "invalid linker name in argument '-fuse-ld=lld'" (soldr#3430).
fn lld_reachable(clang: &Path, search: Option<&std::ffi::OsStr>) -> bool {
    const NAMES: [&str; 3] = ["ld.lld", "ld64.lld", "lld"];
    let suffix = std::env::consts::EXE_SUFFIX;
    let beside = clang.parent().is_some_and(|dir| {
        NAMES
            .iter()
            .any(|name| dir.join(format!("{name}{suffix}")).is_file())
    });
    beside
        || search.is_some_and(|search| {
            NAMES
                .iter()
                .any(|name| crate::exec_cmd::find_on_path(name, search).is_some())
        })
}

/// Whether the driver needs an `lld` of its own, and whether rustc supplies
/// one regardless (soldr#3797).
#[derive(Clone, Copy)]
struct LldNeed {
    needs_lld: bool,
    rustc_supplies_lld: bool,
}

impl LldNeed {
    fn new(needs_lld: bool, rustc_supplies_lld: bool) -> Self {
        Self {
            needs_lld,
            rustc_supplies_lld,
        }
    }
}

/// A system clang is enough unless the driver needs an lld it lacks and
/// rustc will not supply one.
fn usable_system_clang(
    system_clang: Option<&PathBuf>,
    need: LldNeed,
    search: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    system_clang
        .filter(|clang| !need.needs_lld || need.rustc_supplies_lld || lld_reachable(clang, search))
        .cloned()
}

async fn resolve_driver_clang(
    paths: &crate::core::SoldrPaths,
    need: LldNeed,
) -> Result<PathBuf, SoldrError> {
    // The one canonical install path (it carries the bundle revision, soldr#3810).
    let managed_complete = crate::fetch::managed_llvm_install_dir(paths).join(".complete");
    let search = std::env::var_os("PATH");
    let system_clang = search
        .as_deref()
        .and_then(|search| crate::exec_cmd::find_on_path("clang", search));
    let usable_system_clang = usable_system_clang(system_clang.as_ref(), need, search.as_deref());
    if managed_complete.is_file() || std::env::var_os(LLVM_DIR_ENV_VAR).is_some() {
        if let Ok(bin) = crate::fetch::ensure_llvm_toolchain(paths).await {
            if let Some(clang) = clang_in(&bin) {
                return Ok(prefer_host_wrapper_clang(
                    clang,
                    usable_system_clang,
                    linux_host(),
                    managed_clang_finds_host_runtime,
                ));
            }
        }
    }
    if let Some(clang) = usable_system_clang {
        return Ok(clang);
    }
    let managed = crate::fetch::ensure_llvm_toolchain(paths)
        .await
        .map_err(|error| error.to_string())
        .and_then(|bin| {
            clang_in(&bin)
                .ok_or_else(|| format!("no clang in the fetched LLVM at {}", bin.display()))
        });
    match pick_after_managed(system_clang, managed) {
        Ok((clang, warning)) => {
            if let Some(warning) = warning {
                eprintln!("{warning}");
            }
            Ok(clang)
        }
        Err(cause) => Err(missing_clang_error(&cause)),
    }
}

fn linux_host() -> bool {
    crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Linux
}

/// The host C runtime files a Linux link needs from the clang driver: the
/// PIE startup object and the `libgcc_s` that Rust's `*-linux-gnu` std links.
const HOST_RUNTIME_PROBES: [&str; 2] = ["Scrt1.o", "libgcc_s.so"];

/// Keep the managed clang only when it can see the host's C runtime
/// (soldr#3520). Soldr's managed LLVM is an unwrapped clang: it searches the
/// FHS library directories and nothing else. On a host whose own compiler is
/// a wrapper that injects the platform's library search paths at link time --
/// the NixOS cc-wrapper, where `libgcc_s.so` and `Scrt1.o` exist only under
/// `/nix/store/...` -- the managed driver cannot find them and every link
/// fails (`reld: error: Couldn't find library gcc_s`). There the system clang
/// (the wrapper) drives the link instead, as soldr 0.9.26's `exec clang` shim
/// did; the linker argument (`--ld-path=<reld>` or `-fuse-ld=lld`) is
/// unchanged. Linux hosts only: Windows and macOS keep the managed clang.
fn prefer_host_wrapper_clang(
    managed: PathBuf,
    usable_system_clang: Option<PathBuf>,
    linux_host: bool,
    finds_host_runtime: impl FnOnce(&Path) -> bool,
) -> PathBuf {
    match usable_system_clang {
        Some(system) if linux_host && !finds_host_runtime(&managed) => system,
        _ => managed,
    }
}

/// Does `clang -print-file-name=<f>` resolve every [`HOST_RUNTIME_PROBES`]
/// file? clang echoes the bare name back when its search paths lack the file.
/// A clang that cannot be run counts as finding them, so a probe failure
/// never changes the driver.
fn managed_clang_finds_host_runtime(clang: &Path) -> bool {
    probe_host_runtime(clang).unwrap_or(true)
}

/// `Some(true)` when `compiler -print-file-name=<f>` resolves every
/// [`HOST_RUNTIME_PROBES`] file, `Some(false)` when one comes back bare, and
/// `None` when the compiler could not be run or failed.
fn probe_host_runtime(compiler: &Path) -> Option<bool> {
    for file in HOST_RUNTIME_PROBES {
        let mut command = std::process::Command::new(compiler);
        command.arg(format!("-print-file-name={file}"));
        crate::core::suppress_windows_console_window(&mut command);
        let output = command.output().ok()?;
        if !output.status.success() {
            return None;
        }
        if !Path::new(String::from_utf8_lossy(&output.stdout).trim()).is_absolute() {
            return Some(false);
        }
    }
    Some(true)
}

/// The last step of resolution: the managed LLVM was wanted (there is no
/// system clang, or it lacks an lld the driver needs). If it could not be
/// provided, a system clang that exists is still used -- with a warning, since
/// the link may then fail on the missing lld -- which is what happened before
/// this resolution existed. Only having neither is an error (soldr#3430).
fn pick_after_managed(
    system_clang: Option<PathBuf>,
    managed: Result<PathBuf, String>,
) -> Result<(PathBuf, Option<String>), String> {
    match (managed, system_clang) {
        (Ok(clang), _) => Ok((clang, None)),
        (Err(cause), Some(system)) => {
            let warning = format!(
                "soldr: warning: managed LLVM unavailable ({cause}); using the system clang at {} \
                 although no lld was found next to it or on the search path, so the link may \
                 fail with 'invalid linker name'",
                system.display()
            );
            Ok((system, Some(warning)))
        }
        (Err(cause), None) => Err(cause),
    }
}

fn clang_in(bin_dir: &Path) -> Option<PathBuf> {
    let clang = bin_dir.join(format!("clang{}", std::env::consts::EXE_SUFFIX));
    clang.is_file().then_some(clang)
}

fn missing_clang_error(cause: &str) -> SoldrError {
    SoldrError::Other(missing_clang_error_text(cause))
}

pub(crate) fn missing_clang_error_text(cause: &str) -> String {
    format!(
        "the linker driver needs `clang`, but none is on the search path and soldr could not \
         provide one (managed LLVM v{}, catalogue asset llvm-{}): {cause}. Install clang, or \
         set SOLDR_LLVM_DIR to an existing LLVM bin directory.",
        crate::fetch::MANAGED_LLVM_VERSION,
        crate::fetch::MANAGED_LLVM_VERSION,
    )
}

/// The shim body. On Unix it execs an absolute `clang`; a bare `exec clang`
/// is never emitted, so a later search-path change cannot break it and the
/// content digest covers the real driver.
fn render_linker_driver_shim(clang: Option<&Path>, driver_arg: &str) -> Result<String, SoldrError> {
    let clang = clang.ok_or_else(|| {
        SoldrError::Other("internal: linker driver shim rendered without a resolved clang".into())
    })?;
    let clang = clang.to_str().ok_or_else(|| {
        SoldrError::Other(format!(
            "clang path is not valid UTF-8: {}",
            clang.display()
        ))
    })?;
    if windows_host() {
        return Ok(render_windows_linker_driver_shim(clang, driver_arg));
    }
    Ok(format!(
        "#!/bin/sh\n# generated by soldr; content-addressed linker driver\nexec {} {} \"$@\"\n",
        shell_single_quote(clang),
        shell_single_quote(driver_arg)
    ))
}

pub(crate) fn render_windows_linker_driver_shim(clang: &str, driver_arg: &str) -> String {
    // The generated values are one driver argument and cannot contain a
    // double quote (Windows paths cannot either). Quoting the whole argument
    // protects spaces and cmd metacharacters; doubling percent signs prevents
    // environment-variable expansion before clang receives the value. The
    // clang is an absolute path for the same reason the Unix shim's is
    // (soldr#3430): a later search-path change cannot break it.
    let clang = clang.replace('%', "%%");
    let escaped = driver_arg.replace('%', "%%");
    format!("@echo off\r\n\"{clang}\" \"{escaped}\" %*\r\n")
}

pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn write_content_addressed_shim(path: &Path, body: &str) -> Result<(), SoldrError> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(body) {
        return Ok(());
    }
    let sequence = LINKER_SHIM_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
    std::fs::write(&temp, body)?;
    crate::platform::fs::permissions::make_executable(&temp).map_err(SoldrError::Io)?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(_) if std::fs::read_to_string(path).ok().as_deref() == Some(body) => {
            let _ = std::fs::remove_file(&temp);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(SoldrError::Io(error))
        }
    }
}

#[cfg(test)]
pub(crate) fn driver_arg_needs_lld_for_tests(driver_arg: &str) -> bool {
    driver_arg_needs_lld(driver_arg)
}

#[cfg(test)]
pub(crate) fn lld_reachable_for_tests(clang: &Path, search: Option<&std::ffi::OsStr>) -> bool {
    lld_reachable(clang, search)
}

#[cfg(test)]
pub(crate) fn pick_after_managed_for_tests(
    system_clang: Option<PathBuf>,
    managed: Result<PathBuf, String>,
) -> Result<(PathBuf, Option<String>), String> {
    pick_after_managed(system_clang, managed)
}

#[cfg(test)]
pub(crate) async fn resolve_driver_clang_for_tests(
    paths: &crate::core::SoldrPaths,
    needs_lld: bool,
    rustc_supplies_lld: bool,
) -> Result<PathBuf, SoldrError> {
    resolve_driver_clang(paths, LldNeed::new(needs_lld, rustc_supplies_lld)).await
}

#[cfg(test)]
pub(crate) fn usable_system_clang_for_tests(
    system_clang: Option<&PathBuf>,
    needs_lld: bool,
    rustc_supplies_lld: bool,
    search: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    usable_system_clang(
        system_clang,
        LldNeed::new(needs_lld, rustc_supplies_lld),
        search,
    )
}

#[cfg(test)]
#[path = "linker_shim_tests.rs"]
mod tests;
