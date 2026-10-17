use super::{parse_rustc_version, rustc_supplies_lld};

const LINUX: &str = "x86_64-unknown-linux-gnu";

fn holds(target: &str, host: &str, version: Option<(u32, u32)>, flags: &[&str]) -> bool {
    let flags: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
    rustc_supplies_lld(target, host, version, &flags)
}

#[test]
fn version_threshold_is_1_90() {
    assert!(!holds(LINUX, LINUX, Some((1, 89)), &[]));
    assert!(holds(LINUX, LINUX, Some((1, 90)), &[]));
    assert!(holds(LINUX, LINUX, Some((1, 98)), &[]));
    assert!(holds(LINUX, LINUX, Some((2, 0)), &[]));
    assert!(!holds(LINUX, LINUX, None, &[]), "unknown version is doubt");
}

#[test]
fn only_native_x86_64_linux_gnu_qualifies() {
    let v = Some((1, 98));
    assert!(!holds(LINUX, "aarch64-unknown-linux-gnu", v, &[]));
    assert!(!holds(
        "aarch64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        v,
        &[]
    ));
    assert!(
        !holds("aarch64-unknown-linux-gnu", LINUX, v, &[]),
        "cross target"
    );
    assert!(!holds("x86_64-pc-windows-msvc", LINUX, v, &[]));
    assert!(!holds(
        "x86_64-unknown-linux-musl",
        "x86_64-unknown-linux-musl",
        v,
        &[]
    ));
}

#[test]
fn every_opt_out_spelling_disables_the_shortcut() {
    let v = Some((1, 98));
    for flags in [
        "-C link-self-contained=no",
        "-Clink-self-contained=no",
        "-C link-self-contained=-linker",
        "-Clink-self-contained=-linker,-crt",
        "--codegen link-self-contained=no",
        "--codegen=link-self-contained=no",
        "-C linker-features=-lld",
        "-Clinker-features=-lld",
        "-Zlinker-features=-lld",
        "-Z linker-features=-lld",
        "-C opt-level=3 -Clink-self-contained=no",
        "-Copt-level=3\x1f-Clink-self-contained=no",
    ] {
        assert!(!holds(LINUX, LINUX, v, &[flags]), "{flags:?} must opt out");
    }
    // Unrelated flags and the benign spellings keep it on.
    for flags in [
        "",
        "-C opt-level=3 -C target-cpu=native",
        "-C link-self-contained=yes",
        "-Clinker-features=+lld",
        "-C link-arg=-fuse-ld=lld",
    ] {
        assert!(
            holds(LINUX, LINUX, v, &[flags]),
            "{flags:?} must not opt out"
        );
    }
}

#[test]
fn any_one_flag_source_can_opt_out() {
    let v = Some((1, 98));
    assert!(!holds(
        LINUX,
        LINUX,
        v,
        &["-C opt-level=3", "-Clink-self-contained=no"]
    ));
}

#[test]
fn parses_rustc_version_lines() {
    assert_eq!(
        parse_rustc_version("rustc 1.98.1 (abc 2026-06-01)"),
        Some((1, 98))
    );
    assert_eq!(
        parse_rustc_version("rustc 1.90.0-nightly (abc 2026-05-28)"),
        Some((1, 90))
    );
    assert_eq!(parse_rustc_version("1.89.0"), Some((1, 89)));
    assert_eq!(parse_rustc_version("garbage"), None);
    assert_eq!(parse_rustc_version(""), None);
}

// -- cargo config files and --config arguments (conservative scan) ----------

mod config_scan {
    use super::super::config_mentions_lld_policy_for_tests as mentions;

    fn in_dir(dir: &std::path::Path, f: impl FnOnce()) {
        let _lock = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _home = crate::EnvVarGuard::set("CARGO_HOME", dir.join("no-cargo-home"));
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        f();
        std::env::set_current_dir(previous).unwrap();
    }

    fn write_config(dir: &std::path::Path, text: &str) {
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        std::fs::write(dir.join(".cargo/config.toml"), text).unwrap();
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_parent_config_with_link_self_contained_is_doubt() {
        let temp = tempfile::tempdir().unwrap();
        write_config(
            temp.path(),
            "[build]\nrustflags = [\"-C\", \"link-self-contained=no\"]\n",
        );
        let child = temp.path().join("a/b");
        std::fs::create_dir_all(&child).unwrap();
        in_dir(&child, || assert!(mentions(&[])));
    }

    #[test]
    fn linker_features_in_a_config_is_doubt() {
        let temp = tempfile::tempdir().unwrap();
        write_config(
            temp.path(),
            "[build]\nrustflags = [\"-Clinker-features=-lld\"]\n",
        );
        in_dir(temp.path(), || assert!(mentions(&[])));
    }

    #[test]
    fn a_config_argument_is_doubt() {
        let temp = tempfile::tempdir().unwrap();
        in_dir(temp.path(), || {
            assert!(mentions(&args(&[
                "build",
                "--config",
                "build.rustflags=['-Clink-self-contained=no']"
            ])));
            assert!(mentions(&args(&[
                "build",
                "--config=build.rustflags=['-Zlinker-features=-lld']"
            ])));
            assert!(!mentions(&args(&["build", "--config", "build.jobs=2"])));
        });
    }

    #[test]
    fn a_manifest_path_directory_config_is_doubt() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("proj");
        write_config(
            &project,
            "[build]\nrustflags = [\"-Clink-self-contained=no\"]\n",
        );
        let other = temp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        in_dir(&other, || {
            let manifest = project.join("Cargo.toml").to_string_lossy().into_owned();
            assert!(mentions(&[
                "build".into(),
                "--manifest-path".into(),
                manifest
            ]));
            assert!(!mentions(&args(&["build"])));
        });
    }

    #[test]
    fn a_benign_config_leaves_the_shortcut_on() {
        let temp = tempfile::tempdir().unwrap();
        write_config(
            temp.path(),
            "[build]\njobs = 4\nrustflags = [\"-C\", \"opt-level=2\"]\n",
        );
        in_dir(temp.path(), || assert!(!mentions(&[])));
    }
}
