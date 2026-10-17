//! Fair barrier between compile publication and embedded-service shutdown.
//!
//! zccache publishes a successful miss synchronously before `compile()`
//! returns. Holding the read side for the entire call therefore makes the
//! write side a precise drain: shutdown cannot set zccache's shutdown flag
//! until every compile that was already admitted has finished publishing.
//!
//! Deliberately a raw `tokio::sync::RwLock` with owned guards, not a
//! `soldr_platform::sync` critical section (soldr#3836): a read guard lives
//! for a whole compile, which is legitimately longer than the critical-section
//! hold limit. The writer is shutdown, which bounds its own drain.

use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct CompileShutdownGate {
    accepting: Arc<tokio::sync::RwLock<bool>>,
}

impl CompileShutdownGate {
    pub(crate) async fn enter(&self) -> Option<tokio::sync::OwnedRwLockReadGuard<bool>> {
        let guard = Arc::clone(&self.accepting).read_owned().await;
        (*guard).then_some(guard)
    }

    pub(crate) async fn close_and_drain(&self) -> tokio::sync::OwnedRwLockWriteGuard<bool> {
        let mut guard = Arc::clone(&self.accepting).write_owned().await;
        *guard = false;
        guard
    }
}

impl Default for CompileShutdownGate {
    fn default() -> Self {
        Self {
            accepting: Arc::new(tokio::sync::RwLock::new(true)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CompileShutdownGate;
    use std::future::{poll_fn, Future};
    use std::task::Poll;

    #[tokio::test]
    async fn shutdown_waits_for_compile_publication_and_closes_admission() {
        let gate = CompileShutdownGate::default();
        let compile_lease = gate.enter().await.expect("compile admitted");
        let shutdown = gate.close_and_drain();
        tokio::pin!(shutdown);
        poll_fn(|cx| {
            assert!(
                shutdown.as_mut().poll(cx).is_pending(),
                "shutdown must wait while an admitted compile can still publish"
            );
            Poll::Ready(())
        })
        .await;
        drop(compile_lease);
        let shutdown_guard = shutdown.await;
        drop(shutdown_guard);
        assert!(
            gate.enter().await.is_none(),
            "compile admission must stay closed after shutdown starts"
        );
    }
}
