//! Standalone `soldr cc` / `soldr c++` for the Apple and MSVC targets
//! (soldr#3806), exercised from a Linux x86_64 host.
//!
//! Network-touching like `cli_cc`: the managed LLVM, Apple SDK and xwin
//! bundles are fetched on first use. Opt in with
//! `SOLDR_TEST_NETWORK=1 soldr cargo nextest run -p soldr-cli --test fetch_tools --run-ignored all -E 'test(/^cli_cc_cross::/)'`.

use crate::common;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    MachO { cputype: u32 },
    Pe { machine: u16 },
}

struct Case {
    target: &'static str,
    format: Format,
}

const MACHO_ARM64: u32 = 0x0100_000c;
const MACHO_X86_64: u32 = 0x0100_0007;
const PE_X64: u16 = 0x8664;
const PE_ARM64: u16 = 0xaa64;

const CASES: &[Case] = &[
    Case {
        target: "aarch64-apple-darwin",
        format: Format::MachO {
            cputype: MACHO_ARM64,
        },
    },
    Case {
        target: "x86_64-apple-darwin",
        format: Format::MachO {
            cputype: MACHO_X86_64,
        },
    },
    Case {
        target: "x86_64-pc-windows-msvc",
        format: Format::Pe { machine: PE_X64 },
    },
    Case {
        target: "aarch64-pc-windows-msvc",
        format: Format::Pe { machine: PE_ARM64 },
    },
];

fn enabled() -> bool {
    if std::env::var_os("SOLDR_TEST_NETWORK").is_none() {
        eprintln!("skipping: SOLDR_TEST_NETWORK not set");
        return false;
    }
    if !matches!(
        (
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::arch()
        ),
        (
            soldr_platform::host::facts::HostOs::Linux,
            soldr_platform::host::facts::HostArch::X86_64
        )
    ) {
        eprintln!("skipping cross compiler acceptance test outside x86_64 Linux");
        return false;
    }
    true
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn soldr_cc(verb: &str, target: &str, args: &[&str]) -> Output {
    Command::new(common::soldr_bin())
        .args([verb, "--target", target])
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("run soldr {verb} --target {target}: {error}"))
}

fn print_path(flag: &str, target: &str) -> PathBuf {
    let output = soldr_cc("cc", target, &[flag]);
    assert_success(&format!("soldr cc --target {target} {flag}"), &output);
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.trim();
    assert!(
        !line.is_empty() && !line.contains('\n'),
        "{flag} must print exactly one path, got {text:?}"
    );
    PathBuf::from(line)
}

fn u32_le(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn assert_format(path: &Path, format: Format) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    match format {
        Format::MachO { cputype } => {
            assert_eq!(&bytes[..4], &[0xcf, 0xfa, 0xed, 0xfe], "not a 64-bit Mach-O");
            assert_eq!(u32_le(&bytes, 4), cputype, "wrong Mach-O cputype");
        }
        Format::Pe { machine } => {
            assert_eq!(&bytes[..2], b"MZ", "not a PE file");
            let pe = u32_le(&bytes, 0x3c) as usize;
            assert_eq!(&bytes[pe..pe + 4], b"PE\0\0", "missing PE signature");
            let found = u16::from_le_bytes(bytes[pe + 4..pe + 6].try_into().unwrap());
            assert_eq!(found, machine, "wrong PE machine");
        }
    }
}

fn is_apple(target: &str) -> bool {
    target.ends_with("-apple-darwin")
}

fn run_case(case: &Case) {
    let target = case.target;
    for flag in [
        "--print-cc",
        "--print-cxx",
        "--print-ar",
        "--print-linker",
    ] {
        let path = print_path(flag, target);
        assert!(path.is_file(), "{flag} for {target} printed {path:?}, not a file");
    }
    let sysroot = print_path("--print-sysroot", target);
    assert!(
        sysroot.is_dir(),
        "--print-sysroot for {target} printed {sysroot:?}, not a directory"
    );

    let fixture = common::workspace_root().join("ci/fixtures/soldr-cc-cmake");
    let hello = fixture.join("hello.c");
    let temp = tempfile::tempdir().expect("tempdir");
    let object = temp.path().join("hello.o");

    let compile = soldr_cc(
        "cc",
        target,
        &[
            "-c",
            hello.to_str().unwrap(),
            "-Werror",
            "-o",
            object.to_str().unwrap(),
        ],
    );
    assert_success(&format!("{target}: compile hello.c"), &compile);
    assert!(object.is_file(), "{target}: no object produced");

    // The printed compiler must work on its own: it carries the target and
    // sysroot flags (this is what CMAKE_C_COMPILER receives).
    let printed = print_path("--print-cc", target);
    let direct = Command::new(&printed)
        .args(["-c", hello.to_str().unwrap(), "-o"])
        .arg(temp.path().join("direct.o"))
        .output()
        .expect("run printed compiler");
    assert_success(&format!("{target}: printed compiler compile"), &direct);

    let cxx_src = temp.path().join("hello.cpp");
    std::fs::write(
        &cxx_src,
        "#include <cstdio>\nint main() { std::puts(\"hi\"); return 0; }\n",
    )
    .unwrap();
    let cxx = soldr_cc(
        "c++",
        target,
        &["-c", cxx_src.to_str().unwrap(), "-o", temp.path().join("c.o").to_str().unwrap()],
    );
    assert_success(&format!("{target}: compile C++"), &cxx);

    if is_apple(target) {
        let objc = temp.path().join("hello.m");
        std::fs::write(
            &objc,
            "#import <Foundation/Foundation.h>\n\
             int main(void) { @autoreleasepool { NSLog(@\"hi\"); } return 0; }\n",
        )
        .unwrap();
        let out = soldr_cc(
            "cc",
            target,
            &[
                "-x",
                "objective-c",
                "-c",
                objc.to_str().unwrap(),
                "-o",
                temp.path().join("objc.o").to_str().unwrap(),
            ],
        );
        assert_success(&format!("{target}: compile Objective-C"), &out);
    }

    let exe_name = if is_apple(target) { "hello" } else { "hello.exe" };
    let exe = temp.path().join(exe_name);
    let link = soldr_cc(
        "cc",
        target,
        &[hello.to_str().unwrap(), "-o", exe.to_str().unwrap()],
    );
    assert_success(&format!("{target}: link hello"), &link);
    assert_format(&exe, case.format);
}

fn case(target: &str) -> &'static Case {
    CASES.iter().find(|c| c.target == target).unwrap()
}

macro_rules! cross_case {
    ($name:ident, $target:literal) => {
        #[test]
        #[ignore = "network-touching; opt in with SOLDR_TEST_NETWORK=1 and --run-ignored all"]
        fn $name() {
            if enabled() {
                run_case(case($target));
            }
        }
    };
}

cross_case!(aarch64_apple_darwin, "aarch64-apple-darwin");
cross_case!(x86_64_apple_darwin, "x86_64-apple-darwin");
cross_case!(x86_64_pc_windows_msvc, "x86_64-pc-windows-msvc");
cross_case!(aarch64_pc_windows_msvc, "aarch64-pc-windows-msvc");

/// `soldr toolchain ensure --target T --json` provisions the same tools and
/// reports them, additively, under `build_tools.cross`.
#[test]
#[ignore = "network-touching; opt in with SOLDR_TEST_NETWORK=1 and --run-ignored all"]
fn toolchain_ensure_target_reports_cross_tools() {
    if !enabled() {
        return;
    }
    for target in ["aarch64-apple-darwin", "x86_64-pc-windows-msvc"] {
        let temp = tempfile::tempdir().expect("tempdir");
        let output = Command::new(common::soldr_bin())
            .args(["toolchain", "ensure", "--target", target, "--json"])
            .current_dir(temp.path())
            .output()
            .expect("run soldr toolchain ensure");
        assert_success(&format!("toolchain ensure --target {target}"), &output);
        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("ensure --json is JSON");
        assert_eq!(json["schema_version"], 1);
        let cross = &json["build_tools"]["cross"];
        assert_eq!(cross["target"], target, "payload: {json}");
        for key in ["bin_dir", "sysroot", "cc", "cxx", "ar", "linker"] {
            let value = cross[key].as_str().unwrap_or_else(|| {
                panic!("build_tools.cross.{key} missing for {target}: {json}")
            });
            assert!(Path::new(value).exists(), "{key} {value} does not exist");
        }
        // The reported paths are the ones `soldr cc` prints.
        assert_eq!(
            Path::new(cross["cc"].as_str().unwrap()),
            print_path("--print-cc", target)
        );
        assert_eq!(
            Path::new(cross["sysroot"].as_str().unwrap()),
            print_path("--print-sysroot", target)
        );
    }
}

/// `soldr toolchain prepare --target T` provisions without cargo; afterwards
/// `soldr cc --print-cc` is filesystem-only.
#[test]
#[ignore = "network-touching; opt in with SOLDR_TEST_NETWORK=1 and --run-ignored all"]
fn toolchain_prepare_target_provisions_cross_tools() {
    if !enabled() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let output = Command::new(common::soldr_bin())
        .args(["toolchain", "prepare", "--target", "x86_64-apple-darwin"])
        .current_dir(temp.path())
        .output()
        .expect("run soldr toolchain prepare");
    assert_success("toolchain prepare --target", &output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("x86_64-apple-darwin") && stderr.contains("sysroot"),
        "prepare must report the target's tools, got:\n{stderr}"
    );
}
