//! Windows replacement: `MoveFileExW` with verbatim paths and the
//! antivirus-scanner retry ladder.

use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

use super::native_call_path;

/// `ERROR_ACCESS_DENIED`.
const ERROR_ACCESS_DENIED: i32 = 5;
/// `ERROR_SHARING_VIOLATION`.
const ERROR_SHARING_VIOLATION: i32 = 32;
/// `ERROR_LOCK_VIOLATION`.
const ERROR_LOCK_VIOLATION: i32 = 33;

/// Pause before each retry after the first attempt: five attempts in all.
const RETRY_DELAYS_MS: [u64; 4] = [50, 100, 250, 500];

/// Uniquifies intermediate backup names in [`install_directory`].
static DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The transient failures an antivirus scanner or indexer causes around a
/// rename (access denied, sharing violation), plus any error already shaped
/// as `PermissionDenied`. Broader than [`is_transient_share_error`] on
/// purpose: the retry must also cover synthesized permission errors.
fn is_av_scan_transient(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied || is_transient_share_error(error)
}

pub fn is_transient_share_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(ERROR_ACCESS_DENIED) | Some(ERROR_SHARING_VIOLATION)
    )
}

pub fn is_lock_contention(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_LOCK_VIOLATION)
}

fn av_scan_retry<T>(operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    retry_with_delay(operation, |ms| {
        std::thread::sleep(std::time::Duration::from_millis(ms))
    })
}

fn retry_with_delay<T>(
    mut operation: impl FnMut() -> io::Result<T>,
    mut delay_ms: impl FnMut(u64),
) -> io::Result<T> {
    let mut last = operation();
    for delay in RETRY_DELAYS_MS {
        match &last {
            Ok(_) => return last,
            Err(error) if !is_av_scan_transient(error) => return last,
            Err(_) => {
                delay_ms(delay);
                last = operation();
            }
        }
    }
    last
}

fn move_file(source: &Path, destination: &Path, flags: u32) -> io::Result<()> {
    let source = wide_path(&native_call_path(source)?)?;
    let destination = wide_path(&native_call_path(destination)?)?;
    av_scan_retry(|| {
        // SAFETY: both buffers are live, NUL-terminated UTF-16 paths that
        // MoveFileExW does not retain.
        let moved = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) };
        if moved == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

pub fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    move_file(
        source,
        destination,
        MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
    )
}

pub fn rename_generation(source: &Path, destination: &Path) -> io::Result<()> {
    move_file(source, destination, 0)
}

pub fn replace_with_delete_fallback(source: &Path, destination: &Path) -> io::Result<()> {
    match atomic_replace(source, destination) {
        Ok(()) => Ok(()),
        Err(_) if destination.exists() => {
            std::fs::remove_file(destination)?;
            rename_generation(source, destination)
        }
        Err(error) => Err(error),
    }
}

/// Windows has no atomic directory exchange, so the existing tree is first
/// renamed aside to a backup name; failure attempts restoration.
pub fn install_directory(staged: &Path, requested: &Path) -> io::Result<()> {
    let parent = requested.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    if !requested.exists() {
        return std::fs::rename(staged, requested);
    }
    let backup = parent.join(format!(
        ".zccache-directory-backup-{}-{}",
        std::process::id(),
        DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    install_with_backup(
        staged,
        requested,
        &backup,
        |from, to| std::fs::rename(from, to),
        remove_directory_if_present,
    )
}

fn install_with_backup(
    staged: &Path,
    requested: &Path,
    backup: &Path,
    mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
    mut remove: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    rename(requested, backup)?;
    if let Err(error) = rename(staged, requested) {
        let _ = rename(backup, requested);
        return Err(error);
    }
    remove(backup)
}

fn remove_directory_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_install_attempts_rollback_without_masking_the_install_error() {
        for rollback_fails in [false, true] {
            let mut calls = Vec::new();
            let error = install_with_backup(
                Path::new("staged"),
                Path::new("requested"),
                Path::new("backup"),
                |from, to| {
                    calls.push((from.to_path_buf(), to.to_path_buf()));
                    match calls.len() {
                        1 => Ok(()),
                        2 => Err(io::Error::from_raw_os_error(32)),
                        3 if rollback_fails => Err(io::Error::from_raw_os_error(5)),
                        3 => Ok(()),
                        _ => panic!("unexpected rename"),
                    }
                },
                |_| panic!("failed installation must retain the backup, not clean it"),
            )
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(32));
            assert_eq!(
                calls,
                [
                    ("requested".into(), "backup".into()),
                    ("staged".into(), "requested".into()),
                    ("backup".into(), "requested".into()),
                ]
            );
        }
    }

    #[test]
    fn failed_initial_backup_stops_before_installation() {
        let mut calls = 0;
        let error = install_with_backup(
            Path::new("staged"),
            Path::new("requested"),
            Path::new("backup"),
            |_, _| {
                calls += 1;
                Err(io::Error::from_raw_os_error(5))
            },
            |_| panic!("no backup was created"),
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(error.raw_os_error(), Some(5));
    }

    #[test]
    fn cleanup_failure_is_reported_after_successful_install_without_rollback() {
        let mut renames = 0;
        let error = install_with_backup(
            Path::new("staged"),
            Path::new("requested"),
            Path::new("backup"),
            |_, _| {
                renames += 1;
                Ok(())
            },
            |path| {
                assert_eq!(path, Path::new("backup"));
                Err(io::Error::from_raw_os_error(5))
            },
        )
        .unwrap_err();
        assert_eq!(renames, 2);
        assert_eq!(error.raw_os_error(), Some(5));
    }

    #[test]
    fn retry_exhaustion_preserves_exact_attempts_delays_and_last_error() {
        let mut attempts = 0;
        let mut delays = Vec::new();
        let error = retry_with_delay::<()>(
            || {
                attempts += 1;
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    attempts.to_string(),
                ))
            },
            |delay| delays.push(delay),
        )
        .unwrap_err();
        assert_eq!(attempts, 5);
        assert_eq!(delays, [50, 100, 250, 500]);
        assert_eq!(error.to_string(), "5");
    }

    #[test]
    fn retry_stops_on_success_or_nontransient_error() {
        let mut attempts = 0;
        let mut delays = Vec::new();
        let result = retry_with_delay(
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(io::Error::from_raw_os_error(32))
                } else {
                    Ok(42)
                }
            },
            |delay| delays.push(delay),
        )
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(attempts, 3);
        assert_eq!(delays, [50, 100]);
        let error = retry_with_delay::<()>(
            || Err(io::Error::from_raw_os_error(2)),
            |_| panic!("nontransient failure must not wait"),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(2));
        assert_eq!(
            retry_with_delay(|| Ok(7), |_| panic!("success must not wait")).unwrap(),
            7
        );
    }

    #[test]
    fn share_classifiers_are_narrower_than_the_retry_rule() {
        let synthetic = io::Error::new(io::ErrorKind::PermissionDenied, "synthetic");
        assert!(is_av_scan_transient(&synthetic));
        assert!(!is_transient_share_error(&synthetic));
        assert!(is_transient_share_error(&io::Error::from_raw_os_error(5)));
        assert!(is_transient_share_error(&io::Error::from_raw_os_error(32)));
        assert!(!is_transient_share_error(&io::Error::from_raw_os_error(33)));
        assert!(is_lock_contention(&io::Error::from_raw_os_error(33)));
        assert!(!is_lock_contention(&io::Error::from_raw_os_error(32)));
    }

    #[test]
    fn nul_path_is_rejected_and_unpaired_surrogates_are_preserved() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt as _;
        assert_eq!(
            wide_path(Path::new("prefix\0suffix")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let path = OsString::from_wide(&[65, 0xd800, 66]);
        assert_eq!(wide_path(Path::new(&path)).unwrap(), [65, 0xd800, 66, 0]);
    }
}
