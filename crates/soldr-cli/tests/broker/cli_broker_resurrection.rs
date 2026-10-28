//! Process-level correctness coverage for soldr#2476 broker resurrection.

use crate::common;
use crate::common::tracked_child::TrackedChild;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);
/// Wall-clock envelope for one resurrection scenario. Sized for what the
/// scenario actually contains, not for a hang: the expired-lease case
/// injects a 30 s pause into the stale owner
/// (`SOLDR_TEST_KNOWN_BAD_STOP_PAUSE_MS`) and only then lets the
/// replacement stage an image, spawn a broker, and reach ready -- a real cold
/// start (at the time, including a whole-executable hash, soldr#2517; that
/// term has since been memoized and optimized, soldr#2996/#3173). Measured
/// before those fixes: 45.3 s
/// on a warm workstation, 48 s on a 4-vCPU runner under `soldr ci-test`
/// with four test threads and Dylint compiles alongside (runs 33920430360
/// attempts 2 and 3, both failing a 45 s envelope by the same margin).
/// nextest's budget for this group is 60 s x 5, so this stays well inside
/// the real hang detector.
const RESURRECTION_TEST_ENVELOPE: Duration = Duration::from_secs(120);

fn stage_incumbent_broker(home: &Path) -> std::path::PathBuf {
    let broker_dir = home.join(".soldr").join("broker");
    std::fs::create_dir_all(&broker_dir).expect("create incumbent broker directory");
    let broker = broker_dir.join(
        if matches!(
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::HostOs::Windows
        ) {
            "soldr-broker.exe"
        } else {
            "soldr-broker"
        },
    );
    std::fs::copy(common::soldr_bin(), &broker).expect("stage incumbent broker image");
    broker
}

fn front_door(home: &Path) -> Command {
    let mut command = front_door_capturing_stderr(home);
    command.stderr(Stdio::null());
    command
}

/// Same front door, but with stderr left capturable — soldr#2549's broker
/// image-mismatch warning is a stderr diagnostic.
fn front_door_capturing_stderr(home: &Path) -> Command {
    let mut command = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut command);
    command
        .arg("version")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    command
}

fn wait_for_child(child: &mut Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait().expect("inspect front door") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

fn wait_for_path(path: &Path, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(POLL);
    }
    path.exists()
}

fn kill_and_reap(child: &mut Child) {
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn resurrection_failure_context(
    home: &Path,
    stale_stderr: &str,
    replacement_stderr: &str,
) -> String {
    format!(
        "broker status:\n{}\nbroker-spawn.log:\n{}\nstale owner stderr:\n{}\nreplacement owner stderr:\n{}",
        broker_status(home),
        spawn_log(home),
        stale_stderr,
        replacement_stderr,
    )
}

fn spawn_log(home: &Path) -> String {
    std::fs::read_to_string(home.join(".soldr/broker/broker-spawn.log")).unwrap_or_default()
}

/// A front-door command is allowed to complete after it has handed broker
/// startup to the detached process.  In particular, Rosetta can make the
/// interval between that handoff and the eventual listener bind large enough
/// that an immediate log snapshot races the replacement itself.  Wait for the
/// observable event this family of tests is asserting rather than treating the
/// handoff as readiness.
fn wait_for_successful_bind(home: &Path, deadline: Instant) -> String {
    let mut log = spawn_log(home);
    while Instant::now() < deadline
        && !log
            .lines()
            .any(|line| line.contains("stable endpoint bound at"))
    {
        std::thread::sleep(POLL);
        log = spawn_log(home);
    }
    log
}

fn broker_status(home: &Path) -> String {
    let mut command = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut command);
    let output = command
        .args(["broker", "status"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .output()
        .expect("query broker status");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn stop_broker(home: &Path) {
    let mut command = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut command);
    let _ = command
        .args(["broker", "stop"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .output();
}

fn stop_broker_and_confirm_absent(home: &Path) -> Result<(), String> {
    let mut command = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut command);
    let output = command
        .args(["broker", "stop"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("run broker stop: {error}"))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        return Err(format!(
            "broker stop exited {:?}: {combined}",
            output.status.code()
        ));
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut status = String::new();
    while Instant::now() < deadline {
        status = broker_status(home);
        if status.contains("not running") {
            return Ok(());
        }
        std::thread::sleep(POLL);
    }
    Err(format!(
        "broker stop reported success but a route still owns the stable endpoint; \
         stop output: {combined}; final status: {status}"
    ))
}

fn cleanup_race_owners(
    home: &Path,
    stale_owner: TrackedChild,
    replacement_owner: TrackedChild,
    incumbent: &mut Child,
) -> (String, String, Result<(), String>) {
    // soldr#3197: both owners' stderr has drained since spawn, so this collects
    // it (terminating an owner still running) without the wait-then-read
    // deadlock a raw piped child has.
    let stale_stderr = stale_owner
        .wait_bounded(Duration::ZERO)
        .stderr_lossy()
        .into_owned();
    let replacement_stderr = replacement_owner
        .wait_bounded(Duration::ZERO)
        .stderr_lossy()
        .into_owned();
    kill_and_reap(incumbent);
    let stopped = stop_broker_and_confirm_absent(home);
    (stale_stderr, replacement_stderr, stopped)
}

fn remove_broker(home: &Path) -> std::process::Output {
    let mut command = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut command);
    command
        .args(["broker", "remove"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .output()
        .expect("run broker remove")
}

/// Bring up a broker that reports a same-version-but-different-image identity,
/// running from the production per-home image path (Windows broker identity is
/// path-derived, so launching the build artifact directly would not model it).
fn spawn_simulated_old_image_broker(home: &Path, instance: &str) -> Child {
    let incumbent_broker = stage_incumbent_broker(home);
    let mut incumbent_command = Command::new(incumbent_broker);
    common::scrub_outer_soldr_env(&mut incumbent_command);
    incumbent_command
        .args(["broker", "serve"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("SOLDR_INTERNAL_BROKER_INSTANCE_ID", instance)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // soldr#2854: the image was staged moments ago, so this spawn can lose the
    // fork/exec race with another thread's copy and answer ETXTBSY.
    common::spawn_staged(&mut incumbent_command).expect("spawn simulated old-image broker")
}

fn wait_for_broker_instance(home: &Path, instance: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut status = String::new();
    while Instant::now() < deadline {
        status = broker_status(home);
        if status.contains(instance) {
            return status;
        }
        std::thread::sleep(POLL);
    }
    status
}

/// soldr#2549: the broker is a stable, long-lived singleton. A same-version
/// image-digest mismatch must produce a loud diagnostic, never a lifecycle
/// action — this is the inversion of the old
/// `issue_2481_same_version_old_image_is_replaced_before_readiness`, which
/// asserted the incumbent was retired automatically.
#[test]
fn issue_2549_same_version_old_image_broker_is_never_replaced_automatically() {
    let home = common::unique_temp_dir("broker-same-version-old-image");
    let old_instance = format!("soldr-{}-{}", env!("CARGO_PKG_VERSION"), "0".repeat(64));
    let mut incumbent = spawn_simulated_old_image_broker(&home, &old_instance);
    let before = wait_for_broker_instance(&home, &old_instance);

    let front_door = front_door_capturing_stderr(&home)
        .stderr(Stdio::piped())
        .output()
        .expect("run front door against old-image broker");
    let stderr = String::from_utf8_lossy(&front_door.stderr).into_owned();

    // The incumbent must still be the bound broker after the front door has
    // fully returned: not stopped, not killed, not staged over.
    let after = broker_status(&home);
    let survived = incumbent.try_wait().expect("inspect incumbent").is_none();

    // Recovery is explicit and operator-driven.
    let removal = remove_broker(&home);
    let removal_output = format!(
        "{}{}",
        String::from_utf8_lossy(&removal.stdout),
        String::from_utf8_lossy(&removal.stderr)
    );
    let incumbent_exit = wait_for_child(&mut incumbent, Instant::now() + Duration::from_secs(15));
    if incumbent_exit.is_none() {
        let _ = incumbent.kill();
        let _ = incumbent.wait();
    }
    let log = spawn_log(&home);
    stop_broker(&home);

    assert!(
        before.contains(&old_instance),
        "simulated old-image broker never became ready; last status:\n{before}"
    );
    assert!(
        front_door.status.success(),
        "the front door must succeed through a mismatched broker; status={:?}\n{log}",
        front_door.status
    );
    assert!(
        survived,
        "soldr#2549: a live broker must never be retired automatically for an identity mismatch"
    );
    assert!(
        after.contains(&old_instance),
        "the incumbent must still own the endpoint after the front door ran; last status:\n{after}\n{log}"
    );
    assert!(
        stderr.contains("soldr broker remove"),
        "the mismatch warning must name the explicit recovery command; stderr:\n{stderr}"
    );
    assert!(
        !log.contains("stable endpoint bound at"),
        "no replacement broker may be staged or spawned for a live incumbent\n{log}"
    );
    assert!(
        incumbent_exit.is_some(),
        "`soldr broker remove` must retire the broker the operator asked it to:\n{removal_output}"
    );
    assert!(
        removal.status.success(),
        "`soldr broker remove` must succeed:\n{removal_output}"
    );
}

#[test]
fn issue_2920_known_bad_older_broker_is_retired_once_with_a_structured_outcome() {
    let home = common::unique_temp_dir("broker-known-bad-retirement");
    let old_instance = format!("soldr-0.9.0-{}", "0".repeat(64));
    let mut incumbent = spawn_simulated_old_image_broker(&home, &old_instance);
    let before = wait_for_broker_instance(&home, &old_instance);

    let front_door = front_door_capturing_stderr(&home)
        .stderr(Stdio::piped())
        .output()
        .expect("run newer front door against known-bad broker");
    let stderr = String::from_utf8_lossy(&front_door.stderr);
    let incumbent_exit = wait_for_child(&mut incumbent, Instant::now() + Duration::from_secs(15));
    let after = broker_status(&home);
    let log = wait_for_successful_bind(&home, Instant::now() + Duration::from_secs(30));
    stop_broker(&home);

    assert!(
        before.contains(&old_instance),
        "old broker never became ready: {before}"
    );
    assert!(
        front_door.status.success(),
        "front door failed: {stderr}\n{log}"
    );
    assert!(
        incumbent_exit.is_some(),
        "known-bad broker must be retired exactly once: {log}"
    );
    assert!(
        !after.contains(&old_instance),
        "replacement must not retain the old identity: {after}\n{log}"
    );
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("stable endpoint bound at"))
            .count(),
        1,
        "only the replacement may bind the stable endpoint: {log}"
    );
    let record = stderr
        .lines()
        .find_map(|line| line.strip_prefix("soldr: broker retirement "))
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
        .expect("retirement diagnostic is valid JSON")
        .expect("retirement diagnostic is present");
    assert_eq!(record["event"], "known_bad_broker_retirement");
    assert_eq!(record["old_version"], "0.9.0");
    assert_eq!(record["matched_range"], "=0.9.0");
    assert_eq!(record["outcome"], "retired");
    assert!(record["staged_image_removed"].as_bool().unwrap_or(false));
}

/// Every contender first observes the same known-bad broker, but only the
/// lease holder may retire it and stage the replacement. This keeps the
/// soldr#2920 exception single-flight rather than turning recovery into a
/// second stampede.
#[test]
fn issue_2920_concurrent_newer_clients_retire_one_known_bad_broker() {
    let home = common::unique_temp_dir("broker-known-bad-retirement-concurrent");
    let old_instance = format!("soldr-0.9.0-{}", "0".repeat(64));
    let mut incumbent = spawn_simulated_old_image_broker(&home, &old_instance);
    let before = wait_for_broker_instance(&home, &old_instance);

    let mut children: Vec<_> = (0..8)
        .map(|_| {
            front_door(&home)
                .spawn()
                .expect("spawn retirement contender")
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut failures = Vec::new();
    for child in &mut children {
        match wait_for_child(child, deadline) {
            Some(status) if status.success() => {}
            Some(status) => failures.push(format!("pid {} exited {status}", child.id())),
            None => {
                failures.push(format!("pid {} did not exit", child.id()));
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let incumbent_exit = wait_for_child(&mut incumbent, Instant::now() + Duration::from_secs(15));
    let after = broker_status(&home);
    let log = spawn_log(&home);
    stop_broker(&home);

    assert!(
        before.contains(&old_instance),
        "old broker never became ready: {before}"
    );
    assert!(
        failures.is_empty(),
        "newer clients must all complete: {failures:?}\n{log}"
    );
    assert!(
        incumbent_exit.is_some(),
        "the known-bad broker must be retired: {log}"
    );
    assert!(
        !after.contains(&old_instance),
        "the old broker must not still own the endpoint: {after}\n{log}"
    );
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("stable endpoint bound at"))
            .count(),
        1,
        "only one replacement may win the single-flight recovery: {log}"
    );
}

/// soldr#2920: lease expiry after the old broker stops must not let the stale
/// owner unlink a replacement endpoint or delete the image another lease
/// holder just staged. The first contender intentionally pauses beyond the
/// five-second lease after stop; the second owns recovery, while the first is
/// fenced before cleanup.
#[test]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn issue_2920_expired_retirement_lease_preserves_replacement_resources() {
    let deadline = Instant::now() + RESURRECTION_TEST_ENVELOPE;
    let home = common::unique_temp_dir("broker-known-bad-expired-lease");
    let old_instance = format!("soldr-0.9.0-{}", "0".repeat(64));
    let mut incumbent = spawn_simulated_old_image_broker(&home, &old_instance);
    let before = wait_for_broker_instance(&home, &old_instance);
    let stopped = home.join("known-bad-stopped");
    let continue_stale_owner = home.join("allow-stale-owner-to-resume");
    let replacement_ready = home.join("replacement-ready");

    let mut stale_owner = front_door_capturing_stderr(&home);
    stale_owner
        .env("SOLDR_TEST_KNOWN_BAD_STOP_PAUSE_MS", "30000")
        .env("SOLDR_TEST_KNOWN_BAD_STOP_READY_FILE", &stopped)
        .env(
            "SOLDR_TEST_KNOWN_BAD_STOP_CONTINUE_FILE",
            &continue_stale_owner,
        );
    let mut stale_owner = common::tracked_child::spawn_tracked(&mut stale_owner)
        .expect("spawn stale retirement owner");
    let stopped_observed = wait_for_path(&stopped, deadline);
    let stale_owner_marked_replacement = replacement_ready.exists();

    let mut replacement_owner = front_door_capturing_stderr(&home);
    replacement_owner
        // This is injected only into the contender that must prove its
        // replacement is genuinely STATUS-ready. The stale owner cannot
        // publish it while paused before cleanup.
        .env(
            "SOLDR_TEST_KNOWN_BAD_REPLACEMENT_READY_FILE",
            &replacement_ready,
        );
    let mut replacement_owner = common::tracked_child::spawn_tracked(&mut replacement_owner)
        .expect("spawn replacement owner after lease expiry");
    let replacement_ready_observed = wait_for_path(&replacement_ready, deadline);
    let replacement_status_text = broker_status(&home);
    let image = home.join(".soldr").join("broker").join(
        if matches!(
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::HostOs::Windows
        ) {
            "soldr-broker.exe"
        } else {
            "soldr-broker"
        },
    );
    let replacement_image = std::fs::read(&image).ok();

    if !stopped_observed
        || stale_owner_marked_replacement
        || !replacement_ready_observed
        || replacement_status_text.contains(&old_instance)
        || replacement_image.is_none()
    {
        std::fs::write(&continue_stale_owner, b"continue\n").ok();
        let (stale_stderr, replacement_stderr, cleanup) =
            cleanup_race_owners(&home, stale_owner, replacement_owner, &mut incumbent);
        let diagnostics = resurrection_failure_context(&home, &stale_stderr, &replacement_stderr);
        panic!(
            "replacement never became ready before stale cleanup: stopped={stopped_observed} \
             stale_owner_marked_replacement={stale_owner_marked_replacement} \
             replacement_ready={replacement_ready_observed} old_instance_still_present={} \
             staged_image_exists={} cleanup={cleanup:?}\n{diagnostics}",
            replacement_status_text.contains(&old_instance),
            replacement_image.is_some(),
        );
    }

    // Only after a new owner answers STATUS and its image has been staged do
    // we let the stale owner try cleanup. Its next lease renewal must fence
    // and leave both replacement resources byte-for-byte intact.
    std::fs::write(&continue_stale_owner, b"continue\n").expect("release stale owner");
    let stale_status =
        stale_owner.wait_for_exit(deadline.saturating_duration_since(Instant::now()));
    let replacement_status =
        replacement_owner.wait_for_exit(deadline.saturating_duration_since(Instant::now()));
    let incumbent_exit = wait_for_child(&mut incumbent, deadline);
    let after = broker_status(&home);
    let image_after = std::fs::read(&image);
    let (stale_stderr, replacement_stderr, cleanup) =
        cleanup_race_owners(&home, stale_owner, replacement_owner, &mut incumbent);
    let diagnostics = format!(
        "cleanup={cleanup:?}\n{}",
        resurrection_failure_context(&home, &stale_stderr, &replacement_stderr)
    );

    assert!(
        before.contains(&old_instance),
        "old broker never became ready: {before}\n{diagnostics}"
    );
    assert!(
        stopped.exists(),
        "stale owner never reached the post-stop pause\n{diagnostics}"
    );
    assert!(
        incumbent_exit.is_some(),
        "the old broker must be stopped\n{diagnostics}"
    );
    assert!(
        replacement_status.is_some_and(|status| status.success()),
        "the replacement owner must fully recover the route: {replacement_status:?}\n{diagnostics}"
    );
    assert!(
        !replacement_status_text.contains(&old_instance),
        "the fully-started replacement must own the endpoint: {replacement_status_text}\n{diagnostics}"
    );
    assert!(
        stale_status.is_some_and(|status| status.success()),
        "the non-compiling version front door must preserve its exit contract: {stale_status:?}\n{diagnostics}"
    );
    let record = stale_stderr
        .lines()
        .find_map(|line| line.strip_prefix("soldr: broker retirement "))
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
        .expect("fenced retirement diagnostic is valid JSON")
        .expect("fenced retirement diagnostic is present");
    assert_eq!(record["outcome"], "failed");
    assert!(
        record["error"]
            .as_str()
            .is_some_and(|error| error.contains("broker resurrection lease was fenced")),
        "the structured failure must identify the lost lease: {record}\n{diagnostics}"
    );
    assert!(
        !after.contains(&old_instance),
        "the replacement must remain bound after stale cleanup is fenced: {after}\n{diagnostics}"
    );
    assert_eq!(
        image_after.expect("the stale owner must retain the replacement image"),
        replacement_image.expect("replacement staged image exists"),
        "the stale owner must not modify the replacement's staged image: {}",
        image.display()
    );
    assert!(
        cleanup.is_ok(),
        "race cleanup must stop the detached broker and leave no stable endpoint owner: {cleanup:?}\n{diagnostics}"
    );
}

/// soldr#2554: `soldr env --json` against a broker started by a different
/// Soldr image must stay byte-clean JSON on stdout, with no soldr#2549
/// mismatch warning on stderr either — a caller that merges the two streams
/// to parse this output (as `.github/scripts/gnu_linux_toolchain_e2e.py`
/// does) must never see its `json.loads()` broken by an unrelated
/// diagnostic. This is the inverse of
/// `issue_2549_same_version_old_image_broker_is_never_replaced_automatically`,
/// which asserts the warning DOES appear for a human-facing command.
#[test]
fn issue_2554_env_json_against_mismatched_broker_stays_parseable() {
    let home = common::unique_temp_dir("broker-env-json-mismatch");
    let old_instance = format!("soldr-{}-{}", env!("CARGO_PKG_VERSION"), "0".repeat(64));
    let mut incumbent = spawn_simulated_old_image_broker(&home, &old_instance);
    let before = wait_for_broker_instance(&home, &old_instance);

    // A bare cwd outside the workspace, plus SOLDR_NO_BOOTSTRAP, keep PyO3
    // sysroot detection a no-op: this test's process inherits the outer
    // `cargo test` run's toolchain env, and under the isolated `HOME` above
    // that can otherwise send `env --json` down soldr's rustup-bootstrap
    // path -- noisy stderr unrelated to the mismatch-warning invariant this
    // test checks.
    let cwd = common::unique_temp_dir("broker-env-json-mismatch-cwd");
    let mut command = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut command);
    let output = command
        // --plan-only (soldr#2304): this test's subject is the soldr#2549
        // mismatch-warning suppression in machine-readable mode, not
        // toolchain materialization — which is host-dependent and
        // unsupported for a fixed foreign triple on the darwin/windows
        // lanes. The materializing-quiet path is covered by the
        // env-unification container verification.
        .args([
            "env",
            "--target",
            "aarch64-unknown-linux-gnu",
            "--json",
            "--plan-only",
        ])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("SOLDR_NO_BOOTSTRAP", "1")
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .output()
        .expect("run env --json against mismatched broker");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // Nothing retires this incumbent -- soldr#2549 forbids automatic
    // replacement and this test never runs `broker remove` -- so waiting for
    // it to exit only ever spent the full deadline. Reap it directly.
    kill_and_reap(&mut incumbent);
    let log = spawn_log(&home);
    stop_broker(&home);

    assert!(
        before.contains(&old_instance),
        "simulated old-image broker never became ready; last status:\n{before}"
    );
    assert!(
        output.status.success(),
        "env --json must succeed through a mismatched broker; status={:?}\n{log}",
        output.status
    );
    assert!(
        stderr.is_empty(),
        "soldr#2554: --json mode must suppress every unsolicited diagnostic, \
         including the soldr#2549 mismatch warning; stderr:\n{stderr}"
    );
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "soldr#2554: env --json stdout must be valid JSON on its own; error={err}\nstdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(payload["command"], "env");
}

#[test]
fn issue_2476_sigstop_owner_is_fenced_after_lease_expiry() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let home = common::unique_temp_dir("broker-sigstop-takeover");
    let ready = home.join("lease-acquired");
    let stopped_owner_stderr = home.join("stopped-owner.stderr");
    let mut stopped_owner = front_door(&home)
        // Longer than the five-second lease, but short enough that the
        // resumed owner reaches its fence check promptly even when the OS
        // restarts an interrupted sleep with its remaining duration.
        .env("SOLDR_TEST_BROKER_LEASE_PAUSE_MS", "6000")
        .env("SOLDR_TEST_BROKER_LEASE_READY_FILE", &ready)
        .stderr(
            std::fs::File::create(&stopped_owner_stderr)
                .expect("create stopped-owner diagnostic log"),
        )
        .spawn()
        .expect("spawn lease owner");

    // Cold target-run hosts may spend tens of seconds hashing the first
    // broker image before the lease-ready test seam is reached.
    let ready_deadline = Instant::now() + Duration::from_secs(90);
    while !ready.exists() && Instant::now() < ready_deadline {
        std::thread::sleep(POLL);
    }
    assert!(ready.exists(), "first host never acquired the lease");
    let pid = stopped_owner.id().to_string();
    let stop = Command::new("kill")
        .args(["-STOP", &pid])
        .status()
        .expect("SIGSTOP lease owner");
    assert!(stop.success(), "could not SIGSTOP lease owner");

    let mut replacement = front_door(&home)
        .spawn()
        .expect("spawn replacement contender");
    let replacement_status =
        wait_for_child(&mut replacement, Instant::now() + Duration::from_secs(30));

    let _ = Command::new("kill").args(["-CONT", &pid]).status();
    let resumed_status =
        wait_for_child(&mut stopped_owner, Instant::now() + Duration::from_secs(15));
    if resumed_status.is_none() {
        let _ = stopped_owner.kill();
        let _ = stopped_owner.wait();
    }
    let log = wait_for_successful_bind(&home, Instant::now() + Duration::from_secs(30));
    let stopped_owner_diagnostic =
        std::fs::read_to_string(&stopped_owner_stderr).unwrap_or_default();
    stop_broker(&home);

    assert!(
        replacement_status.is_some_and(|status| status.success()),
        "a contender must recover after the stopped owner's five-second lease expires\n{log}"
    );
    assert!(
        resumed_status.is_some_and(|status| status.success()),
        "the resumed former holder must observe its fence and exit without staging/spawning; \
             status={resumed_status:?}\nstderr:\n{stopped_owner_diagnostic}\n{log}"
    );
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("stable endpoint bound at"))
            .count(),
        1,
        "the fenced owner must not spawn a second broker after resume\n{log}"
    );
}

#[cfg(debug_assertions)]
struct SlowStageBrokerCleanup(std::path::PathBuf);

#[cfg(debug_assertions)]
impl Drop for SlowStageBrokerCleanup {
    fn drop(&mut self) {
        let mut command = Command::new(common::soldr_bin());
        common::scrub_outer_soldr_env(&mut command);
        command
            .args(["broker", "stop"])
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0);
        if let Ok(child) = common::tracked_child::spawn_tracked(&mut command) {
            let _ = child.wait_bounded(Duration::from_secs(15));
        }
    }
}

#[test]
#[cfg(debug_assertions)]
fn broker_starts_after_uncontended_image_staging_outlives_lease() {
    let home = common::unique_temp_dir("broker-slow-image-stage");
    let cleanup = SlowStageBrokerCleanup(home.clone());
    let marker = home.join("post-sync-delay-completed");
    let mut command = front_door_capturing_stderr(&home);
    command.env("SOLDR_TEST_BROKER_POST_SYNC_PAUSE_MS", "6000");
    command.env("SOLDR_TEST_BROKER_POST_SYNC_MARKER", &marker);
    let output = common::tracked_child::spawn_tracked(&mut command)
        .expect("spawn front door")
        .wait_bounded(Duration::from_secs(90));
    let log = wait_for_successful_bind(&home, Instant::now() + Duration::from_secs(15));
    drop(cleanup);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.timed_out, "front door timed out: {stderr}");
    assert!(
        output.status.is_some_and(|status| status.success()),
        "front door failed: {stderr}"
    );
    assert_eq!(
        std::fs::read(&marker).ok().as_deref(),
        Some(b"completed".as_slice()),
        "post-sync delay did not complete: {stderr}"
    );
    assert!(
        log.contains("stable endpoint bound at"),
        "broker did not become ready; stderr: {stderr}; log: {log}"
    );
    assert!(
        !stderr.contains("lease was fenced"),
        "uncontended lease was lost: {stderr}"
    );
}

#[cfg(debug_assertions)]
struct PausedStager {
    child: Option<TrackedChild>,
    resume: std::path::PathBuf,
}

#[cfg(debug_assertions)]
impl PausedStager {
    fn finish(mut self) -> common::tracked_child::TrackedOutput {
        let _ = std::fs::write(&self.resume, b"resume");
        self.child
            .take()
            .expect("tracked stager")
            .wait_bounded(Duration::from_secs(30))
    }
}

#[cfg(debug_assertions)]
impl Drop for PausedStager {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.resume, b"resume");
        if let Some(child) = self.child.take() {
            let _ = child.wait_bounded(Duration::from_secs(30));
        }
    }
}

#[test]
#[cfg(debug_assertions)]
fn stale_image_stager_does_not_replace_a_successor_after_lease_takeover() {
    let home = common::unique_temp_dir("broker-slow-stage-takeover");
    let cleanup = SlowStageBrokerCleanup(home.clone());
    let ready = home.join("stage-ready");
    let resume = home.join("stage-resume");
    let stale_published = home.join("stale-published");
    let mut first = front_door_capturing_stderr(&home);
    first
        .env("SOLDR_TEST_BROKER_POST_SYNC_PAUSE_MS", "90000")
        .env("SOLDR_TEST_BROKER_POST_SYNC_READY", &ready)
        .env("SOLDR_TEST_BROKER_POST_SYNC_RESUME", &resume)
        .env("SOLDR_TEST_BROKER_PUBLISHED_MARKER", &stale_published);
    let first = PausedStager {
        child: Some(common::tracked_child::spawn_tracked(&mut first).expect("spawn first owner")),
        resume,
    };
    assert!(
        wait_for_path(&ready, Instant::now() + Duration::from_secs(60)),
        "first owner never reached stage"
    );
    std::thread::sleep(Duration::from_secs(6));
    let mut second = front_door_capturing_stderr(&home);
    let second = common::tracked_child::spawn_tracked(&mut second)
        .expect("spawn successor")
        .wait_bounded(Duration::from_secs(60));
    let log_before = wait_for_successful_bind(&home, Instant::now() + Duration::from_secs(15));
    let first = first.finish();
    let log_after = spawn_log(&home);
    drop(cleanup);
    let first_stderr = String::from_utf8_lossy(&first.stderr);
    let second_stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        !second.timed_out && second.status.is_some_and(|status| status.success()),
        "successor failed: {second_stderr}"
    );
    assert!(
        log_before.contains("stable endpoint bound at"),
        "successor did not bind: {second_stderr}; {log_before}"
    );
    assert!(!first.timed_out, "former owner timed out: {first_stderr}");
    assert!(
        first_stderr.contains("lease was fenced"),
        "former owner did not observe the successor: {first_stderr}"
    );
    assert!(
        !stale_published.exists(),
        "former owner published over its successor"
    );
    assert_eq!(
        log_after.matches("stable endpoint bound at").count(),
        1,
        "multiple broker binds: {log_after}"
    );
}
