//! SIGTERM wait for the daemon's fast-exit hook (soldr#3059; formerly a
//! graceful-drain hook under issue #1286 — see `platform/process/signal.rs`
//! for the history).

/// Wait for SIGTERM, returning `true` once a terminate signal arrived.
/// `false` when the handler cannot be registered (the caller then runs
/// without the TERM hook — same behavior as a host without signals).
///
/// The handler is installed **when this function is called**, not when the
/// returned future is first polled. The daemon constructs this future
/// before it becomes observably ready but polls it only from a task spawned
/// later; an `async fn` here deferred `signal()` to that first poll, so a
/// SIGTERM in between hit the default disposition and killed the daemon by
/// signal 15 instead of taking the fast-exit path (main run 42359197307).
/// Must be called inside a Tokio runtime.
pub fn wait_for_terminate_signal() -> impl std::future::Future<Output = bool> + Send {
    let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    async move {
        let Ok(mut term) = term else {
            return false;
        };
        term.recv().await.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::wait_for_terminate_signal;

    fn sigterm_disposition_is_default() -> bool {
        // SAFETY: querying (not changing) the disposition with a null new action.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            assert_eq!(libc::sigaction(libc::SIGTERM, std::ptr::null(), &mut old), 0);
            old.sa_sigaction == libc::SIG_DFL
        }
    }

    /// soldr#3059 / main run 42359197307: the SIGTERM handler must be
    /// installed when the future is *constructed*, not on its first poll.
    /// The daemon builds this future before it becomes observably ready and
    /// polls it only from a task spawned after READY; a lazily-registering
    /// future left a window where SIGTERM took the default disposition
    /// (killed by signal 15) instead of the fast-exit path.
    #[test]
    fn handler_is_installed_before_the_future_is_polled() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("runtime");
        let _guard = runtime.enter();
        assert!(sigterm_disposition_is_default(), "fresh test process");
        let pending = wait_for_terminate_signal();
        assert!(
            !sigterm_disposition_is_default(),
            "constructing the terminate future must install the SIGTERM handler"
        );
        drop(pending);
    }
}
