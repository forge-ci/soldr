//! soldr#3374: two soldr versions ("generations") used on one machine must
//! not displace or overwrite each other's daemon state. The broker already
//! keeps one route per daemon image; these tests pin the per-root state
//! (route claim / occupancy) to the same per-generation partitioning so a
//! different-version preflight has nothing of the other generation's to act
//! on in the first place.

#[cfg(test)]
mod generation_tests {
    use crate::core::SoldrPaths;
    use crate::daemon::backend_handle_adoption::{
        broker_route_claim_path, publish_broker_route_claim, read_broker_route_claim,
    };
    use crate::daemon::lifecycle::{
        claimed_daemon_occupies_route, pid_exe_stem_matches, pid_is_alive,
        preflight_should_displace,
    };
    use running_process::broker::backend_handle::DaemonProcess;
    use running_process::broker::protocol::Endpoint;
    use tempfile::TempDir;

    use crate::daemon::backend_handle_adoption::set_generation_override;

    /// Selects this thread's daemon generation (the seam behind
    /// `SOLDR_BROKER_SERVICE`) and clears it on drop.
    struct ServiceEnv;

    impl ServiceEnv {
        fn set(service: &str) -> Self {
            set_generation_override(Some(service.to_string()));
            Self
        }
        fn unset() -> Self {
            set_generation_override(None);
            Self
        }
    }

    impl Drop for ServiceEnv {
        fn drop(&mut self) {
            set_generation_override(None);
        }
    }

    fn claim_for(pid: u32, exe_path: &std::path::Path) -> DaemonProcess {
        let endpoint = if crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
        {
            Endpoint::windows_pipe(exe_path.display().to_string(), "soldr-gen-test")
        } else {
            Endpoint::unix_socket(
                exe_path.display().to_string(),
                exe_path
                    .parent()
                    .unwrap_or(exe_path)
                    .join("gen-test.sock")
                    .display()
                    .to_string(),
            )
        }
        .expect("test endpoint");
        DaemonProcess {
            pid,
            exe_hash: [0; 32],
            legacy_exe_sha256: [0; 32],
            exe_path: exe_path.to_path_buf(),
            boot_id: "gen-test-boot".to_string(),
            ipc_endpoint: endpoint,
            started_at_unix_ms: 0,
            idle_timeout_secs: None,
        }
    }

    /// Spawn a long-lived child process whose image is literally named
    /// `soldr-daemon` (or `soldr` on Windows), the only two stems
    /// `pid_is_soldr_daemon` accepts, by copying a real, already-executable
    /// binary to that name and running it. This lets a unit test produce a
    /// PID that clears the same identity gate a real daemon would, without
    /// spawning a real soldr-daemon.
    const FAKE_DAEMON_SLEEPER: &str =
        "daemon::lifecycle::tests::generations::generation_tests::fake_daemon_sleeper";
    const FAKE_DAEMON_SLEEP_ENV: &str = "SOLDR_TEST_FAKE_DAEMON_SLEEP_SECS";

    /// Body of the fake daemon spawned by `spawn_fake_daemon`: holds the
    /// process alive for the requested seconds. Without the variable it is
    /// an ordinary test that returns at once.
    #[test]
    fn fake_daemon_sleeper() {
        if let Some(secs) = std::env::var(FAKE_DAEMON_SLEEP_ENV)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        {
            std::thread::sleep(std::time::Duration::from_secs(secs));
        }
    }

    fn spawn_fake_daemon(
        dir: &std::path::Path,
        name: &str,
    ) -> (running_process::SpawnedChild, std::path::PathBuf) {
        // The image is this test binary itself, re-entered on
        // `fake_daemon_sleeper`. A host `sleep` is not a portable source: it
        // is absent from `/bin` on NixOS, and where it exists it may be a
        // multicall binary (coreutils, busybox) that dispatches on argv[0]
        // and refuses to run once renamed to `soldr-daemon`.
        let source = std::env::current_exe().expect("current test executable");
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let unique = dir.join(format!(
            "image-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&unique).expect("fake daemon dir");
        let target = unique.join(name);
        std::fs::copy(&source, &target).expect("copy fake daemon image");
        // `fs::copy` carries the source's executable mode bits across.
        // Another test thread forking while our copy's write fd was open
        // yields ETXTBSY; that fd closes as soon as the fork execs.
        for _ in 0..50 {
            let mut command = std::process::Command::new(&target);
            command
                .args([
                    "--exact",
                    FAKE_DAEMON_SLEEPER,
                    "--test-threads=1",
                    "--quiet",
                ])
                .env(FAKE_DAEMON_SLEEP_ENV, "30");
            let stdio = running_process::SpawnStdio {
                stdin: running_process::StdioSource::Null,
                stdout: running_process::StdioSource::Parent,
                stderr: running_process::StdioSource::Parent,
                drain_timeout: None,
                show_console: false,
            };
            match running_process::spawn(&mut command, stdio) {
                Ok(child) => return (child, target),
                Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => panic!("spawn fake daemon: {error}"),
            }
        }
        panic!("spawn fake daemon: executable stayed busy")
    }

    fn daemon_stem() -> &'static str {
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            "soldr"
        } else {
            crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_NAME
        }
    }

    /// Acceptance test 2 (soldr#3374): each generation's route-claim path is
    /// distinct, keyed by its resolved `SOLDR_BROKER_SERVICE`, and a front
    /// door resolves only its own.
    #[test]
    fn each_generation_resolves_its_own_claim_path() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));

        let path_a = {
            let _env = ServiceEnv::set("soldr-daemon-generation-a");
            broker_route_claim_path(&paths)
        };
        let path_b = {
            let _env = ServiceEnv::set("soldr-daemon-generation-b");
            broker_route_claim_path(&paths)
        };
        let path_unkeyed = broker_route_claim_path(&paths);

        assert_ne!(
            path_a, path_b,
            "distinct generations must not share a claim path"
        );
        assert_ne!(path_a, path_unkeyed);
        assert_ne!(path_b, path_unkeyed);
    }

    /// Acceptance test 1 (soldr#3374), policy half: with per-generation
    /// claim state, a preflight running as generation B sees no occupancy
    /// evidence at all for generation A's live daemon, so
    /// `preflight_should_displace` never fires against it -- regardless of
    /// how many times it is asked. Repeated 5x from each side per the
    /// issue's "N preflights from each side".
    #[test]
    fn cross_generation_preflight_never_sees_the_other_generation_occupying() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let (child_a, exe_a) = spawn_fake_daemon(&bin_dir, daemon_stem());

        {
            let _env = ServiceEnv::set("soldr-daemon-generation-a");
            publish_broker_route_claim(&paths, &claim_for(child_a.id(), &exe_a))
                .expect("publish generation A claim");
        }

        // Sanity: generation A's own preflight sees its daemon occupying the
        // endpoint (the identity gate this whole scheme relies on).
        {
            let _env = ServiceEnv::set("soldr-daemon-generation-a");
            assert!(
                claimed_daemon_occupies_route(&paths).is_some(),
                "generation A must see its own daemon"
            );
        }

        // From generation B's perspective, repeated preflights must see
        // nothing to displace: no claim, no occupancy, no endpoint artifact.
        for attempt in 0..5 {
            let _env = ServiceEnv::set("soldr-daemon-generation-b");
            assert!(
                claimed_daemon_occupies_route(&paths).is_none(),
                "attempt {attempt}: generation B must not see generation A's daemon"
            );
            assert!(
                !broker_route_claim_path(&paths).exists(),
                "attempt {attempt}: generation B's own claim slot must be empty"
            );
            let should_displace = preflight_should_displace(
                false, // claim_proves_current: no claim at all for B
                false, // stale_daemon_occupies (computed above)
                false, // recorded_process_is_alive (computed above)
                false, // endpoint_artifact_exists (computed above)
            );
            assert!(
                !should_displace,
                "attempt {attempt}: generation B must never decide to displace generation A"
            );
        }

        // And generation A's daemon must still be alive throughout -- no
        // signal was ever sent to it.
        assert!(
            pid_is_alive(child_a.id()),
            "generation A's daemon must survive generation B's preflights"
        );
        assert!(pid_exe_stem_matches(child_a.id(), daemon_stem()));

        let mut child_a = child_a;
        let _ = child_a.kill();
        let _ = child_a.wait();
    }

    /// Acceptance test 3 (soldr#3374): a legacy, version-independent claim
    /// (as an older, unpatched soldr would still write, or state left over
    /// from before this fix) is left exactly alone by a new generation: the
    /// new generation reads and writes only its own keyed slot.
    #[test]
    fn legacy_unkeyed_claim_is_left_alone_by_a_new_generation() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        // Write the legacy (unkeyed) claim exactly as an older soldr would,
        // with no SOLDR_BROKER_SERVICE resolved.
        let (legacy_child, legacy_exe) = spawn_fake_daemon(&bin_dir, daemon_stem());
        let legacy_path = {
            let _env = ServiceEnv::unset();
            publish_broker_route_claim(&paths, &claim_for(legacy_child.id(), &legacy_exe))
                .expect("publish legacy claim");
            broker_route_claim_path(&paths)
        };
        let legacy_bytes_before = std::fs::read(&legacy_path).expect("legacy claim bytes");

        // A new generation resolves and publishes its own claim.
        let (new_child, new_exe) = spawn_fake_daemon(&bin_dir, daemon_stem());
        let new_path = {
            let _env = ServiceEnv::set("soldr-daemon-generation-new");
            publish_broker_route_claim(&paths, &claim_for(new_child.id(), &new_exe))
                .expect("publish new-generation claim");
            broker_route_claim_path(&paths)
        };

        assert_ne!(
            legacy_path, new_path,
            "the new generation must not reuse the legacy slot"
        );
        let legacy_bytes_after = std::fs::read(&legacy_path).expect("legacy claim still present");
        assert_eq!(
            legacy_bytes_before, legacy_bytes_after,
            "the legacy claim must be byte-for-byte untouched by the new generation's publish"
        );

        // The legacy daemon must still be discoverable at its own slot, and
        // still alive -- the new generation neither killed nor overwrote it.
        {
            let _env = ServiceEnv::unset();
            let claim = read_broker_route_claim(&paths)
                .expect("read legacy claim")
                .expect("legacy claim present");
            assert_eq!(claim.pid, legacy_child.id());
        }
        assert!(
            pid_is_alive(legacy_child.id()),
            "legacy daemon must survive untouched"
        );

        let mut legacy_child = legacy_child;
        let mut new_child = new_child;
        let _ = legacy_child.kill();
        let _ = legacy_child.wait();
        let _ = new_child.kill();
        let _ = new_child.wait();
    }

    /// Acceptance test 4 (soldr#3374, the #1866 trade-off): within one
    /// generation, occupancy detection is unchanged -- a same-version daemon
    /// that currently occupies the endpoint is still visible to that
    /// generation's own preflight, so the wedge/recovery ladder still has
    /// something to work with. This guards against the fix over-isolating
    /// generations to the point that same-generation staleness detection
    /// silently breaks.
    #[test]
    fn same_generation_occupancy_detection_is_unaffected() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let (child, exe) = spawn_fake_daemon(&bin_dir, daemon_stem());

        let _env = ServiceEnv::set("soldr-daemon-generation-same");
        publish_broker_route_claim(&paths, &claim_for(child.id(), &exe)).expect("publish claim");

        for attempt in 0..3 {
            assert!(
                claimed_daemon_occupies_route(&paths).is_some(),
                "attempt {attempt}: same generation must still see its own live daemon"
            );
        }

        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
    }

    /// A missing `SOLDR_BROKER_SERVICE` (no resolved generation, e.g. an
    /// env-sanitized invocation) falls back to the pre-#3374 unkeyed path so
    /// existing behavior for that edge case is unchanged.
    #[test]
    fn unset_service_env_falls_back_to_the_legacy_path() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let _env = ServiceEnv::unset();
        let path = broker_route_claim_path(&paths);
        assert_eq!(
            path,
            crate::cache_lib::soldr_daemon_dir(&paths).join("broker-route-claim.pb")
        );
    }

    /// Brokers from before soldr#3374 read only the version-independent slot
    /// and are never replaced automatically. With that slot free, a new
    /// generation mirrors its claim there so such a broker still sees the
    /// daemon it launched; the generation's own reads stay keyed.
    #[test]
    fn free_legacy_slot_is_mirrored_for_pre_generation_brokers() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let (child, exe) = spawn_fake_daemon(&bin_dir, daemon_stem());

        {
            let _env = ServiceEnv::set("soldr-daemon-generation-mirror");
            publish_broker_route_claim(&paths, &claim_for(child.id(), &exe))
                .expect("publish claim");
        }
        let _env = ServiceEnv::unset();
        let legacy = read_broker_route_claim(&paths)
            .expect("read legacy slot")
            .expect("legacy slot mirrored");
        assert_eq!(legacy.pid, child.id());

        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
    }
}
