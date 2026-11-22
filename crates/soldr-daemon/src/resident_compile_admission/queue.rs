//! Weighted daemon capacity, with lint preference and aging for ordinary work.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::platform::sync::Mutex;
use tokio::sync::Notify;

const ORDINARY_GRACE: Duration = Duration::from_millis(250);
const ORDINARY_MAX_PREFERENCE_DELAY: Duration = Duration::from_secs(2);
const CREDIT_LIMIT_SLOT_NS: i128 = 2_000_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum AdmissionClass {
    #[default]
    Ordinary,
    Lint,
}

impl AdmissionClass {
    fn index(self) -> usize {
        match self {
            Self::Ordinary => 0,
            Self::Lint => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WorkKind {
    Compiler { exclusive: bool },
    Resident,
}

pub(super) struct CapacityQueue(Mutex<State>);

#[derive(Debug)]
struct State {
    max_resident: usize,
    resident_slots: usize,
    exclusive_active: bool,
    available: usize,
    pending: VecDeque<Arc<Waiter>>,
    previous: AdmissionClass,
    active: [usize; 2],
    ordinary_credit: i128,
    last_contended_at: Option<Instant>,
    accounted_at: Instant,
}

#[derive(Debug)]
struct Waiter {
    slots: usize,
    kind: WorkKind,
    class: AdmissionClass,
    queued_at: Instant,
    ready: Notify,
}

impl State {
    fn account(&mut self, now: Instant) {
        if now <= self.accounted_at {
            return;
        }
        let elapsed = now.saturating_duration_since(self.accounted_at);
        self.accounted_at = now;
        let has_work = |class: AdmissionClass| {
            self.active[class.index()] > 0 || self.pending.iter().any(|entry| entry.class == class)
        };
        if has_work(AdmissionClass::Ordinary) && has_work(AdmissionClass::Lint) {
            self.last_contended_at = Some(now);
            let delta = self.active[AdmissionClass::Lint.index()] as i128
                - self.active[AdmissionClass::Ordinary.index()] as i128;
            self.ordinary_credit = self
                .ordinary_credit
                .saturating_add(delta.saturating_mul(elapsed.as_nanos() as i128))
                .clamp(-CREDIT_LIMIT_SLOT_NS, CREDIT_LIMIT_SLOT_NS);
        } else if self.last_contended_at.is_some_and(|previous| {
            now.saturating_duration_since(previous) >= ORDINARY_MAX_PREFERENCE_DELAY
        }) {
            self.ordinary_credit = 0;
            self.last_contended_at = None;
        }
    }

    fn next(&self, now: Instant) -> Option<&Arc<Waiter>> {
        let eligible = |entry: &&Arc<Waiter>| {
            entry.kind != WorkKind::Resident
                || entry.slots <= self.max_resident - self.resident_slots
        };
        let lint = self
            .pending
            .iter()
            .filter(eligible)
            .find(|entry| entry.class == AdmissionClass::Lint);
        let ordinary = self
            .pending
            .iter()
            .filter(eligible)
            .find(|entry| entry.class == AdmissionClass::Ordinary);
        let aged = ordinary.filter(|entry| {
            let age = now.saturating_duration_since(entry.queued_at);
            let preference_spent = self.ordinary_credit >= ORDINARY_GRACE.as_nanos() as i128;
            ((age >= ORDINARY_GRACE || preference_spent)
                && (self.ordinary_credit > 0
                    || (self.ordinary_credit == 0 && self.previous == AdmissionClass::Lint)))
                || (age >= ORDINARY_MAX_PREFERENCE_DELAY && self.previous == AdmissionClass::Lint)
        });
        aged.or(lint).or(ordinary)
    }

    fn wake_next(&self, now: Instant) {
        if let Some(next) = self.next(now).filter(|entry| self.fits(entry)) {
            next.ready.notify_one();
        }
    }

    fn slots(&self, waiter: &Waiter) -> usize {
        if matches!(waiter.kind, WorkKind::Compiler { exclusive: true }) {
            // Existing resident processes may be the parents of the
            // compiler. Drain the other compilers, keeping their leases.
            self.max_resident + 1 - self.resident_slots
        } else {
            waiter.slots
        }
    }

    fn fits(&self, waiter: &Waiter) -> bool {
        !self.exclusive_active && self.slots(waiter) <= self.available
    }
}

impl CapacityQueue {
    pub(super) fn new(slots: usize) -> Self {
        Self(Mutex::new(State {
            max_resident: slots - 1,
            resident_slots: 0,
            exclusive_active: false,
            available: slots,
            pending: VecDeque::new(),
            previous: AdmissionClass::Ordinary,
            active: [0; 2],
            ordinary_credit: 0,
            last_contended_at: None,
            accounted_at: Instant::now(),
        }))
    }

    pub(super) async fn acquire(
        self: &Arc<Self>,
        slots: usize,
        class: AdmissionClass,
        kind: WorkKind,
    ) -> Permit {
        let waiter = Arc::new(Waiter {
            slots,
            kind,
            class,
            queued_at: Instant::now(),
            ready: Notify::new(),
        });
        {
            let mut state = self.0.lock();
            state.account(waiter.queued_at);
            state.pending.push_back(Arc::clone(&waiter));
        }
        let mut queued = Queued {
            queue: self,
            waiter: Some(&waiter),
        };
        loop {
            let now = Instant::now();
            {
                let mut state = self.0.lock();
                state.account(now);
                if state.fits(&waiter)
                    && state
                        .next(now)
                        .is_some_and(|next| Arc::ptr_eq(next, &waiter))
                {
                    state.pending.retain(|entry| !Arc::ptr_eq(entry, &waiter));
                    queued.waiter = None;
                    let slots = state.slots(&waiter);
                    state.available -= slots;
                    state.active[class.index()] += slots;
                    match kind {
                        WorkKind::Resident => state.resident_slots += slots,
                        WorkKind::Compiler { exclusive } => state.exclusive_active = exclusive,
                    }
                    state.previous = class;
                    state.wake_next(now);
                    drop(state);
                    if crate::core::lint_priority::trace_enabled() {
                        tracing::warn!(
                            ?class,
                            slots,
                            ?kind,
                            queue_ms = now.saturating_duration_since(waiter.queued_at).as_millis(),
                            "daemon capacity granted"
                        );
                    }
                    return Permit {
                        queue: Arc::clone(self),
                        slots,
                        kind,
                        class,
                        started_at: now,
                    };
                }
                // A newly queued or newly aged request can supersede a wake
                // already delivered to a different waiter.
                state.wake_next(now);
            }
            waiter.ready.notified().await;
        }
    }

    #[cfg(test)]
    pub(super) fn available(&self) -> usize {
        self.0.lock().available
    }
}

struct Queued<'a> {
    queue: &'a CapacityQueue,
    waiter: Option<&'a Arc<Waiter>>,
}

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        if let Some(waiter) = self.waiter {
            let mut state = self.queue.0.lock();
            let now = Instant::now();
            state.account(now);
            state.pending.retain(|entry| !Arc::ptr_eq(entry, waiter));
            state.wake_next(now);
        }
    }
}

pub struct Permit {
    queue: Arc<CapacityQueue>,
    slots: usize,
    kind: WorkKind,
    class: AdmissionClass,
    started_at: Instant,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let now = Instant::now();
        {
            let mut state = self.queue.0.lock();
            state.account(now);
            state.available += self.slots;
            state.active[self.class.index()] -= self.slots;
            match self.kind {
                WorkKind::Resident => state.resident_slots -= self.slots,
                WorkKind::Compiler { exclusive: true } => state.exclusive_active = false,
                WorkKind::Compiler { exclusive: false } => {}
            }
            state.wake_next(now);
        }
        if crate::core::lint_priority::trace_enabled() {
            tracing::warn!(
                class = ?self.class,
                slots = self.slots,
                kind = ?self.kind,
                slot_ms = now.saturating_duration_since(self.started_at).as_millis()
                    .saturating_mul(self.slots as u128),
                "daemon capacity released"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matched_work_converges_after_the_initial_lint_preference() {
        let mut now = Instant::now();
        let queue = CapacityQueue::new(1);
        let mut state = queue.0.lock();
        let enqueue = |state: &mut State, class, queued_at| {
            state.pending.push_back(Arc::new(Waiter {
                slots: 1,
                kind: WorkKind::Compiler { exclusive: false },
                class,
                queued_at,
                ready: Notify::new(),
            }));
        };
        enqueue(&mut state, AdmissionClass::Ordinary, now);
        enqueue(&mut state, AdmissionClass::Lint, now);
        assert_eq!(
            state.next(now).expect("first grant").class,
            AdmissionClass::Lint
        );

        let mut finished = [0_u32; 2];
        for _ in 0..1_000 {
            let next = Arc::clone(state.next(now).expect("both lanes have work"));
            let class = next.class;
            state.pending.retain(|waiter| !Arc::ptr_eq(waiter, &next));
            state.previous = class;
            state.active[class.index()] = 1;
            now += Duration::from_millis(10);
            state.account(now);
            state.active[class.index()] = 0;
            finished[class.index()] += 1;

            // A sequential producer replenishes its queue after each grant.
            now += Duration::from_millis(1);
            state.account(now);
            enqueue(&mut state, class, now);
        }
        assert!(finished[0].abs_diff(finished[1]) <= 50, "{finished:?}");
    }

    #[test]
    fn aged_ordinary_work_gets_a_turn_between_lint_grants() {
        let now = Instant::now();
        let ordinary = Arc::new(Waiter {
            slots: 2,
            kind: WorkKind::Compiler { exclusive: false },
            class: AdmissionClass::Ordinary,
            queued_at: now,
            ready: Notify::new(),
        });
        let lint = Arc::new(Waiter {
            slots: 1,
            kind: WorkKind::Compiler { exclusive: false },
            class: AdmissionClass::Lint,
            queued_at: now,
            ready: Notify::new(),
        });
        let mut state = State {
            max_resident: 1,
            resident_slots: 0,
            exclusive_active: false,
            available: 2,
            pending: VecDeque::from([Arc::clone(&ordinary), Arc::clone(&lint)]),
            previous: AdmissionClass::Lint,
            active: [0; 2],
            ordinary_credit: 0,
            last_contended_at: None,
            accounted_at: now,
        };

        assert!(Arc::ptr_eq(state.next(now).expect("lint"), &lint));
        let later = now + ORDINARY_GRACE;
        assert!(Arc::ptr_eq(state.next(later).expect("ordinary"), &ordinary));
        state.previous = AdmissionClass::Ordinary;
        assert!(Arc::ptr_eq(state.next(later).expect("lint"), &lint));
        state.active[AdmissionClass::Lint.index()] = 2;
        state.active[AdmissionClass::Ordinary.index()] = 1;
        state.account(later);
        assert!(Arc::ptr_eq(state.next(later).expect("ordinary"), &ordinary));
    }
}
