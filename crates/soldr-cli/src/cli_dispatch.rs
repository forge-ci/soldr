//! Argument-parsing and cross-compile dispatch helpers for the `soldr`
//! CLI. Extracted verbatim from `main.rs` (soldr#1368) to keep that file
//! under the 1,500-LOC guard; re-exported at the crate root via
//! `pub(crate) use cli_dispatch::*;` so call sites and the sibling
//! `main_tests.rs` (`use super::*`) resolve these by their original
//! bare names.

use crate::cargo_front_door;
use crate::core::SoldrError;

pub(crate) fn should_self_relocate_for_invocation(raw_args: &[String]) -> bool {
    let user_args = raw_args.get(1..).unwrap_or(&[]);
    let Ok((_, args)) = extract_as_pin(user_args) else {
        return false;
    };

    let mut cache_enabled = true;
    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--no-cache" => {
                cache_enabled = false;
                idx += 1;
            }
            "--zccache" => {
                // Value lives in the next token; skip both so the
                // subcommand check below lands on `cargo` instead of
                // the value.
                idx += 2;
            }
            "--" => return false,
            arg if arg.starts_with('-') => idx += 1,
            "cargo" => {
                return cache_enabled
                    && cargo_front_door::cargo_args_are_cacheable(&args[idx + 1..])
                    && matches!(
                        crate::zccache::rustc_wrapper_mode(),
                        crate::zccache::RustcWrapperMode::ManagedZccache
                    );
            }
            _ => return false,
        }
    }

    false
}

/// soldr#1012 PR 5 — scan `args` for `--target X` (two-arg form) or
/// `--target=X` (single-arg form). Returns the FIRST occurrence; if
/// both forms are present the single-arg form wins by virtue of
/// appearing first in a left-to-right scan (cargo behavior is
/// "last --target wins", but for prep purposes any match is enough
/// because the prep is target-keyed and cargo handles the final
/// dispatch).
pub(crate) fn extract_target_from_args(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(triple) = arg.strip_prefix("--target=") {
            if !triple.is_empty() {
                return Some(triple.to_string());
            }
        }
        if arg == "--target" {
            if let Some(next) = iter.next() {
                if !next.is_empty() {
                    return Some(next.clone());
                }
            }
        }
    }
    None
}

// soldr#2519 removed `pick_cross_subcommand` /
// `pick_cross_subcommand_for_host` / `rewrite_build_args_for_subcommand`
// (added by soldr#882). They existed to route a cross build through
// `cargo xwin` or `cargo zigbuild`, which only ever happened when
// `SOLDR_USE_LEGACY_XWIN` / `SOLDR_USE_LEGACY_ZIGBUILD` was set. With those
// toggles gone every branch returned `None`, so the single production caller
// in `soldr_main_build.rs` could never rewrite argv. soldr now always builds
// cross targets with plain `cargo build` against the blessed,
// sha256-verified sysroot.

pub(crate) fn insert_cargo_config_args(
    mut args: Vec<String>,
    cargo_config_args: &[String],
) -> Vec<String> {
    if cargo_config_args.is_empty() {
        return args;
    }

    let insert_at = if args.first().is_some_and(|arg| arg == "dylint") {
        // soldr#4016: cargo-dylint takes no `--config`; the nested cargo
        // invocation after `--` does.
        match args.iter().position(|arg| arg == "--") {
            Some(index) => index + 1,
            None => {
                args.push("--".into());
                args.len()
            }
        }
    } else if args.first().is_some_and(|arg| arg == "nextest") {
        nextest_inner_command_index(&args)
            .map(|index| index + 1)
            .unwrap_or(1)
    } else if args.first().is_some_and(|arg| arg == "xwin")
        && args.get(1).is_some_and(|arg| arg == "build")
    {
        2
    } else if args.is_empty() {
        0
    } else {
        1
    };
    args.splice(insert_at..insert_at, cargo_config_args.iter().cloned());
    args
}

fn nextest_inner_command_index(args: &[String]) -> Option<usize> {
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return None;
        }
        if matches!(
            arg.as_str(),
            "--color" | "--config-file" | "--tool-config-file"
        ) {
            index += 2;
            continue;
        }
        if arg.starts_with("--color=")
            || arg.starts_with("--config-file=")
            || arg.starts_with("--tool-config-file=")
        {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(index);
    }
    None
}

/// soldr#1079 — bridge between the cargo dispatcher and the MSVC
/// host-discovery module. Resolves the target triple (explicit
/// `--target` first, then `TargetTriple::detect()` for the implicit
/// native default) and asks [`crate::msvc_host`] to inject the
/// vcvars-equivalent env vars when relevant.
///
/// All branches are silent on success / no-op. Discovery errors are
/// printed as a single warning line so the user gets a hint about
/// `SOLDR_MSVC_DISCOVERY=off` if the auto-probe trips on an unusual
/// install — the underlying cargo invocation still runs and emits
/// its own (better) error if it actually needs the env.
pub(crate) async fn ensure_msvc_host_env_for_native(args: &[String]) {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let target = match extract_target_from_args(args) {
        Some(t) => t,
        None => match crate::core::TargetTriple::detect() {
            Ok(t) => t.triple(),
            Err(_) => return,
        },
    };
    match crate::msvc_host::ensure_msvc_env_for_native(&target).await {
        Ok(true) => {
            tracing::debug!(
                target: "soldr::msvc_host",
                target_triple = %target,
                "injected host MSVC env (LIB/INCLUDE/PATH/LIBPATH) for native build"
            );
        }
        Ok(false) => {
            // Skipped — non-windows, non-msvc, opt-out, or already-set.
            // Nothing to log; this is the steady-state branch in dev
            // command prompts.
        }
        Err(err) => {
            eprintln!(
                "soldr: MSVC host discovery failed for {target}: {err}\n\
                 soldr: cargo will still run; set SOLDR_MSVC_DISCOVERY=off to silence this probe."
            );
        }
    }
}

/// soldr#1012 PR 5 — prepend `dir` to the current process's `PATH`
/// env var. Idempotent in the sense that if `dir` is already first
/// on PATH, the value is unchanged (PATH stays clean).
pub(crate) fn prepend_to_path_env(dir: &std::path::Path) {
    let dir = dir.to_path_buf();
    prepend_path_dirs_to_env(std::slice::from_ref(&dir));
}

/// Prepend a complete PATH prefix without reversing its priority. Repeatedly
/// prepending individual entries would turn `[shim, llvm, cmake]` into
/// `[cmake, llvm, shim]`, which lets managed clang bypass the MSVC shim.
pub(crate) fn prepend_path_dirs_to_env(dirs: &[std::path::PathBuf]) {
    if dirs.is_empty() {
        return;
    }
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut existing: Vec<std::path::PathBuf> = std::env::split_paths(&current).collect();
    if existing.starts_with(dirs) {
        return;
    }
    let mut combined = Vec::with_capacity(dirs.len() + existing.len());
    combined.extend_from_slice(dirs);
    combined.append(&mut existing);
    if let Ok(joined) = std::env::join_paths(combined) {
        std::env::set_var("PATH", joined);
    }
}

pub(crate) fn extract_as_pin(args: &[String]) -> Result<(Option<String>, Vec<String>), SoldrError> {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut version: Option<String> = None;
    let mut before_subcommand = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !before_subcommand {
            out.push(arg.clone());
            continue;
        }
        if arg == "--as" {
            let value = iter.next().ok_or_else(|| {
                SoldrError::Other("--as requires a version argument, e.g. --as 0.5.2".into())
            })?;
            if version.is_some() {
                return Err(SoldrError::Other("--as specified more than once".into()));
            }
            if value.is_empty() {
                return Err(SoldrError::Other(
                    "--as version argument must not be empty".into(),
                ));
            }
            version = Some(value.clone());
            continue;
        }
        if let Some(value) = arg.strip_prefix("--as=") {
            if version.is_some() {
                return Err(SoldrError::Other("--as specified more than once".into()));
            }
            if value.is_empty() {
                return Err(SoldrError::Other(
                    "--as= requires a version, e.g. --as=0.5.2".into(),
                ));
            }
            version = Some(value.to_string());
            continue;
        }
        if arg == "--" {
            before_subcommand = false;
            out.push(arg.clone());
            continue;
        }
        if arg.starts_with('-') {
            out.push(arg.clone());
            continue;
        }
        before_subcommand = false;
        out.push(arg.clone());
    }
    Ok((version, out))
}

/// True when the requested version is different from this binary's. A match
/// short-circuits the trampoline so the current in-process soldr handles it.
pub(crate) fn should_trampoline(requested: &str) -> bool {
    let current = env!("CARGO_PKG_VERSION");
    normalize_version(requested) != normalize_version(current)
}

pub(crate) fn normalize_version(v: &str) -> String {
    v.trim().trim_start_matches('v').to_string()
}
