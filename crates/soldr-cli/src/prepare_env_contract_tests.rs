//! Precedence contract for the flags `soldr prepare --github-env` exports.
//!
//! Lives in its own module rather than in `prepare_cmd`'s test block because
//! that file is already over the LOC ratchet's ceiling and may not grow.
//!
//! ## What is being pinned
//!
//! `apply_blessed_prep_env` exports `CARGO_ENCODED_RUSTFLAGS`, which outranks
//! both `RUSTFLAGS` and `CARGO_TARGET_<triple>_RUSTFLAGS` in Cargo's
//! precedence order. Whatever it writes there is therefore the *only* thing
//! that takes effect, so anything the caller had already put in the
//! lower-precedence variables has to be folded in rather than shadowed.
//!
//! `apply_to_process` has covered the in-process half of this since
//! `applying_target_flags_consumes_higher_precedence_globals`
//! (`target_lifecycle`). The `--github-env` half — the one CI actually runs —
//! had no equivalent.
//!
//! zackees/clud#732 is why this is worth a test: a bump that moved the MSVC
//! link configuration into the encoded variable cost that consumer a CI cycle,
//! because the precedence rule was not written down and its guard assumed the
//! target-scoped key still won.

use crate::blessed_build::BlessedPrep;
use crate::prepare_cmd::apply_blessed_prep_env;
use crate::{EnvVarGuard, TEST_PROCESS_ENV_LOCK};

fn write_executable(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).expect("write fake executable");
    let source = std::fs::metadata(path)
        .expect("stat fake executable")
        .permissions();
    crate::platform::fs::permissions::make_executable_from(path, &source)
        .expect("chmod fake executable");
}

struct DynamicEnvVarGuard {
    key: String,
    previous: Option<std::ffi::OsString>,
}

impl DynamicEnvVarGuard {
    fn remove(key: String) -> Self {
        let previous = std::env::var_os(&key);
        std::env::remove_var(&key);
        Self { key, previous }
    }
}

impl Drop for DynamicEnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(&self.key, value),
            None => std::env::remove_var(&self.key),
        }
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn managed_gnu_toolchain_is_exported_for_later_github_steps() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
        return;
    }
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());

    let (target, target_prefix, slug) = if crate::platform::host::facts::arch()
        == crate::platform::host::facts::HostArch::Aarch64
    {
        (
            "x86_64-unknown-linux-gnu",
            "x86_64-conda-linux-gnu",
            "linux-x64-gnu",
        )
    } else {
        (
            "aarch64-unknown-linux-gnu",
            "aarch64-conda-linux-gnu",
            "linux-arm64-gnu",
        )
    };
    let target_u = target.replace('-', "_");
    let target_upper = target_u.to_ascii_uppercase();
    let output_keys = [
        format!("CC_{target_u}"),
        format!("CXX_{target_u}"),
        format!("CXXSTDLIB_{target_u}"),
        "CXXSTDLIB".to_string(),
        format!("AR_{target_u}"),
        format!("RANLIB_{target_u}"),
        format!("CFLAGS_{target_u}"),
        format!("CXXFLAGS_{target_u}"),
        format!("CARGO_TARGET_{target_upper}_LINKER"),
        format!("CARGO_TARGET_{target_upper}_RUSTFLAGS"),
        "CMAKE_SYSROOT".to_string(),
        // soldr#3081: the managed sysroot is exported to the compiler and to
        // CMake, but never to pkg-config as a path-rewriting prefix.
        format!("PKG_CONFIG_ALLOW_CROSS_{target}"),
        "PKG_CONFIG_LIBDIR".to_string(),
        "SOLDR_GNU_LINUX_SYSROOT".to_string(),
        "SOLDR_GNU_LINUX_TOOLCHAIN_ROOT".to_string(),
    ];

    let dir = tempfile::tempdir().expect("tempdir");
    let fake_bin = dir.path().join("fake-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake bin");
    let fake_rustup = fake_bin.join("rustup");
    write_executable(&fake_rustup, "#!/bin/sh\nexit 0\n");

    let _rustup = EnvVarGuard::set(crate::TEST_RUSTUP_BIN_ENV_VAR, &fake_rustup);
    // soldr#2874: this fixture seeds a FAKE bundle and asserts the env
    // contract, so it must reach the catalogue path on every host. Both
    // branches above are deliberately a cross, and on a native ARM64 runner
    // the x86_64-hosted bundle genuinely cannot execute -- selection now
    // refuses it, which is the fix working. The seam says "this host is
    // pretending it can run the Linux bundle", which is exactly what a fake
    // bundle made of `#!/bin/sh` stubs is doing.
    let _cross_guard = EnvVarGuard::set("SOLDR_WINDOWS_LINUX_CROSS_GUARD", "off");
    let _no_network = EnvVarGuard::set("SOLDR_TEST_NO_NETWORK", "1");
    let _legacy_sys = EnvVarGuard::set(crate::blessed_build::USE_LEGACY_VENDORED_SYS_ENV_VAR, "1");
    let _system_cmake = EnvVarGuard::set(crate::blessed_build::USE_SYSTEM_CMAKE_ENV_VAR, "1");
    let _manifest = EnvVarGuard::set("SOLDR_MANIFEST_DISABLE", "1");
    let _path = EnvVarGuard::set("PATH", "/usr/bin:/bin");
    let _ambient_cc = EnvVarGuard::set("CC", "ambient-cc");
    let _ambient_cxx = EnvVarGuard::set("CXX", "ambient-cxx");
    let _ambient_ar = EnvVarGuard::set("AR", "ambient-ar");
    let _ambient_ranlib = EnvVarGuard::set("RANLIB", "ambient-ranlib");
    // soldr#2309: the stdlib pin is setdefault — an ambient value through any
    // spelling in cc-rs's lookup chain suppresses it, so the whole chain must
    // be cleared for the injection half of this test. Deduplicated: guarding
    // the same key twice would restore it twice, last writer wins, and the
    // second guard captured the already-removed state.
    let _output_guards: Vec<_> = output_keys
        .iter()
        .cloned()
        .chain(crate::fetch::gnu_linux_toolchain::cxx_stdlib_lookup_keys(
            target,
        ))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(DynamicEnvVarGuard::remove)
        .collect();

    let paths = crate::core::SoldrPaths::with_root(dir.path().join("soldr"));
    let bundle = paths
        .bin
        .join("syslib")
        .join("gnu-linux-toolchain")
        .join(crate::fetch::gnu_linux_toolchain::GNU_LINUX_TOOLCHAIN_VERSION)
        .join(slug);
    let package = bundle.join("package");
    let managed_bin = package.join("bin");
    let sysroot = package.join(target_prefix).join("sysroot");
    std::fs::create_dir_all(&managed_bin).expect("create managed bin");
    std::fs::create_dir_all(sysroot.join("usr/include")).expect("create sysroot includes");
    std::fs::create_dir_all(sysroot.join("usr/lib")).expect("create sysroot libraries");
    for tool in ["gcc", "g++", "ar", "ranlib", "ld", "readelf"] {
        write_executable(
            &managed_bin.join(format!("{target_prefix}-{tool}")),
            "#!/bin/sh\nexit 0\n",
        );
    }
    std::fs::write(bundle.join(".complete"), "test bundle").expect("write bundle stamp");

    let prep = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(crate::target_lifecycle::prepare_target(&paths, target, &[]))
        .expect("prepare managed GNU target");
    assert!(
        prep.path_dirs.contains(&managed_bin),
        "managed GNU bin directory missing from prepared PATH entries: {:?}",
        prep.path_dirs
    );

    let github_env = dir.path().join("github.env");
    apply_blessed_prep_env(Some(&github_env), &prep, target).expect("export prepared env");

    let process_path = std::env::split_paths(&std::env::var_os("PATH").expect("process PATH"))
        .next()
        .expect("first process PATH entry");
    assert_eq!(process_path, managed_bin);

    let exported = std::fs::read_to_string(&github_env).expect("read github env");
    let exported_path = exported
        .lines()
        .find_map(|line| line.strip_prefix("PATH="))
        .expect("PATH was not exported");
    let first_exported = std::env::split_paths(std::ffi::OsStr::new(exported_path))
        .next()
        .expect("first exported PATH entry");
    assert_eq!(first_exported, managed_bin);

    for key in &output_keys {
        let process_value = std::env::var(key).unwrap_or_else(|_| panic!("{key} not applied"));
        assert!(
            exported
                .lines()
                .any(|line| line == format!("{key}={process_value}")),
            "{key} was not exported to GITHUB_ENV"
        );
    }
    assert_eq!(
        std::env::var(format!("CC_{target_u}")).expect("C compiler"),
        managed_bin
            .join(format!("{target_prefix}-gcc"))
            .to_string_lossy()
    );
    assert_eq!(
        std::env::var("SOLDR_GNU_LINUX_SYSROOT").expect("sysroot"),
        sysroot.to_string_lossy()
    );
    // soldr#2309: both spellings of the C++ stdlib pin point at libstdc++ —
    // the only C++ runtime the catalogue GNU driver ships.
    assert_eq!(
        std::env::var(format!("CXXSTDLIB_{target_u}")).expect("target-scoped stdlib pin"),
        "stdc++"
    );
    assert_eq!(
        std::env::var("CXXSTDLIB").expect("bare stdlib pin on a Linux host"),
        "stdc++"
    );

    // Setdefault: with CXXSTDLIB now present in the process env (exported by
    // the first preparation above), a re-preparation must treat it as a
    // caller decision and inject no stdlib pin at all.
    let reprep = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(crate::target_lifecycle::prepare_target(&paths, target, &[]))
        .expect("re-prepare managed GNU target");
    assert!(
        !reprep
            .env
            .iter()
            .any(|(key, _)| key.starts_with("CXXSTDLIB")),
        "a caller-set CXXSTDLIB must suppress the pin: {:?}",
        reprep.env
    );

    for (alias, ambient) in [
        ("CC", "ambient-cc"),
        ("CXX", "ambient-cxx"),
        ("AR", "ambient-ar"),
        ("RANLIB", "ambient-ranlib"),
    ] {
        assert!(
            !exported
                .lines()
                .any(|line| line.starts_with(&format!("{alias}="))),
            "cross-target {alias} must not leak into later Cargo host builds: {exported}"
        );
        assert_eq!(
            std::env::var_os(alias).as_deref(),
            Some(std::ffi::OsStr::new(ambient)),
            "{alias} must retain its ambient value in Soldr's process"
        );
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn managed_musl_toolchain_is_exported_without_zig_or_host_tools() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
        return;
    }
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());

    let (target, target_prefix, slug) = if crate::platform::host::facts::arch()
        == crate::platform::host::facts::HostArch::Aarch64
    {
        (
            "x86_64-unknown-linux-musl",
            "x86_64-linux-musl",
            "linux-x64-musl",
        )
    } else {
        (
            "aarch64-unknown-linux-musl",
            "aarch64-linux-musl",
            "linux-arm64-musl",
        )
    };
    let target_u = target.replace('-', "_");
    let target_upper = target_u.to_ascii_uppercase();
    let output_keys = [
        format!("CC_{target_u}"),
        format!("CXX_{target_u}"),
        format!("AR_{target_u}"),
        format!("RANLIB_{target_u}"),
        format!("CFLAGS_{target_u}"),
        format!("CXXFLAGS_{target_u}"),
        format!("CARGO_TARGET_{target_upper}_LINKER"),
        format!("CARGO_TARGET_{target_upper}_RUSTFLAGS"),
        "CMAKE_SYSROOT".to_string(),
        // soldr#3081: the managed sysroot is exported to the compiler and to
        // CMake, but never to pkg-config as a path-rewriting prefix.
        format!("PKG_CONFIG_ALLOW_CROSS_{target}"),
        "PKG_CONFIG_LIBDIR".to_string(),
        "SOLDR_MUSL_LINUX_SYSROOT".to_string(),
        "SOLDR_MUSL_LINUX_TOOLCHAIN_ROOT".to_string(),
    ];

    let dir = tempfile::tempdir().expect("tempdir");
    let fake_bin = dir.path().join("fake-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake bin");
    let fake_rustup = fake_bin.join("rustup");
    write_executable(&fake_rustup, "#!/bin/sh\nexit 0\n");

    let _rustup = EnvVarGuard::set(crate::TEST_RUSTUP_BIN_ENV_VAR, &fake_rustup);
    let _no_network = EnvVarGuard::set("SOLDR_TEST_NO_NETWORK", "1");
    let _legacy_sys = EnvVarGuard::set(crate::blessed_build::USE_LEGACY_VENDORED_SYS_ENV_VAR, "1");
    let _system_cmake = EnvVarGuard::set(crate::blessed_build::USE_SYSTEM_CMAKE_ENV_VAR, "1");
    let _manifest = EnvVarGuard::set("SOLDR_MANIFEST_DISABLE", "1");
    let _path = EnvVarGuard::set("PATH", "/usr/bin:/bin");
    let _output_guards: Vec<_> = output_keys
        .iter()
        .cloned()
        .map(DynamicEnvVarGuard::remove)
        .collect();

    let paths = crate::core::SoldrPaths::with_root(dir.path().join("soldr"));
    let bundle = paths
        .bin
        .join("syslib")
        .join("musl-linux-toolchain")
        .join(crate::fetch::musl_linux_toolchain::MUSL_LINUX_TOOLCHAIN_VERSION)
        .join(slug);
    let package = bundle.join("package");
    let managed_bin = package.join("bin");
    let sysroot = package.join(target_prefix);
    std::fs::create_dir_all(&managed_bin).expect("create managed bin");
    std::fs::create_dir_all(sysroot.join("include")).expect("create musl headers");
    std::fs::create_dir_all(sysroot.join("lib")).expect("create musl libraries");
    for tool in [
        "gcc", "g++", "ar", "ranlib", "ld", "readelf", "strip", "objcopy",
    ] {
        write_executable(
            &managed_bin.join(format!("{target_prefix}-{tool}")),
            "#!/bin/sh\nexit 0\n",
        );
    }
    for runtime in [
        "crt1.o",
        "rcrt1.o",
        "crti.o",
        "crtn.o",
        "libc.a",
        "libstdc++.a",
    ] {
        std::fs::write(sysroot.join("lib").join(runtime), "managed musl runtime")
            .expect("write managed musl runtime");
    }
    std::fs::write(bundle.join(".complete"), "test bundle").expect("write bundle stamp");

    let prep = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(crate::target_lifecycle::prepare_target(&paths, target, &[]))
        .expect("prepare managed musl target");
    assert!(prep.path_dirs.contains(&managed_bin));
    assert!(
        !prep
            .path_dirs
            .iter()
            .any(|path| path.to_string_lossy().contains("zig")),
        "normal musl preparation must not add a Zig directory: {:?}",
        prep.path_dirs
    );
    let github_env = dir.path().join("github.env");
    apply_blessed_prep_env(Some(&github_env), &prep, target).expect("export prepared env");
    let exported = std::fs::read_to_string(&github_env).expect("read github env");
    for key in &output_keys {
        let process_value = std::env::var(key).unwrap_or_else(|_| panic!("{key} not applied"));
        assert!(exported
            .lines()
            .any(|line| line == format!("{key}={process_value}")));
    }
    assert_eq!(
        std::env::var(format!("CC_{target_u}")).expect("C compiler"),
        managed_bin
            .join(format!("{target_prefix}-gcc"))
            .to_string_lossy()
    );
    assert_eq!(
        std::env::var("SOLDR_MUSL_LINUX_SYSROOT").expect("sysroot"),
        sysroot.to_string_lossy()
    );
    // soldr#2309: the C++ stdlib pin is a linux-gnu concern — the musl
    // lifecycle must stay unchanged.
    assert!(
        !exported.lines().any(|line| line.starts_with("CXXSTDLIB")),
        "musl preparation must not export a C++ stdlib pin: {exported}"
    );
}

/// soldr#3246 contract: for a graph whose `links = "openssl"` provider is
/// `openssl-sys`, `soldr prepare --github-env` exports exactly these
/// target-scoped keys, and none for a graph without it:
///
/// | key | value |
/// |---|---|
/// | `<T>_OPENSSL_DIR` | `~/.soldr/bin/syslib/openssl/3.5.8/<slug>/package` |
/// | `<T>_OPENSSL_NO_VENDOR` | `1` |
/// | `<T>_OPENSSL_STATIC` | `1` |
/// | `PKG_CONFIG_PATH_<triple>` | `<package>/lib/pkgconfig` (not on MSVC) |
///
/// `<T>` is the triple uppercased with `-` → `_`, openssl-sys's own prefix.
/// No unscoped `OPENSSL_*` key is ever exported: GitHub env reaches host
/// build scripts too.
#[test]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn managed_openssl_is_exported_only_for_graphs_linking_openssl_sys() {
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let msvc = "x86_64-pc-windows-msvc";
    let gnu = "x86_64-unknown-linux-gnu";
    let _no_network = EnvVarGuard::set("SOLDR_TEST_NO_NETWORK", "1");
    let _legacy_sys = EnvVarGuard::remove(crate::blessed_build::USE_LEGACY_VENDORED_SYS_ENV_VAR);
    let _flags: Vec<_> = ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"]
        .into_iter()
        .map(EnvVarGuard::remove)
        .collect();
    let _output_guards: Vec<_> = [msvc, gnu]
        .into_iter()
        .flat_map(|triple| {
            let prefix = crate::blessed_build::openssl_env_prefix(triple);
            ["DIR", "LIB_DIR", "INCLUDE_DIR", "NO_VENDOR", "STATIC"]
                .into_iter()
                .map(move |suffix| format!("{prefix}_OPENSSL_{suffix}"))
                .chain([format!("PKG_CONFIG_PATH_{triple}")])
        })
        .chain(
            ["DIR", "LIB_DIR", "INCLUDE_DIR", "NO_VENDOR", "STATIC"]
                .into_iter()
                .map(|suffix| format!("OPENSSL_{suffix}")),
        )
        .map(DynamicEnvVarGuard::remove)
        .collect();

    let dir = tempfile::tempdir().expect("tempdir");
    let paths = crate::core::SoldrPaths::with_root(dir.path().join("soldr"));
    let seed = |slug: &str| {
        let install_root = paths
            .bin
            .join("syslib")
            .join("openssl")
            .join(crate::fetch::openssl_sysroot::MANAGED_OPENSSL_VERSION)
            .join(slug);
        let package = install_root.join("package");
        std::fs::create_dir_all(package.join("lib").join("pkgconfig")).expect("seed bundle");
        std::fs::write(install_root.join(".complete"), "test bundle").expect("seed stamp");
        package
    };
    let msvc_package = seed("windows-x64");
    let gnu_package = seed("linux-x64-gnu");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");

    // Export one graph for one target, returning the GitHub env file body.
    let export = |workspace: &str, triple: &str, packages: &str| {
        let root = dir.path().join(workspace);
        std::fs::create_dir_all(&root).expect("workspace");
        let _cwd = crate::CwdGuard::enter(&root);
        crate::blessed_build::prime_links_metadata_for_test(
            &std::env::current_dir().expect("cwd"),
            triple,
            format!("{{\"packages\":[{packages}]}}").as_bytes(),
        );
        let mut prep = BlessedPrep::default();
        runtime.block_on(crate::blessed_build::inject_sys_library_overrides(
            &paths,
            triple,
            &mut prep,
            &[],
        ));
        let github_env = root.join(format!("{triple}.env"));
        apply_blessed_prep_env(Some(&github_env), &prep, triple).expect("export prepared env");
        std::fs::read_to_string(&github_env).unwrap_or_default()
    };

    // Graphs without openssl-sys go first: a positive export sets
    // `<T>_OPENSSL_*` in this process, and soldr treats an already-set
    // `<T>_OPENSSL_DIR` as the caller's choice, which would make a later
    // negative case pass vacuously.
    for triple in [msvc, gnu] {
        let exported = export(
            &format!("no-openssl-{triple}"),
            triple,
            r#"{"name":"serde","links":null}"#,
        );
        assert!(
            !exported.contains("OPENSSL"),
            "{triple}: no openssl-sys, no OpenSSL keys: {exported}"
        );
    }

    let openssl_sys = r#"{"name":"openssl","links":null},{"name":"openssl-sys","links":"openssl"}"#;
    let msvc_exported = export("openssl-msvc", msvc, openssl_sys);
    let exported = &msvc_exported;
    let lines: Vec<&str> = exported.lines().collect();
    for expected in [
        format!(
            "X86_64_PC_WINDOWS_MSVC_OPENSSL_DIR={}",
            msvc_package.display()
        ),
        "X86_64_PC_WINDOWS_MSVC_OPENSSL_NO_VENDOR=1".to_string(),
        "X86_64_PC_WINDOWS_MSVC_OPENSSL_STATIC=1".to_string(),
    ] {
        assert!(lines.contains(&expected.as_str()), "{expected}: {exported}");
    }
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("PKG_CONFIG_PATH_x86_64-pc-windows-msvc=")),
        "openssl-sys never uses pkg-config for MSVC: {exported}"
    );

    let gnu_exported = export("openssl-gnu", gnu, openssl_sys);
    let exported = &gnu_exported;
    let lines: Vec<&str> = exported.lines().collect();
    for expected in [
        format!(
            "X86_64_UNKNOWN_LINUX_GNU_OPENSSL_DIR={}",
            gnu_package.display()
        ),
        "X86_64_UNKNOWN_LINUX_GNU_OPENSSL_NO_VENDOR=1".to_string(),
        "X86_64_UNKNOWN_LINUX_GNU_OPENSSL_STATIC=1".to_string(),
        format!(
            "PKG_CONFIG_PATH_x86_64-unknown-linux-gnu={}",
            gnu_package.join("lib").join("pkgconfig").display()
        ),
    ] {
        assert!(lines.contains(&expected.as_str()), "{expected}: {exported}");
    }

    for exported in [&msvc_exported, &gnu_exported] {
        assert!(
            !exported.lines().any(|line| line.starts_with("OPENSSL_")),
            "unscoped OPENSSL_* must never be exported: {exported}"
        );
    }
}

#[test]
fn exported_encoded_rustflags_keep_caller_target_flags() {
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let target_key = "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS";
    let _target = EnvVarGuard::set(target_key, "-C link-arg=advapi32.lib");
    let _global = EnvVarGuard::set("RUSTFLAGS", "-Dwarnings");
    let _encoded = EnvVarGuard::remove("CARGO_ENCODED_RUSTFLAGS");

    let mut prep = BlessedPrep::default();
    prep.env.push((
        target_key.to_string(),
        "-C link-arg=/LIBPATH:/soldr/sdk".to_string(),
    ));

    let dir = tempfile::tempdir().expect("tempdir");
    let github_env = dir.path().join("github.env");
    apply_blessed_prep_env(Some(&github_env), &prep, "x86_64-pc-windows-msvc")
        .expect("apply prep env");

    let exported = std::fs::read_to_string(&github_env).expect("read github env");
    let encoded_line = exported
        .lines()
        .find_map(|line| line.strip_prefix("CARGO_ENCODED_RUSTFLAGS="))
        .expect("CARGO_ENCODED_RUSTFLAGS was not exported");
    let tokens: Vec<&str> = encoded_line.split('\u{1f}').collect();

    // soldr's own required SDK flags.
    assert!(
        tokens.contains(&"link-arg=/LIBPATH:/soldr/sdk"),
        "required SDK flag missing from {tokens:?}"
    );
    // The caller's target-scoped flag, which the encoded variable would
    // otherwise shadow into oblivion.
    assert!(
        tokens.contains(&"link-arg=advapi32.lib"),
        "caller's target-scoped flag was dropped from {tokens:?}"
    );
    // And the lower-precedence global.
    assert!(
        tokens.contains(&"-Dwarnings"),
        "caller's global RUSTFLAGS was dropped from {tokens:?}"
    );
}

/// soldr#3771: the catalogue glibc 2.17 sysroot ships no libuuid, so a
/// `cc` build that includes `<uuid/uuid.h>` and emits
/// `cargo:rustc-link-lib=uuid` (obs-rust/obs-studio#516) failed for
/// `--target *-unknown-linux-gnu`. When the managed libuuid bundle is
/// materialized, preparation must layer its include and link-search
/// directories onto the same target-scoped flags that carry `--sysroot`.
#[test]
fn managed_gnu_target_layers_libuuid_onto_the_sysroot() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
        return;
    }
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (target, target_prefix, slug) = if crate::platform::host::facts::arch()
        == crate::platform::host::facts::HostArch::Aarch64
    {
        (
            "x86_64-unknown-linux-gnu",
            "x86_64-conda-linux-gnu",
            "linux-x64-gnu",
        )
    } else {
        (
            "aarch64-unknown-linux-gnu",
            "aarch64-conda-linux-gnu",
            "linux-arm64-gnu",
        )
    };
    let target_u = target.replace('-', "_");
    let rustflags_key = format!("CARGO_TARGET_{}_RUSTFLAGS", target_u.to_ascii_uppercase());

    let dir = tempfile::tempdir().expect("tempdir");
    let fake_rustup = dir.path().join("rustup");
    write_executable(&fake_rustup, "#!/bin/sh\nexit 0\n");
    let _rustup = EnvVarGuard::set(crate::TEST_RUSTUP_BIN_ENV_VAR, &fake_rustup);
    let _cross_guard = EnvVarGuard::set("SOLDR_WINDOWS_LINUX_CROSS_GUARD", "off");
    let _no_network = EnvVarGuard::set("SOLDR_TEST_NO_NETWORK", "1");
    let _legacy_sys = EnvVarGuard::set(crate::blessed_build::USE_LEGACY_VENDORED_SYS_ENV_VAR, "1");
    let _system_cmake = EnvVarGuard::set(crate::blessed_build::USE_SYSTEM_CMAKE_ENV_VAR, "1");
    let _manifest = EnvVarGuard::set("SOLDR_MANIFEST_DISABLE", "1");
    let _ambient_flags: Vec<_> = [
        "RUSTFLAGS".to_string(),
        "CARGO_ENCODED_RUSTFLAGS".to_string(),
        rustflags_key.clone(),
    ]
    .into_iter()
    .map(DynamicEnvVarGuard::remove)
    .collect();

    let paths = crate::core::SoldrPaths::with_root(dir.path().join("soldr"));
    let syslib = paths.bin.join("syslib");
    let gnu = syslib
        .join("gnu-linux-toolchain")
        .join(crate::fetch::gnu_linux_toolchain::GNU_LINUX_TOOLCHAIN_VERSION)
        .join(slug);
    let managed_bin = gnu.join("package").join("bin");
    let sysroot = gnu.join("package").join(target_prefix).join("sysroot");
    std::fs::create_dir_all(&managed_bin).expect("create managed bin");
    std::fs::create_dir_all(sysroot.join("usr/include")).expect("create sysroot includes");
    std::fs::create_dir_all(sysroot.join("usr/lib")).expect("create sysroot libraries");
    for tool in ["gcc", "g++", "ar", "ranlib", "ld", "readelf"] {
        write_executable(
            &managed_bin.join(format!("{target_prefix}-{tool}")),
            "#!/bin/sh\nexit 0\n",
        );
    }
    std::fs::write(gnu.join(".complete"), "test bundle").expect("stamp gnu bundle");

    let uuid = syslib
        .join("libuuid")
        .join(crate::fetch::libuuid_sysroot::MANAGED_LIBUUID_VERSION)
        .join(slug);
    let uuid_root = uuid.join("package");
    std::fs::create_dir_all(uuid_root.join("include/uuid")).expect("create uuid include");
    std::fs::create_dir_all(uuid_root.join("lib")).expect("create uuid lib");
    std::fs::write(uuid_root.join("include/uuid/uuid.h"), "").expect("write uuid.h");
    std::fs::write(uuid_root.join("lib/libuuid.a"), "!<arch>\n").expect("write libuuid.a");
    std::fs::write(uuid.join(".complete"), "test bundle").expect("stamp uuid bundle");

    let prep = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(crate::target_lifecycle::prepare_target(&paths, target, &[]))
        .expect("prepare managed GNU target");
    let value = |key: &str| {
        prep.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("{key} missing from {:?}", prep.env))
    };
    let include = format!("-isystem {}", uuid_root.join("include").display());
    let link_arg = format!("link-arg=-L{}", uuid_root.join("lib").display());
    for key in [format!("CFLAGS_{target_u}"), format!("CXXFLAGS_{target_u}")] {
        let flags = value(&key);
        assert!(
            flags.contains("--sysroot="),
            "{key} lost --sysroot: {flags}"
        );
        assert!(
            flags.contains(&include),
            "{key} lacks libuuid include: {flags}"
        );
    }
    let rustflags = value(&rustflags_key);
    assert!(rustflags.contains("link-arg=--sysroot="), "{rustflags}");
    // What rustc actually receives: the search path must survive encoding
    // as its own `-C link-arg=` pair, not glued onto `--sysroot`.
    let encoded = crate::target_lifecycle::encoded_rustflags_for_prep(&prep)
        .expect("prepared target rustflags are encoded");
    let tokens: Vec<_> = encoded.split('\u{1f}').collect();
    assert!(
        tokens
            .windows(2)
            .any(|pair| pair == ["-C", link_arg.as_str()]),
        "encoded rustflags lack a standalone libuuid search path: {tokens:?}"
    );
}
