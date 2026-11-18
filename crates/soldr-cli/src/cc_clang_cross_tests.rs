use super::*;

#[test]
fn handles_exactly_the_apple_and_msvc_targets() {
    for target in [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
    ] {
        assert!(handles_target(target), "{target}");
    }
    for target in [
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-gnu",
        "aarch64-apple-ios",
        "i686-pc-windows-msvc",
    ] {
        assert!(!handles_target(target), "{target}");
    }
}

#[test]
fn apple_spec_carries_target_and_sdk_and_keeps_lld_link_only() {
    let spec = apple_spec("aarch64-apple-darwin", Path::new("/sdk/MacOSX14.5.sdk"));
    assert!(spec
        .common
        .contains(&"--target=arm64-apple-darwin".to_string()));
    let at = spec.common.iter().position(|a| a == "-isysroot").unwrap();
    assert_eq!(spec.common[at + 1], "/sdk/MacOSX14.5.sdk");
    assert!(!spec.common.iter().any(|a| a.contains("fuse-ld")));
    assert_eq!(spec.config_lines, vec!["-fuse-ld=lld".to_string()]);
    assert_eq!(spec.cxx_extra, vec!["-stdlib=libc++".to_string()]);
}

#[test]
fn msvc_spec_lists_existing_sdk_dirs_only() {
    let tmp = tempfile::tempdir().unwrap();
    let xwin = tmp.path();
    for dir in [
        "crt/include",
        "sdk/include/ucrt",
        "crt/lib/arm64",
        "sdk/lib/um/arm64",
    ] {
        std::fs::create_dir_all(xwin.join(dir)).unwrap();
    }
    let spec = msvc_spec(
        "aarch64-pc-windows-msvc",
        xwin,
        Path::new("/llvm/lib/clang/21"),
    );
    assert!(spec
        .common
        .contains(&"--target=aarch64-pc-windows-msvc".to_string()));
    let isystems = spec.common.iter().filter(|a| *a == "-isystem").count();
    assert_eq!(isystems, 3, "{:?}", spec.common);
    assert!(!spec.common.iter().any(|a| a.contains("winrt")));
    let libs: Vec<_> = spec
        .config_lines
        .iter()
        .filter(|l| l.trim_start_matches('"').starts_with("-L"))
        .collect();
    assert_eq!(libs.len(), 2, "{:?}", spec.config_lines);
    assert!(spec.config_lines.contains(&"-fuse-ld=lld".to_string()));
}

#[test]
fn unix_driver_execs_absolute_clang_then_flags_then_caller_args() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let body = render_driver(
        Path::new("/llvm/bin/clang"),
        &["--target=x".to_string(), "it's".to_string()],
    )
    .unwrap();
    assert!(body.starts_with("#!/bin/sh\n"));
    assert!(body.ends_with("exec '/llvm/bin/clang' '--target=x' 'it'\\''s' \"$@\"\n"));
}

#[test]
fn drivers_are_content_addressed_and_idempotent() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(tmp.path().join("root"));
    let bin = tmp.path().join("llvm");
    std::fs::create_dir_all(&bin).unwrap();
    for tool in ["clang", "clang++"] {
        std::fs::write(bin.join(tool), "").unwrap();
    }
    let spec = apple_spec("x86_64-apple-darwin", Path::new("/sdk"));
    let (cc, cxx) = write_drivers(&paths, "x86_64-apple-darwin", &bin, &spec).unwrap();
    assert_eq!(
        write_drivers(&paths, "x86_64-apple-darwin", &bin, &spec).unwrap(),
        (cc.clone(), cxx.clone())
    );
    let cc_body = std::fs::read_to_string(&cc).unwrap();
    let cxx_body = std::fs::read_to_string(&cxx).unwrap();
    assert!(cc_body.contains("--target=x86_64-apple-darwin"));
    assert!(cc_body.contains("--config="));
    assert!(!cc_body.contains("-stdlib=libc++"));
    assert!(cxx_body.contains("-stdlib=libc++"));
    let other = apple_spec("x86_64-apple-darwin", Path::new("/other-sdk"));
    let (cc2, _) = write_drivers(&paths, "x86_64-apple-darwin", &bin, &other).unwrap();
    assert_ne!(cc, cc2, "a different SDK must be a different driver path");
}

#[test]
fn drivers_sit_beside_the_llvm_binutils_cmake_needs() {
    // soldr#3806: without llvm-ranlib beside the driver, CMake used the host
    // GNU ranlib and corrupted a Mach-O static archive.
    let tmp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(tmp.path().join("root"));
    let bin = tmp.path().join("llvm");
    std::fs::create_dir_all(&bin).unwrap();
    let native = crate::platform::executable::name::native;
    for tool in ["clang", "clang++", "llvm-ranlib", "llvm-rc"] {
        std::fs::write(bin.join(native(tool)), tool).unwrap();
    }
    let spec = apple_spec("aarch64-apple-darwin", Path::new("/sdk"));
    let (cc, _) = write_drivers(&paths, "aarch64-apple-darwin", &bin, &spec).unwrap();
    let dir = cc.parent().unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join(native("llvm-ranlib"))).unwrap(),
        "llvm-ranlib"
    );
    assert!(dir.join(native("llvm-rc")).is_file());
    assert!(
        !dir.join(native("llvm-ar")).exists(),
        "absent tools are skipped"
    );
}

#[test]
fn msvc_objects_name_the_crt_and_link_only_flags_stay_in_the_config() {
    let tmp = tempfile::tempdir().unwrap();
    let spec = msvc_spec(
        "x86_64-pc-windows-msvc",
        tmp.path(),
        Path::new("/llvm/lib/clang/21"),
    );
    assert!(spec.common.contains(&"-fms-runtime-lib=dll".to_string()));
    assert!(!spec.common.iter().any(|arg| arg.contains("vfsoverlay")));
    assert!(!spec.common.iter().any(|arg| arg.contains("fuse-ld")));
}

#[test]
fn msvc_driver_switches_to_real_cl_arguments_and_publishes_clang_cl() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(tmp.path().join("root"));
    let bin = tmp.path().join("llvm/bin");
    let xwin = tmp.path().join("sdk with spaces");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(xwin.join("crt/include")).unwrap();
    std::fs::create_dir_all(xwin.join("crt/lib/x86_64")).unwrap();
    for tool in ["clang", "clang++", "lld-link"] {
        let path = bin.join(tool);
        std::fs::write(&path, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        crate::platform::fs::permissions::make_executable(&path).unwrap();
    }
    let spec = msvc_spec(
        "x86_64-pc-windows-msvc",
        &xwin,
        Path::new("/llvm/lib/clang/21"),
    );
    let (cc, _) = write_drivers(&paths, "x86_64-pc-windows-msvc", &bin, &spec).unwrap();
    let output = std::process::Command::new(&cc)
        .args(["--driver-mode=cl", "/c", "probe.c", "/Foprobe.obj"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let args = String::from_utf8(output.stdout).unwrap();
    assert!(args.contains("/imsvc"), "{args}");
    assert!(!args.contains("-isystem"), "{args}");
    assert!(!args.contains("-fms-runtime-lib"), "{args}");
    assert!(cc
        .parent()
        .unwrap()
        .join("x86_64-pc-windows-msvc-clang-cl")
        .is_file());
    let linker = std::process::Command::new(cc.parent().unwrap().join("lld-link"))
        .args(["/dll", "probe.obj"])
        .output()
        .unwrap();
    assert!(linker.status.success());
    let linker_args = String::from_utf8(linker.stdout).unwrap();
    assert!(linker_args.contains("/LIBPATH:"));
    assert!(linker_args.contains("lib/windows/clang_rt.builtins-x86_64.lib"));
    assert!(linker_args.contains("probe.obj"));
}

#[test]
fn msvc_resource_headers_precede_sdk_in_both_modes_and_keep_overlay() {
    let tmp = tempfile::tempdir().unwrap();
    let sdk = tmp.path().join("SDK with spaces");
    std::fs::create_dir_all(sdk.join("crt/include")).unwrap();
    std::fs::write(sdk.join("crt/include/intrin.h"), "#pragma once\n").unwrap();
    std::fs::create_dir_all(sdk.join("crt/lib/x86_64")).unwrap();
    let resource = Path::new("/LLVM with spaces/lib/clang/21");
    let spec = msvc_spec("x86_64-pc-windows-msvc", &sdk, resource);
    let at = spec.common.iter().position(|a| a == "-isystem").unwrap();
    assert_eq!(
        spec.common[at + 1],
        resource.join("include").display().to_string()
    );
    let cl = spec.cl.as_ref().unwrap();
    let includes: Vec<_> = cl
        .common
        .iter()
        .filter(|a| a.starts_with("/imsvc"))
        .collect();
    assert_eq!(
        includes[0],
        &format!("/imsvc{}", resource.join("include").display())
    );
    assert_eq!(
        includes[1],
        &format!("/imsvc{}", sdk.join("crt/include").display())
    );
    assert!(cl
        .config_lines
        .iter()
        .any(|a| a.contains("-Xclang -ivfsoverlay -Xclang")));
    assert!(cl.config_lines.iter().any(|a| a == "$-Xlinker"));
    assert!(cl
        .config_lines
        .iter()
        .any(|a| a.starts_with("\"$/LIBPATH:")));
    assert!(cl.config_lines.contains(&"-rtlib=compiler-rt".to_string()));
    assert!(!cl.common.iter().any(|a| a == "-isystem"));
    assert!(!cl.config_lines.iter().any(|a| a.starts_with("-L")));
    let policy: serde_json::Value =
        serde_json::from_str(&msvc_runtime_policy("x86_64-pc-windows-msvc")).unwrap();
    assert_eq!(policy["vc_runtime"], "hosted-runner-System32");
    assert_eq!(
        policy["redistributed_microsoft_dlls"],
        serde_json::json!([])
    );
}

/// Refs #4167: the CL parser counts even forwarded linker inputs before /Fo.
/// A real driver plan catches this without downloading an SDK or linking.
#[test]
fn real_clang_cl_config_keeps_libpaths_out_of_compile_only_inputs() {
    // A host compiler wrapper (notably Nix cc-wrapper) can inject GNU flags
    // independently of Soldr. Exercise the managed, unwrapped LLVM executable.
    let clang = std::env::var_os("SOLDR_TEST_CLANG").map(PathBuf::from);
    let Some(clang) = clang else {
        eprintln!("skip: set SOLDR_TEST_CLANG to the managed, unwrapped Clang 21+ executable");
        return;
    };
    let version = std::process::Command::new(&clang)
        .arg("--version")
        .output()
        .unwrap();
    let version = String::from_utf8_lossy(&version.stdout);
    let major = version
        .split("clang version ")
        .nth(1)
        .and_then(|s| s.split('.').next())
        .and_then(|s| s.parse::<u32>().ok());
    if major.is_none_or(|v| v < 21) {
        eprintln!("skip: real CL parser regression needs Clang 21+, found {version}");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let sdk = temp.path().join("SDK with spaces");
    std::fs::create_dir_all(sdk.join("crt/lib/x86_64")).unwrap();
    let spec = msvc_spec("x86_64-pc-windows-msvc", &sdk, temp.path());
    let cl = spec.cl.unwrap();
    let config = temp.path().join("cl.cfg");
    std::fs::write(&config, cl.config_lines.join("\n")).unwrap();
    let mut args = cl.common;
    args.push(format!("--config={}", config.display()));
    let suffix = crate::platform::executable::name::script_suffix();
    let driver = temp.path().join(format!("clang-cl{suffix}"));
    crate::linker_shim::write_content_addressed_shim(
        &driver,
        &render_msvc_driver(&clang, &spec.common, &args).unwrap(),
    )
    .unwrap();
    let input = temp.path().join("probe.c");
    std::fs::write(&input, "int probe(void) { return 42; }\n").unwrap();
    let compile = std::process::Command::new(&driver)
        .args(["-###", "/c", "/Foprobe.obj"])
        .arg(&input)
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let link = std::process::Command::new(&driver)
        .args(["-###", "/LD", "/clang:-nostdlib"])
        .arg(input)
        .output()
        .unwrap();
    assert!(
        link.status.success(),
        "{}",
        String::from_utf8_lossy(&link.stderr)
    );
    assert!(String::from_utf8_lossy(&link.stderr).contains(&format!(
        "/LIBPATH:{}",
        sdk.join("crt/lib/x86_64").display()
    )));
    assert!(
        String::from_utf8_lossy(&link.stderr).contains("lib/windows/clang_rt.builtins-x86_64.lib")
    );
}
