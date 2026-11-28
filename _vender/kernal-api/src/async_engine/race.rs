//! Ordered, cancellation-safe polling primitives for borrowed futures.

/// Winner of a two-branch biased race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BiasedRace2<A, B> {
    First(A),
    Second(B),
}
/// Winner of a three-branch biased race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BiasedRace3<A, B, C> {
    First(A),
    Second(B),
    Third(C),
}
/// Winner of a four-branch biased race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BiasedRace4<A, B, C, D> {
    First(A),
    Second(B),
    Third(C),
    Fourth(D),
}

/// Winner of a fair two-branch race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FairRace2<A, B> {
    First(A),
    Second(B),
}

/// Winner of a fair four-branch race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FairRace4<A, B, C, D> {
    First(A),
    Second(B),
    Third(C),
    Fourth(D),
}
/// Winner of a fair three-branch race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FairRace3<A, B, C> {
    First(A),
    Second(B),
    Third(C),
}
/// Winner of a fair five-branch race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FairRace5<A, B, C, D, E> {
    First(A),
    Second(B),
    Third(C),
    Fourth(D),
    Fifth(E),
}

use std::sync::atomic::{AtomicU64, Ordering};

static FAIR_RACE_SEED: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

#[doc(hidden)]
pub fn fair_start(branches: usize) -> usize {
    let mut state = FAIR_RACE_SEED.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    state ^= state >> 30;
    state = state.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94D0_49BB_1331_11EB);
    (state ^ (state >> 31)) as usize % branches
}

/// Poll two to five pinned futures with a pseudorandomized first poller.
///
/// Each expression must yield a `Pin<&mut Future>` (for example from
/// [`std::pin::pin!`]) and may carry a `; guard` boolean. The winner is
/// returned as [`FairRace2`](crate::async_engine::FairRace2) through
/// [`FairRace5`](crate::async_engine::FairRace5) by branch count.
///
///
/// A process-global atomic SplitMix-style state chooses a start once per
/// invocation. It is deterministic/pseudorandom rather than entropy-backed,
/// and does not promise Tokio's distribution or contention behavior. It does
/// preserve the important non-fixed-order property; it is not a starvation-
/// proof guarantee.
/// Guards are evaluated once, in source order, before all future expressions.
/// Disabled inputs are constructed but never polled. All-disabled selections
/// panic. Inputs are borrowed pinned futures: dropping the race does not drop
/// their owners, and pending inputs remain available after a winner is returned.
#[macro_export]
macro_rules! fair_race {
    (($first:expr $(; $first_guard:expr)?), ($second:expr $(; $second_guard:expr)?), ($third:expr $(; $third_guard:expr)?), ($fourth:expr $(; $fourth_guard:expr)?), ($fifth:expr $(; $fifth_guard:expr)?) $(,)?) => {{
        async {
            let first_enabled = true $(&& $first_guard)?; let second_enabled = true $(&& $second_guard)?; let third_enabled = true $(&& $third_guard)?; let fourth_enabled = true $(&& $fourth_guard)?; let fifth_enabled = true $(&& $fifth_guard)?;
            let mut first = $first; let mut second = $second; let mut third = $third; let mut fourth = $fourth; let mut fifth = $fifth;
            let start = $crate::async_engine::__fair_race_start(5);
            std::future::poll_fn(move |cx| {
                if !first_enabled && !second_enabled && !third_enabled && !fourth_enabled && !fifth_enabled { panic!("fair race has no enabled branches"); }
                for branch in [start, (start+1)%5, (start+2)%5, (start+3)%5, (start+4)%5] {
                    match branch {
                        0 if first_enabled => if let std::task::Poll::Ready(v) = std::future::Future::poll(first.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::FairRace5::First(v)); },
                        1 if second_enabled => if let std::task::Poll::Ready(v) = std::future::Future::poll(second.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::FairRace5::Second(v)); },
                        2 if third_enabled => if let std::task::Poll::Ready(v) = std::future::Future::poll(third.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::FairRace5::Third(v)); },
                        3 if fourth_enabled => if let std::task::Poll::Ready(v) = std::future::Future::poll(fourth.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::FairRace5::Fourth(v)); },
                        4 if fifth_enabled => if let std::task::Poll::Ready(v) = std::future::Future::poll(fifth.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::FairRace5::Fifth(v)); },
                        _ => {}
                    }
                }
                std::task::Poll::Pending
            }).await
        }
    }};
    (($first:expr $(; $first_guard:expr)?), ($second:expr $(; $second_guard:expr)?), ($third:expr $(; $third_guard:expr)?), ($fourth:expr $(; $fourth_guard:expr)?) $(,)?) => {{
        async {
            let enabled = [
                true $(&& $first_guard)?, true $(&& $second_guard)?,
                true $(&& $third_guard)?, true $(&& $fourth_guard)?,
            ];
            let mut first = $first;
            let mut second = $second;
            let mut third = $third;
            let mut fourth = $fourth;
            let start = $crate::async_engine::__fair_race_start(4);
            std::future::poll_fn(move |cx| {
                assert!(enabled.iter().any(|value| *value), "fair race has no enabled branches");
                for offset in 0..4 {
                    let branch = (start + offset) % 4;
                    if !enabled[branch] { continue; }
                    match branch {
                        0 => if let std::task::Poll::Ready(value) = std::future::Future::poll(first.as_mut(), cx) {
                            return std::task::Poll::Ready($crate::async_engine::FairRace4::First(value));
                        },
                        1 => if let std::task::Poll::Ready(value) = std::future::Future::poll(second.as_mut(), cx) {
                            return std::task::Poll::Ready($crate::async_engine::FairRace4::Second(value));
                        },
                        2 => if let std::task::Poll::Ready(value) = std::future::Future::poll(third.as_mut(), cx) {
                            return std::task::Poll::Ready($crate::async_engine::FairRace4::Third(value));
                        },
                        _ => if let std::task::Poll::Ready(value) = std::future::Future::poll(fourth.as_mut(), cx) {
                            return std::task::Poll::Ready($crate::async_engine::FairRace4::Fourth(value));
                        },
                    }
                }
                std::task::Poll::Pending
            }).await
        }
    }};
    (($first:expr $(; $first_guard:expr)?), ($second:expr $(; $second_guard:expr)?), ($third:expr $(; $third_guard:expr)?) $(,)?) => {{
        async {
            let first_enabled = true $(&& $first_guard)?;
            let second_enabled = true $(&& $second_guard)?;
            let third_enabled = true $(&& $third_guard)?;
            let mut first = $first; let mut second = $second; let mut third = $third;
            let start = $crate::async_engine::__fair_race_start(3);
            std::future::poll_fn(move |cx| {
                if !first_enabled && !second_enabled && !third_enabled { panic!("fair race has no enabled branches"); }
                for branch in [start, (start + 1) % 3, (start + 2) % 3] {
                    match branch {
                        0 if first_enabled => if let std::task::Poll::Ready(v) = std::future::Future::poll(first.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::FairRace3::First(v)); },
                        1 if second_enabled => if let std::task::Poll::Ready(v) = std::future::Future::poll(second.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::FairRace3::Second(v)); },
                        2 if third_enabled => if let std::task::Poll::Ready(v) = std::future::Future::poll(third.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::FairRace3::Third(v)); },
                        _ => {}
                    }
                }
                std::task::Poll::Pending
            }).await
        }
    }};
    (($first:expr $(; $first_guard:expr)?), ($second:expr $(; $second_guard:expr)?) $(,)?) => {{
        async {
            let first_enabled = true $(&& $first_guard)?;
            let second_enabled = true $(&& $second_guard)?;
            let mut first = $first;
            let mut second = $second;
            let first_turn = $crate::async_engine::__fair_race_start(2) == 0;
            std::future::poll_fn(move |cx| {
                if !first_enabled && !second_enabled { panic!("fair race has no enabled branches"); }
                if first_turn {
                    if first_enabled { if let std::task::Poll::Ready(v) = std::future::Future::poll(first.as_mut(), cx) {
                        return std::task::Poll::Ready($crate::async_engine::FairRace2::First(v));
                    } }
                    if second_enabled { if let std::task::Poll::Ready(v) = std::future::Future::poll(second.as_mut(), cx) {
                        return std::task::Poll::Ready($crate::async_engine::FairRace2::Second(v));
                    } }
                } else {
                    if second_enabled { if let std::task::Poll::Ready(v) = std::future::Future::poll(second.as_mut(), cx) {
                        return std::task::Poll::Ready($crate::async_engine::FairRace2::Second(v));
                    } }
                    if first_enabled { if let std::task::Poll::Ready(v) = std::future::Future::poll(first.as_mut(), cx) {
                        return std::task::Poll::Ready($crate::async_engine::FairRace2::First(v));
                    } }
                }
                std::task::Poll::Pending
            }).await
        }
    }};
}

/// Poll two to four pinned futures in written order until one is ready.
///
/// The winner is returned as [`BiasedRace2`](crate::async_engine::BiasedRace2)
/// through [`BiasedRace4`](crate::async_engine::BiasedRace4) by branch count.
///
/// Each expression must yield a `Pin<&mut Future>` (for example from
/// [`std::pin::pin!`]); guards are snapshotted before any future expression is
/// evaluated, matching `select!`. All-disabled races panic (there is no
/// `else` branch). The returned future
/// borrows its inputs, so dropping it cancels neither input nor any unrelated
/// owner -- it merely stops polling the losing borrowed futures.
#[macro_export]
macro_rules! biased_race {
    (($first:expr $(; $first_guard:expr)?), ($second:expr $(; $second_guard:expr)?) $(,)?) => {{
        async {
            let first_enabled = true $(&& $first_guard)?;
            let second_enabled = true $(&& $second_guard)?;
            let mut first = $first;
            let mut second = $second;
            std::future::poll_fn(move |cx| {
                if !first_enabled && !second_enabled { panic!("biased race has no enabled branches"); }
                if first_enabled {
                    if let std::task::Poll::Ready(value) = std::future::Future::poll(first.as_mut(), cx) {
                        return std::task::Poll::Ready($crate::async_engine::BiasedRace2::First(value));
                    }
                }
                if second_enabled {
                    if let std::task::Poll::Ready(value) = std::future::Future::poll(second.as_mut(), cx) {
                        return std::task::Poll::Ready($crate::async_engine::BiasedRace2::Second(value));
                    }
                }
                std::task::Poll::Pending
            }).await
        }
    }};
    (($first:expr $(; $first_guard:expr)?), ($second:expr $(; $second_guard:expr)?), ($third:expr $(; $third_guard:expr)?) $(,)?) => {{
        async {
            let first_enabled = true $(&& $first_guard)?; let second_enabled = true $(&& $second_guard)?; let third_enabled = true $(&& $third_guard)?;
            let mut first = $first; let mut second = $second; let mut third = $third;
            std::future::poll_fn(move |cx| {
                if !first_enabled && !second_enabled && !third_enabled { panic!("biased race has no enabled branches"); }
                if first_enabled { if let std::task::Poll::Ready(v) = std::future::Future::poll(first.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::BiasedRace3::First(v)); } }
                if second_enabled { if let std::task::Poll::Ready(v) = std::future::Future::poll(second.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::BiasedRace3::Second(v)); } }
                if third_enabled { if let std::task::Poll::Ready(v) = std::future::Future::poll(third.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::BiasedRace3::Third(v)); } }
                std::task::Poll::Pending
            }).await
        }
    }};
    (($first:expr $(; $first_guard:expr)?), ($second:expr $(; $second_guard:expr)?), ($third:expr $(; $third_guard:expr)?), ($fourth:expr $(; $fourth_guard:expr)?) $(,)?) => {{
        async {
            let first_enabled = true $(&& $first_guard)?; let second_enabled = true $(&& $second_guard)?; let third_enabled = true $(&& $third_guard)?; let fourth_enabled = true $(&& $fourth_guard)?;
            let mut first = $first; let mut second = $second; let mut third = $third; let mut fourth = $fourth;
            std::future::poll_fn(move |cx| {
                if !first_enabled && !second_enabled && !third_enabled && !fourth_enabled { panic!("biased race has no enabled branches"); }
                if first_enabled { if let std::task::Poll::Ready(v) = std::future::Future::poll(first.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::BiasedRace4::First(v)); } }
                if second_enabled { if let std::task::Poll::Ready(v) = std::future::Future::poll(second.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::BiasedRace4::Second(v)); } }
                if third_enabled { if let std::task::Poll::Ready(v) = std::future::Future::poll(third.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::BiasedRace4::Third(v)); } }
                if fourth_enabled { if let std::task::Poll::Ready(v) = std::future::Future::poll(fourth.as_mut(), cx) { return std::task::Poll::Ready($crate::async_engine::BiasedRace4::Fourth(v)); } }
                std::task::Poll::Pending
            }).await
        }
    }};
}
