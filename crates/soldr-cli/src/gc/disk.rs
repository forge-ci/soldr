//! Host-volume disk watchdog (issue #574).
//!
//! Probes free space on the volume that hosts the build's `target/` dir
//! (or CWD if `target/` doesn't exist yet) and either warns or blocks
//! the cargo invocation when free space drops below configurable
//! thresholds. Distinct from `cargo_front_door::disk` (which is the
//! older, narrower "low disk" advisory) — this is the GiB-scale
//! watchdog tied to cross-repo `target/` reclamation via
//! `soldr gc target`.

use std::io;
use std::path::{Path, PathBuf};

/// Env-var override for the warn threshold (in GiB).
pub(crate) const WARN_FREE_GB_ENV_VAR: &str = "SOLDR_TARGET_WARN_FREE_GB";
/// Env-var override for the block threshold (in GiB).
pub(crate) const BLOCK_FREE_GB_ENV_VAR: &str = "SOLDR_TARGET_BLOCK_FREE_GB";
/// Env-var that disables the watchdog entirely when set to a falsy
/// value (`0` / `false` / `no` / `off`, case-insensitive).
pub(crate) const AUTO_PRUNE_ENABLED_ENV_VAR: &str = "SOLDR_TARGET_AUTO_PRUNE_ENABLED";
/// Test seam — when set, overrides the real `fs2::available_space`
/// probe so unit tests can drive every threshold edge.
///
/// Accepts either a single value, or a comma-separated sequence
/// consumed one entry per probe with the last entry repeating. The
/// sequence form exists because the pre-block reclaim probes twice —
/// once to decide, once to see whether the reclaim helped — and a
/// constant cannot express "space was freed in between".
pub(crate) const TEST_DISK_FREE_BYTES_ENV_VAR: &str = "SOLDR_TEST_DISK_FREE_BYTES";

/// How far into a `SOLDR_TEST_DISK_FREE_BYTES` sequence we are.
static TEST_DISK_PROBE_INDEX: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Reset the sequence cursor. Tests call this so ordering between them
/// cannot leak; there is no production caller.
#[cfg(test)]
pub(crate) fn reset_test_disk_probe_cursor() {
    TEST_DISK_PROBE_INDEX.store(0, std::sync::atomic::Ordering::SeqCst);
}

pub(crate) const DEFAULT_WARN_FREE_GB: u64 = 10;
pub(crate) const DEFAULT_BLOCK_FREE_GB: u64 = 5;

const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

/// Outcome of one watchdog check. Caller decides what to do — for
/// the cargo front door, `Warn` emits a one-line stderr message and
/// continues, `Block` aborts the invocation with a clear error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiskCheckOutcome {
    /// Watchdog disabled via `SOLDR_TARGET_AUTO_PRUNE_ENABLED=0`, or
    /// the disk probe failed. Either way the caller should proceed.
    Disabled,
    /// Free space is healthy.
    Ok { free_bytes: u64 },
    /// Free space dropped below the warn threshold but is still above
    /// the block threshold.
    Warn { free_bytes: u64, threshold_gib: u64 },
    /// Free space dropped below the block threshold. Caller MUST abort
    /// with the rendered suggestion line.
    Block { free_bytes: u64, threshold_gib: u64 },
}

/// Resolve free bytes for the disk that holds `path`. Honors
/// `SOLDR_TEST_DISK_FREE_BYTES` so unit tests can drive every code
/// path without touching the real filesystem.
pub(crate) fn free_bytes_for(path: &Path) -> io::Result<u64> {
    if let Some(raw) = std::env::var_os(TEST_DISK_FREE_BYTES_ENV_VAR) {
        let raw = raw.to_string_lossy();
        let steps: Vec<&str> = raw.split(',').map(str::trim).collect();
        let index = TEST_DISK_PROBE_INDEX.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The last entry repeats, so a single value behaves exactly as
        // it always has and a sequence settles rather than running out.
        let step = steps[index.min(steps.len() - 1)];
        if step.eq_ignore_ascii_case("error") {
            return Err(io::Error::other("test disk-space failure"));
        }
        return step.parse::<u64>().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid {TEST_DISK_FREE_BYTES_ENV_VAR}: {e}"),
            )
        });
    }
    let probe = existing_probe_path(path);
    fs2::available_space(&probe)
}

/// Resolve the build volume for a cargo invocation. Prefers the
/// project's `target/` dir when it already exists (cargo will write
/// into it), falling back to the project's CWD when `target/` hasn't
/// been created yet. Callers pass `cwd` separately so tests can drive
/// both branches without changing the process CWD.
pub(crate) fn build_volume_path(cwd: &Path, target_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = target_dir {
        if dir.exists() {
            return dir.to_path_buf();
        }
    }
    cwd.to_path_buf()
}

/// Run the disk watchdog. Returns a [`DiskCheckOutcome`] describing
/// what (if anything) the caller should do.
pub(crate) fn check_disk_or_warn_or_block(workdir: &Path) -> DiskCheckOutcome {
    if !auto_prune_enabled() {
        return DiskCheckOutcome::Disabled;
    }
    let warn_gib = env_u64(WARN_FREE_GB_ENV_VAR).unwrap_or(DEFAULT_WARN_FREE_GB);
    let block_gib = env_u64(BLOCK_FREE_GB_ENV_VAR).unwrap_or(DEFAULT_BLOCK_FREE_GB);

    // Why: if the user inverts thresholds, prefer "block wins" so the
    // safety net is conservative rather than silently broken.
    let (warn_gib, block_gib) = if block_gib > warn_gib {
        (block_gib, block_gib)
    } else {
        (warn_gib, block_gib)
    };

    let free_bytes = match free_bytes_for(workdir) {
        Ok(b) => b,
        Err(_) => return DiskCheckOutcome::Disabled,
    };
    classify_outcome(free_bytes, warn_gib, block_gib)
}

fn classify_outcome(free_bytes: u64, warn_gib: u64, block_gib: u64) -> DiskCheckOutcome {
    let block_bytes = block_gib.saturating_mul(BYTES_PER_GIB);
    let warn_bytes = warn_gib.saturating_mul(BYTES_PER_GIB);
    if free_bytes < block_bytes {
        DiskCheckOutcome::Block {
            free_bytes,
            threshold_gib: block_gib,
        }
    } else if free_bytes < warn_bytes {
        DiskCheckOutcome::Warn {
            free_bytes,
            threshold_gib: warn_gib,
        }
    } else {
        DiskCheckOutcome::Ok { free_bytes }
    }
}

fn auto_prune_enabled() -> bool {
    match std::env::var_os(AUTO_PRUNE_ENABLED_ENV_VAR) {
        None => true,
        Some(value) => {
            let s = value.to_string_lossy();
            let t = s.trim();
            !(t.eq_ignore_ascii_case("0")
                || t.eq_ignore_ascii_case("false")
                || t.eq_ignore_ascii_case("no")
                || t.eq_ignore_ascii_case("off")
                || t.is_empty())
        }
    }
}

fn env_u64(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    raw.trim().parse::<u64>().ok()
}

fn existing_probe_path(path: &Path) -> PathBuf {
    let mut cursor = if path.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        path.to_path_buf()
    };
    loop {
        if cursor.exists() {
            return cursor;
        }
        if !cursor.pop() {
            return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        }
    }
}

/// Render the one-line stderr warning emitted when the watchdog fires
/// at the `Warn` tier.
pub(crate) fn render_warn_line(free_bytes: u64, threshold_gib: u64) -> String {
    format!(
        "soldr: warning: build volume has {free} free (< {threshold_gib} GiB threshold). Run `soldr gc target` to find reclaimable `target/` directories under ~/dev.",
        free = format_bytes(free_bytes),
    )
}

/// Render the multi-line stderr error emitted when the watchdog fires
/// at the `Block` tier. Caller is expected to bail with a non-zero
/// exit code after printing. Returned without a leading `soldr: `
/// prefix — the top-level `report_and_exit` helper adds it.
pub(crate) fn render_block_message(free_bytes: u64, threshold_gib: u64) -> String {
    format!(
        "build volume has only {free} free, below the {threshold_gib} GiB block threshold. \
         Run `soldr gc target --purge` to reclaim space from cross-repo `target/` directories, or set \
         SOLDR_TARGET_BLOCK_FREE_GB=<lower> / SOLDR_TARGET_AUTO_PRUNE_ENABLED=0 to override.",
        free = format_bytes(free_bytes),
    )
}

/// Test seam — when set, stands in for the real reclaim so the decision
/// logic can be unit-tested without a registry, a config, or deleting
/// anything. The reclaim itself is exercised by the auto-GC tests.
pub(crate) const TEST_RECLAIM_BYTES_ENV_VAR: &str = "SOLDR_TEST_RECLAIM_BYTES";

fn reclaim_bytes(volume_path: &Path) -> u64 {
    if let Some(raw) = std::env::var_os(TEST_RECLAIM_BYTES_ENV_VAR) {
        return raw.to_string_lossy().trim().parse::<u64>().unwrap_or(0);
    }
    crate::gc::auto::reclaim_target_dirs_for_block(volume_path)
}

/// Try to reclaim space before aborting the build, and report whether
/// aborting is still necessary (soldr#2134).
///
/// Returns `None` when the reclaim freed enough to proceed, or the
/// message to fail with when it did not. Splitting the decision here
/// rather than at the call site keeps the front door's arm a single
/// branch, and keeps every disk-threshold judgement in one module.
///
/// The hard block becomes the backstop it was meant to be: reached only
/// when there is genuinely nothing left to reclaim.
/// Reclaim proactively once free space crosses the *warn* threshold
/// (soldr#2134).
///
/// The issue's first defect: auto-prune fired on the wrong side of the
/// failure. The build aborted at the 5 GiB block threshold, prune then ran
/// and freed 57 GB, and the developer re-ran a command that soldr was
/// already capable of not failing. Disk pressure builds gradually, so there
/// is a large window between "getting tight" and "cannot build" in which
/// reclaiming is free.
///
/// Reclaiming here makes the hard block the backstop rather than the
/// trigger: by the time free space reaches 5 GiB, reclaim has already been
/// attempted, so blocking means there was genuinely nothing left to take.
///
/// Best-effort and never fatal -- the build continues either way. This
/// widens *when* reclaim runs, not *what* it may delete: the candidate set
/// is unchanged, so every staleness threshold and safety guard still
/// applies. Once the cold targets are gone this returns 0 and the warn line
/// is all that remains, so it converges rather than deleting on every build.
/// Emit the warn line and then reclaim. Both halves of the warn outcome in
/// one call, so the front door does not orchestrate disk policy -- and so
/// `cargo_front_door/mod.rs`, which is over the per-file line ceiling, swaps
/// one line for one line rather than growing.
pub(crate) fn warn_and_reclaim(volume_path: &Path, free_bytes: u64, threshold_gib: u64) {
    eprintln!("{}", render_warn_line(free_bytes, threshold_gib));
    reclaim_at_warn(volume_path, free_bytes);
}

pub(crate) fn reclaim_at_warn(volume_path: &Path, free_bytes: u64) {
    let reclaimed = reclaim_bytes(volume_path);
    if reclaimed == 0 {
        return;
    }
    let free_now = free_bytes_for(volume_path).unwrap_or(free_bytes);
    eprintln!(
        "soldr: free space is low; reclaimed {} from cross-repo `target/`          directories before it became a hard block, {} now free.",
        format_bytes(reclaimed),
        format_bytes(free_now)
    );
}

pub(crate) fn reclaim_then_block(
    volume_path: &Path,
    free_bytes: u64,
    threshold_gib: u64,
) -> Result<(), crate::core::SoldrError> {
    match reclaim_then_block_message(volume_path, free_bytes, threshold_gib) {
        Some(message) => Err(crate::core::SoldrError::Other(message)),
        None => Ok(()),
    }
}

/// The decision itself, split out so tests can assert on the message
/// without constructing an error.
pub(crate) fn reclaim_then_block_message(
    volume_path: &Path,
    free_bytes: u64,
    threshold_gib: u64,
) -> Option<String> {
    let reclaimed = reclaim_bytes(volume_path);
    if reclaimed == 0 {
        return Some(render_block_message(free_bytes, threshold_gib));
    }
    // Re-probe rather than adding `reclaimed` to the earlier reading:
    // other processes write to this volume too, and the question is how
    // much is free *now*, not how much this pass deleted.
    let free_now = match free_bytes_for(volume_path) {
        Ok(bytes) => bytes,
        // The probe worked moments ago, so a failure here is a genuine
        // unknown. Blocking on an unknown is the safe direction.
        Err(_) => return Some(render_block_message(free_bytes, threshold_gib)),
    };
    if free_now >= threshold_gib.saturating_mul(BYTES_PER_GIB) {
        eprintln!(
            "soldr: build volume was below the {threshold_gib} GiB block threshold; \
             reclaimed {} from cross-repo `target/` directories, {} now free. Continuing.",
            format_bytes(reclaimed),
            format_bytes(free_now)
        );
        return None;
    }
    Some(format!(
        "{} (already reclaimed {} from cross-repo `target/` directories, which was not enough)",
        render_block_message(free_now, threshold_gib),
        format_bytes(reclaimed),
    ))
}

fn format_bytes(bytes: u64) -> String {
    let gib = bytes as f64 / BYTES_PER_GIB as f64;
    if gib >= 1.0 {
        format!("{gib:.2} GiB")
    } else {
        let mib = bytes as f64 / (1024.0 * 1024.0);
        format!("{mib:.0} MiB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::sync::Mutex;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn threshold_classifier_returns_ok_above_warn() {
        let outcome = classify_outcome(
            20 * BYTES_PER_GIB,
            DEFAULT_WARN_FREE_GB,
            DEFAULT_BLOCK_FREE_GB,
        );
        assert!(matches!(outcome, DiskCheckOutcome::Ok { .. }));
    }

    #[test]
    fn threshold_classifier_returns_warn_between_thresholds() {
        let outcome = classify_outcome(
            7 * BYTES_PER_GIB,
            DEFAULT_WARN_FREE_GB,
            DEFAULT_BLOCK_FREE_GB,
        );
        match outcome {
            DiskCheckOutcome::Warn {
                threshold_gib,
                free_bytes,
            } => {
                assert_eq!(threshold_gib, DEFAULT_WARN_FREE_GB);
                assert_eq!(free_bytes, 7 * BYTES_PER_GIB);
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn threshold_classifier_returns_block_below_block() {
        let outcome = classify_outcome(
            2 * BYTES_PER_GIB,
            DEFAULT_WARN_FREE_GB,
            DEFAULT_BLOCK_FREE_GB,
        );
        match outcome {
            DiskCheckOutcome::Block {
                threshold_gib,
                free_bytes,
            } => {
                assert_eq!(threshold_gib, DEFAULT_BLOCK_FREE_GB);
                assert_eq!(free_bytes, 2 * BYTES_PER_GIB);
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn threshold_classifier_block_wins_at_block_boundary() {
        let outcome = classify_outcome(DEFAULT_BLOCK_FREE_GB * BYTES_PER_GIB - 1, 10, 5);
        assert!(matches!(outcome, DiskCheckOutcome::Block { .. }));
    }

    #[test]
    fn check_disk_disabled_via_env_var() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _disabled = EnvVarGuard::set(AUTO_PRUNE_ENABLED_ENV_VAR, "0");
        let _free = EnvVarGuard::set(TEST_DISK_FREE_BYTES_ENV_VAR, "0");
        let outcome = check_disk_or_warn_or_block(std::path::Path::new("."));
        assert!(matches!(outcome, DiskCheckOutcome::Disabled));
    }

    #[test]
    fn check_disk_warns_with_custom_threshold() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _enabled = EnvVarGuard::remove(AUTO_PRUNE_ENABLED_ENV_VAR);
        let _warn = EnvVarGuard::set(WARN_FREE_GB_ENV_VAR, "20");
        let _block = EnvVarGuard::set(BLOCK_FREE_GB_ENV_VAR, "5");
        let _free = EnvVarGuard::set(
            TEST_DISK_FREE_BYTES_ENV_VAR,
            &(15u64 * BYTES_PER_GIB).to_string(),
        );
        let outcome = check_disk_or_warn_or_block(std::path::Path::new("."));
        match outcome {
            DiskCheckOutcome::Warn { threshold_gib, .. } => assert_eq!(threshold_gib, 20),
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn check_disk_blocks_with_custom_threshold() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _enabled = EnvVarGuard::remove(AUTO_PRUNE_ENABLED_ENV_VAR);
        let _warn = EnvVarGuard::set(WARN_FREE_GB_ENV_VAR, "20");
        let _block = EnvVarGuard::set(BLOCK_FREE_GB_ENV_VAR, "10");
        let _free = EnvVarGuard::set(
            TEST_DISK_FREE_BYTES_ENV_VAR,
            &(3u64 * BYTES_PER_GIB).to_string(),
        );
        let outcome = check_disk_or_warn_or_block(std::path::Path::new("."));
        match outcome {
            DiskCheckOutcome::Block { threshold_gib, .. } => assert_eq!(threshold_gib, 10),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn check_disk_disabled_when_probe_errors() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _enabled = EnvVarGuard::remove(AUTO_PRUNE_ENABLED_ENV_VAR);
        let _free = EnvVarGuard::set(TEST_DISK_FREE_BYTES_ENV_VAR, "error");
        let outcome = check_disk_or_warn_or_block(std::path::Path::new("."));
        assert!(matches!(outcome, DiskCheckOutcome::Disabled));
    }

    #[test]
    fn inverted_thresholds_collapse_to_block_value() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _enabled = EnvVarGuard::remove(AUTO_PRUNE_ENABLED_ENV_VAR);
        // warn=5, block=10 (inverted) — should collapse to a single
        // block-only check at 10 GiB.
        let _warn = EnvVarGuard::set(WARN_FREE_GB_ENV_VAR, "5");
        let _block = EnvVarGuard::set(BLOCK_FREE_GB_ENV_VAR, "10");
        let _free = EnvVarGuard::set(
            TEST_DISK_FREE_BYTES_ENV_VAR,
            &(7u64 * BYTES_PER_GIB).to_string(),
        );
        let outcome = check_disk_or_warn_or_block(std::path::Path::new("."));
        assert!(matches!(outcome, DiskCheckOutcome::Block { .. }));
    }

    #[test]
    fn build_volume_path_prefers_target_when_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path();
        let target = cwd.join("target");
        std::fs::create_dir_all(&target).expect("create target");
        let resolved = build_volume_path(cwd, Some(&target));
        assert_eq!(resolved, target);
    }

    #[test]
    fn build_volume_path_falls_back_to_cwd_when_target_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path();
        let target = cwd.join("target");
        let resolved = build_volume_path(cwd, Some(&target));
        assert_eq!(resolved, cwd);
    }

    #[test]
    fn warn_line_includes_threshold_and_command() {
        let line = render_warn_line(7 * BYTES_PER_GIB, 10);
        assert!(line.contains("7.00 GiB"));
        assert!(line.contains("10 GiB"));
        assert!(line.contains("soldr gc target"));
    }

    #[test]
    fn block_line_directs_user_at_purge() {
        let msg = render_block_message(2 * BYTES_PER_GIB, 5);
        assert!(msg.contains("2.00 GiB"));
        assert!(msg.contains("5 GiB"));
        assert!(msg.contains("soldr gc target --purge"));
        assert!(msg.contains("SOLDR_TARGET_BLOCK_FREE_GB"));
        // Why: `report_and_exit` already prepends "soldr: " — the
        // rendered message must not double it.
        assert!(!msg.starts_with("soldr:"));
    }

    // soldr#2134. The reclaim mechanism was never broken — it ran on the
    // wrong side of the failure. These cover the three ways the new
    // decision can go.

    /// Drive the front door's real sequence: the watchdog probes once
    /// to decide, then the reclaim probes again to see whether it
    /// helped. Going through both is the point -- a test that calls the
    /// decision function directly consumes the *first* sequence entry
    /// as its re-probe and then silently asserts the wrong thing.
    fn block_then_reclaim(free_sequence: &str, reclaimed: u64) -> Option<String> {
        reset_test_disk_probe_cursor();
        let _free = EnvVarGuard::set(TEST_DISK_FREE_BYTES_ENV_VAR, free_sequence);
        let _reclaimed = EnvVarGuard::set(TEST_RECLAIM_BYTES_ENV_VAR, &reclaimed.to_string());
        let _block = EnvVarGuard::set(BLOCK_FREE_GB_ENV_VAR, "5");
        let _enabled = EnvVarGuard::remove(AUTO_PRUNE_ENABLED_ENV_VAR);
        let path = std::path::Path::new(".");
        match check_disk_or_warn_or_block(path) {
            DiskCheckOutcome::Block {
                free_bytes,
                threshold_gib,
            } => reclaim_then_block_message(path, free_bytes, threshold_gib),
            other => panic!("fixture must reach the block tier, got {other:?}"),
        }
    }

    #[test]
    fn a_reclaim_that_frees_enough_lets_the_build_proceed() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 3 GiB free at the block, 40 GiB once 37 GiB is reclaimed.
        let outcome = block_then_reclaim(
            &format!("{},{}", 3 * BYTES_PER_GIB, 40 * BYTES_PER_GIB),
            37 * BYTES_PER_GIB,
        );
        assert_eq!(
            outcome, None,
            "the build must not fail for a condition soldr just resolved"
        );
    }

    #[test]
    fn a_reclaim_that_frees_nothing_still_blocks() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let msg = block_then_reclaim(&(3 * BYTES_PER_GIB).to_string(), 0).expect("must block");
        assert!(msg.contains("soldr gc target --purge"));
        assert!(
            !msg.contains("already reclaimed"),
            "nothing was reclaimed, so the message must not claim otherwise: {msg}"
        );
    }

    #[test]
    fn a_partial_reclaim_blocks_but_says_it_tried() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Freed 1 GiB, leaving 4 GiB -- still under the 5 GiB bar.
        let msg = block_then_reclaim(
            &format!("{},{}", 3 * BYTES_PER_GIB, 4 * BYTES_PER_GIB),
            BYTES_PER_GIB,
        )
        .expect("must still block");
        assert!(
            msg.contains("already reclaimed"),
            "a user who just lost a GiB of caches deserves to know it happened: {msg}"
        );
        // The re-probed figure, not the stale pre-reclaim one.
        assert!(msg.contains("4.00 GiB"), "{msg}");
    }

    #[test]
    fn a_single_probe_value_still_repeats_for_every_call() {
        // Regression guard for the sequence seam: every pre-existing
        // test passes one value and expects it on every probe.
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_test_disk_probe_cursor();
        let _free = EnvVarGuard::set(
            TEST_DISK_FREE_BYTES_ENV_VAR,
            &(7 * BYTES_PER_GIB).to_string(),
        );
        let path = std::path::Path::new(".");
        for _ in 0..4 {
            assert_eq!(free_bytes_for(path).unwrap(), 7 * BYTES_PER_GIB);
        }
    }
}
