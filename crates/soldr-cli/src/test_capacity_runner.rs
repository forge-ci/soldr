//! Lease admission at Cargo's executable launch boundary.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::SoldrError;

pub(crate) const SHIM_NAME: &str = "test-capacity-runner";

pub(crate) fn config_option(
    command: &Command,
    args: &[String],
) -> Result<Option<String>, SoldrError> {
    if !runs_test_executables(args)
        || crate::core::flag(crate::core::lint_priority::NEXTEST_DAEMON_ADMISSION_ENV)
        || crate::lint_steps::matches_capacity_root(
            crate::core::lint_priority::NEXTEST_STAGE_ROOT_ENV,
        )
        || args
            .iter()
            .take_while(|arg| *arg != "--")
            .any(|arg| arg == "--config" || arg.starts_with("--config=") || arg.starts_with("-C"))
    {
        return Ok(None);
    }

    let target =
        crate::cargo_front_door::cargo_target_triple(args, crate::pyo3_detect::host_triple());
    if target.is_empty()
        || !target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Ok(None);
    }
    let runner_env = format!(
        "CARGO_TARGET_{}_RUNNER",
        target.to_ascii_uppercase().replace('-', "_")
    );
    let cwd = std::env::current_dir()?;
    if cargo_config_has(&cwd, command, |config| configured_runner(config, &target)) {
        return Ok(None);
    }
    let paths = crate::core::SoldrPaths::new()?;
    let runner = crate::binaries::tool_shim_binary(&paths, SHIM_NAME)?;
    let existing = command
        .get_envs()
        .find(|(key, _)| *key == runner_env.as_str())
        .map_or_else(
            || std::env::var_os(&runner_env),
            |(_, value)| value.map(std::ffi::OsStr::to_os_string),
        );
    let mut runner = vec![toml::Value::String(runner.to_string_lossy().into_owned())];
    if let Some(existing) = existing {
        let Ok(path) = existing.into_string() else {
            return Ok(None);
        };
        // Cargo resolves relative paths and splits its string form. Chain
        // an unambiguous absolute executable without reinterpreting argv.
        if !Path::new(&path).is_absolute() || path.chars().any(char::is_whitespace) {
            return Ok(None);
        }
        runner.push(toml::Value::String(path));
    }
    let value = toml::Value::Array(runner);
    Ok(Some(format!("target.{target}.runner={value}")))
}

pub(crate) fn configure_rustdoc(command: &mut Command, args: &[String]) -> Result<(), SoldrError> {
    if !matches!(
        crate::cargo_front_door::first_cargo_subcommand(args),
        Some("test" | "t")
    ) || args.iter().take_while(|arg| *arg != "--").any(|arg| {
        matches!(arg.as_str(), "--no-run" | "--config")
            || arg.starts_with("--config=")
            || arg.starts_with("-C")
    }) || ["RUSTDOC", "CARGO_BUILD_RUSTDOC"].into_iter().any(|name| {
        std::env::var_os(name).is_some()
            || command
                .get_envs()
                .any(|(key, value)| key == name && value.is_some())
    }) || cargo_config_has(&std::env::current_dir()?, command, |config| {
        config
            .get("build")
            .is_some_and(|build| build.get("rustdoc").is_some())
    }) {
        return Ok(());
    }
    let paths = crate::core::SoldrPaths::new()?;
    command.env(
        "RUSTDOC",
        crate::binaries::tool_shim_binary(&paths, "rustdoc")?,
    );
    Ok(())
}

fn runs_test_executables(args: &[String]) -> bool {
    let Some(index) = crate::cargo_front_door::first_cargo_subcommand_index(args) else {
        return false;
    };
    let args_after = &args[index + 1..];
    let runnable = !args_after
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| matches!(arg.as_str(), "--no-run" | "--doc" | "--help" | "-h"));
    runnable
        && match args[index].as_str() {
            "test" | "t" => true,
            "nextest" => crate::cargo_front_door::first_nextest_verb_index(args, index)
                .is_some_and(|verb| args[verb] == "run"),
            _ => false,
        }
}

fn configured_runner(config: &toml::Table, target: &str) -> bool {
    config
        .get("target")
        .and_then(toml::Value::as_table)
        .is_some_and(|targets| {
            targets.iter().any(|(name, section)| {
                (name == target || name.starts_with("cfg(")) && section.get("runner").is_some()
            })
        })
}

fn cargo_config_has(cwd: &Path, command: &Command, matches: impl Fn(&toml::Table) -> bool) -> bool {
    let cargo_home = command
        .get_envs()
        .find(|(key, _)| *key == "CARGO_HOME")
        .and_then(|(_, value)| value.map(PathBuf::from))
        .or_else(|| std::env::var_os("CARGO_HOME").map(PathBuf::from))
        .or_else(|| std::env::home_dir().map(|home| home.join(".cargo")));
    cwd.ancestors()
        .map(|path| path.join(".cargo"))
        .chain(cargo_home)
        .any(|dir| {
            ["config", "config.toml"].into_iter().any(|name| {
                match std::fs::read_to_string(dir.join(name)) {
                    Ok(text) => text.parse::<toml::Table>().map_or(true, |config| {
                        config.contains_key("include") || matches(&config)
                    }),
                    Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                }
            })
        })
}

pub(crate) fn acquire_test_capacity(
) -> Result<Option<crate::daemon::client::ResidentCapacityLease>, SoldrError> {
    if crate::lint_steps::shares_resident_capacity() {
        return Ok(None);
    }
    let paths = crate::core::SoldrPaths::new()?;
    let socket = crate::daemon::client::default_sock_path(&paths);
    Ok(
        match crate::rustfmt_admission::acquire_starting_daemon(
            || crate::daemon::client::acquire_resident_capacity(&socket, 1),
            || {
                crate::soldr_main::start_daemon_route(crate::soldr_main::DAEMON_START_ROUTE_BUDGET)
                    .map_err(|error| error.to_string())
            },
        ) {
            Ok(lease) => Some(lease),
            Err(error) => {
                eprintln!("soldr: could not reserve ordinary test capacity: {error}");
                None
            }
        },
    )
}

pub(crate) fn run(
    args: &[String],
    inherited_provenance: &crate::replay_nextest::InheritedProvenance,
) -> Result<i32, SoldrError> {
    let (program, args) = args
        .split_first()
        .ok_or_else(|| SoldrError::Other("test capacity runner: missing executable".into()))?;
    let capacity = if args.iter().any(|arg| arg == "--list") {
        None
    } else {
        acquire_test_capacity()?
    };
    let mut command = Command::new(program);
    command.args(args);
    inherited_provenance.restore_to(&mut command);
    // Descendant test orchestrators use this lease while they stay in the
    // same cache root, just as nested compilers share the resident budget.
    crate::lint_steps::mark_resident_child(&mut command, capacity.as_ref());
    crate::exit_guard::mark_spoke();
    let mut child = crate::lint_steps::spawn_guarded(&mut command)?;
    let status = crate::platform::wait::wait_for(&mut child, crate::platform::wait::WHOLE_COMMAND)?;
    drop(capacity);
    Ok(crate::platform::process::test_child::forward_exit_status(
        &status,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_applicable_runner_sections_prevent_injection() {
        let mut config: toml::Table = r#"
            [target.aarch64-unknown-linux-gnu]
            runner = "qemu-aarch64"
        "#
        .parse()
        .unwrap();
        assert!(configured_runner(&config, "aarch64-unknown-linux-gnu"));
        assert!(!configured_runner(&config, "x86_64-unknown-linux-gnu"));

        config = r#"
            [target.'cfg(unix)']
            runner = ["env", "TEST=1"]
        "#
        .parse()
        .unwrap();
        assert!(configured_runner(&config, "x86_64-unknown-linux-gnu"));
    }

    #[test]
    fn cargo_runner_uses_a_sanctioned_multicall_identity() {
        let shim = format!("/fixture/{SHIM_NAME}{}", std::env::consts::EXE_SUFFIX);
        let args = vec![shim.clone(), "test-binary".into(), "--exact".into()];
        assert!(crate::multicall::is_shim_identity(&shim));
        assert_eq!(
            crate::multicall::maybe_dispatch(&args),
            Some(crate::multicall::MulticallDispatch::SoldrArgs(vec![
                SHIM_NAME.into(),
                "test-binary".into(),
                "--exact".into(),
            ]))
        );
    }

    #[test]
    fn only_test_execution_uses_a_runner() {
        for (args, expected) in [
            (&["test"][..], true),
            (&["test", "--no-run"][..], false),
            (&["nextest", "run"][..], true),
            (&["nextest", "list"][..], false),
            (&["test", "--", "--no-run"][..], true),
            (&["clippy", "--tests"][..], false),
        ] {
            let args = args.iter().map(ToString::to_string).collect::<Vec<_>>();
            assert_eq!(runs_test_executables(&args), expected, "{args:?}");
        }
    }
}
