//! Native Unix ownership transition: direct rustc must replace protected cache
//! hardlinks with private writable outputs without changing the cached inode.

use crate::common;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[test]
fn no_cache_rustc_detaches_protected_metadata_outputs() {
    let rustc = common::rustup_which("rustc");
    for (explicit_output, frontend) in [(false, false), (true, false), (false, true), (true, true)]
    {
        let dir = common::unique_temp_dir("direct-output-detach");
        let source = dir.join("probe.rs");
        fs::write(&source, "pub fn value() -> u32 { 42 }\n").unwrap();
        let blob = dir.join("immutable-cache-blob");
        let original = b"protected cached metadata";
        fs::write(&blob, original).unwrap();
        fs::set_permissions(&blob, fs::Permissions::from_mode(0o444)).unwrap();
        let output = dir.join(if explicit_output {
            "requested.rmeta"
        } else {
            "libcompiler_identity_probe.rmeta"
        });
        fs::hard_link(&blob, &output).unwrap();
        let identity = fs::metadata(&blob).unwrap();
        assert_eq!(fs::metadata(&output).unwrap().ino(), identity.ino());
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
        let cached = fs::metadata(&blob).unwrap();
        assert_eq!(cached.ino(), identity.ino());
        assert_eq!(cached.dev(), identity.dev());
        assert_eq!(cached.permissions().mode() & 0o777, 0o444);
        assert_eq!(cached.nlink(), 1);
        assert_eq!(fs::read(&blob).unwrap(), original);
        let compiled = fs::metadata(&output).unwrap();
        assert_ne!(compiled.ino(), cached.ino());
        assert_ne!(compiled.permissions().mode() & 0o200, 0);
        assert_ne!(fs::read(&output).unwrap(), original);
    }
}

#[test]
fn explicit_output_detachment_preserves_unrelated_readonly_files() {
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
        fs::set_permissions(&unrelated, fs::Permissions::from_mode(0o444)).unwrap();
        let identity = fs::metadata(&unrelated).unwrap();
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
        let after = fs::metadata(&unrelated).unwrap();
        assert_eq!(after.ino(), identity.ino(), "{emit}");
        assert_eq!(
            after.permissions().mode(),
            identity.permissions().mode(),
            "{emit}"
        );
        assert_eq!(fs::read(unrelated).unwrap(), b"unrelated protected content");
    }
}
