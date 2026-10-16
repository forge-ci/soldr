#[cfg(test)]
mod lifecycle_event_tests {
    use crate::core::SoldrPaths;
    use crate::daemon::lifecycle::*;
    use running_process::broker::backend_handle::DaemonProcess;
    use running_process::broker::protocol::Endpoint;
    use tempfile::TempDir;

    pub(super) fn write_route_claim(paths: &SoldrPaths, pid: u32, exe_path: &std::path::Path) {
        let endpoint = if crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
        {
            Endpoint::windows_pipe(exe_path.display().to_string(), "soldr-test")
        } else {
            Endpoint::unix_socket(
                exe_path.display().to_string(),
                paths.root.join("test.session.sock").display().to_string(),
            )
        }
        .expect("test endpoint");
        let claim = DaemonProcess {
            pid,
            exe_hash: [0; 32],
            legacy_exe_sha256: [0; 32],
            exe_path: exe_path.to_path_buf(),
            boot_id: "test-boot".to_string(),
            ipc_endpoint: endpoint,
            started_at_unix_ms: 0,
            idle_timeout_secs: None,
        };
        crate::daemon::backend_handle_adoption::publish_broker_route_claim(paths, &claim)
            .expect("route claim");
    }

    fn read_events(paths: &SoldrPaths) -> Vec<serde_json::Value> {
        let path = crate::cache_lib::daemon_lifecycle_log_path(paths);
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each line is a JSON object"))
            .collect()
    }

    // soldr#1808 requires extending this log "without breaking readers that
    // ignore unknown fields". The stricter property is the one worth pinning:
    // a record with no attribution must serialize exactly as it did before,
    // because `crates/soldr-cli/tests/daemon/cli_daemon_lifecycle.rs`
    // matches the raw substring
    // `"event":"spawn"` rather than parsing. Absent fields must vanish, not
    // appear as nulls.
    #[test]
    fn a_record_without_details_keeps_its_original_three_fields() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));

        append_lifecycle_event(&paths, "spawn");

        let events = read_events(&paths);
        assert_eq!(events.len(), 1);
        let obj = events[0].as_object().expect("object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["event", "pid", "ts_ms"],
            "an unattributed record must gain no keys; got {obj:?}"
        );
    }

    // The `event` field stays a stable identifier -- soldr#1808 asks for typed
    // details specifically so circumstances do not get concatenated into the
    // event name. Assert both halves: the name is untouched, and the detail
    // rides alongside it.
    #[test]
    fn details_travel_beside_the_event_name_not_inside_it() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));

        append_lifecycle_event_with(
            &paths,
            "displace-kill-fallback",
            LifecycleDetails::forced(4321, LifecycleReason::ProtocolMismatch),
        );

        let events = read_events(&paths);
        assert_eq!(events[0]["event"], "displace-kill-fallback");
        assert_eq!(events[0]["target_pid"], 4321);
        assert_eq!(events[0]["reason"], "protocol-mismatch");
        assert_eq!(events[0]["outcome"], "forced");
    }

    // Both `displace-kill-fallback` sites knew the victim PID and dropped it
    // (soldr#1808: "Both `displace-kill-fallback` call sites omit the victim
    // PID and reason"). A record naming neither is indistinguishable from any
    // other kill, which is what made these unattributable in the first place.
    #[test]
    fn a_forced_record_always_names_its_victim_and_reason() {
        let details = LifecycleDetails::forced(99, LifecycleReason::StartupDeadline);
        assert_eq!(details.target_pid, Some(99));
        assert_eq!(details.reason, Some(LifecycleReason::StartupDeadline));
        assert_eq!(details.outcome, Some(LifecycleOutcome::Forced));
    }

    // soldr#1808 WS3. Two `displace-kill-fallback` records are otherwise
    // identical whether a build's own preflight cleared a stale daemon or an
    // operator ran `soldr daemon stop` -- different causes, different
    // remedies. The source is what separates them.
    #[test]
    fn the_source_distinguishes_otherwise_identical_records() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));

        let base = LifecycleDetails::forced(7, LifecycleReason::StaleVersion);
        append_lifecycle_event_with(
            &paths,
            "displace-kill-fallback",
            base.clone().from_source(Some(LifecycleSource::Preflight)),
        );
        append_lifecycle_event_with(
            &paths,
            "displace-kill-fallback",
            base.from_source(Some(LifecycleSource::Cli)),
        );

        let events = read_events(&paths);
        assert_eq!(events[0]["requester_source"], "preflight");
        assert_eq!(events[1]["requester_source"], "cli");
        assert_eq!(events[0]["event"], events[1]["event"]);
    }

    // An unattributed record must still omit the key, not emit null -- the
    // byte-identical guarantee for detail-free records covers this field too.
    #[test]
    fn an_unattributed_record_omits_the_source() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        append_lifecycle_event_with(
            &paths,
            "displace-kill-fallback",
            LifecycleDetails::forced(7, LifecycleReason::StaleVersion),
        );
        let obj = read_events(&paths)[0].as_object().expect("object").clone();
        assert!(
            !obj.contains_key("requester_source"),
            "absent source must be omitted, not null; got {obj:?}"
        );
    }

    #[test]
    fn graceful_shutdown_records_the_observed_peer_before_ack() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let peer = crate::daemon::ipc_peer::PeerIdentity {
            pid: Some(1234),
            exe: Some(r"C:\redacted\soldr.exe".into()),
            source: LifecycleSource::IpcPeer,
        };
        append_lifecycle_event_with(
            &paths,
            "shutdown-requested",
            LifecycleDetails::requested(LifecycleReason::ExplicitStop)
                .for_target_generation(4321, 9876)
                .with_peer(peer),
        );

        let event = &read_events(&paths)[0];
        assert_eq!(event["requester_pid"], 1234);
        assert_eq!(event["requester_exe"], r"C:\redacted\soldr.exe");
        assert_eq!(event["requester_source"], "ipc-peer");
        assert_eq!(event["target_pid"], 4321);
        assert_eq!(event["target_generation"], 9876);
        assert_eq!(event["reason"], "explicit-stop");
        assert_eq!(event["outcome"], "requested");
    }

    #[test]
    fn unknown_transport_identity_never_invents_pid_or_exe() {
        let details = LifecycleDetails::requested(LifecycleReason::ExplicitStop)
            .with_peer(crate::daemon::ipc_peer::PeerIdentity::unknown());
        assert_eq!(details.requester_source, Some(LifecycleSource::Unknown));
        assert_eq!(details.requester_pid, None);
        assert_eq!(details.requester_exe, None);
    }

    #[test]
    fn vanished_without_ack_is_a_distinct_observed_outcome() {
        let details =
            LifecycleDetails::vanished_without_ack(Some(55), LifecycleReason::ProtocolMismatch);
        assert_eq!(details.target_pid, Some(55));
        assert_eq!(details.outcome, Some(LifecycleOutcome::VanishedWithoutAck));
        assert_ne!(details.outcome, Some(LifecycleOutcome::Forced));
    }

    // A requested transition has no victim yet -- the field must be absent
    // rather than serialized as null, or readers cannot tell "not applicable"
    // from "we failed to record it".
    #[test]
    fn a_requested_record_omits_the_target_rather_than_nulling_it() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));

        append_lifecycle_event_with(
            &paths,
            "displace-stale-requested",
            LifecycleDetails::requested(LifecycleReason::StaleVersion),
        );

        let obj = read_events(&paths)[0].as_object().expect("object").clone();
        assert!(
            !obj.contains_key("target_pid"),
            "absent detail must be omitted, not null; got {obj:?}"
        );
        assert_eq!(obj["reason"], "stale-version");
        assert_eq!(obj["outcome"], "requested");
    }
}

#[cfg(test)]
mod status_retry_tests {
    use crate::core::SoldrPaths;
    use crate::daemon::client::ClientError;
    use crate::daemon::lifecycle::{
        append_lifecycle_event_with, latest_shutdown_phase, status_with_retiring_retry,
        LifecycleDetails,
    };
    use crate::daemon::protocol::StatusInfo;
    use std::io::{Error, ErrorKind};
    use std::time::Duration;

    fn status(pid: u32) -> StatusInfo {
        StatusInfo {
            version: crate::daemon::protocol::PROTOCOL_VERSION,
            pid,
            generation: 7,
            uptime_secs: 1,
            request_count: 1,
            cook_stats: None,
            compile_backend: crate::daemon::protocol::COMPILE_BACKEND_EMBEDDED.to_string(),
            compile_jobs: 1,
            compile_jobs_source: "test".to_string(),
            in_flight_compiles: 0,
        }
    }

    #[test]
    fn verified_closing_pipe_is_retried_until_the_new_generation_answers() {
        let mut attempts = 0;
        let result = status_with_retiring_retry(
            || {
                attempts += 1;
                if attempts == 1 {
                    Err(ClientError::Io(Error::new(
                        ErrorKind::BrokenPipe,
                        "the pipe is being closed",
                    )))
                } else if attempts == 2 {
                    Err(ClientError::Io(Error::new(
                        ErrorKind::WouldBlock,
                        "the route is not accepting yet",
                    )))
                } else {
                    Ok(status(42))
                }
            },
            || Some((42, 7)),
            Duration::from_secs(1),
            true,
        )
        .expect("replacement generation should become ready");
        assert_eq!(result.pid, 42);
        assert_eq!(attempts, 3);
    }

    #[test]
    fn unverified_or_unrelated_failures_are_not_retried() {
        let mut attempts = 0;
        let result = status_with_retiring_retry(
            || {
                attempts += 1;
                Err(ClientError::Io(Error::new(
                    ErrorKind::BrokenPipe,
                    "unverified pipe",
                )))
            },
            || None,
            Duration::from_secs(1),
            false,
        );
        assert!(result.is_err());
        assert_eq!(attempts, 1);

        let mut attempts = 0;
        let result = status_with_retiring_retry(
            || {
                attempts += 1;
                Err(ClientError::Io(Error::new(
                    ErrorKind::PermissionDenied,
                    "real failure",
                )))
            },
            || Some((42, 7)),
            Duration::from_secs(1),
            false,
        );
        assert!(result.is_err());
        assert_eq!(attempts, 1);

        let result =
            status_with_retiring_retry(|| Ok(status(42)), || None, Duration::from_secs(1), true);
        assert!(
            result.is_err(),
            "negotiated readiness requires identity evidence"
        );
    }

    // soldr#3126. The negotiated path is entered only after the broker has
    // told the caller the route is published, and the daemon writes its route
    // claim part-way through `run_server` -- after hashing its own image and
    // binding the session listener. So the first probes legitimately land in a
    // window where the claim is not readable yet and `client::status` returns
    // its 2 s reply deadline as `WouldBlock`. Gating the retry on
    // `expected.is_some()` made that first probe terminal and burned none of
    // the 30 s budget.
    #[test]
    fn a_negotiated_start_retries_before_the_route_claim_is_readable() {
        let mut attempts = 0;
        let mut identity_reads = 0;
        let result = status_with_retiring_retry(
            || {
                attempts += 1;
                if attempts <= 3 {
                    Err(ClientError::Io(Error::new(
                        ErrorKind::WouldBlock,
                        "the daemon accepted but has not replied yet",
                    )))
                } else {
                    Ok(status(42))
                }
            },
            || {
                identity_reads += 1;
                // The claim only becomes readable once the daemon gets far
                // enough through start-up to publish it.
                (identity_reads > 3).then_some((42, 7))
            },
            Duration::from_secs(5),
            true,
        )
        .expect("the budget must cover a daemon that is still publishing its claim");
        assert_eq!(result.pid, 42);
        assert!(
            attempts > 1,
            "the first pre-claim WouldBlock must not be terminal; attempts={attempts}"
        );
    }

    // The same platform deadline surfaces as `TimedOut` rather than
    // `WouldBlock` on some transports (`client::is_deadline_error` treats both
    // as deadline expiry), so both spellings must be retried.
    #[test]
    fn a_negotiated_start_treats_timed_out_as_a_not_yet_answer() {
        let mut attempts = 0;
        let result = status_with_retiring_retry(
            || {
                attempts += 1;
                if attempts == 1 {
                    Err(ClientError::Io(Error::new(
                        ErrorKind::TimedOut,
                        "named pipe read deadline",
                    )))
                } else {
                    Ok(status(42))
                }
            },
            || Some((42, 7)),
            Duration::from_secs(5),
            true,
        )
        .expect("a pipe read deadline is not a terminal transport fault");
        assert_eq!(result.pid, 42);
        assert_eq!(attempts, 2);
    }

    // The budget is the contract: a negotiated start that never sees a claim
    // must still spend the whole timeout before reporting failure, otherwise
    // the caller-visible wait is the 2 s reply deadline rather than the 30 s
    // readiness budget it asked for.
    #[test]
    fn a_negotiated_start_spends_its_whole_budget_before_failing() {
        let timeout = Duration::from_millis(200);
        let started = std::time::Instant::now();
        let mut attempts = 0;
        let result = status_with_retiring_retry(
            || {
                attempts += 1;
                Err(ClientError::Io(Error::new(
                    ErrorKind::WouldBlock,
                    "never becomes ready",
                )))
            },
            || None,
            timeout,
            true,
        );
        let elapsed = started.elapsed();
        assert!(
            result.is_err(),
            "it must still fail once the budget is gone"
        );
        assert!(
            elapsed >= timeout,
            "must not give up early: waited {elapsed:?} of {timeout:?}"
        );
        assert!(
            attempts > 1,
            "the budget must be spent on retries; attempts={attempts}"
        );
    }

    #[test]
    fn shutdown_diagnostic_reports_the_latest_phase_for_the_responder() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let pid = std::process::id();
        append_lifecycle_event_with(
            &paths,
            "shutdown-phase-maintenance",
            LifecycleDetails::default().for_target_route(pid, 6, "route-old"),
        );
        append_lifecycle_event_with(
            &paths,
            "shutdown-phase-compile-service",
            LifecycleDetails::default().for_target_route(pid, 7, "route-wrong"),
        );
        append_lifecycle_event_with(
            &paths,
            "shutdown-phase-event-batcher",
            LifecycleDetails::default().for_target_route(pid, 7, "route-current"),
        );
        assert_eq!(
            latest_shutdown_phase(&paths, pid, 7, Some("route-current")).as_deref(),
            Some("event-batcher")
        );
        assert_eq!(
            latest_shutdown_phase(&paths, pid, 6, Some("route-old")).as_deref(),
            Some("maintenance")
        );
        assert_eq!(
            latest_shutdown_phase(&paths, u32::MAX, 7, Some("route-current")),
            None
        );
    }
}

#[cfg(test)]
mod root_ownership_diagnostic_tests {
    use super::lifecycle_event_tests::write_route_claim;
    use crate::core::SoldrPaths;
    use crate::daemon::lifecycle::*;
    use tempfile::TempDir;

    fn write_legacy_pid_file(paths: &SoldrPaths, pid: u32, exe_path: &std::path::Path) {
        let daemon_dir = crate::cache_lib::soldr_daemon_dir(paths);
        std::fs::create_dir_all(&daemon_dir).expect("daemon dir");
        std::fs::write(
            daemon_dir.join("daemon.pid"),
            format!("{pid}\n{}\n", exe_path.display()),
        )
        .expect("legacy daemon pid file");
    }

    #[test]
    fn legacy_pid_identity_bridges_an_in_place_upgrade() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let exe = temp.path().join("runtime-v0.8.29").join("soldr-daemon");
        write_legacy_pid_file(&paths, 4242, &exe);

        assert_eq!(
            read_recorded_daemon_identity(&paths),
            Some((4242, exe)),
            "an upgraded broker must still identify the daemon holding the root lock"
        );
    }

    #[test]
    fn a_route_claim_wins_over_a_stale_legacy_pid_file() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let legacy_exe = temp.path().join("old").join("soldr-daemon");
        write_legacy_pid_file(&paths, 111, &legacy_exe);
        let route_exe = temp.path().join("current").join("soldr-daemon");
        write_route_claim(&paths, 222, &route_exe);

        assert_eq!(
            read_recorded_daemon_identity(&paths),
            Some((222, route_exe)),
            "the compatibility record must never override a route claim"
        );
    }

    #[test]
    fn a_dead_route_record_does_not_mask_a_verified_legacy_daemon() {
        let route = Some((111, std::path::PathBuf::from("/dead/soldr-daemon")));
        let legacy = Some((222, std::path::PathBuf::from("/live/soldr-daemon")));

        assert_eq!(
            select_recorded_daemon_identity(route, false, legacy.clone(), true),
            legacy,
            "the live legacy owner must win over a dead route left by a downgrade"
        );
        assert!(should_use_legacy_endpoint(false, true));
        assert!(!should_use_legacy_endpoint(true, true));
    }

    #[test]
    fn legacy_discovery_accepts_historical_daemon_image_stems() {
        for stem in ["soldr-daemon", "soldr", "rustc"] {
            assert!(legacy_executable_stem_is_supported(
                &std::path::PathBuf::from(stem)
            ));
        }
        assert!(!legacy_executable_stem_is_supported(
            &std::path::PathBuf::from("unrelated")
        ));
    }

    #[test]
    fn the_current_process_is_never_verified_from_a_legacy_pid_file() {
        let current_exe = std::env::current_exe().expect("current test executable");
        assert!(!legacy_daemon_identity_is_verified(&(
            std::process::id(),
            current_exe
        )));
    }

    #[test]
    fn a_malformed_legacy_pid_file_is_not_identity_evidence() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let daemon_dir = crate::cache_lib::soldr_daemon_dir(&paths);
        std::fs::create_dir_all(&daemon_dir).expect("daemon dir");
        std::fs::write(daemon_dir.join("daemon.pid"), "not-a-pid\n/soldr-daemon\n")
            .expect("malformed legacy daemon pid file");

        assert_eq!(read_recorded_daemon_identity(&paths), None);
    }

    // soldr#1987. The orphan case is the one that cost 28 hours: a live PID
    // whose image was deleted holds the lock forever, `soldr daemon stop`
    // cannot see it, and the only symptom is `compiler cache unavailable`.
    // The message has to name the PID and say the image is gone, or the user
    // has nothing to act on.
    #[test]
    fn a_live_owner_with_a_deleted_image_is_named_as_recoverable() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        // This process is alive by construction; point the record at an image
        // that does not exist, which is exactly the orphan's shape.
        let me = std::process::id();
        let missing = temp.path().join("deleted-by-uv").join("soldr-daemon.exe");
        write_route_claim(&paths, me, &missing);

        let msg = describe_root_ownership_conflict(&paths);
        assert!(msg.contains(&me.to_string()), "must name the PID: {msg}");
        assert!(
            msg.contains("no longer exists"),
            "must say the image is gone: {msg}"
        );
        assert!(
            msg.contains("soldr#1987"),
            "must point at the issue explaining why stop cannot reach it: {msg}"
        );
    }

    // A healthy owner is a normal single-instance conflict, not an orphan --
    // it must not be described as recoverable-by-killing.
    #[test]
    fn a_live_owner_with_a_real_image_is_not_called_an_orphan() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let me = std::process::id();
        let real = std::env::current_exe().expect("current exe");
        write_route_claim(&paths, me, &real);

        let msg = describe_root_ownership_conflict(&paths);
        assert!(msg.contains(&me.to_string()), "{msg}");
        assert!(
            !msg.contains("no longer exists"),
            "a present image must not be reported as missing: {msg}"
        );
    }

    // soldr#2316 / #3581: recorded owner is dead but acquisition still failed,
    // so an unrecorded process (an orphaned soldr-daemon) holds the lock.
    // The message now enumerates actual lock holders via /proc/*/fd scan
    // instead of asking the operator to find them, and never recommends
    // blind `pkill -f soldr-daemon` which would kill sibling routes.
    #[test]
    fn a_dead_recorded_owner_points_at_the_orphan_remediation() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        // A large positive PID that is almost certainly not running. Not
        // `u32::MAX` (casts to the `-1` "all processes" wildcard on unix and
        // would spuriously look alive) and not `0` (the process-group
        // wildcard) — matches the `i32::MAX as u32` idiom used elsewhere in
        // these lifecycle tests.
        let dead = i32::MAX as u32;
        let some_image = temp.path().join("soldr-daemon.exe");
        write_route_claim(&paths, dead, &some_image);

        let msg = describe_root_ownership_conflict(&paths);
        assert!(
            msg.contains("soldr#2316"),
            "must point at the orphan-holder issue: {msg}"
        );
        assert!(msg.contains("root-owner.lock"), "{msg}");
        // New message enumerates holders (or explains none found) instead of
        // the old "terminate only that process" guidance.
        assert!(msg.contains("no live process currently holds"), "{msg}");
        assert!(msg.contains("permission denied"), "{msg}");
        assert!(msg.contains("SOLDR_CACHE_DIR"), "{msg}");
        assert!(msg.contains("soldr status"), "{msg}");
        assert!(msg.contains("soldr logs paths"), "{msg}");
        assert!(
            !msg.contains("pkill"),
            "must preserve sibling routes: {msg}"
        );
        assert!(
            !msg.contains("Stop-Process"),
            "must preserve sibling routes: {msg}"
        );
    }

    // No route claim at all must still produce something better than silence.
    #[test]
    fn a_missing_route_claim_says_so_rather_than_naming_nobody() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let msg = describe_root_ownership_conflict(&paths);
        assert!(msg.contains("no daemon route claim"), "{msg}");
        // soldr#3456: and it must say how to get unstuck.
        for remedy in ["SOLDR_CACHE_DIR", "root-owner.lock", "soldr status"] {
            assert!(msg.contains(remedy), "missing {remedy}: {msg}");
        }
    }
}

#[cfg(test)]
mod missing_image_detector_tests {
    use crate::daemon::lifecycle::*;

    // The safety property, and the reason for hysteresis at all: a daemon that
    // exits on one bad reading is worse than the orphan it prevents, because
    // it takes down healthy daemons for transient reasons.
    #[test]
    fn a_single_missing_reading_never_triggers() {
        let mut d = MissingImageDetector::default();
        assert!(!d.observe(Some(false)));
        assert_eq!(d.strikes(), 1);
    }

    #[test]
    fn it_triggers_exactly_once_on_the_confirming_strike() {
        let mut d = MissingImageDetector::default();
        let fired: Vec<bool> = (0..5).map(|_| d.observe(Some(false))).collect();
        assert_eq!(
            fired,
            vec![false, false, true, false, false],
            "must fire on strike {DAEMON_IMAGE_MISSING_STRIKES} and not again -- \
             a repeating trigger would re-request shutdown every tick"
        );
    }

    // One sighting proves the condition was not the permanent one.
    #[test]
    fn any_sighting_resets_the_count() {
        let mut d = MissingImageDetector::default();
        d.observe(Some(false));
        d.observe(Some(false));
        assert_eq!(d.strikes(), 2, "primed one strike from firing");
        assert!(!d.observe(Some(true)));
        assert_eq!(d.strikes(), 0);
        assert!(!d.observe(Some(false)), "count must restart, not resume");
    }

    // Not knowing where our image is says nothing about whether it exists, so
    // it must not accumulate toward self-termination.
    #[test]
    fn an_unknown_path_resets_rather_than_accumulating() {
        let mut d = MissingImageDetector::default();
        d.observe(Some(false));
        d.observe(Some(false));
        assert!(
            !d.observe(None),
            "unknown must not be the confirming strike"
        );
        assert_eq!(d.strikes(), 0);
    }

    // The live probe must agree with reality for the running test binary,
    // which exists by construction.
    #[test]
    fn the_live_probe_sees_this_running_executable() {
        assert_eq!(daemon_image_present(), Some(true));
    }
}
