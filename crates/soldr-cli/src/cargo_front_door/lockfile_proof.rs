//! Proof from `Cargo.lock` that a package is not in the dependency graph.
//!
//! Shared by the front door's "skip work that is provably unneeded" paths
//! (managed cmake, soldr#3783; the PyO3 `cargo metadata` probe, soldr#3794).
//! A package is reported absent only when an up-to-date `Cargo.lock` has no
//! package named exactly that. Any uncertainty reports "might be present",
//! which is the pre-optimisation behaviour.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// `true` only when `Cargo.lock` proves no package named exactly
/// `package_name` is in the graph. No lock, an unreadable lock, a manifest
/// newer than the lock, an unsupported workspace glob shape, or an `install`
/// subcommand (which resolves a lockfile we cannot see) all return `false`.
pub(crate) fn lockfile_lacks_package(args: &[String], cwd: &Path, package_name: &str) -> bool {
    let cwd = cargo_dash_c_dir(args, cwd);
    let start = super::profile_debug::cargo_profile_lookup_start_dir_from(args, &cwd);
    lock_lacks_package(
        super::subcommand::first_cargo_subcommand(args),
        &start,
        package_name,
    )
}

fn lock_lacks_package(subcommand: Option<&str>, manifest_dir: &Path, package_name: &str) -> bool {
    if subcommand == Some("install") {
        return false;
    }
    let Some(lock_dir) = manifest_dir
        .ancestors()
        .find(|dir| dir.join("Cargo.lock").is_file())
    else {
        return false;
    };
    let lock = lock_dir.join("Cargo.lock");
    let Ok(text) = std::fs::read_to_string(&lock) else {
        return false;
    };
    let Some(lock_mtime) = mtime(&lock) else {
        return false;
    };
    // A manifest edited after the last resolve (clud#500: a just-added
    // `whisper-rs-sys`) makes the lock untrustworthy.
    let Some(manifests) = manifests_to_check(manifest_dir, lock_dir) else {
        return false;
    };
    if manifests
        .iter()
        .any(|manifest| mtime(manifest).is_none_or(|time| time > lock_mtime))
    {
        return false;
    }
    !lock_has_package(&text, package_name)
}

/// The manifest dir's and lock dir's `Cargo.toml`, plus every workspace
/// member's. A member pattern ending in a single `/*` component is expanded
/// by listing that directory; any other glob shape returns `None`, which
/// means "no proof" rather than a guess.
fn manifests_to_check(manifest_dir: &Path, lock_dir: &Path) -> Option<Vec<PathBuf>> {
    let mut out = vec![manifest_dir.join("Cargo.toml"), lock_dir.join("Cargo.toml")];
    let members = std::fs::read_to_string(lock_dir.join("Cargo.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Value>().ok())
        .and_then(|value| value.get("workspace")?.get("members")?.as_array().cloned())
        .unwrap_or_default();
    for member in members.iter().filter_map(toml::Value::as_str) {
        if !member.contains(['*', '?', '[']) {
            out.push(lock_dir.join(member).join("Cargo.toml"));
            continue;
        }
        let parent = member.strip_suffix("/*")?;
        if parent.contains(['*', '?', '[']) {
            return None;
        }
        for entry in std::fs::read_dir(lock_dir.join(parent)).ok()?.flatten() {
            out.push(entry.path().join("Cargo.toml"));
        }
    }
    out.sort();
    out.dedup();
    out.retain(|path| path.is_file());
    Some(out)
}

fn lock_has_package(text: &str, package_name: &str) -> bool {
    let wanted = format!("name = \"{package_name}\"");
    text.lines().any(|line| line.trim() == wanted)
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Honour cargo's `-C <dir>` global arg (before the subcommand).
fn cargo_dash_c_dir(args: &[String], cwd: &Path) -> PathBuf {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "-C" {
            if let Some(dir) = iter.next() {
                return cwd.join(dir);
            }
        }
    }
    cwd.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(time)).unwrap();
    }

    fn project(lock_body: Option<&str>) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nname = \"x\"\n").unwrap();
        set_mtime(&manifest, SystemTime::now() - Duration::from_secs(1000));
        if let Some(body) = lock_body {
            std::fs::write(tmp.path().join("Cargo.lock"), body).unwrap();
        }
        tmp
    }

    fn lock_with(names: &[&str]) -> String {
        names
            .iter()
            .map(|name| format!("[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\n"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn pyo3_present_is_not_provably_absent() {
        let tmp = project(Some(&lock_with(&["serde", "pyo3"])));
        assert!(!lockfile_lacks_package(
            &args(&["build"]),
            tmp.path(),
            "pyo3"
        ));
    }

    #[test]
    fn pyo3_absent_is_provably_absent() {
        let tmp = project(Some(&lock_with(&["serde"])));
        assert!(lockfile_lacks_package(
            &args(&["build"]),
            tmp.path(),
            "pyo3"
        ));
    }

    #[test]
    fn pyo3_lookalikes_do_not_count_as_pyo3() {
        // The detector keys on the exact name `pyo3` (pyo3_detect.rs), so
        // pyo3-build-config alone cannot produce a PyO3 plan.
        let tmp = project(Some(&lock_with(&[
            "pyo3-build-config",
            "pyo3-ffi",
            "pyo3-macros",
        ])));
        assert!(lockfile_lacks_package(
            &args(&["build"]),
            tmp.path(),
            "pyo3"
        ));
    }

    #[test]
    fn no_lock_is_not_proof() {
        let tmp = project(None);
        assert!(!lockfile_lacks_package(
            &args(&["build"]),
            tmp.path(),
            "pyo3"
        ));
    }

    #[test]
    fn stale_lock_is_not_proof() {
        let tmp = project(Some(&lock_with(&["serde"])));
        set_mtime(
            &tmp.path().join("Cargo.lock"),
            SystemTime::now() - Duration::from_secs(2000),
        );
        assert!(!lockfile_lacks_package(
            &args(&["build"]),
            tmp.path(),
            "pyo3"
        ));
    }

    #[test]
    fn install_is_not_proof() {
        let tmp = project(Some(&lock_with(&["serde"])));
        assert!(!lockfile_lacks_package(
            &args(&["install", "x"]),
            tmp.path(),
            "pyo3"
        ));
    }
}
