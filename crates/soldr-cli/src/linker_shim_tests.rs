//! soldr#3520: the managed clang drives a Linux link only when it can see the
//! host's C runtime; otherwise the host's wrapper clang (NixOS cc-wrapper)
//! does.

use super::{
    host_cc_fallback, managed_clang_finds_host_runtime, materialize_host_cc_shim,
    prefer_host_wrapper_clang,
};
use std::path::{Path, PathBuf};

/// A fake clang whose `-print-file-name=<f>` answers like a real one: the
/// absolute path when `<f>` is under `lib_dir`, else the bare name.
fn fake_clang(dir: &Path, name: &str, lib_dir: &Path) -> PathBuf {
    let clang = dir.join(name);
    std::fs::write(
        &clang,
        format!(
            "#!/bin/sh\nf=\"${{1#-print-file-name=}}\"\n\
             if [ -e '{lib}'/\"$f\" ]; then echo '{lib}'/\"$f\"; else echo \"$f\"; fi\n",
            lib = lib_dir.display()
        ),
    )
    .unwrap();
    crate::platform::fs::permissions::make_executable(&clang).unwrap();
    clang
}

#[test]
fn a_wrapper_only_libgcc_s_makes_the_system_clang_drive_the_link() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    // NixOS shape: the runtime lives only in a store path the managed
    // (unwrapped) clang does not search; only the wrapper knows it.
    let fhs = temp.path().join("usr/lib");
    let store = temp.path().join("nix/store/hash-gcc-15.3.0-lib/lib");
    std::fs::create_dir_all(&fhs).unwrap();
    std::fs::create_dir_all(&store).unwrap();
    for file in ["Scrt1.o", "libgcc_s.so"] {
        std::fs::write(store.join(file), b"").unwrap();
    }
    let managed = fake_clang(temp.path(), "managed-clang", &fhs);
    let system = PathBuf::from("/run/current-system/sw/bin/clang");

    assert!(!managed_clang_finds_host_runtime(&managed));
    assert_eq!(
        prefer_host_wrapper_clang(
            managed.clone(),
            Some(system.clone()),
            true,
            managed_clang_finds_host_runtime
        ),
        system,
        "a managed clang blind to libgcc_s must yield to the host wrapper"
    );

    // Only libgcc_s is wrapper-only: still the wrapper.
    std::fs::write(fhs.join("Scrt1.o"), b"").unwrap();
    assert!(!managed_clang_finds_host_runtime(&managed));

    // An FHS host (Debian, Ubuntu, Fedora): the managed clang keeps the link.
    std::fs::write(fhs.join("libgcc_s.so"), b"").unwrap();
    assert!(managed_clang_finds_host_runtime(&managed));
    assert_eq!(
        prefer_host_wrapper_clang(
            managed.clone(),
            Some(system.clone()),
            true,
            managed_clang_finds_host_runtime
        ),
        managed
    );
}

#[test]
fn the_managed_clang_stays_off_linux_without_a_system_clang_or_when_unprobeable() {
    let managed = PathBuf::from("/managed/clang");
    let system = PathBuf::from("/usr/bin/clang");
    let blind = |_: &Path| false;
    assert_eq!(
        prefer_host_wrapper_clang(managed.clone(), Some(system.clone()), false, blind),
        managed,
        "Windows/macOS hosts keep the managed clang"
    );
    assert_eq!(
        prefer_host_wrapper_clang(managed.clone(), None, true, blind),
        managed,
        "no usable system clang: nothing better to use"
    );
    assert!(
        managed_clang_finds_host_runtime(Path::new("/nonexistent/soldr-3520/clang")),
        "a probe that cannot run must not change the driver"
    );
}

/// soldr#3520 follow-up: a NixOS host with a `cc` (the gcc cc-wrapper) but no
/// `clang` on the search path. Neither the managed clang nor a system clang
/// can drive the link, so the host `cc` drives it -- with no fast-linker
/// argument, exactly the `CARGO_TARGET_<TRIPLE>_LINKER=$(which cc)`
/// workaround users applied by hand.
#[test]
fn a_gcc_only_wrapper_host_links_through_the_host_cc() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let fhs = temp.path().join("usr/lib");
    let store = temp.path().join("nix/store/hash-glibc-2.42/lib");
    std::fs::create_dir_all(&fhs).unwrap();
    std::fs::create_dir_all(&store).unwrap();
    for file in ["Scrt1.o", "libgcc_s.so"] {
        std::fs::write(store.join(file), b"").unwrap();
    }
    let managed = fake_clang(temp.path(), "managed-clang", &fhs);
    let host_cc = fake_clang(temp.path(), "cc", &store);
    let host = "x86_64-unknown-linux-gnu";

    assert_eq!(
        host_cc_fallback(host, host, true, &managed, None, Some(host_cc.clone())),
        Some(host_cc.clone()),
        "a clang blind to the C runtime yields to a host cc that sees it"
    );
    assert_eq!(
        host_cc_fallback(host, host, true, &managed, None, None),
        None,
        "no host cc: nothing better to use"
    );
    assert_eq!(
        host_cc_fallback(
            "aarch64-unknown-linux-gnu",
            host,
            true,
            &managed,
            None,
            Some(host_cc.clone())
        ),
        None,
        "a cross target never links through the host's native cc"
    );
    assert_eq!(
        host_cc_fallback(host, host, false, &managed, None, Some(host_cc.clone())),
        None,
        "non-Linux hosts are untouched"
    );
    // A NixOS host *with* clang: soldr#3520 already chose the system wrapper
    // clang, whose `-print-file-name=libgcc_s.so` comes back bare (the
    // wrapper injects that path only at link time) although it links fine.
    // It keeps the link and the fast-linker argument.
    let wrapper_clang = fake_clang(temp.path(), "wrapper-clang", &fhs);
    assert_eq!(
        host_cc_fallback(
            host,
            host,
            true,
            &wrapper_clang,
            Some(&wrapper_clang),
            Some(host_cc.clone())
        ),
        None,
        "a chosen system clang is never demoted to the host cc"
    );
    let blind_cc = fake_clang(temp.path(), "blind-cc", &fhs);
    assert_eq!(
        host_cc_fallback(host, host, true, &managed, None, Some(blind_cc)),
        None,
        "a host cc that cannot see the runtime either is no improvement"
    );
    assert_eq!(
        host_cc_fallback(
            host,
            host,
            true,
            &managed,
            None,
            Some(PathBuf::from("/nonexistent/soldr-nixos/cc"))
        ),
        None,
        "a host cc that cannot run is never chosen"
    );

    // An FHS host (Debian, Ubuntu, Fedora): the clang keeps the link.
    for file in ["Scrt1.o", "libgcc_s.so"] {
        std::fs::write(fhs.join(file), b"").unwrap();
    }
    assert_eq!(
        host_cc_fallback(host, host, true, &managed, None, Some(host_cc)),
        None
    );
}

#[test]
fn the_host_cc_shim_execs_cc_without_a_fast_linker_argument() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let paths = crate::core::SoldrPaths::with_root(temp.path().to_path_buf());
    let mut injection = crate::linker::LinkerInjection {
        linker: Some("clang".to_string()),
        rustflags: Some("-C link-arg=-fuse-ld=lld".to_string()),
    };
    let cc = Path::new("/nix/store/hash-gcc-wrapper/bin/cc");
    materialize_host_cc_shim(&paths, &mut injection, cc).unwrap();
    let shim = PathBuf::from(injection.linker.as_deref().unwrap());
    assert!(shim.starts_with(paths.bin.join("linker-shims").join("v1")));
    assert_eq!(injection.rustflags, None);
    let body = std::fs::read_to_string(&shim).unwrap();
    assert!(
        body.ends_with("exec '/nix/store/hash-gcc-wrapper/bin/cc' \"$@\"\n"),
        "{body}"
    );
    assert!(!body.contains("fuse-ld"), "{body}");
}

// -- soldr#3797: rustc's bundled lld makes a system clang sufficient --------

fn fake_system_clang_without_lld(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let clang = bin.join("clang");
    std::fs::write(&clang, "#!/bin/sh\nexit 0\n").unwrap();
    crate::platform::fs::permissions::make_executable(&clang).unwrap();
    clang
}

#[test]
fn a_system_clang_without_lld_is_usable_only_when_rustc_supplies_one() {
    let clang = PathBuf::from("/nonexistent/soldr-3797/clang");
    let usable = |needs, rustc, system: Option<&PathBuf>| {
        super::usable_system_clang_for_tests(system, needs, rustc, None)
    };
    // The #3430 path is unchanged when rustc supplies nothing.
    assert_eq!(usable(true, false, Some(&clang)), None);
    // rustc's lld lifts the requirement.
    assert_eq!(usable(true, true, Some(&clang)), Some(clang.clone()));
    // No lld needed: always usable, as before.
    assert_eq!(usable(false, false, Some(&clang)), Some(clang.clone()));
    // No system clang at all keeps today's fetch path.
    assert_eq!(usable(true, true, None), None);
}

#[test]
fn no_llvm_fetch_when_rustc_supplies_lld_and_a_system_clang_exists() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let clang = fake_system_clang_without_lld(temp.path());
    let _path = crate::EnvVarGuard::set("PATH", clang.parent().unwrap());
    let _llvm_dir = crate::EnvVarGuard::remove("SOLDR_LLVM_DIR");
    let paths = crate::core::SoldrPaths::with_root(temp.path().join("root"));
    let resolved = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(super::resolve_driver_clang_for_tests(&paths, true, true))
        .expect("the system clang drives the link");
    assert_eq!(resolved, clang);
    assert!(
        !paths.bin.exists(),
        "nothing may be fetched or materialized under the cache root"
    );
}

#[test]
fn the_shim_digest_is_stable_for_a_fixed_clang_path() {
    let temp = tempfile::tempdir().unwrap();
    let paths = crate::core::SoldrPaths::with_root(temp.path().to_path_buf());
    let clang = PathBuf::from("/usr/bin/clang");
    let link = |paths: &crate::core::SoldrPaths| {
        let mut injection = crate::linker::LinkerInjection {
            linker: Some("clang".to_string()),
            rustflags: Some("-C link-arg=-fuse-ld=lld".to_string()),
        };
        super::materialize_linker_driver_shim(
            paths,
            "x86_64-unknown-linux-gnu",
            &mut injection,
            Some(&clang),
        )
        .unwrap();
        injection.linker.unwrap()
    };
    let first = link(&paths);
    assert_eq!(first, link(&paths));
    // The body names the clang and the driver argument only: no sysroot, no -B.
    let body = std::fs::read_to_string(&first).unwrap();
    assert!(!body.contains(" -B"), "{body}");
    assert!(body.contains("/usr/bin/clang") && body.contains("-fuse-ld=lld"));
    let other = temp.path().join("other");
    let moved = link(&crate::core::SoldrPaths::with_root(other));
    assert_eq!(
        Path::new(&first).file_name(),
        Path::new(&moved).file_name(),
        "the digest depends on the body, not the cache root"
    );
}
