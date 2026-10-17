//! `soldr wheel` honours `[tool.soldr.pep517] bundle-bins` (zackees/soldr#3468).
//!
//! The PEP 517 backend (`src/soldr/__init__.py`) has staged auxiliary Cargo
//! `[[bin]]` targets into its wheels since soldr#3239: after maturin writes the
//! extension wheel, each entry is built through `soldr build` and added under
//! `<dist>.data/scripts/` (or its `dest`) with a regenerated `RECORD`.
//! `soldr wheel` used to skip that step, so a release wheel had the glibc-2.17
//! floor from soldr#3432 but not the bundled CLI, and a consumer could not get
//! both.
//!
//! # One implementation, not two
//!
//! The staging logic lives in `src/soldr/_bundle_bins.py` and nowhere else.
//! This module embeds that exact file and, after a successful maturin build,
//! runs its `stage` command with a Python interpreter. Re-implementing the
//! `bundle-bins` parser, the Cargo JSON scan and the `RECORD` rewrite in Rust
//! would give the two entry points two readings of one configuration table,
//! which is the drift CLAUDE.md warns about ("a green CI lane only proves the
//! verb CI runs").
//!
//! A Python interpreter is required only when `bundle-bins` is declared;
//! projects without it never start one. It is found as `$PYO3_PYTHON`, then
//! `python3`, then `python`.
//!
//! # Matching the extension's toolchain
//!
//! [`BundleBuild`] is decided by the pure wheel planner: when the maturin path
//! prepared the target (a cross build, or a host-target release `*-linux-gnu`
//! wheel built against the catalogue glibc 2.17 sysroot), each bin is built
//! with `soldr build --target <triple>`, which prepares the same catalogue
//! target, so the bundled executable has the same glibc floor as the
//! extension the wheel tag describes. Otherwise both are native host builds.

use std::path::{Path, PathBuf};

use crate::core::SoldrError;
use crate::platform::sync::Mutex;

/// The staging helper, byte-for-byte the PEP 517 backend's own module.
const BUNDLE_BINS_PY: &str = include_str!("../../../src/soldr/_bundle_bins.py");
const PROCESS_CAPTURE_PY: &str = include_str!("../../../src/soldr/_process.py");

/// How the bins are built so they match the extension.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BundleBuild {
    /// `--target` for `soldr build`, or `None` for a native host build.
    pub target: Option<String>,
    /// Cargo profile flags (`--release`, `--profile <p>`, or none for dev).
    pub profile_args: Vec<String>,
}

/// What `pyproject.toml` says about bundling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleConfig {
    /// `[tool.maturin] manifest-path`, as written.
    pub manifest_path: Option<String>,
}

/// A registered request, carried from the wheel planner to the maturin
/// execution path (which ends the process).
#[derive(Debug, Clone, PartialEq, Eq)]
struct BundleRequest {
    pyproject: PathBuf,
    manifest_path: Option<PathBuf>,
    build: BundleBuild,
}

static BUNDLE_REQUEST: Mutex<Option<BundleRequest>> = Mutex::new(None);

/// The Cargo profile flags matching maturin's: `--release` for a release
/// wheel, a forwarded `--profile <p>` as-is, otherwise Cargo's dev profile.
pub fn profile_args(is_release: bool, rest: &[String]) -> Vec<String> {
    if is_release {
        return vec!["--release".to_string()];
    }
    let mut iter = rest.iter().take_while(|arg| arg.as_str() != "--");
    while let Some(arg) = iter.next() {
        if arg == "--profile" {
            if let Some(value) = iter.next() {
                return vec!["--profile".to_string(), value.clone()];
            }
        } else if let Some(value) = arg.strip_prefix("--profile=") {
            return vec!["--profile".to_string(), value.to_string()];
        }
    }
    Vec::new()
}

/// Parse `pyproject.toml` text. `Ok(None)` when no `bundle-bins` is declared
/// (or it is an empty array); an error when it is not an array.
pub fn read_bundle_config(pyproject_text: &str) -> Result<Option<BundleConfig>, String> {
    let Ok(document) = pyproject_text.parse::<toml::Table>() else {
        // maturin reports the authoritative TOML error during the build.
        return Ok(None);
    };
    let tool = document.get("tool").and_then(toml::Value::as_table);
    let entries = tool
        .and_then(|tool| tool.get("soldr"))
        .and_then(toml::Value::as_table)
        .and_then(|soldr| soldr.get("pep517"))
        .and_then(toml::Value::as_table)
        .and_then(|pep517| pep517.get("bundle-bins"));
    let Some(entries) = entries else {
        return Ok(None);
    };
    let Some(entries) = entries.as_array() else {
        return Err("[tool.soldr.pep517] bundle-bins must be an array of tables".to_string());
    };
    if entries.is_empty() {
        return Ok(None);
    }
    let manifest_path = tool
        .and_then(|tool| tool.get("maturin"))
        .and_then(toml::Value::as_table)
        .and_then(|maturin| maturin.get("manifest-path"))
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    Ok(Some(BundleConfig { manifest_path }))
}

/// Read `<workspace_root>/pyproject.toml` and, when it declares
/// `bundle-bins`, register a staging request for the maturin path.
pub(crate) fn request_for_workspace(
    workspace_root: &Path,
    build: &BundleBuild,
) -> Result<(), SoldrError> {
    let pyproject = workspace_root.join("pyproject.toml");
    let Ok(text) = std::fs::read_to_string(&pyproject) else {
        return Ok(());
    };
    let config = read_bundle_config(&text)
        .map_err(|error| SoldrError::Other(format!("soldr wheel: {error}")))?;
    let Some(config) = config else {
        return Ok(());
    };
    let request = BundleRequest {
        manifest_path: config
            .manifest_path
            .map(|manifest| workspace_root.join(manifest)),
        pyproject,
        build: build.clone(),
    };
    *BUNDLE_REQUEST.lock() = Some(request);
    Ok(())
}

/// The `stage` command line for `_bundle_bins.py`, minus the interpreter.
fn stage_args(script: &Path, wheel: &Path, soldr: &Path, request: &BundleRequest) -> Vec<String> {
    let mut args = vec![
        script.display().to_string(),
        "stage".to_string(),
        "--wheel".to_string(),
        wheel.display().to_string(),
        "--pyproject".to_string(),
        request.pyproject.display().to_string(),
        "--soldr".to_string(),
        soldr.display().to_string(),
    ];
    if let Some(manifest) = &request.manifest_path {
        args.push("--manifest-path".to_string());
        args.push(manifest.display().to_string());
    }
    if let Some(target) = &request.build.target {
        args.push("--target".to_string());
        args.push(target.clone());
    }
    for profile in &request.build.profile_args {
        // `=` form: argparse would read a bare `--release` as a new option.
        args.push(format!("--profile-arg={profile}"));
    }
    args
}

fn python_candidates() -> Vec<std::ffi::OsString> {
    let mut candidates = Vec::new();
    if let Some(explicit) = std::env::var_os("PYO3_PYTHON").filter(|value| !value.is_empty()) {
        candidates.push(explicit);
    }
    candidates.push("python3".into());
    candidates.push("python".into());
    candidates
}

/// Run the registered `bundle-bins` staging against `wheel`, if `soldr wheel`
/// registered one. A no-op for every other maturin invocation.
pub(crate) fn stage_requested(wheel: &Path) -> Result<(), SoldrError> {
    let Some(request) = BUNDLE_REQUEST.lock().clone() else {
        return Ok(());
    };
    let failure = |detail: String| {
        SoldrError::Other(format!(
            "soldr wheel: could not stage [tool.soldr.pep517] bundle-bins into {}: {detail}",
            wheel.display()
        ))
    };
    let soldr = std::env::current_exe().map_err(|error| failure(error.to_string()))?;
    let scripts = tempfile::Builder::new()
        .prefix("soldr-bundle-bins-")
        .tempdir()
        .map_err(|error| failure(error.to_string()))?;
    let script = scripts.path().join("_bundle_bins.py");
    std::fs::write(&script, BUNDLE_BINS_PY).map_err(|error| failure(error.to_string()))?;
    std::fs::write(scripts.path().join("_process.py"), PROCESS_CAPTURE_PY)
        .map_err(|error| failure(error.to_string()))?;
    let args = stage_args(&script, wheel, &soldr, &request);
    for python in python_candidates() {
        // The interpreter's `soldr build` children are soldr's own sequential
        // build step, not a recursive chain: mark them with the one-hop
        // self-spawn edge the reentrancy guard sanctions and each child
        // consumes on entry (soldr#2739), as gc and output capture do.
        let status = std::process::Command::new(&python)
            .args(&args)
            .env(soldr_core::self_relocate::SELF_SPAWN_EDGE_ENV_VAR, "1")
            .status();
        match status {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => {
                return Err(failure(format!(
                    "`{} {}` exited with {status}",
                    python.to_string_lossy(),
                    args.join(" ")
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(failure(error.to_string())),
        }
    }
    Err(failure(
        "bundle-bins staging runs soldr's `_bundle_bins.py` and needs a Python \
         interpreter; none was found as $PYO3_PYTHON, `python3` or `python`. \
         Install Python 3.10+ or set PYO3_PYTHON."
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn no_bundle_bins_means_no_request() {
        assert_eq!(read_bundle_config("[project]\nname = \"x\"\n"), Ok(None));
        assert_eq!(
            read_bundle_config("[tool.soldr.pep517]\nbundle-bins = []\n"),
            Ok(None)
        );
        assert_eq!(read_bundle_config("not = [valid"), Ok(None));
    }

    #[test]
    fn bundle_bins_is_detected_with_the_maturin_manifest_path() {
        let text = "[tool.soldr.pep517]\n\
                    bundle-bins = [{ bin = \"template-cli\", package = \"template-cli\" }]\n\
                    [tool.maturin]\n\
                    manifest-path = \"crates/py/Cargo.toml\"\n";
        assert_eq!(
            read_bundle_config(text),
            Ok(Some(BundleConfig {
                manifest_path: Some("crates/py/Cargo.toml".to_string())
            }))
        );
    }

    #[test]
    fn a_non_array_bundle_bins_is_refused() {
        assert!(read_bundle_config("[tool.soldr.pep517]\nbundle-bins = \"x\"\n").is_err());
    }

    #[test]
    fn profile_follows_maturin() {
        assert_eq!(profile_args(true, &[]), strings(&["--release"]));
        assert!(profile_args(false, &[]).is_empty());
        assert_eq!(
            profile_args(false, &strings(&["--profile", "dist"])),
            strings(&["--profile", "dist"])
        );
        assert_eq!(
            profile_args(false, &strings(&["--profile=dist"])),
            strings(&["--profile", "dist"])
        );
        assert!(profile_args(false, &strings(&["--", "--profile", "x"])).is_empty());
    }

    #[test]
    fn stage_args_carry_target_profile_and_manifest() {
        let request = BundleRequest {
            pyproject: PathBuf::from("/w/pyproject.toml"),
            manifest_path: Some(PathBuf::from("/w/crates/py/Cargo.toml")),
            build: BundleBuild {
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                profile_args: strings(&["--release"]),
            },
        };
        assert_eq!(
            stage_args(
                Path::new("/tmp/s.py"),
                Path::new("/w/dist/a.whl"),
                Path::new("/bin/soldr"),
                &request
            ),
            strings(&[
                "/tmp/s.py",
                "stage",
                "--wheel",
                "/w/dist/a.whl",
                "--pyproject",
                "/w/pyproject.toml",
                "--soldr",
                "/bin/soldr",
                "--manifest-path",
                "/w/crates/py/Cargo.toml",
                "--target",
                "x86_64-unknown-linux-gnu",
                "--profile-arg=--release",
            ])
        );
    }

    #[test]
    fn the_embedded_helper_is_the_backends_module() {
        assert!(BUNDLE_BINS_PY.contains("def main("));
        assert!(BUNDLE_BINS_PY.contains("def bundle_into_wheel("));
    }
}
