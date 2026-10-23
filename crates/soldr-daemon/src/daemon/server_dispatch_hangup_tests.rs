//! soldr#3839: a read-only control request whose client hangs up must not
//! hold its task and socket until a wedged handler returns.

use super::*;
use std::time::Duration;

fn control_without_state() -> (
    ControlContext,
    tokio::sync::watch::Sender<Option<Arc<State>>>,
) {
    use running_process::broker::protocol::Endpoint;
    let daemon_identity = running_process::broker::backend_handle::DaemonProcess::current_process(
        Endpoint {
            namespace_id: "soldr-hangup-test".to_string(),
            path: "soldr-hangup-test.sock".to_string(),
        },
        None,
    )
    .expect("current test process identity");
    // State is never published, so any request that needs it waits forever,
    // standing in for a wedged handler.
    let (publisher, state) = tokio::sync::watch::channel(None);
    let control = ControlContext::new(
        daemon_identity,
        Arc::new(crate::daemon::maintenance::ShutdownSignal::default()),
        state,
    );
    (control, publisher)
}

async fn serve(request: Request, hang_up: bool) -> bool {
    let (control, _publisher) = control_without_state();
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    crate::daemon::ipc::write_frame_async(&mut client, &request)
        .await
        .expect("write request");
    let task = tokio::spawn(handle_connection(
        server,
        control,
        crate::daemon::ipc_peer::PeerIdentity::unknown(),
    ));
    if hang_up {
        drop(client);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .is_ok()
    } else {
        let done = tokio::time::timeout(Duration::from_millis(300), task)
            .await
            .is_ok();
        drop(client);
        done
    }
}

#[tokio::test]
async fn read_only_request_is_abandoned_when_the_client_hangs_up() {
    assert!(serve(Request::Status, true).await);
    assert!(serve(Request::CompileStats, true).await);
}

#[tokio::test]
async fn read_only_request_keeps_waiting_while_the_client_is_connected() {
    assert!(!serve(Request::Status, false).await);
}

#[test]
fn mutating_and_lease_requests_are_never_cancelled() {
    assert!(request_is_cancel_safe(&Request::Status));
    for request in [
        Request::Shutdown,
        Request::FlushCaches,
        Request::ReleaseResidentCapacity,
        Request::AcquireResidentCapacity { permits: 1 },
    ] {
        assert!(!request_is_cancel_safe(&request));
    }
}

#[tokio::test]
async fn requests_past_the_in_flight_cap_get_a_loud_error() {
    let (mut control, _publisher) = control_without_state();
    control.in_flight = Arc::new(tokio::sync::Semaphore::new(0));
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    crate::daemon::ipc::write_frame_async(&mut client, &Request::Status)
        .await
        .expect("write request");
    let task = tokio::spawn(handle_connection(
        server,
        control,
        crate::daemon::ipc_peer::PeerIdentity::unknown(),
    ));
    let response: Response = tokio::time::timeout(
        Duration::from_secs(5),
        crate::daemon::ipc::read_frame_async(&mut client),
    )
    .await
    .expect("refused without waiting for state")
    .expect("read response");
    match response {
        Response::Error(message) => assert!(message.contains("soldr daemon stop"), "{message}"),
        other => panic!("expected a busy error, got {other:?}"),
    }
    task.await
        .expect("connection task")
        .expect("connection result");
}
