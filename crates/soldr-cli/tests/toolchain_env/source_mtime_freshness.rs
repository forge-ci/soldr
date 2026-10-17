//! Cargo's default build-script package scan must survive archive replay (#3851).
use std::path::Path;

use soldr_cli::cache_lib::save::{
    load, read_manifest_from_archive, save, save_delta, LoadOptions, SaveDeltaOptions, SaveOptions,
    SaveProfile,
};

use crate::common;

fn test_compile(ws: &Path, target: &Path) -> String {
    let output = common::isolated_soldr_command_with_target(target)
        .current_dir(ws)
        .args([
            "--no-cache",
            "cargo",
            "test",
            "--offline",
            "--locked",
            "--no-run",
            "-vv",
        ])
        .env("CARGO_LOG", "cargo::core::compiler::fingerprint=info")
        .output()
        .expect("run fixture through Soldr");
    let log = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "fixture build failed: {log}");
    log
}

fn create_workspace(ws: &Path) -> bool {
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname = \"mtime-freshness-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    // No rerun-if-changed directives: Cargo scans the whole package, as obs-util does.
    std::fs::write(ws.join("build.rs"), "fn main() {}\n").unwrap();
    std::fs::write(ws.join("src/lib.rs"), "pub fn value() -> u8 { 42 }\n").unwrap();
    let source = ws.join("src/platform.rs");
    std::fs::write(
        &source,
        "// included in the package scan, even though not compiled\n",
    )
    .unwrap();
    let requested = filetime::FileTime::from_unix_time(1_792_262_706, 251_336_800);
    filetime::set_file_mtime(&source, requested).unwrap();
    let original =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&source).unwrap());
    if original.nanoseconds().is_multiple_of(1_000_000) {
        eprintln!("filesystem cannot represent submillisecond source mtimes: {original:?}");
        return false;
    }
    // Make this uncompiled file the newest package input, so Cargo records its
    // precise mtime as a precalculated build-script component. Keep it safely
    // before the first build, without sleeping or fabricating build fingerprints.
    for rel in ["Cargo.toml", "build.rs", "src/lib.rs"] {
        filetime::set_file_mtime(
            ws.join(rel),
            filetime::FileTime::from_unix_time(1_600_000_000, 0),
        )
        .unwrap();
    }
    // Generate the lockfile before the measured package scan; otherwise its
    // first creation could become Cargo's newest input.
    std::fs::write(
        ws.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"mtime-freshness-fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    filetime::set_file_mtime(
        ws.join("Cargo.lock"),
        filetime::FileTime::from_unix_time(1_600_000_000, 0),
    )
    .unwrap();
    true
}

#[test]
fn default_build_script_stays_fresh_after_full_and_base_delta_restore() {
    let root = tempfile::tempdir().unwrap();
    let ws = root.path().join("workspace");
    let target = ws.join("target");
    if !create_workspace(&ws) {
        return;
    }
    test_compile(&ws, &target);
    assert!(test_compile(&ws, &target).contains("Fresh mtime-freshness-fixture"));

    let base_archive = root.path().join("base.tar.zst");
    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&target),
        out: &base_archive,
        zstd_level: 1,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .unwrap();
    let base = read_manifest_from_archive(&base_archive).unwrap();
    let delta_archive = root.path().join("delta.tar.zst");
    save_delta(&SaveDeltaOptions {
        workspace: Some(&ws),
        cache_dir: &target,
        base_manifest: &base,
        out: &delta_archive,
        zstd_level: 1,
        threads: None,
        profile: SaveProfile::Full,
    })
    .unwrap();

    for with_delta in [false, true] {
        // Rename the producer aside and restore into the identical target path.
        let aside = root.path().join(format!("producer-{with_delta}"));
        std::fs::rename(&target, aside).unwrap();
        filetime::set_file_mtime(ws.join("src/platform.rs"), filetime::FileTime::now()).unwrap();
        for archive in std::iter::once(&base_archive).chain(with_delta.then_some(&delta_archive)) {
            load(&LoadOptions {
                archive,
                cache_dir: Some(&target),
                workspace: Some(&ws),
                threads: None,
                mtimes_only: false,
                profile_extract: false,
                auto_defender_exclude: false,
            })
            .unwrap();
        }
        let log = test_compile(&ws, &target);
        assert!(
            log.contains("Fresh mtime-freshness-fixture"),
            "with_delta={with_delta}: {log}"
        );
        assert!(!log.contains("PrecalculatedComponentsChanged"), "{log}");
    }
}
