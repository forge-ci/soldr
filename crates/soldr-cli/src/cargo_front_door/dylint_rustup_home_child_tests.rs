//! soldr#3567 / soldr#3605: the Dylint child Cargo runs under the Rustup home
//! its nightly and targets were provisioned in, while ordinary Cargo keeps
//! soldr#1799's binary-located homes.
//!
//! These drive the real `build_child_cargo_command` with a host-resolved
//! Cargo (outside Soldr's managed homes) and a managed home on disk -- the
//! dev-host shape in which the target was installed into the managed home and
//! the check then looked for it in `~/.rustup`.

use super::*;
use crate::{EnvVarGuard, TEST_PROCESS_ENV_LOCK};

struct Fixture {
    _root: tempfile::TempDir,
    managed_rustup: PathBuf,
    host_cargo: PathBuf,
    host_rustc: PathBuf,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let managed_rustup = root.path().join("soldr").join("rustup");
    std::fs::create_dir_all(&managed_rustup).expect("managed rustup home");
    let host_bin = root.path().join("host-toolchain").join("bin");
    std::fs::create_dir_all(&host_bin).expect("host toolchain bin");
    let host_cargo = host_bin.join("cargo");
    let host_rustc = host_bin.join("rustc");
    std::fs::write(&host_cargo, b"").expect("host cargo");
    std::fs::write(&host_rustc, b"").expect("host rustc");
    Fixture {
        _root: root,
        managed_rustup,
        host_cargo,
        host_rustc,
    }
}

fn plan() -> crate::dylint_toolchain::DylintToolchainPlan {
    crate::dylint_toolchain::DylintToolchainPlan::identity(
        "nightly-2026-05-28-x86_64-unknown-linux-gnu".to_string(),
        "1.98.0-nightly".to_string(),
        "0123456789abcdef0123456789abcdef01234567".to_string(),
    )
}

fn child_rustup_home(
    fixture: &Fixture,
    args: &[&str],
    dylint_plan: Option<&crate::dylint_toolchain::DylintToolchainPlan>,
) -> Option<PathBuf> {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    let (command, _) = build_child_cargo_command(&ChildCargoSpec {
        args: &args,
        cargo: &fixture.host_cargo,
        rustc: &fixture.host_rustc,
        trust_inherited_soldr_env: false,
        build_like_cargo: false,
        explicit_toolchain: None,
        dylint_plan,
        dylint_dependency_cook: false,
        transitive_env_overrides: &[],
    })
    .expect("child cargo command");
    command
        .get_envs()
        .find_map(|(name, value)| (name == crate::core::RUSTUP_HOME_ENV_VAR).then_some(value))
        .flatten()
        .map(PathBuf::from)
}

/// The home every Dylint installer's rustup child runs under: the nightly
/// install, its components, and `rustup target add` all prepare their
/// command with `apply_implicit_toolchain_homes`.
fn installer_rustup_home() -> Option<PathBuf> {
    let mut installer = std::process::Command::new("rustup");
    crate::apply_implicit_toolchain_homes(&mut installer);
    installer
        .get_envs()
        .find_map(|(name, value)| (name == crate::core::RUSTUP_HOME_ENV_VAR).then_some(value))
        .flatten()
        .map(PathBuf::from)
}

#[test]
fn dylint_child_runs_under_the_home_its_targets_are_provisioned_in() {
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = fixture();
    // Home resolution walks up from the cwd for a repo-local `.rustup`; from
    // a checkout under `$HOME` that walk reaches the caller's real
    // `~/.rustup`. Run from the fixture root, which has no such ancestor.
    let _cwd = crate::CwdGuard::enter(fixture._root.path());
    let _root = EnvVarGuard::set(
        crate::core::SOLDR_CACHE_DIR_ENV_VAR,
        fixture.managed_rustup.parent().expect("soldr root"),
    );
    let _home = EnvVarGuard::remove(crate::core::RUSTUP_HOME_ENV_VAR);
    let _driver = EnvVarGuard::remove("DYLINT_DRIVER_PATH");

    let provisioned = crate::dylint_rustup_home::dylint_rustup_home().expect("Dylint home");
    assert_eq!(provisioned, fixture.managed_rustup);
    assert_eq!(
        installer_rustup_home().as_deref(),
        Some(provisioned.as_path())
    );

    let plan = plan();
    let child = child_rustup_home(
        &fixture,
        &[
            "dylint",
            "--all",
            "--",
            "--target",
            "x86_64-pc-windows-msvc",
        ],
        Some(&plan),
    );
    assert_eq!(
        child.as_deref(),
        Some(provisioned.as_path()),
        "a host-resolved Cargo in Dylint scope must resolve the nightly in the \
         home `dylint_target::ensure_targets` installed the target into"
    );
}

#[test]
fn ordinary_cargo_child_keeps_the_callers_homes() {
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = fixture();
    let _root = EnvVarGuard::set(
        crate::core::SOLDR_CACHE_DIR_ENV_VAR,
        fixture.managed_rustup.parent().expect("soldr root"),
    );
    let _home = EnvVarGuard::remove(crate::core::RUSTUP_HOME_ENV_VAR);

    // soldr#1799/#1768: a host Cargo outside Dylint scope never inherits the
    // default-less managed home. It keeps the caller's homes, which
    // `apply_implicit_toolchain_homes` may set explicitly (e.g. `~/.rustup`
    // on a host where it exists), so assert "not managed", not "unset".
    let child = child_rustup_home(&fixture, &["check"], None);
    assert_ne!(
        child.as_deref(),
        Some(fixture.managed_rustup.as_path()),
        "an ordinary Cargo child must not run under Soldr's managed home"
    );
}

#[test]
fn an_explicit_caller_home_is_the_dylint_home() {
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = fixture();
    let caller = fixture.managed_rustup.with_file_name("caller-rustup");
    std::fs::create_dir_all(&caller).expect("caller rustup home");
    let _root = EnvVarGuard::set(
        crate::core::SOLDR_CACHE_DIR_ENV_VAR,
        fixture.managed_rustup.parent().expect("soldr root"),
    );
    let _home = EnvVarGuard::set(crate::core::RUSTUP_HOME_ENV_VAR, &caller);
    let _driver = EnvVarGuard::remove("DYLINT_DRIVER_PATH");

    assert_eq!(
        crate::dylint_rustup_home::dylint_rustup_home().expect("Dylint home"),
        caller
    );
    let plan = plan();
    assert_eq!(
        child_rustup_home(&fixture, &["dylint", "--all"], Some(&plan)).as_deref(),
        Some(caller.as_path())
    );
}
