//! Journal hygiene for the daemon lifecycle log (soldr#2436 phase 2):
//! the rotation bound and successor-side unclean-shutdown detection.
//! Split from `mod.rs` for the #2493 1,000-line production ceiling.

use super::*;

/// Rotation rule (soldr#2436 D1): the lifecycle journal is append-only
/// diagnostics, so bound it at daemon start — over 10,000 lines truncate
/// to the newest 5,000. Best-effort: a failed rotation never blocks
/// startup.
///
/// soldr#3643: rotation and appenders serialize on a sibling
/// `<journal>.lock` file (`with_lifecycle_journal_lock`), and the rotated
/// content goes to `<journal>.tmp` then is renamed over the journal, so a
/// line appended between the read and the write is no longer dropped and a
/// failed write can no longer leave a truncated journal.
const LIFECYCLE_ROTATE_THRESHOLD: usize = 10_000;
const LIFECYCLE_ROTATE_KEEP: usize = 5_000;

/// Run `f` holding an exclusive advisory lock on `<journal>.lock`.
/// Best-effort: if the lock cannot be opened or taken, `f` runs unlocked.
/// The lock is released when the file handle drops.
pub(super) fn with_lifecycle_journal_lock<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    use fs2::FileExt;
    let mut lock_name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    lock_name.push(".lock");
    let lock_path = path.with_file_name(lock_name);
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let guard = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .and_then(|file| lock_exclusive_within(&file, JOURNAL_LOCK_DEADLINE).map(|()| file));
    if let Err(err) = &guard {
        eprintln!("soldr: lifecycle journal lock unavailable: {err}");
    }
    let result = f();
    drop(guard);
    result
}

/// The journal lock is shared with every soldr process, and this runs on
/// daemon worker threads: a stopped process holding it must not stall them.
/// Past the deadline the caller proceeds unlocked, as it already does when
/// the lock errors (soldr#3836 audit).
const JOURNAL_LOCK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

fn lock_exclusive_within(
    file: &std::fs::File,
    deadline: std::time::Duration,
) -> std::io::Result<()> {
    let started = std::time::Instant::now();
    let mut backoff = std::time::Duration::from_millis(1);
    loop {
        if file.try_lock_exclusive().is_ok() {
            return Ok(());
        }
        let waited = started.elapsed();
        if waited >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("held by another process for over {deadline:?}"),
            ));
        }
        std::thread::sleep(backoff.min(deadline - waited));
        backoff = (backoff * 2).min(std::time::Duration::from_millis(50));
    }
}

pub fn rotate_lifecycle_journal(paths: &SoldrPaths) {
    let path = daemon_lifecycle_log_path(paths);
    with_lifecycle_journal_lock(&path, || {
        let Ok(content) = fs::read_to_string(&path) else {
            return;
        };
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() <= LIFECYCLE_ROTATE_THRESHOLD {
            return;
        }
        let keep = &lines[lines.len() - LIFECYCLE_ROTATE_KEEP..];
        let mut rotated = keep.join("\n");
        rotated.push('\n');
        let mut tmp_name = path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        tmp_name.push(".tmp");
        let tmp = path.with_file_name(tmp_name);
        if let Err(err) = fs::write(&tmp, rotated).and_then(|()| fs::rename(&tmp, &path)) {
            eprintln!("soldr: lifecycle journal rotation failed: {err}");
            let _ = fs::remove_file(&tmp);
        }
    });
}

/// Successor-side unclean-shutdown detection (soldr#2436 D3): at daemon
/// start, a pid file naming a dead process that never wrote an exit
/// record means the previous daemon died un-drained (SIGKILL, OOM,
/// TerminateProcess) — exactly the transitions that lose in-memory
/// compile contexts. Record it so restart forensics stop dead-ending.
pub fn detect_unclean_shutdown(paths: &SoldrPaths) {
    // Modern daemons publish a broker route claim; releases ≤0.8.29 wrote
    // the two-line daemon.pid. Either artifact naming a dead pid counts.
    let claimed_pid = crate::daemon::backend_handle_adoption::read_broker_route_claim(paths)
        .ok()
        .flatten()
        .map(|claim| claim.pid);
    let legacy_pid = read_legacy_daemon_pid_identity(paths).map(|(pid, _)| pid);
    let Some(dead_pid) = claimed_pid.or(legacy_pid) else {
        return;
    };
    if pid_is_alive(dead_pid) {
        return;
    }
    let journal = fs::read_to_string(daemon_lifecycle_log_path(paths)).unwrap_or_default();
    let has_exit_record = journal.lines().any(|line| {
        line.contains(&format!("\"pid\":{dead_pid},"))
            && (line.contains("\"event\":\"died-") || line.contains("\"event\":\"exit\""))
    });
    if has_exit_record {
        return;
    }
    append_lifecycle_event_with(
        paths,
        "unclean-shutdown-detected",
        LifecycleDetails {
            target_pid: Some(dead_pid),
            ..LifecycleDetails::recording_daemon_identity()
        },
    );
}
