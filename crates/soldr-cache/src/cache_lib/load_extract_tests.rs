//! soldr#3098 regression: a child forked while an extraction worker holds
//! the staged file open for writing must not be able to make the restored
//! file `ETXTBSY` for cargo.
//!
//! Linux-only: the failure is a POSIX fork-inheritance artefact, and the
//! test relies on `pre_exec` to hold a child in its fork-to-exec window.

use super::*;
use std::sync::mpsc;
use std::time::Duration;

/// Sentinel in the restored file's name so the process-global hook only
/// fires for this test's entry (other extraction tests may share the
/// binary under plain `cargo test`).
const PROBE_NAME: &str = "etxtbsy-probe-3098";

/// How long the forked child lingers between fork and exec. The exec of
/// the restored file happens well inside this window.
const CHILD_PRE_EXEC_HOLD: Duration = Duration::from_secs(3);

fn archive_with_executable(root: &Path) -> (PathBuf, PathBuf) {
    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    // A script is enough: `execve` applies the ETXTBSY check to the file it
    // opens before binfmt_script hands off to the interpreter, and unlike a
    // copied system binary it does not care what argv[0] it runs under.
    let exe = cache.join(PROBE_NAME);
    std::fs::write(&exe, b"#!/bin/sh\nexit 0\n").unwrap();
    crate::platform::fs::permissions::restore_mode(&exe, Some(0o755)).unwrap();

    let archive = root.join("probe.tar.zst");
    save(&SaveOptions {
        workspace: None,
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: 1,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .unwrap();
    let restore = root.join("restore");
    (archive, restore)
}

/// Pause the extraction worker while the staged file is open for write,
/// fork a child that lingers before exec while the worker is paused (from
/// another thread, through the shared spawn guard exactly as soldr's spawn
/// funnels do), resume, then `execve` the restored file.
///
/// Without the exclusive guard in `extract_one`, the child is forked while
/// the write descriptor exists, inherits it, and the exec fails with
/// `Text file busy` (verified RED by removing the guard). With it, the
/// spawn blocks until the descriptor is closed, so the child never holds
/// it and the exec succeeds.
#[test]
fn child_forked_during_staged_write_cannot_make_the_restored_file_busy() {
    restored_file_runs_after_a_fork_during_its_staged_write(true);
}

/// soldr#3350: the spawn guard only excludes spawns that take it. The daemon
/// shares its process with embedded zccache and libraries that fork under
/// their own lock or none; such a child inherits the staged write descriptor
/// across the rename. `load` must still return only once no process holds
/// a restored executable open for writing.
#[test]
fn a_fork_outside_the_spawn_guard_cannot_make_the_restored_file_busy() {
    restored_file_runs_after_a_fork_during_its_staged_write(false);
}

/// Both tests drive the one process-global extraction hook and probe name.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn restored_file_runs_after_a_fork_during_its_staged_write(spawn_guard: bool) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // The fork-to-exec window this test drives is a Unix mechanism; the
    // platform boundary (#2493) forbids a `cfg` here, so gate at runtime.
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (archive, restore) = archive_with_executable(root.path());

    let (opened_tx, opened_rx) = mpsc::channel::<()>();
    let (resume_tx, resume_rx) = mpsc::channel::<()>();
    let resume_rx = std::sync::Mutex::new(resume_rx);
    *extract_test_hooks::HOOK.lock() = Some(Box::new(move |staged: &Path| {
        if !staged.to_string_lossy().contains(PROBE_NAME) {
            return;
        }
        let _ = opened_tx.send(());
        let _ = resume_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(Duration::from_secs(30));
    }));

    let loader = {
        let archive = archive.clone();
        let restore = restore.clone();
        std::thread::spawn(move || {
            load(&LoadOptions {
                archive: &archive,
                cache_dir: Some(&restore),
                workspace: None,
                threads: Some(2),
                mtimes_only: false,
                profile_extract: false,
                auto_defender_exclude: false,
            })
        })
    };

    opened_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("extraction worker must reach the staged write with the file open");

    // The worker is now parked with a write descriptor open on the staged
    // inode. Fork a child through the same funnel soldr's spawns use; hold
    // it between fork and exec so any inherited descriptor stays alive.
    let spawner = std::thread::spawn(move || {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", ":"]);
        crate::platform::process::spawn::spawn_holding_fork_window(
            &mut command,
            CHILD_PRE_EXEC_HOLD,
            spawn_guard,
        )
        .expect("fork the lingering child")
    });

    // Let the spawner reach the guard (guarded: blocks; unguarded: forks
    // now, while the descriptor is open) before releasing the worker.
    std::thread::sleep(Duration::from_millis(300));
    let _ = resume_tx.send(());

    let report = loader.join().unwrap().expect("load must succeed");
    assert_eq!(report.cache_files_restored, 1);
    *extract_test_hooks::HOOK.lock() = None;

    // The published path exists; the only question is whether some child
    // still holds its inode open for writing.
    let restored = restore.join(PROBE_NAME);
    let status = std::process::Command::new(&restored).status();
    let mut child = spawner.join().unwrap();
    let _ = child.wait();
    let status = status.unwrap_or_else(|e| {
        panic!("exec of the restored file must not fail (ETXTBSY = soldr#3098 race): {e}")
    });
    assert!(status.success(), "restored script must exit 0: {status:?}");
}
