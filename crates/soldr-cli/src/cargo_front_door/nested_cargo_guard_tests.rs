use super::*;

const ROOT: u32 = 100;
const TARGET: &str = "/nested-guard-fixture/ws/target";

fn argv(parts: &[&str]) -> Option<Vec<String>> {
    Some(parts.iter().map(|part| part.to_string()).collect())
}

fn build_script() -> String {
    format!("{TARGET}/debug/build/outer-0123456789abcdef/build-script-build")
}

fn no_cwd(_: u32) -> Option<PathBuf> {
    None
}

/// root cargo (100) -> build script (200) -> `cargo <nested>` (300).
fn build_script_tree(nested: &[&str]) -> ProcessTree {
    let mut tree = ProcessTree::new(ROOT);
    tree.started(200, Some(ROOT), argv(&[&build_script()]));
    let mut nested_argv = vec!["/toolchain/bin/cargo"];
    nested_argv.extend_from_slice(nested);
    tree.started(300, Some(200), argv(&nested_argv));
    tree
}

fn hazard(assessment: Assessment) -> Hazard {
    match assessment {
        Assessment::Hazard(hazard) => hazard,
        other => panic!("expected a hazard, got {other:?}"),
    }
}

#[test]
fn a_build_script_nested_build_on_the_outer_target_is_a_hazard() {
    let tree = build_script_tree(&["build", "-p", "helper"]);
    let hazard = hazard(tree.assess(300, &no_cwd));
    assert_eq!(hazard.nested_pid, 300);
    assert_eq!(hazard.lock_holder_pid, ROOT);
    assert_eq!(hazard.phase_pid, 200);
    assert_eq!(hazard.phase_kind, "build script");
    assert_eq!(hazard.exe_name, "cargo");
    assert_eq!(hazard.verb, "build");
    assert_eq!(hazard.head, "build -p helper");
    assert_eq!(hazard.reason, HazardReason::NoTargetDir);
}

#[test]
fn an_intermediate_shell_does_not_hide_the_hazard() {
    let mut tree = ProcessTree::new(ROOT);
    tree.started(200, Some(ROOT), argv(&[&build_script()]));
    tree.started(250, Some(200), argv(&["/bin/sh", "-c", "cargo check"]));
    tree.started(300, Some(250), argv(&["cargo", "check"]));
    assert_eq!(hazard(tree.assess(300, &no_cwd)).phase_pid, 200);
}

#[test]
fn a_distinct_absolute_target_dir_is_allowed() {
    let tree = build_script_tree(&["build", "--target-dir", "/elsewhere/nested-target"]);
    assert_eq!(tree.assess(300, &no_cwd), Assessment::DistinctTargetDir);
    // A target below the build script's own OUT_DIR is distinct too.
    let out_dir = format!("{TARGET}/debug/build/outer-0123456789abcdef/out/nested");
    let tree = build_script_tree(&["build", &format!("--target-dir={out_dir}")]);
    assert_eq!(tree.assess(300, &no_cwd), Assessment::DistinctTargetDir);
}

#[test]
fn a_target_dir_naming_the_outer_target_is_a_hazard() {
    for target in [TARGET.to_string(), format!("{TARGET}/../target")] {
        let tree = build_script_tree(&["build", "--target-dir", &target]);
        assert_eq!(
            hazard(tree.assess(300, &no_cwd)).reason,
            HazardReason::SameTarget,
            "{target}"
        );
    }
}

#[test]
fn a_symlinked_alias_of_the_outer_target_is_a_hazard() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("target");
    let script_dir = target.join("debug/build/outer-0123456789abcdef");
    std::fs::create_dir_all(&script_dir).expect("script dir");
    let script = script_dir.join("build-script-build");
    std::fs::write(&script, b"").expect("script");
    let alias = dir.path().join("alias");
    if crate::platform::fs::links::create(&target.to_string_lossy(), &alias, true).is_err() {
        return;
    }
    let mut tree = ProcessTree::new(ROOT);
    tree.started(200, Some(ROOT), argv(&[&script.display().to_string()]));
    tree.started(
        300,
        Some(200),
        argv(&[
            "cargo",
            "build",
            "--target-dir",
            &alias.display().to_string(),
        ]),
    );
    assert_eq!(
        hazard(tree.assess(300, &no_cwd)).reason,
        HazardReason::SameTarget
    );
}

#[test]
fn a_relative_target_dir_needs_the_working_directory() {
    let tree = build_script_tree(&["build", "--target-dir", "nested-target"]);
    assert_eq!(
        hazard(tree.assess(300, &no_cwd)).reason,
        HazardReason::UnresolvableTargetDir
    );
    // Resolved against the process's cwd, a relative target outside the
    // outer target is distinct; `-C` is applied first.
    let cwd = |_| Some(PathBuf::from("/elsewhere"));
    assert_eq!(tree.assess(300, &cwd), Assessment::DistinctTargetDir);
    let into_outer = |_| Some(PathBuf::from("/nested-guard-fixture/ws"));
    let tree = build_script_tree(&["-C", ".", "build", "--target-dir", "target"]);
    assert_eq!(
        hazard(tree.assess(300, &into_outer)).reason,
        HazardReason::SameTarget
    );
}

#[test]
fn env_and_config_target_overrides_are_not_proof() {
    // An env-only `CARGO_TARGET_DIR` leaves no trace on the command line.
    let tree = build_script_tree(&["build"]);
    assert_eq!(
        hazard(tree.assess(300, &no_cwd)).reason,
        HazardReason::NoTargetDir
    );
    let tree = build_script_tree(&[
        "build",
        "--config",
        "build.target-dir=\"/elsewhere\"",
        "--target-dir",
        "/elsewhere",
    ]);
    assert_eq!(
        hazard(tree.assess(300, &no_cwd)).reason,
        HazardReason::ConfigTargetDir
    );
}

#[test]
fn non_locking_nested_cargo_is_not_a_candidate() {
    for nested in [
        &["--version"][..],
        &["metadata", "--no-deps", "--format-version", "1"][..],
        &["locate-project"][..],
    ] {
        let tree = build_script_tree(nested);
        assert_eq!(
            tree.assess(300, &no_cwd),
            Assessment::NotCandidate,
            "{nested:?}"
        );
    }
}

#[test]
fn clippy_driving_check_through_cargo_is_not_a_hazard() {
    // soldr cargo clippy: cargo clippy (root) -> cargo-clippy -> cargo check.
    // The nested `cargo check` is the lock holder, not a waiter.
    let mut tree = ProcessTree::new(ROOT);
    tree.started(
        200,
        Some(ROOT),
        argv(&["/toolchain/bin/cargo-clippy", "clippy"]),
    );
    tree.started(300, Some(200), argv(&["/toolchain/bin/cargo", "check"]));
    assert_eq!(tree.assess(300, &no_cwd), Assessment::NoLockHeld);
    // ...but a build script under that `cargo check` running `cargo build`
    // does wait on it.
    tree.started(
        400,
        Some(300),
        argv(&["/ws/target/debug/build/dep-0011/build-script-build"]),
    );
    tree.started(500, Some(400), argv(&["cargo", "build"]));
    let hazard = hazard(tree.assess(500, &no_cwd));
    assert_eq!(hazard.lock_holder_pid, 300);
}

#[test]
fn a_test_body_building_through_cargo_is_not_a_lock_held_phase() {
    // cargo test (root) -> test binary -> cargo build: Cargo releases the
    // build lock before running test binaries.
    let mut tree = ProcessTree::new(ROOT);
    tree.started(
        200,
        Some(ROOT),
        argv(&["/ws/target/debug/deps/it-0011223344556677", "--nocapture"]),
    );
    tree.started(
        300,
        Some(200),
        argv(&["cargo", "build", "-p", "mock-agent"]),
    );
    assert_eq!(tree.assess(300, &no_cwd), Assessment::NoLockHeld);
    // Doctests (`rustdoc --test`) run after the compile phase as well.
    let mut tree = ProcessTree::new(ROOT);
    tree.started(
        200,
        Some(ROOT),
        argv(&["rustdoc", "--test", "src/lib.rs", "--crate-name", "ws"]),
    );
    tree.started(300, Some(200), argv(&["cargo", "build"]));
    assert_eq!(tree.assess(300, &no_cwd), Assessment::NoLockHeld);
}

#[test]
fn a_nested_soldr_owns_its_subtree() {
    let mut tree = ProcessTree::new(ROOT);
    tree.started(200, Some(ROOT), argv(&[&build_script()]));
    tree.started(250, Some(200), argv(&["/ci/bin/soldr", "cargo", "build"]));
    tree.started(300, Some(250), argv(&["cargo", "build"]));
    assert_eq!(tree.assess(300, &no_cwd), Assessment::SoldrBoundary);
}

#[test]
fn a_compiler_is_a_lock_held_phase_with_its_out_dir_as_evidence() {
    let mut tree = ProcessTree::new(ROOT);
    tree.started(
        200,
        Some(ROOT),
        argv(&[
            "rustc",
            "--crate-name",
            "outer",
            "--out-dir",
            &format!("{TARGET}/debug/deps"),
        ]),
    );
    tree.started(
        300,
        Some(200),
        argv(&["cargo", "build", "--target-dir", TARGET]),
    );
    let hazard = hazard(tree.assess(300, &no_cwd));
    assert_eq!(hazard.phase_kind, "compiler");
    assert_eq!(hazard.reason, HazardReason::SameTarget);
    tree.exited(300);
    tree.started(
        301,
        Some(200),
        argv(&["cargo", "build", "--target-dir", "/elsewhere"]),
    );
    assert_eq!(tree.assess(301, &no_cwd), Assessment::DistinctTargetDir);
}

#[test]
fn every_enclosing_lock_holder_must_be_distinct() {
    // An isolated nested build (300) runs its own build script (400), whose
    // nested build (500) targets the *outer* target: it waits on the root.
    let mut tree = build_script_tree(&["build", "--target-dir", "/elsewhere/t"]);
    tree.started(
        400,
        Some(300),
        argv(&["/elsewhere/t/debug/build/helper-99/build-script-build"]),
    );
    tree.started(
        500,
        Some(400),
        argv(&["cargo", "build", "--target-dir", TARGET]),
    );
    let hazard = hazard(tree.assess(500, &no_cwd));
    assert_eq!(
        hazard.lock_holder_pid, ROOT,
        "the root holds the contended lock"
    );
    assert_eq!(hazard.phase_pid, 200);
    assert_eq!(hazard.reason, HazardReason::SameTarget);
}

#[test]
fn an_unexeced_fork_is_not_a_candidate_and_incomplete_ancestry_waits() {
    // Cargo's fork of a child shows Cargo's argv until it execs.
    let mut tree = ProcessTree::new(ROOT);
    tree.started(200, Some(ROOT), argv(&["cargo", "build"]));
    assert_eq!(tree.assess(200, &no_cwd), Assessment::NotCandidate);
    // A nested Cargo whose parent has not been observed yet is re-assessed.
    let mut tree = ProcessTree::new(ROOT);
    tree.started(300, Some(250), argv(&["cargo", "build"]));
    assert_eq!(tree.assess(300, &no_cwd), Assessment::Pending);
    tree.started(250, Some(ROOT), None);
    assert_eq!(tree.assess(300, &no_cwd), Assessment::Pending);
}

#[test]
fn a_fork_that_execs_into_cargo_is_reclassified_on_refresh() {
    let mut tree = ProcessTree::new(ROOT);
    tree.started(200, Some(ROOT), argv(&[&build_script()]));
    // Seen between fork and exec: still the build script's argv.
    tree.started(300, Some(200), argv(&[&build_script()]));
    assert_eq!(tree.assess(300, &no_cwd), Assessment::NotCandidate);
    let exec = |pid: u32| (pid == 300).then(|| vec!["cargo".to_string(), "build".to_string()]);
    tree.refresh(Instant::now(), &exec);
    assert_eq!(
        hazard(tree.assess(300, &no_cwd)).reason,
        HazardReason::NoTargetDir
    );
}

#[test]
fn the_refresh_skips_settled_processes_outside_lock_held_phases() {
    let mut tree = ProcessTree::new(ROOT);
    let long_ago = Instant::now() - Duration::from_secs(10);
    tree.started_at(
        200,
        Some(ROOT),
        argv(&["/ws/target/debug/deps/it-00"]),
        long_ago,
    );
    tree.started_at(210, Some(ROOT), argv(&[&build_script()]), long_ago);
    tree.started_at(220, Some(210), argv(&["/bin/sh"]), long_ago);
    let read = std::cell::RefCell::new(Vec::new());
    let record = |pid: u32| {
        read.borrow_mut().push(pid);
        None
    };
    tree.refresh(Instant::now(), &record);
    // Only the process below the build script is re-read.
    assert_eq!(read.into_inner(), vec![220]);
}

#[test]
fn the_mode_variable_fails_closed() {
    assert_eq!(GuardMode::parse(None), (GuardMode::Enforce, None));
    assert_eq!(GuardMode::parse(Some("")).0, GuardMode::Enforce);
    assert_eq!(GuardMode::parse(Some("enforce")).0, GuardMode::Enforce);
    assert_eq!(GuardMode::parse(Some(" Report ")).0, GuardMode::Report);
    assert_eq!(GuardMode::parse(Some("allow")).0, GuardMode::Allow);
    for disable_attempt in ["0", "off", "false", "disable"] {
        let (mode, warning) = GuardMode::parse(Some(disable_attempt));
        assert_eq!(mode, GuardMode::Enforce, "{disable_attempt}");
        assert!(warning.is_some_and(|w| w.contains(NESTED_CARGO_ENV_VAR)));
    }
}

#[test]
fn canonicalish_resolves_the_existing_prefix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = std::fs::canonicalize(dir.path()).expect("canonical tempdir");
    assert_eq!(
        canonicalish(&dir.path().join("missing/child")),
        real.join("missing").join("child")
    );
    assert_eq!(
        lexical_normalize(Path::new("/a/b/../c/./d")),
        PathBuf::from("/a/c/d")
    );
}

#[test]
fn an_enforced_hazard_trips_the_guard_and_writes_one_audit_record() {
    let audit = tempfile::tempdir().expect("audit dir");
    let guard = NestedCargoGuard::new(GuardMode::Enforce, false, Some(audit.path().to_path_buf()));
    guard.bind_root(ROOT);
    {
        let mut state = guard.state();
        let tree = state.tree.as_mut().expect("bound");
        tree.started(200, Some(ROOT), argv(&[&build_script()]));
        tree.started(300, Some(200), argv(&["cargo", "build", "-p", "helper"]));
        // Keep the synthetic pids out of the live refresh.
        state.last_refresh = Some(Instant::now());
    }
    assert!(guard.violation().is_none());
    guard.tick();
    let violation = guard.violation().expect("tripped");
    assert_eq!(violation.hazard.nested_pid, 300);
    assert!(violation.message.contains("nested Cargo pid 300"));
    assert!(violation.message.contains("`cargo build -p helper`"));
    assert!(violation.message.contains(NESTED_CARGO_ENV_VAR));
    let records: Vec<_> = std::fs::read_dir(audit.path())
        .expect("audit dir")
        .flatten()
        .collect();
    assert_eq!(records.len(), 1);
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(records[0].path()).expect("read"))
            .expect("json");
    assert_eq!(record["action"], "terminated");
    assert_eq!(record["permit"], serde_json::Value::Null);
    assert_eq!(record["outer_cargo_pid"], ROOT);
    assert_eq!(record["nested_pid"], 300);
    assert_eq!(record["reason"], "no_target_dir");
    // A second tick neither re-records nor changes the verdict.
    guard.tick();
    assert_eq!(std::fs::read_dir(audit.path()).expect("dir").count(), 1);
}

#[test]
fn the_permit_records_the_hazard_without_tripping() {
    let audit = tempfile::tempdir().expect("audit dir");
    let guard = NestedCargoGuard::new(GuardMode::Allow, false, Some(audit.path().to_path_buf()));
    guard.bind_root(ROOT);
    {
        let mut state = guard.state();
        let tree = state.tree.as_mut().expect("bound");
        tree.started(200, Some(ROOT), argv(&[&build_script()]));
        tree.started(300, Some(200), argv(&["cargo", "build"]));
        state.last_refresh = Some(Instant::now());
    }
    guard.tick();
    guard.tick();
    assert!(guard.violation().is_none());
    let records: Vec<_> = std::fs::read_dir(audit.path())
        .expect("audit dir")
        .flatten()
        .collect();
    assert_eq!(records.len(), 1, "one record per nested pid");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(records[0].path()).expect("read"))
            .expect("json");
    assert_eq!(record["action"], "permitted");
    assert_eq!(record["permit"], "allow");
}

#[test]
fn the_child_walk_tracks_starts_exits_and_skips_compiler_and_soldr_subtrees() {
    let children = std::cell::RefCell::new(HashMap::<u32, Vec<u32>>::from([
        (ROOT, vec![200, 210, 220]),
        (200, vec![300]),
        (210, vec![999]),
        (220, vec![998]),
    ]));
    let argvs: HashMap<u32, Vec<String>> = HashMap::from([
        (200, vec![build_script()]),
        (300, vec!["cargo".to_string(), "build".to_string()]),
        (
            210,
            ["rustc", "--crate-name", "x", "--out-dir", "/t/debug/deps"]
                .map(String::from)
                .to_vec(),
        ),
        (999, vec!["cc".to_string()]),
        (220, vec!["/ci/bin/soldr".to_string(), "cargo".to_string()]),
        (998, vec!["cargo".to_string(), "build".to_string()]),
    ]);
    let children_of = |pid: u32| children.borrow().get(&pid).cloned();
    let read = |pid: u32| argvs.get(&pid).cloned();
    let mut tree = ProcessTree::new(ROOT);
    tree.walk(&children_of, &read);
    assert_eq!(
        hazard(tree.assess(300, &no_cwd)).phase_pid,
        200,
        "a build script found by the walk anchors the hazard"
    );
    // The compiler and the nested Soldr were discovered; on the next walk
    // their subtrees are skipped.
    tree.walk(&children_of, &read);
    assert!(!tree.nodes.contains_key(&999));
    assert!(!tree.nodes.contains_key(&998));
    // An exit disappears from the next walk.
    children.borrow_mut().insert(200, Vec::new());
    tree.walk(&children_of, &read);
    assert!(!tree.nodes.contains_key(&300));
    assert_eq!(tree.assess(300, &no_cwd), Assessment::NotCandidate);
}

/// soldr#4198: a tick walks `/proc`, reads argv and working directories, and
/// none of that may run under the state lock -- the lock is a critical
/// section whose watchdog aborts the process when a hold passes 15 s, and on
/// a loaded host the inspection alone took longer. Every probe here asserts
/// the lock is free when it is called; `try_lock` on the same thread fails
/// if the tick still holds it, so the check is deterministic, not timed.
#[test]
fn a_tick_runs_every_host_probe_with_the_state_lock_released() {
    let audit = tempfile::tempdir().expect("audit dir");
    let guard = NestedCargoGuard::new(GuardMode::Enforce, true, Some(audit.path().to_path_buf()));
    guard.bind_root(ROOT);
    let calls = std::cell::RefCell::new(Vec::<&'static str>::new());
    let lock_free = |probe: &'static str| {
        assert!(
            guard.state.try_lock().is_some(),
            "{probe} ran while the tick held the state lock"
        );
        calls.borrow_mut().push(probe);
    };
    let children: HashMap<u32, Vec<u32>> = HashMap::from([(ROOT, vec![200]), (200, vec![300])]);
    let argvs: HashMap<u32, Vec<String>> = HashMap::from([
        (200, vec![build_script()]),
        // A relative --target-dir makes the assessment read the cwd.
        (
            300,
            ["cargo", "build", "--target-dir", "rel"]
                .map(String::from)
                .to_vec(),
        ),
    ]);
    let children_of = |pid: u32| {
        lock_free("child enumeration");
        children.get(&pid).cloned()
    };
    let read = |pid: u32| {
        lock_free("argv read");
        argvs.get(&pid).cloned()
    };
    let cwd_of = |_: u32| {
        lock_free("cwd read");
        None
    };
    let probes = Probes {
        children_of: &children_of,
        read_argv: &read,
        cwd_of: &cwd_of,
    };
    guard.tick_with(&probes);
    let calls = calls.into_inner();
    for probe in ["child enumeration", "argv read", "cwd read"] {
        assert!(calls.contains(&probe), "{probe} never ran: {calls:?}");
    }
    let violation = guard
        .violation()
        .expect("the walked hazard trips the guard");
    assert_eq!(violation.hazard.nested_pid, 300);
    assert_eq!(violation.hazard.reason, HazardReason::UnresolvableTargetDir);
    assert_eq!(std::fs::read_dir(audit.path()).expect("dir").count(), 1);
    // The walked tree was merged back into the shared state.
    let state = guard.state();
    let tree = state.tree.as_ref().expect("bound");
    assert!(tree.nodes.contains_key(&200) && tree.nodes.contains_key(&300));
    assert!(state.handled.contains(&300));
}
