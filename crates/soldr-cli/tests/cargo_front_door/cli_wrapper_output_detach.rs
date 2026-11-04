//! Native Unix ownership transition: direct rustc must replace protected cache
//! hardlinks with private writable outputs without changing the cached inode.

use crate::common;
use soldr_platform::fs::{identity, permissions};
use std::fs;
use std::path::Path;

fn unix_ownership_available() -> bool {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        eprintln!("skipped: native link-count ownership proof requires Unix");
        return false;
    }
    true
}

fn hardlink(path: &Path) -> identity::HardlinkIdentity {
    identity::hardlink_identity(&fs::metadata(path).unwrap()).expect("native Unix file identity")
}

#[test]
fn no_cache_rustc_detaches_protected_metadata_outputs() {
    if !unix_ownership_available() {
        return;
    }
    let rustc = common::rustup_which("rustc");
    for (explicit_output, frontend) in [(false, false), (true, false), (false, true), (true, true)]
    {
        let dir = common::unique_temp_dir("direct-output-detach");
        let source = dir.join("probe.rs");
        fs::write(&source, "pub fn value() -> u32 { 42 }\n").unwrap();
        let blob = dir.join("immutable-cache-blob");
        let original = b"protected cached metadata";
        fs::write(&blob, original).unwrap();
        permissions::restore_mode(&blob, Some(0o444)).unwrap();
        let output = dir.join(if explicit_output {
            "requested.rmeta"
        } else {
            "libcompiler_identity_probe.rmeta"
        });
        fs::hard_link(&blob, &output).unwrap();
        let identity = hardlink(&blob);
        assert_eq!(hardlink(&output).index, identity.index);
        let mut command = common::isolated_soldr_command();
        if frontend {
            command.args(["--no-cache", "rustc"]);
        } else {
            command.arg(&rustc).env("SOLDR_CACHE_ENABLED", "0");
        }
        command
            .args([
                "--crate-name=compiler_identity_probe",
                "--crate-type=lib",
                "--emit=metadata",
                "--edition=2021",
            ])
            .arg(&source);
        if explicit_output {
            command.arg("-o").arg(&output);
        } else {
            command.arg("--out-dir").arg(&dir);
        }
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "explicit_output={explicit_output}, frontend={frontend}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let cached = hardlink(&blob);
        assert_eq!(cached.index, identity.index);
        assert_eq!(cached.dev, identity.dev);
        assert_eq!(permissions::mode(&blob).unwrap() & 0o777, 0o444);
        assert_eq!(cached.links, 1);
        assert_eq!(fs::read(&blob).unwrap(), original);
        let compiled = hardlink(&output);
        assert_ne!(compiled.index, cached.index);
        assert_ne!(permissions::mode(&output).unwrap() & 0o200, 0);
        assert_ne!(fs::read(&output).unwrap(), original);
    }
}

#[test]
fn explicit_output_detachment_preserves_unrelated_readonly_files() {
    if !unix_ownership_available() {
        return;
    }
    for emit in ["metadata", "metadata,link", "metadata=kind.rmeta"] {
        let dir = common::unique_temp_dir("direct-output-sentinel");
        let products = dir.join("products");
        fs::create_dir_all(&products).unwrap();
        let source = dir.join("probe.rs");
        fs::write(&source, "pub fn value() -> u32 { 42 }\n").unwrap();
        let requested = products.join("requested.rmeta");
        let unrelated = if emit == "metadata" {
            dir.join("libprobe.rmeta")
        } else {
            requested.clone()
        };
        fs::write(&unrelated, b"unrelated protected content").unwrap();
        permissions::restore_mode(&unrelated, Some(0o444)).unwrap();
        let identity = hardlink(&unrelated);
        let original_mode = permissions::mode(&unrelated);
        let result = common::isolated_soldr_command()
            .args([
                "--no-cache",
                "rustc",
                "--crate-name=probe",
                "--crate-type=lib",
                "--edition=2021",
            ])
            .arg(format!("--emit={emit}"))
            .arg(&source)
            .arg("-o")
            .arg(&requested)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{emit}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let after = hardlink(&unrelated);
        assert_eq!(after.index, identity.index, "{emit}");
        assert_eq!(permissions::mode(&unrelated), original_mode, "{emit}");
        assert_eq!(fs::read(unrelated).unwrap(), b"unrelated protected content");
    }
}

#[test]
fn duplicate_emit_kinds_preserve_superseded_readonly_paths() {
    if !unix_ownership_available() {
        return;
    }
    for (emits, stale, product) in [
        (
            &["--emit=metadata=old.rmeta,metadata"][..],
            "old.rmeta",
            "libprobe.rmeta",
        ),
        (
            &["--emit=metadata=old.rmeta", "--emit=metadata"][..],
            "old.rmeta",
            "libprobe.rmeta",
        ),
        (
            &["--emit=metadata=old.rmeta,metadata=new.rmeta"][..],
            "old.rmeta",
            "new.rmeta",
        ),
        (
            &["--emit=link=old.rlib,link"][..],
            "old.rlib",
            "libprobe.rlib",
        ),
        (
            &["--emit=link=old.rlib", "--emit=link"][..],
            "old.rlib",
            "libprobe.rlib",
        ),
    ] {
        let dir = common::unique_temp_dir("superseded-emit-sentinel");
        let source = dir.join("probe.rs");
        fs::write(&source, "pub fn value() -> u32 { 42 }\n").unwrap();
        let sentinel = dir.join(stale);
        fs::write(&sentinel, b"superseded output must stay untouched").unwrap();
        permissions::restore_mode(&sentinel, Some(0o444)).unwrap();
        let before = hardlink(&sentinel);
        let mode = permissions::mode(&sentinel);
        let result = common::isolated_soldr_command()
            .args([
                "--no-cache",
                "rustc",
                "--crate-name=probe",
                "--crate-type=lib",
                "--edition=2021",
            ])
            .args(emits)
            .arg(&source)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{emits:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(hardlink(&sentinel), before, "{emits:?}");
        assert_eq!(permissions::mode(&sentinel), mode, "{emits:?}");
        assert_eq!(
            fs::read(&sentinel).unwrap(),
            b"superseded output must stay untouched"
        );
        assert!(dir.join(product).metadata().unwrap().len() > 0, "{emits:?}");
    }
}

#[test]
fn distinct_emit_flags_keep_both_declared_outputs() {
    if !unix_ownership_available() {
        return;
    }
    let dir = common::unique_temp_dir("distinct-emit-outputs");
    let source = dir.join("probe.rs");
    fs::write(&source, "pub fn value() -> u32 { 42 }\n").unwrap();
    let blob = dir.join("immutable-blob");
    fs::write(&blob, b"original cached metadata").unwrap();
    permissions::restore_mode(&blob, Some(0o444)).unwrap();
    let output = dir.join("old.rmeta");
    fs::hard_link(&blob, &output).unwrap();
    let before = hardlink(&blob);
    let result = common::isolated_soldr_command()
        .args([
            "--no-cache",
            "rustc",
            "--crate-name=probe",
            "--crate-type=lib",
            "--edition=2021",
            "--emit=metadata=old.rmeta",
            "--emit=obj",
        ])
        .arg(&source)
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let after = hardlink(&blob);
    assert_eq!(
        (after.dev, after.index, after.links),
        (before.dev, before.index, 1)
    );
    assert_eq!(permissions::mode(&blob).unwrap() & 0o777, 0o444);
    assert_eq!(fs::read(&blob).unwrap(), b"original cached metadata");
    assert_ne!(hardlink(&output).index, after.index);
    assert_ne!(permissions::mode(&output).unwrap() & 0o200, 0);
    assert!(dir.join("probe.o").metadata().unwrap().len() > 0);
}
