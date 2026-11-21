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
    use std::future::Future;
    #[test]
    fn resident_compile_admission_queued_clients_progress_and_cancel_cleanly() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let gate = crate::resident_compile_admission::ResidentCompileAdmission::new(1);
            let held = gate.acquire_compiler().await.unwrap();
            let mut guards: Vec<_> = (0..3)
                .map(|n| register(format!("queue-test-{n}")))
                .collect();
            let mut waiters: Vec<_> = guards
                .iter()
                .map(|guard| Box::pin(CURRENT.scope(guard.0, gate.acquire_compiler())))
                .collect();
            for waiter in &mut waiters {
                let polled =
                    std::future::poll_fn(|cx| std::task::Poll::Ready(waiter.as_mut().poll(cx)))
                        .await;
                assert!(polled.is_pending());
            }
            let queued = snapshot();
            let own: Vec<_> = queued
                .waits
                .iter()
                .filter(|w| w.token.starts_with("queue-test-"))
                .collect();
            assert_eq!(own.len(), 3);
            assert!(own.iter().all(|w| w.phase == 1));
            drop(waiters.remove(1));
            drop(guards.remove(1));
            assert_eq!(
                snapshot()
                    .waits
                    .iter()
                    .filter(|w| w.token.starts_with("queue-test-"))
                    .count(),
                2
            );
            drop(held);
            for (waiter, guard) in waiters.into_iter().zip(guards) {
                let permit = waiter.await.unwrap();
                let admitted = snapshot();
                assert!(admitted
                    .waits
                    .iter()
                    .any(|w| w.token.starts_with("queue-test-") && w.phase == 2));
                guard.complete();
                drop(permit);
            }
            assert!(!snapshot()
                .waits
                .iter()
                .any(|w| w.token.starts_with("queue-test-")));
        });
    }
}
