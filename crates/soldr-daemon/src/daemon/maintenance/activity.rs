//! Maintenance activity follows the worker, including detached blocking work.

use crate::cache_lib::build_active::MaintenanceLease;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct MaintenanceActivity {
    workers: Arc<AtomicUsize>,
}

impl MaintenanceActivity {
    pub fn is_active(&self) -> bool {
        self.workers.load(Ordering::Acquire) != 0
    }

    pub(super) fn enter(&self) -> ActivityToken {
        self.workers.fetch_add(1, Ordering::AcqRel);
        ActivityToken(self.clone())
    }

    /// Keep the embedded store's awaited blocking work alive independently
    /// of cancellation of the maintenance loop's waiter.
    pub(super) fn store<F>(&self, work: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let activity = self.enter();
        tokio::spawn(async move {
            let _activity = activity;
            work.await
        })
    }

    pub(super) async fn worker<T: Send + 'static>(
        &self,
        lease: Option<MaintenanceLease>,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, tokio::task::JoinError> {
        // Acquire before scheduling so even a queued worker reports busy.
        // Cancellation drops the JoinHandle, not the blocking closure: both
        // guards must belong to that closure until its work really returns.
        let activity = self.enter();
        tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let _lease = lease;
            work()
        })
        .await
    }
}

pub(super) struct ActivityToken(MaintenanceActivity);

impl Drop for ActivityToken {
    fn drop(&mut self) {
        self.0.workers.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_lib::build_active::BuildRootLease;
    use crate::core::SoldrPaths;

    #[tokio::test]
    async fn cancelled_awaiter_keeps_worker_busy_and_root_exclusive() {
        cancelled_worker(true).await;
    }

    #[tokio::test]
    async fn cancelled_store_worker_stays_busy_without_blocking_builds() {
        cancelled_worker(false).await;
    }

    #[tokio::test]
    async fn cancelled_store_awaiter_keeps_async_store_busy_until_done() {
        let activity = MaintenanceActivity::default();
        let worker_activity = activity.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            worker_activity
                .store(async move {
                    started_tx.send(()).unwrap();
                    release_rx.await.unwrap();
                    let _ = done_tx.send(());
                })
                .await
        });
        started_rx.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        let busy = activity.is_active();
        release_tx.send(()).unwrap();
        done_rx.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while activity.is_active() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(busy, "the store must outlive its cancelled waiter");
    }

    async fn cancelled_worker(exclusive: bool) {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let lease = exclusive.then(|| MaintenanceLease::try_acquire(&paths).unwrap().unwrap());
        let activity = MaintenanceActivity::default();
        let worker_activity = activity.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            worker_activity
                .worker(lease, move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    let _ = done_tx.send(());
                })
                .await
        });
        started_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let busy = activity.is_active();
        let (acquired_tx, mut acquired_rx) = tokio::sync::oneshot::channel();
        let (attempt_tx, attempt_rx) = tokio::sync::oneshot::channel();
        let build = tokio::task::spawn_blocking(move || {
            let _ = attempt_tx.send(());
            let lease = BuildRootLease::acquire(&paths).unwrap();
            let _ = acquired_tx.send(());
            lease
        });
        attempt_rx.await.unwrap();
        let blocked = tokio::time::timeout(std::time::Duration::from_millis(50), &mut acquired_rx)
            .await
            .is_err();
        release_tx.send(()).unwrap();
        done_rx.await.unwrap();
        build.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while activity.is_active() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            (busy, blocked),
            (true, exclusive),
            "detached workers stay busy; only destructive workers block builds",
        );
    }
}
