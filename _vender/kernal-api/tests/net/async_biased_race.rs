//! Public macro contract; deliberately do not import the Future trait.

use kernal_api::async_engine::{BiasedRace2, BiasedRace3, BiasedRace4};
use std::pin::{pin, Pin};
use std::task::{Context, Poll, Waker};

fn poll_once<F: std::future::Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    std::future::Future::poll(future, &mut Context::from_waker(Waker::noop()))
}

#[test]
fn first_ready_branch_wins_without_consuming_the_loser() {
    let mut first = pin!(std::future::ready(1));
    let mut second = pin!(std::future::ready("retained"));
    {
        let mut race = pin!(kernal_api::biased_race!(
            (first.as_mut()),
            (second.as_mut())
        ));
        assert_eq!(poll_once(race.as_mut()), Poll::Ready(BiasedRace2::First(1)));
    }
    assert_eq!(poll_once(second.as_mut()), Poll::Ready("retained"));
}

#[test]
fn three_and_four_branch_forms_support_distinct_output_types() {
    let mut first = pin!(std::future::pending::<u8>());
    let mut second = pin!(std::future::pending::<String>());
    let mut third = pin!(std::future::ready(true));
    {
        let mut race = pin!(kernal_api::biased_race!(
            (first.as_mut()),
            (second.as_mut()),
            (third.as_mut()),
        ));
        assert_eq!(
            poll_once(race.as_mut()),
            Poll::Ready(BiasedRace3::Third(true))
        );
    }
    let mut third = pin!(std::future::pending::<bool>());
    let mut fourth = pin!(std::future::ready(42_u64));
    let mut race = pin!(kernal_api::biased_race!(
        (first.as_mut()),
        (second.as_mut()),
        (third.as_mut()),
        (fourth.as_mut()),
    ));
    assert_eq!(
        poll_once(race.as_mut()),
        Poll::Ready(BiasedRace4::Fourth(42))
    );
}

#[test]
fn guard_is_snapshotted_once_even_when_race_is_polled_again() {
    let calls = std::cell::Cell::new(0);
    let mut first = pin!(std::future::pending::<()>());
    let mut second = pin!(std::future::pending::<()>());
    let mut race = pin!(kernal_api::biased_race!(
        (first.as_mut(); { calls.set(calls.get() + 1); true }),
        (second.as_mut()),
    ));
    assert!(poll_once(race.as_mut()).is_pending());
    assert!(poll_once(race.as_mut()).is_pending());
    assert_eq!(calls.get(), 1);
}

#[test]
fn all_guards_precede_future_expressions_and_disabled_future_is_not_polled() {
    let events = std::cell::RefCell::new(Vec::new());
    let mut first = pin!(std::future::poll_fn(|_| -> Poll<()> {
        panic!("disabled future was polled");
    }));
    let mut second = pin!(std::future::ready(7));
    let mut race = pin!(kernal_api::biased_race!(
        ({ events.borrow_mut().push("future 1"); first.as_mut() };
         { events.borrow_mut().push("guard 1"); false }),
        ({ events.borrow_mut().push("future 2"); second.as_mut() };
         { events.borrow_mut().push("guard 2"); true }),
    ));
    assert_eq!(
        poll_once(race.as_mut()),
        Poll::Ready(BiasedRace2::Second(7))
    );
    assert_eq!(
        *events.borrow(),
        ["guard 1", "guard 2", "future 1", "future 2"]
    );
}

#[test]
#[should_panic(expected = "biased race has no enabled branches")]
fn all_disabled_branches_fail_instead_of_hanging() {
    let mut first = pin!(std::future::pending::<()>());
    let mut second = pin!(std::future::pending::<()>());
    let mut race = pin!(kernal_api::biased_race!(
        (first.as_mut(); false), (second.as_mut(); false),
    ));
    let _ = poll_once(race.as_mut());
}
