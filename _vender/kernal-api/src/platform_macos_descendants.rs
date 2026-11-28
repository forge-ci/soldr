//! macOS implementation of launched-tree descendant monitoring.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::platform::process::{DescendantEvent, DescendantMonitorStop, ProcessSnapshot};

const RECONCILE_INTERVAL: Duration = Duration::from_millis(50);
const STOP_EVENT_IDENT: libc::uintptr_t = libc::uintptr_t::MAX;

type Identity = crate::platform::process::ProcessIdentity;

struct Kqueue(libc::c_int);

impl Drop for Kqueue {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns the descriptor returned by
        // `kqueue`; Drop runs once and ignores an already-invalid descriptor.
        unsafe { libc::close(self.0) };
    }
}

pub fn start_descendant_monitor(
    root_pid: u32,
    stop: Arc<DescendantMonitorStop>,
    emit: Box<dyn Fn(DescendantEvent) + Send>,
) -> std::io::Result<()> {
    let Some(identity) = process_identity(root_pid) else {
        emit(DescendantEvent::Completed);
        return Ok(());
    };
    std::thread::Builder::new()
        .name("rp-macos-descpump".to_string())
        .spawn(move || pump_loop(root_pid, identity, stop, emit))
        .map(|_| ())
        .map_err(|error| std::io::Error::other(format!("spawn descendant monitor: {error}")))
}

fn process_identity(pid: u32) -> Option<Identity> {
    super::process_snapshot_for_pid(pid)
        .map(|snapshot| snapshot.identity)
}

fn descendants(root_pid: u32) -> HashMap<Identity, u32> {
    descendants_of(root_pid, &super::process_snapshot())
}

/// Map of live descendant pid -> immediate parent pid, from the same
/// process snapshot that already carries each entry's `parent_pid`.
fn descendants_of(root_pid: u32, snapshots: &[ProcessSnapshot]) -> HashMap<Identity, u32> {
    let mut children: HashMap<u32, Vec<Identity>> = HashMap::new();
    for snapshot in snapshots {
        children
            .entry(snapshot.parent_pid)
            .or_default()
            .push(snapshot.identity);
    }
    let mut result = HashMap::new();
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        if let Some(child_pids) = children.get(&pid) {
            for &child in child_pids {
                if result.insert(child, pid).is_none() {
                    stack.push(child.pid());
                }
            }
        }
    }
    result
}

fn snapshot(root_pid: u32, expected: Identity) -> Option<HashMap<Identity, u32>> {
    let before = process_identity(root_pid);
    let descendants = descendants(root_pid);
    verified_snapshot(expected, before, descendants, process_identity(root_pid))
}

fn verified_snapshot(
    expected: Identity,
    before: Option<Identity>,
    descendants: HashMap<Identity, u32>,
    after: Option<Identity>,
) -> Option<HashMap<Identity, u32>> {
    (before == Some(expected) && after == Some(expected)).then_some(descendants)
}

fn pump_loop(
    root_pid: u32,
    root_identity: Identity,
    stop: Arc<DescendantMonitorStop>,
    emit: Box<dyn Fn(DescendantEvent) + Send>,
) {
    // SAFETY: `kqueue` takes no pointers and returns a newly owned descriptor.
    let queue = unsafe { libc::kqueue() };
    let queue = (queue >= 0).then(|| Arc::new(Kqueue(queue)));
    if let Some(queue) = queue.as_ref() {
        register_stop_event(queue.0);
        let notifier_queue = Arc::clone(queue);
        let notifier_stop = Arc::clone(&stop);
        let _ = std::thread::Builder::new()
            .name("rp-macos-kqueue-stop".to_owned())
            .spawn(move || {
                while !notifier_stop.wait_timeout(Duration::from_secs(24 * 60 * 60)) {}
                trigger_stop_event(notifier_queue.0);
            });
    }
    let mut known: HashMap<Identity, u32> = HashMap::new();
    loop {
        if stop.is_stopped() {
            emit(DescendantEvent::Completed);
            return;
        }
        let Some(current) = snapshot(root_pid, root_identity) else {
            for identity in known.into_keys() {
                emit(DescendantEvent::Exited(identity.pid()));
            }
            // Wake and retire the queue notifier before the last Arc<Kqueue>
            // can be dropped. The notifier itself also owns an Arc, so the
            // descriptor can never be closed and reused beneath kevent().
            stop.stop();
            emit(DescendantEvent::Completed);
            return;
        };
        for (&identity, &parent_pid) in &current {
            if !known.contains_key(&identity) {
                emit(DescendantEvent::Started {
                    pid: identity.pid(),
                    parent_pid: Some(parent_pid),
                });
            }
        }
        for &identity in known.keys() {
            if !current.contains_key(&identity) {
                emit(DescendantEvent::Exited(identity.pid()));
            }
        }
        if let Some(queue) = queue.as_ref() {
            register_process_hint(queue.0, root_pid);
            for identity in current.keys() {
                register_process_hint(queue.0, identity.pid());
            }
        }
        known = current;
        if wait_for_hint(queue.as_ref(), &stop) {
            emit(DescendantEvent::Completed);
            return;
        }
    }
}

fn register_stop_event(queue: libc::c_int) {
    let change = libc::kevent {
        ident: STOP_EVENT_IDENT,
        filter: libc::EVFILT_USER,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: `change` remains valid for the synchronous registration call;
    // no output buffer is requested and the queue descriptor is live.
    unsafe {
        libc::kevent(
            queue,
            &raw const change,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        );
    }
}

fn trigger_stop_event(queue: libc::c_int) {
    let change = libc::kevent {
        ident: STOP_EVENT_IDENT,
        filter: libc::EVFILT_USER,
        flags: 0,
        fflags: libc::NOTE_TRIGGER,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: `change` remains valid for the synchronous trigger call. The
    // notifier owns an Arc<Kqueue>, so this descriptor cannot close or be
    // reused until kevent returns.
    unsafe {
        libc::kevent(
            queue,
            &raw const change,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        );
    }
}

fn register_process_hint(queue: libc::c_int, pid: u32) {
    let change = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
        fflags: libc::NOTE_FORK | libc::NOTE_EXEC | libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // ESRCH is expected when a very short-lived process disappears between
    // reconciliation and registration. The snapshot grade remains explicitly
    // best-effort, so registration failure is only a missed wake-up hint.
    // SAFETY: `change` remains valid for the synchronous registration call;
    // no output buffer is requested and the queue descriptor is live.
    unsafe {
        libc::kevent(
            queue,
            &raw const change,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        );
    }
}

fn wait_for_hint(queue: Option<&Arc<Kqueue>>, stop: &DescendantMonitorStop) -> bool {
    let Some(queue) = queue else {
        return stop.wait_timeout(RECONCILE_INTERVAL);
    };
    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: RECONCILE_INTERVAL.as_nanos() as libc::c_long,
    };
    // SAFETY: `kevent` is a plain C record for which all-zero is a valid
    // output-buffer initialization.
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    // SAFETY: `event` and `timeout` are valid for this synchronous call and
    // the queue wrapper keeps the descriptor open for the duration.
    unsafe {
        libc::kevent(
            queue.0,
            std::ptr::null(),
            0,
            &raw mut event,
            1,
            &raw const timeout,
        );
    }
    stop.is_stopped()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, parent_pid: u32, start_time_b: u64) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: crate::platform::process::ProcessIdentity::from_native(pid, [100, start_time_b]),
            parent_pid,
        }
    }

    fn descendants_if_root_matches(
        root_pid: u32,
        expected: Identity,
        snapshots: &[ProcessSnapshot],
    ) -> Option<HashMap<Identity, u32>> {
        snapshots
            .iter()
            .find(|snapshot| snapshot.identity.pid() == root_pid)
            .and_then(|snapshot| {
                (snapshot.identity == expected)
                    .then(|| descendants_of(root_pid, snapshots))
            })
    }

    #[test]
    fn descendants_of_handles_branching_tree() {
        let snapshots = [
            process(100, 0, 1),
            process(200, 100, 2),
            process(201, 200, 3),
            process(300, 100, 4),
            process(999, 1, 5),
        ];
        assert_eq!(
            descendants_of(100, &snapshots),
            [
                (process(200, 100, 2).identity, 100),
                (process(201, 200, 3).identity, 200),
                (process(300, 100, 4).identity, 100),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn descendants_of_for_unknown_root_returns_empty() {
        let snapshots = [process(100, 0, 1), process(200, 100, 2)];
        assert!(descendants_of(0x7fff_fffe, &snapshots).is_empty());
    }

    #[test]
    fn list_all_processes_returns_non_empty_on_real_macos() {
        let snapshots = super::super::process_snapshot();
        assert!(snapshots.len() > 5);
        assert!(snapshots
            .iter()
            .any(|snapshot| snapshot.identity.pid() == std::process::id()));
    }

    #[test]
    fn reused_root_pid_identity_mismatch_terminates_snapshot() {
        let expected = crate::platform::process::ProcessIdentity::from_native(100, [100, 1]);
        let recycled = [process(100, 0, 99), process(200, 100, 2)];
        assert_eq!(descendants_if_root_matches(100, expected, &recycled), None);
    }

    #[test]
    fn root_identity_change_after_walk_rejects_mixed_snapshot() {
        let expected = crate::platform::process::ProcessIdentity::from_native(100, [100, 1]);
        let recycled = crate::platform::process::ProcessIdentity::from_native(100, [100, 99]);
        assert_eq!(
            verified_snapshot(
                expected,
                Some(expected),
                [(crate::platform::process::ProcessIdentity::from_native(42, [100, 7]), 7)]
                    .into_iter()
                    .collect(),
                Some(recycled),
            ),
            None
        );
    }
}
