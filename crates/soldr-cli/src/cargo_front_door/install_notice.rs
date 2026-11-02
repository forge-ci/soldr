//! soldr#3960: `soldr cargo install` installs into the root Cargo resolves for
//! the child (`--root`, `CARGO_INSTALL_ROOT`, else soldr's `CARGO_HOME`), which
//! is often soldr's private home and therefore off `PATH`. The install root is
//! left unchanged; after a successful install soldr names each installed
//! binary and, when its directory is not on `PATH`, how to reach it. Porcelain
//! runs carry the same paths as `installed` in the JSON line instead.

use crate::platform::sync::Mutex;
use std::path::{Path, PathBuf};

static INSTALLED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Binaries this process saw `cargo install` place, for the porcelain JSON.
pub(crate) fn installed_paths() -> Vec<String> {
    INSTALLED.lock().clone()
}

/// The `--root` value from `cargo install` arguments, if any.
fn explicit_root(args: &[String]) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--root" {
            return iter.next().map(PathBuf::from);
        }
        if let Some(value) = arg.strip_prefix("--root=") {
            return Some(PathBuf::from(value));
        }
    }
    None
}

/// The install root Cargo uses: `--root`, then `CARGO_INSTALL_ROOT`, then
/// `CARGO_HOME`. `child_env` is the child's environment lookup.
pub(super) fn install_root(
    args: &[String],
    child_env: &dyn Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    explicit_root(args)
        .or_else(|| child_env("CARGO_INSTALL_ROOT").map(PathBuf::from))
        .or_else(|| child_env("CARGO_HOME").map(PathBuf::from))
}

/// Executables in `bin_dir` written at or after `since_ms` (Unix ms).
pub(super) fn fresh_binaries(bin_dir: &Path, since_ms: i64) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(bin_dir) else {
        return Vec::new();
    };
    let since = std::time::UNIX_EPOCH
        + std::time::Duration::from_millis(u64::try_from(since_ms.max(0)).unwrap_or(0));
    let mut fresh: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .metadata()
                .ok()
                .filter(std::fs::Metadata::is_file)
                .and_then(|meta| meta.modified().ok())
                .is_some_and(|modified| modified >= since)
        })
        .map(|entry| entry.path())
        .collect();
    fresh.sort();
    fresh
}

/// Is `dir` one of the entries of the `PATH` value `path_var`?
pub(super) fn dir_on_path(dir: &Path, path_var: Option<&std::ffi::OsStr>) -> bool {
    let Some(path_var) = path_var else {
        return false;
    };
    let canonical = std::fs::canonicalize(dir).ok();
    std::env::split_paths(path_var).any(|entry| {
        entry == dir || (canonical.is_some() && std::fs::canonicalize(&entry).ok() == canonical)
    })
}

/// The one-line notice for an installed binary.
pub(super) fn notice_line(binary: &Path, on_path: bool) -> String {
    let name = binary
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if on_path {
        format!("soldr: installed {name} -> {}", binary.display())
    } else {
        format!(
            "soldr: installed {name} -> {} (not on PATH; add {} to PATH or pass --root)",
            binary.display(),
            binary.parent().unwrap_or(binary).display()
        )
    }
}

/// After a successful `cargo install`, name each binary it wrote (stderr), or
/// record them for the porcelain JSON.
pub(super) fn report(
    args: &[String],
    command: &std::process::Command,
    started_at_ms: i64,
    porcelain: bool,
) {
    if super::subcommand::first_cargo_subcommand(args) != Some("install") {
        return;
    }
    let child_env = |key: &str| child_env_var(command, key);
    let Some(root) = install_root(args, &child_env) else {
        return;
    };
    let bin_dir = root.join("bin");
    // Filesystem mtime granularity can trail the wall clock; allow 2 s slack.
    let binaries = fresh_binaries(&bin_dir, started_at_ms - 2_000);
    if binaries.is_empty() {
        return;
    }
    if porcelain {
        INSTALLED
            .lock()
            .extend(binaries.iter().map(|path| path.display().to_string()));
        return;
    }
    // PATH as the user's shell sees it, not the child's (which carries
    // soldr's shim directories).
    let path_var = std::env::var_os("PATH");
    let on_path = dir_on_path(&bin_dir, path_var.as_deref());
    for binary in &binaries {
        eprintln!("{}", notice_line(binary, on_path));
    }
}

fn child_env_os(command: &std::process::Command, key: &str) -> Option<std::ffi::OsString> {
    for (name, value) in command.get_envs() {
        if name == key {
            return value.map(std::ffi::OsStr::to_os_string);
        }
    }
    std::env::var_os(key)
}

fn child_env_var(command: &std::process::Command, key: &str) -> Option<String> {
    child_env_os(command, key).and_then(|value| value.into_string().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn install_root_prefers_root_flag_then_env() {
        let env = |key: &str| match key {
            "CARGO_INSTALL_ROOT" => Some("/install-root".to_string()),
            "CARGO_HOME" => Some("/cargo-home".to_string()),
            _ => None,
        };
        let none = |_: &str| None;
        let home_only = |key: &str| (key == "CARGO_HOME").then(|| "/cargo-home".to_string());
        assert_eq!(
            install_root(&args(&["install", "--root", "/r", "x"]), &env),
            Some(PathBuf::from("/r"))
        );
        assert_eq!(
            install_root(&args(&["install", "--root=/r2", "x"]), &env),
            Some(PathBuf::from("/r2"))
        );
        assert_eq!(
            install_root(&args(&["install", "x"]), &env),
            Some(PathBuf::from("/install-root"))
        );
        assert_eq!(
            install_root(&args(&["install", "x"]), &home_only),
            Some(PathBuf::from("/cargo-home"))
        );
        assert_eq!(install_root(&args(&["install", "x"]), &none), None);
    }

    #[test]
    fn fresh_binaries_lists_only_files_written_since_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("ghd"), b"bin").expect("write");
        std::fs::create_dir(dir.path().join("subdir")).expect("mkdir");
        let found = fresh_binaries(dir.path(), 0);
        assert_eq!(found, vec![dir.path().join("ghd")]);
        assert!(fresh_binaries(dir.path(), i64::MAX / 2).is_empty());
        assert!(fresh_binaries(&dir.path().join("missing"), 0).is_empty());
    }

    #[test]
    fn notice_names_binary_and_path_remedy_only_when_off_path() {
        let bin = Path::new("/home/u/.soldr/cargo/bin/ghd");
        let off = notice_line(bin, false);
        assert!(off.starts_with("soldr: installed ghd -> "), "{off}");
        assert!(off.contains("not on PATH"), "{off}");
        assert!(off.contains("--root"), "{off}");
        assert_eq!(off.lines().count(), 1);
        let on = notice_line(bin, true);
        assert!(!on.contains("not on PATH"), "{on}");
    }

    #[test]
    fn dir_on_path_matches_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let joined = std::env::join_paths([Path::new("/nope"), dir.path()]).expect("join");
        assert!(dir_on_path(dir.path(), Some(&joined)));
        let other = std::env::join_paths([Path::new("/nope")]).expect("join");
        assert!(!dir_on_path(dir.path(), Some(&other)));
        assert!(!dir_on_path(dir.path(), None));
    }
}
