//! Linux implementation of launched-tree descendant monitoring.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::platform::process::{DescendantEvent, DescendantMonitorStop};

const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessIdentity {
    start_ticks: u64,
}

pub fn start_descendant_monitor(
    root_pid: u32,
    stop: Arc<DescendantMonitorStop>,
    emit: Box<dyn Fn(DescendantEvent) + Send>,
) -> std::io::Result<()> {
    let Some(root_identity) = process_identity(root_pid) else {
        emit(DescendantEvent::Completed);
        return Ok(());
    };
    enable_subreaper();
    std::thread::Builder::new()
        .name("rp-linux-descpump".to_string())
        .spawn(move || pump_loop(root_pid, root_identity, stop, emit))
        .map(|_| ())
        .map_err(|error| std::io::Error::other(format!("spawn descendant monitor: {error}")))
}

fn enable_subreaper() {
    // Failure is intentionally best-effort: the monitor can still track a
    // descendant while its immediate parent remains alive.
    let _ = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
}

fn parse_start_ticks(stat: &str) -> Option<u64> {
    let suffix = stat.get(stat.rfind(')')? + 1..)?;
    suffix
        .split_ascii_whitespace()
        .nth(19)
        .and_then(|field| field.parse().ok())
}

fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(ProcessIdentity {
        start_ticks: parse_start_ticks(&stat)?,
    })
}

/// Map of live descendant pid -> immediate parent pid. The parent is a
/// free by-product of the `children`-file walk: the pid whose children
/// file listed the entry.
fn descendant_pids(root_pid: u32) -> HashMap<u32, u32> {
    let mut result = HashMap::new();
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        let path = format!("/proc/{pid}/task/{pid}/children");
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        for token in contents.split_ascii_whitespace() {
            if let Ok(child) = token.parse::<u32>() {
                if result.insert(child, pid).is_none() {
                    stack.push(child);
                }
            }
        }
    }
    result
}

fn snapshot(root_pid: u32, expected: ProcessIdentity) -> Option<HashMap<u32, u32>> {
    let before = process_identity(root_pid);
    let descendants = descendant_pids(root_pid);
    verified_snapshot(
        expected,
        before,
        descendants,
        process_identity(root_pid),
    )
}

fn verified_snapshot(
    expected: ProcessIdentity,
    before: Option<ProcessIdentity>,
    descendants: HashMap<u32, u32>,
    after: Option<ProcessIdentity>,
) -> Option<HashMap<u32, u32>> {
    (before == Some(expected) && after == Some(expected)).then_some(descendants)
}

fn emit_diff(
    previous: &HashMap<u32, u32>,
    current: &HashMap<u32, u32>,
    emit: &dyn Fn(DescendantEvent),
) {
    for (&pid, &parent_pid) in current {
        if !previous.contains_key(&pid) {
            emit(DescendantEvent::Started {
                pid,
                parent_pid: Some(parent_pid),
            });
        }
    }
    for &pid in previous.keys() {
        if !current.contains_key(&pid) {
            emit(DescendantEvent::Exited(pid));
        }
    }
}

fn pump_loop_with(
    stop: &DescendantMonitorStop,
    mut take_snapshot: impl FnMut() -> Option<HashMap<u32, u32>>,
    emit: &dyn Fn(DescendantEvent),
    mut wait: impl FnMut() -> bool,
) {
    let mut known = HashMap::new();
    loop {
        if stop.is_stopped() {
            emit(DescendantEvent::Completed);
            return;
        }
        let Some(current) = take_snapshot() else {
            for pid in known.into_keys() {
                emit(DescendantEvent::Exited(pid));
            }
            emit(DescendantEvent::Completed);
            return;
        };
        emit_diff(&known, &current, emit);
        known = current;
        if wait() {
            emit(DescendantEvent::Completed);
            return;
        }
    }
}

fn pump_loop(
    root_pid: u32,
    root_identity: ProcessIdentity,
    stop: Arc<DescendantMonitorStop>,
    emit: Box<dyn Fn(DescendantEvent) + Send>,
) {
    pump_loop_with(
        &stop,
        || snapshot(root_pid, root_identity),
        emit.as_ref(),
        || stop.wait_timeout(POLL_INTERVAL),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::mpsc;

    fn collect_diff(
        previous: &HashMap<u32, u32>,
        current: &HashMap<u32, u32>,
    ) -> Vec<DescendantEvent> {
        let (tx, rx) = mpsc::channel();
        emit_diff(previous, current, &|event| tx.send(event).unwrap());
        drop(tx);
        rx.iter().collect()
    }

    #[test]
    fn descendant_sampling_cadence_is_twenty_milliseconds() {
        assert_eq!(POLL_INTERVAL, Duration::from_millis(20));
    }

    #[test]
    fn emit_diff_fires_one_started_per_new_pid_with_its_parent() {
        let previous = [(10, 1), (20, 10)].into_iter().collect();
        let current = [(10, 1), (20, 10), (30, 20), (40, 10)].into_iter().collect();
        let events = collect_diff(&previous, &current);
        let started: HashMap<_, _> = events
            .into_iter()
            .filter_map(|event| match event {
                DescendantEvent::Started { pid, parent_pid } => Some((pid, parent_pid)),
                DescendantEvent::Exited(_) | DescendantEvent::Completed => None,
            })
            .collect();
        assert_eq!(
            started,
            [(30, Some(20)), (40, Some(10))].into_iter().collect()
        );
    }

    #[test]
    fn emit_diff_fires_one_exited_per_gone_pid() {
        let previous = [(10, 1), (20, 10), (30, 20)].into_iter().collect();
        let current = [(10, 1)].into_iter().collect();
        let events = collect_diff(&previous, &current);
        let exited: HashSet<_> = events
            .into_iter()
            .filter_map(|event| match event {
                DescendantEvent::Exited(pid) => Some(pid),
                DescendantEvent::Started { .. } | DescendantEvent::Completed => None,
            })
            .collect();
        assert_eq!(exited, [20, 30].into_iter().collect());
    }

    #[test]
    fn emit_diff_no_events_when_steady_state() {
        let current = [(10, 1), (20, 10)].into_iter().collect();
        assert!(collect_diff(&current, &current).is_empty());
    }

    #[test]
    fn descendant_pids_for_nonexistent_root_returns_empty() {
        assert!(descendant_pids(0x7fff_fffe).is_empty());
    }

    #[test]
    fn descendant_pids_for_self_includes_no_phantom_entries() {
        assert!(descendant_pids(std::process::id())
            .into_iter()
            .all(|(pid, parent_pid)| pid > 1 && parent_pid > 0));
    }

    #[test]
    fn parse_start_ticks_handles_spaces_and_parentheses_in_comm() {
        let mut fields = vec!["S".to_string()];
        fields.extend((4..=21).map(|number| number.to_string()));
        fields.push("424242".to_string());
        let stat = format!("123 (odd ) process name) {}", fields.join(" "));
        assert_eq!(parse_start_ticks(&stat), Some(424242));
    }

    #[test]
    fn identity_mismatch_terminates_pump_without_tracking_reused_pid() {
        let stop = DescendantMonitorStop::new();
        let (tx, rx) = mpsc::channel();
        let mut polls = 0;
        pump_loop_with(
            &stop,
            || {
                polls += 1;
                None
            },
            &|event| tx.send(event).unwrap(),
            || panic!("terminated pump must not wait"),
        );
        assert_eq!(polls, 1);
        assert_eq!(rx.try_recv(), Ok(DescendantEvent::Completed));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn reused_pid_with_different_start_ticks_rejects_descendant_snapshot() {
        let expected = ProcessIdentity { start_ticks: 100 };
        let recycled = ProcessIdentity { start_ticks: 200 };
        assert_eq!(
            verified_snapshot(
                expected,
                Some(recycled),
                [(42, 7)].into_iter().collect(),
                Some(recycled),
            ),
            None
        );
    }

    #[test]
    fn scripted_normal_pump_emits_descendant_start_and_exit() {
        let stop = DescendantMonitorStop::new();
        let (tx, rx) = mpsc::channel();
        let mut snapshots = [
            Some([(42, 7)].into_iter().collect()),
            Some(HashMap::new()),
            None,
        ]
        .into_iter();
        pump_loop_with(
            &stop,
            || snapshots.next().flatten(),
            &|event| tx.send(event).unwrap(),
            || false,
        );
        drop(tx);
        assert_eq!(
            rx.iter().collect::<Vec<_>>(),
            [
                DescendantEvent::Started {
                    pid: 42,
                    parent_pid: Some(7),
                },
                DescendantEvent::Exited(42),
                DescendantEvent::Completed,
            ]
        );
    }

    #[test]
    fn stop_wakes_waiting_pump_without_polling() {
        let stop = Arc::new(DescendantMonitorStop::new());
        let pump_stop = Arc::clone(&stop);
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let pump = std::thread::spawn(move || {
            let mut announced = false;
            pump_loop_with(
                &pump_stop,
                || {
                    if !announced {
                        announced = true;
                        waiting_tx.send(()).unwrap();
                    }
                    Some(HashMap::new())
                },
                &|event| event_tx.send(event).unwrap(),
                || pump_stop.wait_timeout(Duration::from_secs(30)),
            );
            done_tx.send(()).unwrap();
        });
        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        stop.stop();
        done_rx.recv_timeout(Duration::from_millis(250)).unwrap();
        pump.join().unwrap();
        assert_eq!(event_rx.try_recv(), Ok(DescendantEvent::Completed));
        assert!(event_rx.try_recv().is_err());
    }
}
