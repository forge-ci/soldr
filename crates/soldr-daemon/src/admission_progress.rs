//! Admission telemetry for the live daemon. Cancellation removes queue entries.
use crate::core::wire::proto::{WireAdmissionProgress, WireCompileWait};
use crate::platform::sync::Mutex;
use std::collections::{BTreeMap, VecDeque};
use std::sync::OnceLock;
use std::time::Instant;

pub const CLIENT_TOKEN: &str = "SOLDR_ADMISSION_CLIENT";
tokio::task_local! { pub(crate) static CURRENT: u64; }

#[derive(Default)]
struct Registry {
    next: u64,
    entries: BTreeMap<u64, Entry>,
    admissions: u64,
    completions: u64,
    recent: VecDeque<Instant>,
}
struct Entry {
    token: String,
    since: Instant,
    phase: u32,
}
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}
pub(crate) struct CompileGuard(pub(crate) u64);
pub(crate) fn register_request(req: &mut crate::daemon::protocol::CompileRequest) -> CompileGuard {
    let token = req
        .env
        .iter()
        .find(|(k, _)| k == CLIENT_TOKEN)
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    req.env.retain(|(k, _)| k != CLIENT_TOKEN);
    register(token)
}
pub(crate) fn register(token: String) -> CompileGuard {
    let mut r = registry().lock();
    r.next += 1;
    let id = r.next;
    r.entries.insert(
        id,
        Entry {
            token,
            since: Instant::now(),
            phase: 0,
        },
    );
    CompileGuard(id)
}
impl CompileGuard {
    pub(crate) fn complete(&self) {
        let mut r = registry().lock();
        r.completions += 1;
        r.recent.push_back(Instant::now());
        trim_recent(&mut r);
    }
}
impl Drop for CompileGuard {
    fn drop(&mut self) {
        registry().lock().entries.remove(&self.0);
    }
}
pub(crate) fn phase(phase: u32) {
    let _ = CURRENT.try_with(|id| {
        let mut r = registry().lock();
        if let Some(entry) = r.entries.get_mut(id) {
            entry.phase = phase;
            if phase == 2 {
                entry.since = Instant::now();
            }
            if phase == 2 {
                r.admissions += 1;
            }
        }
    });
}
fn trim_recent(r: &mut Registry) {
    while r
        .recent
        .front()
        .is_some_and(|t| t.elapsed().as_secs() >= 60)
    {
        r.recent.pop_front();
    }
}
pub(crate) fn snapshot() -> WireAdmissionProgress {
    let mut r = registry().lock();
    trim_recent(&mut r);
    let mut position = 0;
    let mut oldest = 0;
    let mut running = 0;
    let waits = r
        .entries
        .values()
        .map(|entry| {
            let waited_secs = entry.since.elapsed().as_secs();
            if entry.phase == 2 {
                running += 1;
            } else {
                position += 1;
                oldest = oldest.max(waited_secs);
            }
            WireCompileWait {
                token: entry.token.clone(),
                phase: entry.phase,
                position: if entry.phase == 2 { 0 } else { position },
                waited_secs,
            }
        })
        .collect();
    WireAdmissionProgress {
        queued: position,
        running,
        oldest_wait_secs: oldest,
        admissions: r.admissions,
        completions: r.completions,
        completions_last_minute: r.recent.len() as u64,
        waits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queued_clients_progress_through_one_slot_and_cancel_cleanly() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
            let held = gate.clone().acquire_owned().await.unwrap();
            let mut tasks = Vec::new();
            for n in 0..3 {
                let progress = register(format!("queue-test-{n}"));
                let id = progress.0;
                let gate = gate.clone();
                tasks.push(tokio::spawn(CURRENT.scope(id, async move {
                    phase(1);
                    let _permit = gate.acquire_owned().await.unwrap();
                    phase(2);
                    tokio::task::yield_now().await;
                    progress.complete();
                })));
            }
            tokio::task::yield_now().await;
            assert_eq!(
                snapshot()
                    .waits
                    .iter()
                    .filter(|w| w.token.starts_with("queue-test-"))
                    .count(),
                3
            );
            drop(held);
            for task in tasks {
                task.await.unwrap();
            }
            assert!(!snapshot()
                .waits
                .iter()
                .any(|w| w.token.starts_with("queue-test-")));
        });
    }
}
