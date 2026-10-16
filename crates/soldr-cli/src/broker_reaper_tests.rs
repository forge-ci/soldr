//! Unit tests for owner-death reaping of broker routes (soldr#3054).

use super::*;
use std::collections::BTreeSet;

/// Build an identified requester key. Test-only shorthand so the fixtures
/// below read as "pid 4242, born at tick 100" rather than repeating the
/// `RequesterKey::Identified(PidKey { .. })` wrapping at every call site.
fn identified(pid: u32, start_token: u64) -> RequesterKey {
    RequesterKey::Identified(PidKey { pid, start_token })
}

fn key(pid: u32, start_token: u64) -> PidKey {
    PidKey { pid, start_token }
}

/// A fixture asks for its own route exactly once and then dies. Nothing else
/// will ever ask for that route again, so it must not survive the grace
/// window. This is the leak the whole module exists to close: 63 daemons in
/// this state after one run of the suite.
#[test]
fn a_route_whose_only_requester_died_is_reaped_after_the_grace_window() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-fixture", identified(4242, 1), start);

    let dead = |_key: PidKey| false;

    let during = ownership.sweep(start, DEFAULT_GRACE, dead);
    assert_eq!(
        during,
        vec![("soldr-daemon-fixture".into(), RouteVerdict::Draining)]
    );

    let after = ownership.sweep(start + DEFAULT_GRACE, DEFAULT_GRACE, dead);
    assert_eq!(
        after,
        vec![("soldr-daemon-fixture".into(), RouteVerdict::Reap)]
    );
}

/// The canonical route for the user's own root is asked for by every build,
/// and each build's process exits when it finishes. Reaping on "the last
/// requester exited" alone would therefore kill the daemon the user actually
/// wants resident, and every cold daemon start is real spawn and bringup work
/// (see `DEFAULT_GRACE`). A new requester inside the window must reset it.
#[test]
fn a_new_requester_inside_the_window_keeps_the_route() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-canonical", identified(100, 1), start);

    let only_200_lives = |k: PidKey| k == key(200, 1);

    // The first build's process is gone; the route starts draining.
    let draining = ownership.sweep(start, DEFAULT_GRACE, |_| false);
    assert_eq!(draining[0].1, RouteVerdict::Draining);

    // The next build arrives before the window elapses.
    ownership.record_request("soldr-daemon-canonical", identified(200, 1), start);
    let after_window = ownership.sweep(start + DEFAULT_GRACE * 2, DEFAULT_GRACE, only_200_lives);
    assert_eq!(
        after_window,
        vec![("soldr-daemon-canonical".into(), RouteVerdict::Live)],
        "a live requester outranks any amount of elapsed time"
    );
}

/// The window is measured from when the route went empty, not from the sweep
/// that noticed it. A broker that sweeps every 30s against a 120s grace would
/// otherwise restart the clock on every tick and never reap anything.
#[test]
fn the_grace_window_runs_from_when_the_route_emptied_not_from_the_last_sweep() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-fixture", identified(7, 1), start);
    let dead = |_key: PidKey| false;

    for tick in 1..4 {
        let verdicts = ownership.sweep(start + Duration::from_secs(30 * tick), DEFAULT_GRACE, dead);
        assert_eq!(verdicts[0].1, RouteVerdict::Draining, "tick {tick}");
    }
    let verdicts = ownership.sweep(start + DEFAULT_GRACE, DEFAULT_GRACE, dead);
    assert_eq!(verdicts[0].1, RouteVerdict::Reap);
}

/// A route may be shared. One requester dying is not the route dying, and the
/// dead one must still be dropped from the set so it cannot keep the route
/// alive forever after its PID is reused by something unrelated.
#[test]
fn a_dead_requester_is_dropped_while_a_live_one_holds_the_route() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-shared", identified(11, 1), start);
    ownership.record_request("soldr-daemon-shared", identified(22, 1), start);
    assert_eq!(ownership.owners_of("soldr-daemon-shared").unwrap().len(), 2);

    let verdicts = ownership.sweep(Instant::now(), DEFAULT_GRACE, |k| k == key(22, 1));
    assert_eq!(verdicts[0].1, RouteVerdict::Live);
    assert_eq!(
        ownership.owners_of("soldr-daemon-shared").unwrap().len(),
        1,
        "the exited requester must not linger in the set"
    );
}

/// Recording the same requester twice is the normal case -- one process runs
/// several builds -- and must not inflate the set.
#[test]
fn the_same_requester_is_recorded_once() {
    let mut ownership = RouteOwnership::new();
    for _ in 0..5 {
        ownership.record_request("soldr-daemon-fixture", identified(99, 1), Instant::now());
    }
    assert_eq!(
        ownership.owners_of("soldr-daemon-fixture").unwrap().len(),
        1
    );
}

/// Routes are independent: reaping one must not disturb another.
#[test]
fn routes_are_classified_independently() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-a", identified(1, 1), start);
    ownership.record_request("soldr-daemon-b", identified(2, 1), start);

    let verdicts = ownership.sweep(start + DEFAULT_GRACE, DEFAULT_GRACE, |k| k == key(2, 1));
    let by_name: BTreeSet<(String, RouteVerdict)> = verdicts.into_iter().collect();
    assert!(by_name.contains(&("soldr-daemon-a".to_string(), RouteVerdict::Reap)));
    assert!(by_name.contains(&("soldr-daemon-b".to_string(), RouteVerdict::Live)));
}

/// After the daemon is gone the route must leave the table, or a broker that
/// runs for days accumulates an entry per fixture that ever existed.
#[test]
fn a_reaped_route_is_forgotten() {
    let mut ownership = RouteOwnership::new();
    ownership.record_request("soldr-daemon-fixture", identified(5, 1), Instant::now());
    assert_eq!(ownership.len(), 1);
    ownership.forget("soldr-daemon-fixture");
    assert_eq!(ownership.len(), 0);
    assert!(ownership.owners_of("soldr-daemon-fixture").is_none());
}

/// The production probe must agree with reality on this host, in both
/// directions. This is the one test that touches the real platform layer, and
/// it is why the module takes `is_alive` as a parameter everywhere else.
#[test]
fn the_production_probe_answers_for_real_processes() {
    let this_process = std::process::id();
    let this_token = crate::platform::process::inspect::process_start_token(this_process)
        .expect("this process must have a resolvable start token");
    assert!(
        requester_is_alive(key(this_process, this_token)),
        "this process is its own proof of liveness"
    );
    // Not a pid this host can be running: `pid_max` is far below this, and a
    // handle-based probe cannot be fooled by the signedness trap that made
    // `kill(u32::MAX as pid_t, 0)` succeed as `kill(-1, 0)`.
    assert!(!requester_is_alive(key(u32::MAX, this_token)));
    // The pid is real and alive, but the token does not match -- exactly the
    // shape a recycled pid produces. The probe must not fall back to
    // pid-only liveness.
    assert!(!requester_is_alive(key(
        this_process,
        this_token.wrapping_add(1)
    )));
}

/// The reap decision must survive a requester whose PID is reused.
///
/// The set holds `(pid, start_token)` pairs, so a probe that only asks "is
/// some process with this number alive" would keep a route alive forever
/// once the kernel recycled the PID onto something unrelated. This test
/// pins the decision logic that sits on top of the PID-Key, using an oracle
/// that reports pid 1234 as alive but under a *different* start token --
/// exactly what a recycled pid looks like from the reaper's point of view.
#[test]
fn a_recycled_pid_does_not_resurrect_a_route() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-fixture", identified(1234, 100), start);

    // A number-based probe would see pid 1234 as alive and keep the route
    // alive forever. The PID-Key-aware probe below reports pid 1234 alive
    // but stamped with a different start token -- the recycled process --
    // which must not count as the original requester.
    let recycled = |k: PidKey| k.pid == 1234 && k.start_token == 200;
    let verdicts = ownership.sweep(start + DEFAULT_GRACE, DEFAULT_GRACE, recycled);
    assert_eq!(verdicts[0].1, RouteVerdict::Reap);

    // And the route is forgotten, so a later request from the recycled PID
    // starts a fresh route rather than re-entering the old one's window.
    ownership.forget("soldr-daemon-fixture");
    ownership.record_request(
        "soldr-daemon-fixture",
        identified(1234, 200),
        start + DEFAULT_GRACE,
    );
    let verdicts = ownership.sweep(start + DEFAULT_GRACE, DEFAULT_GRACE, recycled);
    assert_eq!(verdicts[0].1, RouteVerdict::Live);
}

/// A requester whose start token could not be resolved at record time must
/// never let its route reach `Reap`, no matter how long the window has been
/// open. Killing a daemon whose owner cannot be re-identified risks killing
/// something that has nothing to do with the dead requester's pid.
#[test]
fn a_route_with_an_unidentified_requester_is_never_reaped() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-fixture", RequesterKey::Unidentified, start);
    assert!(ownership
        .owners_of("soldr-daemon-fixture")
        .unwrap()
        .has_unidentified_requester());

    // No identified requester was ever recorded, so the probe is never even
    // consulted for one -- but it must still report Live, not Reap, however
    // far past the grace window the clock has run.
    let far_future = start + DEFAULT_GRACE * 1000;
    let verdicts = ownership.sweep(far_future, DEFAULT_GRACE, |_| false);
    assert_eq!(
        verdicts,
        vec![("soldr-daemon-fixture".into(), RouteVerdict::Live)],
        "an unidentified requester must hold the route Live forever"
    );
}

/// An unidentified requester recorded alongside identified ones still
/// protects the route even after every identified requester exits.
#[test]
fn an_unidentified_requester_outweighs_every_identified_requester_exiting() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-mixed", identified(1, 1), start);
    ownership.record_request("soldr-daemon-mixed", RequesterKey::Unidentified, start);

    let verdicts = ownership.sweep(start + DEFAULT_GRACE * 10, DEFAULT_GRACE, |_| false);
    assert_eq!(verdicts[0].1, RouteVerdict::Live);
}

/// An empty table is the steady state on a machine doing nothing, and must
/// not produce work or panic.
#[test]
fn sweeping_an_empty_table_is_a_no_op() {
    let mut ownership = RouteOwnership::new();
    assert!(ownership
        .sweep(Instant::now(), DEFAULT_GRACE, |_| true)
        .is_empty());
    assert_eq!(ownership.len(), 0);
}

/// The sweep interval must leave room inside the grace window, or a route
/// reaches `Reap` and then waits most of another window to be noticed.
#[test]
fn the_sweep_interval_fits_inside_the_grace_window() {
    assert!(
        REAP_SWEEP_INTERVAL < DEFAULT_GRACE,
        "a sweep slower than the window makes the window meaningless"
    );
}

/// The production platform probe backing `requester_is_alive` must itself be
/// stable and non-zero for a live process, and absent for an impossible pid.
/// This pins the contract at the call site the reaper actually depends on,
/// distinct from `soldr-platform`'s own facade-level test of the same
/// contract.
#[test]
fn the_production_start_token_is_stable_and_bounded() {
    let pid = std::process::id();
    let first = crate::platform::process::inspect::process_start_token(pid);
    assert!(first.is_some());
    assert_eq!(
        first,
        crate::platform::process::inspect::process_start_token(pid)
    );
    assert_eq!(
        crate::platform::process::inspect::process_start_token(u32::MAX),
        None
    );
    assert_eq!(
        crate::platform::process::inspect::process_start_token(0),
        None
    );
}

// ── soldr#3572: route endpoint socket reclamation ───────────────────────────

fn touch_socket(dir: &std::path::Path, stem: &str, suffix: &str) -> std::path::PathBuf {
    let path = dir.join(format!("{stem}{suffix}"));
    // A plain file stands in for a socket node: the sweep's liveness probe
    // refuses to connect to it exactly as it refuses a dead socket.
    std::fs::write(&path, b"").expect("write placeholder endpoint");
    path
}

fn seed_route(dir: &std::path::Path, stem: &str) -> Vec<std::path::PathBuf> {
    [".session.sock", ".control.sock", ".handoff.sock"]
        .iter()
        .map(|suffix| touch_socket(dir, stem, suffix))
        .collect()
}

#[test]
fn dead_route_endpoints_are_removed_and_live_ones_are_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dead = seed_route(dir.path(), "d-deadbeefdeadbeef");
    // A route whose daemon is still up: bind and listen so a connect succeeds.
    let live_stem = "d-cafecafecafecafe";
    let live_paths: Vec<std::path::PathBuf> = [".session.sock", ".control.sock", ".handoff.sock"]
        .iter()
        .map(|suffix| dir.path().join(format!("{live_stem}{suffix}")))
        .collect();
    // `bind_listener` returns a Tokio listener, so it needs a reactor to bind.
    // The listener keeps the underlying socket alive after the runtime ends,
    // which is all this test needs — a peer that answers `connect`.
    let mut listeners = Vec::new();
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let _guard = runtime.enter();
        for path in &live_paths {
            let listener = crate::platform::ipc::broker::bind_listener(&path.to_string_lossy(), 8)
                .expect("bind live endpoint");
            listeners.push(listener);
        }
    }

    let removed = crate::broker_reaper::reap_dead_route_endpoints(dir.path());

    assert_eq!(removed, vec!["d-deadbeefdeadbeef".to_owned()]);
    for path in &dead {
        assert!(
            !path.exists(),
            "dead endpoint {} should be removed",
            path.display()
        );
    }
    for path in &live_paths {
        assert!(
            path.exists(),
            "live endpoint {} must be kept",
            path.display()
        );
    }
}

#[test]
fn the_sweep_never_touches_the_broker_itself_or_unrelated_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Files that share the broker directory and must survive untouched.
    let keep = [
        "soldr-broker.sock",
        "soldr-broker",
        "broker-lease-abc.sqlite3",
        "broker-spawn.log",
        "bind.lock",
        "stamp",
    ];
    for name in keep {
        std::fs::write(dir.path().join(name), b"").expect("write bystander");
    }
    // A route with only some of its sockets present is still reclaimed once
    // none of them answer.
    touch_socket(dir.path(), "d-partialpartial", ".control.sock");

    crate::broker_reaper::reap_dead_route_endpoints(dir.path());

    for name in keep {
        assert!(
            dir.path().join(name).exists(),
            "{name} must not be removed by the route sweep"
        );
    }
    assert!(!dir.path().join("d-partialpartial.control.sock").exists());
}

#[test]
fn a_missing_broker_directory_is_not_an_error() {
    let missing = std::path::Path::new("/nonexistent/broker/dir/for/tests");
    assert!(crate::broker_reaper::reap_dead_route_endpoints(missing).is_empty());
}

/// Not a regression test: proves the sweep against the *measured* pileup on a
/// developer host (#3572 reported 1,725 routes, 1,724 with no listener).
/// Ignored by default because it would delete a real home's routes.
#[test]
#[ignore = "destructive: removes dead route sockets from the real broker dir"]
fn the_real_broker_directory_has_no_dead_routes_left_after_a_sweep() {
    let Ok(endpoint) = crate::broker_identity::ResolvedBrokerEndpoint::resolve() else {
        return;
    };
    let Some(dir) = endpoint
        .executable_path
        .parent()
        .map(std::path::Path::to_path_buf)
    else {
        return;
    };
    let first = crate::broker_reaper::reap_dead_route_endpoints(&dir);
    let second = crate::broker_reaper::reap_dead_route_endpoints(&dir);
    assert!(
        second.is_empty(),
        "a second sweep found more dead routes: {second:?}"
    );
    println!("swept {} dead route(s) from {}", first.len(), dir.display());
}

fn reap_candidate_table() -> (Mutex<RouteOwnership>, Instant) {
    let start = Instant::now();
    let mut ownership = RouteOwnership::new();
    ownership.record_request("soldr-daemon-a", identified(1, 1), start);
    (Mutex::new(ownership), start)
}

/// soldr#3815: the starter exited, but another session still has compiles in
/// flight. The route must survive, and its grace window must restart.
#[test]
fn requesters_gone_but_daemon_busy_is_not_reaped() {
    let (table, start) = reap_candidate_table();
    let later = start + DEFAULT_GRACE * 2;
    let candidates: Vec<String> = table
        .lock()
        .unwrap()
        .sweep(later, DEFAULT_GRACE, |_| false)
        .into_iter()
        .filter_map(|(n, v)| (v == RouteVerdict::Reap).then_some(n))
        .collect();
    assert_eq!(candidates, vec!["soldr-daemon-a".to_string()]);

    let reapable = defer_busy_candidates(&table, candidates, later, |_| DaemonLoad::Busy);
    assert!(reapable.is_empty());

    // The window restarted at `later`: still draining just after, reapable
    // only once a full idle window has passed.
    let soon = table
        .lock()
        .unwrap()
        .sweep(later + Duration::from_secs(1), DEFAULT_GRACE, |_| false);
    assert_eq!(soon[0].1, RouteVerdict::Draining);
    let idle_window = table
        .lock()
        .unwrap()
        .sweep(later + DEFAULT_GRACE, DEFAULT_GRACE, |_| false);
    assert_eq!(idle_window[0].1, RouteVerdict::Reap);
}

/// Acceptance criterion 3: an idle daemon is still reaped after the grace.
#[test]
fn requesters_gone_daemon_idle_grace_elapsed_is_reaped() {
    let (table, start) = reap_candidate_table();
    let verdict = table
        .lock()
        .unwrap()
        .sweep(start + DEFAULT_GRACE, DEFAULT_GRACE, |_| false);
    assert_eq!(verdict[0].1, RouteVerdict::Reap);
    let reapable = defer_busy_candidates(
        &table,
        vec!["soldr-daemon-a".to_string()],
        start + DEFAULT_GRACE,
        |_| DaemonLoad::Idle,
    );
    assert_eq!(reapable, vec!["soldr-daemon-a".to_string()]);
}

/// A daemon that cannot be asked is not shown idle, so it is not reaped, and
/// its window is left alone so the next sweep asks again.
#[test]
fn busy_query_failure_is_not_reaped_and_is_retried_next_sweep() {
    let (table, start) = reap_candidate_table();
    let at = start + DEFAULT_GRACE;
    let reapable = defer_busy_candidates(&table, vec!["soldr-daemon-a".to_string()], at, |_| {
        DaemonLoad::Unknown
    });
    assert!(reapable.is_empty());
    let again = table.lock().unwrap().sweep(at, DEFAULT_GRACE, |_| false);
    assert_eq!(again[0].1, RouteVerdict::Reap);
}
