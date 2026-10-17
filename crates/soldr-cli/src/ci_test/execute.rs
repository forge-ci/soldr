use super::dylint_library_marker;
use super::execute_report::{
    summarize_compiler_report, write_compiler_run_report, CompilerEvent, CompilerIdentity,
};
use super::model::{CiTestPlan, Stage};
use super::nextest_resident_lease;
use crate::core::SoldrError;

#[path = "execute_branches.rs"]
mod branches;
use branches::*;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const CI_TEST_CARGO_RUNNER_ENV: &str = "SOLDR_CI_TEST_CARGO_RUNNER";

pub(super) use super::executor_contract::validate_executor_contract;
#[cfg(test)]
pub(super) use super::executor_contract::{require_dependencies, validate_tail_dependencies};

/// Execute the frozen plan in its dependency order. After Clippy, two chains
/// run side by side: stable Nextest compilation -> Fresh Nextest execution,
/// and the Dylint library -> workspace-analysis -> UI-test branch. Compiles on
/// both sides are compiler-bearing, so the daemon's canonical shared/exclusive
/// admission sees all of their rustc work and grants the measured
/// `soldr_daemon --test` and `soldr_cli --test` links exclusive access. Fresh
/// test processes are outside compiler admission, so Nextest execution holds
/// a resident-capacity lease and runs under the memory monitor for exactly its
/// own lifetime (soldr#2878, soldr#2885), and starts as soon as its compile
/// succeeds rather than after Dylint's analysis (soldr#3446). Individual tests
/// may launch nested compiler fixtures; those and Dylint still share the
/// daemon's canonical admission gate.
/// Dylint manifests remain sequential because all eight intentionally share one
/// target tree per domain. After both branches join, doctests and the three
/// non-compiling policy consumers run together from that completed join.
///
/// soldr#2349: the library half of the Dylint branch may be fast-forwarded
/// by an input-keyed marker skip straight to `dylint-workspace`. The skip is
/// OPT-IN (`SOLDR_DYLINT_LIBRARY_SKIP=on`); see `dylint_library_marker`.
pub(crate) async fn run(
    plan: &CiTestPlan,
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    validate_executor_contract(plan)?;
    let mut factory = StageCommandFactory::new(plan, cache_enabled, trust_inherited_soldr_env)?;
    // soldr#3143: the policy tail's tool downloads depend on nothing the DAG
    // produces. Start them now, off the critical path, so a GitHub Releases
    // failure surfaces at the start of the run instead of after the gate.
    let policy_prefetch = super::policy_prefetch::PolicyPrefetch::start(&plan.stages);
    macro_rules! stop_on_failure {
        ($result:expr) => {
            let code = $result?;
            if let Some(failure) = failure_code(code) {
                return Ok(failure);
            }
        };
    }

    stop_on_failure!(run_group(&factory, plan, &["rustfmt", "lint-ci"]));
    // soldr#3460: resolving Dylint -- cargo-dylint, the nightly's rustc-dev /
    // rust-src / llvm-tools components (~22 s of download on a fresh runner),
    // and the prebuilt driver -- touches no stable target tree, so it runs
    // beside Clippy instead of after it. Clippy's code is reported first; the
    // preparation is still awaited so it never outlives the run half-done.
    let preparing = tokio::spawn(prepare_dylint(factory.dylint.clone()));
    let clippy = run_group(&factory, plan, &["clippy"]);
    let prepared = preparing.await.map_err(|error| {
        SoldrError::Other(format!(
            "soldr ci-test: Dylint preparation task failed: {error}"
        ))
    })?;
    stop_on_failure!(clippy);
    factory.apply_prepared_dylint(prepared?);
    // soldr#2349: library-skip wiring lives in dylint_library_marker::decide/finish.
    let library_decision = dylint_library_marker::decide(plan, &factory.dylint)?;
    if plan.no_run {
        eprintln!(
            "soldr ci-test: overlapping Nextest compilation with Dylint library/workspace compilation under canonical compiler admission"
        );
        stop_on_failure!(run_parallel_nextest_compile_and_dylint_compile(
            &factory,
            plan,
            library_decision.skip
        ));
        dylint_library_marker::finish(&library_decision);
        let archive_file = plan.archive_file.as_ref().ok_or_else(|| {
            SoldrError::Other("soldr ci-test: compile-only plan has no archive path".into())
        })?;
        let parent = std::path::Path::new(archive_file).parent().ok_or_else(|| {
            SoldrError::Other("soldr ci-test: archive path has no parent directory".into())
        })?;
        std::fs::create_dir_all(parent)?;
        stop_on_failure!(run_group(&factory, plan, &["nextest-archive"]));
        if !std::fs::metadata(archive_file)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
        {
            return Err(SoldrError::Other(format!(
                "soldr ci-test: Nextest archive stage succeeded without a non-empty archive at {archive_file}"
            )));
        }
        println!("soldr ci-test: archive: {archive_file}");
        policy_prefetch.join().await;
        stop_on_failure!(run_group(
            &factory,
            plan,
            &["cargo-deny-bans", "cargo-audit", "cargo-machete"]
        ));
        return Ok(0);
    }
    eprintln!(
        "soldr ci-test: running Nextest compile -> execution beside the Dylint library -> workspace -> UI-test branch under canonical compiler admission"
    );
    let (code, libraries_built) =
        run_nextest_and_dylint_branches(&factory, plan, library_decision.skip)?;
    if libraries_built {
        dylint_library_marker::finish(&library_decision);
    }
    if let Some(failure) = failure_code(code) {
        return Ok(failure);
    }
    policy_prefetch.join().await;
    // All four tail stages consume the same completed Nextest + Dylint join.
    // The policy tools inspect manifests/advisories and do not compile; they
    // are independent of rustdoc's doctest compile-and-run domain.
    run_group(
        &factory,
        plan,
        &[
            "doctests",
            "cargo-deny-bans",
            "cargo-audit",
            "cargo-machete",
        ],
    )
}

trait StageSpawner {
    fn spawn_stage(&self, stage: &Stage) -> Result<Child, SoldrError>;
}

impl StageSpawner for StageCommandFactory {
    fn spawn_stage(&self, stage: &Stage) -> Result<Child, SoldrError> {
        self.spawn(stage)
    }
}

struct RunningStage<'a> {
    stage: &'a Stage,
    child: Child,
    started: Instant,
}

/// soldr#3446: Nextest execution starts the moment `nextest-compile`
/// succeeds, beside whatever phase the serial Dylint branch (libraries ->
/// workspace analysis -> UI tests) has reached. It used to wait for Dylint's
/// workspace analysis as well, leaving ~56 s of the Linux gate's critical
/// path with no tests running (run 36512583508: compile done at t+137 s,
/// analysis at t+197 s). That wait was soldr#3024's answer to test processes
/// overlapping the exclusive nightly analysis before test memory was
/// governed. Test memory is governed now: the lease below declares Nextest's
/// resident footprint to compiler admission, and soldr#2885's monitor pauses
/// new tests under memory pressure.
fn run_nextest_and_dylint_branches(
    factory: &StageCommandFactory,
    plan: &CiTestPlan,
    skip_libraries: bool,
) -> Result<(i32, bool), SoldrError> {
    let peer_chain = [
        stage_named(plan, "nextest-compile")?,
        stage_named(plan, "nextest")?,
    ];
    let verifier = PlanDylintVerifier(plan);
    let mut dylint = DylintBranch::full_from_plan(plan, skip_libraries, &verifier)?;
    let mut hooks = NextestExecutionHooks {
        admission: &factory.nextest_admission,
        lease_controller: nextest_resident_lease::DaemonResidentLeaseController {
            permits: nextest_resident_lease::NEXTEST_RESIDENT_LEASE_PERMITS,
        },
        lease: None,
        pressure: None,
    };
    let result =
        supervise_peer_chain_and_dylint(factory, &peer_chain, &mut hooks, &mut dylint, &verifier);
    Ok((result?, dylint.libraries_built))
}

fn run_parallel_nextest_compile_and_dylint_compile(
    factory: &StageCommandFactory,
    plan: &CiTestPlan,
    skip_libraries: bool,
) -> Result<i32, SoldrError> {
    let nextest_compile = stage_named(plan, "nextest-compile")?;
    let verifier = PlanDylintVerifier(plan);
    let dylint = DylintBranch::compilation_from_plan(plan, skip_libraries, &verifier)?;
    supervise_parallel_stage_and_dylint(factory, nextest_compile, dylint, &verifier)
}

fn supervise_parallel_stage_and_dylint<'a>(
    spawner: &impl StageSpawner,
    peer_stage: &'a Stage,
    dylint_branch: DylintBranch<'a>,
    verifier: &impl DylintBranchVerifier,
) -> Result<i32, SoldrError> {
    let mut dylint_branch = dylint_branch;
    supervise_peer_chain_and_dylint(
        spawner,
        &[peer_stage],
        &mut NoPeerHooks,
        &mut dylint_branch,
        verifier,
    )
}

/// Runs `peer_chain` in order beside the serial Dylint branch.
///
/// Each chain advances on its own: a peer stage starts as soon as the one
/// before it succeeds, whatever phase Dylint is in (soldr#3446). A failed
/// stage ends its own chain -- later stages depend on it -- but never the
/// other chain (soldr#3100), and the first failure is the returned code.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn supervise_peer_chain_and_dylint<'a>(
    spawner: &impl StageSpawner,
    peer_chain: &[&'a Stage],
    hooks: &mut impl PeerStageHooks,
    dylint_branch: &mut DylintBranch<'a>,
    verifier: &impl DylintBranchVerifier,
) -> Result<i32, SoldrError> {
    let fork_started = Instant::now();
    // soldr#3100: a failing branch no longer cancels its sibling. Both
    // branches run to completion so one red run reports every failure; the
    // first non-zero status is what the caller gets. Spawn errors still
    // cancel, since nothing meaningful can complete after them.
    let mut first_failure: Option<i32> = None;
    let mut remaining_peers = peer_chain.iter().copied();
    let first_peer = remaining_peers
        .next()
        .expect("a supervised peer chain has a first stage");
    hooks.before_spawn(first_peer)?;
    let mut peer = match spawn_running(spawner, first_peer) {
        Ok(running) => Some(running),
        Err(error) => {
            hooks.after_exit(first_peer);
            return Err(error);
        }
    };
    let first_dylint = dylint_branch
        .current()
        .expect("a validated Dylint branch has a first stage");
    let mut dylint = match spawn_running(spawner, first_dylint) {
        Ok(running) => Some(running),
        Err(error) => {
            cancel_peer(&mut peer, hooks);
            return Err(error);
        }
    };

    loop {
        let peer_status = match poll_running(&mut peer) {
            Ok(status) => status,
            Err(error) => {
                cancel_peer(&mut peer, hooks);
                cancel_running(&mut dylint);
                return Err(error);
            }
        };
        if let Some(status) = peer_status {
            let completed = peer.take().expect("polled peer child exists");
            report_completed(&completed, status);
            hooks.after_exit(completed.stage);
            if status.success() {
                if let Some(next) = remaining_peers.next() {
                    let spawned = hooks
                        .before_spawn(next)
                        .and_then(|()| spawn_running(spawner, next));
                    match spawned {
                        Ok(running) => peer = Some(running),
                        Err(error) => {
                            hooks.after_exit(next);
                            cancel_running(&mut dylint);
                            return Err(error);
                        }
                    }
                }
            } else {
                first_failure.get_or_insert(exit_code(status));
                eprintln!(
                    "soldr ci-test: `{}` failed; letting the Dylint branch finish so this run reports every failure",
                    completed.stage.name
                );
            }
        }

        let dylint_status = match poll_running(&mut dylint) {
            Ok(status) => status,
            Err(error) => {
                cancel_peer(&mut peer, hooks);
                cancel_running(&mut dylint);
                return Err(error);
            }
        };
        if let Some(status) = dylint_status {
            let completed = dylint.take().expect("polled Dylint child exists");
            report_completed(&completed, status);
            if !status.success() {
                // A failed lint stage stops its own branch (later stages
                // depend on it) but not the peer.
                first_failure.get_or_insert(exit_code(status));
                if let Some(running) = &peer {
                    eprintln!(
                        "soldr ci-test: Dylint branch failed; letting `{}` finish so this run reports every failure",
                        running.stage.name
                    );
                }
                continue;
            }
            let next_stage = match dylint_branch.advance(verifier) {
                Ok(stage) => stage,
                Err(error) => {
                    cancel_peer(&mut peer, hooks);
                    return Err(error);
                }
            };
            if let Some(stage) = next_stage {
                dylint = match spawn_running(spawner, stage) {
                    Ok(running) => Some(running),
                    Err(error) => {
                        cancel_peer(&mut peer, hooks);
                        return Err(error);
                    }
                };
            }
        }

        if peer.is_none() && dylint.is_none() {
            let last_peer = peer_chain.last().expect("non-empty peer chain");
            eprintln!(
                "soldr ci-test: `{}` + Dylint branches joined in {} ms",
                last_peer.name,
                fork_started.elapsed().as_millis()
            );
            return Ok(first_failure.unwrap_or(0));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn cancel_peer(peer: &mut Option<RunningStage<'_>>, hooks: &mut impl PeerStageHooks) {
    let stage = peer.as_ref().map(|running| running.stage);
    cancel_running(peer);
    if let Some(stage) = stage {
        hooks.after_exit(stage);
    }
}

pub(super) fn stage_named<'a>(plan: &'a CiTestPlan, name: &str) -> Result<&'a Stage, SoldrError> {
    plan.stages
        .iter()
        .find(|stage| stage.name == name)
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "soldr ci-test: frozen plan is missing stage {name:?}"
            ))
        })
}

fn spawn_running<'a>(
    spawner: &impl StageSpawner,
    stage: &'a Stage,
) -> Result<RunningStage<'a>, SoldrError> {
    Ok(RunningStage {
        stage,
        child: spawner.spawn_stage(stage)?,
        started: Instant::now(),
    })
}

fn poll_running(
    running: &mut Option<RunningStage<'_>>,
) -> Result<Option<std::process::ExitStatus>, SoldrError> {
    running.as_mut().map_or(Ok(None), |running| {
        running.child.try_wait().map_err(Into::into)
    })
}

fn report_completed(running: &RunningStage<'_>, status: std::process::ExitStatus) {
    report_status(running.stage, &running.child, status);
    eprintln!(
        "soldr ci-test: stage `{}` completed in {} ms",
        running.stage.name,
        running.started.elapsed().as_millis()
    );
}

fn cancel_running(running: &mut Option<RunningStage<'_>>) {
    if let Some(running) = running.as_mut() {
        cancel_child(running.stage, &mut running.child);
    }
    *running = None;
}

fn failure_code(code: i32) -> Option<i32> {
    (code != 0).then_some(code)
}

fn run_named_prefix(
    factory: &StageCommandFactory,
    plan: &CiTestPlan,
    prefix: &str,
) -> Result<i32, SoldrError> {
    for stage in plan
        .stages
        .iter()
        .filter(|stage| stage.name.starts_with(prefix))
    {
        let code = run_group(factory, plan, &[stage.name.as_str()])?;
        if code != 0 {
            return Ok(code);
        }
    }
    Ok(0)
}

fn run_group(
    factory: &StageCommandFactory,
    plan: &CiTestPlan,
    names: &[&str],
) -> Result<i32, SoldrError> {
    let started = std::time::Instant::now();
    let stages: Vec<&Stage> = names
        .iter()
        .map(|name| {
            plan.stages
                .iter()
                .find(|stage| stage.name == *name)
                .ok_or_else(|| {
                    SoldrError::Other(format!(
                        "soldr ci-test: frozen plan is missing stage {name:?}"
                    ))
                })
        })
        .collect::<Result<_, _>>()?;
    let code = run_stage_group(factory, &stages)?;
    if stages.len() == 1 {
        eprintln!(
            "soldr ci-test: stage `{}` completed in {} ms",
            stages[0].name,
            started.elapsed().as_millis()
        );
    } else {
        eprintln!(
            "soldr ci-test: stage group [{}] completed in {} ms",
            names.join(", "),
            started.elapsed().as_millis()
        );
    }
    Ok(code)
}

fn run_stage_group(spawner: &impl StageSpawner, stages: &[&Stage]) -> Result<i32, SoldrError> {
    if stages.len() == 1 {
        return wait_one(spawner.spawn_stage(stages[0])?, stages[0]);
    }
    let mut children = Vec::with_capacity(stages.len());
    for &stage in stages {
        match spawner.spawn_stage(stage) {
            Ok(child) => children.push((stage, child)),
            Err(error) => {
                cancel_remaining(&mut children, None);
                return Err(error);
            }
        }
    }
    wait_parallel(&mut children)
}

fn wait_one(mut child: Child, stage: &Stage) -> Result<i32, SoldrError> {
    let status = child.wait()?;
    report_status(stage, &child, status);
    Ok(exit_code(status))
}

fn wait_parallel(children: &mut [(&Stage, Child)]) -> Result<i32, SoldrError> {
    let mut reported = vec![false; children.len()];
    loop {
        let mut complete = 0;
        for index in 0..children.len() {
            let failure = {
                let polled = children[index].1.try_wait();
                let status = match polled {
                    Ok(status) => status,
                    Err(error) => {
                        cancel_remaining(children, None);
                        return Err(error.into());
                    }
                };
                let (stage, child) = &mut children[index];
                if let Some(status) = status {
                    complete += 1;
                    if !reported[index] {
                        reported[index] = true;
                        report_status(stage, child, status);
                    }
                    (!status.success()).then_some((child.id(), exit_code(status)))
                } else {
                    None
                }
            };
            if let Some((failed_pid, code)) = failure {
                cancel_remaining(children, Some(failed_pid));
                return Ok(code);
            }
        }
        if complete == children.len() {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn cancel_remaining(children: &mut [(&Stage, Child)], failed_pid: Option<u32>) {
    for (stage, child) in children {
        if Some(child.id()) == failed_pid {
            continue;
        }
        if child.try_wait().ok().flatten().is_some() {
            continue;
        }
        cancel_child(stage, child);
    }
}

fn cancel_child(stage: &Stage, child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    match crate::cargo_front_door::kill_cargo_process_tree(child) {
        Ok(kind) => eprintln!(
            "soldr ci-test: stage `{}` (pid {}) {kind}",
            stage.name,
            child.id()
        ),
        Err(error) => eprintln!(
            "soldr ci-test: stage `{}` (pid {}) could not be terminated: {error}",
            stage.name,
            child.id()
        ),
    }
    if let Ok(status) = child.wait() {
        eprintln!(
            "soldr ci-test: stage `{}` (pid {}) canceled with {status}",
            stage.name,
            child.id()
        );
    }
}

fn report_status(stage: &Stage, child: &Child, status: std::process::ExitStatus) {
    eprintln!(
        "soldr ci-test: stage `{}` (pid {}) exited with {status}",
        stage.name,
        child.id()
    );
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

fn verify_target_tree(label: &str, directory: &str) -> Result<(), SoldrError> {
    let path = std::path::Path::new(directory);
    let populated = std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    if populated {
        return Ok(());
    }
    Err(SoldrError::Other(format!(
        "soldr ci-test: {label} target tree was not populated at {}; refusing to continue with a different target domain",
        path.display()
    )))
}

fn verify_dylint_test_targets(plan: &CiTestPlan) -> Result<(), SoldrError> {
    let shared = std::path::Path::new(&plan.dylint_target_trees.tests);
    let deps = shared.join("debug").join("deps");
    verify_target_tree("Dylint UI-test dependencies", &deps.display().to_string())?;

    let dylints = std::path::Path::new(&plan.workspace_root).join("dylints");
    let entries = std::fs::read_dir(&dylints).map_err(|error| {
        SoldrError::Other(format!(
            "soldr ci-test: cannot inspect {}: {error}",
            dylints.display()
        ))
    })?;
    for entry in entries {
        let local_target = entry?.path().join("target");
        let offenders = super::dylint_target_guard::material_artifacts(
            &local_target,
            super::dylint_target_guard::MATERIAL_ARTIFACT_LIST_LIMIT,
        )?;
        if !offenders.is_empty() {
            // Name what was found, not just where: the writer is whichever
            // nested tool produced these paths, and that is the only lead a
            // reader of a CI log has (the tree is gone with the runner).
            let listing = offenders
                .iter()
                .map(|(path, bytes)| {
                    format!(
                        "\n    {} ({bytes} bytes)",
                        path.strip_prefix(&local_target).unwrap_or(path).display()
                    )
                })
                .collect::<String>();
            let limit = super::dylint_target_guard::MATERIAL_ARTIFACT_LIST_LIMIT;
            let more = if offenders.len() >= limit {
                format!("\n    ... (first {limit} shown)")
            } else {
                String::new()
            };
            return Err(SoldrError::Other(format!(
                "soldr ci-test: Dylint UI tests created compiler artifacts in {}; all eight tests must share {}. Found:{listing}{more}",
                local_target.display(),
                shared.display()
            )));
        }
    }
    Ok(())
}

struct StageCommandFactory {
    soldr: PathBuf,
    trust_inherited_soldr_env: bool,
    cache_enabled: bool,
    cargo_build_jobs: Option<String>,
    soldr_jobs: Option<String>,
    host_triple: String,
    nextest_test_cargo_runner: Option<PathBuf>,
    dylint: crate::dylint_toolchain::DylintToolchainPlan,
    dylint_bin_dirs: Vec<PathBuf>,
    dylint_env: Vec<(String, String)>,
    ci_test_report_path: PathBuf,
    nextest_admission: super::test_pressure::NextestAdmission,
}

/// What `prepare_dylint` resolves for every Dylint stage.
struct PreparedDylint {
    dylint: crate::dylint_toolchain::DylintToolchainPlan,
    bin_dirs: Vec<PathBuf>,
    env: Vec<(String, String)>,
}

/// Owns its inputs so it can run as its own task beside Clippy (soldr#3460).
async fn prepare_dylint(
    dylint: crate::dylint_toolchain::DylintToolchainPlan,
) -> Result<PreparedDylint, SoldrError> {
    let paths = crate::core::SoldrPaths::new()?;
    paths.ensure_dirs()?;
    let bootstrap =
        crate::cargo_front_door::ensure_known_subcommand_tool(&["dylint".to_string()], &paths)
            .await?;
    // The prebuilt driver probe needs the selected rustc's runtime
    // libraries. Install and verify the exact nightly first so a clean
    // managed rustup home cannot fail while probing an otherwise valid
    // catalogued driver.
    let dylint = crate::dylint_toolchain::prepare_resolved(dylint)?;
    crate::dylint_driver::ensure_prebuilt_driver(&dylint, &paths).await?;
    Ok(PreparedDylint {
        dylint,
        bin_dirs: bootstrap.bin_dirs,
        env: bootstrap.env,
    })
}

impl StageCommandFactory {
    fn new(
        plan: &CiTestPlan,
        cache_enabled: bool,
        trust_inherited_soldr_env: bool,
    ) -> Result<Self, SoldrError> {
        let dylint_domain = plan
            .domains
            .iter()
            .find(|domain| domain.id == "dylint-libraries")
            .ok_or_else(|| SoldrError::Other("soldr ci-test: missing Dylint domain".into()))?;
        let compiler_release = dylint_domain.compiler_release.clone().ok_or_else(|| {
            SoldrError::Other("soldr ci-test: Dylint domain has no compiler release".into())
        })?;
        let compiler_commit = dylint_domain.compiler_commit.clone().ok_or_else(|| {
            SoldrError::Other("soldr ci-test: Dylint domain has no compiler commit".into())
        })?;
        let cargo_build_jobs = plan.resource_limits.cargo_build_jobs.clone();
        let soldr_jobs = plan.resource_limits.soldr_jobs.clone();
        let paths = crate::core::SoldrPaths::new()?;
        let report_dir = paths.cache.join("logs").join("ci-test");
        std::fs::create_dir_all(&report_dir)?;
        let ci_test_report_path = report_dir.join(format!(
            "compiler-events-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ));
        eprintln!(
            "soldr ci-test: compiler-unit report will be written to {}",
            ci_test_report_path.display()
        );
        Ok(Self {
            soldr: crate::current_soldr_binary()?,
            trust_inherited_soldr_env,
            // The parent `--no-cache` choice is process-local. It needs to be
            // repeated for child Soldr commands, otherwise the plan would
            // silently switch wrapper identity at its first stage.
            cache_enabled,
            cargo_build_jobs,
            soldr_jobs,
            host_triple: plan.host_triple.clone(),
            nextest_test_cargo_runner: std::env::var_os(CI_TEST_CARGO_RUNNER_ENV)
                .map(PathBuf::from),
            // Reconstructed from the frozen plan document rather than
            // re-derived, so it carries no precedence tier (soldr#2945).
            dylint: crate::dylint_toolchain::DylintToolchainPlan::identity(
                dylint_domain.toolchain.clone(),
                compiler_release,
                compiler_commit,
            ),
            dylint_bin_dirs: Vec::new(),
            dylint_env: Vec::new(),
            ci_test_report_path,
            nextest_admission: super::test_pressure::NextestAdmission::new(
                &report_dir,
                plan.test_admission.clone(),
            ),
        })
    }

    fn apply_prepared_dylint(&mut self, prepared: PreparedDylint) {
        self.dylint = prepared.dylint;
        self.dylint_bin_dirs = prepared.bin_dirs;
        self.dylint_env = prepared.env;
    }

    fn spawn(&self, stage: &Stage) -> Result<Child, SoldrError> {
        let mut command = Command::new(&self.soldr);
        if !self.cache_enabled {
            command.arg("--no-cache");
        }
        if self.trust_inherited_soldr_env {
            command.arg("--trust-inherited-soldr-env");
        }
        command.args(stage.command.iter().skip(1));
        command.current_dir(&stage.working_directory);
        apply_stage_resource_limits(
            &mut command,
            self.cargo_build_jobs.as_deref(),
            self.soldr_jobs.as_deref(),
        );
        command.env(
            crate::core::CI_TEST_REPORT_PATH_ENV_VAR,
            &self.ci_test_report_path,
        );
        command.env(crate::core::CI_TEST_STAGE_ENV_VAR, &stage.name);
        configure_stage_cache_lifecycle(&mut command);
        configure_nextest_test_cargo_runner(
            &mut command,
            stage,
            &self.host_triple,
            self.nextest_test_cargo_runner.as_deref(),
        )?;
        configure_nextest_admission(&mut command, stage, &self.nextest_admission);
        if stage.domain.starts_with("dylint-") {
            for (key, value) in &self.dylint_env {
                command.env(key, value);
            }
            self.dylint.apply_to_command(&mut command);
            prepend_command_path(&mut command, &self.dylint_bin_dirs)?;
            command.env("SOLDR_NO_GC_TARGET", "1");
            command.env("SOLDR_LINKER", "default");
            // UI tests spawn nested Cargo work (trybuild/UI harnesses). Give
            // that child the same nightly-keyed target root, not Cargo's
            // ambient workspace target directory.
            if stage.name.starts_with("dylint-test-") {
                command.env("CARGO_TARGET_DIR", &self.dylint_test_target_dir(stage)?);
            }
            // soldr#3460: the tests-tree cook names its tree with
            // `--target-root`; an inherited CARGO_TARGET_DIR would fight it.
            if stage.name.starts_with("dylint-cook-") {
                command.env_remove("CARGO_TARGET_DIR");
            }
        }
        crate::cargo_front_door::configure_cargo_child_for_timeout(&mut command);
        command.env(
            crate::cargo_front_door::INHERIT_PARENT_PROCESS_GROUP_ENV,
            "1",
        );
        crate::exit_guard::mark_spoke();
        let _spawn = crate::core::spawn_exclusion::spawn_shared(); // soldr#3098
        command.spawn().map_err(|error| {
            SoldrError::Other(format!(
                "soldr ci-test: failed to start stage `{}`: {error}",
                stage.name
            ))
        })
    }

    fn dylint_test_target_dir(&self, stage: &Stage) -> Result<String, SoldrError> {
        stage
            .command
            .windows(2)
            .find(|pair| pair[0] == "--target-dir")
            .map(|pair| pair[1].clone())
            .ok_or_else(|| {
                SoldrError::Other(format!(
                    "soldr ci-test: frozen Dylint test stage `{}` has no --target-dir",
                    stage.name
                ))
            })
    }
}

fn configure_nextest_test_cargo_runner(
    command: &mut Command,
    stage: &Stage,
    host_triple: &str,
    runner: Option<&std::path::Path>,
) -> Result<(), SoldrError> {
    // Cargo deliberately replaces an inherited CARGO with the real binary
    // performing the build. Nextest carries that value into tests. A target
    // runner is the last process boundary before the test executable, so the
    // generated runner can restore the allowed Soldr shim after Cargo's write.
    if stage.name != "nextest" {
        return Ok(());
    }
    let Some(runner) = runner else {
        return Ok(());
    };
    if !runner.is_absolute() || !runner.is_file() {
        return Err(SoldrError::Other(format!(
            "soldr ci-test: {CI_TEST_CARGO_RUNNER_ENV} must name an absolute runner file: {}",
            runner.display()
        )));
    }
    let target = host_triple.to_ascii_uppercase().replace('-', "_");
    command.env(format!("CARGO_TARGET_{target}_RUNNER"), runner);
    Ok(())
}

/// soldr#2885: only Nextest *execution* runs tests, so only it receives the
/// admission directory, the per-test ceiling, and the admission summary.
fn configure_nextest_admission(
    command: &mut Command,
    stage: &Stage,
    admission: &super::test_pressure::NextestAdmission,
) {
    if stage.name != "nextest" {
        return;
    }
    for (key, value) in admission.stage_env() {
        command.env(key, value);
    }
}

fn apply_stage_resource_limits(
    command: &mut Command,
    cargo_build_jobs: Option<&str>,
    soldr_jobs: Option<&str>,
) {
    if let Some(value) = cargo_build_jobs {
        command.env("CARGO_BUILD_JOBS", value);
    }
    if let Some(value) = soldr_jobs {
        command.env("SOLDR_JOBS", value);
    }
}

impl Drop for StageCommandFactory {
    fn drop(&mut self) {
        let Ok(report) = summarize_compiler_report(&self.ci_test_report_path) else {
            eprintln!(
                "soldr ci-test: compiler-unit report has no compiler events (all units may be Fresh): {}",
                self.ci_test_report_path.display()
            );
            return;
        };
        let summary_path = self.ci_test_report_path.with_extension("summary.json");
        if let Err(error) = write_compiler_run_report(&summary_path, &report) {
            eprintln!(
                "warning: soldr ci-test: could not write compiler-unit summary {}: {error}",
                summary_path.display()
            );
        }
        eprintln!(
            "soldr ci-test: compiler-unit report: {} executions, {} identities, {} duplicate executions; {}",
            report.compiler_executions,
            report.unique_identities,
            report.duplicate_executions,
            summary_path.display()
        );
        if report.duplicate_executions != 0 {
            for duplicate in report.duplicates {
                eprintln!(
                    "warning: soldr ci-test: duplicate compiler identity {} executed {} times: {}",
                    duplicate.identity.digest,
                    duplicate.executions,
                    serde_json::to_string(&duplicate.identity).unwrap_or_default()
                );
            }
        }
    }
}

/// `ci-test` owns one shared daemon across its overlapping Cargo branches.
/// setup-soldr deliberately exports command-lifetime flushing for ordinary
/// one-command jobs, but inheriting that value here lets the first completed
/// stage checkpoint the cache while sibling stages still have writes in
/// flight. Keep stage children in job-lifetime mode; the workflow-level
/// shutdown remains the single durability boundary after the DAG joins.
fn configure_stage_cache_lifecycle(command: &mut Command) {
    command.env(crate::zccache::SOLDR_CACHE_LIFECYCLE_ENV_VAR, "job");
    command.env_remove(crate::zccache::SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR);
}

fn prepend_command_path(command: &mut Command, prefixes: &[PathBuf]) -> Result<(), SoldrError> {
    if prefixes.is_empty() {
        return Ok(());
    }
    let mut entries = prefixes.to_vec();
    let configured = command
        .get_envs()
        .find(|(key, _)| *key == std::ffi::OsStr::new("PATH"))
        .and_then(|(_, value)| value.map(std::ffi::OsString::from));
    if let Some(path) = configured.or_else(|| std::env::var_os("PATH")) {
        entries.extend(std::env::split_paths(&path));
    }
    let joined = std::env::join_paths(entries).map_err(|error| {
        SoldrError::Other(format!("soldr ci-test: invalid Dylint PATH: {error}"))
    })?;
    command.env("PATH", joined);
    Ok(())
}

#[cfg(test)]
#[path = "execute_tests.rs"]
mod tests;
