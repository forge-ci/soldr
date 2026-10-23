//! Routes every cached rustfmt child launched by `soldr cargo fmt` through
//! the daemon's EXISTING resident-capacity admission (soldr#2877).
//!
//! # Why this exists
//!
//! `cargo fmt` re-enters soldr via the `RUSTFMT` shim
//! (`crate::toolchain::run_rustfmt`), and each formatter child is a plain
//! `rustfmt` process -- never a `CompileRequest`. zccache's compile
//! admission (`crates/soldr-daemon/src/resident_compile_admission.rs`) only
//! ever sees `CompileRequest`s, so it has never seen rustfmt at all: a wide
//! `cargo fmt` fan-out (every crate's formatter launched concurrently by
//! Cargo) is completely invisible to the daemon's existing scheduling, and
//! on a memory-constrained worker that fan-out can OOM the host or race with
//! a concurrent compiler admission for the same resources -- in the worst
//! case truncating a source file rustfmt was mid-write on.
//!
//! This module closes that gap without a wire/protocol change and without a
//! global job cap (see CLAUDE.md's "Concurrency caps are a last resort" --
//! `CARGO_BUILD_JOBS` / `SOLDR_JOBS` are never read, set, or modified here).
//! Each rustfmt child now holds [`RUSTFMT_RESIDENT_PERMITS`] permit(s) of the
//! daemon's *existing* compile-capacity semaphore
//! (`crate::daemon::client::acquire_resident_capacity`, the same mechanism
//! `crate::ci_test::nextest_resident_lease` uses for Nextest EXECUTION) for
//! its whole lifetime. Concurrent formatter fan-out is therefore bounded by
//! the same effective capacity `crates/soldr-core/src/core/jobs.rs` already
//! resolves for compiler admission -- rustfmt and rustc children now compete
//! for the one capacity the daemon tracks, instead of rustfmt running
//! entirely outside that accounting.
//!
//! When no daemon is running the controller starts the broker-owned one and
//! retries (soldr#3855). If that still fails, it prints a loud error naming
//! the recovery steps, but it must never fail formatting, so [`RustfmtLeaseController::
//! acquire`] returning `None` still runs the formatter -- it only means the
//! admission signal was unavailable, not that formatting itself failed.
//!
//! # Source-integrity guard
//!
//! Bounding concurrency reduces the chance of a truncated file but does not
//! eliminate it (a killed worker, an OOM signal landing mid-write, a crashed
//! rustfmt child). [`SourceSnapshot`] captures the bytes of every `.rs` file
//! rustfmt was given as an explicit CLI argument *before* it runs, and
//! [`SourceSnapshot::restore_if_damaged`] rewrites any of those files that
//! come back empty (or unreadable) after a failed run, atomically (temp file
//! + rename) so a reader never observes a partial write. Module files that
//! rustfmt discovers on its own beyond the explicit arguments are **not**
//! covered here -- `cargo fmt` always passes each target's root file
//! explicitly, so the explicit-argument set is exactly the set this guard
//! needs to protect.

/// Permits reserved from the daemon's compile-capacity semaphore for the
/// lifetime of a single cached rustfmt child.
///
/// One permit mirrors `crate::ci_test::nextest_resident_lease::
/// NEXTEST_RESIDENT_LEASE_PERMITS`'s conservative choice: it declares
/// rustfmt's resident footprint to the admission system without starving
/// concurrent compiler admissions of the same capacity
/// (`crates/soldr-core/src/core/jobs.rs` resolves the shared ceiling; this
/// module never reads or overrides it).
pub(crate) const RUSTFMT_RESIDENT_PERMITS: u32 = 1;

/// Abstraction over the daemon's resident-capacity lease so the acquire/
/// release scoping around a rustfmt child is unit-testable without a
/// running daemon. Production uses [`DaemonRustfmtLeaseController`]; tests
/// substitute a fixture that records calls.
pub(crate) trait RustfmtLeaseController {
    type Lease;

    /// Acquire before the rustfmt child is spawned. `None` means "proceed
    /// without a lease": the lease is a best-effort resource-contention
    /// mitigation, not a correctness requirement, so a daemon that is
    /// unreachable or refuses the reservation must not fail formatting.
    fn acquire(&self) -> Option<Self::Lease>;

    /// Release exactly once, on every exit path of the child it guarded --
    /// success, formatter failure, and spawn/poll error alike.
    fn release(&self, lease: Option<Self::Lease>);
}

/// Real daemon-backed lease controller.
pub(crate) struct DaemonRustfmtLeaseController {
    pub(crate) permits: u32,
}

impl RustfmtLeaseController for DaemonRustfmtLeaseController {
    type Lease = crate::daemon::client::ResidentCapacityLease;

    fn acquire(&self) -> Option<Self::Lease> {
        let paths = match crate::core::SoldrPaths::new() {
            Ok(paths) => paths,
            Err(error) => {
                report_lease_unavailable(&format!("could not resolve daemon paths: {error}"));
                return None;
            }
        };
        let sock = crate::daemon::client::default_sock_path(&paths);
        let acquired = acquire_starting_daemon(
            || crate::daemon::client::acquire_resident_capacity(&sock, self.permits),
            || {
                crate::soldr_main::start_daemon_route(crate::soldr_main::DAEMON_START_ROUTE_BUDGET)
                    .map_err(|error| error.to_string())
            },
        );
        match acquired {
            Ok(lease) => {
                if crate::core::flag("SOLDR_RUSTFMT_ADMISSION_TRACE") {
                    eprintln!(
                        "soldr rustfmt: admitted with {} resident-capacity permit(s) (source: \
                         daemon compile capacity)",
                        lease.permits()
                    );
                }
                Some(lease)
            }
            Err(reason) => {
                report_lease_unavailable(&reason);
                None
            }
        }
    }

    fn release(&self, lease: Option<Self::Lease>) {
        let Some(lease) = lease else {
            return;
        };
        if let Err(error) = lease.finish() {
            eprintln!(
                "soldr rustfmt: resident-capacity lease release did not receive a daemon \
                 acknowledgement (soldr#2877): {error:?}"
            );
        }
    }
}

/// Reserve the lease, starting the broker-owned daemon first when none is
/// running (soldr#3855). The broker reaps an idle daemon once every
/// requester exits, so between builds `NotRunning` is the normal state, and
/// without a start every such formatter ran outside admission. Only
/// `NotRunning` triggers a start: any other error is a real daemon fault, and
/// waiting behind compile capacity is not an error at all.
pub(crate) fn acquire_starting_daemon<L>(
    mut try_acquire: impl FnMut() -> Result<L, crate::daemon::client::ClientError>,
    start_daemon: impl FnOnce() -> Result<(), String>,
) -> Result<L, String> {
    match try_acquire() {
        Ok(lease) => Ok(lease),
        Err(crate::daemon::client::ClientError::NotRunning) => {
            start_daemon().map_err(|error| {
                format!("no soldr-daemon was running and starting one failed: {error}")
            })?;
            try_acquire().map_err(|error| {
                format!("started soldr-daemon but the lease request still failed: {error:?}")
            })
        }
        Err(error) => Err(format!("the daemon refused the lease request: {error:?}")),
    }
}

/// Loud, actionable notice that this formatter runs without admission
/// (soldr#3815). Formatting still proceeds and keeps its own exit status.
fn report_lease_unavailable(reason: &str) {
    use std::io::IsTerminal as _;
    eprintln!(
        "{}",
        lease_unavailable_message(reason, std::io::stderr().is_terminal())
    );
}

pub(crate) fn lease_unavailable_message(reason: &str, color: bool) -> String {
    let text = format!(
        "soldr rustfmt: ERROR: could not reserve the resident-capacity lease (soldr#2877): \
         {reason}\n\
         soldr rustfmt: formatting continues WITHOUT daemon admission protection. Recover with:\n\
         soldr rustfmt:   soldr daemon status\n\
         soldr rustfmt:   soldr daemon stop && soldr daemon start\n\
         soldr rustfmt:   soldr logs paths   (daemon and broker logs)"
    );
    if color {
        format!("\x1b[1;31m{text}\x1b[0m")
    } else {
        text
    }
}

/// Drop guard that releases a controller's lease exactly once, on every exit
/// path of the closure it wraps -- including a panic unwinding through it.
struct LeaseReleaseGuard<'a, C: RustfmtLeaseController> {
    controller: &'a C,
    lease: Option<C::Lease>,
}

impl<'a, C: RustfmtLeaseController> Drop for LeaseReleaseGuard<'a, C> {
    fn drop(&mut self) {
        self.controller.release(self.lease.take());
    }
}

/// Runs `run` (spawning + waiting on a rustfmt child) with the
/// resident-capacity lease held for its whole duration, released
/// unconditionally afterward regardless of the outcome `run` produces --
/// including a panic, via a drop guard.
pub(crate) fn with_rustfmt_lease<C: RustfmtLeaseController, T>(
    controller: &C,
    run: impl FnOnce() -> T,
) -> T {
    let lease = controller.acquire();
    let _guard = LeaseReleaseGuard { controller, lease };
    run()
}

/// Snapshot of the pre-run bytes of every `.rs` file passed to rustfmt as an
/// explicit CLI argument, used to detect and repair a truncated write after
/// a failed formatter run. Files rustfmt discovers on its own beyond the
/// explicit arguments (module files reached via `mod` declarations) are not
/// covered -- `cargo fmt` always passes each target's root file explicitly.
pub(crate) struct SourceSnapshot {
    files: Vec<(std::path::PathBuf, Vec<u8>)>,
}

/// CLI flags whose value is a single following argument, never a source
/// file path, so the value must be skipped rather than captured.
const VALUE_TAKING_FLAGS: &[&str] = &[
    "--config",
    "--config-path",
    "--edition",
    "--emit",
    "--color",
    "--style-edition",
];

impl SourceSnapshot {
    /// Capture the bytes of every argument that looks like a `.rs` source
    /// path: it does not start with `-`, ends in `.rs`, and resolves (via
    /// `cwd` for relative paths) to an existing regular file. Values that
    /// follow a flag in [`VALUE_TAKING_FLAGS`] are skipped even if they
    /// happen to end in `.rs`.
    pub(crate) fn capture(args: &[String], cwd: &std::path::Path) -> SourceSnapshot {
        let mut files = Vec::new();
        let mut skip_next = false;
        for arg in args {
            if skip_next {
                skip_next = false;
                continue;
            }
            if VALUE_TAKING_FLAGS.contains(&arg.as_str()) {
                skip_next = true;
                continue;
            }
            if arg.starts_with('-') || !arg.ends_with(".rs") {
                continue;
            }
            let path = std::path::Path::new(arg);
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            let Ok(metadata) = std::fs::symlink_metadata(&resolved) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Ok(bytes) = std::fs::read(&resolved) else {
                continue;
            };
            files.push((resolved, bytes));
        }
        SourceSnapshot { files }
    }

    /// When `succeeded` is `false`, rewrite (atomically) every captured file
    /// whose current content is empty while the snapshot was non-empty, or
    /// that cannot currently be read at all. Returns the paths that were
    /// restored. When `succeeded` is `true`, does nothing and returns an
    /// empty vector -- a legitimate, non-empty rewrite is never touched.
    pub(crate) fn restore_if_damaged(
        &self,
        succeeded: bool,
    ) -> std::io::Result<Vec<std::path::PathBuf>> {
        let mut restored = Vec::new();
        if succeeded {
            return Ok(restored);
        }
        for (path, original) in &self.files {
            if original.is_empty() {
                continue;
            }
            let damaged = match std::fs::read(path) {
                Ok(current) => current.is_empty(),
                Err(_) => true,
            };
            if !damaged {
                continue;
            }
            let tmp_path = {
                let mut name = path
                    .file_name()
                    .map(|n| n.to_os_string())
                    .unwrap_or_default();
                name.push(format!(".soldr-rustfmt-restore.{}", std::process::id()));
                path.with_file_name(name)
            };
            std::fs::write(&tmp_path, original)?;
            std::fs::rename(&tmp_path, path)?;
            eprintln!(
                "soldr rustfmt: restored {} after formatter failure (soldr#2877)",
                path.display()
            );
            restored.push(path.clone());
        }
        Ok(restored)
    }
}

#[cfg(test)]
#[path = "rustfmt_admission_tests.rs"]
mod tests;
