//! Build activity tracking for cleanup coordination.
//!
//! The atomic flag is retained for cheap same-process checks, but the
//! cross-process authority is an OS-held lease file. A detached GC process can
//! therefore see a build owned by a different soldr process, and overlapping
//! sessions cannot clear one another's activity.

use crate::cache_lib::cargo_lock::lock_is_held;
use crate::core::SoldrPaths;
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

static IS_BUILD_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn set(active: bool) {
    IS_BUILD_ACTIVE.store(active, Ordering::Release);
}

pub fn is_active() -> bool {
    IS_BUILD_ACTIVE.load(Ordering::Acquire)
}

fn lease_dir(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("build-leases")
}

const MAINTENANCE_LOCK_NAME: &str = "root-maintenance.lock";

fn maintenance_lock_path(paths: &SoldrPaths) -> PathBuf {
    lease_dir(paths).join(MAINTENANCE_LOCK_NAME)
}

/// An OS-held lease that marks one build session active across processes.
pub struct BuildActivityLease {
    _root_lease: BuildRootLease,
    file: File,
    path: PathBuf,
}

impl BuildActivityLease {
    pub fn acquire(paths: &SoldrPaths, session_id: u64) -> io::Result<Self> {
        let dir = lease_dir(paths);
        fs::create_dir_all(&dir)?;
        let root_lease = BuildRootLease::acquire(paths)?;
        let path = dir.join(format!("{session_id:016x}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        file.try_lock_exclusive()?;
        Ok(Self {
            _root_lease: root_lease,
            file,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BuildActivityLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
        let _ = fs::remove_file(&self.path);
    }
}

/// Shared side of the root-wide maintenance lease. The daemon uses this for
/// session clients that did not enter through `soldr cargo`; the front door's
/// [`BuildActivityLease`] adds the per-session diagnostic lock on top.
pub struct BuildRootLease {
    file: File,
}

impl BuildRootLease {
    pub fn acquire(paths: &SoldrPaths) -> io::Result<Self> {
        let dir = lease_dir(paths);
        fs::create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(maintenance_lock_path(paths))?;
        // A maintenance pass already owns the root exclusively. Builds are
        // correctness-critical, so wait for that bounded pass to finish
        // instead of failing an otherwise valid cargo invocation.
        crate::platform::wait::lock_shared_for(&file, crate::platform::wait::LONG_LOCK)?;
        Ok(Self { file })
    }
}

impl Drop for BuildRootLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Exclusive root-wide lease held for the complete destructive maintenance
/// pass. Acquiring this lock blocks new build leases and fails while any build
/// already owns its shared side, closing the probe-then-delete race.
pub struct MaintenanceLease {
    file: File,
}

impl MaintenanceLease {
    pub fn try_acquire(paths: &SoldrPaths) -> io::Result<Option<Self>> {
        let dir = lease_dir(paths);
        fs::create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(maintenance_lock_path(paths))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { file })),
            Err(error) if lock_is_held(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

impl Drop for MaintenanceLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Return whether any other process currently holds a build lease.
///
/// Unreadable state fails closed as active. Stale, unlocked lease files are
/// removed after successfully acquiring their lock.
pub fn any_active(paths: &SoldrPaths) -> io::Result<bool> {
    let dir = lease_dir(paths);
    fs::create_dir_all(&dir)?;
    let root_lock = match OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(maintenance_lock_path(paths))
    {
        Ok(file) => file,
        Err(error) if lock_is_held(&error) => return Ok(true),
        Err(error) => return Err(error),
    };
    match root_lock.try_lock_exclusive() {
        Ok(()) => {
            let _ = root_lock.unlock();
        }
        Err(error) if lock_is_held(&error) => return Ok(true),
        Err(error) => return Err(error),
    }
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|name| name == MAINTENANCE_LOCK_NAME)
        {
            continue;
        }
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if lock_is_held(&error) => return Ok(true),
            Err(error) => return Err(error),
        };
        match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = file.unlock();
                let _ = fs::remove_file(path);
            }
            Err(error) if lock_is_held(&error) => return Ok(true),
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_clear_round_trip() {
        set(true);
        assert!(is_active());
        set(false);
        assert!(!is_active());
    }

    #[test]
    fn leases_coordinate_and_cleanup_stale_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let first = BuildActivityLease::acquire(&paths, 1).unwrap();
        let second = BuildActivityLease::acquire(&paths, 2).unwrap();
        assert!(any_active(&paths).unwrap());
        drop(first);
        assert!(any_active(&paths).unwrap());
        drop(second);
        assert!(!any_active(&paths).unwrap());
    }

    #[test]
    fn maintenance_defers_for_an_existing_build() {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let build = BuildActivityLease::acquire(&paths, 1).unwrap();
        assert!(MaintenanceLease::try_acquire(&paths).unwrap().is_none());
        drop(build);
        MaintenanceLease::try_acquire(&paths)
            .unwrap()
            .expect("maintenance resumes after build");
    }

    #[test]
    fn root_only_daemon_session_is_visible_to_legacy_gc() {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let lease = BuildRootLease::acquire(&paths).unwrap();
        assert!(any_active(&paths).unwrap());
        drop(lease);
        assert!(!any_active(&paths).unwrap());
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn subprocess_probe_build_lease() {
        let root = std::env::var_os("SOLDR_TEST_BUILD_LEASE_ROOT").expect("test root");
        let ready = std::env::var_os("SOLDR_TEST_BUILD_LEASE_READY").expect("ready path");
        let mode = std::env::var("SOLDR_TEST_BUILD_LEASE_MODE").expect("mode");
        let paths = SoldrPaths::with_root(PathBuf::from(root));
        std::fs::write(&ready, b"waiting").unwrap();
        let lease = BuildActivityLease::acquire(&paths, std::process::id() as u64).unwrap();
        std::fs::write(&ready, b"acquired").unwrap();
        if mode == "abrupt" {
            std::process::exit(0);
        }
        drop(lease);
    }

    fn wait_for_file(path: &Path, expected: &[u8]) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if std::fs::read(path).is_ok_and(|value| value == expected) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn build_waits_for_maintenance_then_proceeds() {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let ready = temp.path().join("ready");
        let maintenance = MaintenanceLease::try_acquire(&paths).unwrap().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "cache_lib::build_active::tests::subprocess_probe_build_lease",
                "--nocapture",
            ])
            .env("SOLDR_TEST_BUILD_LEASE_ROOT", &paths.root)
            .env("SOLDR_TEST_BUILD_LEASE_READY", &ready)
            .env("SOLDR_TEST_BUILD_LEASE_MODE", "normal")
            .spawn()
            .unwrap();
        wait_for_file(&ready, b"waiting");
        assert!(
            child.try_wait().unwrap().is_none(),
            "build must wait for maintenance"
        );
        drop(maintenance);
        assert!(child.wait().unwrap().success());
        assert_eq!(std::fs::read(&ready).unwrap(), b"acquired");
    }

    #[test]
    fn abrupt_build_process_exit_releases_root_lease() {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let ready = temp.path().join("ready");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "cache_lib::build_active::tests::subprocess_probe_build_lease",
                "--nocapture",
            ])
            .env("SOLDR_TEST_BUILD_LEASE_ROOT", &paths.root)
            .env("SOLDR_TEST_BUILD_LEASE_READY", &ready)
            .env("SOLDR_TEST_BUILD_LEASE_MODE", "abrupt")
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read(&ready).unwrap(), b"acquired");
        MaintenanceLease::try_acquire(&paths)
            .unwrap()
            .expect("OS releases the crashed client's build lease");
    }
}
