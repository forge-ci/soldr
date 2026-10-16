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
