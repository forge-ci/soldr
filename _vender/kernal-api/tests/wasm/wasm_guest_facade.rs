//! Native checks of the semantic adapter, before public guest-package wiring.
//! These tests deliberately perform no host imports and are not Wasm runtime
//! acceptance evidence.

#[allow(dead_code)]
#[path = "../../src/guest.rs"]
mod guest;

#[test]
fn unsupported_pending_future_is_dropped_and_reports_failure() {
    struct PendingCommand(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl std::future::Future for PendingCommand {
        type Output = Result<(), guest::OperationError>;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            std::task::Poll::Pending
        }
    }

    impl Drop for PendingCommand {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert_eq!(
        guest::run(PendingCommand(dropped.clone())),
        Err(guest::OperationError::Failed)
    );
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn nested_kernel_command_preserves_the_semantic_result() {
    assert_eq!(
        guest::run(async {
            let value = guest::run(async { Ok(21) })?;
            Ok(value * 2)
        }),
        Ok(42)
    );
}
