//! soldr#3801: which compiles wait for the background LLVM fetch, and how a
//! wait that fails is reported.

use super::*;
use std::cell::Cell;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

const SHIM: &str = "clang-0123456789abcdef0123";

#[test]
fn the_link_predicate_table() {
    let cases: &[(&str, &[&str], bool)] = &[
        (
            "bin with the default emit",
            &["rustc", "--crate-name", "app", "--crate-type", "bin"],
            true,
        ),
        (
            "no crate type is a bin",
            &["rustc", "--crate-name", "app", "--emit=dep-info,link"],
            true,
        ),
        ("no emit at all", &["rustc", "--crate-type=cdylib"], true),
        (
            "cdylib",
            &["rustc", "--crate-type", "cdylib", "--emit=dep-info,link"],
            true,
        ),
        (
            "dylib",
            &["rustc", "--crate-type=dylib", "--emit", "link"],
            true,
        ),
        (
            "proc-macro",
            &[
                "rustc",
                "--crate-type",
                "proc-macro",
                "--emit=dep-info,link",
            ],
            true,
        ),
        (
            "build script",
            &[
                "rustc",
                "--crate-name",
                "build_script_build",
                "--crate-type",
                "bin",
                "--emit=dep-info,link",
            ],
            true,
        ),
        (
            "unit-test harness",
            &[
                "rustc",
                "--crate-name",
                "x",
                "--test",
                "--emit=dep-info,link",
            ],
            true,
        ),
        (
            "lib with --test still links the harness",
            &["rustc", "--crate-type=lib", "--test", "--emit=link"],
            true,
        ),
    ];
    assert_table(cases);
}

#[test]
fn the_link_predicate_table_non_linking_and_mixed() {
    let cases: &[(&str, &[&str], bool)] = &[
        (
            "metadata only (cargo check) never links",
            &["rustc", "--crate-type", "bin", "--emit=dep-info,metadata"],
            false,
        ),
        (
            "metadata-only proc-macro (cargo check)",
            &["rustc", "--crate-type=proc-macro", "--emit", "metadata"],
            false,
        ),
        (
            "metadata with a path value",
            &["rustc", "--crate-type=bin", "--emit=metadata=/t/x.rmeta"],
            false,
        ),
        (
            "link with a path value",
            &["rustc", "--crate-type=bin", "--emit=link=/t/app"],
            true,
        ),
        (
            "staticlib invokes no linker",
            &["rustc", "--crate-type", "staticlib", "--emit=dep-info,link"],
            false,
        ),
        (
            "rlib",
            &[
                "rustc",
                "--crate-type=rlib",
                "--emit=dep-info,metadata,link",
            ],
            false,
        ),
        (
            "lib",
            &["rustc", "--crate-type", "lib", "--emit=link"],
            false,
        ),
        (
            "rlib plus bin links",
            &["rustc", "--crate-type=rlib,bin", "--emit=link"],
            true,
        ),
    ];
    assert_table(cases);
}

fn assert_table(cases: &[(&str, &[&str], bool)]) {
    for (name, argv, expected) in cases {
        assert_eq!(invocation_links(&args(argv)), *expected, "{name}");
    }
}

#[test]
fn only_the_managed_clang_shim_is_a_managed_linker() {
    assert!(names_managed_shim(SHIM));
    assert!(names_managed_shim(&format!(
        "/cache/bin/linker-shims/v1/{SHIM}"
    )));
    for other in [
        "cc-0123456789abcdef0123",
        "clang",
        "clang-short",
        "clang-zzzzzzzzzzzzzzzzzzzz",
        "dylint-link",
        "/usr/bin/gcc",
        "",
    ] {
        assert!(!names_managed_shim(other), "{other:?}");
    }
}

#[test]
fn the_linker_flag_is_read_in_every_spelling_and_the_last_one_wins() {
    for spelling in [
        vec!["rustc", "-C", "linker=X"],
        vec!["rustc", "-Clinker=X"],
        vec!["rustc", "--codegen", "linker=X"],
        vec!["rustc", "--codegen=linker=X"],
    ] {
        assert_eq!(linker_arg(&args(&spelling)), Some("X"), "{spelling:?}");
    }
    assert_eq!(
        linker_arg(&args(&["rustc", "-Clinker=A", "-C", "linker=B"])),
        Some("B")
    );
    assert_eq!(linker_arg(&args(&["rustc", "-C", "opt-level=3"])), None);
}

fn linking(linker: &str) -> Vec<String> {
    args(&[
        "rustc",
        "--crate-type",
        "bin",
        "--emit=dep-info,link",
        &format!("-Clinker={linker}"),
    ])
}

fn fixture() -> (tempfile::TempDir, SoldrPaths, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    let install_dir = temp.path().join("llvm");
    (temp, paths, install_dir)
}

const T: Duration = Duration::from_secs(1);

#[test]
fn non_linking_compiles_never_wait() {
    let (_t, paths, install_dir) = fixture();
    let waited = Cell::new(false);
    let wait = |_: &SoldrPaths, _: &Path, _: Duration| {
        waited.set(true);
        Err("must not be asked".to_string())
    };
    for argv in [
        args(&[
            "rustc",
            "--crate-type=bin",
            "--emit=metadata",
            &format!("-Clinker={SHIM}"),
        ]),
        args(&[
            "rustc",
            "--crate-type=staticlib",
            "--emit=link",
            &format!("-Clinker={SHIM}"),
        ]),
        args(&["rustc", "--crate-type=rlib", &format!("-Clinker={SHIM}")]),
        // Links, but through a linker that is not the managed shim.
        linking("cc-0123456789abcdef0123"),
        linking("dylint-link"),
        args(&["rustc", "--crate-type=bin", "--emit=link"]),
    ] {
        gate_with(&argv, &install_dir, Some(&paths), Some(1), T, wait).unwrap();
    }
    assert!(!waited.get());
}

#[test]
fn a_linking_compile_through_the_shim_waits_and_proceeds_when_the_fetch_lands() {
    let (_t, paths, install_dir) = fixture();
    let waited = Cell::new(0);
    gate_with(
        &linking(SHIM),
        &install_dir,
        Some(&paths),
        Some(1),
        T,
        |_, dir, _| {
            assert_eq!(dir, install_dir);
            waited.set(waited.get() + 1);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(waited.get(), 1);
}

#[test]
fn a_complete_install_is_not_waited_on() {
    let (_t, paths, install_dir) = fixture();
    std::fs::create_dir_all(&install_dir).unwrap();
    std::fs::write(install_dir.join(".complete"), "x").unwrap();
    gate_with(
        &linking(SHIM),
        &install_dir,
        Some(&paths),
        Some(1),
        T,
        |_, _, _| panic!("a stamped install needs no wait"),
    )
    .unwrap();
}

#[test]
fn a_failed_fetch_fails_the_first_linking_unit_clearly_and_the_rest_fast() {
    let (_t, paths, install_dir) = fixture();
    let waits = Cell::new(0);
    let first = gate_with(
        &linking(SHIM),
        &install_dir,
        Some(&paths),
        Some(9),
        T,
        |_, _, _| {
            waits.set(waits.get() + 1);
            Err("catalogue offline".to_string())
        },
    )
    .unwrap_err();
    assert!(
        first.contains("catalogue offline") && first.contains("clang"),
        "{first}"
    );
    // N more linking units of the same build: the recorded failure, no retry.
    for _ in 0..5 {
        let again = gate_with(
            &linking(SHIM),
            &install_dir,
            Some(&paths),
            Some(9),
            T,
            |_, _, _| {
                waits.set(waits.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(again, first, "the same message");
    }
    assert_eq!(waits.get(), 1, "only the first unit tried the fetch");
    // Another build is unaffected.
    gate_with(
        &linking(SHIM),
        &install_dir,
        Some(&paths),
        Some(10),
        T,
        |_, _, _| Ok(()),
    )
    .unwrap();
    // Non-linking compiles of the failed build still proceed.
    gate_with(
        &args(&["rustc", "--crate-type=rlib", &format!("-Clinker={SHIM}")]),
        &install_dir,
        Some(&paths),
        Some(9),
        T,
        |_, _, _| panic!("no wait"),
    )
    .unwrap();
}

#[test]
fn a_timed_out_wait_is_reported_and_recorded_like_any_failure() {
    let (_t, paths, install_dir) = fixture();
    let error = gate_with(
        &linking(SHIM),
        &install_dir,
        Some(&paths),
        Some(3),
        T,
        |_, _, _| Err("timed out after 1s waiting for the managed LLVM fetch".to_string()),
    )
    .unwrap_err();
    assert!(error.contains("timed out"), "{error}");
    assert!(recorded_failure(&paths, Some(3)).is_some());
}

#[test]
fn without_a_build_id_each_unit_reports_for_itself() {
    let (_t, paths, install_dir) = fixture();
    let waits = Cell::new(0);
    for _ in 0..2 {
        let _ = gate_with(
            &linking(SHIM),
            &install_dir,
            Some(&paths),
            None,
            T,
            |_, _, _| {
                waits.set(waits.get() + 1);
                Err("offline".to_string())
            },
        );
    }
    assert_eq!(waits.get(), 2);
}
