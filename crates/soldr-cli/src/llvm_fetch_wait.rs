//! The wrapper-side half of the background managed-LLVM fetch (soldr#3801).
//!
//! The front door starts the ~95 MiB LLVM download on a background thread and
//! exports `SOLDR_LLVM_FETCH_PENDING_DIR` naming the install directory. A
//! wrapper process then waits for that install only for a compile that will
//! *link* through the managed linker shim; every other compile proceeds
//! immediately. The wait is bounded (`SOLDR_LLVM_FETCH_WAIT_SECS`) and a
//! failure is recorded per build, like `route_refusal_sentinel`, so N linking
//! units do not each retry the download.

use crate::core::SoldrPaths;
use std::path::{Path, PathBuf};
use std::time::Duration;

const FAILURE_DIR: &str = "llvm-fetch-failures";

/// Does this rustc invocation run the linker?
///
/// Links when `--emit` is absent or names `link`, **and** the crate type is
/// `bin`, `cdylib`, `dylib` or `proc-macro`, is absent (defaults to `bin`),
/// or `--test` is present. Metadata-only (`--emit=metadata`) does not link,
/// and neither does `staticlib` / `lib` / `rlib`: no linker driver runs.
/// Build scripts are plain `bin` crates, so they link.
pub(crate) fn invocation_links(args: &[String]) -> bool {
    emit_links(args) && crate_type_links(args)
}

/// Every value of a flag spelled `--flag v`, `--flag=v`.
fn long_flag_values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
    let joined = format!("{flag}=");
    let mut out = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            if let Some(value) = iter.next() {
                out.push(value.as_str());
            }
        } else if let Some(value) = arg.strip_prefix(&joined) {
            out.push(value);
        }
    }
    out
}

fn emit_links(args: &[String]) -> bool {
    let emits = long_flag_values(args, "--emit");
    emits.is_empty()
        || emits
            .iter()
            .flat_map(|value| value.split(','))
            .any(|kind| kind.split('=').next().unwrap_or(kind).trim() == "link")
}

fn crate_type_links(args: &[String]) -> bool {
    if args.iter().any(|arg| arg == "--test") {
        return true;
    }
    let types = long_flag_values(args, "--crate-type");
    types.is_empty()
        || types
            .iter()
            .flat_map(|value| value.split(','))
            .any(|kind| matches!(kind.trim(), "bin" | "cdylib" | "dylib" | "proc-macro"))
}

/// The value of the last `-C linker=...` (any spelling) in `args`.
pub(crate) fn linker_arg(args: &[String]) -> Option<&str> {
    let mut found = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let codegen = if arg == "-C" || arg == "--codegen" {
            iter.next().map(String::as_str)
        } else if let Some(rest) = arg.strip_prefix("--codegen=") {
            Some(rest)
        } else {
            arg.strip_prefix("-C")
        };
        if let Some(value) = codegen.and_then(|c| c.strip_prefix("linker=")) {
            found = Some(value);
        }
    }
    found
}

/// Is `linker` the generated managed-clang shim (`clang-<20 hex>`; see
/// `linker_shim::write_linker_shim`), by bare name or path? The host-`cc` shim
/// (`cc-<digest>`) needs no LLVM.
pub(crate) fn names_managed_shim(linker: &str) -> bool {
    let Some(name) = Path::new(linker).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let suffix = crate::platform::executable::name::script_suffix();
    let stem = name.strip_suffix(suffix).unwrap_or(name);
    stem.strip_prefix("clang-")
        .is_some_and(|digest| digest.len() == 20 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Should this compile wait on the pending LLVM fetch?
pub(crate) fn needs_managed_llvm(args: &[String]) -> bool {
    invocation_links(args) && linker_arg(args).is_some_and(names_managed_shim)
}

fn failure_path(paths: &SoldrPaths, build_id: u64) -> PathBuf {
    paths
        .cache
        .join(FAILURE_DIR)
        .join(format!("build-{build_id}.failed"))
}

/// The recorded failure of this build's LLVM fetch, if one was recorded.
fn recorded_failure(paths: &SoldrPaths, build_id: Option<u64>) -> Option<String> {
    let path = failure_path(paths, build_id?);
    std::fs::read_to_string(path).ok()
}

/// Record `message` as this build's LLVM fetch failure (first writer wins).
fn record_failure(paths: &SoldrPaths, build_id: Option<u64>, message: &str) {
    let Some(id) = build_id else { return };
    let path = failure_path(paths, id);
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    crate::route_refusal_sentinel::sweep_stale(dir);
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    if std::fs::write(&temp, message).is_ok() && !path.exists() {
        let _ = std::fs::rename(&temp, &path);
    }
    let _ = std::fs::remove_file(&temp);
}

/// Wait (when needed) for the front door's background LLVM fetch before this
/// compile is dispatched. `Err` carries the message the compile fails with.
pub(crate) fn gate_linking_compile(rustc_argv: &[String]) -> Result<(), String> {
    let Some(pending) = std::env::var_os(crate::cache_lib::SOLDR_LLVM_FETCH_PENDING_ENV_VAR) else {
        return Ok(());
    };
    let paths = SoldrPaths::new().ok();
    gate_with(
        rustc_argv,
        Path::new(&pending),
        paths.as_ref(),
        crate::wrapper_target::read_build_session_id_env(),
        crate::fetch::llvm_fetch_wait_from_env(),
        |paths, install_dir, timeout| {
            let paths = paths.clone();
            let install_dir = install_dir.to_path_buf();
            // A dedicated thread: the caller may already be inside a runtime.
            let handle = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                crate::platform::wait::block_on_for(
                    &runtime,
                    crate::fetch::wait_for_managed_llvm(&paths, &install_dir, timeout),
                    timeout + crate::platform::wait::PROBE,
                )
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())
            });
            crate::platform::wait::join_for(handle, timeout + crate::platform::wait::HELPER_JOIN)
                .unwrap_or_else(|_| Err("the LLVM wait thread panicked or overran".to_string()))
        },
    )
}

/// [`gate_linking_compile`] with its environment and the wait itself
/// supplied, so tests drive every branch without a network or a runtime.
pub(crate) fn gate_with(
    rustc_argv: &[String],
    install_dir: &Path,
    paths: Option<&SoldrPaths>,
    build_id: Option<u64>,
    timeout: Duration,
    wait: impl FnOnce(&SoldrPaths, &Path, Duration) -> Result<(), String>,
) -> Result<(), String> {
    if !needs_managed_llvm(rustc_argv) || crate::fetch::llvm_install_complete(install_dir) {
        return Ok(());
    }
    let Some(paths) = paths else {
        return Err(crate::linker_shim::missing_clang_error_text(
            "the soldr cache root could not be resolved",
        ));
    };
    if let Some(message) = recorded_failure(paths, build_id) {
        return Err(message);
    }
    match wait(paths, install_dir, timeout) {
        Ok(()) => Ok(()),
        Err(cause) => {
            let message = crate::linker_shim::missing_clang_error_text(&cause);
            record_failure(paths, build_id, &message);
            Err(message)
        }
    }
}

#[cfg(test)]
#[path = "llvm_fetch_wait_tests.rs"]
mod tests;
