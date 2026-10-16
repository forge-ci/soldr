//! Decides whether native compiles need the managed cmake/ninja (soldr#3783).
//!
//! Provisioning is skipped only when cmake is provably unneeded: an
//! up-to-date `Cargo.lock` with no package named exactly `cmake` (the proof
//! lives in [`super::lockfile_proof`]). Any uncertainty provisions, which is
//! the pre-#3783 behaviour.

use std::path::Path;

/// `true` when the managed cmake/ninja must be provisioned.
pub(super) fn managed_cmake_needed(args: &[String], cwd: &Path) -> bool {
    !super::lockfile_proof::lockfile_lacks_package(args, cwd, "cmake")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

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

    /// Workspace whose root lists `members = [<pattern>]`, with one member
    /// at `crates/m`; every file older than the lock.
    fn workspace(pattern: &str) -> tempfile::TempDir {
        let tmp = project(Some(NO_CMAKE));
        let old = SystemTime::now() - Duration::from_secs(1000);
        let root = tmp.path().join("Cargo.toml");
        std::fs::write(&root, format!("[workspace]\nmembers = [\"{pattern}\"]\n")).unwrap();
        set_mtime(&root, old);
        let member = tmp.path().join("crates").join("m");
        std::fs::create_dir_all(&member).unwrap();
        let manifest = member.join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nname = \"m\"\n").unwrap();
        set_mtime(&manifest, old);
        tmp
    }

    #[test]
    fn glob_member_edited_after_resolve_is_needed() {
        // clud#500 shape in a `crates/*` workspace: building from the root,
        // a member just gained a cmake-driven dependency.
        let tmp = workspace("crates/*");
        assert!(!managed_cmake_needed(&args(&["build"]), tmp.path()));
        let manifest = tmp.path().join("crates").join("m").join("Cargo.toml");
        set_mtime(&manifest, SystemTime::now() + Duration::from_secs(1000));
        assert!(managed_cmake_needed(&args(&["build"]), tmp.path()));
    }

    #[test]
    fn unsupported_glob_shape_is_needed() {
        let tmp = workspace("crates/m*");
        assert!(managed_cmake_needed(&args(&["build"]), tmp.path()));
    }

    #[test]
    fn dash_c_redirects_lookup() {
        let tmp = project(Some(NO_CMAKE));
        let a = args(&["-C", tmp.path().to_str().unwrap(), "build"]);
        assert!(!managed_cmake_needed(&a, Path::new("/nonexistent")));
    }
}
