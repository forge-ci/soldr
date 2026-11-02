#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::fs;
use std::process::Command;

fn write_bytes(path: &std::path::Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, bytes).expect("write file");
}

fn touch_dir(path: &std::path::Path) {
    fs::create_dir_all(path).expect("create dir");
}

#[test]
fn cache_trim_target_ci_profile_strips_recreatable_noise() {
    let cache_root = unique_temp_dir("cache-trim-target-ci");
    let target = cache_root.join("target");

    // Recreatable noise the CI profile should drop.
    let large_stderr = target.join("debug/build/ring-aaaaaaaaaaaaa/stderr");
    let build_script = target.join("debug/build/foo-bbbbbbbbbbbbb/build-script-build");
    let incremental_dir = target.join("debug/incremental/foo-ccccccccccccc");
    let examples_dir = target.join("debug/examples");

    write_bytes(&large_stderr, &vec![0u8; 200 * 1024]);
    write_bytes(&build_script, b"binary");
    touch_dir(&incremental_dir);
    write_bytes(&incremental_dir.join("inc.bin"), b"x");
    touch_dir(&examples_dir);
    write_bytes(&examples_dir.join("ex1"), b"x");

    // Allowlisted bookkeeping that must survive (bit-exact).
    let rustc_info = target.join(".rustc_info.json");
    let cachedir_tag = target.join("CACHEDIR.TAG");
    write_bytes(&rustc_info, b"{}");
    write_bytes(&cachedir_tag, b"Signature: cachedir tag\n");

    // Small stderr stays.
    let small_stderr = target.join("debug/build/foo-bbbbbbbbbbbbb/stderr");
    write_bytes(&small_stderr, b"no errors\n");

    // Sibling that should NOT be touched.
    let real_artifact = target.join("debug/deps/libreal-eeeeeeeeeeeee.rlib");
    write_bytes(&real_artifact, b"keep me");

    let output = Command::new(common::soldr_bin())
        .args(["cache", "trim-target"])
        .arg(&target)
        .args(["--profile", "ci", "--force", "--json"])
        .output()
        .expect("failed to run soldr cache trim-target --profile=ci --force --json");

    assert!(
        output.status.success(),
        "cache trim-target failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout)
        .expect("cache trim-target --json must produce parseable JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "cache trim-target");
    assert_eq!(json["profile"], "ci");
    assert_eq!(json["dry_run"], false);
    assert_eq!(json["incremental_removed_count"], 1);
    // strip: large_stderr + build_script + examples = 3
    assert_eq!(json["strip_deleted"], 3);
    assert!(json["total_reclaimed_bytes"].as_u64().unwrap() > 0);

    // Noise gone.
    assert!(!large_stderr.exists(), "large stderr must be deleted");
    assert!(
        !build_script.exists(),
        "build-script binary must be deleted"
    );
    assert!(
        !target.join("debug/incremental").exists(),
        "incremental/ must be deleted"
    );
    assert!(!examples_dir.exists(), "examples/ must be deleted");

    // Bookkeeping survives bit-exact.
    assert!(rustc_info.exists(), ".rustc_info.json must survive");
    assert_eq!(
        fs::read(&rustc_info).unwrap(),
        b"{}",
        ".rustc_info.json must be bit-exact"
    );
    assert!(cachedir_tag.exists(), "CACHEDIR.TAG must survive");

    // Small stderr + real artifact untouched.
    assert!(small_stderr.exists(), "small stderr must survive");
    assert!(real_artifact.exists(), ".rlib in deps/ must survive");
}

#[test]
fn cache_trim_target_local_profile_only_prunes_hash_siblings() {
    let cache_root = unique_temp_dir("cache-trim-target-local");
    let target = cache_root.join("target");
    let parent = target.join("debug/deps");
    fs::create_dir_all(&parent).expect("create deps dir");
    let older = parent.join("libfoo-aaaaaaaaaaaaa.rlib");
    let newer = parent.join("libfoo-bbbbbbbbbbbbb.rlib");
    fs::write(&older, b"older").unwrap();
    fs::write(&newer, b"newer-content").unwrap();

    let older_when =
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let newer_when =
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_500);
    fs::File::options()
        .write(true)
        .open(&older)
        .unwrap()
        .set_modified(older_when)
        .unwrap();
    fs::File::options()
        .write(true)
        .open(&newer)
        .unwrap()
        .set_modified(newer_when)
        .unwrap();

    // Noise that the LOCAL profile must NOT touch.
    let large_stderr = target.join("debug/build/ring-aaaaaaaaaaaaa/stderr");
    let incremental_dir = target.join("debug/incremental/foo-ccccccccccccc");
    write_bytes(&large_stderr, &vec![0u8; 200 * 1024]);
    touch_dir(&incremental_dir);

    let output = Command::new(common::soldr_bin())
        .args(["cache", "trim-target"])
        .arg(&target)
        .args(["--profile", "local", "--force", "--json"])
        .output()
        .expect("failed to run soldr cache trim-target --profile=local --force --json");

    assert!(
        output.status.success(),
        "cache trim-target failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("parse json");
    assert_eq!(json["profile"], "local");
    assert_eq!(json["incremental_removed_count"], 0);
    assert_eq!(json["strip_deleted"], 0);
    // prune still ran: scanned 3 (libfoo-aaa, libfoo-bbb, foo-ccc), but
    // libfoo-* are bucketed together (delete 1), foo-ccc is alone.
    assert!(json["prune_scanned"].as_u64().unwrap() >= 2);
    assert!(json["prune_deleted"].as_u64().unwrap() >= 1);

    // Local profile preserves noise.
    assert!(
        large_stderr.exists(),
        "local profile must keep large stderr"
    );
    assert!(
        incremental_dir.exists(),
        "local profile must keep incremental/"
    );

    // Newest sibling survives.
    assert!(newer.exists(), "newer sibling must survive");
    assert!(!older.exists(), "older sibling must be pruned");
}

#[test]
fn cache_trim_target_dry_run_does_not_modify_disk() {
    let cache_root = unique_temp_dir("cache-trim-target-dry");
    let target = cache_root.join("target");
    let large_stderr = target.join("debug/build/ring-aaaaaaaaaaaaa/stderr");
    let incremental_dir = target.join("debug/incremental/foo-ccccccccccccc");
    write_bytes(&large_stderr, &vec![0u8; 200 * 1024]);
    touch_dir(&incremental_dir);

    let output = Command::new(common::soldr_bin())
        .args(["cache", "trim-target"])
        .arg(&target)
        .args(["--profile", "ci", "--dry-run", "--json"])
        .output()
        .expect("failed to run soldr cache trim-target --profile=ci --dry-run --json");

    assert!(
        output.status.success(),
        "cache trim-target failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("parse json");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["incremental_removed_count"], 1);
    assert!(json["strip_deleted"].as_u64().unwrap() >= 1);

    // Dry-run preserves everything.
    assert!(large_stderr.exists(), "dry-run must preserve large stderr");
    assert!(
        incremental_dir.exists(),
        "dry-run must preserve incremental/"
    );
}

#[test]
fn cache_trim_target_ignores_a_stale_cargo_lock_file() {
    let cache_root = unique_temp_dir("cache-trim-target-stale-lock");
    let target = cache_root.join("target");
    let parent = target.join("debug/deps");
    fs::create_dir_all(&parent).expect("create deps");
    let rlib = parent.join("libfoo-aaaaaaaaaaaaa.rlib");
    fs::write(&rlib, b"x").unwrap();
    // Cargo leaves this sentinel behind after every build. Its existence is
    // not activity (cargo_lock.rs: "Existence alone is not activity"), so a
    // finished build must not block the trim forever.
    let lock = target.join(".cargo-lock");
    fs::write(&lock, b"").unwrap();

    let output = Command::new(common::soldr_bin())
        .args(["cache", "trim-target"])
        .arg(&target)
        .args(["--profile", "ci", "--force"])
        .output()
        .expect("failed to run soldr cache trim-target with a stale lock");

    assert!(
        output.status.success(),
        "a stale .cargo-lock file must not be treated as an active build: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(rlib.exists(), "the newest artifact must be kept");
    assert!(lock.exists(), "the sentinel file itself is never deleted");
}

#[test]
fn cache_trim_target_refuses_when_cargo_lock_is_held() {
    use std::fs::File;

    let cache_root = unique_temp_dir("cache-trim-target-held-lock");
    let target = cache_root.join("target");
    let parent = target.join("debug/deps");
    fs::create_dir_all(&parent).expect("create deps");
    let rlib = parent.join("libfoo-bbbbbbbbbbbbb.rlib");
    fs::write(&rlib, b"x").unwrap();
    let lock = target.join(".cargo-lock");
    fs::write(&lock, b"").unwrap();
    let held = File::open(&lock).expect("open sentinel");
    held.lock().expect("hold the sentinel lock");

    let output = Command::new(common::soldr_bin())
        .args(["cache", "trim-target"])
        .arg(&target)
        .args(["--profile", "ci", "--force"])
        .output()
        .expect("failed to run soldr cache trim-target with a held lock");

    assert!(
        !output.status.success(),
        "a held .cargo-lock must refuse the trim"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(".cargo-lock") || stderr.contains("active build"),
        "stderr must mention .cargo-lock / active build: {stderr}"
    );

    // Nothing on disk should have been touched.
    assert!(rlib.exists(), "lock refusal must not delete anything");
    assert!(lock.exists(), "lock file must survive");
}
