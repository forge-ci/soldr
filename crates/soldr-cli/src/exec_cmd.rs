//! `soldr exec <cmd>` — issue #1059 escape hatch.
//!
//! Runs an arbitrary command with rustup's toolchain bin dir prepended
//! to PATH, so any subprocess the command spawns finds rustup's
//! `cargo` (and therefore the rustup proxy, which DOES honor per-crate
//! `rust-toolchain.toml` overrides) before any Chocolatey/scoop/
//! standalone shim that may also be on PATH.
//!
//! Resolution flow:
//!
//!   1. Resolve `cargo` via the shared `resolve_toolchain_binary`
//!      helper. This either calls `rustup which cargo` or hits one of
//!      the direct-probe shortcuts in `binaries.rs`.
//!   2. Take its containing directory (the rustup toolchain `bin/`).
//!   3. Build a new `PATH` value with that dir prepended.
//!   4. Resolve the user-supplied `<cmd>` against the new PATH so
//!      `soldr exec cargo-dylint ...` works even when `cargo-dylint`
//!      lives under `~/.cargo/bin/`.
//!   5. Spawn the resolved binary with the new PATH and the remaining
//!      args, forwarding stdout / stderr unchanged.
//!
//! The wrapped command inherits the current process env otherwise. We
//! deliberately don't set `RUSTUP_TOOLCHAIN` or `CARGO` — those wouldn't
//! help against the very subprocess-y `cargo-dylint` style hardcoded
//! `"cargo"` invocations the issue is about. The PATH-prepend is the
//! load-bearing change.
//!
//! Issue #1417 adds a child-only Soldr shim layer ahead of the rustup
//! bin dir when `SOLDR_CHILD_SHIMS_ACTIVE` is not already set and
//! `SOLDR_DISABLE_CHILD_SHIMS` is not truthy. Explicit `CARGO`,
//! `RUSTC`, and `RUSTC_WRAPPER` values are inherited rather than
//! rewritten here; hardcoded PATH lookups for shimmed tools route back
//! through Soldr by default, and users can opt out with
//! `SOLDR_DISABLE_CHILD_SHIMS=1`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::{suppress_windows_console_window, SoldrError};

/// Implementation of `soldr exec`. Returns the child process exit code
/// (or 127 on spawn failure, matching the POSIX "command not found"
/// convention).
pub fn run_exec(args: &[String]) -> Result<i32, SoldrError> {
    if args.is_empty() {
        return Err(SoldrError::Other(
            "soldr exec: missing command — usage: soldr exec <cmd> [args...]".to_string(),
        ));
    }

    let cargo = resolve_rustup_cargo()?;
    let rustup_bin_dir = cargo.parent().ok_or_else(|| {
        SoldrError::Other(format!(
            "soldr exec: resolved cargo has no parent dir: {}",
            cargo.display()
        ))
    })?;

    let rustup_path = path_with_prepend(rustup_bin_dir);
    let shim_guard = if crate::shim_dir::should_install_shims() {
        Some(crate::shim_dir::build_shim_dir()?)
    } else {
        None
    };
    let child_path = match shim_guard.as_ref() {
        Some(guard) => path_with_prepend_using(&guard.path, rustup_path.as_os_str()),
        None => rustup_path,
    };

    let (cmd, cmd_args) = args.split_first().expect("non-empty checked above");
    // Resolve `cmd` against the NEW path so the rustup-bin lookup wins.
    let resolved_cmd = find_on_path(cmd, &child_path).unwrap_or_else(|| PathBuf::from(cmd));

    eprintln!(
        "soldr exec: PATH prefix {} | running {} {}",
        exec_path_prefix_for_display(
            shim_guard.as_ref().map(|guard| guard.path.as_path()),
            rustup_bin_dir,
        ),
        resolved_cmd.display(),
        shell_quote(cmd_args)
    );

    let mut command = Command::new(&resolved_cmd);
    command.args(cmd_args);
    command.env("PATH", &child_path);
    if shim_guard.is_some() {
        command.env(crate::shim_dir::SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR, "1");
    }
    suppress_windows_console_window(&mut command);

    let status = crate::platform::wait::status_for(&mut command, crate::platform::wait::LONG_CHILD)
        .map_err(|e| {
            SoldrError::Other(format!(
                "soldr exec: failed to spawn {}: {e}",
                resolved_cmd.display()
            ))
        })?;
    Ok(status.code().unwrap_or(127))
}

fn exec_path_prefix_for_display(shim_dir: Option<&Path>, rustup_bin_dir: &Path) -> String {
    match shim_dir {
        Some(shim_dir) => format!("{} -> {}", shim_dir.display(), rustup_bin_dir.display()),
        None => rustup_bin_dir.display().to_string(),
    }
}

/// Build a new PATH-style env var value with `prepend_dir` placed first
/// (and de-duplicated against an existing copy in the rest of PATH).
pub fn path_with_prepend(prepend_dir: &Path) -> std::ffi::OsString {
    let current = std::env::var_os("PATH").unwrap_or_default();
    path_with_prepend_using(prepend_dir, current.as_os_str())
}

/// Pure-function form of [`path_with_prepend`] — takes the base PATH
/// value explicitly so tests don't race on the process env.
pub fn path_with_prepend_using(prepend_dir: &Path, base: &std::ffi::OsStr) -> std::ffi::OsString {
    let mut entries: Vec<PathBuf> = std::env::split_paths(base)
        .filter(|p| p != prepend_dir)
        .collect();
    entries.insert(0, prepend_dir.to_path_buf());
    std::env::join_paths(entries).unwrap_or_else(|_| base.to_os_string())
}

/// Look up `name` against a PATH-shape value (NOT the process env).
/// Used so `soldr exec` resolves the command against its own
/// PATH-prepended view rather than the unmodified process PATH.
///
/// Delegates to the host-platform facade: the candidate suffix list is
/// the Windows PATHEXT (no-op on other hosts), and path-like arguments
/// are trusted as explicit paths by the shared walker.
pub fn find_on_path(name: &str, path_value: &std::ffi::OsStr) -> Option<PathBuf> {
    crate::platform::executable::search::find_on_path(name, path_value)
}

/// Resolve the path of rustup's `cargo` binary. soldr#1059's escape
/// hatch is specifically for hosts where `which cargo` returns a
/// shadowing standalone — we deliberately DO NOT consult PATH here.
/// Lookup order:
///
///   1. `RUSTUP_HOME` / default rustup home → `<home>/bin/cargo[.exe]`
///      (the rustup proxy).
///   2. `rustup which cargo` shelled out (covers the case where
///      rustup is on PATH under a non-standard home).
///
/// Returns a `SoldrError` when neither resolution path produces a
/// real file. The error message names both attempts so the user
/// knows whether to install rustup or set `RUSTUP_HOME`.
fn resolve_rustup_cargo() -> Result<PathBuf, SoldrError> {
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        let candidate = PathBuf::from(home)
            .join("bin")
            .join(crate::platform::executable::name::native("cargo"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Some(home) = dirs_home_dir() {
        let candidate = home
            .join(".cargo")
            .join("bin")
            .join(crate::platform::executable::name::native("cargo"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    // Fallback to `rustup which cargo`.
    let rustup = crate::platform::executable::name::native("rustup");
    let mut command = Command::new(rustup);
    command.args(["which", "cargo"]);
    suppress_windows_console_window(&mut command);
    if let Ok(out) = crate::platform::wait::output_for(&mut command, crate::platform::wait::PROBE) {
        if out.status.success() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !path.is_empty() {
                let pb = PathBuf::from(path);
                if pb.is_file() {
                    return Ok(pb);
                }
            }
        }
    }
    Err(SoldrError::Other(
        "soldr exec: could not resolve rustup's cargo binary; \
         tried $CARGO_HOME/bin, ~/.cargo/bin, and `rustup which cargo`. \
         Install rustup or set CARGO_HOME to a rustup install."
            .to_string(),
    ))
}

fn dirs_home_dir() -> Option<PathBuf> {
    crate::platform::host::dirs::home()
}

fn shell_quote(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.contains(' ') || a.contains('"') {
                format!("{a:?}")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn path_with_prepend_inserts_dir_first() {
        // Pure-function form — no env mutation, no race with sibling
        // tests under parallel `cargo test`.
        let sep = crate::platform::host::facts::path_list_separator();
        let base: std::ffi::OsString = format!("/a/b{sep}/c/d").into();
        let prepended = path_with_prepend_using(Path::new("/x/y"), base.as_os_str());
        let entries: Vec<PathBuf> = std::env::split_paths(&prepended).collect();
        assert_eq!(
            entries.first().map(|p| p.as_path()),
            Some(Path::new("/x/y"))
        );
        assert!(
            entries.iter().any(|p| p == Path::new("/a/b")),
            "missing /a/b in {entries:?}",
        );
        assert!(
            entries.iter().any(|p| p == Path::new("/c/d")),
            "missing /c/d in {entries:?}",
        );
    }

    #[test]
    fn path_with_prepend_deduplicates_existing() {
        let sep = crate::platform::host::facts::path_list_separator();
        let base: std::ffi::OsString = format!("/x/y{sep}/a/b").into();
        let prepended = path_with_prepend_using(Path::new("/x/y"), base.as_os_str());
        let entries: Vec<PathBuf> = std::env::split_paths(&prepended).collect();
        // `/x/y` should appear exactly once — the duplicate should have
        // been dropped before we prepended.
        let count = entries.iter().filter(|p| p == &Path::new("/x/y")).count();
        assert_eq!(
            count, 1,
            "expected exactly one /x/y entry, got {count} in {entries:?}"
        );
    }

    #[test]
    fn find_on_path_locates_executable() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let exe = tmp
            .path()
            .join(crate::platform::executable::name::native("myexe"));
        std::fs::write(&exe, b"x").unwrap();
        if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
            // The find_on_path probe requires an executable bit; the
            // facade applies a fixed 0o755 on Unix and is a no-op on
            // Windows (where the .exe extension decides executability).
            let perms = std::fs::metadata(&exe).unwrap().permissions();
            crate::platform::fs::permissions::make_executable_from(&exe, &perms).unwrap();
        }
        let path_value: std::ffi::OsString =
            std::env::join_paths(std::iter::once(tmp.path())).unwrap();
        let resolved =
            find_on_path("myexe", path_value.as_os_str()).expect("should find the fake executable");
        assert_eq!(resolved, exe);
    }

    #[test]
    fn find_on_path_passthrough_for_explicit_path() {
        // When the name already contains a separator, treat it as a
        // direct path and skip PATH lookup.
        let path_value: std::ffi::OsString = "/this/does/not/exist".into();
        let resolved = find_on_path("/explicit/path/binary", path_value.as_os_str());
        assert_eq!(
            resolved.as_deref(),
            Some(Path::new("/explicit/path/binary"))
        );
    }
}
