//! Generation-safe process-tree traversal shared by selected platform roots.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use sysinfo::{Pid, System};

use crate::platform::process::{
    ProcessIdentity, ProcessIdentityAction, ProcessIdentityActionError, ProcessIdentityCapture,
};

pub(crate) fn kill_tree(
    root: ProcessIdentity,
    timeout: Duration,
    capture: fn(u32) -> ProcessIdentityCapture,
    signal: fn(ProcessIdentity) -> Result<ProcessIdentityAction, ProcessIdentityActionError>,
) -> Result<u32, ProcessIdentityActionError> {
    match capture(root.pid()) {
        ProcessIdentityCapture::Found(current) if current == root => {}
        ProcessIdentityCapture::Found(_) => return Err(ProcessIdentityActionError::StaleIdentity),
        ProcessIdentityCapture::Exited => return Ok(0),
        ProcessIdentityCapture::Unavailable(reason) => return Err(ProcessIdentityActionError::Unavailable(reason)),
        ProcessIdentityCapture::Error(error) => return Err(ProcessIdentityActionError::Host(error)),
    }

    let mut system = System::new();
    system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::everything());
    let mut targets = vec![(root, 0_usize)];
    collect_descendants(&system, root.pid(), 1, &mut HashSet::new(), &mut targets, capture);
    targets.sort_unstable_by_key(|(_, depth)| std::cmp::Reverse(*depth));
    let targets: Vec<_> = targets.into_iter().map(|(identity, _)| identity).collect();
    let mut signaled = HashSet::new();
    let started = Instant::now();
    loop {
        for target in &targets {
            match signal(*target)? {
                ProcessIdentityAction::Performed => {
                    signaled.insert(*target);
                }
                ProcessIdentityAction::AlreadyExited => {}
            }
        }
        if started.elapsed() >= timeout {
            break;
        }
        system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::everything());
        if !targets.iter().any(|target| matches!(capture(target.pid()), ProcessIdentityCapture::Found(current) if current == *target)) {
            break;
        }
        std::thread::sleep(timeout.saturating_sub(started.elapsed()).min(Duration::from_millis(25)));
    }
    Ok(signaled.len() as u32)
}

fn collect_descendants(
    system: &System,
    parent_pid: u32,
    depth: usize,
    visited: &mut HashSet<Pid>,
    targets: &mut Vec<(ProcessIdentity, usize)>,
    capture: fn(u32) -> ProcessIdentityCapture,
) {
    for (pid, process) in system.processes() {
        if process.parent() != Some(Pid::from_u32(parent_pid)) || !visited.insert(*pid) {
            continue;
        }
        let child_pid = pid.as_u32();
        if let ProcessIdentityCapture::Found(identity) = capture(child_pid) {
            targets.push((identity, depth));
            collect_descendants(system, child_pid, depth + 1, visited, targets, capture);
        }
    }
}
