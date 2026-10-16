//! soldr#3801 PR 2: which links are rewritten to the system `cc`, what is
//! stripped from them, and that the result never depends on the fetch.

use super::*;
use crate::core::SoldrPaths;
use std::time::Duration;

const SHIM: &str = "clang-0123456789abcdef0123";
const CC: &str = "/run/current-system/sw/bin/cc";

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

fn cc() -> Option<PathBuf> {
    Some(PathBuf::from(CC))
}

fn unit(name: &str, crate_type: &str, linker: &str) -> Vec<String> {
    args(&[
        "rustc",
        "--crate-name",
        name,
        "--crate-type",
        crate_type,
        "--emit=dep-info,link",
        "-C",
        &format!("linker={linker}"),
        "-C",
        "link-arg=--ld-path=reld",
        "-Clink-arg=-fuse-ld=lld",
        "-C",
        "link-arg=-Wl,--as-needed",
    ])
}

#[test]
fn the_rewrite_table() {
    let cases: Vec<(&str, Vec<String>, Option<PathBuf>, bool)> = vec![
        (
            "build script",
            unit("build_script_build", "bin", SHIM),
            cc(),
            true,
        ),
        (
            "proc-macro",
            unit("serde_derive", "proc-macro", SHIM),
            cc(),
            true,
        ),
        ("final bin", unit("app", "bin", SHIM), cc(), false),
        ("final cdylib", unit("lib", "cdylib", SHIM), cc(), false),
        (
            "build script, non-managed linker",
            unit("build_script_build", "bin", "/usr/bin/gcc"),
            cc(),
            false,
        ),
        (
            "proc-macro, host cc shim",
            unit("m", "proc-macro", "cc-0123456789abcdef0123"),
            cc(),
            false,
        ),
        (
            "build script, no system cc",
            unit("build_script_build", "bin", SHIM),
            None,
            false,
        ),
        (
            "check-mode proc-macro never links",
            args(&[
                "rustc",
                "--crate-type=proc-macro",
                "--emit=metadata",
                "-Clinker=clang-0123456789abcdef0123",
            ]),
            cc(),
            false,
        ),
    ];
    for (name, argv, cc, rewritten) in cases {
        assert_eq!(
            rewrite_host_tool_link(&argv, cc.as_deref()).is_some(),
            rewritten,
            "{name}"
        );
    }
}

#[test]
fn a_rewritten_unit_links_through_cc_without_clang_driver_args() {
    let out =
        rewrite_host_tool_link(&unit("build_script_build", "bin", SHIM), cc().as_deref()).unwrap();
    assert_eq!(
        out,
        args(&[
            "rustc",
            "--crate-name",
            "build_script_build",
            "--crate-type",
            "bin",
            "--emit=dep-info,link",
            "-C",
            &format!("linker={CC}"),
            "-C",
            "link-arg=-Wl,--as-needed",
        ])
    );
    let spelled = args(&[
        "rustc",
        "--crate-name=build_script_build",
        "-Clinker=clang-0123456789abcdef0123",
        "--codegen=link-arg=--ld-path=reld",
        "--codegen",
        "link-args=-fuse-ld=lld -static",
    ]);
    assert_eq!(
        rewrite_host_tool_link(&spelled, cc().as_deref()).unwrap(),
        args(&[
            "rustc",
            "--crate-name=build_script_build",
            &format!("-Clinker={CC}"),
            "--codegen",
            "link-args=-static",
        ])
    );
}

#[test]
fn incompatible_args_are_stripped_only_on_rewritten_units() {
    // Not rewritten: the original argv is untouched, driver args and all.
    for argv in [
        unit("app", "bin", SHIM),
        unit("build_script_build", "bin", "/usr/bin/gcc"),
    ] {
        assert!(rewrite_host_tool_link(&argv, cc().as_deref()).is_none());
        assert!(argv.iter().any(|a| a.contains("--ld-path=")));
    }
    assert!(rewrite_host_tool_link(&unit("app", "bin", SHIM), None).is_none());
}

#[test]
fn the_rewritten_argv_is_independent_of_the_fetch_and_never_waits() {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    let pending = temp.path().join("pending");
    let complete = temp.path().join("complete");
    std::fs::create_dir_all(&complete).unwrap();
    std::fs::write(complete.join(".complete"), "").unwrap();
    for (name, crate_type) in [("build_script_build", "bin"), ("derive", "proc-macro")] {
        let original = unit(name, crate_type, SHIM);
        let keyed: Vec<Vec<String>> = [&pending, &complete]
            .iter()
            .map(|dir| {
                // This is the argv the compiler (and so zccache's linker key) sees.
                let rewritten = rewrite_host_tool_link(&original, cc().as_deref()).unwrap();
                crate::llvm_fetch_wait::gate_with(
                    &rewritten,
                    dir,
                    Some(&paths),
                    Some(7),
                    Duration::from_secs(1),
                    |_, _, _| panic!("a host-tool link must never wait"),
                )
                .unwrap();
                rewritten
            })
            .collect();
        assert_eq!(keyed[0], keyed[1], "{name}");
        assert_eq!(crate::llvm_fetch_wait::linker_arg(&keyed[0]), Some(CC));
    }
}

#[test]
fn a_final_link_still_waits() {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    let waited = std::cell::Cell::new(false);
    let argv = unit("app", "bin", SHIM);
    assert!(rewrite_host_tool_link(&argv, cc().as_deref()).is_none());
    crate::llvm_fetch_wait::gate_with(
        &argv,
        &temp.path().join("pending"),
        Some(&paths),
        Some(8),
        Duration::from_secs(1),
        |_, _, _| {
            waited.set(true);
            Ok(())
        },
    )
    .unwrap();
    assert!(waited.get());
}

#[test]
fn cc_is_found_on_the_given_path_only() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let path = std::env::join_paths([temp.path().join("none"), dir.clone()]).unwrap();
    assert_eq!(find_cc(Some(path.as_os_str())), None);
    let file = dir.join(format!("cc{}", std::env::consts::EXE_SUFFIX));
    std::fs::write(&file, "").unwrap();
    assert_eq!(find_cc(Some(path.as_os_str())), Some(file));
    assert_eq!(find_cc(None), None);
}
