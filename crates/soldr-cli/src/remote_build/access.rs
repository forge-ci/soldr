//! Signed grants own sessions across connections. SSH sessions keep their
//! existing connection-scoped output leases.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use axum::http::HeaderMap;

use super::{auth::RunnerAuth, retention::Connection, snapshot::digest};

#[derive(Default)]
pub(super) struct SessionAccess(Option<String>);

impl SessionAccess {
    pub(super) fn authorize(auth: &RunnerAuth, headers: &HeaderMap, start: bool) -> Option<Self> {
        if !auth.authorized(headers, start) {
            return None;
        }
        let owner = match auth {
            RunnerAuth::Tunnel(_) => None,
            RunnerAuth::Forge(_) => Some(digest(headers.get("authorization")?.as_bytes())),
        };
        Some(Self(owner))
    }

    pub(super) fn outputs(&self, connection: Connection, worker: Connection) -> Connection {
        if self.0.is_some() {
            worker
        } else {
            connection
        }
    }

    pub(super) async fn allocate_for_stream(
        self,
        connection: Connection,
        root: &Path,
        id: &str,
    ) -> io::Result<Arc<super::retention::OutputDirectory>> {
        let root = root.to_owned();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            self.check(&root, &id)?;
            connection.allocate(&root, &id)
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(super) fn record(&self, directory: &Path) -> io::Result<()> {
        if let Some(owner) = &self.0 {
            fs::write(directory.join("grant.sha256"), owner)?;
        }
        Ok(())
    }

    pub(super) fn check(&self, root: &Path, id: &str) -> io::Result<()> {
        let Some(owner) = &self.0 else {
            return Ok(());
        };
        let id = super::server::key(id, 32)?;
        let on_disk = fs::read_to_string(root.join("sessions").join(id).join("grant.sha256"))?;
        if on_disk != *owner {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unknown remote session",
            ));
        }
        Ok(())
    }

    pub(super) fn check_operation(&self, root: &Path, operation: &str) -> io::Result<()> {
        if let Some((verb, id)) = operation.split_once('/') {
            if matches!(
                verb,
                "workspace" | "run" | "run-stream" | "artifact" | "artifact-list" | "fetch"
            ) {
                self.check(root, id)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_worker_grants_isolate_sessions_across_connections() {
        let root = tempfile::tempdir().unwrap();
        let id = "0".repeat(32);
        let directory = root.path().join("sessions").join(&id);
        fs::create_dir_all(&directory).unwrap();
        let owner = SessionAccess(Some("first".into()));
        let stranger = SessionAccess(Some("second".into()));
        owner.record(&directory).unwrap();
        assert!(owner.check(root.path(), &id).is_ok());
        assert_eq!(
            stranger.check(root.path(), &id).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(owner.check(root.path(), "../other").is_err());

        let worker = Connection::default();
        let original = owner.outputs(Connection::default(), worker.clone());
        super::super::retention::prepare(root.path()).unwrap();
        let _output = original.allocate(root.path(), &id).unwrap();
        drop(original);
        assert!(owner
            .outputs(Connection::default(), worker)
            .output(&id)
            .is_some());
    }
}
