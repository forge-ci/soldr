//! Windows process termination.

use std::io;
use std::process::Child;

/// How a tree kill was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeKill {
    /// The whole tree was killed (the snapshotted descendant set, then the root).
    TreeKilled,
    /// The tree could not be enumerated; the direct child was killed instead.
    ProcessKilled,
}

/// Kill `child` and every descendant, then *verify* the tree is gone.
///
/// This replaces a `taskkill /PID <root> /T /F` shell-out. The motivation is
/// soldr#2605: on the Windows target-run lanes a cancelled `soldr lint deps`
/// sibling kept running -- its `cmd.exe` died while the `ping.exe` grandchild
/// slept its full 30s, still holding the inherited stderr pipe (the `1 leaky`
/// in the same nextest summary).
///
/// The exact escape route is NOT established, and this function does not claim
/// to have found it. A tempting explanation -- that a grandchild becomes
/// unreachable once its parent dies -- was tested and is false: parent links
/// keep resolving after the root exits (see
/// `a_dead_root_still_exposes_its_grandchildren`). What is established is that
/// `taskkill` gave us no way to tell:
///
///   * it is a one-shot request with no confirmation that anything died;
///   * a non-zero exit fell back to killing only the direct child, silently;
///   * and it can name no survivor, so a recurrence produced no evidence.
///
/// So the guarantee is strengthened where it can be: enumerate in-process,
/// kill leaves-first, then re-enumerate and re-kill until the tree is empty or
/// the budget expires. A verified kill beats an unverified one regardless of
/// which mechanism let the descendant escape, and retained process handles
/// make completion evidence survive later snapshot changes.
pub fn terminate_tree(child: &mut Child) -> io::Result<TreeKill> {
    let root = child.id();

    // Safe to read parent links now: we hold the child handle un-reaped, so
    // Windows cannot recycle `root` out from under the walk.
    let root_created = process_creation_token(root);
    let enumerated = descendants_with_tokens(root, |pid| {
        if pid == root {
            root_created
        } else {
            process_creation_token(pid)
        }
    });

    // Every descendant ever seen, with a query-only handle retained until the
    // verification budget expires. A retained handle stays attached to the
    // original process after it exits, so its FILETIMEs prove completion even
    // if its parent disappears from a later snapshot or its pid is recycled.
    //
    // soldr#2806, second half: each sweep re-derives reachability from a fresh
    // snapshot, and a process that has exited is no longer in that snapshot. So
    // once the intermediate `cmd.exe` dies, its surviving `ping.exe` grandchild
    // still names it as parent but that parent is not a node any more -- the
    // walk from `root` cannot reach the orphan, the sweep reports `Clear`, and
    // `terminate_tree` returns `TreeKilled` while the grandchild runs on. That
    // is precisely the case the verification loop was added to catch, and the
    // one observed in #2806.
    //
    // Remembering the set makes the orphan keep being watched after the link to
    // it is gone.
    //
    // Windows reuses pids freely, so a remembered pid is not an identity on its
    // own -- the identity is (pid, creation time), the same pairing
    // `broker_lease`'s start token stands for. Captured here, while the
    // descendants are still alive to be asked.
    //
    // A descendant whose creation time cannot be read is deliberately **not**
    // remembered. Without a captured identity there is no way to prove later
    // that the pid still holds the same process, and acting on it anyway is how
    // a stranger gets killed. It is still terminated below and still found by
    // the walk while it remains reachable; only the after-the-link-is-gone
    // guarantee is given up, which is the honest trade.
    let mut known = Vec::new();
    let mut verified = enumerated.is_some();

    if let Some(descendants) = &enumerated {
        let untracked = retain_snapshot(descendants, &mut known);
        verified &= untracked.is_empty();
        // Leaves first. Killing a parent before its child is exactly the
        // orphaning this function exists to avoid -- and while the snapshot
        // already pins the set, bottom-up also stops a shell from spawning one
        // more child in the window before its own termination lands.
        //
        // No identity constraint: these came from a snapshot taken moments ago,
        // which is the evidence. The constraint applies to remembered pids,
        // whose snapshot has since gone stale.
        best_effort_terminate(&untracked);
    }

    // Kill the direct cargo/cmd child promptly. Its descendants may become
    // unreachable in a later process snapshot when that happens, which is why
    // their query handles were retained above; those handles remain bound to
    // the original processes and let the following terminate requests and
    // FILETIME verification finish without relying on parent links. In the
    // diagnostic-capture path this also closes the root side of an inherited
    // stderr pipe before reaping the child.
    child.kill()?;

    // The retained records are identity-confirmed even though the root may
    // already be gone, so request their termination only after the root kill.
    // Do this even if retaining another fresh descendant already downgraded the
    // proof: every tracked handle is still safe to terminate, and a failed
    // query for one sibling must not strand the rest.
    request_tracked_after_root(&mut verified, || request_termination(&mut known));
    if !verified {
        // The snapshot or a query/identity/terminate request failed, so report
        // the weaker guarantee rather than claiming a tree kill we did not
        // prove.
        return Ok(unverified_tree(
            root,
            "initial identity or termination request failed",
            &known,
        ));
    }

    // Verification sweep. TerminateProcess is asynchronous, and a shell can
    // spawn one more child in the window between the snapshot and its own
    // termination -- so a single pass cannot prove the tree is gone. Re-kill
    // whatever is still standing until nothing is, or the budget expires.
    let deadline = std::time::Instant::now() + VERIFY_BUDGET;
    loop {
        // Child and TrackedDescendant keep these process objects alive, so
        // their PIDs cannot be reused until those handles are closed. Reuse
        // their captured identities rather than reopening a killed process:
        // OpenProcess may reject it while the retained handle remains valid.
        let sweep = descendants_with_tokens(root, |pid| {
            retained_creation_token(
                pid,
                root_created
                    .map(|created| (root, created))
                    .into_iter()
                    .chain(known.iter().map(|tracked| (tracked.pid, tracked.created))),
                process_creation_token,
            )
        });
        let descendants = match sweep {
            Some(descendants) => descendants,
            // A failed snapshot proves nothing. Do not let a later empty
            // snapshot erase that failure: a verified kill requires every
            // snapshot used for verification to have succeeded.
            None => {
                return Ok(unverified_tree(
                    root,
                    "verification snapshot failed",
                    &known,
                ))
            }
        };
        // A shell can spawn one more child in the window before its own
        // termination lands, so retain and terminate each newly observed pid.
        let untracked = retain_snapshot(&descendants, &mut known);
        verified &= untracked.is_empty();
        best_effort_terminate(&untracked);
        verified &= request_termination(&mut known);
        if !verified {
            return Ok(unverified_tree(
                root,
                "fresh identity or termination request failed",
                &known,
            ));
        }
        if all_exited(&known) {
            return Ok(TreeKill::TreeKilled);
        }
        if std::time::Instant::now() >= deadline {
            // Out of budget with survivors still live, or never able to look.
            // Say so: the whole point of soldr#2605 is that this case used to
            // report success.
            return Ok(unverified_tree(
                root,
                "descendant exit proof deadline expired",
                &known,
            ));
        }
        std::thread::sleep(VERIFY_POLL);
    }
}

const PROCESS_TERMINATE: u32 = 0x0001;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

fn unverified_tree(root: u32, reason: &str, known: &[TrackedDescendant]) -> TreeKill {
    eprintln!("soldr: process-tree cleanup for root {root} could not be verified: {reason}");
    for tracked in known.iter().take(8) {
        eprintln!(
            "soldr: descendant pid={} created={} termination_requested={} times={:?}",
            tracked.pid,
            tracked.created,
            tracked.termination_requested,
            tracked.times()
        );
    }
    TreeKill::ProcessKilled
}
// Test-only: terminate_tests.rs proves the identity handle works under a
// token that denies SYNCHRONIZE.
#[cfg(test)]
const SYNCHRONIZE: u32 = 0x0010_0000;

/// FILETIMEs read from one retained query-only process handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessTimes {
    created: u64,
    exited: u64,
}

/// A descendant identity which cannot be confused with a later PID reuse.
struct TrackedDescendant {
    pid: u32,
    created: u64,
    query_handle: windows_sys::Win32::Foundation::HANDLE,
    termination_requested: bool,
}

impl TrackedDescendant {
    fn open(pid: u32) -> io::Result<Self> {
        let query_handle = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let times = match process_times(query_handle) {
            Ok(times) => times,
            Err(error) => {
                // SAFETY: `query_handle` was opened above and has not been
                // moved into a TrackedDescendant yet.
                unsafe { windows_sys::Win32::Foundation::CloseHandle(query_handle) };
                return Err(error);
            }
        };
        Ok(Self {
            pid,
            created: times.created,
            query_handle,
            termination_requested: false,
        })
    }

    /// Read both identity and completion evidence from the original handle.
    fn times(&self) -> io::Result<ProcessTimes> {
        let times = process_times(self.query_handle)?;
        if !is_same_process(self.created, Some(times.created)) {
            return Err(io::Error::other(format!(
                "process identity changed for PID {}: retained creation {}, queried creation {}, exit {}",
                self.pid, self.created, times.created, times.exited
            )));
        }
        Ok(times)
    }

    fn request_termination(&mut self) -> io::Result<()> {
        // Query immediately before opening the terminate-only handle. This
        // rejects an inaccessible or replaced identity instead of claiming a
        // tree kill based on a stale PID.
        if self.times()?.exited != 0 {
            self.termination_requested = true;
            return Ok(());
        }
        let termination = open_process(self.pid, PROCESS_TERMINATE).and_then(|terminate_handle| {
            // SAFETY: this process identity is pinned by query_handle, and the
            // newly opened handle has PROCESS_TERMINATE access.
            let terminated = unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(terminate_handle, 1)
            };
            let error = (terminated == 0).then(io::Error::last_os_error);
            // SAFETY: the handle came from open_process and is not used again.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(terminate_handle) };
            error.map_or(Ok(()), Err)
        });
        if let Err(error) = termination {
            // A descendant that exits on its own between the query above and
            // TerminateProcess -- a `cmd.exe` whose awaited `ping` was just
            // killed leaves-first -- makes TerminateProcess fail with
            // ERROR_ACCESS_DENIED, including from OpenProcess itself. The
            // retained handle can still prove that exit; only an unproven
            // failure weakens the result.
            if !exited_despite_failed_terminate(self.created, self.times().ok()) {
                return Err(error);
            }
        }
        self.termination_requested = true;
        Ok(())
    }
}

impl Drop for TrackedDescendant {
    fn drop(&mut self) {
        // SAFETY: `query_handle` was obtained by OpenProcess and is retained
        // exclusively by this record until it is dropped.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.query_handle) };
    }
}

/// Open a deliberately narrow process handle. Query handles never request
/// SYNCHRONIZE: restricted/job-object tokens commonly grant query access while
/// denying the combined TERMINATE | SYNCHRONIZE shape that broke #2929.
fn open_process(pid: u32, access: u32) -> io::Result<windows_sys::Win32::Foundation::HANDLE> {
    let handle = unsafe { windows_sys::Win32::System::Threading::OpenProcess(access, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(handle)
}

/// Capture creation and exit FILETIMEs using an already-open query handle.
fn process_times(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<ProcessTimes> {
    use windows_sys::Win32::Foundation::FILETIME;

    // SAFETY: FILETIME is plain-old-data.
    let mut created: FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: FILETIME is plain-old-data.
    let mut exited: FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: FILETIME is plain-old-data.
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: FILETIME is plain-old-data.
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        windows_sys::Win32::System::Threading::GetProcessTimes(
            handle,
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ProcessTimes {
        created: filetime_value(created),
        exited: filetime_value(exited),
    })
}

fn filetime_value(time: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// Creation FILETIME for `pid`, as a PID-reuse-safe identity token.
///
/// `pub(crate)` so `process::inspect::process_start_token` -- the portable
/// facade soldr-cli's broker route reaper calls -- can reuse this exact
/// query instead of a second, independently-written `OpenProcess` +
/// `GetProcessTimes` call site that could drift from the one this module's
/// own `is_same_process` identity check already depends on.
///
/// `None` on any failure. `None` must never be treated as a match by a
/// caller comparing tokens.
pub(crate) fn process_creation_token(pid: u32) -> Option<u64> {
    let handle = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    let times = process_times(handle);
    // SAFETY: `handle` came from `open_process` above and is not used again.
    unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
    times.ok().map(|times| times.created)
}

/// Retain a query handle for each fresh snapshot identity. Pids which cannot
/// be opened or queried are returned for best-effort termination; they still
/// make the final result `ProcessKilled`, because no retained handle can prove
/// their identity or exit.
fn retain_snapshot(pids: &[u32], known: &mut Vec<TrackedDescendant>) -> Vec<u32> {
    retain_snapshot_with(pids, known, TrackedDescendant::open)
}

fn retain_snapshot_with(
    pids: &[u32],
    known: &mut Vec<TrackedDescendant>,
    mut open: impl FnMut(u32) -> io::Result<TrackedDescendant>,
) -> Vec<u32> {
    let mut untracked = Vec::new();
    for pid in pids {
        // The retained handle pins both the identity and PID, including after
        // exit. A second OpenProcess can fail for that killed process and must
        // not turn an already tracked descendant into an untracked one.
        if known.iter().any(|tracked| tracked.pid == *pid) {
            continue;
        }
        match open(*pid) {
            Ok(tracked) => known.push(tracked),
            Err(error) if open_failure_proves_gone(&error) => {}
            Err(error) => {
                eprintln!("soldr: could not retain descendant {pid}: {error}");
                untracked.push(*pid);
            }
        }
    }
    untracked
}

/// `OpenProcess` answers ERROR_INVALID_PARAMETER when no process holds the
/// pid any more: the snapshot entry exited before it could be opened. That is
/// the outcome termination wants, not an unverifiable descendant, and it
/// leaves nothing to signal -- killing the bare pid could only hit a recycled
/// one. Every other open failure (access denied, ...) stays untracked.
fn open_failure_proves_gone(error: &io::Error) -> bool {
    const ERROR_INVALID_PARAMETER: i32 = 87;
    error.raw_os_error() == Some(ERROR_INVALID_PARAMETER)
}

/// A failed TerminateProcess still completes the request when the retained
/// handle proves the same process has exited.
fn exited_despite_failed_terminate(expected_created: u64, observed: Option<ProcessTimes>) -> bool {
    completion_proven(expected_created, observed, true)
}

/// Preserve the old fresh-snapshot behaviour for a descendant that cannot be
/// queried: still request termination, but never call that request proof.
fn best_effort_terminate(pids: &[u32]) {
    terminate_pids_leaves_first(pids, |pid| {
        let _ = signal_pid(pid, true);
    });
}

/// Injected solely to pin the leaves-first best-effort path without requiring
/// an ACL-shaped Windows process fixture in every test run.
fn terminate_pids_leaves_first(pids: &[u32], mut terminate: impl FnMut(u32)) {
    for pid in pids.iter().rev() {
        terminate(*pid);
    }
}

/// Request each observed process stop, leaves first. A failed open or a false
/// BOOL from TerminateProcess makes a verified tree kill impossible.
fn request_termination(known: &mut [TrackedDescendant]) -> bool {
    let mut complete = true;
    for tracked in known.iter_mut().rev() {
        if tracked.termination_requested {
            continue;
        }
        if let Err(error) = tracked.request_termination() {
            eprintln!(
                "soldr: could not terminate tracked descendant {}: {error}",
                tracked.pid
            );
            complete = false;
        }
    }
    complete
}

/// Request every identity-confirmed descendant even when another descendant
/// already made the final claim weaker. This keeps termination and proof
/// separate: a failed proof never authorises `TreeKilled`, but it also never
/// abandons a safe retained-handle termination request.
fn request_tracked_after_root(verified: &mut bool, request: impl FnOnce() -> bool) {
    *verified &= request();
}

/// All identity-confirmed descendants must report a non-zero exit FILETIME
/// through their retained query handles before the strong result is allowed.
fn all_exited(known: &[TrackedDescendant]) -> bool {
    known.iter().all(|tracked| {
        completion_proven(
            tracked.created,
            tracked.times().ok(),
            tracked.termination_requested,
        )
    })
}

#[cfg(test)]
fn terminate_request_succeeded(result: bool) -> bool {
    result
}

/// Pure completion rule used by the retained-handle loop and deterministic
/// tests. No missing query, changed creation time, failed terminate request,
/// or zero exit FILETIME may become `TreeKilled`.
fn completion_proven(
    expected_created: u64,
    observed: Option<ProcessTimes>,
    termination_succeeded: bool,
) -> bool {
    termination_succeeded
        && observed.is_some_and(|times| {
            is_same_process(expected_created, Some(times.created)) && times.exited != 0
        })
}

/// Is the process at `pid` now the same one whose identity we captured?
///
/// Windows reuses pids freely and `known` deliberately outlives the processes
/// in it, so `pid` alone proves nothing. `(pid, creation time)` is the identity
/// -- the same pairing `broker_lease`'s start token stands for, at
/// `GetProcessTimes` resolution rather than sysinfo's whole seconds.
///
/// **An unreadable time answers `false`.** It means the process exited, or
/// refuses the query, and neither establishes that this is still our
/// descendant. The consequence of a wrong `true` here is terminating an
/// unrelated process that happens to hold a recycled pid; the consequence of a
/// wrong `false` is reporting `ProcessKilled` instead of `TreeKilled`, which is
/// the weaker claim this module already makes when it cannot verify.
fn is_same_process(recorded: u64, current: Option<u64>) -> bool {
    current == Some(recorded)
}

/// How long `terminate_tree` keeps re-killing before admitting descendants
/// survived. Bounded so a wedged descendant cannot hang the caller.
const VERIFY_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Gap between verification sweeps.
const VERIFY_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Every transitive descendant of `root`, ordered parents before children.
///
/// `None` means the process snapshot could not be taken, which is distinct from
/// `Some(vec![])` ("the root is a leaf") -- the caller reports a weaker kill for
/// the former and a complete one for the latter.
#[cfg(test)]
fn descendants_of(root: u32) -> Option<Vec<u32>> {
    descendants_with_tokens(root, process_creation_token)
}

/// Prefer identities pinned by live handles; only fresh PIDs need opening.
fn retained_creation_token(
    pid: u32,
    mut retained: impl Iterator<Item = (u32, u64)>,
    query: impl FnOnce(u32) -> Option<u64>,
) -> Option<u64> {
    retained
        .find_map(|(retained_pid, created)| (retained_pid == pid).then_some(created))
        .or_else(|| query(pid))
}

fn descendants_with_tokens(root: u32, created: impl Fn(u32) -> Option<u64>) -> Option<Vec<u32>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };

    // SAFETY: TH32CS_SNAPPROCESS with pid 0 snapshots all processes; the handle
    // is closed on every path out.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }

    // (pid, parent) for every live process, read in one pass so the whole graph
    // is consistent -- walking the snapshot repeatedly would reintroduce the
    // very inconsistency this avoids.
    let mut edges: Vec<(u32, u32)> = Vec::new();
    // SAFETY: PROCESSENTRY32W is plain-old-data; the API requires it zeroed
    // with `dwSize` set before the first call.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    // SAFETY: `entry` is zeroed with dwSize set as the API requires.
    // Process32FirstW is skipped deliberately: entry 0 is the idle process,
    // which is never part of a tree we spawned.
    while unsafe { Process32NextW(snapshot, &mut entry) } != 0 {
        edges.push((entry.th32ProcessID, entry.th32ParentProcessID));
    }

    // SAFETY: `snapshot` came from CreateToolhelp32Snapshot and is not used
    // after this point.
    unsafe { CloseHandle(snapshot) };

    // A parent edge is only trusted when the child was created no earlier
    // than the process now holding the parent PID (see `collect_descendants`).
    // Without the root's own creation time no edge can be proven, so the
    // sweep reports itself unverified instead of guessing.
    created(root)?;
    Some(collect_descendants(&edges, root, created))
}

/// Breadth-first walk of `edges` from `root`, parents before children.
///
/// Split out from the snapshot so the traversal -- including its cycle and
/// self-parent guards -- is unit-testable on any host.
///
/// Windows never rewrites `th32ParentProcessID` when a parent exits, and it
/// recycles PIDs aggressively. An orphan whose dead parent's PID was reused by
/// a process in our tree would otherwise be collected -- and terminated --
/// as our descendant, even though it is unrelated. On the Windows arm64
/// target-run lane this reached the test process itself, which died mid-test
/// with no panic output while its own children leaked. A real child is always
/// created at or after its parent, so an edge counts only when both creation
/// times are known and the child's is not earlier than the parent's.
fn collect_descendants(
    edges: &[(u32, u32)],
    root: u32,
    created: impl Fn(u32) -> Option<u64>,
) -> Vec<u32> {
    let mut ordered = Vec::new();
    let mut frontier = vec![root];
    // `seen` carries `root` from the start so a process reporting itself as its
    // own parent, or a recycled pid that closes a cycle back to the root, cannot
    // loop forever.
    let mut seen = vec![root];

    while let Some(parent) = frontier.pop() {
        let Some(parent_created) = created(parent) else {
            continue;
        };
        for (pid, ppid) in edges {
            if *ppid != parent || seen.contains(pid) {
                continue;
            }
            if !created(*pid).is_some_and(|child| child >= parent_created) {
                continue;
            }
            seen.push(*pid);
            ordered.push(*pid);
            frontier.push(*pid);
        }
    }
    ordered
}

/// Terminate a single PID (`TerminateProcess` - the Windows equivalent of
/// SIGKILL; never the graceful request).
pub fn terminate_pid(pid: u32) {
    let _ = signal_pid(pid, true);
}

/// Signal a single PID.
///
/// `force = false` is the Windows equivalent of SIGTERM (soldr#3096): it
/// signals the target daemon's named terminate event
/// (`super::signal::request_graceful_terminate`) so the daemon takes its
/// fast-exit path itself. When the target has no such event -- it is not a
/// soldr daemon, or it predates the mechanism -- this falls back to
/// `TerminateProcess` so `soldr daemon stop`-style escalation keeps working.
/// `force = true` is always `TerminateProcess` (SIGKILL-equivalent).
pub fn signal_pid(pid: u32, force: bool) -> io::Result<()> {
    if !force && super::signal::request_graceful_terminate(pid)? {
        return Ok(());
    }
    kill_pid(pid)
}

/// `TerminateProcess(pid, 1)`: the kernel kill, no code runs in the target.
fn kill_pid(pid: u32) -> io::Result<()> {
    use std::os::windows::raw::HANDLE;
    // Win32 API spelling - clippy would rename to Dword.
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    const PROCESS_TERMINATE: DWORD = 0x0001;
    extern "system" {
        fn OpenProcess(desired_access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn TerminateProcess(h: HANDLE, exit_code: DWORD) -> BOOL;
        fn CloseHandle(h: HANDLE) -> BOOL;
    }
    // SAFETY: OpenProcess on a pid the caller has already verified; a null
    // return means the process is gone or inaccessible, which is success for a
    // terminate request. TerminateProcess is SIGKILL-equivalent.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return Ok(());
    }
    unsafe {
        TerminateProcess(handle, 1);
        CloseHandle(handle);
    }
    Ok(())
}

#[cfg(test)]
#[path = "terminate_tests.rs"]
mod tests;
