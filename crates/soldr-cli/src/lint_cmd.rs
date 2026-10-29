//! Cache-aware unified validation command (soldr#1721).
//!
//! The command deliberately keeps compiler-bearing work on Soldr's cargo
//! front door while spawning dependency-only checks as cache-disabled child
//! Soldr commands. That preserves the pinned toolchain and managed tool
//! resolution without compiler-cache startup for deny, audit, or machete.

use crate::cargo_front_door;
use crate::core::SoldrError;
use crate::current_soldr_binary;
use crate::lint_ci;
use crate::lint_ci::model::OutputFormat;
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

    fn rust_steps(
        &self,
        all_features: bool,
        cross_targets: &[String],
    ) -> Result<Vec<Vec<String>>, SoldrError> {
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

        let mut steps = vec![fmt, clippy];

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
            steps.push(cross_clippy);
        }

        let mut dylint = vec!["dylint".into(), "--all".into(), "--".into()];
        dylint.extend(["--workspace".into(), "--all-targets".into()]);
        dylint.extend(compiler_scope);
        if self.cross_dylint {
            steps.push(dylint.clone());
            for target in cross_targets {
                let mut cross_dylint = dylint.clone();
                cross_dylint.extend(["--target".into(), target.clone()]);
                steps.push(cross_dylint);
            }
        } else {
            steps.push(dylint);
        }

        Ok(steps)
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
    let plan = LintPlan::parse(args)?;
    match plan.mode {
        LintMode::Rust => {
            // soldr#3378: resolved (and validated) before any compile starts.
            let cross_targets = plan.resolve_cross_targets()?;
            run_compile_steps(
                plan.rust_steps(false, &cross_targets)?,
                cache_enabled,
                trust_inherited_soldr_env,
            )
            .await
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
            let code = run_compile_steps(
                plan.rust_steps(true, &cross_targets)?,
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
            run_compile_steps(
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

async fn run_compile_steps(
    steps: Vec<Vec<String>>,
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    for args in steps {
        let dylint = cargo_front_door::first_cargo_subcommand(&args) == Some("dylint");
        let cross_target = target_arg_value(&args);
        if !dylint {
            if let Some(target) = &cross_target {
                // Clippy only needs the target's std, not a full cross linker.
                // `rustup_add_target` is idempotent, a no-op for the host triple,
                // and routes through soldr's managed rustup/cargo homes rather
                // than a bare `rustup` on PATH.
                crate::prepare_cmd::rustup_add_target(target)?;
            }
        }
        let code =
            cargo_front_door::run_cargo_front_door(&args, cache_enabled, trust_inherited_soldr_env)
                .await?;
        if code != 0 {
            if let Some(target) = target_arg_value(&args) {
                let command = if dylint { "dylint" } else { "clippy" };
                eprintln!("soldr lint: {command} failed for target {target}");
            }
            return Ok(code);
        }
    }
    Ok(0)
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
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn default_suite_has_one_clippy_scope_without_check() {
        let plan = LintPlan::parse(&[]).unwrap();
        assert_eq!(plan.mode, LintMode::Rust);
        assert_eq!(
            plan.rust_steps(false, &[]).unwrap(),
            vec![
                strings(&["fmt", "--all", "--", "--check"]),
                strings(&[
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings"
                ]),
                strings(&["dylint", "--all", "--", "--workspace", "--all-targets"]),
            ]
        );
    }

    #[test]
    fn dylint_stage_passes_the_callers_target_scope_through() {
        // soldr#3930. A pre-expansion Dylint must not see test or bench
        // targets, so `--lib --bins` replaces the forced `--all-targets`.
        let plan = LintPlan::parse(&strings(&["rust", "--lib", "--bins"])).unwrap();
        let steps = plan.rust_steps(false, &[]).unwrap();
        assert_eq!(
            steps[2],
            strings(&["dylint", "--all", "--", "--workspace", "--lib", "--bins"])
        );
    }

    #[test]
    fn dylint_stage_keeps_other_selectors_without_widening() {
        let plan =
            LintPlan::parse(&strings(&["rust", "--tests", "--package", "soldr-cli"])).unwrap();
        let steps = plan.rust_steps(false, &[]).unwrap();
        assert_eq!(
            steps[2],
            strings(&[
                "dylint",
                "--all",
                "--",
                "--workspace",
                "--tests",
                "--package",
                "soldr-cli"
            ])
        );
        assert!(!steps[2].contains(&"--all-targets".to_string()));
    }

    #[test]
    fn target_selector_detection_covers_both_spellings() {
        for scope in [
            strings(&["--lib"]),
            strings(&["--bin=soldr"]),
            strings(&["--example", "demo"]),
            strings(&["--all-targets"]),
        ] {
            assert!(has_target_selector(&scope), "{scope:?}");
        }
        for scope in [
            Vec::new(),
            strings(&["--package", "soldr-cli"]),
            strings(&["--target", "x86_64-unknown-linux-musl"]),
            strings(&["--all-features"]),
        ] {
            assert!(!has_target_selector(&scope), "{scope:?}");
        }
    }

    #[test]
    fn all_suite_uses_all_features_for_compiler_steps() {
        let plan = LintPlan::parse(&strings(&["all", "--package", "soldr-cli"])).unwrap();
        let rust = plan.rust_steps(true, &[]).unwrap();
        assert!(rust[1].contains(&"--all-features".into()));
        assert!(rust[2].contains(&"--all-features".into()));
        let exhaustive = plan.exhaustive_steps().unwrap();
        assert_eq!(exhaustive[0][0], "udeps");
        assert!(exhaustive[0].contains(&"--all-features".into()));
        assert_eq!(exhaustive[1], strings(&["semver-checks"]));
    }

    #[test]
    fn dependency_suite_is_limited_to_shared_manifest_scope() {
        let plan = LintPlan::parse(&strings(&["deps", "--manifest-path", "Cargo.toml"])).unwrap();
        assert_eq!(
            plan.dependency_steps().unwrap(),
            vec![
                strings(&["--manifest-path", "Cargo.toml", "deny", "check"]),
                strings(&["--manifest-path", "Cargo.toml", "audit"]),
                strings(&["--manifest-path", "Cargo.toml", "machete"]),
            ]
        );
        let invalid = LintPlan::parse(&strings(&["deps", "--all-features"])).unwrap();
        assert!(invalid.dependency_steps().is_err());
    }

    #[test]
    fn unknown_suite_is_rejected() {
        let error = LintPlan::parse(&strings(&["everything"])).unwrap_err();
        assert!(error.to_string().contains("unknown suite"));
    }

    #[test]
    fn ci_suite_is_parsed_with_default_human_format() {
        let plan = LintPlan::parse(&strings(&["ci"])).unwrap();
        assert_eq!(plan.mode, LintMode::Ci);
        assert_eq!(plan.ci_format, OutputFormat::Human);
        assert!(plan.scope.is_empty());
    }

    #[test]
    fn ci_suite_parses_format_flag_both_spellings() {
        let split = LintPlan::parse(&strings(&["ci", "--format", "json"])).unwrap();
        assert_eq!(split.mode, LintMode::Ci);
        assert_eq!(split.ci_format, OutputFormat::Json);
        let joined = LintPlan::parse(&strings(&["ci", "--format=json"])).unwrap();
        assert_eq!(joined.ci_format, OutputFormat::Json);
    }

    #[test]
    fn ci_suite_rejects_cargo_scope_flags() {
        // `--package` is a cargo scope flag; the ci suite must not accept it.
        let error = LintPlan::parse(&strings(&["ci", "--package", "soldr-cli"])).unwrap_err();
        assert!(error.to_string().contains("unexpected argument"));
        let bad_format = LintPlan::parse(&strings(&["ci", "--format", "yaml"])).unwrap_err();
        assert!(bad_format.to_string().contains("unknown --format"));
    }

    #[test]
    fn all_suite_mode_is_all() {
        // `lint all` must reach LintMode::All, which now also runs the CI
        // suite before the compile/dep steps.
        let plan = LintPlan::parse(&strings(&["all"])).unwrap();
        assert_eq!(plan.mode, LintMode::All);
    }

    /// Write a minimal `Cargo.toml` fixture declaring `targets` (or none,
    /// when `targets` is `None`) and return its path.
    fn write_manifest_fixture(dir: &std::path::Path, targets: Option<&[&str]>) -> String {
        let body = match targets {
            Some(list) => format!(
                "[workspace]\n\n[workspace.metadata.soldr]\ntargets = [{}]\n",
                list.iter()
                    .map(|t| format!("{t:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => "[package]\nname = \"thing\"\nversion = \"0.1.0\"\n".to_string(),
        };
        let path = dir.join("Cargo.toml");
        std::fs::write(&path, body).expect("write fixture");
        path.to_str().expect("utf8 path").to_string()
    }

    #[test]
    fn declared_targets_produce_one_clippy_per_target() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let manifest = write_manifest_fixture(
            tmp.path(),
            Some(&["x86_64-pc-windows-msvc", "aarch64-apple-darwin"]),
        );
        let plan = LintPlan::parse(&strings(&["rust", "--manifest-path", &manifest])).unwrap();
        let cross = plan.resolve_cross_targets().unwrap();
        assert_eq!(
            cross,
            vec![
                "x86_64-pc-windows-msvc".to_string(),
                "aarch64-apple-darwin".to_string(),
            ]
        );
        let steps = plan.rust_steps(false, &cross).unwrap();
        // fmt, host clippy, two cross clippy steps, dylint.
        assert_eq!(steps.len(), 5);
        assert!(steps[2].contains(&"--target".to_string()));
        assert!(steps[2].contains(&"x86_64-pc-windows-msvc".to_string()));
        assert!(steps[3].contains(&"--target".to_string()));
        assert!(steps[3].contains(&"aarch64-apple-darwin".to_string()));
        assert_eq!(steps[4][0], "dylint", "Dylint stays host-only, last");
    }

    #[test]
    fn cross_dylint_checks_each_selected_target_with_the_host_pass_first() {
        let plan = LintPlan::parse(&strings(&[
            "rust",
            "--cross-dylint",
            "--target",
            "win-x64",
            "--target=mac-x64",
        ]))
        .unwrap();
        assert!(plan.scope.is_empty());
        let cross = plan.resolve_cross_targets().unwrap();
        let steps = plan.rust_steps(false, &cross).unwrap();
        assert_eq!(steps.len(), 7);
        assert_eq!(
            steps[4],
            strings(&["dylint", "--all", "--", "--workspace", "--all-targets"])
        );
        assert_eq!(
            steps[5],
            strings(&[
                "dylint",
                "--all",
                "--",
                "--workspace",
                "--all-targets",
                "--target",
                "x86_64-pc-windows-msvc"
            ])
        );
        assert_eq!(
            steps[6],
            strings(&[
                "dylint",
                "--all",
                "--",
                "--workspace",
                "--all-targets",
                "--target",
                "x86_64-apple-darwin"
            ])
        );
        assert!(LintPlan::parse(&strings(&["rust", "--cross-dylint", "--host-only"])).is_err());
        assert!(LintPlan::parse(&strings(&["deps", "--cross-dylint"])).is_err());
    }

    #[test]
    fn no_declared_targets_behaves_exactly_like_today() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let manifest = write_manifest_fixture(tmp.path(), None);
        let plan = LintPlan::parse(&strings(&["rust", "--manifest-path", &manifest])).unwrap();
        let cross = plan.resolve_cross_targets().unwrap();
        assert!(cross.is_empty());
        let steps = plan.rust_steps(false, &cross).unwrap();
        assert_eq!(steps.len(), 3, "fmt, clippy, dylint only");
    }

    #[test]
    fn host_only_flag_gives_todays_steps_exactly() {
        let plan = LintPlan::parse(&strings(&["rust", "--host-only"])).unwrap();
        assert!(plan.host_only);
        assert!(plan.scope.is_empty(), "--host-only must be stripped");
        // No filesystem access even though no --manifest-path was given: a
        // declared workspace list must never be consulted under --host-only.
        let cross = plan.resolve_cross_targets().unwrap();
        assert!(cross.is_empty());
        assert_eq!(
            plan.rust_steps(false, &cross).unwrap(),
            vec![
                strings(&["fmt", "--all", "--", "--check"]),
                strings(&[
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings"
                ]),
                strings(&["dylint", "--all", "--", "--workspace", "--all-targets"]),
            ]
        );
    }

    #[test]
    fn explicit_target_overrides_the_declared_list() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let manifest = write_manifest_fixture(tmp.path(), Some(&["x86_64-unknown-linux-musl"]));
        let plan = LintPlan::parse(&strings(&[
            "rust",
            "--manifest-path",
            &manifest,
            "--target",
            "win-x64",
        ]))
        .unwrap();
        assert_eq!(plan.explicit_targets, vec!["win-x64".to_string()]);
        let cross = plan.resolve_cross_targets().unwrap();
        // The declared musl target from the fixture must NOT appear -- only
        // the explicit override, resolved through the alias table.
        assert_eq!(cross, vec!["x86_64-pc-windows-msvc".to_string()]);
    }

    #[test]
    fn unknown_target_triple_is_a_clear_error() {
        // A single word (no hyphens) doesn't even look like a Rust triple, so
        // it can't slip through as an unrecognized-but-plausible passthrough
        // the way a triple-shaped typo would.
        let plan = LintPlan::parse(&strings(&["rust", "--target", "bogus"])).unwrap();
        let error = plan.resolve_cross_targets().unwrap_err();
        assert!(
            error.to_string().contains("bogus"),
            "error must name the bad value: {error}"
        );
    }

    #[test]
    fn host_triple_is_not_duplicated_as_a_cross_target() {
        let host = crate::pyo3_detect::host_triple().to_string();
        let plan = LintPlan::parse(&strings(&[
            "rust", "--target", &host, "--target", "win-x64",
        ]))
        .unwrap();
        let cross = plan.resolve_cross_targets().unwrap();
        assert_eq!(
            cross,
            vec!["x86_64-pc-windows-msvc".to_string()],
            "the host's own triple must be filtered out, not compiled twice"
        );
    }

    #[test]
    fn target_and_host_only_flags_are_rejected_outside_rust_and_all() {
        let error = LintPlan::parse(&strings(&["deps", "--target", "win-x64"])).unwrap_err();
        assert!(error.to_string().contains("only valid for"));
        let error = LintPlan::parse(&strings(&["deps", "--host-only"])).unwrap_err();
        assert!(error.to_string().contains("only valid for"));
    }

    #[test]
    fn host_only_cannot_combine_with_target() {
        let error =
            LintPlan::parse(&strings(&["rust", "--host-only", "--target", "win-x64"])).unwrap_err();
        assert!(error.to_string().contains("cannot be combined"));
    }

    // soldr#3390: `resolve_target_triples` used to prepend a hand-rolled
    // `"lint: --target `{value}`: "` prefix on top of the resolver's own
    // `"soldr build --target ..."`-worded message, doubling the input and
    // naming the wrong command. RED on `main`.
    #[test]
    fn an_unknown_target_names_the_lint_surface_exactly_once() {
        let error = resolve_target_triples(&strings(&["win-armm"])).unwrap_err();
        let message = error.to_string();
        assert!(
            message.starts_with("soldr lint --target `win-armm`"),
            "{message}"
        );
        assert_eq!(message.matches("win-armm").count(), 1, "{message}");
        assert!(!message.contains("soldr build"), "{message}");
    }
}
