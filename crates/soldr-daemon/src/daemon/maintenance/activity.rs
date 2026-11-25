//! Maintenance activity follows the worker, including detached blocking work.

use crate::cache_lib::build_active::MaintenanceLease;
use crate::platform::sync::Mutex;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

#[derive(Clone, Default)]
pub struct MaintenanceActivity {
    workers: Arc<AtomicUsize>,
    blocking: Arc<BlockingWorker>,
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
    ) -> io::Result<T> {
        let slot = self.blocking.try_reserve()?;
        let activity = self.enter();
        let (send, receive) = tokio::sync::oneshot::channel();
        self.blocking.submit(Box::new(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .map_err(|_| io::Error::other("maintenance worker panicked"));
            drop(lease);
            drop(activity);
            drop(slot);
            let _ = send.send(result);
        }))?;
        receive
            .await
            .map_err(|_| io::Error::other("maintenance worker exited without a result"))?
    }
}

type BlockingJob = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct BlockingWorker {
    busy: AtomicBool,
    sender: Mutex<Option<mpsc::SyncSender<BlockingJob>>>,
}

impl BlockingWorker {
    fn try_reserve(self: &Arc<Self>) -> io::Result<BlockingSlot> {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "maintenance worker is busy"))?;
        Ok(BlockingSlot(self.clone()))
    }

    fn submit(&self, work: BlockingJob) -> io::Result<()> {
        let mut sender = self.sender.lock();
        if sender.is_none() {
            let (send, receive) = mpsc::sync_channel::<BlockingJob>(1);
            std::thread::Builder::new()
                .name("soldr-maintenance".into())
                .spawn(move || {
                    for work in receive {
                        work();
                    }
                })?;
            *sender = Some(send);
        }
        let result = sender
            .as_ref()
            .ok_or_else(|| io::Error::other("maintenance worker channel not initialized"))?
            .try_send(work);
        if let Err(error) = result {
            *sender = None;
            return Err(io::Error::other(format!(
                "could not submit maintenance work: {error}"
            )));
        }
        Ok(())
    }
}

struct BlockingSlot(Arc<BlockingWorker>);

impl Drop for BlockingSlot {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
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

    #[tokio::test]
    async fn blocking_passes_reuse_one_thread_and_reject_overlap() {
        let activity = MaintenanceActivity::default();
        let current = std::thread::current().id();
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, finish) = mpsc::channel();
        let first = tokio::spawn({
            let activity = activity.clone();
            async move {
                activity
                    .worker(None, move || {
                        let id = std::thread::current().id();
                        started.send(id).unwrap();
                        finish.recv().unwrap();
                        id
                    })
                    .await
            }
        });
        let worker_thread = running.await.unwrap();
        assert_ne!(worker_thread, current);
        let overlap = activity
            .worker(None, || panic!("overlap must not run"))
            .await;
        assert_eq!(overlap.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        release.send(()).unwrap();
        assert_eq!(first.await.unwrap().unwrap(), worker_thread);
        let next = activity
            .worker(None, || std::thread::current().id())
            .await
            .unwrap();
        assert_eq!(next, worker_thread);
    }

    #[tokio::test]
    async fn failed_mock_pass_releases_worker_for_next_pass() {
        let activity = MaintenanceActivity::default();
        let result = activity.worker(None, || panic!("mock pass failure")).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "maintenance worker panicked"
        );
        assert!(!activity.is_active());
        assert_eq!(activity.worker(None, || 42).await.unwrap(), 42);
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
