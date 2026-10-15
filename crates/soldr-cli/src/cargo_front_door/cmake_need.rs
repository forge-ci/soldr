//! Decides whether native compiles need the managed cmake/ninja (soldr#3783).
//!
//! Provisioning is skipped only when cmake is provably unneeded: an
//! up-to-date `Cargo.lock` with no package named exactly `cmake`. Any
//! uncertainty provisions, which is the pre-#3783 behaviour.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// `true` when the managed cmake/ninja must be provisioned.
pub(super) fn managed_cmake_needed(args: &[String], cwd: &Path) -> bool {
    let cwd = cargo_dash_c_dir(args, cwd);
    let start = super::profile_debug::cargo_profile_lookup_start_dir_from(args, &cwd);
    decide(super::subcommand::first_cargo_subcommand(args), &start)
}

fn decide(subcommand: Option<&str>, manifest_dir: &Path) -> bool {
    // `install` resolves a remote lockfile we cannot see.
    if subcommand == Some("install") {
        return true;
    }
    let Some(lock_dir) = manifest_dir
        .ancestors()
        .find(|dir| dir.join("Cargo.lock").is_file())
    else {
        return true;
    };
    let lock = lock_dir.join("Cargo.lock");
    let Ok(text) = std::fs::read_to_string(&lock) else {
        return true;
    };
    let Some(lock_mtime) = mtime(&lock) else {
        return true;
    };
    // A manifest edited after the last resolve (clud#500: a just-added
    // `whisper-rs-sys`) makes the lock untrustworthy.
    if manifests_to_check(manifest_dir, lock_dir)
        .iter()
        .any(|manifest| mtime(manifest).is_none_or(|time| time > lock_mtime))
    {
        return true;
    }
    lock_has_cmake_package(&text)
}

/// The manifest dir's and lock dir's `Cargo.toml`, plus the lock dir's
/// workspace `members` that are plain paths. Glob members are not expanded:
/// that is a documented limit, covered by the two manifests above in the
/// common case of editing the member being built.
fn manifests_to_check(manifest_dir: &Path, lock_dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![manifest_dir.join("Cargo.toml"), lock_dir.join("Cargo.toml")];
    let members = std::fs::read_to_string(lock_dir.join("Cargo.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Value>().ok())
        .and_then(|value| value.get("workspace")?.get("members")?.as_array().cloned())
        .unwrap_or_default();
    for member in members.iter().filter_map(toml::Value::as_str) {
        if !member.contains(['*', '?', '[']) {
            out.push(lock_dir.join(member).join("Cargo.toml"));
        }
    }
    out.sort();
    out.dedup();
    out.retain(|path| path.is_file());
    out
}

fn lock_has_cmake_package(text: &str) -> bool {
    text.lines().any(|line| line.trim() == "name = \"cmake\"")
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

    /// Project with a lock containing `lock_body`, lock newer than manifest.
    fn project(lock_body: Option<&str>) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let old = SystemTime::now() - Duration::from_secs(1000);
        let manifest = tmp.path().join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nname = \"x\"\n").unwrap();
        set_mtime(&manifest, old);
        if let Some(body) = lock_body {
            std::fs::write(tmp.path().join("Cargo.lock"), body).unwrap();
        }
        tmp
    }

    const NO_CMAKE: &str = "[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\n";

    #[test]
    fn no_lock_is_needed() {
        let tmp = project(None);
        assert!(managed_cmake_needed(&args(&["build"]), tmp.path()));
    }

    #[test]
    fn lock_without_cmake_is_skipped() {
        let tmp = project(Some(NO_CMAKE));
        assert!(!managed_cmake_needed(&args(&["build"]), tmp.path()));
    }

    #[test]
    fn lock_with_cmake_is_needed() {
        let tmp = project(Some(
            "[[package]]\nname = \"cmake\"\nversion = \"0.1.50\"\n",
        ));
        assert!(managed_cmake_needed(&args(&["build"]), tmp.path()));
    }

    #[test]
    fn lookalike_name_is_skipped() {
        let tmp = project(Some(
            "[[package]]\nname = \"cmake-foo\"\nversion = \"1.0.0\"\n",
        ));
        assert!(!managed_cmake_needed(&args(&["build"]), tmp.path()));
    }

    #[test]
    fn manifest_newer_than_lock_is_needed() {
        let tmp = project(Some(NO_CMAKE));
        let lock = tmp.path().join("Cargo.lock");
        set_mtime(&lock, SystemTime::now() - Duration::from_secs(2000));
        assert!(managed_cmake_needed(&args(&["build"]), tmp.path()));
    }

    #[test]
    fn install_is_needed() {
        let tmp = project(Some(NO_CMAKE));
        assert!(managed_cmake_needed(&args(&["install", "foo"]), tmp.path()));
    }

    #[test]
    fn manifest_path_into_member_finds_parent_lock() {
        let tmp = project(Some(NO_CMAKE));
        let member = tmp.path().join("crates").join("m");
        std::fs::create_dir_all(&member).unwrap();
        let manifest = member.join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nname = \"m\"\n").unwrap();
        set_mtime(&manifest, SystemTime::now() - Duration::from_secs(1000));
        let a = args(&["build", "--manifest-path", manifest.to_str().unwrap()]);
        assert!(!managed_cmake_needed(&a, Path::new("/nonexistent")));
        // A stale lock relative to the member manifest provisions.
        set_mtime(&manifest, SystemTime::now() + Duration::from_secs(1000));
        assert!(managed_cmake_needed(&a, Path::new("/nonexistent")));
    }

    #[test]
    fn dash_c_redirects_lookup() {
        let tmp = project(Some(NO_CMAKE));
        let a = args(&["-C", tmp.path().to_str().unwrap(), "build"]);
        assert!(!managed_cmake_needed(&a, Path::new("/nonexistent")));
    }
}
