//! Cross-process serialization and background provisioning of the managed
//! LLVM (soldr#3801).
//!
//! The managed LLVM download is ~95 MiB. A build that needs it only at its
//! *linking* compiles should not stall `cargo` before the first rustc: the
//! front door starts the fetch on a background thread
//! ([`start_background_llvm_fetch`]) and only a linking wrapper process waits
//! for it ([`wait_for_managed_llvm`]).
//!
//! Both halves meet on one exclusive file lock beside the install directory.
//! The fetcher holds it across download, extract and the `.complete` stamp
//! (written last), so a waiter that gets the lock afterwards finds either the
//! stamp -- done -- or no stamp, meaning the fetcher failed or was killed, in
//! which case the waiter runs the ensure itself to surface the real error.

use super::llvm::{ensure_llvm_toolchain, unsupported_host_error, MANAGED_LLVM_VERSION};
use crate::core::{SoldrError, SoldrPaths};
use std::collections::HashSet;
use std::fs::File;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Seconds a linking compile waits for the background LLVM fetch before it
/// gives up with a clear error. A hard limit on top of the network timeouts:
/// without it cargo waits on the wrapper, the wrapper on the fetch lock, and a
/// wedged fetcher would hang the build forever.
pub const LLVM_FETCH_WAIT_ENV_VAR: &str = "SOLDR_LLVM_FETCH_WAIT_SECS";
/// Default for [`LLVM_FETCH_WAIT_ENV_VAR`].
pub const DEFAULT_LLVM_FETCH_WAIT: Duration = Duration::from_secs(300);

const LOCK_POLL: Duration = Duration::from_millis(25);

/// The directory the managed LLVM installs into (`.complete` lives here).
#[must_use]
pub fn managed_llvm_install_dir(paths: &SoldrPaths) -> PathBuf {
    paths.bin.join(format!("llvm-{MANAGED_LLVM_VERSION}"))
}

/// The managed LLVM `bin` directory, whether or not it is installed yet.
#[must_use]
pub fn managed_llvm_bin_dir(paths: &SoldrPaths) -> PathBuf {
    managed_llvm_install_dir(paths)
        .join("hardlinked")
        .join("bin")
}

/// Has the install in `install_dir` been fully stamped?
#[must_use]
pub fn llvm_install_complete(install_dir: &Path) -> bool {
    install_dir.join(".complete").is_file()
}

/// The wait budget: [`LLVM_FETCH_WAIT_ENV_VAR`] seconds when it holds a
/// positive integer, else [`DEFAULT_LLVM_FETCH_WAIT`].
#[must_use]
pub fn llvm_fetch_wait_from_env() -> Duration {
    parse_wait_secs(std::env::var(LLVM_FETCH_WAIT_ENV_VAR).ok().as_deref())
}

fn parse_wait_secs(value: Option<&str>) -> Duration {
    value
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map_or(DEFAULT_LLVM_FETCH_WAIT, Duration::from_secs)
}

fn lock_path(install_dir: &Path) -> PathBuf {
    let mut name = install_dir.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// An exclusive advisory lock on the install; released when dropped (or when
/// the holding process dies).
pub(super) struct InstallLock(File);

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

/// Take the lock, polling until `deadline` (forever when `None`). `Ok(None)`
/// means the deadline passed with the lock still held elsewhere.
fn acquire_blocking(
    install_dir: &Path,
    deadline: Option<Instant>,
) -> Result<Option<InstallLock>, SoldrError> {
    let path = lock_path(install_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    let contended = fs2::lock_contended_error().raw_os_error();
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(Some(InstallLock(file))),
            Err(error) if error.raw_os_error() == contended => {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Ok(None);
                }
                std::thread::sleep(LOCK_POLL);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Take the install lock without blocking the async runtime.
pub(super) async fn lock_install(install_dir: &Path) -> Result<InstallLock, SoldrError> {
    let dir = install_dir.to_path_buf();
    let lock = tokio::task::spawn_blocking(move || acquire_blocking(&dir, None))
        .await
        .map_err(|error| SoldrError::Other(format!("LLVM install lock task failed: {error}")))??;
    lock.ok_or_else(|| SoldrError::Other("LLVM install lock: unreachable timeout".into()))
}

fn wait_timeout_error(timeout: Duration) -> SoldrError {
    SoldrError::Other(format!(
        "timed out after {}s waiting for the managed LLVM v{MANAGED_LLVM_VERSION} fetch to \
         finish; raise {LLVM_FETCH_WAIT_ENV_VAR}, or install clang",
        timeout.as_secs()
    ))
}

/// Wait for another process's LLVM fetch of `install_dir`, then make sure it
/// is complete: block on the install lock (bounded by `timeout`), and if the
/// stamp is still missing once the lock is free -- the fetcher failed or was
/// killed -- run `ensure` here, within the remaining budget, so the real error
/// reaches the caller.
pub(crate) async fn wait_for_install<F, Fut>(
    install_dir: &Path,
    timeout: Duration,
    ensure: F,
) -> Result<(), SoldrError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<(), SoldrError>>,
{
    if llvm_install_complete(install_dir) {
        return Ok(());
    }
    let deadline = Instant::now() + timeout;
    let dir = install_dir.to_path_buf();
    let held = tokio::task::spawn_blocking(move || acquire_blocking(&dir, Some(deadline)))
        .await
        .map_err(|error| SoldrError::Other(format!("LLVM wait task failed: {error}")))??;
    let Some(lock) = held else {
        return Err(wait_timeout_error(timeout));
    };
    drop(lock);
    if llvm_install_complete(install_dir) {
        return Ok(());
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    match tokio::time::timeout(remaining, ensure()).await {
        Ok(result) => result,
        Err(_) => Err(wait_timeout_error(timeout)),
    }
}

/// [`wait_for_install`] with the real [`ensure_llvm_toolchain`] as the
/// fallback.
pub async fn wait_for_managed_llvm(
    paths: &SoldrPaths,
    install_dir: &Path,
    timeout: Duration,
) -> Result<(), SoldrError> {
    wait_for_install(install_dir, timeout, || async {
        ensure_llvm_toolchain(paths).await.map(|_| ())
    })
    .await
}

/// `Ok` when this host has a managed LLVM asset to fetch.
pub fn check_managed_llvm_host() -> Result<(), SoldrError> {
    if super::llvm::host_has_managed_llvm() {
        Ok(())
    } else {
        Err(unsupported_host_error())
    }
}

/// Start fetching the managed LLVM on a detached thread and return at once
/// with the install directory being filled. Errors early (before spawning)
/// when this host has no managed LLVM. Started at most once per install
/// directory per process. The thread dies with the process; an interrupted
/// install has no stamp, so the next caller redoes it under the lock.
pub fn start_background_llvm_fetch(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    check_managed_llvm_host()?;
    let install_dir = managed_llvm_install_dir(paths);
    if llvm_install_complete(&install_dir) {
        return Ok(install_dir);
    }
    static STARTED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let started = STARTED.get_or_init(|| Mutex::new(HashSet::new()));
    let first = started
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(install_dir.clone());
    if first {
        let paths = paths.clone();
        std::thread::Builder::new()
            .name("soldr-llvm-fetch".into())
            .spawn(move || run_fetch_thread(&paths))
            .map_err(SoldrError::Io)?;
    }
    Ok(install_dir)
}

fn run_fetch_thread(paths: &SoldrPaths) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("soldr: background LLVM fetch could not start: {error}");
            return;
        }
    };
    if let Err(error) = runtime.block_on(ensure_llvm_toolchain(paths)) {
        eprintln!("soldr: background LLVM fetch failed: {error}");
    }
}

#[cfg(test)]
#[path = "llvm_background_tests.rs"]
mod tests;
