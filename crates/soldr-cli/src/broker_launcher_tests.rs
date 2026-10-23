use super::*;
use running_process::broker::protocol::ServiceDefinition;
use running_process::broker::protocol_v2::ServiceDefinitionBuilder;
use running_process::broker::server::service_definition_v2_to_v1;
use running_process::broker::server::{BackendKey, BrokerInstanceKey, TraceContext};

fn request_parts(
    root: &std::path::Path,
    binary: &std::path::Path,
    image_hash: &str,
) -> (ServiceDefinition, BackendKey, TraceContext) {
    let mut definition = service_definition_v2_to_v1(
        ServiceDefinitionBuilder::shared_broker("soldr-daemon-test", binary.display().to_string())
            .per_version_binary_dir(
                binary
                    .parent()
                    .expect("binary parent")
                    .display()
                    .to_string(),
            )
            .min_version("1.0.0")
            .version_allow_list(["1.0.0"])
            .build(),
    );
    definition.labels.insert(
        crate::daemon::service_definition::SOLDR_ROOT_SERVICE_LABEL.into(),
        root.display().to_string(),
    );
    definition.labels.insert(
        crate::daemon::backend_handle_adoption::SOLDR_DAEMON_IMAGE_HASH_LABEL.into(),
        image_hash.into(),
    );
    let key = BackendKey::new(BrokerInstanceKey::Shared, "soldr-daemon-test", "1.0.0", "");
    (definition, key, TraceContext::default())
}

// soldr#3684: a probe timeout is inconclusive and must not prune a live
// daemon's route claim; a definitive failure still prunes it.
fn claim_probe_error(
    inner: running_process::broker::backend_lifecycle::probe::EndpointProbeError,
) -> crate::daemon::backend_handle_adoption::ClaimAdoptionError {
    crate::daemon::backend_handle_adoption::ClaimAdoptionError::Probe(
        running_process::broker::backend_handle::BackendHandleError::Probe(
            running_process::broker::backend_lifecycle::probe::ProbeError::EndpointResponse(inner),
        ),
    )
}

fn seeded_claim(temp: &tempfile::TempDir) -> (crate::core::SoldrPaths, PathBuf) {
    let paths = crate::core::SoldrPaths::with_root(temp.path().join("root"));
    let claim = crate::daemon::backend_handle_adoption::broker_route_claim_path(&paths);
    std::fs::create_dir_all(claim.parent().expect("claim parent")).expect("claim dir");
    std::fs::write(&claim, b"claim").expect("write claim");
    (paths, claim)
}

#[test]
fn probe_timeout_keeps_route_claim() {
    use running_process::broker::backend_lifecycle::probe::EndpointProbeError;
    let temp = tempfile::tempdir().expect("tempdir");
    let (paths, claim) = seeded_claim(&temp);
    let pruned = prune_route_claim_after_probe_failure(
        &paths,
        b"claim",
        &claim_probe_error(EndpointProbeError::Timeout),
        true,
    );
    assert!(!pruned);
    assert!(claim.exists(), "a probe timeout must not prune the claim");
}

#[test]
fn definitive_probe_failure_prunes_route_claim() {
    use running_process::broker::backend_lifecycle::probe::EndpointProbeError;
    let temp = tempfile::tempdir().expect("tempdir");
    let (paths, claim) = seeded_claim(&temp);
    let refused =
        EndpointProbeError::Connect(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
    assert!(prune_route_claim_after_probe_failure(
        &paths,
        b"claim",
        &claim_probe_error(refused),
        true
    ));
    assert!(!claim.exists(), "a definitive failure must prune the claim");
}

#[test]
fn changed_registered_image_is_rejected_before_spawn() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = temp.path().join("soldr-daemon");
    std::fs::write(&binary, b"changed image").expect("write image");
    let root = temp.path().join("root");
    let (definition, key, trace_context) = request_parts(&root, &binary, &"0".repeat(64));
    let request = BackendLaunchRequest {
        key: &key,
        service_definition: &definition,
        trace_context: &trace_context,
        session_token: None,
    };

    let err = match SoldrBackendLauncher::new().launch(&request) {
        Ok(_) => panic!("stale image registration must fail"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("changed before broker launch"),
        "{err}"
    );
}

#[test]
fn daemon_launch_failure_surfaces_the_startup_log() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("daemon-spawn.log");
    std::fs::write(
        &log,
        "old launch\n\nsoldr-daemon failed: Io(Kind(Unsupported))\n",
    )
    .expect("write daemon log");
    let error = std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "broker-launched soldr-daemon exited before readiness (1)",
    );

    let message = daemon_launch_failure(&error, &log);

    assert!(message.contains("exited before readiness (1)"), "{message}");
    assert!(
        message.contains("soldr-daemon failed: Io(Kind(Unsupported))"),
        "{message}"
    );
}

#[test]
fn foreign_live_route_claim_survives_replacement_preflight() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("root");
    let paths = crate::core::SoldrPaths::with_root(root.clone());
    let endpoint = running_process::broker::protocol::Endpoint::unix_socket(
        "incumbent-test",
        temp.path().join("incumbent.sock").display().to_string(),
    )
    .or_else(|_| {
        running_process::broker::protocol::Endpoint::windows_pipe(
            "incumbent-test",
            "soldr-incumbent-test",
        )
    })
    .expect("incumbent endpoint");
    let incumbent =
        running_process::broker::backend_handle::DaemonProcess::current_process(endpoint, None)
            .expect("incumbent claim");
    crate::daemon::backend_handle_adoption::publish_broker_route_claim(&paths, &incumbent)
        .expect("publish incumbent claim");

    let replacement = temp.path().join("replacement").join("soldr-daemon");
    std::fs::create_dir_all(replacement.parent().expect("replacement parent"))
        .expect("replacement dir");
    std::fs::write(&replacement, b"replacement image").expect("replacement image");
    let (definition, key, trace_context) = request_parts(&root, &replacement, &"0".repeat(64));
    let request = BackendLaunchRequest {
        key: &key,
        service_definition: &definition,
        trace_context: &trace_context,
        session_token: None,
    };

    assert!(
        SoldrBackendLauncher::new()
            .adopt_route_claim(&request, &paths, true)
            .is_none(),
        "a foreign image claim must not be adopted"
    );
    assert!(
        crate::daemon::backend_handle_adoption::broker_route_claim_path(&paths).exists(),
        "replacement preflight still needs the incumbent PID/image claim"
    );
}

/// soldr#3057: an exited daemon child must be reaped, not left a zombie
/// under the broker that spawned it.
#[test]
fn an_exited_daemon_child_is_reaped_rather_than_left_a_zombie() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = crate::broker_spawn::open_append(&temp.path().join("daemon-spawn.log"))
        .expect("daemon log");
    // The test binary itself is a portable short-lived child: `--list`
    // prints the test names and exits.
    let mut command = std::process::Command::new(std::env::current_exe().expect("test exe"));
    command.arg("--list");
    let child = running_process::spawn_daemon_with_stdio_and_env_policy(
        &mut command,
        crate::broker_spawn::daemon_stdio(&log),
        running_process::EnvironmentPolicy::UserBaseline,
    )
    .expect("spawn short-lived daemon child");
    let pid = child.id();
    let reaper = reap_on_exit(child).expect("reaper thread");

    // A zombie is not alive, so this loop ends once the child has exited
    // whether or not anything reaped it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while crate::platform::process::inspect::is_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "child {pid} did not exit"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    reaper.join().expect("reaper thread joined");

    assert!(
        !crate::platform::process::inspect::is_zombie(pid),
        "daemon child {pid} exited but was never reaped"
    );
}
