use super::model::{
    CiTestPlan, CompileDomain, CompilerExecutionGroup, CookDecision, DylintTargetTrees, Invocation,
    PlanScope, ResourceLimits, Stage, SubsumedStep, WorkspaceMetadata, PLAN_SCHEMA_VERSION,
};
use crate::core::{SoldrError, TargetTriple};
use std::path::{Path, PathBuf};

const DYLINTS: &[&str] = &[
    "ban_raw_process_creation",
    "ban_raw_network_access",
    "ban_raw_local_socket_name",
    "ban_raw_ipc_transport",
    "ban_platform_cfg_outside_boundary",
    "ban_raw_env_flag",
    "ban_swallowed_child_stdio",
    "ban_raw_sync_primitive",
];

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) async fn freeze(
    invocation: &Invocation,
    cache_enabled: bool,
) -> Result<CiTestPlan, SoldrError> {
    let cwd = std::env::current_dir()?;
    let root = crate::cook::resolve_manifest_dir(&cwd)?;
    reject_non_host_target(&root)?;
    let host = TargetTriple::host()?.triple();
    let target = invocation
        .requested_target
        .clone()
        .unwrap_or_else(|| host.clone());
    // A matching OS and architecture do not prove ABI compatibility: a musl
    // host need not have the glibc loader for a GNU target, for example.
    // Additional host-executable triples need an explicit verified registry.
    let host_executable = target == host;
    if !host_executable && !invocation.no_run {
        return Err(SoldrError::Other(format!(
            "soldr ci-test: no executor registered for target {target}; use `soldr ci-test --no-run --target {target}` to compile and archive without executing target tests"
        )));
    }
    let manifest = crate::core::read_rust_toolchain_manifest(&root)?;
    let stable_toolchain = manifest.channel.ok_or_else(|| {
        SoldrError::Other(format!(
            "soldr ci-test: {} has no pinned [toolchain].channel",
            root.join("rust-toolchain.toml").display()
        ))
    })?;
    let nightly = resolve_dylint_plan(&root, &host).await?;
    let target_root = cargo_target_root(&root)?;
    let stable_target = target_root.join(&target);
    let archive_file = invocation.no_run.then(|| {
        target_root
            .join("ci-test")
            .join(format!("{target}-tests.tar.zst"))
            .display()
            .to_string()
    });
    let config = cargo_config_paths(&root);
    let rustflags = effective_rustflags();
    let wrapper = wrapper_identity(cache_enabled);
    // Absence is load-bearing: ci-test schedules work, but it does not invent a
    // one-thread Cargo/compiler policy. Explicit values are frozen verbatim.
    let cargo_build_jobs = inherited_limit("CARGO_BUILD_JOBS");
    let soldr_jobs = inherited_limit("SOLDR_JOBS");
    // soldr#2885: an unset NEXTEST_TEST_THREADS is measured from CPUs and
    // available memory; an explicit value is frozen verbatim, with a warning
    // when the measurement says it cannot fit.
    let test_admission =
        super::test_admission::resolve(&super::test_admission::AdmissionInputs::from_process());
    if let Some(warning) = &test_admission.warning {
        eprintln!("warning: {warning}");
    }
    let nextest_test_threads = test_admission.effective_test_threads.clone();
    let dylint_key = canonical_channel(&nightly.channel, &host);
    let dylint_libraries = target_root
        .join("dylint")
        .join("libraries")
        .join(&dylint_key);
    let dylint_analysis = target_root.join("dylint").join("target").join(&dylint_key);
    let dylint_tests = target_root.join("dylint").join("tests").join(&dylint_key);
    // soldr#2936: the census is taken while the plan is frozen, i.e. before a
    // single stage compiles, so a workspace that is about to link 98 test
    // binaries says so up front rather than after the disk is gone. Reading
    // the filesystem (not `cargo metadata`) keeps it in the same idiom as the
    // manifest and `.cargo/config.toml` reads above, and costs no subprocess.
    let test_target_count = super::test_targets::count_workspace_test_targets(&root);
    let test_target_warn_threshold = super::test_targets::warn_threshold();
    let scope_args = invocation.scope.cargo_args();
    let mut stages = Vec::new();
    let mut fmt = vec!["fmt".into()];
    if invocation.scope.packages.is_empty() {
        fmt.push("--all".into());
    }
    fmt.extend(fmt_scope(&invocation.scope));
    fmt.extend(["--".into(), "--check".into()]);
    stages.push(stage(
        "rustfmt",
        "stable",
        PREFLIGHT,
        cargo_command(&fmt, &[]),
        &[],
        &root,
    ));
    stages.push(stage(
        "lint-ci",
        "policy",
        PREFLIGHT,
        vec!["soldr".into(), "lint".into(), "ci".into()],
        &[],
        &root,
    ));
    let mut clippy = vec!["clippy".into()];
    clippy.extend(workspace_selection(&invocation.scope));
    clippy.extend(["--all-targets".into(), "--target".into(), target.clone()]);
    clippy.extend(scope_args.clone());
    clippy.extend(["--".into(), "-D".into(), "warnings".into()]);
    stages.push(stage(
        "clippy",
        "stable",
        COMPILER,
        cargo_command(&clippy, &[]),
        &["rustfmt", "lint-ci"],
        &root,
    ));
    let library_names: Vec<String> = DYLINTS
        .iter()
        .map(|lint| format!("dylint-library-{lint}"))
        .collect();
    for (index, lint) in DYLINTS.iter().enumerate() {
        let dependency = if index == 0 {
            "clippy"
        } else {
            library_names[index - 1].as_str()
        };
        stages.push(stage(
            &library_names[index],
            "dylint-libraries",
            COMPILER,
            cargo_command(
                &[
                    "build",
                    "--manifest-path",
                    "Cargo.toml",
                    "--target-dir",
                    dylint_libraries.to_string_lossy().as_ref(),
                    "--profile",
                    "release",
                ],
                &[],
            ),
            &[dependency],
            &root.join("dylints").join(lint),
        ));
    }
    // soldr#3460: pre-compile the UI tests' third-party dependency layer
    // (dylint_testing -> compiletest_rs, git2, libgit2-sys, plus dylint's own
    // build script) inside the Dylint branch, between the libraries and the
    // workspace analysis. That is still the compile-only window -- Fresh
    // Nextest starts only after `nextest-compile`, and the Dylint branch is
    // shorter than the Nextest one -- which is what soldr#3042 required: none
    // of these ~137 units may compile beside running tests. It used to be a
    // serial workflow step ahead of `ci-test`, on the lane's critical path.
    // One cook per lint crate: each is its own Cargo workspace whose cook key
    // hashes its own manifest and lockfile.
    let cook_names: Vec<String> = DYLINTS
        .iter()
        .map(|lint| format!("dylint-cook-{lint}"))
        .collect();
    if !invocation.no_run {
        for (index, lint) in DYLINTS.iter().enumerate() {
            let dependency = if index == 0 {
                library_names
                    .last()
                    .expect("the frozen Dylint inventory is non-empty")
                    .as_str()
            } else {
                cook_names[index - 1].as_str()
            };
            stages.push(stage(
                &cook_names[index],
                "dylint-ui-tests",
                COMPILER,
                dylint_tests_cook_command(&target_root),
                &[dependency],
                &root.join("dylints").join(lint),
            ));
        }
    }
    let workspace_dependency = if invocation.no_run {
        library_names.last()
    } else {
        cook_names.last()
    }
    .expect("the frozen Dylint inventory is non-empty");
    let mut dylint = vec![
        "dylint".into(),
        "--no-build".into(),
        "--all".into(),
        "--".into(),
    ];
    dylint.extend(workspace_selection(&invocation.scope));
    dylint.push("--all-targets".into());
    dylint.extend(scope_args.clone());
    stages.push(stage(
        "dylint-workspace",
        "dylint-analysis",
        COMPILER,
        cargo_command(&dylint, &[]),
        &[workspace_dependency],
        &root,
    ));
    let dylint_test_names: Vec<String> = DYLINTS
        .iter()
        .map(|lint| format!("dylint-test-{lint}"))
        .collect();
    if !invocation.no_run {
        for (index, lint) in DYLINTS.iter().enumerate() {
            let dependency = if index == 0 {
                "dylint-workspace"
            } else {
                dylint_test_names[index - 1].as_str()
            };
            stages.push(stage(
                &dylint_test_names[index],
                "dylint-ui-tests",
                COMPILER,
                cargo_command(
                    &[
                        "test",
                        "--manifest-path",
                        "Cargo.toml",
                        "--target-dir",
                        dylint_tests.to_string_lossy().as_ref(),
                    ],
                    &[],
                ),
                &[dependency],
                &root.join("dylints").join(lint),
            ));
        }
    }
    let mut nextest_args = workspace_selection(&invocation.scope);
    nextest_args.extend([
        "--lib".into(),
        "--tests".into(),
        "--target".into(),
        target.clone(),
        "--test-threads".into(),
        nextest_test_threads.clone(),
    ]);
    nextest_args.extend(scope_args.clone());

    // Compile the selected test binaries before executing or archiving them.
    let mut nextest_compile = vec!["nextest".into(), "run".into(), "--no-run".into()];
    nextest_compile.extend(nextest_args.clone());
    stages.push(stage(
        "nextest-compile",
        "stable",
        COMPILER,
        cargo_command(&nextest_compile, &[]),
        &["clippy"],
        &root,
    ));
    // soldr#3100: one red test must not hide the rest of the suite. Nextest's
    // default fail-fast stopped scheduling after the first failure, so a red
    // run reported only the tests executed until then. The run stage keeps
    // going; it still exits non-zero when any test failed. `--no-run` makes
    // the flag meaningless on the compile stage, so it is not shared.
    if !invocation.no_run {
        let mut nextest = vec!["nextest".into(), "run".into(), "--no-fail-fast".into()];
        nextest.extend(nextest_args);
        stages.push(stage(
            "nextest",
            "stable",
            TEST,
            cargo_command(&nextest, &[]),
            &["nextest-compile"],
            &root,
        ));
    }
    let mut doctest = vec!["test".into()];
    doctest.extend(workspace_selection(&invocation.scope));
    doctest.extend(["--doc".into(), "--target".into(), target.clone()]);
    doctest.extend(scope_args.clone());
    let tail_dependencies = tail_join_dependencies(
        dylint_test_names
            .last()
            .expect("the frozen Dylint test inventory is non-empty"),
    );
    if invocation.no_run {
        let mut archive = vec!["nextest".into(), "archive".into()];
        archive.extend(workspace_selection(&invocation.scope));
        archive.extend(["--lib".into(), "--tests".into()]);
        archive.extend(["--target".into(), target.clone()]);
        archive.extend(scope_args.clone());
        archive.extend([
            "--archive-file".into(),
            archive_file.clone().expect("no-run has archive path"),
            "--archive-format".into(),
            "tar-zst".into(),
        ]);
        stages.push(stage(
            "nextest-archive",
            "stable",
            COMPILER,
            cargo_command(&archive, &[]),
            &["nextest-compile", "dylint-workspace"],
            &root,
        ));
    } else {
        stages.push(stage(
            "doctests",
            "rustdoc",
            COMPILER_AND_TEST,
            cargo_command(&doctest, &[]),
            &tail_dependencies,
            &root,
        ));
    }
    for (name, args) in [
        ("cargo-deny-bans", vec!["deny", "check", "bans"]),
        ("cargo-audit", vec!["audit"]),
        ("cargo-machete", vec!["machete"]),
    ] {
        let dependencies: &[&str] = if invocation.no_run {
            &["nextest-archive"]
        } else {
            &tail_dependencies
        };
        stages.push(stage(
            name,
            "policy",
            POLICY,
            cargo_command(&args, &[]),
            dependencies,
            &root,
        ));
    }
    let mut domains = vec![
        CompileDomain {
            id: "stable",
            family: "stable",
            toolchain: stable_toolchain.clone(),
            compiler_release: None,
            compiler_commit: None,
            target_triple: target.clone(),
            target_directory: stable_target.display().to_string(),
            profile: "test",
            rustflags: rustflags.clone(),
            cargo_config: config.clone(),
            wrapper_identity: wrapper.clone(),
        },
        CompileDomain {
            id: "dylint-libraries",
            family: "dylint-nightly",
            toolchain: nightly.channel.clone(),
            compiler_release: Some(nightly.compiler_release.clone()),
            compiler_commit: Some(nightly.compiler_commit.clone()),
            target_triple: host.clone(),
            target_directory: dylint_libraries.display().to_string(),
            profile: "release",
            rustflags: rustflags.clone(),
            cargo_config: config.clone(),
            wrapper_identity: wrapper.clone(),
        },
        CompileDomain {
            id: "dylint-analysis",
            family: "dylint-nightly",
            toolchain: nightly.channel.clone(),
            compiler_release: Some(nightly.compiler_release.clone()),
            compiler_commit: Some(nightly.compiler_commit.clone()),
            target_triple: host.clone(),
            target_directory: dylint_analysis.display().to_string(),
            profile: "dev/check",
            rustflags: rustflags.clone(),
            cargo_config: config.clone(),
            wrapper_identity: wrapper.clone(),
        },
    ];
    if !invocation.no_run {
        domains.push(CompileDomain {
            id: "dylint-ui-tests",
            family: "dylint-nightly",
            toolchain: nightly.channel.clone(),
            compiler_release: Some(nightly.compiler_release.clone()),
            compiler_commit: Some(nightly.compiler_commit.clone()),
            target_triple: host.clone(),
            target_directory: dylint_tests.display().to_string(),
            profile: "test",
            rustflags: rustflags.clone(),
            cargo_config: config.clone(),
            wrapper_identity: wrapper.clone(),
        });
        domains.push(CompileDomain {
            id: "rustdoc",
            family: "rustdoc",
            toolchain: stable_toolchain,
            compiler_release: None,
            compiler_commit: None,
            target_triple: target.clone(),
            target_directory: stable_target.display().to_string(),
            profile: "test",
            rustflags,
            cargo_config: config.clone(),
            wrapper_identity: wrapper,
        });
    }
    let mut compiler_execution_groups = vec![
        group("stable-clippy", "stable", vec!["clippy".into()]),
        group("dylint-libraries", "dylint-libraries", library_names),
    ];
    if !invocation.no_run {
        compiler_execution_groups.push(group("dylint-ui-test-cook", "dylint-ui-tests", cook_names));
    }
    compiler_execution_groups.extend([group(
        "dylint-workspace",
        "dylint-analysis",
        vec!["dylint-workspace".into()],
    )]);
    if !invocation.no_run {
        compiler_execution_groups.push(group(
            "dylint-ui-tests",
            "dylint-ui-tests",
            dylint_test_names,
        ));
    }
    compiler_execution_groups.push(group(
        "nextest-compile",
        "stable",
        vec!["nextest-compile".into()],
    ));
    if invocation.no_run {
        compiler_execution_groups.push(group(
            "nextest-archive",
            "stable",
            vec!["nextest-archive".into()],
        ));
    } else {
        compiler_execution_groups.push(group("doctests", "rustdoc", vec!["doctests".into()]));
    }
    Ok(CiTestPlan {
        schema_version: PLAN_SCHEMA_VERSION,
        command: "ci-test",
        workspace_root: root.display().to_string(),
        workspace_metadata: WorkspaceMetadata {
            manifest_path: root.join("Cargo.toml").display().to_string(),
            lockfile_path: root.join("Cargo.lock").display().to_string(),
            cargo_config: config.clone(),
            fingerprint: workspace_metadata_fingerprint(&root, &config)?,
        },
        host_triple: host.clone(),
        target_triple: target.clone(),
        no_run: invocation.no_run,
        archive_file,
        scope: PlanScope {
            packages: invocation.scope.packages.clone(),
            features: invocation.scope.features.clone(),
            all_features: invocation.scope.all_features,
            no_default_features: invocation.scope.no_default_features,
        },
        domains,
        stages,
        subsumed_steps: vec![SubsumedStep {
            name: "cargo check",
            subsumed_by: "clippy",
            reason: "clippy covers the selected workspace/all-targets target scope",
        }],
        cook: CookDecision {
            action: "not-run",
            reason: "ci-test observes Cargo freshness and does not insert a dev-profile warm-up",
        },
        resource_limits: ResourceLimits {
            cargo_build_jobs,
            soldr_jobs,
            nextest_test_threads: Some(nextest_test_threads),
        },
        test_admission,
        test_target_count,
        test_target_warn_threshold,
        dylint_target_trees: DylintTargetTrees {
            libraries: dylint_libraries.display().to_string(),
            analysis: dylint_analysis.display().to_string(),
            tests: dylint_tests.display().to_string(),
        },
        compiler_execution_groups,
        observability: super::model::Observability {
            freshness_authority: "cargo",
            zccache_counters: "reported by the Soldr child build logs where observable",
            stage_wall_time: "reported after each executed stage",
            stage_bytes: "reported where the child exposes byte counters",
        },
    })
}

/// The post-validation tail has one join and four independent consumers.
/// Keeping this edge list shared with the executor prevents the frozen graph
/// and its optimized schedule from silently drifting apart.
pub(super) fn tail_join_dependencies(last_dylint_ui_test: &str) -> [&str; 2] {
    ["nextest", last_dylint_ui_test]
}

#[derive(Clone, Copy)]
struct StageExecution {
    kind: &'static str,
    concurrency_group: Option<&'static str>,
    executes_compiler: bool,
}

const PREFLIGHT: StageExecution = StageExecution {
    kind: "non-compiling",
    concurrency_group: Some("preflight"),
    executes_compiler: false,
};
const COMPILER: StageExecution = StageExecution {
    kind: "compiler",
    concurrency_group: None,
    executes_compiler: true,
};
const COMPILER_AND_TEST: StageExecution = StageExecution {
    kind: "compiler-and-test",
    concurrency_group: None,
    executes_compiler: true,
};
const TEST: StageExecution = StageExecution {
    kind: "test",
    concurrency_group: None,
    executes_compiler: false,
};
const POLICY: StageExecution = StageExecution {
    kind: "policy",
    concurrency_group: Some("policy"),
    executes_compiler: false,
};

fn stage(
    name: &str,
    domain: &'static str,
    execution: StageExecution,
    command: Vec<String>,
    depends_on: &[&str],
    root: &Path,
) -> Stage {
    Stage {
        name: name.into(),
        domain,
        kind: execution.kind,
        command,
        working_directory: root.display().to_string(),
        depends_on: depends_on.iter().map(|value| (*value).into()).collect(),
        concurrency_group: execution.concurrency_group,
        executes_compiler: execution.executes_compiler,
        metrics: super::model::StageMetrics {
            wall_time_ms: None,
            bytes: None,
            zccache_counters: None,
        },
    }
}

fn group(id: &'static str, domain: &'static str, stages: Vec<String>) -> CompilerExecutionGroup {
    CompilerExecutionGroup {
        id,
        domain,
        stages,
        fresh_dirty: "Cargo-reported at execution time",
    }
}

/// `soldr dylint cook --tree tests` for one lint crate, run from that crate's
/// directory. `--target-root` points at the repository's target root because
/// `dylints/<lint>/target` must stay empty
/// (`.github/scripts/verify_dylint_target_dirs.py`); the cook derives the same
/// nightly-keyed `dylint/tests/<key>` tree the UI-test stages build into.
fn dylint_tests_cook_command(target_root: &Path) -> Vec<String> {
    vec![
        "soldr".into(),
        "dylint".into(),
        "cook".into(),
        "--tree".into(),
        "tests".into(),
        "--tests".into(),
        "--target-root".into(),
        target_root.display().to_string(),
        "--json".into(),
    ]
}

fn cargo_command(fixed: &[impl AsRef<str>], scope: &[String]) -> Vec<String> {
    let mut command = vec!["soldr".into(), "cargo".into()];
    command.extend(fixed.iter().map(|value| value.as_ref().into()));
    command.extend(scope.iter().cloned());
    command
}

fn fmt_scope(scope: &super::model::Scope) -> Vec<String> {
    let mut args = Vec::new();
    for package in &scope.packages {
        args.extend(["--package".into(), package.clone()]);
    }
    args
}

fn workspace_selection(scope: &super::model::Scope) -> Vec<String> {
    if scope.packages.is_empty() {
        vec!["--workspace".into()]
    } else {
        Vec::new()
    }
}

/// soldr#3203: the shared resolver, run as Cargo would be from `root`.
fn cargo_target_root(root: &Path) -> Result<PathBuf, SoldrError> {
    let inputs = crate::core::cargo_target_dir::CargoTargetDirInputs::from_process(root, &[]);
    Ok(
        crate::core::cargo_target_dir::resolve_cargo_target_dir(&inputs)
            .unwrap_or_else(|| root.join("target")),
    )
}

fn cargo_config_paths(root: &Path) -> Vec<String> {
    [".cargo/config.toml", ".cargo/config"]
        .into_iter()
        .map(|name| root.join(name))
        .filter(|path| path.is_file())
        .map(|path| path.display().to_string())
        .collect()
}

fn effective_rustflags() -> Option<String> {
    ["CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

fn wrapper_identity(cache_enabled: bool) -> String {
    if !cache_enabled {
        return "disabled (--no-cache)".into();
    }
    std::env::var("SOLDR_RUSTC_WRAPPER")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "soldr-managed-zccache".into())
}

/// soldr#2945: the per-lint `rust-toolchain.toml` loop this used to run —
/// keyed on the hard-coded `DYLINTS` name list — is now the shared, glob-aware
/// reader in `crate::dylint_libraries`, which reads
/// `workspace.metadata.dylint.libraries`, distinguishes no libraries from
/// all-inherit, and reports disagreeing pins itself.
/// `DYLINTS` survives only because the stage graph above still names one build
/// and one UI-test stage per lint. Only the env-override refusal below is
/// ci-test-specific.
async fn resolve_dylint_plan(
    root: &Path,
    host: &str,
) -> Result<crate::dylint_toolchain::DylintToolchainPlan, SoldrError> {
    let (channel, libraries) = match crate::dylint_libraries::toolchain_state(root)? {
        crate::dylint_libraries::LibraryToolchainState::NoLibraries => {
            return Err(SoldrError::Other(format!(
                "soldr ci-test: {} declares no Dylint libraries under \
                 workspace.metadata.dylint.libraries",
                root.join("Cargo.toml").display()
            )));
        }
        crate::dylint_libraries::LibraryToolchainState::InheritRoot { libraries } => {
            let channel = crate::dylint_libraries::inherited_root_channel(root, &libraries)?;
            (channel, libraries)
        }
        crate::dylint_libraries::LibraryToolchainState::Pinned { channel, libraries } => {
            (channel, libraries)
        }
    };
    let library_count = libraries.len();
    // ci-test is deliberately stricter than the `soldr dylint` front door.
    // There, an explicit `+toolchain` or SOLDR_DYLINT_TOOLCHAIN is a developer
    // poking at one lint and is honoured. Here the plan is a frozen,
    // reproducible DAG whose three Dylint compile domains are keyed on the one
    // nightly the lint libraries themselves declare, so an override naming a
    // different nightly would silently key the target trees to a compiler the
    // libraries were not built for. That is a plan-integrity failure, not a
    // preference — so it is refused before the plan is frozen.
    for key in [
        crate::dylint_toolchain::TOOLCHAIN_ENV_VAR,
        crate::dylint_toolchain::CONFIGURED_TOOLCHAIN_ENV_VAR,
    ] {
        let Some(override_channel) = non_empty_env(key) else {
            continue;
        };
        if canonical_channel(&override_channel, host) != canonical_channel(&channel, host) {
            return Err(SoldrError::Other(format!(
                "soldr ci-test: configured Dylint toolchain {override_channel} ({key}) conflicts with the {library_count} lint libraries resolved to {channel}"
            )));
        }
    }
    let plan = crate::dylint_toolchain::resolve_plan(Some(&channel), root).await?;
    if canonical_channel(&plan.channel, host) != canonical_channel(&channel, host) {
        return Err(SoldrError::Other(format!(
            "soldr ci-test: configured Dylint toolchain {} conflicts with the {library_count} lint libraries resolved to {channel}",
            plan.channel
        )));
    }
    Ok(plan)
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn inherited_limit(name: &str) -> Option<String> {
    // Keep even an empty/invalid explicit value. Cargo/the daemon owns
    // validation; treating it as absent would silently change caller intent.
    std::env::var(name).ok()
}

fn workspace_metadata_fingerprint(root: &Path, config: &[String]) -> Result<String, SoldrError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"soldr-ci-test-workspace-metadata-v1\0");
    for path in std::iter::once(root.join("Cargo.toml"))
        .chain(std::iter::once(root.join("Cargo.lock")))
        .chain(config.iter().map(PathBuf::from))
    {
        hasher.update(path.display().to_string().as_bytes());
        hasher.update(&[0]);
        match std::fs::read(&path) {
            Ok(contents) => {
                hasher.update(&contents);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                hasher.update(b"<missing>");
            }
            Err(error) => {
                return Err(SoldrError::Other(format!(
                    "soldr ci-test: cannot read workspace metadata {}: {error}",
                    path.display()
                )));
            }
        }
        hasher.update(&[0]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// The key every Dylint target tree is named by: the host-qualified toolchain
/// cargo-dylint uses for `dylint/target/<toolchain>`. Shared with
/// `dylint_cook_tree::CookTree::channel_segment` so each cook lands in the
/// directory its stage reads (soldr#3042, soldr#3049).
pub(crate) fn canonical_channel(channel: &str, host: &str) -> String {
    if channel.ends_with(host) {
        channel.into()
    } else {
        format!("{channel}-{host}")
    }
}

fn reject_non_host_target(root: &Path) -> Result<(), SoldrError> {
    if let Some(value) = std::env::var_os("CARGO_BUILD_TARGET").filter(|value| !value.is_empty()) {
        return Err(SoldrError::Other(format!(
            "soldr ci-test: CARGO_BUILD_TARGET={} creates a non-host compile domain; unset it or use `soldr cargo ...` explicitly",
            PathBuf::from(value).display()
        )));
    }
    for name in [".cargo/config.toml", ".cargo/config"] {
        let path = root.join(name);
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed: toml::Value = toml::from_str(&contents).map_err(|error| {
            SoldrError::Other(format!(
                "soldr ci-test: failed to parse {}: {error}",
                path.display()
            ))
        })?;
        if let Some(target) = parsed
            .get("build")
            .and_then(|build| build.get("target"))
            .and_then(toml::Value::as_str)
        {
            return Err(SoldrError::Other(format!(
                "soldr ci-test: {} sets build.target={target:?}, which creates a non-host compile domain; use `soldr cargo ...` explicitly",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_channel_adds_the_host_once() {
        assert_eq!(
            canonical_channel("nightly-2026-05-28", "x86_64-unknown-linux-gnu"),
            "nightly-2026-05-28-x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            canonical_channel(
                "nightly-2026-05-28-x86_64-unknown-linux-gnu",
                "x86_64-unknown-linux-gnu"
            ),
            "nightly-2026-05-28-x86_64-unknown-linux-gnu"
        );
    }

    #[test]
    fn post_validation_tail_has_one_shared_join() {
        assert_eq!(
            tail_join_dependencies("dylint-test-final"),
            ["nextest", "dylint-test-final"]
        );
    }
}
