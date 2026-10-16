use super::*;
use crate::{EnvVarGuard, TEST_PROCESS_ENV_LOCK};

fn files_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// A fake LLVM `bin` dir. `SOLDR_LLVM_DIR` makes `ensure_llvm_toolchain`
/// return it without any network, which is the stub fetcher: a real fetch
/// would have to leave files under the soldr root, and the tests assert none
/// appear.
fn fake_llvm_bin() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("llvm dir");
    let clang = dir
        .path()
        .join(format!("clang{}", std::env::consts::EXE_SUFFIX));
    std::fs::write(&clang, "#!/bin/sh\nexit 0\n").expect("write clang");
    crate::platform::fs::permissions::make_executable(&clang).expect("chmod");
    dir
}

/// Remove the per-target linker/rustflags variables an outer `soldr cargo`
/// (CI, bosn) exports to the test process: the front door leaves an existing
/// non-empty value alone, which would hide the injection under test.
struct TargetEnvScrub(Vec<(std::ffi::OsString, std::ffi::OsString)>);

impl TargetEnvScrub {
    fn new() -> Self {
        let saved: Vec<_> = std::env::vars_os()
            .filter(|(key, _)| {
                let key = key.to_string_lossy();
                key.starts_with("CARGO_TARGET_")
                    && (key.ends_with("_LINKER") || key.ends_with("_RUSTFLAGS"))
            })
            .collect();
        for (key, _) in &saved {
            std::env::remove_var(key);
        }
        Self(saved)
    }
}

impl Drop for TargetEnvScrub {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            std::env::set_var(key, value);
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("runtime")
}

#[test]
fn provisioning_uses_exactly_build_release_args() {
    // Both front-door gates depend on args; empty args silently skip cmake.
    assert_eq!(provisioning_args(), vec!["build", "--release"]);
    assert!(host_tooling::should_inject_native_cmake(
        &provisioning_args()
    ));
}

#[test]
fn prepare_then_front_door_agree_and_the_build_adds_nothing() {
    let _env = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let llvm = fake_llvm_bin();
    let _llvm = EnvVarGuard::set("SOLDR_LLVM_DIR", llvm.path());
    let _cmake = EnvVarGuard::set(crate::blessed_build::USE_SYSTEM_CMAKE_ENV_VAR, "1");
    let _linker = EnvVarGuard::remove("SOLDR_LINKER");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    // Force the managed-LLVM path even where rustc's bundled lld would apply.
    let _flags = EnvVarGuard::set("RUSTFLAGS", "-Clink-self-contained=-linker");
    let _scrub = TargetEnvScrub::new();

    let root = tempfile::tempdir().expect("root");
    let paths = SoldrPaths::with_root(root.path().to_path_buf());

    let summary = runtime()
        .block_on(provision_native_build_tools(&paths))
        .expect("provision");
    let after_prepare = files_under(root.path());

    // The front door, with the same env, resolves the same linker...
    let mut command = std::process::Command::new("cargo");
    target::apply_linker_override_blocking(&mut command, &provisioning_args(), None, &paths)
        .expect("front door");
    let injected = command.get_envs().find_map(|(key, value)| {
        key.to_string_lossy()
            .ends_with("_LINKER")
            .then(|| value.map(|v| v.to_string_lossy().into_owned()))
            .flatten()
    });
    // The front door may name the shim relative to PATH (`linker_value_for_command`);
    // the content-addressed file name is the identity both must agree on.
    let shim_name = |value: Option<String>| {
        value.map(|value| {
            std::path::Path::new(&value)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or(value)
        })
    };
    let prepared = summary
        .linker
        .as_ref()
        .and_then(|linker| linker.driver.clone());
    assert_eq!(
        shim_name(prepared),
        shim_name(injected),
        "prepare and build disagree"
    );
    // ...and adds nothing under the root: no second fetch, no new shim.
    assert_eq!(files_under(root.path()), after_prepare);

    // Idempotent: a second prepare changes nothing either.
    let again = runtime()
        .block_on(provision_native_build_tools(&paths))
        .expect("second provision");
    assert_eq!(again, summary);
    assert_eq!(files_under(root.path()), after_prepare);
}

#[test]
fn nothing_provisioned_is_reported_explicitly() {
    let _env = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _cmake = EnvVarGuard::set(crate::blessed_build::USE_SYSTEM_CMAKE_ENV_VAR, "1");
    let _linker = EnvVarGuard::set("SOLDR_LINKER", "default");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");

    let root = tempfile::tempdir().expect("root");
    let paths = SoldrPaths::with_root(root.path().to_path_buf());
    let summary = runtime()
        .block_on(provision_native_build_tools(&paths))
        .expect("provision");

    assert!(!summary.provisioned);
    assert!(summary.linker.is_none() && !summary.cmake);
    let note = summary.note.as_deref().expect("a reason is given");
    assert!(
        note.contains("linker:") && note.contains("cmake:"),
        "{note}"
    );
    assert!(
        summary.render().starts_with("soldr: nothing provisioned ("),
        "{}",
        summary.render()
    );
    assert!(files_under(root.path()).is_empty(), "nothing was fetched");
}

#[test]
fn render_names_what_was_provisioned() {
    let summary = BuildToolsSummary {
        provisioned: true,
        linker: Some(BuildToolsLinker {
            choice: "fast".to_string(),
            target: "x86_64-unknown-linux-gnu".to_string(),
            driver: Some("/root/shim".to_string()),
            rustflags: None,
            reld: None,
        }),
        cmake: true,
        cross: None,
        note: None,
    };
    let line = summary.render();
    assert!(line.contains("linker fast for x86_64-unknown-linux-gnu -> /root/shim"));
    assert!(line.contains("managed cmake/ninja"));
}
