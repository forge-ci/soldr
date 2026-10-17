//! Unit coverage split from `prepare_cmd.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;
use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;
use std::ffi::{OsStr, OsString};

fn reld_selection(
    choice: crate::linker::LinkerChoice,
    source: crate::linker::LinkerSource,
) -> crate::linker::ProjectLinkerSelection {
    crate::linker::ProjectLinkerSelection {
        choice,
        source,
        reld_cargo_config: None,
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

#[test]
fn ensure_selected_reld_fetches_once_when_selected() {
    let calls = std::cell::Cell::new(0);
    let selection = reld_selection(
        crate::linker::LinkerChoice::Reld,
        crate::linker::LinkerSource::CargoTomlMetadata,
    );
    let result = block_on(ensure_selected_reld_with(&selection, || {
        calls.set(calls.get() + 1);
        async { Ok(PathBuf::from("/managed/reld-0.1.0/reld")) }
    }))
    .expect("fetch ok");
    assert_eq!(calls.get(), 1);
    assert_eq!(result, Some(PathBuf::from("/managed/reld-0.1.0/reld")));
}

#[test]
fn ensure_selected_reld_skips_fetch_when_not_selected() {
    let calls = std::cell::Cell::new(0);
    let selection = reld_selection(
        crate::linker::LinkerChoice::Fast,
        crate::linker::LinkerSource::Default,
    );
    let result = block_on(ensure_selected_reld_with(&selection, || {
        calls.set(calls.get() + 1);
        async { Ok(PathBuf::from("/unused")) }
    }))
    .expect("no fetch");
    assert_eq!(calls.get(), 0);
    assert_eq!(result, None);
}

#[test]
fn ensure_selected_reld_explicit_failure_is_hard_error() {
    let selection = reld_selection(
        crate::linker::LinkerChoice::Reld,
        crate::linker::LinkerSource::Env,
    );
    let error = block_on(ensure_selected_reld_with(&selection, || async {
        Err(SoldrError::Other("offline".to_string()))
    }))
    .expect_err("explicit reld must fail loudly");
    let message = error.to_string();
    assert!(
        message.contains(crate::fetch::MANAGED_RELD_VERSION),
        "{message}"
    );
    assert!(
        message.contains(crate::fetch::RELD_BIN_ENV_VAR),
        "{message}"
    );
}

fn write_host_script(path: &Path, windows: &str, unix: &str) {
    let body =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            windows
        } else {
            unix
        };
    std::fs::write(path, body).expect("write host script");
    crate::platform::fs::permissions::make_executable(path).expect("make host script executable");
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// Scrub the ambient compiler-flag globals that `target_lifecycle::resolved_env`
/// deliberately folds into a `CARGO_TARGET_*_RUSTFLAGS` / `CFLAGS_*` / `CXXFLAGS_*`
/// key (documented there; a `CARGO_ENCODED_RUSTFLAGS` set upstream would silently
/// outrank the target flags otherwise). A test that asserts the *exact* prepared
/// flags must control these, or it reads whatever the surrounding shell set. Under
/// `cargo test` they happened to be unset; under nextest each test is its own
/// process that inherits the CI lane's env, where they are not (soldr#2521 B3).
#[must_use]
fn scrub_flag_globals() -> Vec<EnvVarGuard> {
    ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CFLAGS", "CXXFLAGS"]
        .into_iter()
        .map(EnvVarGuard::remove)
        .collect()
}

// soldr#1663 follow-up: one shared cwd guard at the crate root, for the
// same reason there is one shared env barrier -- a per-module copy makes
// each site look correct while leaving the global state unprotected.
use crate::CwdGuard;

#[test]
fn append_env_creates_file_and_appends() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let p = tmp.path().join("env");
    append_env(Some(&p), "FOO", "bar").expect("append");
    append_env(Some(&p), "BAZ", "/some/path").expect("append");
    let body = std::fs::read_to_string(&p).expect("read");
    assert!(body.contains("FOO=bar"));
    assert!(body.contains("BAZ=/some/path"));
}

#[test]
fn append_env_no_op_when_none() {
    append_env(None, "FOO", "bar").expect("no-op");
}

#[test]
fn apply_blessed_prep_env_exports_mingw_and_syslib_env() {
    let _lock = ENV_LOCK.lock();
    let _flag_globals = scrub_flag_globals();
    let _mingw = EnvVarGuard::remove("MINGW_W64_GCC_ROOT");
    let _pkg_config = EnvVarGuard::remove("PKG_CONFIG_PATH_x86_64-pc-windows-gnu");

    let tmp = tempfile::tempdir().expect("tmpdir");
    let github_env = tmp.path().join("github-env");
    let mingw_bin = tmp.path().join("mingw").join("bin");
    let pkgconfig = tmp
        .path()
        .join("syslib")
        .join("sqlite")
        .join("lib")
        .join("pkgconfig");
    let prep = crate::blessed_build::BlessedPrep {
        path_dirs: vec![mingw_bin.clone()],
        env: vec![
            (
                "MINGW_W64_GCC_ROOT".to_string(),
                tmp.path().join("mingw").to_string_lossy().into_owned(),
            ),
            (
                "PKG_CONFIG_PATH_x86_64-pc-windows-gnu".to_string(),
                pkgconfig.to_string_lossy().into_owned(),
            ),
        ],
        ..Default::default()
    };
    apply_blessed_prep_env(Some(&github_env), &prep, "x86_64-pc-windows-gnu")
        .expect("apply prep env");
    assert_eq!(
        std::env::var("MINGW_W64_GCC_ROOT").expect("mingw env"),
        tmp.path().join("mingw").to_string_lossy()
    );
    assert_eq!(
        std::env::var("PKG_CONFIG_PATH_x86_64-pc-windows-gnu").expect("pkg-config env"),
        pkgconfig.to_string_lossy()
    );

    let body = std::fs::read_to_string(&github_env).expect("read github env");
    assert!(body.contains("MINGW_W64_GCC_ROOT="));
    assert!(body.contains("PKG_CONFIG_PATH_x86_64-pc-windows-gnu="));
    assert!(body.contains(&format!("PATH={}", mingw_bin.to_string_lossy())));
}

#[test]
fn apply_blessed_prep_env_exports_msvc_target_env() {
    let _lock = ENV_LOCK.lock();
    let _flag_globals = scrub_flag_globals();
    let _cc = EnvVarGuard::remove("CC_x86_64_pc_windows_msvc");
    let _cxx = EnvVarGuard::remove("CXX_x86_64_pc_windows_msvc");
    let _ar = EnvVarGuard::remove("AR_x86_64_pc_windows_msvc");
    let _linker = EnvVarGuard::remove("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER");
    let _rustflags = EnvVarGuard::remove("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS");
    let _xwin = EnvVarGuard::remove(crate::fetch::xwin_cache::XWIN_CACHE_DIR_ENV_VAR);
    let _path = EnvVarGuard::set("PATH", "");

    let tmp = tempfile::tempdir().expect("tmpdir");
    let github_env = tmp.path().join("github-env");
    let shim_dir = tmp.path().join("clang-shim");
    let llvm_bin = tmp.path().join("llvm").join("bin");
    let xwin_dir = tmp.path().join("xwin");
    let libpath = xwin_dir.join("sdk").join("lib").join("um").join("x64");
    let rustflags = format!("-C link-arg=/LIBPATH:{}", libpath.display());
    let prep = crate::blessed_build::BlessedPrep {
        shim_path_dir: Some(shim_dir.clone()),
        path_dirs: vec![llvm_bin.clone()],
        env: vec![
            ("CC_x86_64_pc_windows_msvc".to_string(), "clang".to_string()),
            (
                "CXX_x86_64_pc_windows_msvc".to_string(),
                "clang".to_string(),
            ),
            (
                "AR_x86_64_pc_windows_msvc".to_string(),
                "llvm-lib".to_string(),
            ),
            (
                "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER".to_string(),
                "lld-link".to_string(),
            ),
            (
                crate::fetch::xwin_cache::XWIN_CACHE_DIR_ENV_VAR.to_string(),
                xwin_dir.to_string_lossy().into_owned(),
            ),
            (
                "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS".to_string(),
                rustflags.clone(),
            ),
        ],
        ..Default::default()
    };
    apply_blessed_prep_env(Some(&github_env), &prep, "x86_64-pc-windows-msvc")
        .expect("apply prep env");
    assert_eq!(
        std::env::var("CC_x86_64_pc_windows_msvc").expect("cc env"),
        "clang"
    );
    assert_eq!(
        std::env::var("AR_x86_64_pc_windows_msvc").expect("ar env"),
        "llvm-lib"
    );
    assert_eq!(
        std::env::var("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER").expect("linker env"),
        "lld-link"
    );
    assert_eq!(
        std::env::var(crate::fetch::xwin_cache::XWIN_CACHE_DIR_ENV_VAR).expect("xwin env"),
        xwin_dir.to_string_lossy()
    );
    assert_eq!(
        std::env::var("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS").expect("rustflags env"),
        rustflags
    );

    let path = std::env::var_os("PATH").expect("path env");
    let path_dirs = std::env::split_paths(&path).collect::<Vec<_>>();
    assert_eq!(path_dirs[0], shim_dir);
    assert_eq!(path_dirs[1], llvm_bin);

    let body = std::fs::read_to_string(&github_env).expect("read github env");
    assert!(body.contains("CC_x86_64_pc_windows_msvc=clang"));
    assert!(body.contains("AR_x86_64_pc_windows_msvc=llvm-lib"));
    assert!(body.contains("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER=lld-link"));
    assert!(body.contains("XWIN_CACHE_DIR="));
    assert!(body.contains("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="));
    assert!(body.contains("PATH="));
}

#[test]
fn darwin_prepare_exports_blessed_env_for_deferred_cook() {
    let _lock = ENV_LOCK.lock();
    let _flag_globals = scrub_flag_globals();
    let tmp = tempfile::tempdir().expect("tmpdir");
    let soldr_root = tmp.path().join("soldr-root");
    let github_env = tmp.path().join("github-env");
    let sdk = tmp.path().join("MacOSX.fake.sdk");
    let llvm_bin = tmp.path().join("llvm").join("bin");
    let fake_dsymutil = llvm_bin.join(
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            "dsymutil.exe"
        } else {
            "dsymutil"
        },
    );
    let fake_zig_dir = tmp.path().join("zig-bin");
    let fake_zig = fake_zig_dir.join(
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            "zig.exe"
        } else {
            "zig"
        },
    );
    let fake_rustup = tmp.path().join(
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            "rustup.cmd"
        } else {
            "rustup"
        },
    );

    std::fs::create_dir_all(&sdk).expect("mkdir sdk");
    std::fs::create_dir_all(&llvm_bin).expect("mkdir llvm");
    std::fs::create_dir_all(&fake_zig_dir).expect("mkdir zig");
    std::fs::write(&fake_dsymutil, b"fake dsymutil").expect("write fake dsymutil");
    std::fs::write(&fake_zig, b"fake zig").expect("write fake zig");

    write_host_script(
        &fake_rustup,
        "@echo off\r\nexit /b 0\r\n",
        "#!/bin/sh\nexit 0\n",
    );

    let _root = EnvVarGuard::set(crate::core::SOLDR_CACHE_DIR_ENV_VAR, &soldr_root);
    let _rustup = EnvVarGuard::set(crate::TEST_RUSTUP_BIN_ENV_VAR, &fake_rustup);
    let _zig = EnvVarGuard::set("ZIG", &fake_zig);
    let _sdkroot = EnvVarGuard::set("SDKROOT", &sdk);
    let _llvm = EnvVarGuard::set("SOLDR_LLVM_DIR", &llvm_bin);
    let _dsymutil = EnvVarGuard::set("SOLDR_DSYMUTIL", &fake_dsymutil);
    let _legacy_sys = EnvVarGuard::set(crate::blessed_build::USE_LEGACY_VENDORED_SYS_ENV_VAR, "1");
    let _system_cmake = EnvVarGuard::set(crate::blessed_build::USE_SYSTEM_CMAKE_ENV_VAR, "1");

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(run(
            "x86_64-apple-darwin".to_string(),
            Some(github_env.clone()),
            None,
            None,
        ))
        .expect("prepare darwin");

    let body = std::fs::read_to_string(&github_env).expect("read github env");
    assert!(body.contains("SDKROOT="), "SDKROOT missing: {body}");
    assert!(
        body.contains("CC_x86_64_apple_darwin=clang --target=x86_64-apple-darwin"),
        "darwin CC env missing blessed clang target: {body}"
    );
    assert!(
        body.contains("CFLAGS_x86_64_apple_darwin=--target=x86_64-apple-darwin"),
        "darwin CFLAGS must target darwin: {body}"
    );
    // soldr#3776: lld is selected link-only through a clang config file, never
    // as a raw `-fuse-ld=lld` that warns on every `-c` compile under -Werror.
    for key in [
        "CFLAGS_x86_64_apple_darwin=",
        "CXXFLAGS_x86_64_apple_darwin=",
    ] {
        let line = body
            .lines()
            .find(|line| line.starts_with(key))
            .unwrap_or_else(|| panic!("{key} missing: {body}"));
        assert!(!line.contains("-fuse-ld"), "{line}");
        assert!(line.contains("--config="), "{line}");
    }
    assert!(
        body.lines().any(
            |l| l.starts_with("CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS=")
                && l.contains("-fuse-ld=lld")
        ),
        "Rust links still select lld: {body}"
    );
    assert!(
        body.contains("CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER=clang"),
        "darwin linker env missing: {body}"
    );
    assert!(
        body.contains("CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS=")
            && body.contains("-mmacosx-version-min=10.12"),
        "x86_64 darwin rustflags: SDK/link args at the 10.12 floor: {body}"
    );
    assert!(
        body.contains("PATH=") && body.contains(&llvm_bin.to_string_lossy().to_string()),
        "managed LLVM bin dir must be exported on PATH: {body}"
    );
}

#[test]
fn xwin_cache_case_aliases_mixed_case_sdk_files() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let xwin = tmp.path().join("xwin");
    let lib = xwin.join("sdk").join("lib").join("um").join("x86_64");
    let include = xwin.join("sdk").join("include").join("um");
    std::fs::create_dir_all(&lib).expect("mkdir lib");
    std::fs::create_dir_all(&include).expect("mkdir include");
    std::fs::write(lib.join("Kernel32.Lib"), b"kernel32").expect("write kernel32");
    std::fs::write(lib.join("UserEnv.Lib"), b"userenv").expect("write userenv");
    std::fs::write(include.join("Windows.h"), b"windows").expect("write windows.h");

    // soldr#1229 — probe filesystem case-sensitivity. macOS APFS
    // is case-insensitive by default: `Kernel32.Lib` and
    // `kernel32.lib` resolve to the same inode, so
    // `ensure_lowercase_file_aliases`'s `alias.exists()` guard
    // trips and no aliases are created. That's CORRECT behavior
    // (aliases aren't needed on case-insensitive FS) — the test
    // just needs to adjust its expectation.
    let probe = tmp.path().join("CaseProbe");
    std::fs::write(&probe, b"").expect("write case probe");
    let case_insensitive = tmp.path().join("caseprobe").exists();
    std::fs::remove_file(&probe).ok();

    let created = ensure_xwin_case_aliases(&xwin).expect("aliases");
    let expected_created = if crate::platform::host::facts::os()
        == crate::platform::host::facts::HostOs::Windows
        || case_insensitive
    {
        0
    } else {
        3
    };
    assert_eq!(created, expected_created);
    // These assertions pass on both case-sensitive (real aliases
    // created) and case-insensitive (same file resolvable under any
    // case) filesystems.
    assert!(lib.join("kernel32.lib").is_file());
    assert!(lib.join("userenv.lib").is_file());
    assert!(include.join("windows.h").is_file());

    let created_again = ensure_xwin_case_aliases(&xwin).expect("aliases are idempotent");
    assert_eq!(created_again, 0);
}

#[test]
fn rustup_add_target_uses_soldr_managed_rustup_state() {
    let _lock = ENV_LOCK.lock();
    let tmp = tempfile::tempdir().expect("tmpdir");
    let soldr_root = tmp.path().join("soldr-root");
    let log = tmp.path().join("rustup.log");
    let fake_rustup = tmp.path().join(
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            "rustup.cmd"
        } else {
            "rustup"
        },
    );

    write_host_script(
        &fake_rustup,
        "@echo off\r\n\
             (\r\n\
             echo args=%*\r\n\
             echo CARGO_HOME=%CARGO_HOME%\r\n\
             echo RUSTUP_HOME=%RUSTUP_HOME%\r\n\
             ) >> \"%SOLDR_RUSTUP_LOG%\"\r\n",
        "#!/bin/sh\n\
                 {\n\
                   printf 'args=%s\\n' \"$*\"\n\
                   printf 'CARGO_HOME=%s\\n' \"$CARGO_HOME\"\n\
                   printf 'RUSTUP_HOME=%s\\n' \"$RUSTUP_HOME\"\n\
                 } >> \"$SOLDR_RUSTUP_LOG\"\n",
    );

    let _rustup = EnvVarGuard::set(crate::TEST_RUSTUP_BIN_ENV_VAR, &fake_rustup);
    let _root = EnvVarGuard::set(crate::core::SOLDR_CACHE_DIR_ENV_VAR, &soldr_root);
    let _log = EnvVarGuard::set("SOLDR_RUSTUP_LOG", &log);
    let _cargo_home = EnvVarGuard::remove(crate::core::CARGO_HOME_ENV_VAR);
    let _rustup_home = EnvVarGuard::remove(crate::core::RUSTUP_HOME_ENV_VAR);

    // A triple that is never a CI host: the soldr#2613 host short-circuit
    // must not skip the spawn these assertions observe.
    rustup_add_target("x86_64-pc-windows-gnu").expect("rustup target add");

    let body = std::fs::read_to_string(&log).expect("read fake rustup log");
    assert!(
        body.contains("args=target add x86_64-pc-windows-gnu"),
        "fake rustup should receive target add args, got: {body}"
    );
    assert!(
        body.contains(&format!(
            "CARGO_HOME={}",
            crate::fetch::managed_cargo_home(&SoldrPaths::with_root(soldr_root.clone())).display()
        )),
        "fake rustup should receive managed CARGO_HOME, got: {body}"
    );
    assert!(
        body.contains(&format!(
            "RUSTUP_HOME={}",
            crate::fetch::managed_rustup_home(&SoldrPaths::with_root(soldr_root)).display()
        )),
        "fake rustup should receive managed RUSTUP_HOME, got: {body}"
    );

    let explicit_cargo = tmp.path().join("action-cargo");
    let explicit_toolchain = tmp.path().join("action-toolchain");
    {
        let _cargo_home = EnvVarGuard::set(crate::core::CARGO_HOME_ENV_VAR, &explicit_cargo);
        let _toolchain_home =
            EnvVarGuard::set(crate::core::RUSTUP_HOME_ENV_VAR, &explicit_toolchain);
        rustup_add_target("i686-pc-windows-gnu").expect("explicit target add");
    }
    let body = std::fs::read_to_string(&log).expect("read explicit toolchain log");
    assert!(body.contains(&format!("CARGO_HOME={}", explicit_cargo.display())));
    assert!(body.contains(&format!("RUSTUP_HOME={}", explicit_toolchain.display())));
}

#[test]
fn rustup_add_target_scopes_to_pinned_toolchain_channel() {
    let _lock = ENV_LOCK.lock();
    let tmp = tempfile::tempdir().expect("tmpdir");
    let soldr_root = tmp.path().join("soldr-root");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::write(
        project.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.94.1\"\n",
    )
    .expect("write toolchain");
    let log = tmp.path().join("rustup.log");
    let fake_rustup = tmp.path().join(
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            "rustup.cmd"
        } else {
            "rustup"
        },
    );

    write_host_script(
        &fake_rustup,
        "@echo off\r\n\
             echo args=%* >> \"%SOLDR_RUSTUP_LOG%\"\r\n",
        "#!/bin/sh\n\
                 printf 'args=%s\\n' \"$*\" >> \"$SOLDR_RUSTUP_LOG\"\n",
    );

    let _cwd_guard = CwdGuard::enter(&project);
    let _rustup = EnvVarGuard::set(crate::TEST_RUSTUP_BIN_ENV_VAR, &fake_rustup);
    let _root = EnvVarGuard::set(crate::core::SOLDR_CACHE_DIR_ENV_VAR, &soldr_root);
    let _log = EnvVarGuard::set("SOLDR_RUSTUP_LOG", &log);

    rustup_add_target("x86_64-pc-windows-gnu").expect("rustup target add");

    let body = std::fs::read_to_string(&log).expect("read fake rustup log");
    assert!(
        body.contains("args=target add x86_64-pc-windows-gnu --toolchain 1.94.1"),
        "fake rustup should receive pinned toolchain args, got: {body}"
    );
}

#[test]
fn rustup_target_add_timeout_is_an_explicit_safety_ceiling() {
    let _lock = ENV_LOCK.lock();
    {
        let _guard = EnvVarGuard::set(RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR, "19");
        assert_eq!(
            InstallerWatchdogConfig::from_env(RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR).safety_timeout,
            Duration::from_secs(19)
        );
    }
    for value in ["", "0", "-1", "abc"] {
        let _guard = EnvVarGuard::set(RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR, value);
        assert_eq!(
            InstallerWatchdogConfig::from_env(RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR).safety_timeout,
            Duration::from_secs(crate::core::DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS),
            "invalid override {value:?} should use default"
        );
    }
    let _guard = EnvVarGuard::remove(RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR);
    assert_eq!(
        InstallerWatchdogConfig::from_env(RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR).safety_timeout,
        Duration::from_secs(crate::core::DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS)
    );
}

#[test]
fn parse_target_arg_all_is_sentinel() {
    assert_eq!(parse_target_arg("all").unwrap(), ParsedTargetArg::All);
}

#[test]
fn parse_target_arg_single_triple() {
    let got = parse_target_arg("x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(
        got,
        ParsedTargetArg::Explicit(vec!["x86_64-unknown-linux-gnu".into()])
    );
}

#[test]
fn parse_target_arg_comma_separated() {
    let got =
        parse_target_arg("x86_64-pc-windows-msvc,aarch64-apple-darwin,x86_64-unknown-linux-musl")
            .unwrap();
    assert_eq!(
        got,
        ParsedTargetArg::Explicit(vec![
            "x86_64-pc-windows-msvc".into(),
            "aarch64-apple-darwin".into(),
            "x86_64-unknown-linux-musl".into(),
        ])
    );
}

#[test]
fn parse_target_arg_trims_whitespace() {
    let got = parse_target_arg(" x86_64-pc-windows-msvc , aarch64-apple-darwin ").unwrap();
    assert_eq!(
        got,
        ParsedTargetArg::Explicit(vec![
            "x86_64-pc-windows-msvc".into(),
            "aarch64-apple-darwin".into(),
        ])
    );
}

#[test]
fn parse_target_arg_drops_empty_entries() {
    // Leading / trailing / consecutive commas are silently dropped
    // because they're a common copy-paste mistake. The error path
    // covers the "every entry was empty" case below.
    let got = parse_target_arg(",x86_64-pc-windows-msvc,,aarch64-apple-darwin,").unwrap();
    assert_eq!(
        got,
        ParsedTargetArg::Explicit(vec![
            "x86_64-pc-windows-msvc".into(),
            "aarch64-apple-darwin".into(),
        ])
    );
}

#[test]
fn parse_target_arg_all_empty_errors() {
    let err = parse_target_arg(", , ,").unwrap_err();
    assert!(
        err.to_string().contains("comma-separated list was empty"),
        "unexpected error: {err}"
    );
}

#[test]
fn classify_target_windows_msvc() {
    let attrs = classify_target("x86_64-pc-windows-msvc").expect("classify");
    assert_eq!(attrs.arch, TargetArch::X86_64);
    assert_eq!(attrs.os, TargetOs::Windows);
    assert_eq!(attrs.abi, Some(TargetAbi::Msvc));
    assert!(attrs.needs_xwin_cache);
    assert!(attrs.needs_llvm_toolchain);
    assert!(!attrs.needs_mingw_w64_gcc);
    assert!(!attrs.needs_zig);
    assert!(!attrs.needs_apple_sdk);

    let arm = classify_target("aarch64-pc-windows-msvc").expect("classify arm");
    assert_eq!(arm.arch, TargetArch::Aarch64);
    assert_eq!(arm.os, TargetOs::Windows);
}

#[test]
fn classify_target_apple_darwin() {
    let attrs = classify_target("aarch64-apple-darwin").expect("classify");
    assert_eq!(attrs.arch, TargetArch::Aarch64);
    assert_eq!(attrs.os, TargetOs::Darwin);
    assert_eq!(attrs.abi, None);
    assert!(attrs.needs_zig);
    assert!(attrs.needs_apple_sdk);
    assert!(!attrs.needs_xwin_cache);
    assert!(!attrs.needs_llvm_toolchain);

    let intel = classify_target("x86_64-apple-darwin").expect("classify intel");
    assert_eq!(intel.arch, TargetArch::X86_64);
}

#[test]
fn classify_target_linux_gnu_and_musl() {
    let gnu = classify_target("x86_64-unknown-linux-gnu").expect("classify gnu");
    assert_eq!(gnu.os, TargetOs::Linux);
    assert_eq!(gnu.abi, Some(TargetAbi::Gnu));
    assert!(!gnu.needs_zig, "GNU uses the catalogue-backed toolchain");
    assert!(!gnu.needs_xwin_cache);
    assert!(!gnu.needs_apple_sdk);

    let musl = classify_target("aarch64-unknown-linux-musl").expect("classify musl");
    assert_eq!(musl.os, TargetOs::Linux);
    assert_eq!(musl.abi, Some(TargetAbi::Musl));
    assert!(
        !musl.needs_zig,
        "#2244 makes the catalogue-backed musl lifecycle the normal path"
    );
}

#[test]
fn classify_target_rejects_unknown_arch() {
    let err = classify_target("riscv64-unknown-linux-gnu").expect_err("riscv unsupported");
    assert!(
        err.to_string().contains("did not match any known arch"),
        "msg: {err}"
    );
}

#[test]
fn classify_target_rejects_unknown_os() {
    // freebsd has no abi suffix so the triple is 3 parts; the os
    // slot ("freebsd") doesn't score above threshold against any
    // KNOWN_OSES entry.
    let err = classify_target("x86_64-unknown-freebsd").expect_err("freebsd unsupported");
    assert!(
        err.to_string().contains("did not match any known os"),
        "msg: {err}"
    );
}

#[test]
fn classify_target_rejects_malformed_triple() {
    let err = classify_target("x86_64").expect_err("too few parts");
    assert!(err.to_string().contains("unrecognized target triple shape"));
    let err = classify_target("a-b-c-d-e").expect_err("too many parts");
    assert!(err.to_string().contains("unrecognized target triple shape"));
}

#[test]
fn classify_target_windows_gnu_x64() {
    let attrs = classify_target("x86_64-pc-windows-gnu").expect("classify mingw");
    assert_eq!(attrs.arch, TargetArch::X86_64);
    assert_eq!(attrs.os, TargetOs::Windows);
    assert_eq!(attrs.abi, Some(TargetAbi::Gnu));
    assert!(attrs.needs_mingw_w64_gcc);
    assert!(!attrs.needs_xwin_cache);
    assert!(!attrs.needs_llvm_toolchain);
    assert!(!attrs.needs_zig);
    assert!(!attrs.needs_apple_sdk);
}

#[test]
fn classify_target_rejects_non_x64_windows_gnu_scope() {
    let err = classify_target("aarch64-pc-windows-gnu").expect_err("non-x64 gnu out of scope");
    assert!(
        err.to_string().contains("only x86_64-pc-windows-gnu"),
        "msg: {err}"
    );

    let err = classify_target("x86_64-pc-windows-gnullvm").expect_err("gnullvm out of scope");
    assert!(
        err.to_string().contains("did not match any known abi"),
        "msg: {err}"
    );
}

// ---- Fuzzy-matching behavior ----

#[test]
fn fuzzy_exact_match_scores_one() {
    assert_eq!(fuzzy_score("x86_64", "x86_64"), 1.0);
    assert_eq!(fuzzy_score("linux", "linux"), 1.0);
    // Case-insensitive exact = 0.99 — still cleanly above threshold.
    let case = fuzzy_score("LINUX", "linux");
    assert!(
        case > FUZZY_MATCH_THRESHOLD,
        "case-insensitive score={case}"
    );
}

#[test]
fn fuzzy_best_match_prefers_exact_over_prefix() {
    // The user's example: input "x86_AMD"; registry has both "x86"
    // and "x86_AMD". Exact must beat prefix.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Tag {
        Short,
        AmdLong,
    }
    let registry: &[(&str, Tag)] = &[("x86", Tag::Short), ("x86_AMD", Tag::AmdLong)];
    let picked = best_match("x86_AMD", registry, "test").expect("matches");
    assert_eq!(picked, Tag::AmdLong);

    // And the inverse: input "x86" picks the short entry.
    let picked = best_match("x86", registry, "test").expect("matches");
    assert_eq!(picked, Tag::Short);
}

#[test]
fn fuzzy_rejects_below_threshold() {
    // "x86" against the real registry (only "x86_64", "aarch64")
    // scores ~0.65 against x86_64 — below 0.85, so rejected. This
    // is the safety property: typos and abbreviations don't
    // silently route to the wrong target.
    let err = best_match("x86", KNOWN_ARCHES, "arch").expect_err("rejected");
    let msg = err.to_string();
    assert!(msg.contains("did not match"), "msg: {msg}");
    assert!(
        msg.contains("x86_64"),
        "must name closest candidate; got: {msg}"
    );
}

#[test]
fn fuzzy_case_insensitive_classify() {
    // Uppercase triple components classify the same as lowercase.
    let attrs = classify_target("X86_64-PC-Windows-MSVC").expect("case-insensitive");
    assert_eq!(attrs.arch, TargetArch::X86_64);
    assert_eq!(attrs.os, TargetOs::Windows);
    assert_eq!(attrs.abi, Some(TargetAbi::Msvc));
}

#[test]
fn soldr_workspace_metadata_dogfood() {
    let _env_guard = ENV_LOCK.lock();
    // Regression guard: soldr's own workspace `Cargo.toml`
    // declares `[workspace.metadata.soldr].targets` (RFC #914).
    // Every entry must classify cleanly via the fuzzy classifier
    // — typos in soldr's own manifest fail at test time, not
    // mid-CI when `soldr prepare --target all` blows up.
    //
    let manifest = std::env::var_os("SOLDR_TEST_WORKSPACE_ROOT")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::current_dir().ok().and_then(|current_dir| {
                current_dir
                    .ancestors()
                    .find(|ancestor| {
                        ancestor.join("Cargo.toml").is_file()
                            && ancestor.join("crates/soldr-cli/Cargo.toml").is_file()
                    })
                    .map(std::path::Path::to_path_buf)
            })
        })
        .expect("run from the soldr workspace or set SOLDR_TEST_WORKSPACE_ROOT")
        .join("Cargo.toml");
    assert!(manifest.is_file(), "workspace manifest at {manifest:?}");
    let meta = crate::cargo_metadata_soldr::read_soldr_metadata(&manifest)
        .expect("parse soldr Cargo.toml");
    assert!(
        !meta.targets.is_empty(),
        "soldr's own [workspace.metadata.soldr].targets is empty — regression"
    );
    for triple in &meta.targets {
        classify_target(triple)
            .unwrap_or_else(|e| panic!("triple `{triple}` in soldr Cargo.toml: {e}"));
    }
}

// ---- Corpus test ----
//
// `triple_corpus.txt` is the canonical `rustc --print target-list`
// (snapshot taken on 2026-06-22, 308 entries) augmented with
// extra real-world triples scraped from FastLED + zackees repos.
// The rustc list is *the* answer to "what are the common target
// triples across the Rust ecosystem" — every Rust toolchain
// recognizes exactly this set.
//
// For each triple we assert:
//   - If it's in soldr's supported subset ({x86_64, aarch64} ×
//     {pc-windows-msvc, apple-darwin, unknown-linux-gnu,
//     unknown-linux-musl}) → classify_target returns Ok with the
//     expected attrs.
//   - Otherwise → returns Err. This protects against the fuzzy
//     matcher silently routing some `wasm32-...` or
//     `riscv64gc-...` to one of the supported arms.

fn is_soldr_supported(triple: &str) -> bool {
    matches!(
        triple,
        "x86_64-pc-windows-msvc"
            | "aarch64-pc-windows-msvc"
            | "x86_64-pc-windows-gnu"
            | "x86_64-apple-darwin"
            | "aarch64-apple-darwin"
            | "x86_64-unknown-linux-gnu"
            | "aarch64-unknown-linux-gnu"
            | "x86_64-unknown-linux-musl"
            | "aarch64-unknown-linux-musl"
    )
}

#[test]
fn classifier_against_rustc_target_list() {
    let corpus = include_str!("triple_corpus.txt");
    let triples: Vec<&str> = corpus
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert!(
        triples.len() >= 300,
        "corpus shrunk unexpectedly: {} entries",
        triples.len()
    );

    let mut supported_ok = 0;
    let mut unsupported_rejected = 0;
    let mut surprises: Vec<String> = Vec::new();

    for triple in &triples {
        let result = classify_target(triple);
        let expected_ok = is_soldr_supported(triple);
        match (expected_ok, &result) {
            (true, Ok(attrs)) => {
                // Spot-check that the fuzzy matcher picked the
                // right enum variants, not just *some* variant.
                if triple.starts_with("x86_64-") {
                    assert_eq!(attrs.arch, TargetArch::X86_64, "{triple}");
                } else {
                    assert_eq!(attrs.arch, TargetArch::Aarch64, "{triple}");
                }
                if triple.contains("-windows-") {
                    assert_eq!(attrs.os, TargetOs::Windows, "{triple}");
                    let expected_abi = if triple.ends_with("-gnu") {
                        TargetAbi::Gnu
                    } else {
                        TargetAbi::Msvc
                    };
                    assert_eq!(attrs.abi, Some(expected_abi), "{triple}");
                } else if triple.contains("-darwin") {
                    assert_eq!(attrs.os, TargetOs::Darwin, "{triple}");
                    assert_eq!(attrs.abi, None, "{triple}");
                } else if triple.ends_with("-gnu") {
                    assert_eq!(attrs.os, TargetOs::Linux, "{triple}");
                    assert_eq!(attrs.abi, Some(TargetAbi::Gnu), "{triple}");
                } else if triple.ends_with("-musl") {
                    assert_eq!(attrs.os, TargetOs::Linux, "{triple}");
                    assert_eq!(attrs.abi, Some(TargetAbi::Musl), "{triple}");
                }
                supported_ok += 1;
            }
            (false, Err(_)) => {
                unsupported_rejected += 1;
            }
            (true, Err(e)) => {
                surprises.push(format!("FALSE NEGATIVE `{triple}` → Err: {e}"));
            }
            (false, Ok(attrs)) => {
                surprises.push(format!(
                    "FALSE POSITIVE `{triple}` → Ok({:?}/{:?}/{:?})",
                    attrs.arch, attrs.os, attrs.abi
                ));
            }
        }
    }

    eprintln!(
        "corpus: {} triples; {} soldr-supported classify Ok; {} unsupported correctly rejected",
        triples.len(),
        supported_ok,
        unsupported_rejected
    );
    if !surprises.is_empty() {
        for s in &surprises {
            eprintln!("  {s}");
        }
        panic!("{} corpus surprise(s) — see stderr above", surprises.len());
    }
    // Sanity: confirm we actually exercised the supported set.
    assert_eq!(
        supported_ok, 9,
        "expected all 9 soldr-supported triples to classify Ok"
    );
}

/// soldr#2612: adding the host triple as a target must be a no-op — on a
/// musl host rustup hard-fails with a missing-manifest error for the
/// musl-hosted toolchain, and the host std is already installed anyway.
/// The proof is structural: this call returns Ok without resolving
/// rustup, paths, or a pinned channel (any of which would error or spawn
/// in this bare test environment).
#[test]
fn host_triple_target_add_is_a_no_op() {
    rustup_add_target(crate::pyo3_detect::host_triple()).expect("host triple must short-circuit");
}
