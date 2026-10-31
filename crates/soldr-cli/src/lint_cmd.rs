//! Cache-aware unified validation command (soldr#1721).
//!
//! The command deliberately keeps compiler-bearing work on Soldr's cargo
//! front door while spawning dependency-only checks as cache-disabled child
//! Soldr commands. That preserves the pinned toolchain and managed tool
//! resolution without compiler-cache startup for deny, audit, or machete.

use crate::cargo_front_door;
use crate::cargo_metadata_soldr::LintStep;
use crate::core::SoldrError;
use crate::current_soldr_binary;
use crate::lint_ci;
use crate::lint_ci::model::OutputFormat;
use crate::lint_steps::{self, PlannedStep};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LintMode {
    Rust,
    Deps,
    /// soldr#2038 — CI/build-surface policy suite (`soldr lint ci`).
    Ci,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LintPlan {
    mode: LintMode,
    scope: Vec<String>,
    /// Output format for the `ci` suite; ignored by other suites.
    ci_format: OutputFormat,
    /// `--target <triple>` values (repeatable), in the order given. Overrides
    /// the workspace's declared `[workspace.metadata.soldr].targets` list.
    /// Only meaningful for the `rust`/`all` suites (soldr#3378).
    explicit_targets: Vec<String>,
    /// `--host-only`: skip cross-target Clippy entirely, even when the
    /// workspace declares targets. Only meaningful for `rust`/`all`.
    host_only: bool,
    /// Opt in to one Dylint workspace analysis pass per selected cross target.
    cross_dylint: bool,
    /// soldr#4006: reformat in place before the fmt check, so a fmt-only
    /// problem never costs a second run. Off under `--check` or in CI.
    fmt_fix: bool,
    /// `--check`: verify formatting only, never rewrite.
    check_only: bool,
}

impl LintPlan {
    fn parse(args: &[String]) -> Result<Self, SoldrError> {
        // The `ci` suite has its own tiny, non-cargo argument grammar
        // (`--format json|human`), so it is parsed before the cargo-scope
        // path to avoid its flags being misread as cargo scope flags.
        if args.first().map(String::as_str) == Some("ci") {
            return Self::parse_ci(&args[1..]);
        }

        let (mode, mut scope) = match args.first().map(String::as_str) {
            None => (LintMode::Rust, Vec::new()),
            Some("rust") => (LintMode::Rust, args[1..].to_vec()),
            Some("deps") => (LintMode::Deps, args[1..].to_vec()),
            Some("all") => (LintMode::All, args[1..].to_vec()),
            Some(value) if value.starts_with('-') => (LintMode::Rust, args.to_vec()),
            Some(value) => {
                return Err(SoldrError::Other(format!(
                    "lint: unknown suite {value:?}; expected rust, deps, ci, or all"
                )))
            }
        };

        if scope.iter().any(|arg| arg == "--") {
            return Err(SoldrError::Other(
                "lint: compiler arguments after -- are not supported; pass cargo scope flags before the suite".into(),
            ));
        }

        // soldr#3378: `--target`/`--host-only` are Soldr-level cross-compile
        // selectors, not cargo scope flags -- strip them before the remaining
        // scope reaches fmt/Clippy/Dylint verbatim.
        let cross_dylint = scope.iter().any(|arg| arg == "--cross-dylint");
        scope.retain(|arg| arg != "--cross-dylint");
        let check_only = scope.iter().any(|arg| arg == "--check");
        scope.retain(|arg| arg != "--check");
        let (explicit_targets, host_only) = extract_target_flags(&mut scope)?;
        if !matches!(mode, LintMode::Rust | LintMode::All)
            && (host_only || cross_dylint || !explicit_targets.is_empty())
        {
            return Err(SoldrError::Other(
                "lint: --target, --host-only, and --cross-dylint are only valid for the rust or all suites".into(),
            ));
        }
        if host_only && cross_dylint {
            return Err(SoldrError::Other(
                "lint: --cross-dylint cannot be combined with --host-only".into(),
            ));
        }

        Ok(Self {
            mode,
            scope,
            ci_format: OutputFormat::Human,
            explicit_targets,
            host_only,
            cross_dylint,
            fmt_fix: false,
            check_only,
        })
    }

    /// Parse the `ci` suite grammar: only `--format json|human` is accepted.
    fn parse_ci(args: &[String]) -> Result<Self, SoldrError> {
        let mut ci_format = OutputFormat::Human;
        let mut index = 0;
        while index < args.len() {
            let arg = args[index].as_str();
            let value = if arg == "--format" {
                let value = args.get(index + 1).ok_or_else(|| {
                    SoldrError::Other("lint ci: --format requires a value (json or human)".into())
                })?;
                index += 2;
                value.clone()
            } else if let Some(value) = arg.strip_prefix("--format=") {
                index += 1;
                value.to_string()
            } else {
                return Err(SoldrError::Other(format!(
                    "lint ci: unexpected argument {arg:?}; only --format json|human is supported"
                )));
            };
            ci_format = OutputFormat::parse(&value).ok_or_else(|| {
                SoldrError::Other(format!(
                    "lint ci: unknown --format {value:?}; expected json or human"
                ))
            })?;
        }
        Ok(Self {
            mode: LintMode::Ci,
            scope: Vec::new(),
            ci_format,
            explicit_targets: Vec::new(),
            host_only: false,
            cross_dylint: false,
            fmt_fix: false,
            check_only: false,
        })
    }

    /// Resolve the additional cross-compile target triples this plan's
    /// Clippy pass runs beyond the host (soldr#3378 — "declaring a target is
    /// the marker").
    ///
    /// Resolution order: `--host-only` -> none; `--target` (repeatable) ->
    /// overrides any declared list; otherwise the workspace's declared
    /// `[workspace.metadata.soldr].targets` (package-metadata fallback),
    /// honoring `--manifest-path` from the lint scope when present and
    /// defaulting to the current directory otherwise. Every value — declared
    /// or explicit — is resolved through the alias table, so a friendly
    /// spelling (`win-x64`) is accepted and an unknown triple is a clear
    /// error raised here, before any compile starts. The host's own triple
    /// is dropped so Clippy never gets a redundant `--target <host>` pass.
    fn resolve_cross_targets(&self) -> Result<Vec<String>, SoldrError> {
        if self.host_only {
            return Ok(Vec::new());
        }
        let raw = if !self.explicit_targets.is_empty() {
            self.explicit_targets.clone()
        } else {
            crate::cargo_metadata_soldr::declared_targets_from(&self.manifest_scope_dir()?)?
        };
        resolve_target_triples(&raw)
    }

    /// The manifest path/dir declared-target resolution should start from:
    /// `--manifest-path` out of the lint scope when present, else `cwd`.
    fn manifest_scope_dir(&self) -> Result<PathBuf, SoldrError> {
        let mut index = 0;
        while index < self.scope.len() {
            let arg = &self.scope[index];
            if arg == "--manifest-path" {
                if let Some(value) = self.scope.get(index + 1) {
                    return Ok(PathBuf::from(value));
                }
            } else if let Some(value) = arg.strip_prefix("--manifest-path=") {
                return Ok(PathBuf::from(value));
            }
            index += 1;
        }
        std::env::current_dir().map_err(|e| SoldrError::Other(format!("lint: cwd: {e}")))
    }

    /// The built-in steps as `(kind, label, argv)`, in today's order:
    /// fmt, host Clippy plus one Clippy per cross target, then Dylint.
    fn rust_step_args(
        &self,
        all_features: bool,
        cross_targets: &[String],
    ) -> Result<Vec<(LintStep, String, Vec<String>)>, SoldrError> {
        let mut compiler_scope = self.scope.clone();
        if all_features {
            add_all_features(&mut compiler_scope)?;
        }

        let mut fmt = vec!["fmt".into(), "--all".into()];
        fmt.extend(fmt_compatible_scope(&self.scope));
        fmt.extend(["--".into(), "--check".into()]);

        let mut clippy = vec![
            "clippy".into(),
            "--workspace".into(),
            "--all-targets".into(),
        ];
        clippy.extend(compiler_scope.iter().cloned());
        clippy.extend(["--".into(), "-D".into(), "warnings".into()]);

        let mut steps = Vec::new();
        if self.fmt_fix {
            let mut fix = vec!["fmt".into(), "--all".into()];
            fix.extend(fmt_compatible_scope(&self.scope));
            fix.extend(["--".into(), "-l".into()]);
            steps.push((LintStep::Fmt, lint_steps::FMT_FIX_LABEL.to_string(), fix));
        }
        steps.extend([
            (LintStep::Fmt, "fmt".to_string(), fmt),
            (LintStep::Clippy, "clippy".to_string(), clippy),
        ]);

        // soldr#3378: one additional Clippy pass per declared/explicit cross
        // target, each landing under cargo's own `target/<triple>/`
        // subdirectory so the host tree's fingerprints stay untouched.
        for target in cross_targets {
            let mut cross_clippy = vec![
                "clippy".into(),
                "--workspace".into(),
                "--all-targets".into(),
                "--target".into(),
                target.clone(),
            ];
            cross_clippy.extend(compiler_scope.iter().cloned());
            cross_clippy.extend(["--".into(), "-D".into(), "warnings".into()]);
            steps.push((
                LintStep::Clippy,
                format!("clippy --target {target}"),
                cross_clippy,
            ));
        }

        // soldr#3930: a workspace whose Dylint is pre-expansion (it forbids
        // `#[cfg]` outside the platform boundary) cannot be linted with
        // `--all-targets`, because test and bench targets are full of it.
        // When the caller named a target scope, pass exactly that scope; only
        // an unscoped call keeps today's `--all-targets`.
        let mut dylint = vec!["dylint".into(), "--all".into(), "--".into()];
        dylint.push("--workspace".into());
        if !has_target_selector(&compiler_scope) {
            dylint.push("--all-targets".into());
        }
        dylint.extend(compiler_scope);
        if self.cross_dylint {
            steps.push((LintStep::Dylint, "dylint".to_string(), dylint.clone()));
            for target in cross_targets {
                let mut cross_dylint = dylint.clone();
                cross_dylint.extend(["--target".into(), target.clone()]);
                steps.push((
                    LintStep::Dylint,
                    format!("dylint --target {target}"),
                    cross_dylint,
                ));
            }
        } else {
            steps.push((LintStep::Dylint, "dylint".to_string(), dylint));
        }

        Ok(steps)
    }

    /// The built-in steps for the unconfigured default (all of them).
    #[cfg(test)]
    fn rust_steps(
        &self,
        all_features: bool,
        cross_targets: &[String],
    ) -> Result<Vec<Vec<String>>, SoldrError> {
        Ok(self
            .rust_step_args(all_features, cross_targets)?
            .into_iter()
            .map(|(_, _, args)| args)
            .collect())
    }

    /// The rust-suite plan (soldr#3958): the declared built-ins in declared
    /// order (all of them when `[workspace.metadata.soldr.lint]` is absent),
    /// then the declared `extra` repo commands. Invalid metadata and missing
    /// extra tools fail here, before any step runs.
    fn rust_plan(
        &self,
        all_features: bool,
        cross_targets: &[String],
    ) -> Result<Vec<PlannedStep>, SoldrError> {
        let (config, root) =
            crate::cargo_metadata_soldr::lint_config_from(&self.manifest_scope_dir()?)?;
        let built_ins = self.rust_step_args(all_features, cross_targets)?;
        let mut plan = Vec::new();
        for kind in &config.steps {
            plan.extend(built_ins.iter().filter(|(step, _, _)| step == kind).map(
                |(_, label, args)| PlannedStep::Cargo {
                    label: label.clone(),
                    args: args.clone(),
                },
            ));
        }
        let root = match root {
            Some(root) => root,
            None => {
                std::env::current_dir().map_err(|e| SoldrError::Other(format!("lint: cwd: {e}")))?
            }
        };
        plan.extend(lint_steps::plan_extras(&config.extra, &root)?);
        Ok(plan)
    }

    fn dependency_steps(&self) -> Result<Vec<Vec<String>>, SoldrError> {
        let mut index = 0;
        while index < self.scope.len() {
            let arg = &self.scope[index];
            if arg == "--manifest-path" {
                if self.scope.get(index + 1).is_none() {
                    return Err(SoldrError::Other(
                        "lint deps: --manifest-path requires a path".into(),
                    ));
                }
                index += 2;
                continue;
            }
            if arg.starts_with("--manifest-path=") {
                index += 1;
                continue;
            }
            return Err(SoldrError::Other(
                "lint deps accepts only an optional --manifest-path scope; dependency tools do not share Cargo package/feature flags".into(),
            ));
        }
        Ok(vec![
            prepend("deny", &["check"], &self.scope),
            prepend("audit", &[], &self.scope),
            prepend("machete", &[], &self.scope),
        ])
    }

    fn exhaustive_steps(&self) -> Result<Vec<Vec<String>>, SoldrError> {
        let mut scope = self.scope.clone();
        add_all_features(&mut scope)?;
        let mut udeps = vec!["udeps".into(), "--workspace".into(), "--all-targets".into()];
        udeps.extend(scope);
        Ok(vec![udeps, vec!["semver-checks".into()]])
    }
}

/// Does this cargo scope name the targets to build?
///
/// The Dylint stage widens an unscoped call to `--all-targets`; a scope that
/// already selects targets (`--lib --bins` for a pre-expansion boundary lint,
/// soldr#3930) must be passed through as given, never widened. Both the bare
/// and the `--flag=value` spellings count.
fn has_target_selector(scope: &[String]) -> bool {
    const SELECTORS: [&str; 10] = [
        "--lib",
        "--bins",
        "--bin",
        "--tests",
        "--test",
        "--benches",
        "--bench",
        "--examples",
        "--example",
        "--all-targets",
    ];
    scope.iter().any(|arg| {
        let flag = arg.split('=').next().unwrap_or(arg.as_str());
        SELECTORS.contains(&flag)
    })
}

/// Extract Soldr's `--target <value>` (repeatable, both `--target` /
/// `--target=` spellings) and `--host-only` flags out of a cargo scope,
/// leaving everything else in place and order-preserved. Values are returned
/// raw (alias or triple, not yet resolved) so the caller can decide how to
/// treat parse failures independently of resolution failures.
fn extract_target_flags(scope: &mut Vec<String>) -> Result<(Vec<String>, bool), SoldrError> {
    let mut explicit = Vec::new();
    let mut host_only = false;
    let mut out = Vec::with_capacity(scope.len());
    let mut index = 0;
    while index < scope.len() {
        let arg = scope[index].clone();
        if arg == "--host-only" {
            host_only = true;
            index += 1;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--target=") {
            explicit.push(value.to_string());
            index += 1;
            continue;
        }
        if arg == "--target" {
            let value = scope
                .get(index + 1)
                .cloned()
                .ok_or_else(|| SoldrError::Other("lint: --target requires a value".into()))?;
            explicit.push(value);
            index += 2;
            continue;
        }
        out.push(arg);
        index += 1;
    }
    *scope = out;
    if host_only && !explicit.is_empty() {
        return Err(SoldrError::Other(
            "lint: --host-only cannot be combined with --target".into(),
        ));
    }
    Ok((explicit, host_only))
}

/// Resolve raw `--target` values (soldr aliases or bare Rust triples) to Rust
/// triples, dropping the host's own triple (Clippy already type-checks the
/// host natively) and de-duplicating while preserving order. An alias/triple
/// that the resolver does not recognize is a hard error naming the bad value
/// — soldr#3378 requires this to surface before any compile starts.
fn resolve_target_triples(raw: &[String]) -> Result<Vec<String>, SoldrError> {
    let host = crate::pyo3_detect::host_triple();
    let mut resolved = Vec::with_capacity(raw.len());
    for value in raw {
        let target = crate::target_alias::resolve_soldr_target(value)
            .map_err(|err| err.into_soldr_error(crate::target_alias::TargetSurface::Lint))?;
        if target.rust_triple == host {
            continue;
        }
        if !resolved.contains(&target.rust_triple) {
            resolved.push(target.rust_triple);
        }
    }
    Ok(resolved)
}

fn prepend(subcommand: &str, fixed: &[&str], scope: &[String]) -> Vec<String> {
    let mut args = Vec::with_capacity(1 + fixed.len() + scope.len());
    args.extend(scope.iter().cloned());
    args.push(subcommand.into());
    args.extend(fixed.iter().map(|arg| (*arg).into()));
    args
}

fn fmt_compatible_scope(scope: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < scope.len() {
        let arg = &scope[index];
        if arg == "--manifest-path" || arg == "--package" || arg == "-p" {
            out.push(arg.clone());
            if let Some(value) = scope.get(index + 1) {
                out.push(value.clone());
                index += 1;
            }
        } else if arg.starts_with("--manifest-path=") || arg.starts_with("--package=") {
            out.push(arg.clone());
        }
        index += 1;
    }
    out
}

fn add_all_features(scope: &mut Vec<String>) -> Result<(), SoldrError> {
    if scope.iter().any(|arg| arg == "--no-default-features") {
        return Err(SoldrError::Other(
            "lint all always validates every feature and cannot be combined with --no-default-features".into(),
        ));
    }
    if !scope.iter().any(|arg| arg == "--all-features") {
        scope.push("--all-features".into());
    }
    Ok(())
}

pub(crate) async fn run_lint(
    args: &[String],
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    let mut plan = LintPlan::parse(args)?;
    // soldr#4006: CI verifies formatting; it never rewrites the checkout.
    plan.fmt_fix = !plan.check_only && !crate::core::flag("CI");
    match plan.mode {
        LintMode::Rust => {
            // soldr#3378: resolved (and validated) before any compile starts.
            let cross_targets = plan.resolve_cross_targets()?;
            let steps = plan.rust_plan(false, &cross_targets)?;
            if cargo_front_door::porcelain_lint::requested() {
                // soldr#3958: one folded summary for the whole plan.
                return lint_steps::run_porcelain(steps, cache_enabled, trust_inherited_soldr_env);
            }
            lint_steps::run_planned_steps(steps, cache_enabled, trust_inherited_soldr_env).await
        }
        LintMode::Deps => run_dependency_steps(plan.dependency_steps()?, trust_inherited_soldr_env),
        LintMode::Ci => run_ci_suite(plan.ci_format),
        LintMode::All => {
            // soldr#2038 — run the pure-filesystem CI policy scan first so a
            // policy violation fails fast without starting any compile.
            let code = run_ci_suite(plan.ci_format)?;
            if code != 0 {
                return Ok(code);
            }
            // soldr#3378: resolved (and validated) before any compile starts.
            let cross_targets = plan.resolve_cross_targets()?;
            let code = lint_steps::run_planned_steps(
                plan.rust_plan(true, &cross_targets)?,
                cache_enabled,
                trust_inherited_soldr_env,
            )
            .await?;
            if code != 0 {
                return Ok(code);
            }
            let code = run_dependency_steps(plan.dependency_steps()?, trust_inherited_soldr_env)?;
            if code != 0 {
                return Ok(code);
            }
            lint_steps::run_compile_steps(
                plan.exhaustive_steps()?,
                cache_enabled,
                trust_inherited_soldr_env,
            )
            .await
        }
    }
}

/// Run the `ci` policy suite (soldr#2038). Pure filesystem scan over the
/// current directory: no cargo front door, no compiler cache, no workspace
/// requirement.
fn run_ci_suite(format: OutputFormat) -> Result<i32, SoldrError> {
    let root = std::env::current_dir().map_err(|e| {
        SoldrError::Other(format!("lint ci: cannot resolve current directory: {e}"))
    })?;
    let code = lint_ci::run(&root, format)?;
    // The suite always renders a report (findings or a clean summary), so a
    // non-zero exit is never unexplained — suppress the exit-guard's #2024
    // "soldr emitted no diagnostic" annotation.
    crate::exit_guard::mark_spoke();
    Ok(code)
}

fn run_dependency_steps(
    steps: Vec<Vec<String>>,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    let soldr = current_soldr_binary()?;
    let mut children = Vec::with_capacity(steps.len());
    for args in steps {
        let label = format!("cargo {}", args.join(" "));
        let mut command = Command::new(&soldr);
        command.arg("--no-cache");
        if trust_inherited_soldr_env {
            command.arg("--trust-inherited-soldr-env");
        }
        command.arg("cargo").args(args);
        cargo_front_door::configure_cargo_child_for_timeout(&mut command);
        command.env(cargo_front_door::INHERIT_PARENT_PROCESS_GROUP_ENV, "1");
        // soldr#2726: these children inherit soldr's stdio, so whatever they
        // report -- an advisory from `cargo audit`, a denied licence from
        // `cargo deny` -- reaches the user through our streams, and
        // `wait_for_parallel_children` adds a per-leg pid + exit status of
        // its own. That is the "spawns a child that inherits stdio" case
        // `exit_guard` asks callers to record. Without it every ordinary
        // `lint deps` failure was followed by "soldr emitted no diagnostic
        // and ran no child process ... this is a fault in soldr itself",
        // directly under lines naming three child pids. Marked at the spawn
        // rather than on the exit code, matching soldr#2718.
        crate::exit_guard::mark_spoke();
        // soldr#3098: spawns share, staged writes exclude. Held only across
        // the spawn call (dropped right after), and kept as a plain statement
        // so `tests/daemon/inherited_stdio_spawns_mark_spoke.rs` still finds
        // the exact child-start line it pins.
        let spawn_guard = crate::core::spawn_exclusion::spawn_shared();
        let child = command.spawn();
        drop(spawn_guard);
        let child = child.map_err(|error| {
            SoldrError::Other(format!(
                "lint deps: failed to start child Soldr process for `{label}`: {error}"
            ))
        })?;
        children.push((label, child));
    }
    wait_for_parallel_children(&mut children)
}

/// Waits for every `(label, child)` pair, reporting each child's exit on
/// stderr as it completes. The per-leg line is diagnostic load-bearing, not
/// decoration: soldr#2589's Windows lane loses one dependency-check
/// invocation while `lint deps` still exits 0, and without the observed
/// pid + exit status per leg a recurrence cannot distinguish "child ran and
/// its effects vanished" from "child never ran but reported success".
fn wait_for_parallel_children(children: &mut [(String, Child)]) -> Result<i32, SoldrError> {
    let mut reported = vec![false; children.len()];
    loop {
        let mut complete = 0;
        for index in 0..children.len() {
            let (label, child) = &mut children[index];
            if let Some(status) = child.try_wait()? {
                complete += 1;
                if !reported[index] {
                    reported[index] = true;
                    eprintln!(
                        "soldr: lint deps: `{label}` (pid {}) exited with {status}",
                        child.id()
                    );
                }
                if !status.success() {
                    let failed_id = child.id();
                    for (other_label, other) in children.iter_mut() {
                        if other.id() != failed_id {
                            // soldr#2605: this outcome used to be discarded. A
                            // cancellation that only reached the direct child
                            // leaves a descendant running -- and holding the
                            // stdio it inherited -- while this loop still
                            // reports a clean cancel. Five sightings produced
                            // no evidence beyond a wall-clock number because
                            // nothing here ever said which kind of kill it got.
                            match cargo_front_door::kill_cargo_process_tree(other) {
                                Ok(kind) => eprintln!(
                                    "soldr: lint deps: `{other_label}` (pid {}) {kind}",
                                    other.id()
                                ),
                                Err(error) => eprintln!(
                                    "soldr: lint deps: `{other_label}` (pid {}) could not be                                      terminated: {error}",
                                    other.id()
                                ),
                            }
                            if let Ok(other_status) =
                                crate::platform::wait::wait_for(other, crate::platform::wait::PROBE)
                            {
                                eprintln!(
                                    "soldr: lint deps: `{other_label}` (pid {}) canceled with {other_status}",
                                    other.id()
                                );
                            }
                        }
                    }
                    return Ok(status.code().unwrap_or(1));
                }
            }
        }
        if complete == children.len() {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
#[path = "lint_cmd_tests.rs"]
mod tests;
