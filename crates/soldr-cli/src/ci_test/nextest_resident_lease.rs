//! Activates the daemon's weighted resident-capacity lease around Fresh
//! Nextest EXECUTION only (soldr#2878's known-open item, filed against
//! soldr#2349/soldr#2878's own dylint-cache-skip regression).
//!
//! # Why this exists
//!
//! soldr#2349 taught `soldr ci-test` to skip the six `dylint-library-*`
//! stages on a warm tree. That is correct on its own, but it also removed
//! roughly five minutes of serialized work that had been accidentally
//! staggering the DAG: two consecutive warm CI runs then failed under
//! resource contention on a standard `ubuntu-24.04` GitHub-hosted runner
//! (4 logical CPUs, 16 GiB RAM) --
//!
//! - a nested-cargo Nextest test (`cli_cargo_doc_routes::
//!   bare_doc_keeps_rustc_wrapped_but_rustdoc_direct`) timed out at 77s;
//! - `dylint-test-ban_raw_network_access` exited 101 first, and Nextest was
//!   then killed as collateral. What is established from reading the code
//!   (not run, per this task's constraints):
//!   1. No daemon code sheds an already-running compiler process for memory
//!      pressure -- `ResidentCompileAdmission::acquire_resident`
//!      (`crates/soldr-daemon/src/resident_compile_admission.rs`) only ever
//!      gates *new* admissions, so the daemon cannot have deliberately
//!      killed anything here. That is exactly why the lease -- an input the
//!      admission system can act on before the fact -- is the missing
//!      piece, not a workaround for some other shedding path.
//!   2. The generic signal diagnostic
//!      (`crates/soldr-daemon/src/compiler_exit.rs::signal_diagnostic`)
//!      only formats an *already observed* signal termination; it never
//!      decides to send one.
//!   3. The recorded cgroup evidence for that run
//!      (`oom_kill+oom_group_kill=0` at 13.10 GiB of 16 GiB) rules out the
//!      kernel's cgroup OOM killer as the sender.
//!   4. `execute.rs`'s own sibling-failure cancellation
//!      (`cancel_running`/`cancel_child`, which calls
//!      `kill_cargo_process_tree` -> `terminate_tree` --
//!      `crates/soldr-platform/src/platform_linux/process/terminate.rs`,
//!      SIGTERM then SIGKILL to the whole process group) fully explains
//!      **Nextest's own** collateral death once `ban_raw_network_access`
//!      failed.
//!
//!   What is *not* established by reading alone: whether the SIGTERM to the
//!   compiler compiling `compiletest_rs` (attributed to the UI-test
//!   dependency compile) landed *before* `ban_raw_network_access`'s exit
//!   101 -- in which case it caused the 101 rather than resulted from it --
//!   or after, as executor collateral. The observed ordering
//!   ("`ban_raw_network_access` exited 101 first, Nextest killed as
//!   collateral then") is consistent with either reading once a compiler
//!   subprocess restarts or retries are considered, and this task is
//!   read-only, so the *first* SIGTERM's sender is left undetermined here.
//!   A runner-level memory manager (outside the cgroup this evidence reads)
//!   remains a live candidate precisely because the kernel cgroup OOM
//!   killer is ruled out. Either way, the daemon's compiler-admission
//!   semaphore has never accounted for Nextest's own resident test
//!   processes -- only for compiler processes -- so nothing in the DAG
//!   throttled the combination, and closing that gap is this module's job
//!   regardless of which reading of the collateral kill is correct.
//!
//! The daemon has carried a weighted resident-capacity lease
//! (`crates/soldr-daemon/src/daemon/client.rs::acquire_resident_capacity`,
//! `crates/soldr-daemon/src/resident_compile_admission.rs::
//! ResidentCompileAdmission::acquire_resident`) since the soldr#3037
//! family, but nothing in `soldr-cli` ever called it. This module is the
//! first caller: it reserves a slice of the daemon's compiler-admission
//! semaphore for the lifetime of the Nextest EXECUTION stage, so the
//! daemon's ordinary cache-miss compiler admission (which nested fixture
//! compiles launched *inside* test bodies also go through) sees fewer
//! available slots while Nextest's resident test processes are running --
//! without ever imposing a global Cargo job cap (see CLAUDE.md's "Trap
//! unexpected Cargo calls during Nextest" and "Cgroup memory observations
//! remain telemetry" rules: `CARGO_BUILD_JOBS` / `SOLDR_JOBS` stay exactly
//! as the caller set them, unset or not).
//!
//! # Scope: EXECUTION only, never `nextest-compile`
//!
//! `nextest-compile` is a pure compile stage; the daemon's existing
//! shared/exclusive compiler admission already accounts for it like any
//! other compiler unit. Only the EXECUTION stage introduces resident test
//! *processes* the admission semaphore cannot see on its own. Since
//! soldr#3446 both stages run in one peer chain beside Dylint, so the scope
//! is enforced per stage: `execute.rs`'s `NextestExecutionHooks` acquires
//! through [`acquire_for_stage`], whose name gate yields nothing for
//! `nextest-compile`, and releases when the `nextest` stage exits.
//! `run_parallel_nextest_compile_and_dylint_compile` (the compile-only
//! archive path) never references [`ResidentLeaseController`] at all.

use crate::core::SoldrError;

/// Permits reserved from the daemon's compiler-admission semaphore for the
/// duration of Nextest EXECUTION.
///
/// The semaphore's capacity is `crate::core::jobs::default_compile_jobs()`
/// when `CARGO_BUILD_JOBS`/`SOLDR_JOBS` are unset (the `ci-test` lane's
/// normal case, per CLAUDE.md) -- `available_parallelism() - 1`, floored at
/// one. On the standard `ubuntu-24.04` GitHub-hosted runner (4 logical CPUs)
/// that resolves to 3, and `ResidentCompileAdmission::acquire_resident`
/// refuses any reservation `>= max` so at least one compiler slot always
/// remains (`crates/soldr-daemon/src/resident_compile_admission.rs`).
///
/// One permit is deliberately conservative rather than the 2-permit ceiling
/// that capacity allows: this stage runs concurrently with the Dylint
/// branch (`run_nextest_and_dylint_branches`), and Nextest's own
/// test bodies launch nested compiler fixtures (the
/// `cli_cargo_doc_routes` / maturin class) that must still be able to get
/// a compiler slot promptly. Warm run 1's regression was exactly one of
/// those nested fixtures timing out at 77s under a shrunken admission
/// window -- trading that OOM for fixture timeouts would not be a fix.
/// Reserving 1 permit leaves 2 of 3 slots free at all times during
/// EXECUTION: real headroom, while still declaring Nextest's resident
/// footprint to the admission system instead of leaving it invisible.
pub(super) const NEXTEST_RESIDENT_LEASE_PERMITS: u32 = 1;

/// Abstraction over the daemon's resident-capacity lease so the acquire/
/// release scoping around Nextest EXECUTION is unit-testable without a
/// running daemon. Production uses [`DaemonResidentLeaseController`]; tests
/// substitute a fixture that records calls.
pub(super) trait ResidentLeaseController {
    type Lease;

    /// Acquire before the guarded stage is spawned. `None` means "proceed
    /// without a lease": the lease is a best-effort resource-contention
    /// mitigation, not a correctness requirement, so a daemon that is
    /// unreachable or refuses the reservation must not fail `ci-test` --
    /// that would make host validation depend on a mechanism whose entire
    /// job is to make host validation more reliable.
    fn acquire(&self) -> Option<Self::Lease>;

    /// Release exactly once, on every exit path of the stage it guarded --
    /// success, stage failure, and spawn/poll error alike. Callers must
    /// invoke this unconditionally after the guarded call returns (never
    /// behind a `?`), so a failure/cancel outcome releases the lease just
    /// as reliably as success does.
    fn release(&self, lease: Option<Self::Lease>);
}

/// Real daemon-backed lease controller.
pub(super) struct DaemonResidentLeaseController {
    pub(super) permits: u32,
    pub(super) enabled: bool,
}

impl ResidentLeaseController for DaemonResidentLeaseController {
    type Lease = crate::daemon::client::ResidentCapacityLease;

    fn acquire(&self) -> Option<Self::Lease> {
        if !self.enabled {
            return None;
        }
        let paths = match crate::core::SoldrPaths::new() {
            Ok(paths) => paths,
            Err(error) => {
                eprintln!(
                    "soldr ci-test: could not resolve daemon paths for the Nextest \
                     resident-capacity lease (soldr#2878), proceeding without it: {error}"
                );
                return None;
            }
        };
        let sock = crate::daemon::client::default_sock_path(&paths);
        match crate::daemon::client::acquire_resident_capacity(&sock, self.permits) {
            Ok(lease) => {
                eprintln!(
                    "soldr ci-test: reserved {} resident-capacity permit(s) around Fresh \
                     Nextest execution (soldr#2878)",
                    lease.permits()
                );
                Some(lease)
            }
            Err(error) => {
                eprintln!(
                    "soldr ci-test: could not reserve the Nextest resident-capacity lease \
                     (soldr#2878), proceeding without it: {error:?}"
                );
                None
            }
        }
    }

    fn release(&self, lease: Option<Self::Lease>) {
        let Some(lease) = lease else {
            return;
        };
        // `finish()` sends the explicit release frame and waits for the
        // daemon's acknowledgement -- the clean handshake on every path,
        // not just success, because nothing before this call can skip it
        // (see the trait contract above). If the connection is already
        // gone (the daemon exited, or a prior IPC error already dropped
        // it) `finish()` still leaves nothing to leak: dropping
        // `ResidentCapacityLease` closes its control-connection socket,
        // and the daemon's `serve_resident_capacity_lease` drops the same
        // opaque permit guard as soon as it observes that disconnect
        // (`crates/soldr-daemon/src/daemon/server_dispatch.rs`), so the
        // reservation cannot outlive the failed handshake either.
        if let Err(error) = lease.finish() {
            eprintln!(
                "soldr ci-test: Nextest resident-capacity lease release did not receive a \
                 daemon acknowledgement (soldr#2878): {error:?}"
            );
        }
    }
}

/// Acquires the lease for `stage_name` when -- and only when -- it is the
/// Nextest EXECUTION stage. The outer `None` means "not a leased stage"; the
/// inner value is the controller's best-effort acquisition.
///
/// The caller (`execute.rs`'s `NextestExecutionHooks`) calls this immediately
/// before spawning the stage and hands the result back to
/// [`ResidentLeaseController::release`] on every exit path of that stage --
/// success, failure, cancellation, and spawn error alike.
///
/// The stage-name gate mirrors the one `configure_nextest_test_cargo_runner`
/// uses for the Cargo-runner injection (`stage.name != "nextest"`,
/// `execute.rs`), including its paired positive/negative test shape, so a
/// future caller cannot wire the lease to `nextest-compile` by accident.
pub(super) fn acquire_for_stage<C: ResidentLeaseController>(
    lease_controller: &C,
    stage_name: &str,
) -> Option<Option<C::Lease>> {
    (stage_name == "nextest").then(|| lease_controller.acquire())
}

#[cfg(test)]
#[path = "nextest_resident_lease_tests.rs"]
mod tests;
