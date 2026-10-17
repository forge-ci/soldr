//! soldr#3053 item 4 — process-global registry of the daemon's own
//! kill/cancel decisions, so a breach dump can list the ones pending (and
//! recently resolved) at breach time. Mirrors
//! `daemon::inflight_compiles` exactly in shape: a process-global
//! `static Mutex`, a monotonic `SEQ`, an RAII guard, and a `snapshot()`
//! free function -- the reader is the same breach-dump path and has no
//! handle to the thing making the decision, only the process it runs in.
//!
//! Two kinds of decision are recorded today: `terminate-pid` (lifecycle's
//! SIGTERM-then-SIGKILL escalation) and `cancel-on-disconnect` (dropping an
//! in-flight compile future when the IPC client goes away). Both are
//! "the daemon decided to end something", which is exactly the context a
//! breach dump wants next to the RSS numbers and the in-flight compile
//! list: not just what the daemon was holding, but what it was already
//! doing about it.

use crate::platform::sync::Mutex;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bound on the "recent" ring. Resolved decisions older than the last 32
/// are evicted -- a breach dump only needs recent context, not a full
/// history, and an unbounded ring would be an unbounded-growth risk in a
/// long-lived daemon.
const RECENT_CAPACITY: usize = 32;

/// One kill/cancel decision, as seen by a breach dump reader.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct KillDecision {
    pub id: u64,
    /// `"terminate-pid"` | `"cancel-on-disconnect"`.
    pub kind: String,
    pub target_pid: Option<u32>,
    pub reason: String,
    pub escalated_to_sigkill: bool,
    pub decided_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
    pub outcome: Option<String>,
}

/// Snapshot of the registry: decisions still open, and the most recent
/// resolved ones.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct KillDecisionsSnapshot {
    pub pending: Vec<KillDecision>,
    pub recent: Vec<KillDecision>,
}

/// Registry entry. Mutable in place so `mark_escalated`/`resolve` do not
/// need to re-insert.
struct Entry {
    kind: String,
    target_pid: Option<u32>,
    reason: String,
    escalated_to_sigkill: bool,
    decided_at_ms: i64,
}

impl Entry {
    fn to_pending(&self, id: u64) -> KillDecision {
        KillDecision {
            id,
            kind: self.kind.clone(),
            target_pid: self.target_pid,
            reason: self.reason.clone(),
            escalated_to_sigkill: self.escalated_to_sigkill,
            decided_at_ms: self.decided_at_ms,
            resolved_at_ms: None,
            outcome: None,
        }
    }
}

/// Monotonic id source. Mirrors `inflight_compiles::SEQ` -- a plain
/// `fetch_add` is sufficient because uniqueness, not ordering across
/// restarts, is all that is needed.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Decisions still open (not yet resolved).
static PENDING: Mutex<BTreeMap<u64, Entry>> = Mutex::new(BTreeMap::new());

/// Bounded ring of the most recently resolved decisions, oldest evicted
/// first once `RECENT_CAPACITY` is exceeded.
static RECENT: Mutex<VecDeque<KillDecision>> = Mutex::new(VecDeque::new());

/// RAII handle for one recorded decision. Dropping it without calling
/// [`KillDecisionGuard::resolve`] resolves it with outcome `"dropped"` --
/// including on an early return or a panic unwind -- so no decision can be
/// silently lost from the pending set.
pub(crate) struct KillDecisionGuard {
    id: u64,
    resolved: bool,
}

impl KillDecisionGuard {
    /// Mark the decision as having escalated to SIGKILL. Idempotent-safe
    /// to call more than once; only affects entries still pending.
    pub(crate) fn mark_escalated(&self) {
        let mut map = PENDING.lock();
        if let Some(entry) = map.get_mut(&self.id) {
            entry.escalated_to_sigkill = true;
        }
    }

    /// Resolve the decision with the given outcome, moving it from
    /// `pending` into the `recent` ring.
    pub(crate) fn resolve(mut self, outcome: &str) {
        self.resolve_with(outcome);
    }

    fn resolve_with(&mut self, outcome: &str) {
        if self.resolved {
            return;
        }
        self.resolved = true;
        let mut pending = PENDING.lock();
        let Some(entry) = pending.remove(&self.id) else {
            return;
        };
        drop(pending);

        let mut decision = entry.to_pending(self.id);
        decision.resolved_at_ms = Some(unix_millis());
        decision.outcome = Some(outcome.to_string());

        let mut recent = RECENT.lock();
        recent.push_back(decision);
        while recent.len() > RECENT_CAPACITY {
            recent.pop_front();
        }
    }
}

impl Drop for KillDecisionGuard {
    fn drop(&mut self) {
        // Poison-proof for the same reason as `inflight_compiles::Drop`:
        // this can run during unwind while the process is already in a bad
        // state, and a panic here would only make things worse.
        self.resolve_with("dropped");
    }
}

/// Record a new kill/cancel decision and return a guard. Call
/// [`KillDecisionGuard::resolve`] on every reachable return path; a guard
/// dropped without resolving is recorded with outcome `"dropped"`.
pub(crate) fn record(kind: &str, target_pid: Option<u32>, reason: String) -> KillDecisionGuard {
    let id = SEQ.fetch_add(1, Ordering::Relaxed);
    let entry = Entry {
        kind: kind.to_string(),
        target_pid,
        reason,
        escalated_to_sigkill: false,
        decided_at_ms: unix_millis(),
    };
    let mut map = PENDING.lock();
    map.insert(id, entry);
    KillDecisionGuard {
        id,
        resolved: false,
    }
}

/// Snapshot both the pending set and the recent-resolved ring. Never holds
/// either lock across I/O -- both locks are released before this function
/// returns, well before any caller writes the result to disk.
pub(crate) fn snapshot() -> KillDecisionsSnapshot {
    let pending = {
        let map = PENDING.lock();
        map.iter()
            .map(|(&id, entry)| entry.to_pending(id))
            .collect()
    };
    let recent = {
        let ring = RECENT.lock();
        ring.iter().cloned().collect()
    };
    KillDecisionsSnapshot { pending, recent }
}

/// Identical in behaviour to `inflight_compiles::unix_millis` -- duplicated
/// rather than shared for the same reason: that module's copy is private
/// and this module has no dependency on it.
fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is process-global and nextest/libtest run every test in
    /// a binary as threads of one process, so these tests must never assert
    /// on the total length of `snapshot()` -- another test's entries may be
    /// alive concurrently. Each test uses its own unique reason string and
    /// filters `snapshot()` down to just those (see the same comment on
    /// `inflight_compiles`'s tests / `rss_ceiling.rs` ~L860).

    #[test]
    fn record_shows_in_pending() {
        let guard = record(
            "terminate-pid",
            Some(4242),
            "t-record-pending: unique reason".to_string(),
        );

        let found = snapshot()
            .pending
            .into_iter()
            .find(|d| d.reason == "t-record-pending: unique reason")
            .expect("recorded decision must appear in pending");
        assert_eq!(found.kind, "terminate-pid");
        assert_eq!(found.target_pid, Some(4242));
        assert!(!found.escalated_to_sigkill);
        assert!(found.resolved_at_ms.is_none());
        assert!(found.outcome.is_none());

        guard.resolve("test-cleanup");
    }

    #[test]
    fn resolve_moves_to_recent_with_outcome() {
        let guard = record(
            "cancel-on-disconnect",
            None,
            "t-resolve-recent: unique reason".to_string(),
        );
        guard.resolve("compile-future-dropped");

        let still_pending = snapshot()
            .pending
            .iter()
            .any(|d| d.reason == "t-resolve-recent: unique reason");
        assert!(!still_pending, "resolved decision must leave pending");

        let found = snapshot()
            .recent
            .into_iter()
            .find(|d| d.reason == "t-resolve-recent: unique reason")
            .expect("resolved decision must appear in recent");
        assert_eq!(found.outcome.as_deref(), Some("compile-future-dropped"));
        assert!(found.resolved_at_ms.is_some());
    }

    #[test]
    fn drop_without_resolve_yields_dropped_outcome() {
        {
            let _guard = record(
                "terminate-pid",
                Some(9999),
                "t-drop-yields-dropped: unique reason".to_string(),
            );
            // Guard falls out of scope here without calling `resolve`.
        }

        let found = snapshot()
            .recent
            .into_iter()
            .find(|d| d.reason == "t-drop-yields-dropped: unique reason")
            .expect("dropped decision must appear in recent");
        assert_eq!(found.outcome.as_deref(), Some("dropped"));
    }

    #[test]
    fn recent_ring_is_bounded_at_capacity() {
        let prefix = "t-ring-bounded: unique reason ";
        for i in 0..(RECENT_CAPACITY + 5) {
            record("terminate-pid", None, format!("{prefix}{i}")).resolve("ok");
        }

        let count = snapshot()
            .recent
            .iter()
            .filter(|d| d.reason.starts_with(prefix))
            .count();
        assert!(
            count <= RECENT_CAPACITY,
            "recent ring must never exceed RECENT_CAPACITY entries, got {count}"
        );
    }

    #[test]
    fn mark_escalated_is_reflected_in_pending_and_recent() {
        let guard = record(
            "terminate-pid",
            Some(5555),
            "t-mark-escalated: unique reason".to_string(),
        );
        guard.mark_escalated();

        let pending = snapshot()
            .pending
            .into_iter()
            .find(|d| d.reason == "t-mark-escalated: unique reason")
            .expect("decision must still be pending after mark_escalated");
        assert!(pending.escalated_to_sigkill);

        guard.resolve("sigkill-sent");

        let recent = snapshot()
            .recent
            .into_iter()
            .find(|d| d.reason == "t-mark-escalated: unique reason")
            .expect("resolved decision must appear in recent");
        assert!(recent.escalated_to_sigkill);
    }
}
