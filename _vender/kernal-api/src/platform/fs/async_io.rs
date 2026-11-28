//! Bounded async filesystem effects.

use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

/// Shared admission and byte limits for asynchronous file writes. Clones share
/// the same budget. At capacity operations fail with `WouldBlock`, without
/// creating another queued task or retaining another copy of the input.
#[derive(Clone, Debug)]
pub struct AsyncFileIo {
    permits: Arc<tokio::sync::Semaphore>,
    max_bytes: usize,
    timeout: Duration,
}

impl AsyncFileIo {
    /// Configure 1..=64 outstanding operations, at most 64 MiB per write, and
    /// a positive timeout no greater than one day. Zero bytes permits empty files.
    ///
    /// # Errors
    /// Returns `InvalidInput` for limits outside these ranges.
    pub fn new(max_operations: usize, max_bytes: usize, timeout: Duration) -> io::Result<Self> {
        if !(1..=64).contains(&max_operations)
            || max_bytes > 64 * 1024 * 1024
            || timeout.is_zero()
            || timeout > Duration::from_secs(86400)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid async file I/O limits",
            ));
        }
        Ok(Self {
            permits: Arc::new(tokio::sync::Semaphore::new(max_operations)),
            max_bytes,
            timeout,
        })
    }

    /// Create parent directories and write a caller-authorized path, replacing
    /// its contents. This is NOT atomic or crash-durable. Symlinks are followed;
    /// callers must select trusted paths. No screenshot or content policy applies.
    ///
    /// Cancellation/timeouts request a stop between native calls, but cannot
    /// interrupt an OS call in progress. They can leave directories or a partial
    /// file, and a native effect can finish after return. Its admission permit
    /// stays held until the worker really ends, including after cancellation.
    /// No success is returned until every write and flush has completed.
    ///
    /// # Errors
    /// Reports oversize input (`InvalidInput`), exhausted capacity (`WouldBlock`),
    /// timeout (`TimedOut`), missing runtime, worker failure, or native I/O errors.
    /// Requires a runtime with timers enabled.
    pub async fn write(&self, path: PathBuf, bytes: Vec<u8>) -> io::Result<()> {
        if bytes.len() > self.max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file write exceeds byte limit",
            ));
        }
        self.run(move |stop| {
            check_stop(&stop)?;
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            check_stop(&stop)?;
            let mut file = std::fs::File::create(path)?;
            for chunk in bytes.chunks(65536) {
                check_stop(&stop)?;
                file.write_all(chunk)?;
            }
            check_stop(&stop)?;
            file.flush()
        })
        .await
    }

    async fn run<F>(&self, operation: F) -> io::Result<()>
    where
        F: FnOnce(Arc<AtomicBool>) -> io::Result<()> + Send + 'static,
    {
        crate::async_engine::RuntimeHandle::current().map_err(io::Error::other)?;
        let permit = self.permits.clone().try_acquire_owned().map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "async file I/O capacity exhausted",
            )
        })?;
        let stop = Arc::new(AtomicBool::new(false));
        let _cancel = StopOnDrop(stop.clone());
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            check_stop(&stop)?;
            operation(stop)
        });
        tokio::time::timeout(self.timeout, worker)
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "async file I/O deadline expired")
            })?
            .map_err(io::Error::other)?
    }
}

struct StopOnDrop(Arc<AtomicBool>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn check_stop(stop: &AtomicBool) -> io::Result<()> {
    if stop.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "async file I/O cancelled",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timed_out_worker_keeps_its_permit_until_native_work_ends() {
        let io = AsyncFileIo::new(1, 4, Duration::from_millis(100)).unwrap();
        let (release, waiting) = std::sync::mpsc::channel::<()>();
        let (started, began) = tokio::sync::oneshot::channel();
        let clone = io.clone();
        let task = tokio::spawn(async move {
            clone
                .run(move |_| {
                    let _ = started.send(());
                    let _ = waiting.recv();
                    Ok(())
                })
                .await
        });
        began.await.unwrap();
        assert_eq!(
            task.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            io.run(|_| Ok(())).await.unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(release);
        tokio::time::timeout(Duration::from_secs(2), async {
            while io.permits.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        io.run(|_| Ok(())).await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_signals_worker_without_releasing_its_permit_early() {
        let io = AsyncFileIo::new(1, 4, Duration::from_secs(2)).unwrap();
        let (release, waiting) = std::sync::mpsc::channel::<()>();
        let (started, began) = tokio::sync::oneshot::channel();
        let clone = io.clone();
        let task = tokio::spawn(async move {
            clone
                .run(move |stop| {
                    let _ = started.send(stop.clone());
                    let _ = waiting.recv();
                    check_stop(&stop)
                })
                .await
        });
        let stop = began.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(io.permits.available_permits(), 0);
        drop(release);
        tokio::time::timeout(Duration::from_secs(2), async {
            while io.permits.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn bounded_write_creates_parents_and_rejects_oversize_before_effects() {
        let root = tempfile::tempdir().unwrap();
        let io = AsyncFileIo::new(1, 4, Duration::from_secs(1)).unwrap();
        let path = root.path().join("nested/file");
        io.write(path.clone(), b"data".to_vec()).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"data");
        let rejected = root.path().join("absent/file");
        assert_eq!(
            io.write(rejected.clone(), b"large".to_vec())
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(!rejected.parent().unwrap().exists());
    }
}
