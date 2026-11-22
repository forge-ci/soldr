//! Connection-scoped storage for streamed remote session outputs.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::extract::connect_info::Connected;
use tokio::sync::watch;

use crate::platform::{sync::Mutex, wait};

const RETENTION: Duration = Duration::from_secs(4 * 60 * 60);
const PREFIX: &str = "remote-output-";
const LEASE_TIMEOUT: Duration = Duration::from_millis(50);

#[derive(Clone, Default)]
pub(super) struct Connection(Arc<ConnectionOutputs>);

struct ConnectionOutputs {
    entries: Mutex<HashMap<String, Arc<OutputDirectory>>>,
    closed: watch::Sender<()>,
}

impl Default for ConnectionOutputs {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            closed: watch::Sender::new(()),
        }
    }
}

impl Drop for ConnectionOutputs {
    fn drop(&mut self) {
        let outputs = std::mem::take(&mut *self.entries.lock());
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(move || drop(outputs));
        }
    }
}

pub(super) struct OutputDirectory {
    _lock: fs::File,
    directory: tempfile::TempDir,
    owner: Weak<ConnectionOutputs>,
    id: String,
}

impl OutputDirectory {
    pub(super) fn path(&self) -> &Path {
        self.directory.path()
    }

    pub(super) fn discard(&self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.entries.lock().remove(&self.id);
        }
    }

    pub(super) fn mark_ready(&self) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let mut closed = owner.closed.subscribe();
        let owner = self.owner.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(RETENTION) => {}
                _ = closed.changed() => return,
            }
            if let Some(owner) = owner.upgrade() {
                let expired = owner.entries.lock().remove(&id);
                if let Some(expired) = expired {
                    let _ = tokio::task::spawn_blocking(move || drop(expired)).await;
                }
            }
        });
    }
}

impl Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>> for Connection {
    fn connect_info(_: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        Self::default()
    }
}

impl Connection {
    pub(super) async fn allocate_for_stream(
        &self,
        root: &Path,
        id: &str,
    ) -> io::Result<Arc<OutputDirectory>> {
        let owner = self.clone();
        let root = root.to_owned();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || owner.allocate(&root, &id))
            .await
            .map_err(io::Error::other)?
    }

    pub(super) fn allocate(&self, root: &Path, id: &str) -> io::Result<Arc<OutputDirectory>> {
        let mut outputs = self.0.entries.lock();
        if outputs.contains_key(id) {
            return Err(io::Error::other(
                "remote session already has an output lease",
            ));
        }
        let directory = tempfile::Builder::new()
            .prefix(PREFIX)
            .tempdir_in(temp_root(root))?;
        let lock = fs::File::create_new(directory.path().join("lease"))?;
        wait::lock_exclusive_for(&lock, LEASE_TIMEOUT)?;
        let directory = Arc::new(OutputDirectory {
            _lock: lock,
            directory,
            owner: Arc::downgrade(&self.0),
            id: id.to_owned(),
        });
        outputs.insert(id.to_owned(), Arc::clone(&directory));
        Ok(directory)
    }

    pub(super) fn output(&self, id: &str) -> Option<Arc<OutputDirectory>> {
        self.0.entries.lock().get(id).cloned()
    }
}

fn temp_root(root: &Path) -> PathBuf {
    root.join("artifact-tmp")
}

pub(super) fn prepare(root: &Path) -> io::Result<()> {
    let scratch = temp_root(root);
    fs::create_dir_all(&scratch)?;
    for entry in fs::read_dir(&scratch)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with(PREFIX) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.is_dir()
            && metadata
                .modified()?
                .elapsed()
                .is_ok_and(|age| age >= RETENTION)
        {
            let path = entry.path();
            if let Err(error) = collect(&path) {
                if error.kind() != io::ErrorKind::NotFound {
                    eprintln!(
                        "soldr remote: unable to collect expired archive {}: {error}",
                        path.display()
                    );
                }
            }
        }
    }
    Ok(())
}

fn collect(path: &Path) -> io::Result<()> {
    let lock = match fs::OpenOptions::new().write(true).open(path.join("lease")) {
        Ok(lock) => {
            if wait::lock_exclusive_for(&lock, LEASE_TIMEOUT).is_err() {
                return Ok(());
            }
            Some(lock)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    drop(lock);
    fs::remove_dir_all(path)
}

pub(super) fn maintain(root: PathBuf) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.tick().await;
        loop {
            interval.tick().await;
            let root = root.clone();
            if let Ok(Err(error)) = tokio::task::spawn_blocking(move || prepare(&root)).await {
                eprintln!("soldr remote: archive cleanup failed: {error}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn outputs_are_connection_scoped_and_expire_after_four_hours() {
        let root = tempfile::tempdir().unwrap();
        prepare(root.path()).unwrap();
        let owner = Connection::default();
        let output = owner.allocate(root.path(), "session").unwrap();
        assert!(output.path().is_dir());
        assert!(owner.output("session").is_some());
        assert!(Connection::default().output("session").is_none());
        tokio::time::advance(RETENTION).await;
        assert!(
            owner.output("session").is_some(),
            "active builds do not expire"
        );
        output.mark_ready();
        tokio::task::yield_now().await;
        tokio::time::advance(RETENTION).await;
        tokio::task::yield_now().await;
        assert!(owner.output("session").is_none());
    }
}
