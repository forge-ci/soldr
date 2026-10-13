//! soldr#3277: the cargo front door must not override a linker the project
//! declared for itself in `[target.<triple>]` of its own `.cargo/config.toml`.
//!
//! A new file rather than more of the sibling `tests.rs` for the same reason
//! `backtrace_policy_tests.rs` and `scrub_pool_tests.rs` exist: that file is
//! already past the 1,500-line ceiling, so the ratchet refuses to let it grow.

use super::target;
use crate::core::SoldrPaths;
use crate::EnvVarGuard;
use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;
use std::ffi::{OsStr, OsString};
use std::path::Path;

/// The cross target every case below uses. Deliberate: `resolve_for_target`
/// returns a non-empty injection for every Linux target regardless of the
/// reld-on-PATH probe (`clang` either way), so "nothing was injected" can only
/// mean the soldr#3277 guard fired and never an unlucky probe result.
const TARGET: &str = "aarch64-unknown-linux-gnu";
const LINKER_KEY: &str = "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER";
const RUSTFLAGS_KEY: &str = "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS";

fn command_env_override(
    command: &std::process::Command,
    key: &'static str,
) -> Option<Option<OsString>> {
    command
        .get_envs()
        .find(|(candidate, _)| *candidate == OsStr::new(key))
        .map(|(_, value)| value.map(OsString::from))
}

/// Where the cargo child would find the shim `name` through its `PATH`.
fn resolve_shim_on_command_path(
    command: &std::process::Command,
    name: &str,
) -> Option<std::path::PathBuf> {
    let path = command_env_override(command, "PATH").flatten()?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn argvec(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

fn assert_generated_linux_linker(command: &std::process::Command, context: &str) {
    let linker = command_env_override(command, LINKER_KEY)
        .and_then(|value| value)
        .expect("Linux linker injection");
    let name = linker.to_string_lossy();
    assert!(
        !name.contains('/') && name.starts_with("clang-"),
        "{context}: expected a generated linker shim passed by file name, got {name}",
    );
    // The helper's temporary root is gone by now, so check the search-path
    // entry rather than the file; the reld end-to-end test reads a live shim.
    let search = command_env_override(command, "PATH")
        .flatten()
        .unwrap_or_default();
    assert!(
        std::env::split_paths(&search).any(|dir| dir.ends_with("linker-shims/v1")),
        "{context}: the shim's directory must be on the cargo child's search path: {search:?}",
    );
    assert_eq!(
        command_env_override(command, RUSTFLAGS_KEY),
        None,
        "{context}: linker selection must preserve project rustflags",
    );
}

/// Build a project root whose `.cargo/config.toml` declares `[target.TARGET]`
/// with `body`, then run `apply_linker_override` from inside it.
///
/// The root is a fresh tempdir with no `Cargo.toml` anywhere above it, so
/// `linker::project_root`'s ancestor walk falls back to the fixture directory
/// itself — the config file soldr reads is the one written here.
fn apply_from_project_with_config(body: &str) -> std::process::Command {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let project = root.path().join("project");
    std::fs::create_dir_all(project.join(".cargo")).expect("mkdir .cargo");
    std::fs::write(project.join(".cargo").join("config.toml"), body).expect("write config.toml");

    let mut command = std::process::Command::new("cargo");
    let _cwd = crate::CwdGuard::enter(&project);
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(target::apply_linker_override(
            &mut command,
            &argvec(&format!("build --target {TARGET}")),
            None,
            &paths,
            false,
        ))
        .expect("apply_linker_override");
    command
}

/// Cargo gives `CARGO_TARGET_<TRIPLE>_LINKER` precedence over the config file,
/// so the *automatic* default (no `SOLDR_LINKER`, no `~/.soldr/config.toml`
/// `linker =`) must not inject one at all. Before this guard the soldr#3262
/// reld default silently replaced the project's declared linker.
#[test]
fn automatic_linker_defers_to_project_target_linker() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _soldr_linker = EnvVarGuard::remove("SOLDR_LINKER");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);

    let command = apply_from_project_with_config(&format!("[target.{TARGET}]\nlinker = \"cc\"\n"));

    assert_eq!(
        command_env_override(&command, LINKER_KEY),
        None,
        "the automatic default must not clobber the project's [target.{TARGET}] linker",
    );
    assert_eq!(
        command_env_override(&command, RUSTFLAGS_KEY),
        None,
        "and must not clobber it through rustflags either",
    );
}

/// `rustflags` alone is enough to suppress the automatic default: Cargo's
/// `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` replaces the config file's target
/// rustflags outright rather than merging with them.
#[test]
fn automatic_linker_defers_to_project_target_rustflags() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _soldr_linker = EnvVarGuard::remove("SOLDR_LINKER");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);

    let command = apply_from_project_with_config(&format!(
        "[target.{TARGET}]\nrustflags = [\"-C\", \"target-cpu=native\"]\n"
    ));

    assert_eq!(command_env_override(&command, LINKER_KEY), None);
    assert_eq!(command_env_override(&command, RUSTFLAGS_KEY), None);
}

/// The guard is scoped to the *matching* triple: a config section for some
/// other target says nothing about this build, so the default still applies.
#[test]
fn automatic_linker_still_injects_for_an_unrelated_target_section() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _soldr_linker = EnvVarGuard::remove("SOLDR_LINKER");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);

    let command =
        apply_from_project_with_config("[target.x86_64-pc-windows-msvc]\nlinker = \"cc\"\n");

    assert_generated_linux_linker(
        &command,
        "a config section for another triple must not suppress the default",
    );
}

/// The other half of the soldr#3277 rule: an explicit `SOLDR_LINKER` request is
/// a user decision too, and a more direct one, so it still wins over the
/// project's `.cargo/config.toml`.
#[test]
fn explicit_linker_request_still_overrides_project_target_config() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);
    let _soldr_linker = EnvVarGuard::set("SOLDR_LINKER", "fast");

    let command = apply_from_project_with_config(&format!("[target.{TARGET}]\nlinker = \"cc\"\n"));

    assert_generated_linux_linker(
        &command,
        "an explicit SOLDR_LINKER request outranks the project's config",
    );
}

/// Build a project root whose `Cargo.toml` declares
/// `[workspace.metadata.soldr] linker = "reld"` (soldr#3276 §6 dogfooding),
/// then run `apply_linker_override` from inside it.
fn apply_from_project_with_cargo_toml_metadata_linker(reld_bin: &Path) -> std::process::Command {
    // `.keep()` rather than a `TempDir` guard: the linker shim is materialized
    // under `paths.bin` and read back by the caller after this function
    // returns, so the directory must outlive the local `TempDir` drop.
    let root = tempfile::tempdir().expect("tempdir").keep();
    let paths = SoldrPaths::with_root(root.join("soldr"));
    let project = root.join("project");
    std::fs::create_dir_all(&project).expect("mkdir project");
    std::fs::write(
        project.join("Cargo.toml"),
        "[workspace]\nmembers = []\n\n[workspace.metadata.soldr]\nlinker = \"reld\"\n",
    )
    .expect("write Cargo.toml");

    let _reld_bin = EnvVarGuard::set(
        crate::fetch::RELD_BIN_ENV_VAR,
        reld_bin.to_str().expect("utf8 reld path"),
    );

    let mut command = std::process::Command::new("cargo");
    let _cwd = crate::CwdGuard::enter(&project);
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(target::apply_linker_override(
            &mut command,
            &argvec(&format!("build --target {TARGET}")),
            None,
            &paths,
            false,
        ))
        .expect("apply_linker_override");
    command
}

/// soldr#3276 §6: the dogfooding gap. A project that declares
/// `linker = "reld"` in its own `Cargo.toml` `[workspace.metadata.soldr]`
/// table must never hand clang a *bare* `reld` name in `--ld-path=reld` —
/// clang requires `--ld-path=` to be an absolute path
/// (`clang: error: invalid linker name in argument '--ld-path=reld'`).
/// The resolved managed/fetched reld's absolute path must be substituted in
/// before the linker shim is materialized. This exercises the
/// `LinkerSource::CargoTomlMetadata` branch end to end through
/// `apply_linker_override`, which no other test in this file or
/// `cli_cargo_linker.rs` covers (those cover env, `.cargo/config.toml`, and
/// `~/.soldr/config.toml`, not `Cargo.toml` metadata).
#[test]
fn cargo_toml_metadata_linker_reld_resolves_absolute_path_end_to_end() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _soldr_linker = EnvVarGuard::remove("SOLDR_LINKER");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);

    let reld_dir = tempfile::tempdir().expect("reld tempdir");
    let reld_bin = reld_dir.path().join(format!(
        "reld{}",
        crate::platform::executable::name::script_suffix()
    ));
    std::fs::write(&reld_bin, "#!/bin/sh\nexit 0\n").expect("write fake reld");
    crate::platform::fs::permissions::make_executable(&reld_bin).expect("chmod +x");

    let command = apply_from_project_with_cargo_toml_metadata_linker(&reld_bin);

    let linker = command_env_override(&command, LINKER_KEY)
        .flatten()
        .expect("CARGO_TARGET_<TRIPLE>_LINKER must be injected for a declared reld linker");
    let linker = linker.to_string_lossy();

    // The bug this guards against: a bare `--ld-path=reld` (or a bare
    // `reld` CARGO_TARGET_*_LINKER value) reaching clang/cargo unresolved.
    assert!(
        !linker.ends_with("reld") && linker != "reld",
        "linker injection must not be a bare `reld` name, got: {linker}"
    );

    // On Linux the value is a generated content-addressed clang driver shim
    // (materialize_linker_driver_shim); everywhere else it is the direct
    // absolute reld path. Either way it must never be the literal `reld`.
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Linux {
        let shim = resolve_shim_on_command_path(&command, &linker).unwrap_or_else(|| {
            panic!("linux reld uses a generated clang driver shim on PATH: {linker}")
        });
        let shim_body = std::fs::read_to_string(shim).expect("read shim");
        assert!(
            shim_body.contains(&format!("--ld-path={}", reld_bin.display())),
            "shim must embed the resolved absolute reld path, not a bare name: {shim_body}"
        );
        assert!(
            !shim_body.contains("--ld-path=reld\n") && !shim_body.contains("--ld-path=reld "),
            "shim must not contain the unresolved bare `--ld-path=reld`: {shim_body}"
        );
    } else {
        assert_eq!(linker, reld_bin.to_string_lossy());
    }
}

// --- soldr#3483: `cargo dylint` must keep the lint library's dylint-link ---

/// A workspace that declares one Dylint library the way running-process and
/// this repo do (`workspace.metadata.dylint.libraries`), with the lint
/// package at `lints/fixture`. Returns the lint root; the workspace
/// `Cargo.toml` is written but carries no `.cargo/config.toml` of its own —
/// the outer `cargo dylint` guard used to read exactly that missing file,
/// find nothing, and inject over the lint's declaration.
fn workspace_with_declared_lint(project: &Path) -> std::path::PathBuf {
    std::fs::create_dir_all(project).expect("mkdir project");
    std::fs::write(
        project.join("Cargo.toml"),
        "[workspace]\nmembers = []\n\n[workspace.metadata.dylint]\nlibraries = [{ path = \"lints/fixture\" }]\n",
    )
    .expect("write workspace Cargo.toml");
    let lint = project.join("lints").join("fixture");
    std::fs::create_dir_all(lint.join(".cargo")).expect("mkdir lint .cargo");
    lint
}

/// The task fixture for soldr#3483: a lint crate declaring its linker under
/// an **exact** `[target.<triple>]` section of its own `.cargo/config.toml`,
/// while the workspace root (the cwd of the outer `cargo dylint`) declares
/// nothing. The resolution must see the lint's config — `source` proves the
/// guard read that file; today it only ever opens the workspace root's,
/// resolves the automatic `Fast` default, and would inject
/// `CARGO_TARGET_<TRIPLE>_LINKER` over `dylint-link`.
#[test]
fn dylint_scope_resolves_through_the_declared_lint_library_config() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _soldr_linker = EnvVarGuard::remove("SOLDR_LINKER");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);

    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let project = root.path().join("project");
    let lint = workspace_with_declared_lint(&project);
    std::fs::write(
        lint.join(".cargo").join("config.toml"),
        format!("[target.{TARGET}]\nlinker = \"dylint-link\"\n"),
    )
    .expect("write lint config.toml");

    let _cwd = crate::CwdGuard::enter(&project);
    let selection = crate::linker::resolve_project_choice_from_cwd(Some(TARGET), &paths, true)
        .expect("resolve");

    assert_eq!(
        selection.source,
        crate::linker::LinkerSource::CargoConfig,
        "the lint library's exact-triple linker declaration must be what the guard saw"
    );
    assert_eq!(
        selection.choice,
        crate::linker::LinkerChoice::Default,
        "a declared non-reld linker suppresses the automatic default (soldr#3277)"
    );
}

/// End-to-end through `apply_linker_override`: with the Dylint scope active,
/// no `CARGO_TARGET_*` injection may reach the child cargo — the injected env
/// outranks every `[target.*]` config section (exact triple beats `cfg`), so
/// even a working injected linker silently disables `dylint-link` and the
/// lint build fails with "Could not find lib<name>@<toolchain>.so despite
/// successful build". This fixture's lint declares the linker the way every
/// real lint crate does, under `[target.'cfg(all())']`.
#[test]
fn dylint_scope_never_injects_the_automatic_linker() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _soldr_linker = EnvVarGuard::remove("SOLDR_LINKER");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);

    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let project = root.path().join("project");
    let lint = workspace_with_declared_lint(&project);
    std::fs::write(
        lint.join(".cargo").join("config.toml"),
        "[target.'cfg(all())']\nlinker = \"dylint-link\"\n",
    )
    .expect("write lint config.toml");

    let mut command = std::process::Command::new("cargo");
    let _cwd = crate::CwdGuard::enter(&project);
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(target::apply_linker_override(
            &mut command,
            &argvec(&format!("build --target {TARGET}")),
            None,
            &paths,
            true,
        ))
        .expect("apply_linker_override");

    assert_eq!(
        command_env_override(&command, LINKER_KEY),
        None,
        "the automatic default must never set a target linker inside the Dylint scope",
    );
    assert_eq!(
        command_env_override(&command, RUSTFLAGS_KEY),
        None,
        "…nor target rustflags, which would also clobber the lint's declaration",
    );
}

/// The backstop half of soldr#3483: a lint the workspace metadata does not
/// declare (`cargo dylint --path …`) still gets no automatic injection, even
/// though the per-lint config read cannot see it. Only an explicit
/// `SOLDR_LINKER` may inject inside the Dylint scope.
#[test]
fn dylint_scope_without_any_declared_config_still_injects_nothing() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _soldr_linker = EnvVarGuard::remove("SOLDR_LINKER");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);

    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let project = root.path().join("project");
    // Declared, but with no `.cargo/config.toml` at all: nothing for the
    // guard to read, so only the backstop stands between the automatic
    // default and the lint build's env.
    workspace_with_declared_lint(&project);

    let mut command = std::process::Command::new("cargo");
    let _cwd = crate::CwdGuard::enter(&project);
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(target::apply_linker_override(
            &mut command,
            &argvec(&format!("build --target {TARGET}")),
            None,
            &paths,
            true,
        ))
        .expect("apply_linker_override");

    assert_eq!(command_env_override(&command, LINKER_KEY), None);
    assert_eq!(command_env_override(&command, RUSTFLAGS_KEY), None);
}

/// The exempt case for the backstop: `SOLDR_LINKER` is an explicit user
/// decision and outranks project config everywhere else (soldr#3277), so it
/// keeps working inside the Dylint scope too.
#[test]
fn dylint_scope_still_honors_an_explicit_linker_request() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);
    let _soldr_linker = EnvVarGuard::set("SOLDR_LINKER", "fast");

    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let project = root.path().join("project");
    workspace_with_declared_lint(&project);

    let mut command = std::process::Command::new("cargo");
    let _cwd = crate::CwdGuard::enter(&project);
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(target::apply_linker_override(
            &mut command,
            &argvec(&format!("build --target {TARGET}")),
            None,
            &paths,
            true,
        ))
        .expect("apply_linker_override");

    assert_generated_linux_linker(
        &command,
        "an explicit SOLDR_LINKER request outranks the Dylint scope backstop",
    );
}

/// The gate on the whole feature: outside the Dylint scope nothing changes.
/// A lint package's config is *not* cargo config for a plain workspace
/// build (Cargo only reads `.cargo/config.toml` from the cwd ancestry), so
/// it must not suppress the automatic default for `cargo build` either —
/// otherwise every dylint-enabled repo silently loses the fast linker on
/// ordinary builds.
#[test]
fn plain_builds_do_not_read_lint_library_configs() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _soldr_linker = EnvVarGuard::remove("SOLDR_LINKER");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);

    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let project = root.path().join("project");
    let lint = workspace_with_declared_lint(&project);
    std::fs::write(
        lint.join(".cargo").join("config.toml"),
        format!("[target.{TARGET}]\nlinker = \"dylint-link\"\n"),
    )
    .expect("write lint config.toml");

    let mut command = std::process::Command::new("cargo");
    let _cwd = crate::CwdGuard::enter(&project);
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(target::apply_linker_override(
            &mut command,
            &argvec(&format!("build --target {TARGET}")),
            None,
            &paths,
            false,
        ))
        .expect("apply_linker_override");

    assert_generated_linux_linker(
        &command,
        "a lint's config must not suppress the default outside the Dylint scope",
    );
}
