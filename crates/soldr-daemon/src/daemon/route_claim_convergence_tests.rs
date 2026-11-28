//! soldr#3561: a route request must converge on its generation's claimed
//! daemon even when that daemon listens on an endpoint derived from a
//! different executable path, and must still fail -- bounded -- when the
//! claimed daemon does not authenticate as the route's image.

use super::*;
use crate::daemon::backend_handle_adoption::publish_broker_route_claim;
use crate::daemon::generation_key::set_generation_override;
use tempfile::TempDir;

const GENERATION: &str = "soldr-daemon-convergence-test";
const SHORT: Duration = Duration::from_millis(300);

/// Selects this thread's generation and clears it on drop.
struct Generation;

impl Generation {
    fn set() -> Self {
        set_generation_override(Some(GENERATION.to_string()));
        Self
    }
}

impl Drop for Generation {
    fn drop(&mut self) {
        set_generation_override(None);
    }
}

fn endpoint(temp: &TempDir, exe: &str) -> Endpoint {
    let exe_path = temp.path().join(exe).join("soldr-daemon");
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        Endpoint::windows_pipe(
            exe_path.display().to_string(),
            format!("soldr-convergence-{exe}"),
        )
    } else {
        Endpoint::unix_socket(
            exe_path.display().to_string(),
            temp.path()
                .join(format!("{exe}.session.sock"))
                .display()
                .to_string(),
        )
    }
    .expect("test endpoint")
}

/// A claim for image `[7; 32]` published by a daemon placed under `exe`.
fn claim_at(temp: &TempDir, exe: &str) -> DaemonProcess {
    DaemonProcess {
        pid: 4242,
        exe_hash: [7; 32],
        legacy_exe_sha256: [0; 32],
        exe_path: temp.path().join(exe).join("soldr-daemon"),
        boot_id: "convergence-test-boot".to_string(),
        ipc_endpoint: endpoint(temp, exe),
        started_at_unix_ms: 0,
        idle_timeout_secs: None,
    }
}

fn route_image() -> String {
    kernal_api::hash::Blake3Digest::from_bytes([7_u8; 32]).to_hex()
}

fn paths(temp: &TempDir) -> SoldrPaths {
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    std::fs::create_dir_all(&paths.root).expect("root");
    paths
}

/// The #3561 shape: one image, two placements. The claim names the endpoint
/// of placement A, the waiter expects placement B's. When A authenticates as
/// the route's image the request returns A's daemon instead of spinning on
/// the endpoint mismatch until the acquisition ceiling.
#[test]
fn same_image_at_another_executable_path_converges_on_the_claimed_daemon() {
    let temp = TempDir::new().expect("tempdir");
    let paths = paths(&temp);
    let _generation = Generation::set();
    let claim = claim_at(&temp, "placement-a");
    publish_broker_route_claim(&paths, &claim).expect("publish claim");
    let expected = endpoint(&temp, "placement-b");
    assert_ne!(claim.ipc_endpoint, expected, "test needs two endpoints");

    let started = Instant::now();
    let adopted = wait_for_route_claim_while(
        &paths,
        &expected,
        SHORT,
        || Ok(None),
        |_| {},
        // Stands in for the exact probe succeeding at the claim's endpoint.
        |claim| {
            claim_matches_route_image(claim, &route_image()).map(|()| claim.ipc_endpoint.clone())
        },
    )
    .expect("the generation's claimed daemon must be adopted");
    assert_eq!(adopted, claim.ipc_endpoint);
    assert!(
        started.elapsed() < SHORT,
        "convergence must not wait out the window"
    );
}

/// A claimed daemon that does not answer the exact probe is not adopted; the
/// wait stays bounded and names both the mismatch and the probe failure.
#[test]
fn claimed_daemon_that_fails_its_probe_is_not_adopted() {
    let temp = TempDir::new().expect("tempdir");
    let paths = paths(&temp);
    let _generation = Generation::set();
    publish_broker_route_claim(&paths, &claim_at(&temp, "placement-a")).expect("publish claim");
    let expected = endpoint(&temp, "placement-b");

    let started = Instant::now();
    let error = wait_for_route_claim_while(
        &paths,
        &expected,
        SHORT,
        || Ok(None),
        |_| {},
        |_| Err::<(), _>("probe refused"),
    )
    .expect_err("an unauthenticated claim must not be adopted");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let message = error.to_string();
    assert!(message.contains("endpoint mismatch"), "{message}");
    assert!(message.contains("probe refused"), "{message}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "wait must stay bounded"
    );
}

/// Through the public waiter: a claim for a different image is never
/// adopted, so a foreign generation's daemon is left alone, and no live
/// daemon is needed to reach that verdict.
#[test]
fn foreign_image_claim_is_never_adopted() {
    let temp = TempDir::new().expect("tempdir");
    let paths = paths(&temp);
    let _generation = Generation::set();
    publish_broker_route_claim(&paths, &claim_at(&temp, "placement-a")).expect("publish claim");

    let error = wait_for_broker_backend_handle_while(
        &paths,
        GENERATION,
        "1.0.0",
        &kernal_api::hash::Blake3Digest::from_bytes([9_u8; 32]).to_hex(),
        &endpoint(&temp, "placement-b"),
        SHORT,
        || Ok(None),
        |_| {},
    )
    .err()
    .expect("a different image must not be adopted");
    let message = error.to_string();
    assert!(message.contains("different image"), "{message}");
    assert!(
        crate::daemon::backend_handle_adoption::broker_route_claim_path(&paths).exists(),
        "a foreign claim is never pruned by the waiter"
    );
}

/// Through the public waiter with the real exact probe: the claim records
/// this route's image, but nothing answers at its endpoint, so it fails the
/// probe and the wait ends bounded.
#[test]
fn matching_image_without_a_live_daemon_fails_the_exact_probe() {
    let temp = TempDir::new().expect("tempdir");
    let paths = paths(&temp);
    let _generation = Generation::set();
    let claim = DaemonProcess::current_process(endpoint(&temp, "placement-a"), None)
        .expect("current process identity");
    let image = kernal_api::hash::Blake3Digest::from_bytes(claim.exe_hash).to_hex();
    publish_broker_route_claim(&paths, &claim).expect("publish claim");

    let error = wait_for_broker_backend_handle_while(
        &paths,
        GENERATION,
        "1.0.0",
        &image,
        &endpoint(&temp, "placement-b"),
        SHORT,
        || Ok(None),
        |_| {},
    )
    .err()
    .expect("no daemon answers at the claimed endpoint");
    let message = error.to_string();
    assert!(message.contains("exact probe"), "{message}");
}

/// The caller's own child exits because the generation's daemon already
/// serves the root. The request converges on that daemon; only when nothing
/// authenticates does the child's exit end the wait.
#[test]
fn child_exit_converges_when_the_generation_daemon_serves() {
    let temp = TempDir::new().expect("tempdir");
    let paths = paths(&temp);
    let _generation = Generation::set();
    let claim = claim_at(&temp, "placement-a");
    publish_broker_route_claim(&paths, &claim).expect("publish claim");
    let expected = endpoint(&temp, "placement-b");

    let adopted = wait_for_route_claim_while(
        &paths,
        &expected,
        SHORT,
        || Ok(Some(1)),
        |_| {},
        |claim| Ok::<_, String>(claim.pid),
    )
    .expect("converges despite the redundant child's exit");
    assert_eq!(adopted, claim.pid);

    let error = wait_for_route_claim_while(
        &paths,
        &expected,
        Duration::from_secs(30),
        || Ok(Some(1)),
        |_| {},
        |_| Err::<u32, _>("probe refused"),
    )
    .expect_err("an exited child with no adoptable daemon fails");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}
