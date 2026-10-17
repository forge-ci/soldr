//! Runtime toolchain discovery: explicit target overrides, implicit
//! `CARGO_HOME` / `RUSTUP_HOME` detection, direct binary probing, and
//! the `rustc --print target-triple` shell-out that powers
//! `TargetTriple::detect_from_dir`.
//!
//! Public entry points are `apply_implicit_toolchain_homes`,
//! `suppress_windows_console_window`, and `probe_toolchain_binary`.
//! Everything else is `pub(super)` so `target_triple.rs` can call
//! `read_explicit_target_override` / `detect_runtime_rustc_triple` without
//! re-exposing the helpers crate-wide.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use super::target_triple::{compile_time_arch, compile_time_host_os, TargetTriple};
use super::toolchain_manifest::read_rust_toolchain_manifest;
use super::{
    non_empty_env_path, CARGO_HOME_ENV_VAR, RUSTUP_HOME_ENV_VAR, RUSTUP_TOOLCHAIN_ENV_VAR,
};

#[derive(Debug, Deserialize)]
struct CargoConfigFile {
    build: Option<CargoBuildSection>,
}

#[derive(Debug, Deserialize)]
struct CargoBuildSection {
    target: Option<CargoBuildTarget>,
}

/// Cargo accepts `build.target` as either a string or an array of strings
/// (soldr#3642).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CargoBuildTarget {
    One(String),
    Many(Vec<String>),
}

pub(super) fn read_explicit_target_override(start_dir: Option<&Path>) -> Option<String> {
    find_in_ancestors(start_dir, ".cargo/config.toml")
        .and_then(read_cargo_config_target)
        .or_else(|| {
            find_in_ancestors(start_dir, ".cargo/config").and_then(read_cargo_config_target)
        })
        .or_else(|| {
            find_in_ancestors(start_dir, "rust-toolchain.toml").and_then(read_toolchain_target)
        })
}

fn read_cargo_config_target(path: PathBuf) -> Option<String> {
    let text = std::fs::read_to_string(&path).ok()?;
    let config: CargoConfigFile = match toml::from_str(&text) {
        Ok(config) => config,
        Err(err) => {
            eprintln!(
                "soldr: warning: ignoring unparseable Cargo config {}: {err}",
                path.display()
            );
            return None;
        }
    };
    match config.build?.target? {
        CargoBuildTarget::One(target) => Some(target),
        CargoBuildTarget::Many(list) => choose_target_override(
            list.into_iter()
                .filter(|target| TargetTriple::from_triple(target).is_ok())
                .collect(),
        ),
    }
}

fn read_toolchain_target(path: PathBuf) -> Option<String> {
    let workspace_root = path.parent()?;
    let manifest = read_rust_toolchain_manifest(workspace_root).ok()?;
    let supported = manifest
        .targets?
        .into_iter()
        .filter(|target| TargetTriple::from_triple(target).is_ok())
        .collect::<Vec<_>>();

    choose_target_override(supported)
}

fn choose_target_override(targets: Vec<String>) -> Option<String> {
    if targets.len() == 1 {
        return targets.into_iter().next();
    }

    let host_os = compile_time_host_os().ok()?;
    let host_arch = compile_time_arch().ok()?;
    let matching_host = targets
        .into_iter()
        .filter_map(|target| {
            let parsed = TargetTriple::from_triple(&target).ok()?;
            if parsed.os == host_os && parsed.arch == host_arch {
                Some(target)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    if matching_host.len() == 1 {
        matching_host.into_iter().next()
    } else {
        None
    }
}

/// Locate the nearest `rust-toolchain.toml` at or above `start_dir`.
///
/// soldr#1766: the front door needs to distinguish "this repo pins a
/// toolchain" from "there is no pin anywhere", and it must do so by walking
/// ancestors. [`read_rust_toolchain_manifest`](super::read_rust_toolchain_manifest)
/// reads `start_dir` only, so asking it would report "unpinned" for every
/// build launched from a subdirectory of a pinned repo -- including this
/// workspace's own integration tests, which run with the package directory as
/// their cwd while the pin sits at the workspace root.
pub fn find_rust_toolchain_manifest(start_dir: &Path) -> Option<PathBuf> {
    find_in_ancestors(Some(start_dir), "rust-toolchain.toml")
}

fn find_in_ancestors(start_dir: Option<&Path>, relative_path: &str) -> Option<PathBuf> {
    let mut current = start_dir?.to_path_buf();
    loop {
        let candidate = current.join(relative_path);
        if candidate.exists() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// The nearest ancestor `.cargo/` that is a real repo-local `CARGO_HOME`:
/// one holding a `bin/` directory, which is what a repo-local home is for.
///
/// A `.cargo/` without `bin/` is Cargo's per-project *config* directory
/// (`.cargo/config.toml`), never a home. Exporting it as `CARGO_HOME` made
/// Cargo download a full crates.io registry cache into the repository
/// (`<repo>/.cargo/registry`). Such a directory is skipped and the walk
/// continues upward, so a home above it is still found.
fn find_cargo_home_in_ancestors(start_dir: Option<&Path>) -> Option<PathBuf> {
    let mut current = start_dir?.to_path_buf();
    loop {
        let candidate = current.join(".cargo");
        if candidate.join("bin").is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ImplicitToolchainHomes {
    cargo_home: Option<PathBuf>,
    rustup_home: Option<PathBuf>,
}

impl ImplicitToolchainHomes {
    fn from_env(
        start_dir: Option<&Path>,
        cargo_home_env: Option<&OsStr>,
        rustup_home_env: Option<&OsStr>,
    ) -> Self {
        Self {
            cargo_home: if cargo_home_env.is_none() {
                find_cargo_home_in_ancestors(start_dir)
            } else {
                None
            },
            rustup_home: if rustup_home_env.is_none() {
                find_dir_in_ancestors(start_dir, ".rustup")
            } else {
                None
            },
        }
    }

    fn detect(start_dir: Option<&Path>) -> Self {
        Self::from_env(
            start_dir,
            std::env::var_os(CARGO_HOME_ENV_VAR).as_deref(),
            std::env::var_os(RUSTUP_HOME_ENV_VAR).as_deref(),
        )
    }

    fn apply_to_command(&self, command: &mut Command) {
        if let Some(cargo_home) = &self.cargo_home {
            command.env(CARGO_HOME_ENV_VAR, cargo_home);
        }
        if let Some(rustup_home) = &self.rustup_home {
            command.env(RUSTUP_HOME_ENV_VAR, rustup_home);
        }
    }
}

fn cargo_home_bin_dir(start_dir: Option<&Path>) -> Option<PathBuf> {
    non_empty_env_path(std::env::var_os(CARGO_HOME_ENV_VAR).as_deref())
        .map(|path| path.join("bin"))
        .or_else(|| {
            ImplicitToolchainHomes::detect(start_dir)
                .cargo_home
                .map(|path| path.join("bin"))
        })
}

fn rustup_home_dir(start_dir: Option<&Path>) -> Option<PathBuf> {
    non_empty_env_path(std::env::var_os(RUSTUP_HOME_ENV_VAR).as_deref())
        .or_else(|| ImplicitToolchainHomes::detect(start_dir).rustup_home)
}

fn rustup_toolchain_bin_dir(start_dir: Option<&Path>) -> Option<PathBuf> {
    let toolchains_dir = rustup_home_dir(start_dir)?.join("toolchains");
    let mut candidates = std::fs::read_dir(toolchains_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .map(|path| path.join("bin"))
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();

    if candidates.len() == 1 {
        candidates.pop()
    } else {
        None
    }
}

fn path_bin_dir(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|value| {
        std::env::split_paths(&value).find(|dir| find_executable_in_dir(dir, tool).is_some())
    })
}

fn rustup_toolchain_env_is_explicit(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

fn executable_exists(path: &Path) -> bool {
    path.is_file()
}

/// PATHEXT-driven executable suffixes. Only meaningful on Windows, where
/// the bare `tool` name is resolved by appending each extension in order.
fn windows_pathexts() -> Vec<String> {
    let pathext = std::env::var_os("PATHEXT")
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
    pathext
        .split(';')
        .map(str::trim)
        .filter(|ext| !ext.is_empty())
        .map(|ext| ext.to_ascii_lowercase())
        .collect()
}

fn find_executable_in_dir(dir: &Path, tool: &str) -> Option<PathBuf> {
    let candidate = dir.join(tool);
    if executable_exists(&candidate) {
        return Some(candidate);
    }

    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        let ext = candidate
            .extension()
            .and_then(OsStr::to_str)
            .map(|ext| format!(".{}", ext.to_ascii_lowercase()));
        if ext.is_some() {
            return None;
        }

        for suffix in windows_pathexts() {
            let suffixed = dir.join(format!("{tool}{suffix}"));
            if executable_exists(&suffixed) {
                return Some(suffixed);
            }
        }
    }

    None
}

fn find_dir_in_ancestors(start_dir: Option<&Path>, relative_path: &str) -> Option<PathBuf> {
    let mut current = start_dir?.to_path_buf();
    loop {
        let candidate = current.join(relative_path);
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub fn apply_implicit_toolchain_homes(command: &mut Command, start_dir: Option<&Path>) {
    ImplicitToolchainHomes::detect(start_dir).apply_to_command(command);
}

pub fn suppress_windows_console_window(command: &mut Command) {
    crate::platform::process::command::suppress_console(command);
}

pub fn probe_toolchain_binary(tool: &str, start_dir: Option<&Path>) -> Option<PathBuf> {
    if rustup_toolchain_env_is_explicit(std::env::var_os(RUSTUP_TOOLCHAIN_ENV_VAR).as_deref()) {
        return None;
    }

    rustup_toolchain_bin_dir(start_dir)
        .and_then(|dir| find_executable_in_dir(&dir, tool))
        .or_else(|| {
            cargo_home_bin_dir(start_dir).and_then(|dir| find_executable_in_dir(&dir, tool))
        })
        .or_else(|| path_bin_dir(tool).and_then(|dir| find_executable_in_dir(&dir, tool)))
}

pub(super) fn detect_runtime_rustc_triple(start_dir: Option<&Path>) -> Option<String> {
    let rustc = resolve_runtime_rustc(start_dir)?;
    let mut command = std::process::Command::new(rustc);
    apply_implicit_toolchain_homes(&mut command, start_dir);
    suppress_windows_console_window(&mut command);
    if let Some(start_dir) = start_dir {
        command.current_dir(start_dir);
    }
    let output = crate::core::command_output_with_timeout(
        command.args(["--print", "target-triple"]),
        "rustc --print target-triple",
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }

    let triple = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if triple.is_empty() {
        None
    } else {
        Some(triple)
    }
}

fn resolve_runtime_rustc(start_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(rustc) = probe_toolchain_binary("rustc", start_dir) {
        return Some(rustc);
    }

    let mut rustup = std::process::Command::new("rustup");
    apply_implicit_toolchain_homes(&mut rustup, start_dir);
    suppress_windows_console_window(&mut rustup);
    if let Some(start_dir) = start_dir {
        rustup.current_dir(start_dir);
    }
    let rustup_output = crate::core::command_output_with_timeout(
        rustup.args(["which", "rustc"]),
        "rustup which rustc",
    )
    .ok()?;
    if rustup_output.status.success() {
        let path = String::from_utf8_lossy(&rustup_output.stdout)
            .trim()
            .to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    Some(PathBuf::from("rustc"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, fs};
    use tempfile::tempdir;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn is_windows_test_host() -> bool {
        crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
    }

    fn fake_script_path(dir: &Path, name: &str) -> PathBuf {
        if is_windows_test_host() {
            dir.join(format!("{name}.bat"))
        } else {
            dir.join(name)
        }
    }

    fn write_fake_script(path: &Path, script: &str) {
        fs::write(path, script).unwrap();
        if !is_windows_test_host() {
            // `PermissionsExt::set_mode` is unix-only, so go through the
            // shell instead of a cfg'd import.
            let status = std::process::Command::new("chmod")
                .args(["755"])
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success(), "chmod 755 failed for {}", path.display());
        }
    }

    fn fake_rustc_script(triple: &str) -> String {
        if is_windows_test_host() {
            format!(
                "@echo off\r\n\
                 if \"%~1\"==\"--print\" if \"%~2\"==\"target-triple\" (\r\n\
                 echo {triple}\r\n\
                 exit /b 0\r\n\
                 )\r\n\
                 echo unexpected rustc args %* 1>&2\r\n\
                 exit /b 1\r\n"
            )
        } else {
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--print\" ] && [ \"$2\" = \"target-triple\" ]; then\n\
                     printf '%s\\n' '{triple}'\n\
                     exit 0\n\
                 fi\n\
                 echo \"unexpected rustc args: $*\" >&2\n\
                 exit 1\n"
            )
        }
    }

    fn fake_failing_rustup_script(log_path: &Path) -> String {
        if is_windows_test_host() {
            format!(
                "@echo off\r\n\
                 echo rustup %*>>\"{}\"\r\n\
                 echo rustup should not have been invoked 1>&2\r\n\
                 exit /b 1\r\n",
                log_path.display()
            )
        } else {
            format!(
                "#!/bin/sh\n\
                 echo \"rustup $*\" >> \"{}\"\n\
                 echo \"rustup should not have been invoked\" >&2\n\
                 exit 1\n",
                log_path.display()
            )
        }
    }

    fn assert_rustup_not_invoked(log_path: &Path) {
        let log = fs::read_to_string(log_path).unwrap_or_default();
        assert!(
            log.trim().is_empty(),
            "direct tool resolution should bypass rustup entirely: {log}"
        );
    }

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn detects_target_override_from_cargo_config() {
        let dir = tempdir().unwrap();
        let cargo_dir = dir.path().join(".cargo");
        std::fs::create_dir_all(&cargo_dir).unwrap();
        std::fs::write(
            cargo_dir.join("config.toml"),
            "[build]\ntarget = \"x86_64-unknown-linux-musl\"\n",
        )
        .unwrap();

        let target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        assert_eq!(target.triple(), "x86_64-unknown-linux-musl");
    }

    fn write_cargo_config(dir: &Path, text: &str) {
        let cargo_dir = dir.join(".cargo");
        std::fs::create_dir_all(&cargo_dir).unwrap();
        std::fs::write(cargo_dir.join("config.toml"), text).unwrap();
    }

    #[test]
    fn detects_single_element_array_target_from_cargo_config() {
        let dir = tempdir().unwrap();
        write_cargo_config(
            dir.path(),
            "[build]\ntarget = [\"x86_64-unknown-linux-musl\"]\n",
        );

        assert_eq!(
            read_explicit_target_override(Some(dir.path())),
            Some("x86_64-unknown-linux-musl".to_string())
        );
        let target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        assert_eq!(target.triple(), "x86_64-unknown-linux-musl");
    }

    #[test]
    fn ambiguous_array_target_in_cargo_config_yields_no_override() {
        let dir = tempdir().unwrap();
        write_cargo_config(dir.path(), "[build]\ntarget = []\n");

        assert_eq!(read_explicit_target_override(Some(dir.path())), None);
    }

    #[test]
    fn malformed_cargo_config_falls_through_to_toolchain_target() {
        let dir = tempdir().unwrap();
        write_cargo_config(dir.path(), "[build\ntarget=");
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\ntargets = [\"x86_64-unknown-linux-musl\"]\n",
        )
        .unwrap();

        assert_eq!(
            read_explicit_target_override(Some(dir.path())),
            Some("x86_64-unknown-linux-musl".to_string())
        );
    }

    #[test]
    fn detects_gnu_override_from_rust_toolchain_toml() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\ntargets = [\"x86_64-pc-windows-gnu\"]\n",
        )
        .unwrap();

        let target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        assert_eq!(target.triple(), "x86_64-pc-windows-gnu");
    }

    #[test]
    fn detects_msvc_override_from_rust_toolchain_toml() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\ntargets = [\"x86_64-pc-windows-msvc\"]\n",
        )
        .unwrap();

        let target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        assert_eq!(target.triple(), "x86_64-pc-windows-msvc");
    }

    #[test]
    fn detects_macos_override_from_rust_toolchain_toml() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\ntargets = [\"aarch64-apple-darwin\"]\n",
        )
        .unwrap();

        let target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        assert_eq!(target.triple(), "aarch64-apple-darwin");
    }

    #[test]
    fn detects_override_from_parent_directory() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\ntargets = [\"aarch64-apple-darwin\"]\n",
        )
        .unwrap();
        let nested = dir.path().join("nested").join("child");
        std::fs::create_dir_all(&nested).unwrap();

        let target = TargetTriple::detect_in_dir(&nested).unwrap();
        assert_eq!(target.triple(), "aarch64-apple-darwin");
    }

    #[test]
    fn ignores_ambiguous_toolchain_target_list() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\ntargets = [\"x86_64-pc-windows-msvc\", \"aarch64-pc-windows-msvc\"]\n",
        )
        .unwrap();

        let _target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        // Ambiguous list → fall back to the host arch on Windows.
        if is_windows_test_host() {
            let expected_arch = if std::env::consts::ARCH == "x86_64" {
                "x86_64"
            } else {
                "aarch64"
            };
            assert_eq!(_target.triple(), format!("{expected_arch}-pc-windows-msvc"));
        }
    }

    #[test]
    fn implicit_toolchain_homes_detect_repo_local_directories_from_ancestors() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".cargo").join("bin")).unwrap();
        std::fs::create_dir_all(dir.path().join(".rustup")).unwrap();
        let nested = dir.path().join("workspace").join("crate");
        std::fs::create_dir_all(&nested).unwrap();

        let homes = ImplicitToolchainHomes::from_env(Some(nested.as_path()), None, None);
        assert_eq!(homes.cargo_home, Some(dir.path().join(".cargo")));
        assert_eq!(homes.rustup_home, Some(dir.path().join(".rustup")));
    }

    /// A project's `.cargo/` holding only `config.toml` is Cargo's
    /// per-project *config* directory, never a `CARGO_HOME`. Treating it as
    /// one made Cargo download a full crates.io registry cache into the
    /// repository (`<repo>/.cargo/registry`, hundreds of MB).
    #[test]
    fn a_project_config_dir_is_not_a_cargo_home() {
        let dir = tempdir().unwrap();
        let project = dir.path().join("project");
        let config_dir = project.join(".cargo");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            "[target.'cfg(unix)']\nrunner = \"ci/test_guard.sh\"\n",
        )
        .unwrap();

        let homes = ImplicitToolchainHomes::from_env(Some(project.as_path()), None, None);
        assert_eq!(homes.cargo_home, None);

        // A config dir an earlier soldr already polluted with a registry
        // cache is still not a home: it has no `bin/`.
        std::fs::create_dir_all(config_dir.join("registry").join("index")).unwrap();
        let homes = ImplicitToolchainHomes::from_env(Some(project.as_path()), None, None);
        assert_eq!(homes.cargo_home, None);

        // The walk goes past the config dir to a real repo-local home.
        let real_home = dir.path().join(".cargo");
        std::fs::create_dir_all(real_home.join("bin")).unwrap();
        let homes = ImplicitToolchainHomes::from_env(Some(project.as_path()), None, None);
        assert_eq!(homes.cargo_home, Some(real_home));
    }

    #[test]
    fn implicit_toolchain_homes_only_fill_missing_env_vars() {
        let dir = tempdir().unwrap();
        let repo_cargo_home = dir.path().join(".cargo");
        let repo_rustup_home = dir.path().join(".rustup");
        std::fs::create_dir_all(repo_cargo_home.join("bin")).unwrap();
        std::fs::create_dir_all(&repo_rustup_home).unwrap();

        let homes = ImplicitToolchainHomes::from_env(
            Some(dir.path()),
            Some(OsStr::new("C:/explicit-cargo-home")),
            None,
        );
        assert_eq!(homes.cargo_home, None);
        assert_eq!(homes.rustup_home, Some(repo_rustup_home));
    }

    #[test]
    fn implicit_toolchain_homes_treat_empty_env_as_explicit() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".cargo").join("bin")).unwrap();
        std::fs::create_dir_all(dir.path().join(".rustup")).unwrap();

        let homes = ImplicitToolchainHomes::from_env(
            Some(dir.path()),
            Some(OsStr::new("")),
            Some(OsStr::new("")),
        );
        assert_eq!(homes, ImplicitToolchainHomes::default());
    }

    #[test]
    fn explicit_rustup_toolchain_env_disables_direct_probe() {
        assert!(rustup_toolchain_env_is_explicit(Some(OsStr::new("stable"))));
        assert!(!rustup_toolchain_env_is_explicit(Some(OsStr::new(""))));
        assert!(!rustup_toolchain_env_is_explicit(None));
    }

    #[test]
    fn resolve_runtime_rustc_prefers_path_before_rustup() {
        let _env_lock = lock_env();
        let dir = tempdir().unwrap();
        let tool_dir = dir.path().join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        let log_path = dir.path().join("rustup.log");
        let rustc = fake_script_path(&tool_dir, "rustc");
        let rustup = fake_script_path(&tool_dir, "rustup");
        write_fake_script(&rustc, &fake_rustc_script("x86_64-unknown-linux-gnu"));
        write_fake_script(&rustup, &fake_failing_rustup_script(&log_path));

        let _path = EnvVarGuard::set("PATH", std::env::join_paths([&tool_dir]).unwrap());
        let _cargo_home = EnvVarGuard::remove(CARGO_HOME_ENV_VAR);
        let _rustup_home = EnvVarGuard::remove(RUSTUP_HOME_ENV_VAR);
        let _rustup_toolchain = EnvVarGuard::remove(RUSTUP_TOOLCHAIN_ENV_VAR);

        assert_eq!(resolve_runtime_rustc(None), Some(rustc));
        assert_rustup_not_invoked(&log_path);
    }

    #[test]
    fn suppress_windows_console_window_preserves_piped_output() {
        // Use absolute paths so this test does NOT race with the
        // PATH-mutating tests elsewhere in this module (the
        // `resolve_runtime_rustc_*` family temporarily sets PATH to a
        // synthetic tool dir; if their `EnvVarGuard` hasn't dropped by
        // the time this test runs in parallel, `Command::new("sh")`
        // fails with `NotFound` because the shell isn't on the
        // synthetic PATH). Surfaced as an intermittent Linux CI
        // failure on PR #431.
        let mut command = if is_windows_test_host() {
            let comspec = std::env::var_os("ComSpec")
                .unwrap_or_else(|| std::ffi::OsString::from(r"C:\Windows\System32\cmd.exe"));
            let mut command = Command::new(comspec);
            command.args(["/C", "echo soldr-no-window"]);
            command
        } else {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "printf soldr-no-window"]);
            command
        };

        suppress_windows_console_window(&mut command);
        let output = command.output().expect("failed to run child command");
        assert!(
            output.status.success(),
            "child command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("soldr-no-window"),
            "missing expected stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn resolve_runtime_rustc_prefers_explicit_rustup_home_toolchain_before_rustup() {
        let _env_lock = lock_env();
        let dir = tempdir().unwrap();
        let explicit_rustup_home = dir.path().join("explicit-rustup-home");
        let rustc = fake_script_path(
            &explicit_rustup_home
                .join("toolchains")
                .join("stable-test")
                .join("bin"),
            "rustc",
        );
        fs::create_dir_all(rustc.parent().unwrap()).unwrap();
        write_fake_script(&rustc, &fake_rustc_script("aarch64-apple-darwin"));

        let tool_dir = dir.path().join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        let log_path = dir.path().join("rustup.log");
        let rustup = fake_script_path(&tool_dir, "rustup");
        write_fake_script(&rustup, &fake_failing_rustup_script(&log_path));

        let _path = EnvVarGuard::set("PATH", OsStr::new(""));
        let _cargo_home = EnvVarGuard::remove(CARGO_HOME_ENV_VAR);
        let _rustup_home = EnvVarGuard::set(RUSTUP_HOME_ENV_VAR, &explicit_rustup_home);
        let _rustup_toolchain = EnvVarGuard::remove(RUSTUP_TOOLCHAIN_ENV_VAR);

        assert_eq!(resolve_runtime_rustc(None), Some(rustc));
        assert_rustup_not_invoked(&log_path);
    }

    #[test]
    fn resolve_runtime_rustc_prefers_repo_local_rustup_home_toolchain_before_rustup() {
        let _env_lock = lock_env();
        let dir = tempdir().unwrap();
        let nested = dir.path().join("workspace").join("crate");
        fs::create_dir_all(&nested).unwrap();

        let rustc = fake_script_path(
            &dir.path()
                .join(".rustup")
                .join("toolchains")
                .join("stable-test")
                .join("bin"),
            "rustc",
        );
        fs::create_dir_all(rustc.parent().unwrap()).unwrap();
        write_fake_script(&rustc, &fake_rustc_script("x86_64-pc-windows-msvc"));

        let _path = EnvVarGuard::set("PATH", OsStr::new(""));
        let _cargo_home = EnvVarGuard::remove(CARGO_HOME_ENV_VAR);
        let _rustup_home = EnvVarGuard::remove(RUSTUP_HOME_ENV_VAR);
        let _rustup_toolchain = EnvVarGuard::remove(RUSTUP_TOOLCHAIN_ENV_VAR);

        assert_eq!(resolve_runtime_rustc(Some(&nested)), Some(rustc));
    }

    #[test]
    fn resolve_runtime_rustc_prefers_repo_local_rustup_home_before_explicit_cargo_home_shim() {
        let _env_lock = lock_env();
        let dir = tempdir().unwrap();
        let nested = dir.path().join("workspace").join("crate");
        fs::create_dir_all(&nested).unwrap();

        let repo_local_rustc = fake_script_path(
            &dir.path()
                .join(".rustup")
                .join("toolchains")
                .join("stable-test")
                .join("bin"),
            "rustc",
        );
        fs::create_dir_all(repo_local_rustc.parent().unwrap()).unwrap();
        write_fake_script(
            &repo_local_rustc,
            &fake_rustc_script("x86_64-pc-windows-msvc"),
        );

        let explicit_cargo_home = dir.path().join("explicit-cargo-home");
        let shim_rustc = fake_script_path(&explicit_cargo_home.join("bin"), "rustc");
        fs::create_dir_all(shim_rustc.parent().unwrap()).unwrap();
        write_fake_script(&shim_rustc, &fake_rustc_script("x86_64-unknown-linux-gnu"));

        let tool_dir = dir.path().join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        let log_path = dir.path().join("rustup.log");
        let rustup = fake_script_path(&tool_dir, "rustup");
        write_fake_script(&rustup, &fake_failing_rustup_script(&log_path));

        let _path = EnvVarGuard::set("PATH", std::env::join_paths([&tool_dir]).unwrap());
        let _cargo_home = EnvVarGuard::set(CARGO_HOME_ENV_VAR, &explicit_cargo_home);
        let _rustup_home = EnvVarGuard::remove(RUSTUP_HOME_ENV_VAR);
        let _rustup_toolchain = EnvVarGuard::remove(RUSTUP_TOOLCHAIN_ENV_VAR);

        assert_eq!(resolve_runtime_rustc(Some(&nested)), Some(repo_local_rustc));
        assert_rustup_not_invoked(&log_path);
    }
}
