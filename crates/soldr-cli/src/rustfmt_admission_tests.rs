use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// Fixture controller backed by a counting semaphore (`Mutex<usize>` +
/// `Condvar`): `acquire` blocks until a slot under `capacity` is free, and
/// hands back a receipt that `release` returns to the pool.
struct CountingSemaphoreController {
    capacity: usize,
    state: Mutex<usize>,
    condvar: Condvar,
}

impl CountingSemaphoreController {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(0),
            condvar: Condvar::new(),
        }
    }
}

impl RustfmtLeaseController for CountingSemaphoreController {
    type Lease = ();

    fn acquire(&self) -> Option<Self::Lease> {
        let mut held = self.state.lock().unwrap();
        while *held >= self.capacity {
            held = self.condvar.wait(held).unwrap();
        }
        *held += 1;
        Some(())
    }

    fn release(&self, lease: Option<Self::Lease>) {
        let Some(()) = lease else {
            return;
        };
        let mut held = self.state.lock().unwrap();
        *held -= 1;
        self.condvar.notify_one();
    }
}

/// Fixture controller whose `acquire` always returns `None` immediately --
/// no blocking, no admission at all. Documents the pre-fix fan-out shape:
/// with no lease, concurrency is bounded only by however many threads the
/// caller spawns.
struct NoLeaseController;

impl RustfmtLeaseController for NoLeaseController {
    type Lease = ();

    fn acquire(&self) -> Option<Self::Lease> {
        None
    }

    fn release(&self, _lease: Option<Self::Lease>) {}
}

/// RED -> GREEN concurrency proof: eight threads all guarded by a
/// capacity-2 controller must never see more than 2 active at once, and
/// with 8 threads and blocking acquisition at least 1 must be observed
/// active at some point.
#[test]
fn with_rustfmt_lease_bounds_concurrency_to_the_controllers_capacity() {
    let controller = CountingSemaphoreController::new(2);
    let active = AtomicUsize::new(0);
    let max_active = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                with_rustfmt_lease(&controller, || {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            });
        }
    });

    let observed = max_active.load(Ordering::SeqCst);
    assert!(
        observed <= 2,
        "observed {observed} concurrently active, expected <= 2"
    );
    assert!(
        observed >= 1,
        "expected at least one active run to be observed"
    );
}

/// Companion RED shape: a controller whose `acquire` never actually admits
/// anything (always `None`, no blocking) lets concurrency exceed the
/// capacity a real lease would have enforced -- this is the fan-out the fix
/// closes.
#[test]
fn a_controller_that_never_admits_does_not_bound_concurrency() {
    let controller = NoLeaseController;
    let active = AtomicUsize::new(0);
    let max_active = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                with_rustfmt_lease(&controller, || {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            });
        }
    });

    let observed = max_active.load(Ordering::SeqCst);
    assert!(
        observed > 2,
        "expected the unbounded fixture to exceed capacity 2, observed {observed}"
    );
}

/// Fixture that records how many times `acquire`/`release` ran, for the
/// exactly-once release proofs below.
struct RecordingController {
    acquire_calls: std::sync::atomic::AtomicUsize,
    release_calls: std::sync::atomic::AtomicUsize,
}

impl RecordingController {
    fn new() -> Self {
        Self {
            acquire_calls: std::sync::atomic::AtomicUsize::new(0),
            release_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl RustfmtLeaseController for RecordingController {
    type Lease = u32;

    fn acquire(&self) -> Option<Self::Lease> {
        self.acquire_calls.fetch_add(1, Ordering::SeqCst);
        Some(7)
    }

    fn release(&self, _lease: Option<Self::Lease>) {
        self.release_calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn release_runs_exactly_once_on_a_normal_return() {
    let controller = RecordingController::new();

    let result = with_rustfmt_lease(&controller, || 42);

    assert_eq!(result, 42);
    assert_eq!(controller.acquire_calls.load(Ordering::SeqCst), 1);
    assert_eq!(controller.release_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn release_runs_exactly_once_when_the_closure_returns_err() {
    let controller = RecordingController::new();

    let result: Result<i32, &str> = with_rustfmt_lease(&controller, || Err("boom"));

    assert_eq!(result, Err("boom"));
    assert_eq!(controller.acquire_calls.load(Ordering::SeqCst), 1);
    assert_eq!(controller.release_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn release_runs_exactly_once_when_the_closure_panics() {
    let controller = RecordingController::new();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_rustfmt_lease(&controller, || {
            panic!("fixture panic");
        })
    }));

    assert!(result.is_err());
    assert_eq!(controller.acquire_calls.load(Ordering::SeqCst), 1);
    assert_eq!(controller.release_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn acquire_returning_none_still_runs_the_closure() {
    let controller = NoLeaseController;
    let ran = std::cell::Cell::new(false);

    with_rustfmt_lease(&controller, || {
        ran.set(true);
    });

    assert!(ran.get());
}

fn unique_temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "soldr-rustfmt-admission-test-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn restore_if_damaged_rewrites_a_truncated_file_after_failure() {
    let dir = unique_temp_dir("truncate");
    let file = dir.join("a.rs");
    std::fs::write(&file, b"fn main() {}\n").unwrap();

    let snapshot = SourceSnapshot::capture(
        &[
            "--edition".to_string(),
            "2021".to_string(),
            "a.rs".to_string(),
        ],
        &dir,
    );

    // Simulate a formatter that truncated the file mid-write.
    std::fs::write(&file, b"").unwrap();

    let restored = snapshot.restore_if_damaged(false).unwrap();

    assert_eq!(restored, vec![file.clone()]);
    assert_eq!(std::fs::read(&file).unwrap(), b"fn main() {}\n");
}

#[test]
fn restore_if_damaged_does_nothing_when_succeeded_is_true() {
    let dir = unique_temp_dir("succeeded");
    let file = dir.join("a.rs");
    std::fs::write(&file, b"fn main() {}\n").unwrap();

    let snapshot = SourceSnapshot::capture(&["a.rs".to_string()], &dir);

    std::fs::write(&file, b"").unwrap();

    let restored = snapshot.restore_if_damaged(true).unwrap();

    assert!(restored.is_empty());
    assert_eq!(std::fs::read(&file).unwrap(), b"");
}

#[test]
fn restore_if_damaged_leaves_a_legitimately_reformatted_file_alone() {
    let dir = unique_temp_dir("reformatted");
    let file = dir.join("a.rs");
    std::fs::write(&file, b"fn main(){}\n").unwrap();

    let snapshot = SourceSnapshot::capture(&["a.rs".to_string()], &dir);

    // A legitimate rustfmt rewrite: still non-empty, just reformatted.
    std::fs::write(&file, b"fn main() {}\n").unwrap();

    let restored = snapshot.restore_if_damaged(false).unwrap();

    assert!(restored.is_empty());
    assert_eq!(std::fs::read(&file).unwrap(), b"fn main() {}\n");
}

#[test]
fn config_path_value_is_not_captured_as_a_source_file() {
    let dir = unique_temp_dir("config-path");
    // A file literally named x.rs that is only ever used as a flag value,
    // never a positional source arg, must not be captured.
    let flag_value_file = dir.join("x.rs");
    std::fs::write(&flag_value_file, b"not really rust source\n").unwrap();

    let snapshot =
        SourceSnapshot::capture(&["--config-path".to_string(), "x.rs".to_string()], &dir);

    assert!(snapshot.files.is_empty());
}

mod daemon_start {
    use super::super::{acquire_starting_daemon, lease_unavailable_message};
    use crate::daemon::client::ClientError;
    use std::cell::Cell;

    /// soldr#3855 RED->GREEN: with no daemon running, the first request
    /// reports `NotRunning`; the controller must start the daemon and get
    /// the lease on the retry instead of formatting without admission.
    #[test]
    fn not_running_starts_daemon_then_acquires() {
        let attempts = Cell::new(0);
        let starts = Cell::new(0);
        let lease = acquire_starting_daemon(
            || {
                attempts.set(attempts.get() + 1);
                if starts.get() == 0 {
                    Err(ClientError::NotRunning)
                } else {
                    Ok("lease")
                }
            },
            || {
                starts.set(starts.get() + 1);
                Ok(())
            },
        );
        assert_eq!(lease, Ok("lease"));
        assert_eq!(attempts.get(), 2);
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn running_daemon_is_used_without_a_start() {
        let starts = Cell::new(0);
        let lease = acquire_starting_daemon(
            || Ok::<_, ClientError>(7),
            || {
                starts.set(starts.get() + 1);
                Ok(())
            },
        );
        assert_eq!(lease, Ok(7));
        assert_eq!(starts.get(), 0);
    }

    #[test]
    fn failed_start_reports_the_cause() {
        let attempts = Cell::new(0);
        let lease = acquire_starting_daemon(
            || {
                attempts.set(attempts.get() + 1);
                Err::<(), _>(ClientError::NotRunning)
            },
            || Err("broker unreachable".to_string()),
        );
        let reason = lease.unwrap_err();
        assert!(
            reason.contains("starting one failed: broker unreachable"),
            "{reason}"
        );
        assert_eq!(attempts.get(), 1, "no retry after a failed start");
    }

    #[test]
    fn retry_failure_after_start_is_reported() {
        let reason = acquire_starting_daemon(|| Err::<(), _>(ClientError::NotRunning), || Ok(()))
            .unwrap_err();
        assert!(reason.contains("started soldr-daemon but"), "{reason}");
    }

    /// A daemon that answers with a fault is not a missing daemon: starting
    /// another would not help and must not be attempted.
    #[test]
    fn other_errors_never_start_a_daemon() {
        let starts = Cell::new(0);
        let reason = acquire_starting_daemon(
            || Err::<(), _>(ClientError::Protocol("refused".to_string())),
            || {
                starts.set(starts.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(starts.get(), 0);
        assert!(reason.contains("refused"), "{reason}");
    }

    #[test]
    fn unavailable_message_names_recovery_and_continuation() {
        let plain = lease_unavailable_message("why", false);
        assert!(plain.contains("ERROR"));
        assert!(plain.contains("why"));
        assert!(plain.contains("WITHOUT daemon admission protection"));
        assert!(plain.contains("soldr daemon stop && soldr daemon start"));
        assert!(plain.contains("soldr logs paths"));
        assert!(!plain.contains('\x1b'));
        assert!(lease_unavailable_message("why", true).starts_with("\x1b[1;31m"));
    }
}
