//! Acceptance coverage for soldr#3806 beyond `cli_cc_cross` (which covers the
//! per-target `soldr cc` compile/link matrix): the alias grammar, the
//! `soldr ld` / `soldr link` target linkers, `--print-rc` (soldr#3810), and a
//! CMake cross build driven only by the printed shims.
//!
//! Network-touching (managed LLVM, Apple SDK, xwin, GNU/musl toolchains, reld)
//! and x86_64-Linux-host only, like `cli_cc`.

use crate::common;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn should_run() -> bool {
    if std::env::var_os("SOLDR_TEST_NETWORK").is_none() {
        eprintln!("skipping: SOLDR_TEST_NETWORK not set");
        return false;
    }
    let linux_x64 = matches!(
        (
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::arch()
        ),
        (
            soldr_platform::host::facts::HostOs::Linux,
            soldr_platform::host::facts::HostArch::X86_64
        )
    );
    if !linux_x64 {
        eprintln!("skipping cross compiler acceptance test outside x86_64 Linux");
    }
    linux_x64
}

fn soldr(args: &[&str]) -> Output {
    Command::new(common::soldr_bin())
        .args(args)
        .output()
        .expect("run soldr")
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

fn stdout_path(label: &str, output: &Output) -> PathBuf {
    assert_success(label, output);
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    assert!(
        path.exists(),
        "{label}: printed path {} does not exist",
        path.display()
    );
    path
}

fn hello_c() -> PathBuf {
    common::workspace_root().join("ci/fixtures/soldr-cc-cmake/hello.c")
}

/// Mach-O 64-bit magic + cputype (`CPU_TYPE_X86_64` / `CPU_TYPE_ARM64`).
fn assert_macho(path: &Path, arm64: bool) {
    let bytes = std::fs::read(path).expect("read Mach-O");
    assert_eq!(
        &bytes[..4],
        &[0xcf, 0xfa, 0xed, 0xfe],
        "{}: not Mach-O 64",
        path.display()
    );
    let cputype = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let want = if arm64 { 0x0100_000c } else { 0x0100_0007 };
    assert_eq!(cputype, want, "{}: cputype {cputype:#x}", path.display());
}

/// PE image with the expected `IMAGE_FILE_MACHINE_*`.
fn assert_pe(path: &Path, arm64: bool) {
    let bytes = std::fs::read(path).expect("read PE");
    assert_eq!(&bytes[..2], b"MZ", "{}: not a PE image", path.display());
    let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(
        &bytes[pe..pe + 4],
        b"PE\0\0",
        "{}: bad PE header",
        path.display()
    );
    let machine = u16::from_le_bytes(bytes[pe + 4..pe + 6].try_into().unwrap());
    let want = if arm64 { 0xaa64 } else { 0x8664 };
    assert_eq!(machine, want, "{}: machine {machine:#x}", path.display());
}

fn assert_print_flags(verb: &str, target: &str) {
    for flag in [
        "--print-cc",
        "--print-cxx",
        "--print-ar",
        "--print-linker",
        "--print-sysroot",
    ] {
        stdout_path(
            &format!("soldr {verb} --target {target} {flag}"),
            &soldr(&[verb, "--target", target, flag]),
        );
    }
}

#[test]
#[ignore = "network-touching; opt in with SOLDR_TEST_NETWORK=1 soldr cargo test -p soldr-cli --test fetch_tools -- --ignored cli_ld_link::"]
fn darwin_soldr_ld_links_macho() {
    if !should_run() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let objc = temp.path().join("hello.m");
    std::fs::write(
        &objc,
        "#import <Foundation/Foundation.h>\nint main(void) { @autoreleasepool { NSLog(@\"hi\"); } return 0; }\n",
    )
    .unwrap();
    for (target, arm64) in [("mac-x64", false), ("mac-arm64", true)] {
        assert_print_flags("cc", target);
        let obj = temp.path().join(format!("hello-{target}.o"));
        let compile = soldr(&[
            "cc",
            "--target",
            target,
            "-c",
            "-Werror",
            hello_c().to_str().unwrap(),
            "-o",
            obj.to_str().unwrap(),
        ]);
        assert_success(&format!("{target}: compile -c -Werror"), &compile);

        let objc_obj = temp.path().join(format!("objc-{target}.o"));
        let objc_compile = soldr(&[
            "cc",
            "--target",
            target,
            "-c",
            objc.to_str().unwrap(),
            "-o",
            objc_obj.to_str().unwrap(),
        ]);
        assert_success(
            &format!("{target}: ObjC + Foundation compile"),
            &objc_compile,
        );

        let exe = temp.path().join(format!("hello-{target}"));
        let link = soldr(&[
            "cc",
            "--target",
            target,
            hello_c().to_str().unwrap(),
            "-o",
            exe.to_str().unwrap(),
        ]);
        assert_success(&format!("{target}: link through cc"), &link);
        assert_macho(&exe, arm64);

        let raw = temp.path().join(format!("raw-{target}"));
        let ld = soldr(&[
            "ld",
            "--target",
            target,
            obj.to_str().unwrap(),
            "-lSystem",
            "-o",
            raw.to_str().unwrap(),
        ]);
        assert_success(&format!("{target}: soldr ld"), &ld);
        assert_macho(&raw, arm64);
        stdout_path(
            "soldr ld --print-ld",
            &soldr(&["ld", "--target", target, "--print-ld"]),
        );
    }
}

#[test]
#[ignore = "network-touching; opt in with SOLDR_TEST_NETWORK=1 soldr cargo test -p soldr-cli --test fetch_tools -- --ignored cli_ld_link::"]
fn msvc_soldr_link_links_pe_and_print_rc_runs() {
    if !should_run() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    for (target, arm64) in [("win-x64", false), ("win-arm64", true)] {
        assert_print_flags("cc", target);
        assert_print_flags("c++", target);
        let rc = stdout_path(
            "--print-rc",
            &soldr(&["cc", "--target", target, "--print-rc"]),
        );
        let rc_help = Command::new(&rc).arg("/?").output().expect("run llvm-rc");
        assert!(
            String::from_utf8_lossy(&rc_help.stdout).contains("LLVM Resource Converter"),
            "{}",
            String::from_utf8_lossy(&rc_help.stdout)
        );
        let obj = temp.path().join(format!("hello-{target}.obj"));
        let compile = soldr(&[
            "cc",
            "--target",
            target,
            "-c",
            "-Werror",
            hello_c().to_str().unwrap(),
            "-o",
            obj.to_str().unwrap(),
        ]);
        assert_success(&format!("{target}: compile -c -Werror"), &compile);

        let exe = temp.path().join(format!("hello-{target}.exe"));
        let link = soldr(&[
            "cc",
            "--target",
            target,
            hello_c().to_str().unwrap(),
            "-o",
            exe.to_str().unwrap(),
        ]);
        assert_success(&format!("{target}: link through cc"), &link);
        assert_pe(&exe, arm64);

        let raw = temp.path().join(format!("raw-{target}.exe"));
        let out = format!("/OUT:{}", raw.display());
        let link = soldr(&["link", "--target", target, obj.to_str().unwrap(), &out]);
        assert_success(&format!("{target}: soldr link"), &link);
        assert_pe(&raw, arm64);
        stdout_path(
            "soldr link --print-link",
            &soldr(&["link", "--target", target, "--print-link"]),
        );
    }
}

#[test]
#[ignore = "network-touching; opt in with SOLDR_TEST_NETWORK=1 soldr cargo test -p soldr-cli --test fetch_tools -- --ignored cli_ld_link::"]
fn aliases_reach_the_same_toolchain_as_triples() {
    if !should_run() {
        return;
    }
    for (alias, triple) in [
        ("linux-x64-glibc_2_17", "x86_64-unknown-linux-gnu.2.17"),
        ("win-arm", "aarch64-pc-windows-msvc"),
        ("mac-x64", "x86_64-apple-darwin"),
    ] {
        let a = stdout_path(alias, &soldr(&["cc", "--target", alias, "--print-cc"]));
        let b = stdout_path(triple, &soldr(&["cc", "--target", triple, "--print-cc"]));
        assert_eq!(a, b, "{alias} vs {triple}");
    }
    let bad = soldr(&["cc", "--target", "mac-x64-glibc_2_17", "--print-cc"]);
    assert!(!bad.status.success());
}

#[test]
#[ignore = "network-touching; opt in with SOLDR_TEST_NETWORK=1 soldr cargo test -p soldr-cli --test fetch_tools -- --ignored cli_ld_link::"]
fn soldr_ld_links_and_runs_elf_with_every_linker() {
    if !should_run() {
        return;
    }
    let target = "linux-x64-glibc_2_17";
    let temp = tempfile::tempdir().unwrap();
    let obj = temp.path().join("hello.o");
    assert_success(
        "compile",
        &soldr(&[
            "cc",
            "--target",
            target,
            "-c",
            hello_c().to_str().unwrap(),
            "-o",
            obj.to_str().unwrap(),
        ]),
    );
    let sysroot = stdout_path(
        "sysroot",
        &soldr(&["ld", "--target", target, "--print-sysroot"]),
    );
    let crt = |name: &str| -> String {
        ["usr/lib64", "usr/lib", "lib64", "lib"]
            .iter()
            .map(|dir| sysroot.join(dir).join(name))
            .find(|path| path.is_file())
            .unwrap_or_else(|| panic!("{name} not in {}", sysroot.display()))
            .display()
            .to_string()
    };
    let (crt1, crti, crtn) = (crt("crt1.o"), crt("crti.o"), crt("crtn.o"));
    for fuse in ["lld", "RELD", "ld"] {
        let exe = temp.path().join(format!("hello-{fuse}"));
        let fuse_arg = format!("--fuse={fuse}");
        let link = soldr(&[
            "ld",
            "--target",
            target,
            &fuse_arg,
            "-dynamic-linker",
            "/lib64/ld-linux-x86-64.so.2",
            &crt1,
            &crti,
            obj.to_str().unwrap(),
            "-lc",
            &crtn,
            "-o",
            exe.to_str().unwrap(),
        ]);
        assert_success(&format!("soldr ld {fuse_arg}"), &link);
        let run = Command::new(&exe).output().expect("run linked ELF");
        assert_success(&format!("run {fuse_arg} output"), &run);
        assert_eq!(
            String::from_utf8_lossy(&run.stdout).trim(),
            "hello from soldr cc"
        );
    }
    stdout_path(
        "win-gnu --print-ld",
        &soldr(&["ld", "--target", "win-x64-gnu", "--print-ld"]),
    );
}

#[test]
fn ld_and_link_refuse_the_other_dialect_and_unsupported_linkers() {
    // Offline: these fail before any toolchain is prepared.
    let wrong = soldr(&["ld", "--target", "win-x64", "--print-ld"]);
    assert!(!wrong.status.success());
    assert!(
        String::from_utf8_lossy(&wrong.stderr).contains("use `soldr link --target win-x64`"),
        "{}",
        String::from_utf8_lossy(&wrong.stderr)
    );
    let wrong = soldr(&["link", "--target", "linux-x64", "--print-link"]);
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("use `soldr ld --target linux-x64`"));
    let reld = soldr(&["ld", "--target", "mac-x64", "--fuse=RELD", "--print-ld"]);
    assert!(!reld.status.success());
    assert!(String::from_utf8_lossy(&reld.stderr).contains("--fuse=reld cannot link"));
    // Three dash-separated parts pass through as a custom triple; `ld` then
    // refuses it by name.
    let unknown = soldr(&["ld", "--target", "mac-x64-musl", "--print-ld"]);
    assert!(!unknown.status.success());
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("has no linker for `mac-x64-musl`"),
        "{}",
        String::from_utf8_lossy(&unknown.stderr)
    );
    let bad_alias = soldr(&["ld", "--target", "mac-x64-glibc_2_17", "--print-ld"]);
    assert!(String::from_utf8_lossy(&bad_alias.stderr).contains("soldr ld --target"));
}

#[test]
#[ignore = "network-touching; opt in with SOLDR_TEST_NETWORK=1 soldr cargo test -p soldr-cli --test fetch_tools -- --ignored cli_ld_link::"]
fn cmake_cross_builds_through_the_printed_shims() {
    if !should_run() {
        return;
    }
    let fixture = common::workspace_root().join("ci/fixtures/soldr-cc-cmake");
    let temp = tempfile::tempdir().unwrap();
    for (target, system, exe, check) in [
        (
            "mac-arm64",
            "Darwin",
            "soldr-cc-cmake",
            assert_macho as fn(&Path, bool),
        ),
        (
            "win-x64",
            "Windows",
            "soldr-cc-cmake.exe",
            assert_pe as fn(&Path, bool),
        ),
    ] {
        // Only the compilers: CMake must find llvm-ar / llvm-ranlib beside
        // them (a host GNU ranlib corrupts Mach-O/COFF archives).
        let print = |flag: &str| {
            stdout_path(flag, &soldr(&["cc", "--target", target, flag]))
                .display()
                .to_string()
        };
        let build = temp.path().join(target);
        let mut configure = Command::new("cmake");
        configure
            .args(["-S", fixture.to_str().unwrap(), "-B"])
            .arg(&build)
            .arg(format!("-DCMAKE_SYSTEM_NAME={system}"))
            .arg(format!("-DCMAKE_C_COMPILER={}", print("--print-cc")))
            .arg(format!("-DCMAKE_CXX_COMPILER={}", print("--print-cxx")));
        if system == "Windows" {
            // CMake's Windows-Clang module demands an RC compiler and
            // searches only PATH for it (soldr#3810).
            configure.arg(format!("-DCMAKE_RC_COMPILER={}", print("--print-rc")));
            // The xwin bundle has no debug CRT (msvcrtd.lib), like
            // cargo-xwin; CMake's default Debug runtime needs it.
            configure.arg("-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreadedDLL");
        }
        let configure = configure.output().expect("run CMake configure");
        assert_success(&format!("{target}: CMake configure"), &configure);
        let cmake_build = Command::new("cmake")
            .arg("--build")
            .arg(&build)
            .output()
            .expect("run CMake build");
        assert_success(&format!("{target}: CMake build"), &cmake_build);
        check(&build.join(exe), target.ends_with("arm64"));
    }
}
