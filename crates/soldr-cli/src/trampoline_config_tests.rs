//! Unit tests for `trampoline_config`: the cargo-config walker and the
//! content digest.

use super::*;
use std::fs;

/// Env vars (`CARGO_HOME`, `HOME`, `USERPROFILE`) are process-global, so
/// tests that mutate them must serialize.
///
/// soldr#1938: this was a module-local `static ENV_LOCK: Mutex<()>`. Two
/// mutexes over one process environment exclude nothing -- each test took
/// *a* lock, so every one read as correct in isolation, and you had to
/// notice the locks were different objects.
///
/// These three are worse than a private-lock variable that nobody else
/// writes, because they are **ambient**: `trampoline_config.rs`,
/// `binaries.rs`, `exec_cmd.rs`, and `rust_plan_memo.rs` all read them to
/// resolve paths. Any concurrent test that resolves a path at all is a
/// reader, whether or not it knows this module exists -- so the barrier
/// has to be the one the rest of the crate uses.
use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;

/// Build a tempdir-anchored test bed and return `(tempdir, manifest_dir,
/// cargo_home)`. We override `CARGO_HOME` and `HOME` per test so the
/// walker can't reach into the developer's real `~/.cargo`.
fn temp_layout(label: &str) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let _ = label;
    let tmp = tempfile::tempdir().expect("tempdir");
    let manifest_dir = tmp.path().join("proj");
    fs::create_dir_all(&manifest_dir).expect("create manifest dir");
    let cargo_home = tmp.path().join("cargo-home");
    fs::create_dir_all(&cargo_home).expect("create cargo home");
    (tmp, manifest_dir, cargo_home)
}

fn write_config(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let cargo_dir = dir.join(".cargo");
    fs::create_dir_all(&cargo_dir).expect("create .cargo");
    let path = cargo_dir.join("config.toml");
    fs::write(&path, body).expect("write config.toml");
    path
}

/// Helper that scopes env-var mutations for the duration of one test.
/// `std::env::set_var` is process-global; with `cargo test` running
/// these tests serially within the binary, restoring on drop is enough.
struct EnvGuard {
    keys: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn new() -> Self {
        Self { keys: Vec::new() }
    }
    fn set(&mut self, key: &'static str, value: &std::path::Path) {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        self.keys.push((key, prev));
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prev) in self.keys.drain(..) {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn discovers_configs_walking_up_three_levels() {
    let _guard = ENV_LOCK.lock();
    let (_tmp, manifest_dir, cargo_home) = temp_layout("walk-three-levels");
    let mut env = EnvGuard::new();
    env.set("CARGO_HOME", &cargo_home);
    // Point HOME at the tempdir too so the walker can't escape.
    env.set("HOME", _tmp.path());
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        env.set("USERPROFILE", _tmp.path());
    }

    // Layout: tmp/a/b/c/proj/Cargo.toml — `proj` is the manifest dir.
    // Place .cargo/config.toml at depths 1, 2, and 3 above it.
    let deep_manifest = manifest_dir.join("a").join("b").join("c").join("proj");
    fs::create_dir_all(&deep_manifest).expect("create deep manifest");
    let depth_1 = write_config(deep_manifest.parent().unwrap(), "[build]\n");
    let depth_2 = write_config(deep_manifest.parent().unwrap().parent().unwrap(), "[net]\n");
    let depth_3 = write_config(
        deep_manifest
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap(),
        "[registries]\n",
    );

    let files = discover_cargo_config_files(&deep_manifest);
    let strs: Vec<String> = files
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let canon = |p: &std::path::Path| {
        fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .to_string()
    };
    assert!(strs.contains(&canon(&depth_1)), "missing depth 1: {strs:?}");
    assert!(strs.contains(&canon(&depth_2)), "missing depth 2: {strs:?}");
    assert!(strs.contains(&canon(&depth_3)), "missing depth 3: {strs:?}");

    // Sorted by canonical path string — deterministic.
    let mut sorted = strs.clone();
    sorted.sort();
    assert_eq!(strs, sorted, "discovered files should be sorted");
}

#[test]
fn discovers_cargo_home_config() {
    let _guard = ENV_LOCK.lock();
    let (_tmp, manifest_dir, cargo_home) = temp_layout("cargo-home");
    let mut env = EnvGuard::new();
    env.set("CARGO_HOME", &cargo_home);
    env.set("HOME", _tmp.path());
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        env.set("USERPROFILE", _tmp.path());
    }

    let home_config = write_config(&cargo_home, "[build]\nrustflags = []\n");
    let files = discover_cargo_config_files(&manifest_dir);
    let canon_home = fs::canonicalize(&home_config)
        .unwrap_or(home_config.clone())
        .to_string_lossy()
        .to_string();
    let found: Vec<String> = files
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    assert!(
        found.iter().any(|s| s == &canon_home),
        "cargo home config not discovered: {found:?}"
    );
}

#[test]
fn discovers_legacy_config_file_without_extension() {
    let _guard = ENV_LOCK.lock();
    let (_tmp, manifest_dir, cargo_home) = temp_layout("legacy-name");
    let mut env = EnvGuard::new();
    env.set("CARGO_HOME", &cargo_home);
    env.set("HOME", _tmp.path());
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        env.set("USERPROFILE", _tmp.path());
    }

    // Use the legacy bare-`config` name.
    let cargo_dir = manifest_dir.join(".cargo");
    fs::create_dir_all(&cargo_dir).expect("create .cargo");
    let legacy = cargo_dir.join("config");
    fs::write(&legacy, "[build]\n").expect("write legacy config");

    let files = discover_cargo_config_files(&manifest_dir);
    let canon = fs::canonicalize(&legacy)
        .unwrap_or(legacy.clone())
        .to_string_lossy()
        .to_string();
    let found: Vec<String> = files
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    assert!(
        found.iter().any(|s| s == &canon),
        "legacy config not discovered: {found:?}"
    );
}

#[test]
fn digest_changes_when_config_content_changes() {
    let _guard = ENV_LOCK.lock();
    let (_tmp, manifest_dir, cargo_home) = temp_layout("content-change");
    let mut env = EnvGuard::new();
    env.set("CARGO_HOME", &cargo_home);
    env.set("HOME", _tmp.path());
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        env.set("USERPROFILE", _tmp.path());
    }

    let cfg_path = write_config(
        &manifest_dir,
        "[build]\nrustflags = [\"-C\", \"opt-level=0\"]\n",
    );
    let before = cargo_config_digest(&manifest_dir);

    fs::write(
        &cfg_path,
        "[build]\nrustflags = [\"-C\", \"opt-level=0\", \"-C\", \"debug-assertions=on\"]\n",
    )
    .expect("rewrite config");
    let after = cargo_config_digest(&manifest_dir);

    assert_ne!(before, after, "digest must change when rustflags change");
}

#[test]
fn digest_changes_when_config_appears() {
    let _guard = ENV_LOCK.lock();
    let (_tmp, manifest_dir, cargo_home) = temp_layout("appears");
    let mut env = EnvGuard::new();
    env.set("CARGO_HOME", &cargo_home);
    env.set("HOME", _tmp.path());
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        env.set("USERPROFILE", _tmp.path());
    }

    let before = cargo_config_digest(&manifest_dir);
    write_config(&manifest_dir, "[build]\nrustflags = []\n");
    let after = cargo_config_digest(&manifest_dir);
    assert_ne!(before, after, "digest must change when a config appears");
}

#[test]
fn digest_stable_for_unchanged_inputs() {
    let _guard = ENV_LOCK.lock();
    let (_tmp, manifest_dir, cargo_home) = temp_layout("stable");
    let mut env = EnvGuard::new();
    env.set("CARGO_HOME", &cargo_home);
    env.set("HOME", _tmp.path());
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        env.set("USERPROFILE", _tmp.path());
    }

    write_config(
        &manifest_dir,
        "[build]\nrustflags = [\"-C\", \"opt-level=3\"]\n",
    );
    let a = cargo_config_digest(&manifest_dir);
    let b = cargo_config_digest(&manifest_dir);
    assert_eq!(a, b, "digest must be deterministic");
    assert!(a.starts_with("blake3:"), "digest format: {a}");
}

#[test]
fn digest_with_no_configs_is_well_defined() {
    let _guard = ENV_LOCK.lock();
    let (_tmp, manifest_dir, cargo_home) = temp_layout("no-configs");
    let mut env = EnvGuard::new();
    env.set("CARGO_HOME", &cargo_home);
    env.set("HOME", _tmp.path());
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        env.set("USERPROFILE", _tmp.path());
    }

    let d = cargo_config_digest(&manifest_dir);
    assert!(d.starts_with("blake3:"));
    // Empty input is still deterministic.
    let d2 = cargo_config_digest(&manifest_dir);
    assert_eq!(d, d2);
}
