use super::*;
use std::io::Write;
use tempfile::TempDir;

#[test]
fn cook_level_defaults_to_9_and_follows_soldr_zstd_level() {
    // 9 packs a real obs-studio closure in 1.2 s at 53 MiB; 19 with the
    // long window took 26 s for 38 MiB (soldr#4162).
    assert_eq!(cook_zstd_level_from(None), 9);
    assert_eq!(cook_zstd_level_from(Some(1)), 1);
    assert_eq!(cook_zstd_level_from(Some(-3)), -3);
}

#[test]
fn default_cook_archive_skips_the_long_window() {
    assert!(!cook_uses_long_window(cook_zstd_level_from(None)));
}

#[test]
fn config_default_matches_the_cook_level() {
    assert_eq!(
        crate::core::CookConfig::default_zstd_level(),
        COOK_ZSTD_LEVEL
    );
}

#[test]
fn long_window_only_at_high_levels() {
    assert!(cook_uses_long_window(19));
    assert!(cook_uses_long_window(COOK_LONG_WINDOW_MIN_LEVEL));
    assert!(!cook_uses_long_window(COOK_LONG_WINDOW_MIN_LEVEL - 1));
    assert!(!cook_uses_long_window(1));
    assert!(!cook_uses_long_window(-5));
}

#[test]
fn fast_level_archive_round_trips() {
    let dir = TempDir::new().expect("tempdir");
    let source = dir.path().join("debug");
    write_file(&source.join("deps/libfoo.rlib"), &[7u8; 4096]);
    let cook = dir.path().join("cook");
    let fast = pack_cook_archive_at_level(&source, &cook, -1).expect("pack -1");
    let slow =
        pack_cook_archive_at_level(&source, &dir.path().join("cook19"), 19).expect("pack 19");
    assert_ne!(fast.sha256, slow.sha256, "the level must reach the encoder");
    let decoder = zstd::Decoder::new(File::open(&fast.path).expect("open")).expect("decoder");
    let mut archive = tar::Archive::new(decoder);
    let names: Vec<String> = archive
        .entries()
        .expect("entries")
        .map(|e| {
            e.expect("entry")
                .path()
                .expect("path")
                .display()
                .to_string()
        })
        .collect();
    assert!(
        names.iter().any(|n| n == "debug/deps/libfoo.rlib"),
        "{names:?}"
    );
}

fn write_file(p: &Path, bytes: &[u8]) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    let mut f = File::create(p).expect("create");
    f.write_all(bytes).expect("write");
}

fn manifest_name() -> &'static str {
    concat!("Car", "go.toml")
}

fn lock_name() -> &'static str {
    concat!("Car", "go.lock")
}

#[test]
fn recipe_hash_skips_large_operational_state_trees() {
    let dir = TempDir::new().expect("tempdir");
    write_file(&dir.path().join(lock_name()), b"lock-v1\n");
    write_file(&dir.path().join(manifest_name()), b"[workspace]\n");
    write_file(
        &dir.path().join("crates/app").join(manifest_name()),
        b"[package]\nname='app'\nversion='0.1.0'\n",
    );
    write_file(
        &dir.path().join("vendor/notify").join(manifest_name()),
        b"[package]\nname='notify'\nversion='1.0.0'\n",
    );
    let baseline = compute_recipe_hash_proxy(dir.path()).expect("baseline hash");

    let mut state_dirs = vec![
        ".cache",
        ".claude",
        ".clud",
        ".codex",
        ".extern-repos",
        ".git",
        ".loop",
        ".perf-local",
        ".pytest_cache",
        ".rustup",
        ".soldr",
        ".venv",
        "node_modules",
        "target",
    ];
    state_dirs.push(concat!(".car", "go"));
    for state_dir in state_dirs {
        for index in 0..100 {
            write_file(
                &dir.path()
                    .join(state_dir)
                    .join(format!("decoy-{index}"))
                    .join(manifest_name()),
                format!("[package]\nname='decoy-{index}'\n").as_bytes(),
            );
        }
    }

    let after_decoys = compute_recipe_hash_proxy(dir.path()).expect("hash after decoys");
    assert_eq!(
        after_decoys, baseline,
        "operational state changed recipe hash"
    );

    write_file(
        &dir.path().join("crates/app").join(manifest_name()),
        b"[package]\nname='app'\nversion='0.2.0'\n",
    );
    assert_ne!(
        compute_recipe_hash_proxy(dir.path()).expect("changed source hash"),
        baseline,
        "included source manifest mutation must change the recipe hash"
    );
}

#[test]
fn referenced_manifests_override_operational_tree_exclusions() {
    let dir = TempDir::new().expect("tempdir");
    write_file(&dir.path().join(lock_name()), b"lock-v1\n");
    write_file(
        &dir.path().join(manifest_name()),
        b"[workspace]\nmembers=['crates/app']\n",
    );
    write_file(
        &dir.path().join("crates/app").join(manifest_name()),
        b"[package]\nname='app'\nversion='0.1.0'\n\
          [dependencies]\nhidden={path='../../.extern-repos/member'}\n",
    );
    let member = dir
        .path()
        .join(".extern-repos/member")
        .join(manifest_name());
    let path_dependency = dir
        .path()
        .join(".perf-local/path-dep")
        .join(manifest_name());
    write_file(
        &member,
        b"[package]\nname='member'\nversion='0.1.0'\n\
          [dependencies]\ntransitive={path='../../.perf-local/path-dep'}\n",
    );
    write_file(
        &path_dependency,
        b"[package]\nname='path-dep'\nversion='0.1.0'\n",
    );
    let baseline = compute_recipe_hash_proxy(dir.path()).expect("baseline hash");

    write_file(
        &member,
        b"[package]\nname='member'\nversion='0.2.0'\n\
          [dependencies]\ntransitive={path='../../.perf-local/path-dep'}\n",
    );
    let member_changed = compute_recipe_hash_proxy(dir.path()).expect("member hash");
    assert_ne!(
        member_changed, baseline,
        "workspace member must affect hash"
    );

    write_file(
        &member,
        b"[package]\nname='member'\nversion='0.1.0'\n\
          [dependencies]\ntransitive={path='../../.perf-local/path-dep'}\n",
    );
    write_file(
        &path_dependency,
        b"[package]\nname='path-dep'\nversion='0.2.0'\n",
    );
    assert_ne!(
        compute_recipe_hash_proxy(dir.path()).expect("path dependency hash"),
        baseline,
        "path dependency must affect hash"
    );
}

#[test]
fn pack_writes_content_addressed_artifact() {
    let dir = TempDir::new().expect("tempdir");
    let source = dir.path().join("release");
    write_file(&source.join("deps").join("libfoo-abc.rlib"), b"hello\n");
    write_file(&source.join("deps").join("libbar-def.rmeta"), b"world\n");

    let cook_dir = dir.path().join("cook");
    let packed = pack_cook_archive(&source, &cook_dir).expect("pack");

    // Filename matches the sha256 hex.
    let expected = artifact_path_for_sha(&cook_dir, &packed.sha256);
    assert_eq!(packed.path, expected);
    assert!(packed.path.is_file());
    assert!(packed.size_bytes > 0);

    // Sanity: tmp dir is empty after success.
    let tmp = cook_dir.join(".tmp");
    let leftover_count = std::fs::read_dir(&tmp)
        .map(|it| it.flatten().count())
        .unwrap_or(0);
    assert_eq!(leftover_count, 0);
}

#[test]
fn pack_is_idempotent_on_same_content() {
    let dir = TempDir::new().expect("tempdir");
    let source = dir.path().join("release");
    write_file(&source.join("deps").join("libfoo-abc.rlib"), b"hello\n");

    let cook_dir = dir.path().join("cook");
    let a = pack_cook_archive(&source, &cook_dir).expect("a");
    let b = pack_cook_archive(&source, &cook_dir).expect("b");

    // Identical input → identical sha → identical artifact path.
    // (Note: tar entries carry per-file mtimes; zstd output may
    // differ. We assert path stability via the daemon-side hash,
    // not byte stability across runs.)
    assert_eq!(a.path.file_name(), b.path.file_name());
    assert!(a.path.is_file());
}

#[test]
fn pack_errors_on_missing_source_dir() {
    let dir = TempDir::new().expect("tempdir");
    let missing = dir.path().join("does-not-exist");
    let cook_dir = dir.path().join("cook");
    let err = pack_cook_archive(&missing, &cook_dir).expect_err("must error");
    match err {
        RegistryError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        other => panic!("expected Io NotFound, got {other:?}"),
    }
}

#[test]
fn sha_abbrev_is_twelve_lowercase_hex_chars() {
    let bytes = [0xAB; 32];
    let abbrev = sha_abbrev(&bytes);
    assert_eq!(abbrev.len(), 12);
    assert!(abbrev
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
}

#[test]
fn artifact_path_matches_sha_filename() {
    let cook = Path::new("/cook");
    let sha = [0x12u8; 32];
    let path = artifact_path_for_sha(cook, &sha);
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(name.ends_with(".tar.zst"));
    assert!(name.starts_with(&hex_lower(&sha)));
}

#[test]
fn verify_sha256_matches_packed_artifact() {
    let dir = TempDir::new().expect("tempdir");
    let source = dir.path().join("release");
    write_file(&source.join("deps").join("libfoo-abc.rlib"), b"hello\n");
    let cook = dir.path().join("cook");
    let packed = pack_cook_archive(&source, &cook).expect("pack");
    assert!(verify_sha256(&packed.path, &packed.sha256).expect("verify"));
    // Flip a byte → mismatch.
    let mut wrong = packed.sha256;
    wrong[0] ^= 0xFF;
    assert!(!verify_sha256(&packed.path, &wrong).expect("verify"));
}

#[test]
fn extract_round_trips_packed_bytes() {
    let dir = TempDir::new().expect("tempdir");
    let source = dir.path().join("release");
    write_file(&source.join("deps").join("libfoo-abc.rlib"), b"foo\n");
    write_file(&source.join("deps").join("libbar-def.rmeta"), b"bar\n");
    let cook = dir.path().join("cook");
    let packed = pack_cook_archive(&source, &cook).expect("pack");

    let dest = dir.path().join("target");
    std::fs::create_dir_all(&dest).unwrap();
    let report = extract_skip_existing(&packed.path, &dest).expect("extract");
    assert!(report.files_written >= 2, "expected files written");
    assert_eq!(report.files_skipped, 0);

    let restored_foo =
        std::fs::read(dest.join("release").join("deps").join("libfoo-abc.rlib")).expect("read foo");
    assert_eq!(restored_foo, b"foo\n");
}

#[test]
fn extract_preserves_fractional_mtime_for_cargo_fingerprints() {
    let dir = TempDir::new().expect("tempdir");
    let source = dir.path().join("release");
    let artifact = source.join("deps").join("libfoo-abc.rlib");
    write_file(&artifact, b"foo\n");
    let expected = filetime::FileTime::from_unix_time(1_700_000_000, 123_456_700);
    filetime::set_file_mtime(&artifact, expected).expect("set source mtime");

    let cook = dir.path().join("cook");
    let packed = pack_cook_archive(&source, &cook).expect("pack");
    let dest = dir.path().join("target");
    extract_skip_existing(&packed.path, &dest).expect("extract");

    let restored = filetime::FileTime::from_last_modification_time(
        &std::fs::metadata(dest.join("release/deps/libfoo-abc.rlib")).expect("restored metadata"),
    );
    assert_eq!(
        restored, expected,
        "tar hydration must not round to seconds"
    );
}

// #1880: cargo's `build-script-build` binaries must come back
// executable. The extractor hand-copies bytes into a fresh
// `File::create` (to honor skip-existing), which lands at the
// umask default and silently drops `+x` unless the tar header's
// mode is applied explicitly.
//
// The consequence in the field is badly disguised: cargo reports
// `could not execute process ... (never executed)` /
// `Permission denied (os error 13)` naming whichever build script
// it happened to reach first, so the failure looks like a flaky
// problem with an unrelated third-party crate.
//
// The regression test for this lives in
// `tests/cook_archive_executable_bit.rs` (Unix-only, since mode
// bits only exist there).
#[test]
fn extract_skips_existing_files() {
    let dir = TempDir::new().expect("tempdir");
    let source = dir.path().join("release");
    write_file(&source.join("deps").join("libfoo-abc.rlib"), b"FRESH\n");
    let cook = dir.path().join("cook");
    let packed = pack_cook_archive(&source, &cook).expect("pack");

    // Pre-populate the destination with user content that must NOT
    // be overwritten.
    let dest = dir.path().join("target");
    let user_path = dest.join("release").join("deps").join("libfoo-abc.rlib");
    write_file(&user_path, b"USER-OWNS-THIS\n");

    let report = extract_skip_existing(&packed.path, &dest).expect("extract");
    assert!(report.files_skipped >= 1, "expected skip count");
    let on_disk = std::fs::read(&user_path).expect("read");
    assert_eq!(
        on_disk, b"USER-OWNS-THIS\n",
        "must not overwrite user state"
    );
}

#[test]
fn quarantine_renames_artifact_in_place() {
    let dir = TempDir::new().expect("tempdir");
    let cook = dir.path().join("cook");
    std::fs::create_dir_all(&cook).unwrap();
    let art = cook.join("deadbeef.tar.zst");
    write_file(&art, b"corrupt bytes");
    let quarantined = quarantine_artifact(&art).expect("quarantine");
    assert!(!art.exists());
    assert!(quarantined.exists());
    assert!(quarantined
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with(".quarantine"));
}

#[test]
fn is_unsafe_tar_path_rejects_traversal() {
    assert!(is_unsafe_tar_path(Path::new("../etc/passwd")));
    assert!(is_unsafe_tar_path(Path::new("/etc/passwd")));
    assert!(is_unsafe_tar_path(Path::new("")));
    assert!(!is_unsafe_tar_path(Path::new("release/deps/libfoo.rlib")));
}
