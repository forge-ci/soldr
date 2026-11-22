//! Profile selection for the host plan. Unconfigured plans are untouched.
use super::model::{Invocation, PlanScope, Stage};
use crate::cargo_front_door::canonical::{self, CompilingVerb, Selection};
use crate::cargo_metadata_canonical::{
    profile_from, CanonicalFeatures, CanonicalPackages, CanonicalTarget,
};
use crate::core::SoldrError;
use std::path::Path;

pub(super) fn apply(
    root: &Path,
    invocation: &Invocation,
    target: &str,
    stages: &mut [Stage],
) -> Result<PlanScope, SoldrError> {
    let scope = &invocation.scope;
    let Some((mut profile, _)) = profile_from(root)? else {
        return Ok(PlanScope {
            packages: scope.packages.clone(),
            features: scope.features.clone(),
            all_features: scope.all_features,
            no_default_features: scope.no_default_features,
        });
    };
    // A requested cross target must remain explicit even for an implicit-host
    // profile. The plan's domain already records this intentional override.
    if invocation.requested_target.is_some() {
        profile.target = CanonicalTarget::ExplicitHost;
    }
    let explicit = scope.cargo_args();
    for stage in stages {
        let verb = match stage.name.as_str() {
            "clippy" => CompilingVerb::Clippy,
            "nextest" | "nextest-compile" => CompilingVerb::NextestRun,
            "nextest-archive" => CompilingVerb::NextestArchive,
            "dylint-workspace" | "doctests" => CompilingVerb::Check,
            _ => continue,
        };
        let mut args = canonical::scoped_args(&profile, verb, target, &explicit);
        match stage.name.as_str() {
            "dylint-workspace" => {
                args.splice(
                    0..1,
                    [
                        "dylint".into(),
                        "--no-build".into(),
                        "--all".into(),
                        "--".into(),
                    ],
                );
            }
            "doctests" => {
                args[0] = "test".into();
                // Cargo rejects --doc combined with target-kind selectors.
                args.retain(|arg| arg != "--all-targets");
                args.push("--doc".into());
            }
            "clippy" => {}
            _ => args.extend(execution_args(&stage.command)),
        }
        stage.command = vec!["soldr".into(), "cargo".into()];
        stage.command.extend(args);
    }
    let selection = Selection::from_args(
        CompilingVerb::Clippy,
        &canonical::scoped_args(&profile, CompilingVerb::Clippy, target, &explicit),
        &[],
    );
    Ok(PlanScope {
        packages: match selection.packages {
            CanonicalPackages::Workspace => Vec::new(),
            CanonicalPackages::Explicit(packages) => packages,
        },
        all_features: selection.features == CanonicalFeatures::All,
        features: match selection.features {
            CanonicalFeatures::All => Vec::new(),
            CanonicalFeatures::Explicit(features) => features,
        },
        no_default_features: !selection.default_features,
    })
}

/// Cargo's implicit-host artifacts live directly under target/, not target/host/.
pub(super) fn stable_target_dir(
    root: &Path,
    invocation: &Invocation,
    target_root: &Path,
    target: &str,
) -> Result<std::path::PathBuf, SoldrError> {
    let implicit = invocation.requested_target.is_none()
        && profile_from(root)?
            .is_some_and(|(profile, _)| profile.target == CanonicalTarget::Implicit);
    Ok(if implicit {
        target_root.to_path_buf()
    } else {
        target_root.join(target)
    })
}

fn execution_args(command: &[String]) -> Vec<String> {
    let mut execution = Vec::new();
    let mut iter = command.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--no-run" | "--no-fail-fast" => execution.push(arg.clone()),
            "--test-threads" | "--archive-file" | "--archive-format" => {
                execution.push(arg.clone());
                execution.extend(iter.next().cloned());
            }
            _ => {}
        }
    }
    execution
}

#[cfg(test)]
mod tests {
    use super::super::model::{OutputFormat, Scope, StageMetrics};
    use super::*;

    fn stage(name: &str, args: &[&str]) -> Stage {
        Stage {
            name: name.into(),
            domain: "stable",
            kind: "compiler",
            command: args.iter().map(|arg| (*arg).into()).collect(),
            working_directory: ".".into(),
            depends_on: Vec::new(),
            concurrency_group: None,
            executes_compiler: true,
            metrics: StageMetrics {
                wall_time_ms: None,
                bytes: None,
                zccache_counters: None,
            },
        }
    }

    fn invocation() -> Invocation {
        Invocation {
            explain: true,
            format: OutputFormat::Json,
            scope: Scope::default(),
            requested_target: None,
            no_run: false,
        }
    }

    #[test]
    fn canonical_ci_plan_clippy_and_test_steps_share_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        for selection in [
            "features = 'all'\npackages = 'workspace'\nexclude = ['skip']",
            "features = ['b', 'a']\npackages = ['b', 'a']\ndefault-features = false\ndev-targets = false\ntarget = 'implicit'",
        ] {
            std::fs::write(dir.path().join("Cargo.toml"), format!("[workspace.metadata.soldr.cargo]\n{selection}\nclippy-args = ['-W', 'clippy::all']")).unwrap();
            let (profile, _) = profile_from(dir.path()).unwrap().unwrap();
            let mut stages = [stage("clippy", &[]), stage("nextest-compile", &["--no-run", "--test-threads", "4"]),
                stage("nextest", &["--no-fail-fast", "--test-threads", "4"]),
                stage("nextest-archive", &["--archive-file", "tests.tar.zst", "--archive-format", "tar-zst"]),
                stage("doctests", &[])];
            let scope = apply(dir.path(), &invocation(), "host", &mut stages).unwrap();
            let expected: Vec<String> = canonical::command(&profile, CompilingVerb::Clippy, "host")
                .split_whitespace().map(str::to_owned).collect();
            assert_eq!(stages[0].command, expected);
            for (index, verb) in [(1, CompilingVerb::NextestRun), (2, CompilingVerb::NextestRun), (3, CompilingVerb::NextestArchive)] {
                let args = &stages[index].command[2..];
                assert!(Selection::from_args(verb, args, &[]).drift(&profile).is_empty(), "{args:?}");
            }
            assert_eq!(scope.all_features, profile.features == CanonicalFeatures::All);
            assert_eq!(scope.no_default_features, !profile.default_features);
            assert!(stages[1].command.ends_with(&["--no-run".into(), "--test-threads".into(), "4".into()]));
            assert!(stages[2].command.contains(&"--no-fail-fast".into()));
            assert!(stages[3].command.contains(&"tests.tar.zst".into()));
            assert!(stages[4].command.contains(&"--doc".into()));
            assert!(Selection::from_args(CompilingVerb::Test, &stages[4].command[2..], &[]).drift(&profile).is_empty());
            assert!(!stages[4].command.contains(&"--all-targets".into()));
        }
    }

    #[test]
    fn canonical_ci_plan_without_profile_is_byte_for_byte_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\nmembers = []").unwrap();
        let mut stages = [
            stage(
                "clippy",
                &[
                    "soldr",
                    "cargo",
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--target",
                    "host",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
            stage(
                "nextest",
                &[
                    "soldr",
                    "cargo",
                    "nextest",
                    "run",
                    "--workspace",
                    "--lib",
                    "--tests",
                    "--target",
                    "host",
                    "--test-threads",
                    "4",
                ],
            ),
        ];
        let before = serde_json::to_value(&stages).unwrap();
        apply(dir.path(), &invocation(), "host", &mut stages).unwrap();
        assert_eq!(serde_json::to_value(&stages).unwrap(), before);
    }

    #[test]
    fn canonical_ci_repeating_canonical_scope_does_not_duplicate_flags() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace.metadata.soldr.cargo]\ndefault-features = false\npackages = ['a']\nfeatures = ['base']").unwrap();
        let mut invocation = invocation();
        invocation.scope.packages = vec!["a".into()];
        invocation.scope.features = vec!["base".into()];
        invocation.scope.no_default_features = true;
        let mut stages = [stage("clippy", &[])];
        apply(dir.path(), &invocation, "host", &mut stages).unwrap();
        let args = &stages[0].command;
        assert_eq!(
            args.iter()
                .filter(|arg| *arg == "--no-default-features")
                .count(),
            1
        );
        assert!(!args.contains(&"-p".into()));
        assert_eq!(args.iter().filter(|arg| *arg == "--features").count(), 1);
    }

    #[test]
    fn canonical_ci_explicit_scope_reaches_runtime_enforcement() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace.metadata.soldr.cargo]\nfeatures = ['base']",
        )
        .unwrap();
        let mut invocation = invocation();
        invocation.scope.packages = vec!["a".into()];
        invocation.scope.features = vec!["extra".into()];
        let mut stages = [stage("clippy", &[])];
        apply(dir.path(), &invocation, "host", &mut stages).unwrap();
        let args = &stages[0].command;
        assert!(args.windows(2).any(|w| w == ["--package", "a"]));
        assert!(args.windows(2).any(|w| w == ["--features", "extra"]));
        assert_eq!(&args[args.len() - 3..], ["--", "-D", "warnings"]);
    }
}
