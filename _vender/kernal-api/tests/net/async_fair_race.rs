//! Characterization of selection across independent invocations.

use kernal_api::async_engine::{FairRace2, FairRace3, FairRace4, FairRace5};
use std::pin::{pin, Pin};
use std::task::{Context, Poll, Waker};

fn poll_once<F: std::future::Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    std::future::Future::poll(future, &mut Context::from_waker(Waker::noop()))
}

#[test]
fn five_branch_guards_select_each_enabled_branch() {
    for enabled in 0..5 {
        let mut first = pin!(std::future::ready(1_u8));
        let mut second = pin!(std::future::ready("two"));
        let mut third = pin!(std::future::ready(true));
        let mut fourth = pin!(std::future::ready(4_u64));
        let mut fifth = pin!(std::future::ready(Some(5_i32)));
        let mut race = pin!(kernal_api::fair_race!(
            (first.as_mut(); enabled == 0), (second.as_mut(); enabled == 1),
            (third.as_mut(); enabled == 2), (fourth.as_mut(); enabled == 3),
            (fifth.as_mut(); enabled == 4),
        ));
        let winner = match poll_once(race.as_mut()) {
            Poll::Ready(FairRace5::First(1)) => 0,
            Poll::Ready(FairRace5::Second("two")) => 1,
            Poll::Ready(FairRace5::Third(true)) => 2,
            Poll::Ready(FairRace5::Fourth(4)) => 3,
            Poll::Ready(FairRace5::Fifth(Some(5))) => 4,
            other => panic!("unexpected result: {other:?}"),
        };
        assert_eq!(winner, enabled);
    }
}

#[test]
#[should_panic(expected = "fair race has no enabled branches")]
fn five_disabled_branches_fail_instead_of_hanging() {
    let mut first = pin!(std::future::pending::<()>());
    let mut second = pin!(std::future::pending::<()>());
    let mut third = pin!(std::future::pending::<()>());
    let mut fourth = pin!(std::future::pending::<()>());
    let mut fifth = pin!(std::future::pending::<()>());
    let mut race = pin!(kernal_api::fair_race!(
        (first.as_mut(); false), (second.as_mut(); false),
        (third.as_mut(); false), (fourth.as_mut(); false), (fifth.as_mut(); false),
    ));
    let _ = poll_once(race.as_mut());
}

#[test]
fn four_branch_guards_select_each_enabled_branch() {
    for enabled in 0..4 {
        let mut first = pin!(std::future::ready(1_u8));
        let mut second = pin!(std::future::ready("two"));
        let mut third = pin!(std::future::ready(true));
        let mut fourth = pin!(std::future::ready(4_u64));
        let mut race = pin!(kernal_api::fair_race!(
            (first.as_mut(); enabled == 0), (second.as_mut(); enabled == 1),
            (third.as_mut(); enabled == 2), (fourth.as_mut(); enabled == 3),
        ));
        let winner = match poll_once(race.as_mut()) {
            Poll::Ready(FairRace4::First(1)) => 0,
            Poll::Ready(FairRace4::Second("two")) => 1,
            Poll::Ready(FairRace4::Third(true)) => 2,
            Poll::Ready(FairRace4::Fourth(4)) => 3,
            other => panic!("unexpected result: {other:?}"),
        };
        assert_eq!(winner, enabled);
    }
}

#[test]
fn three_branch_guards_select_each_enabled_branch() {
    for enabled in 0..3 {
        let mut first = pin!(std::future::ready(1_u8));
        let mut second = pin!(std::future::ready("two"));
        let mut third = pin!(std::future::ready(true));
        let mut race = pin!(kernal_api::fair_race!(
            (first.as_mut(); enabled == 0), (second.as_mut(); enabled == 1),
            (third.as_mut(); enabled == 2),
        ));
        let winner = match poll_once(race.as_mut()) {
            Poll::Ready(FairRace3::First(1)) => 0,
            Poll::Ready(FairRace3::Second("two")) => 1,
            Poll::Ready(FairRace3::Third(true)) => 2,
            other => panic!("unexpected result: {other:?}"),
        };
        assert_eq!(winner, enabled);
    }
}

#[test]
fn four_pending_branches_are_all_polled_and_guards_are_not_repeated() {
    let polls = std::cell::RefCell::new([0; 4]);
    let guards = std::cell::Cell::new(0);
    let pending = |index: usize| {
        let polls = &polls;
        std::future::poll_fn(move |_| {
            polls.borrow_mut()[index] += 1;
            Poll::<()>::Pending
        })
    };
    let guard = || {
        guards.set(guards.get() + 1);
        true
    };
    let mut first = pin!(pending(0));
    let mut second = pin!(pending(1));
    let mut third = pin!(pending(2));
    let mut fourth = pin!(pending(3));
    let mut race = pin!(kernal_api::fair_race!(
        (first.as_mut(); guard()), (second.as_mut(); guard()),
        (third.as_mut(); guard()), (fourth.as_mut(); guard()),
    ));
    assert!(poll_once(race.as_mut()).is_pending());
    assert_eq!(*polls.borrow(), [1; 4]);
    assert!(poll_once(race.as_mut()).is_pending());
    assert_eq!(*polls.borrow(), [2; 4]);
    assert_eq!(guards.get(), 4);
}

#[test]
fn repeated_ready_selections_do_not_always_prioritize_one_branch() {
    let mut wins = [0_usize; 2];
    for _ in 0..1024 {
        let mut first = pin!(std::future::ready(()));
        let mut second = pin!(std::future::ready(()));
        let mut race = pin!(kernal_api::fair_race!((first.as_mut()), (second.as_mut())));
        match poll_once(race.as_mut()) {
            Poll::Ready(FairRace2::First(())) => wins[0] += 1,
            Poll::Ready(FairRace2::Second(())) => wins[1] += 1,
            Poll::Pending => panic!("both branches are ready"),
        }
    }
    // This is a starvation regression, not a distribution or bounded-fairness
    // promise. Pseudorandom starts need not split wins equally, and this sample
    // does not establish independence or a probability bound.
    assert!(
        wins.iter().all(|count| *count > 0),
        "fixed branch priority: {wins:?}"
    );
}

#[test]
fn cancelling_a_pending_race_keeps_both_borrowed_futures_alive() {
    struct PendingDrop<'a>(&'a std::cell::Cell<usize>);
    impl std::future::Future for PendingDrop<'_> {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            Poll::Pending
        }
    }
    impl Drop for PendingDrop<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let drops = std::cell::Cell::new(0);
    {
        let mut first = pin!(PendingDrop(&drops));
        let mut second = pin!(PendingDrop(&drops));
        {
            let mut race = pin!(kernal_api::fair_race!((first.as_mut()), (second.as_mut())));
            assert!(poll_once(race.as_mut()).is_pending());
        }
        assert_eq!(
            drops.get(),
            0,
            "race cancellation must not drop its borrowed inputs"
        );
        assert!(poll_once(first.as_mut()).is_pending());
        assert!(poll_once(second.as_mut()).is_pending());
    }
    assert_eq!(drops.get(), 2, "input owners still perform destruction");
}

#[test]
fn a_pending_branch_can_wake_the_race_and_later_win() {
    struct CountWake(std::sync::atomic::AtomicUsize);
    impl std::task::Wake for CountWake {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let wake_count = std::sync::Arc::new(CountWake(std::sync::atomic::AtomicUsize::new(0)));
    let waker = Waker::from(wake_count.clone());
    let mut context = Context::from_waker(&waker);
    let ready = std::cell::Cell::new(false);
    let registered = std::cell::RefCell::new(None::<Waker>);
    let mut first = pin!(std::future::pending::<()>());
    let mut second = pin!(std::future::poll_fn(|cx| {
        if ready.get() {
            Poll::Ready(23)
        } else {
            *registered.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        }
    }));
    let mut race = pin!(kernal_api::fair_race!((first.as_mut()), (second.as_mut())));
    assert!(std::future::Future::poll(race.as_mut(), &mut context).is_pending());
    ready.set(true);
    registered
        .borrow_mut()
        .take()
        .expect("branch received race waker")
        .wake();
    assert_eq!(wake_count.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        std::future::Future::poll(race.as_mut(), &mut context),
        Poll::Ready(FairRace2::Second(23))
    );
}
