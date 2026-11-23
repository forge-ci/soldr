//! Path / layout resolution for the trampoline (extracted from
//! trampoline.rs to keep that file under the soldr-wide 1K LOC budget).
//!
//! Used by the trampoline fast-path and by `cargo_front_door`'s
//! sidecar refresh to compute where cargo will write the binary +
//! sidecar + dep-info files given a parsed `cargo run` argv. Pure
//! functions over `ParsedRunArgs`.

use crate::core::SoldrError;
use crate::trampoline::ParsedRunArgs;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) struct Layout {
    pub(crate) binary_path: PathBuf,
    pub(crate) sidecar_path: PathBuf,
    pub(crate) dep_info_path: PathBuf,
}

pub(crate) fn compute_layout(parsed: &ParsedRunArgs, bin: &str) -> Layout {
    let manifest_path = parsed
        .manifest_path
        .clone()
        .unwrap_or_else(|| find_nearest_manifest().unwrap_or_else(|| PathBuf::from("Cargo.toml")));
    let manifest_dir = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let target_dir = resolve_target_dir(parsed, &manifest_dir);

    let mut leaf = target_dir;
    if let Some(triple) = effective_target_triple(parsed) {
        leaf.push(triple);
    }
    let profile_dir = if parsed.release {
        "release".to_string()
    } else if let Some(profile) = parsed.profile.as_deref() {
        // cargo aliases `dev` to `debug` on disk.
        if profile == "dev" {
            "debug".to_string()
        } else {
            profile.to_string()
        }
    } else {
        "debug".to_string()
    };
    leaf.push(&profile_dir);

    let bin_filename = crate::platform::executable::name::native(bin);
    let binary_path = leaf.join(&bin_filename);
    let sidecar_path = leaf.join(".soldr-trampoline").join(format!("{bin}.toml"));
    let dep_info_path = leaf.join(format!("{bin}.d"));

    Layout {
        binary_path,
        sidecar_path,
        dep_info_path,
    }
}

pub(crate) fn resolve_target_dir(parsed: &ParsedRunArgs, manifest_dir: &Path) -> PathBuf {
    if let Some(explicit) = parsed.target_dir.as_ref() {
        return absolutize(explicit.clone(), manifest_dir);
    }
    if let Some(env_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        if !env_dir.is_empty() {
            return absolutize(PathBuf::from(env_dir), manifest_dir);
        }
    }
    manifest_dir.join("target")
}

/// Determine the target subdirectory cargo will produce artifacts under.
///
/// Precedence mirrors `cargo_front_door::default_cargo_build_target`:
/// explicit `--target`, then `CARGO_BUILD_TARGET` env var, and on Windows
/// the auto-detected host triple that soldr injects when neither of those
/// is set. On non-Windows hosts cargo defaults to the unprefixed
/// `target/<profile>/` layout, so we return `None`.
pub(crate) fn effective_target_triple(parsed: &ParsedRunArgs) -> Option<String> {
    if let Some(t) = parsed.target.as_deref() {
        return Some(t.to_string());
    }
    if let Some(v) = std::env::var_os("CARGO_BUILD_TARGET") {
        if let Some(s) = v.to_str() {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        crate::core::TargetTriple::detect().ok().map(|t| t.triple())
    } else {
        None
    }
}

fn absolutize(path: PathBuf, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

pub(crate) fn find_nearest_manifest() -> Option<PathBuf> {
    let mut current = std::env::current_dir().ok()?;
    loop {
        let candidate = current.join("Cargo.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Resolve the `[bin]` name to build. Slice 1 only handles unambiguous
/// cases: explicit `--bin <NAME>`, a manifest default, or one binary.
/// Returns `Ok(None)` when ambiguity forces fall-through.
pub(crate) fn resolve_bin_name(parsed: &ParsedRunArgs) -> Result<Option<String>, SoldrError> {
    if let Some(name) = parsed.bin.as_ref() {
        return Ok(Some(name.clone()));
    }
    let manifest_path = parsed
        .manifest_path
        .clone()
        .or_else(find_nearest_manifest)
        .ok_or_else(|| SoldrError::Other("Cargo.toml not found".into()))?;
    let text = fs::read_to_string(&manifest_path).map_err(|err| {
        SoldrError::Other(format!("read manifest {}: {err}", manifest_path.display()))
    })?;
    let value: toml::Value = text.parse().map_err(|err| {
        SoldrError::Other(format!("parse manifest {}: {err}", manifest_path.display()))
    })?;

    let package = value.get("package");
    if let Some(default) = package.and_then(|package| package.get("default-run")) {
        let name = default
            .as_str()
            .filter(|name| Path::new(name).file_name() == Some(std::ffi::OsStr::new(name)));
        return Ok(name.map(str::to_owned));
    }

    let bins = value.get("bin").and_then(|v| v.as_array());
    let package_name = package
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string);

    let manifest_dir = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    // Case 1: explicit `[[bin]]` table(s).
    if let Some(bins) = bins {
        if bins.len() == 1 {
            let entry = &bins[0];
            if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                return Ok(Some(name.to_string()));
            }
        }
        if !bins.is_empty() {
            return Ok(None);
        }
    }

    // Case 2: no `[[bin]]` table, but `src/main.rs` exists → cargo treats it
    // as a default binary named after the package.
    let auto_main = manifest_dir.join("src").join("main.rs");
    let auto_bin_dir = manifest_dir.join("src").join("bin");
    let auto_bins: Vec<PathBuf> = if auto_bin_dir.is_dir() {
        match fs::read_dir(&auto_bin_dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("rs"))
                .collect(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let mut candidates: Vec<String> = Vec::new();
    if auto_main.is_file() {
        if let Some(name) = package_name.as_deref() {
            candidates.push(name.to_string());
        }
    }
    for entry in &auto_bins {
        if let Some(stem) = entry.file_stem().and_then(|s| s.to_str()) {
            candidates.push(stem.to_string());
        }
    }

    if candidates.len() == 1 {
        return Ok(Some(candidates.remove(0)));
    }
    Ok(None)
}
