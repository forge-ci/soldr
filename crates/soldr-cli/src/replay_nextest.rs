//! Run an already packaged nextest against an archive without toolchain setup.
//!
//! This entry point is limited to archive-backed replay. It never resolves a
//! Cargo toolchain or downloads a runner; the caller supplies its exact path.

use std::path::Path;
use std::process::Command;

use crate::core::SoldrError;

/// Detect the replay subcommand without parsing its (possibly invalid) body.
/// The values of the three global options cannot themselves be subcommands.
pub(crate) fn is_invocation(args: &[String]) -> bool {
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--no-cache"
            | "--trust-inherited-soldr-env"
            | "--allow-unpinned"
            | "--timestamp-lines"
            | "--no-timestamp-lines"
            | "--no-cache-states"
            | "--debug" => {}
            "--zccache" | "--jobs" | "--zccache-mode" => {
                if iter.next().is_none() {
                    return false;
                }
            }
            value
                if value.starts_with("--zccache=")
                    || value.starts_with("--jobs=")
                    || value.starts_with("--zccache-mode=") => {}
            value => return value == "replay-nextest",
        }
    }
    false
}

fn validate(args: &[String]) -> Result<(), SoldrError> {
    let invalid = || {
        SoldrError::Other("soldr replay-nextest: expected nextest --version or archive-backed nextest list/run with supported options".to_string())
    };
    if args.first().map(String::as_str) != Some("nextest") {
        return Err(invalid());
    }
    if args.len() == 2 && args[1] == "--version" {
        return Ok(());
    }
    if !matches!(args.get(1).map(String::as_str), Some("list" | "run")) {
        return Err(invalid());
    }
    let mut archive = false;
    let mut binaries = false;
    let mut cargo = false;
    let mut remap = false;
    let mut index = 2;
    while index < args.len() {
        let arg = &args[index];
        if matches!(arg.as_str(), "--extract-overwrite" | "--no-fail-fast") {
            index += 1;
            continue;
        }
        let (option, inline) = match arg.split_once('=') {
            Some((option, value)) => (option, Some(value)),
            None => (arg.as_str(), None),
        };
        if !matches!(
            option,
            "--archive-file"
                | "--binaries-metadata"
                | "--cargo-metadata"
                | "--target-dir-remap"
                | "--extract-to"
                | "--workspace-remap"
                | "--profile"
                | "--partition"
                | "--message-format"
                | "--max-fail"
                | "-E"
        ) {
            return Err(invalid());
        }
        let value = match inline {
            Some(value) => value,
            None => {
                index += 1;
                args.get(index).ok_or_else(invalid)?.as_str()
            }
        };
        if value.is_empty() || value.starts_with('-') {
            return Err(invalid());
        }
        match option {
            "--archive-file" => archive = true,
            "--binaries-metadata" => binaries = true,
            "--cargo-metadata" => cargo = true,
            "--target-dir-remap" => remap = true,
            _ => {}
        }
        index += 1;
    }
    if !(archive || (binaries && cargo && remap)) || (archive && (binaries || cargo || remap)) {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn run(runner: &Path, args: &[String]) -> Result<i32, SoldrError> {
    validate(args)?;
    if !runner.is_absolute() || !runner.is_file() {
        return Err(SoldrError::Other(format!(
            "soldr replay-nextest: runner must be an existing absolute file: {}",
            runner.display()
        )));
    }
    let mut command = Command::new(runner);
    command.args(args);
    let status =
        crate::platform::process::spawn::exec_or_status(&mut command).map_err(|error| {
            SoldrError::Other(format!(
                "soldr replay-nextest: cannot execute {}: {error}",
                runner.display()
            ))
        })?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{is_invocation, run, validate};

    fn args(value: &[&str]) -> Vec<String> {
        value.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn replay_prefix_covers_all_declared_global_options() {
        use clap::CommandFactory;
        let mut actual: Vec<String> = crate::cli_args::Cli::command()
            .get_arguments()
            .filter_map(|arg| arg.get_long().map(str::to_owned))
            .filter(|name| name != "help" && name != "version")
            .collect();
        actual.sort();
        let mut expected: Vec<String> = [
            "no-cache",
            "trust-inherited-soldr-env",
            "allow-unpinned",
            "timestamp-lines",
            "no-timestamp-lines",
            "no-cache-states",
            "debug",
            "zccache",
            "jobs",
            "zccache-mode",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        expected.sort();
        assert_eq!(
            actual, expected,
            "update the replay prefix parser for new global CLI options"
        );
    }

    #[test]
    fn detects_replay_even_when_its_arguments_are_invalid() {
        for command in [
            vec![
                "soldr",
                "--zccache-mode",
                "copy",
                "replay-nextest",
                "--",
                "nextest",
            ],
            vec![
                "soldr",
                "--zccache-mode=copy",
                "replay-nextest",
                "--",
                "nextest",
            ],
        ] {
            assert!(is_invocation(&args(&command)));
        }
        assert!(!is_invocation(&args(&[
            "soldr",
            "cargo",
            "run",
            "replay-nextest"
        ])));
    }

    #[test]
    fn permits_version_and_archive_backed_replay_only() {
        assert!(validate(&args(&["nextest", "--version"])).is_ok());
        assert!(validate(&args(&[
            "nextest",
            "run",
            "--archive-file",
            "tests.tar.zst"
        ]))
        .is_ok());
        assert!(validate(&args(&[
            "nextest",
            "list",
            "--binaries-metadata",
            "b.json",
            "--cargo-metadata",
            "c.json",
            "--target-dir-remap",
            "target"
        ]))
        .is_ok());
        assert!(validate(&args(&["nextest", "run"])).is_err());
        assert!(validate(&args(&["nextest", "run", "--", "--archive-file", "fake"])).is_err());
        assert!(validate(&args(&[
            "nextest",
            "run",
            "--profile",
            "--archive-file",
            "fake"
        ]))
        .is_err());
        assert!(validate(&args(&[
            "nextest",
            "run",
            "--archive-file",
            "real",
            "--manifest-path",
            "other"
        ]))
        .is_err());
        assert!(validate(&args(&[
            "nextest",
            "archive",
            "--archive-file",
            "tests.tar.zst"
        ]))
        .is_err());
        assert!(validate(&args(&["nextest", "run", "--binaries-metadata", "b.json"])).is_err());
    }
    #[test]
    fn rejects_missing_and_relative_runners() {
        let version = args(&["nextest", "--version"]);
        assert!(run(std::path::Path::new("cargo-nextest"), &version).is_err());
        let dir = tempfile::tempdir().expect("temporary directory");
        let missing = dir.path().join("missing-runner");
        assert!(run(&missing, &version).is_err());
    }

    #[test]
    fn rejected_commands_never_start_the_supplied_runner() {
        let runner = std::env::current_exe().expect("current test binary");
        assert!(run(
            &runner,
            &args(&["nextest", "run", "--", "--archive-file", "fake"])
        )
        .is_err());
        assert!(run(
            &runner,
            &args(&["nextest", "list", "--profile", "--archive-file", "fake"])
        )
        .is_err());
    }
}
