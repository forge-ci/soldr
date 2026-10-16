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
    let spec = msvc_spec("aarch64-pc-windows-msvc", xwin);
    assert!(spec
        .common
        .contains(&"--target=aarch64-pc-windows-msvc".to_string()));
    let isystems = spec.common.iter().filter(|a| *a == "-isystem").count();
    assert_eq!(isystems, 2, "{:?}", spec.common);
    assert!(!spec.common.iter().any(|a| a.contains("winrt")));
    let libs: Vec<_> = spec
        .config_lines
        .iter()
        .filter(|l| l.starts_with("-L"))
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
    let spec = msvc_spec("x86_64-pc-windows-msvc", tmp.path());
    assert!(spec.common.contains(&"-fms-runtime-lib=dll".to_string()));
    assert!(!spec.common.iter().any(|arg| arg.contains("vfsoverlay")));
    assert!(!spec.common.iter().any(|arg| arg.contains("fuse-ld")));
}
