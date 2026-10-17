//! Manual uv-provisioned PyPI tool environments — soldr#1264 follow-on.
//!
//! Some managed tools have a second acquisition path via PyPI: the
//! `maturin` and `ninja` wheels ship the actual native executables.
//! When the primary source misses (GitHub Releases rate limit /
//! missing asset for maturin, a not-yet-ingested or unsupported-host
//! catalogue bundle for ninja), soldr provisions the tool into a
//! manually-managed isolated environment using the managed `uv` from
//! the soldr-toolchain archive:
//!
//! ```text
//! uv venv --python 3.12 ~/.soldr/bin/<package>-uv-<ver>/
//! uv pip install --python <env python> <package>==<ver>
//! ```
//!
//! Deliberately hand-rolled rather than depending on the `uv-iso-env`
//! PyPI package: soldr's build backend carries zero third-party Python
//! deps, and the two uv invocations above are the entire surface that
//! package would have wrapped.
//!
//! uv state is kept under the soldr root (`UV_CACHE_DIR`,
//! `UV_PYTHON_INSTALL_DIR`) so nothing leaks into the user's uv dirs
//! and `soldr clean`-style wipes take it along.
//!
//! ## Env-var surface
//!
//! `SOLDR_MATURIN_PROVISIONER` — `auto` (default: prebuilt binary,
//! uv-env fallback), `binary` (prebuilt only, fail hard on miss),
//! `uv` (skip the binary fetch, go straight to the uv env).

use std::path::{Path, PathBuf};

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};

/// Selects how `soldr maturin` acquires the maturin executable.
pub const MATURIN_PROVISIONER_ENV_VAR: &str = "SOLDR_MATURIN_PROVISIONER";

/// PyPI distribution that provides Soldr's managed `maturin` executable.
pub const MATURIN_PYPI_PACKAGE: &str = super::MANAGED_MATURIN_PYPI_PACKAGE;

/// Python version the isolated envs are created with. uv downloads
/// its managed CPython on demand when the host has none — pinned so
/// the envs are reproducible across machines.
pub const UV_ENV_PYTHON: &str = "3.12";

/// PyPI `ninja` version used by the ninja fallback. The PyPI package
/// (scikit-build's repackaging of upstream ninja) trails the upstream
/// release line — 1.13.0 is the newest 1.13.x wheel, vs the 1.13.2
/// catalogue bundle. Fine for a fallback: any Ninja >= 1.11 satisfies
/// CMake's generator.
pub const NINJA_PYPI_FALLBACK_VERSION: &str = "1.13.0";

/// Sentinel filename marking a fully-provisioned env. Written last;
/// its absence means a previous attempt died mid-way and the dir is
/// rebuilt from scratch.
const COMPLETE_SENTINEL: &str = ".soldr-complete";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaturinProvisioner {
    /// Prebuilt binary first, uv env as fallback (default).
    Auto,
    /// Prebuilt binary only — a fetch miss is a hard error.
    Binary,
    /// Straight to the uv-provisioned env.
    Uv,
}

impl MaturinProvisioner {
    /// Parse from the env-var value. Unknown / empty values fall back
    /// to `Auto` — diagnostic env vars must never wedge the build.
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(v) if v.eq_ignore_ascii_case("binary") => Self::Binary,
            Some(v) if v.eq_ignore_ascii_case("uv") => Self::Uv,
            _ => Self::Auto,
        }
    }

    pub fn from_env() -> Self {
        let raw = std::env::var(MATURIN_PROVISIONER_ENV_VAR).ok();
        Self::from_env_value(raw.as_deref())
    }
}

/// Directory of the isolated env for `(package, version)`.
pub fn env_dir_for(paths: &SoldrPaths, package: &str, version: &str) -> PathBuf {
    paths.bin.join(format!("{package}-uv-{version}"))
}

/// Path of a tool executable inside an isolated env dir.
pub fn tool_exe_in_env(env_dir: &Path, exe_base: &str) -> PathBuf {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        env_dir.join("Scripts").join(format!("{exe_base}.exe"))
    } else {
        env_dir.join("bin").join(exe_base)
    }
}

/// A provisioned env counts as complete only when BOTH the tool
/// executable and the sentinel exist — the sentinel is written last,
/// so its absence means a prior attempt died mid-way and the dir must
/// be rebuilt rather than served.
pub fn env_is_complete(env_dir: &Path, exe_base: &str) -> bool {
    tool_exe_in_env(env_dir, exe_base).is_file() && env_dir.join(COMPLETE_SENTINEL).is_file()
}

fn python_exe_in_env(env_dir: &Path) -> PathBuf {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        env_dir.join("Scripts").join("python.exe")
    } else {
        env_dir.join("bin").join("python")
    }
}

/// Provision maturin `version` into an isolated uv-managed env and
/// return the maturin executable path.
pub async fn provision_maturin_via_uv(
    paths: &SoldrPaths,
    version: &str,
) -> Result<PathBuf, SoldrError> {
    provision_pypi_tool_via_uv(paths, MATURIN_PYPI_PACKAGE, version, "maturin").await
}

/// Provision the PyPI `ninja` wheel (pinned
/// [`NINJA_PYPI_FALLBACK_VERSION`]) into an isolated uv-managed env
/// and return the ninja executable path. Fallback for hosts whose
/// catalogue ninja bundle is missing or not yet ingested.
pub async fn provision_ninja_via_uv(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    provision_pypi_tool_via_uv(paths, "ninja", NINJA_PYPI_FALLBACK_VERSION, "ninja").await
}

/// Provision PyPI `package==version` into an isolated uv-managed env
/// and return the path of its `exe_base` executable. Idempotent: a
/// completed env is a couple of stat calls; a half-built one is wiped
/// and rebuilt.
pub async fn provision_pypi_tool_via_uv(
    paths: &SoldrPaths,
    package: &str,
    version: &str,
    exe_base: &str,
) -> Result<PathBuf, SoldrError> {
    let env_dir = env_dir_for(paths, package, version);
    let tool = tool_exe_in_env(&env_dir, exe_base);
    if env_is_complete(&env_dir, exe_base) {
        return Ok(tool);
    }

    // Cross-process guard: parallel soldr invocations (cargo build
    // scripts, fanned-out CI jobs) must not race the wipe + venv +
    // install sequence below. Blocking exclusive lock — the winner
    // provisions, waiters re-check the sentinel after acquiring and
    // short-circuit. Held until this function returns.
    let _install_lock =
        super::syslib_common::acquire_install_lock(&paths.bin, &format!("{package}-uv-{version}"))?;
    if env_is_complete(&env_dir, exe_base) {
        return Ok(tool);
    }

    let host = super::cmake_tools::current_host_triple();
    let uv_root = super::uv_tool::ensure_uv_bundle(paths, host).await?;
    let uv = super::uv_tool::uv_exe(&uv_root);
    if !uv.is_file() {
        return Err(SoldrError::Other(format!(
            "managed uv bundle at {} has no {}",
            uv_root.display(),
            uv.display()
        )));
    }

    // Half-built env from a previous attempt → start clean.
    if env_dir.exists() {
        let _ = std::fs::remove_dir_all(&env_dir);
    }

    eprintln!(
        "soldr: provisioning {package} {version} via managed uv \
         ({UV_ENV_PYTHON} isolated env)..."
    );

    run_uv(
        &uv,
        paths,
        &[
            "venv".as_ref(),
            "--python".as_ref(),
            UV_ENV_PYTHON.as_ref(),
            env_dir.as_os_str(),
        ],
        "uv venv",
    )?;

    let env_python = python_exe_in_env(&env_dir);
    let spec = format!("{package}=={version}");
    run_uv(
        &uv,
        paths,
        &[
            "pip".as_ref(),
            "install".as_ref(),
            "--python".as_ref(),
            env_python.as_os_str(),
            spec.as_str().as_ref(),
        ],
        "uv pip install",
    )?;

    if !tool.is_file() {
        return Err(SoldrError::Other(format!(
            "uv reported success but {} does not exist",
            tool.display()
        )));
    }
    std::fs::write(env_dir.join(COMPLETE_SENTINEL), b"ok\n")?;
    Ok(tool)
}

/// Run one uv command with soldr-scoped uv state dirs. Inherits
/// stdout/stderr so download progress is visible to the user.
fn uv_command(uv: &Path, paths: &SoldrPaths) -> std::process::Command {
    let mut command = std::process::Command::new(uv);
    command.env("UV_CACHE_DIR", paths.root.join("uv-cache"));
    command.env("UV_PYTHON_INSTALL_DIR", paths.root.join("uv-python"));
    // Provisioning may source-build the PyPI package. Never inherit a wrapper
    // tied to the caller's already-finished Cargo session: its broker route is
    // no longer valid by the time uv invokes rustc. An explicit CARGO command
    // still controls the source build and can enter Soldr through a fresh
    // front door of its own.
    command.env_remove("RUSTC_WRAPPER");
    command.env_remove("RUSTC_WORKSPACE_WRAPPER");
    suppress_windows_console_window(&mut command);
    command
}

fn run_uv(
    uv: &Path,
    paths: &SoldrPaths,
    args: &[&std::ffi::OsStr],
    label: &str,
) -> Result<(), SoldrError> {
    let mut command = uv_command(uv, paths);
    command.args(args);
    let status = crate::platform::wait::status_for(&mut command, crate::platform::wait::LONG_CHILD)
        .map_err(|e| {
            SoldrError::Other(format!("{label}: failed to spawn {}: {e}", uv.display()))
        })?;
    if !status.success() {
        return Err(SoldrError::Other(format!(
            "{label} exited with {status} (uv: {})",
            uv.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_parse_defaults_to_auto() {
        assert_eq!(
            MaturinProvisioner::from_env_value(None),
            MaturinProvisioner::Auto
        );
        assert_eq!(
            MaturinProvisioner::from_env_value(Some("")),
            MaturinProvisioner::Auto
        );
        assert_eq!(
            MaturinProvisioner::from_env_value(Some("garbage")),
            MaturinProvisioner::Auto,
            "unknown values must not wedge the build"
        );
        assert_eq!(
            MaturinProvisioner::from_env_value(Some("BINARY")),
            MaturinProvisioner::Binary
        );
        assert_eq!(
            MaturinProvisioner::from_env_value(Some(" uv ")),
            MaturinProvisioner::Uv
        );
    }

    #[test]
    fn env_layout_is_versioned_and_platform_correct() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        assert_eq!(MATURIN_PYPI_PACKAGE, "soldr-maturin");
        let dir = env_dir_for(&paths, MATURIN_PYPI_PACKAGE, "1.14.1.post1");
        assert!(dir.ends_with("soldr-maturin-uv-1.14.1.post1"));
        let ninja_dir = env_dir_for(&paths, "ninja", NINJA_PYPI_FALLBACK_VERSION);
        assert!(ninja_dir.ends_with("ninja-uv-1.13.0"));

        let exe = tool_exe_in_env(&dir, "maturin");
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            assert!(exe.ends_with("Scripts/maturin.exe") || exe.ends_with("Scripts\\maturin.exe"));
        } else {
            assert!(exe.ends_with("bin/maturin"));
        }
    }

    #[test]
    fn completed_env_short_circuits_without_uv() {
        // A tool exe + sentinel must satisfy the provisioner with
        // zero network / uv-bundle work — proven by pointing at a
        // synthetic root where no uv bundle could possibly exist.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let env_dir = env_dir_for(&paths, "maturin", "9.9.9");
        let exe = tool_exe_in_env(&env_dir, "maturin");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"stub").unwrap();
        std::fs::write(env_dir.join(COMPLETE_SENTINEL), b"ok\n").unwrap();

        let got = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(provision_pypi_tool_via_uv(
                &paths, "maturin", "9.9.9", "maturin",
            ))
            .expect("completed env must short-circuit");
        assert_eq!(got, exe);
    }

    #[test]
    fn missing_sentinel_means_incomplete() {
        // Exe without sentinel = half-built env → NOT complete, must
        // be rebuilt rather than served. Sentinel without exe likewise.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let env_dir = tmp.path().join("ninja-uv-9.9.9");
        let exe = tool_exe_in_env(&env_dir, "ninja");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"stub").unwrap();
        assert!(
            !env_is_complete(&env_dir, "ninja"),
            "no sentinel → incomplete"
        );

        std::fs::write(env_dir.join(COMPLETE_SENTINEL), b"ok\n").unwrap();
        assert!(env_is_complete(&env_dir, "ninja"));

        std::fs::remove_file(&exe).unwrap();
        assert!(!env_is_complete(&env_dir, "ninja"), "no exe → incomplete");
    }

    #[test]
    fn ninja_fallback_version_well_formed() {
        let parts: Vec<&str> = NINJA_PYPI_FALLBACK_VERSION.split('.').collect();
        assert!(parts.len() >= 2);
        for p in parts {
            assert!(p.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn uv_source_build_does_not_inherit_a_stale_rustc_wrapper() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let command = uv_command(Path::new("uv"), &paths);

        let env = command
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(ToOwned::to_owned)))
            .collect::<std::collections::HashMap<_, _>>();
        for name in ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"] {
            assert_eq!(env.get(std::ffi::OsStr::new(name)), Some(&None));
        }
        assert!(
            !env.contains_key(std::ffi::OsStr::new("SOLDR_RUSTC_WRAPPER")),
            "the persistent caller override must remain inherited by a fresh Soldr front door"
        );
    }
}
