//! Unit coverage split from `blessed_build.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;
use crate::platform::host::facts::{HostArch, HostOs};
use crate::TEST_PROCESS_ENV_LOCK as ENV_MUTEX;

// Serialize tests that mutate process env vars. `std::env::set_var`
// / `remove_var` mutate global state, and cargo runs tests in
// parallel within a single process — without a barrier the tests
// race and intermittently fail (soldr#1267).
// soldr#2519 deleted `opt_out_env_var_recognized`: the opt-out it covered
// (`SOLDR_USE_LEGACY_XWIN`) no longer exists. What still needs proving is
// that MSVC prep is decided by host+target alone, with no escape hatch.
#[test]
fn xwin_prep_is_linux_host_only_and_has_no_opt_out() {
    assert_eq!(
        should_prepare_xwin_for_target("x86_64-pc-windows-msvc"),
        crate::platform::host::facts::os() == HostOs::Linux
    );
    assert_eq!(
        should_prepare_xwin_for_target("X86_64-PC-Windows-MSVC"),
        crate::platform::host::facts::os() == HostOs::Linux,
        "target classification is case-insensitive before canonicalization"
    );
    assert!(!should_prepare_xwin_for_target("x86_64-unknown-linux-musl"));

    // The removed toggle must stay removed: setting it changes nothing.
    let _guard = ENV_MUTEX.lock();
    let prev = std::env::var_os("SOLDR_USE_LEGACY_XWIN");
    std::env::set_var("SOLDR_USE_LEGACY_XWIN", "1");
    assert_eq!(
        should_prepare_xwin_for_target("x86_64-pc-windows-msvc"),
        crate::platform::host::facts::os() == HostOs::Linux,
        "SOLDR_USE_LEGACY_XWIN must no longer suppress blessed MSVC prep"
    );
    match prev {
        Some(v) => std::env::set_var("SOLDR_USE_LEGACY_XWIN", v),
        None => std::env::remove_var("SOLDR_USE_LEGACY_XWIN"),
    }
}

#[test]
fn native_windows_msvc_gets_no_xwin_prep() {
    if crate::platform::host::facts::os() != HostOs::Windows {
        return;
    }
    let _guard = ENV_MUTEX.lock();
    let prev_sys = std::env::var_os(USE_LEGACY_VENDORED_SYS_ENV_VAR);
    let prev_cmake = std::env::var_os(USE_SYSTEM_CMAKE_ENV_VAR);

    std::env::set_var(USE_LEGACY_VENDORED_SYS_ENV_VAR, "1");
    std::env::set_var(USE_SYSTEM_CMAKE_ENV_VAR, "1");

    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let target = if crate::platform::host::facts::arch() == HostArch::Aarch64 {
        "aarch64-pc-windows-msvc"
    } else {
        "x86_64-pc-windows-msvc"
    };
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(prepare(&paths, target, &[]));

    match prev_sys {
        Some(v) => std::env::set_var(USE_LEGACY_VENDORED_SYS_ENV_VAR, v),
        None => std::env::remove_var(USE_LEGACY_VENDORED_SYS_ENV_VAR),
    }
    match prev_cmake {
        Some(v) => std::env::set_var(USE_SYSTEM_CMAKE_ENV_VAR, v),
        None => std::env::remove_var(USE_SYSTEM_CMAKE_ENV_VAR),
    }

    let prep = result.expect("native Windows MSVC target should not error");
    assert!(prep.xwin_cache_dir.is_none());
    assert!(prep.shim_path_dir.is_none());
    assert!(prep.sdkroot.is_none());
    assert!(prep.env.is_empty());
    assert!(prep.path_dirs.is_empty());
    assert!(prep.cargo_args.is_empty());
}

#[test]
fn linux_targets_get_no_xwin_or_sdk_prep() {
    let _guard = ENV_MUTEX.lock();
    // soldr#1064 Phase B: syslib catalogue overrides may populate
    // prep.env even on linux targets. The invariant we still want to
    // assert is "linux gets no Windows-xwin and no Apple-SDK prep" —
    // opt out of catalogue injection (and the managed cmake/ninja
    // injection, which is host-side and target-independent) to keep
    // this test hermetic.
    std::env::set_var(USE_LEGACY_VENDORED_SYS_ENV_VAR, "1");
    std::env::set_var(USE_SYSTEM_CMAKE_ENV_VAR, "1");
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let result = tokio::runtime::Runtime::new().unwrap().block_on(prepare(
        &paths,
        "x86_64-unknown-linux-musl",
        &[],
    ));
    std::env::remove_var(USE_LEGACY_VENDORED_SYS_ENV_VAR);
    std::env::remove_var(USE_SYSTEM_CMAKE_ENV_VAR);
    let prep = result.expect("linux musl target should not error");
    assert!(prep.xwin_cache_dir.is_none());
    assert!(prep.sdkroot.is_none());
    assert!(prep.env.is_empty());
    assert!(prep.cargo_args.is_empty());
}

#[test]
fn msvc_tool_env_uses_clang_cl_for_cc_rs() {
    let mut prep = BlessedPrep::default();

    add_msvc_tool_env(
        &mut prep,
        "x86_64_pc_windows_msvc",
        "X86_64_PC_WINDOWS_MSVC",
    );

    let env: std::collections::HashMap<&str, &str> = prep
        .env
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    assert_eq!(env.get("CC_x86_64_pc_windows_msvc"), Some(&"clang-cl"));
    assert_eq!(env.get("CXX_x86_64_pc_windows_msvc"), Some(&"clang-cl"));
    assert_eq!(env.get("AR_x86_64_pc_windows_msvc"), Some(&"llvm-lib"));
    assert_eq!(
        env.get("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"),
        Some(&"lld-link")
    );
}

#[test]
fn path_prefix_keeps_clang_shim_ahead_of_managed_tools() {
    let shim = PathBuf::from("/soldr/clang-shim");
    let llvm = PathBuf::from("/soldr/llvm/bin");
    let cmake = PathBuf::from("/soldr/cmake/bin");
    let prep = BlessedPrep {
        shim_path_dir: Some(shim.clone()),
        path_dirs: vec![llvm.clone(), cmake.clone()],
        ..Default::default()
    };

    assert_eq!(prep.path_prefix(), vec![shim, llvm, cmake]);
}

#[test]
fn dsymutil_override_is_added_to_child_path() {
    let _guard = ENV_MUTEX.lock();
    let tmp = tempfile::tempdir().expect("tmpdir");
    let bin = tmp.path().join("dsymutil");
    std::fs::write(&bin, b"stub").expect("stub");
    let prior = std::env::var_os(SOLDR_DSYMUTIL_ENV_VAR);
    std::env::set_var(SOLDR_DSYMUTIL_ENV_VAR, &bin);
    let mut prep = BlessedPrep::default();
    ensure_dsymutil_on_path(&mut prep).expect("override should satisfy preflight");
    match prior {
        Some(value) => std::env::set_var(SOLDR_DSYMUTIL_ENV_VAR, value),
        None => std::env::remove_var(SOLDR_DSYMUTIL_ENV_VAR),
    }
    assert_eq!(prep.path_dirs, vec![tmp.path()]);
}

#[test]
fn darwin_lld_policy_uses_host_lld_on_linux_fallback() {
    assert!(
        darwin_should_use_lld(true),
        "managed LLVM should always enable LLD for Darwin cross-links",
    );

    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Linux {
        assert!(
            darwin_should_use_lld(false),
            "Linux Darwin fallback must prefer LLD over GNU ld for Mach-O links",
        );
    } else {
        assert!(
            !darwin_should_use_lld(false),
            "non-Linux fallback keeps platform clang behavior unless managed LLVM is present",
        );
    }
}

#[test]
fn mingw_w64_gcc_env_injects_target_scoped_tools() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let root = tmp.path().join("mingw");
    let mut prep = BlessedPrep::default();

    add_mingw_w64_gcc_env(&mut prep, "x86_64-pc-windows-gnu", &root);

    assert_eq!(
        prep.path_dirs,
        vec![crate::fetch::mingw_w64_gcc::bin_dir(&root)]
    );
    let names: std::collections::HashSet<&str> =
        prep.env.iter().map(|(name, _)| name.as_str()).collect();
    for required in [
        "MINGW_W64_GCC_ROOT",
        "MINGW_W64_GCC_BIN",
        "CC_x86_64_pc_windows_gnu",
        "CXX_x86_64_pc_windows_gnu",
        "AR_x86_64_pc_windows_gnu",
        "RANLIB_x86_64_pc_windows_gnu",
        "WINDRES_x86_64_pc_windows_gnu",
        "CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER",
    ] {
        assert!(names.contains(required), "missing env var {required}");
    }
}

#[test]
fn windows_gnu_requires_supported_mingw_host() {
    let host = crate::platform::host::facts::info();
    let supported =
        (host.os == HostOs::Windows || host.os == HostOs::Linux) && host.arch == HostArch::X86_64;
    if supported {
        return;
    }
    let _guard = ENV_MUTEX.lock();
    let prev_sys = std::env::var_os(USE_LEGACY_VENDORED_SYS_ENV_VAR);
    let prev_cmake = std::env::var_os(USE_SYSTEM_CMAKE_ENV_VAR);

    std::env::set_var(USE_LEGACY_VENDORED_SYS_ENV_VAR, "1");
    std::env::set_var(USE_SYSTEM_CMAKE_ENV_VAR, "1");

    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let result = tokio::runtime::Runtime::new().unwrap().block_on(prepare(
        &paths,
        "x86_64-pc-windows-gnu",
        &[],
    ));

    match prev_sys {
        Some(v) => std::env::set_var(USE_LEGACY_VENDORED_SYS_ENV_VAR, v),
        None => std::env::remove_var(USE_LEGACY_VENDORED_SYS_ENV_VAR),
    }
    match prev_cmake {
        Some(v) => std::env::set_var(USE_SYSTEM_CMAKE_ENV_VAR, v),
        None => std::env::remove_var(USE_SYSTEM_CMAKE_ENV_VAR),
    }

    let err = result.expect_err("unsupported host should fail before cargo");
    assert!(matches!(err, SoldrError::UnsupportedPlatform(_)));
    assert!(
        err.to_string().contains("cargo-zigbuild is no longer used"),
        "unexpected error: {err}"
    );
}

#[test]
fn cmake_generator_sweep_removes_only_mismatches() {
    // Simulate a pre-Ninja cached target tree: one Unix Makefiles
    // cache in the host-profile layout, one Ninja cache in the
    // per-triple layout, one Visual Studio cache in the per-triple
    // layout. Sweep for Ninja: the mismatched two vanish, the
    // Ninja one survives.
    let tmp = tempfile::tempdir().expect("tmpdir");
    let target = tmp.path();

    let mk = |rel: &str, generator: &str| {
        let out_build = target.join(rel).join("out").join("build");
        std::fs::create_dir_all(&out_build).unwrap();
        std::fs::write(
            out_build.join("CMakeCache.txt"),
            format!("SOMEVAR:BOOL=ON\nCMAKE_GENERATOR:INTERNAL={generator}\n"),
        )
        .unwrap();
        out_build
    };

    let stale_host = mk("release/build/libz-ng-sys-aaa", "Unix Makefiles");
    let ninja_triple = mk(
        "aarch64-apple-darwin/release/build/libz-ng-sys-bbb",
        "Ninja",
    );
    let stale_triple = mk(
        "x86_64-pc-windows-msvc/debug/build/zstd-sys-ccc",
        "Visual Studio 16 2019",
    );

    sweep_mismatched_cmake_build_dirs(target, "Ninja");

    assert!(!stale_host.exists(), "Unix Makefiles dir must be swept");
    assert!(!stale_triple.exists(), "Visual Studio dir must be swept");
    assert!(ninja_triple.exists(), "matching Ninja dir must survive");
    assert!(
        ninja_triple.join("CMakeCache.txt").is_file(),
        "surviving cache intact"
    );
}

#[test]
fn cmake_generator_sweep_tolerates_missing_target() {
    // A nonexistent target root is a no-op, not an error.
    sweep_mismatched_cmake_build_dirs(std::path::Path::new("Z:/does/not/exist"), "Ninja");
}

#[test]
fn cmake_injection_respects_system_opt_out() {
    let _guard = ENV_MUTEX.lock();
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let mut prep = BlessedPrep::default();

    std::env::set_var(USE_SYSTEM_CMAKE_ENV_VAR, "1");
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(inject_cmake_tooling(&paths, &mut prep));
    std::env::remove_var(USE_SYSTEM_CMAKE_ENV_VAR);

    assert!(prep.env.is_empty(), "opt-out must inject nothing");
    assert!(prep.path_dirs.is_empty());
}

#[test]
fn cmake_injection_defers_to_user_cmake_env() {
    let _guard = ENV_MUTEX.lock();
    // A caller-provided CMAKE (or CMAKE_GENERATOR) means the user
    // already decided; the managed injection must stand down
    // entirely — no fetch attempt, no env, no PATH dirs.
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let rt = tokio::runtime::Runtime::new().unwrap();

    for user_var in ["CMAKE", "CMAKE_GENERATOR"] {
        let mut prep = BlessedPrep::default();
        std::env::set_var(user_var, "user-choice");
        rt.block_on(inject_cmake_tooling(&paths, &mut prep));
        std::env::remove_var(user_var);
        assert!(
            prep.env.is_empty(),
            "pre-set {user_var} must suppress injection"
        );
        assert!(prep.path_dirs.is_empty());
    }
}

#[test]
fn xwin_cflags_emits_imsvc_for_present_dirs() {
    // soldr#1036: simulate an xwin-cache layout, confirm CFLAGS
    // string contains an `/imsvc <path>` entry for each present
    // include subtree (and skips absent ones).
    let tmp = tempfile::tempdir().expect("tmpdir");
    let root = tmp.path();
    // Materialize crt/include + sdk/include/ucrt only; leave the
    // others absent to prove the filter works.
    std::fs::create_dir_all(root.join("crt").join("include")).unwrap();
    std::fs::create_dir_all(root.join("sdk").join("include").join("ucrt")).unwrap();

    let cflags = xwin_msvc_cflags(root);
    assert!(
        cflags.contains("/imsvc"),
        "cflags must contain /imsvc directive: {cflags}"
    );
    // Both materialized paths should appear, separated by `/imsvc`.
    let imsvc_count = cflags.matches("/imsvc").count();
    assert_eq!(
        imsvc_count, 2,
        "expected 2 /imsvc entries (one per present subtree), got: {cflags}"
    );
    // Absent winrt subtree must NOT have an entry.
    assert!(
        !cflags.contains("winrt"),
        "absent winrt subtree leaked into cflags: {cflags}"
    );
}

#[test]
fn xwin_cflags_adds_the_case_insensitive_overlay_once_headers_exist() {
    // soldr#3415: with headers on disk the CFLAGS also carry a
    // `-vfsoverlay <generated file>` so any include casing resolves; the flag
    // names a file that exists and stays the same for the same headers.
    let tmp = tempfile::tempdir().expect("tmpdir");
    let root = tmp.path();
    let shared = root.join("sdk").join("include").join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("basetsd.h"), b"#pragma once\n").unwrap();

    let cflags = xwin_msvc_cflags(root);
    let (_, overlay) = cflags
        .split_once(" -vfsoverlay ")
        .unwrap_or_else(|| panic!("no -vfsoverlay in: {cflags}"));
    assert!(
        std::path::Path::new(overlay).is_file(),
        "the overlay must exist: {overlay}"
    );
    assert_eq!(
        cflags,
        xwin_msvc_cflags(root),
        "stable for the same headers"
    );
}

#[test]
fn xwin_cflags_empty_for_empty_cache() {
    // No subtrees present → empty cflags string. Caller can detect
    // this and skip the CFLAGS_<t> env var injection entirely.
    let tmp = tempfile::tempdir().expect("tmpdir");
    let cflags = xwin_msvc_cflags(tmp.path());
    assert!(cflags.is_empty(), "expected empty cflags, got: {cflags:?}");
}

#[test]
fn xwin_link_args_picks_correct_arch_subdir() {
    // Confirm aarch64-pc-windows-msvc looks under `arm64/`,
    // x86_64-pc-windows-msvc looks under `x64/`. This is the
    // MS-arch-notation contract from xwin's
    // --preserve-ms-arch-notation flag in the upstream recipe.
    let tmp = tempfile::tempdir().expect("tmpdir");
    let root = tmp.path();
    for arch in ["arm64", "x64"] {
        std::fs::create_dir_all(root.join("crt").join("lib").join(arch)).unwrap();
        std::fs::create_dir_all(root.join("sdk").join("lib").join("um").join(arch)).unwrap();
        std::fs::create_dir_all(root.join("sdk").join("lib").join("ucrt").join(arch)).unwrap();
    }

    let aarch64 = xwin_msvc_link_args(root, "aarch64-pc-windows-msvc", CrtLinkage::Dynamic);
    assert!(
        aarch64.contains("/arm64") || aarch64.contains("\\arm64"),
        "aarch64 must hit arm64 subdir: {aarch64}"
    );
    assert!(
        !aarch64.contains("/x64") && !aarch64.contains("\\x64"),
        "aarch64 link args leaked x64 path: {aarch64}"
    );

    let x86 = xwin_msvc_link_args(root, "x86_64-pc-windows-msvc", CrtLinkage::Dynamic);
    assert!(
        x86.contains("/x64") || x86.contains("\\x64"),
        "x86_64 must hit x64 subdir: {x86}"
    );
    assert!(
        !x86.contains("/arm64") && !x86.contains("\\arm64"),
        "x86_64 link args leaked arm64 path: {x86}"
    );
}

#[test]
fn xwin_link_args_unknown_arch_returns_empty() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    // Non-MSVC triple → empty link args.
    let out = xwin_msvc_link_args(tmp.path(), "x86_64-unknown-linux-gnu", CrtLinkage::Dynamic);
    assert!(
        out.is_empty(),
        "non-msvc triple must yield empty link args, got: {out:?}"
    );
}

#[test]
fn xwin_link_args_accepts_cargo_xwin_arch_names() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("crt").join("lib").join("x86_64")).unwrap();
    std::fs::create_dir_all(root.join("sdk").join("lib").join("um").join("x86_64")).unwrap();
    std::fs::create_dir_all(root.join("sdk").join("lib").join("ucrt").join("x86_64")).unwrap();

    let out = xwin_msvc_link_args(root, "x86_64-pc-windows-msvc", CrtLinkage::Dynamic);
    assert!(
        out.contains("/x86_64") || out.contains("\\x86_64"),
        "cargo-xwin-style x86_64 lib dirs must be accepted: {out}"
    );
    assert_eq!(
        out.matches("link-arg=/LIBPATH:").count(),
        3,
        "expected all three x86_64 lib dirs in link args: {out}"
    );
}

#[test]
fn xwin_link_args_accepts_cargo_xwin_aarch64_arch_names() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("crt").join("lib").join("aarch64")).unwrap();
    std::fs::create_dir_all(root.join("sdk").join("lib").join("um").join("aarch64")).unwrap();
    std::fs::create_dir_all(root.join("sdk").join("lib").join("ucrt").join("aarch64")).unwrap();

    let out = xwin_msvc_link_args(root, "aarch64-pc-windows-msvc", CrtLinkage::Dynamic);
    assert!(
        out.contains("/aarch64") || out.contains("\\aarch64"),
        "cargo-xwin-style aarch64 lib dirs must be accepted: {out}"
    );
    assert_eq!(
        out.matches("link-arg=/LIBPATH:").count(),
        3,
        "expected all three aarch64 lib dirs in link args: {out}"
    );
}

#[test]
fn xwin_link_args_format_uses_c_link_arg_pairs() {
    // Each /LIBPATH: must be paired with a leading `-C` so rustc
    // parses them as link-args. Without the `-C` prefix the flag
    // would be passed as a plain rustc arg and silently dropped.
    let tmp = tempfile::tempdir().expect("tmpdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("crt").join("lib").join("arm64")).unwrap();

    let out = xwin_msvc_link_args(root, "aarch64-pc-windows-msvc", CrtLinkage::Dynamic);
    let tokens = out.split_whitespace().collect::<Vec<_>>();
    let libpath_indexes = tokens
        .iter()
        .enumerate()
        .filter_map(|(index, token)| token.starts_with("link-arg=/LIBPATH:").then_some(index))
        .collect::<Vec<_>>();
    assert!(
        !libpath_indexes.is_empty(),
        "expected a /LIBPATH pair: {out}"
    );
    for index in libpath_indexes {
        assert_eq!(tokens.get(index.wrapping_sub(1)), Some(&"-C"), "{out}");
    }
}

#[test]
fn xwin_link_args_select_dynamic_crt_import_libraries() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let root = tmp.path();
    std::fs::create_dir_all(root.join("crt").join("lib").join("arm64")).unwrap();
    std::fs::create_dir_all(root.join("sdk").join("lib").join("ucrt").join("arm64")).unwrap();

    let out = xwin_msvc_link_args(root, "aarch64-pc-windows-msvc", CrtLinkage::Dynamic);
    assert!(out.contains("linker-flavor=lld-link"), "{out}");
    assert!(out.contains("link-arg=/NODEFAULTLIB:libucrt.lib"), "{out}");
    assert!(out.contains("link-arg=/DEFAULTLIB:ucrt.lib"), "{out}");
    assert!(out.contains("link-arg=/DEFAULTLIB:vcruntime.lib"), "{out}");
    assert!(!out.contains("link-arg=/DEFAULTLIB:libucrt.lib"), "{out}");
}
