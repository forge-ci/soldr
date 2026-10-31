//! Declared `soldr lint` steps and their execution (soldr#3958).
//!
//! `lint_cmd` decides *which* built-in steps exist; this module owns the
//! repo-declared extras (`[workspace.metadata.soldr.lint].extra`), the
//! typed plan both kinds of step share, and the porcelain runner that folds
//! a whole plan into one summary (see
//! [`crate::cargo_front_door::porcelain_lint`]).

use crate::cargo_front_door;
use crate::cargo_front_door::porcelain_lint::{self, LintLog, StepOutcome};
use crate::core::{SoldrError, SoldrPaths};
use crate::current_soldr_binary;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

/// A repo command from `extra`, resolved before anything runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExtraCommand {
    /// The command as written (`ruff check .`); also the step name.
    pub(crate) label: String,
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    /// The workspace root: the directory of the manifest that declared it.
    pub(crate) cwd: PathBuf,
}

/// One step of a lint plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlannedStep {
    /// A cargo subcommand run through the Soldr front door.
    Cargo { label: String, args: Vec<String> },
    /// A repo command.
    Extra(ExtraCommand),
}

impl PlannedStep {
    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Cargo { label, .. } => label,
            Self::Extra(extra) => &extra.label,
        }
    }
}

/// Resolve every `extra` command up front. A tool that is not there fails the
/// whole lint, naming it, before any step runs.
pub(crate) fn plan_extras(
    extra: &[Vec<String>],
    root: &Path,
) -> Result<Vec<PlannedStep>, SoldrError> {
    extra
        .iter()
        .map(|command| {
            let (tool, args) = command
                .split_first()
                .ok_or_else(|| SoldrError::Other("lint: empty extra command".into()))?;
            let program = resolve_tool(tool, root).ok_or_else(|| {
                SoldrError::Other(format!(
                    "lint: extra step `{}`: tool `{tool}` not found on PATH (cwd {})",
                    command.join(" "),
                    root.display()
                ))
            })?;
            Ok(PlannedStep::Extra(ExtraCommand {
                label: command.join(" "),
                program,
                args: args.to_vec(),
                cwd: root.to_path_buf(),
            }))
        })
        .collect()
}

/// Find `tool`: a path (contains a separator) is taken relative to `root`;
/// a bare name is searched on `PATH`, trying each `PATHEXT` suffix when the
/// host defines one.
fn resolve_tool(tool: &str, root: &Path) -> Option<PathBuf> {
    if tool.contains('/') || tool.contains('\\') {
        let candidate = root.join(tool);
        return candidate.is_file().then_some(candidate);
    }
    let suffixes: Vec<String> = std::iter::once(String::new())
        .chain(
            std::env::var("PATHEXT")
                .unwrap_or_default()
                .split(';')
                .filter(|ext| !ext.is_empty())
                .map(str::to_lowercase),
        )
        .collect();
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        suffixes.iter().find_map(|suffix| {
            let candidate = dir.join(format!("{tool}{suffix}"));
            candidate.is_file().then_some(candidate)
        })
    })
}

fn spawn_guarded(command: &mut Command) -> std::io::Result<Child> {
    // Spawns share, staged writes exclude (soldr#3098); held across the spawn
    // call only.
    let guard = crate::core::spawn_exclusion::spawn_shared();
    let child = command.spawn();
    drop(guard);
    child
}

fn extra_command(extra: &ExtraCommand) -> Command {
    let mut command = Command::new(&extra.program);
    command.args(&extra.args).current_dir(&extra.cwd);
    command
}

/// Run a repo command with the caller's stdio (the non-porcelain path).
pub(crate) fn run_extra_inherited(extra: &ExtraCommand) -> Result<i32, SoldrError> {
    let mut command = extra_command(extra);
    // The child inherits our stdio and explains its own failure.
    crate::exit_guard::mark_spoke();
    let status = spawn_guarded(&mut command)
        .and_then(|mut child| {
            crate::platform::wait::wait_for(&mut child, crate::platform::wait::WHOLE_COMMAND)
        })
        .map_err(|error| start_error(extra, &error))?;
    Ok(status.code().unwrap_or(1))
}

fn start_error(extra: &ExtraCommand, error: &std::io::Error) -> SoldrError {
    SoldrError::Other(format!(
        "lint: failed to run extra step `{}`: {error}",
        extra.label
    ))
}

/// Run a whole plan under porcelain: one log, one summary line. Stops at the
/// first failing step, whose output is printed in full.
pub(crate) fn run_porcelain(
    steps: Vec<PlannedStep>,
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    let cwd = std::env::current_dir()
        .map_err(|e| SoldrError::Other(format!("lint: cannot resolve current directory: {e}")))?;
    let mut log = LintLog::create(&SoldrPaths::new()?, &cwd)?;
    let soldr = current_soldr_binary()?;
    let mut outcomes = Vec::with_capacity(steps.len());
    let mut failure_output = None;
    let mut autofixed = Vec::new();
    for step in &steps {
        let mut command = match step {
            PlannedStep::Cargo { args, .. } => {
                prepare_cargo_step(args)?;
                cargo_child(&soldr, args, cache_enabled, trust_inherited_soldr_env)
            }
            PlannedStep::Extra(extra) => extra_command(extra),
        };
        let sink = log.begin_step(step.label(), &describe(&command))?;
        let mark = sink.mark;
        command
            .stdin(Stdio::null())
            .stdout(sink.stdout)
            .stderr(sink.stderr);
        crate::exit_guard::mark_spoke();
        let started = Instant::now();
        let status = spawn_guarded(&mut command)
            .and_then(|mut child| {
                crate::platform::wait::wait_for(&mut child, crate::platform::wait::WHOLE_COMMAND)
            })
            .map_err(|error| {
                SoldrError::Other(format!(
                    "lint: failed to run step `{}`: {error}",
                    step.label()
                ))
            })?;
        let exit_code = status.code().unwrap_or(1);
        outcomes.push(StepOutcome {
            name: step.label().to_string(),
            exit_code,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        });
        if exit_code != 0 {
            failure_output = Some(log.step_output(mark));
            break;
        }
        if step.label() == FMT_FIX_LABEL {
            autofixed = fixed_files(&log.step_output(mark));
        }
    }
    Ok(log.finish(&outcomes, failure_output.as_deref(), autofixed))
}

fn cargo_child(
    soldr: &Path,
    args: &[String],
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Command {
    let mut command = Command::new(soldr);
    if !cache_enabled {
        command.arg("--no-cache");
    }
    if trust_inherited_soldr_env {
        command.arg("--trust-inherited-soldr-env");
    }
    command.arg("cargo").args(args);
    cargo_front_door::configure_cargo_child_for_timeout(&mut command);
    command.env(cargo_front_door::INHERIT_PARENT_PROCESS_GROUP_ENV, "1");
    porcelain_lint::scrub_child_env(&mut command);
    command
}

fn describe(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) async fn run_compile_steps(
    steps: Vec<Vec<String>>,
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    for args in steps {
        let code = run_cargo_step(&args, cache_enabled, trust_inherited_soldr_env).await?;
        if code != 0 {
            return Ok(code);
        }
    }
    Ok(0)
}

/// Run a rust-suite plan with the caller's stdio, stopping at the first
/// failing step (soldr#3958).
pub(crate) async fn run_planned_steps(
    steps: Vec<PlannedStep>,
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    for step in steps {
        let code = match &step {
            PlannedStep::Cargo { args, .. } => {
                run_cargo_step(args, cache_enabled, trust_inherited_soldr_env).await?
            }
            PlannedStep::Extra(extra) => run_extra_inherited(extra)?,
        };
        if code != 0 {
            return Ok(code);
        }
    }
    Ok(0)
}

/// Per-step preparation shared by every runner: a cross-target Clippy only
/// needs the target's std, not a full cross linker. `rustup_add_target` is
/// idempotent, a no-op for the host triple, and routes through soldr's managed
/// rustup/cargo homes rather than a bare `rustup` on PATH.
pub(crate) fn prepare_cargo_step(args: &[String]) -> Result<(), SoldrError> {
    let dylint = cargo_front_door::first_cargo_subcommand(args) == Some("dylint");
    if !dylint {
        if let Some(target) = target_arg_value(args) {
            crate::prepare_cmd::rustup_add_target(&target)?;
        }
    }
    Ok(())
}

async fn run_cargo_step(
    args: &[String],
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    prepare_cargo_step(args)?;
    let code =
        cargo_front_door::run_cargo_front_door(args, cache_enabled, trust_inherited_soldr_env)
            .await?;
    if code != 0 {
        if let Some(target) = target_arg_value(args) {
            let dylint = cargo_front_door::first_cargo_subcommand(args) == Some("dylint");
            let command = if dylint { "dylint" } else { "clippy" };
            eprintln!("soldr lint: {command} failed for target {target}");
        }
    }
    Ok(code)
}

/// Extract the `--target <value>` (either spelling) argument from a cargo
/// invocation's argv, if present.
fn target_arg_value(args: &[String]) -> Option<String> {
    let mut index = 0;
    while index < args.len() {
        if let Some(value) = args[index].strip_prefix("--target=") {
            return Some(value.to_string());
        }
        if args[index] == "--target" {
            return args.get(index + 1).cloned();
        }
        index += 1;
    }
    None
}

/// Label of the in-place format pass `soldr lint` runs before the `fmt`
/// check when it may fix (soldr#4006).
pub(crate) const FMT_FIX_LABEL: &str = "fmt-fix";

/// The files `cargo fmt -- -l` rewrote. The step log also holds the child's
/// stderr, so only Rust source paths count.
pub(crate) fn fixed_files(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| line.ends_with(".rs"))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_extra_tool_fails_naming_the_tool() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let error = plan_extras(
            &[vec![
                "definitely-not-a-real-tool-3958".into(),
                "check".into(),
            ]],
            tmp.path(),
        )
        .expect_err("missing tool");
        let message = error.to_string();
        assert!(
            message.contains("definitely-not-a-real-tool-3958"),
            "{message}"
        );
        assert!(message.contains("not found"), "{message}");
    }

    #[test]
    fn extra_with_a_path_resolves_against_the_workspace_root() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        std::fs::create_dir_all(tmp.path().join("ci")).expect("dir");
        std::fs::write(tmp.path().join("ci/guard.sh"), "").expect("script");
        let planned = plan_extras(&[vec!["ci/guard.sh".into(), "--strict".into()]], tmp.path())
            .expect("plan");
        let PlannedStep::Extra(extra) = &planned[0] else {
            panic!("expected an extra step");
        };
        assert_eq!(extra.program, tmp.path().join("ci/guard.sh"));
        assert_eq!(extra.args, vec!["--strict".to_string()]);
        assert_eq!(extra.cwd, tmp.path());
        assert_eq!(planned[0].label(), "ci/guard.sh --strict");
    }
}
