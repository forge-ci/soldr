use kernal_api::async_engine::{
    self, Deadline, MissedTickBehavior, PeriodicTimer, Semaphore, SemaphoreAcquireTimedOut,
};
use std::time::Duration;

#[test]
fn immediate_permits_share_capacity_and_release_on_drop() {
    let pool = Semaphore::new(1);
    let clone = pool.clone();
    let permit = pool.try_acquire().unwrap();
    assert!(clone.try_acquire().is_none());
    drop(pool);
    drop(permit);
    assert!(clone.try_acquire().is_some());
    assert!(Semaphore::new(0).try_acquire().is_none());
}

#[tokio::test(start_paused = true)]
async fn deadline_waits_keep_the_original_expiry() {
    let deadline = Deadline::after(Duration::from_secs(10));
    async_engine::sleep(Duration::from_secs(4)).await;
    let start = tokio::time::Instant::now();
    async_engine::sleep_until(deadline).await;
    assert_eq!(start.elapsed(), Duration::from_secs(6));
    assert!(deadline.is_elapsed());
    async_engine::sleep_until(deadline).await;
    assert_eq!(start.elapsed(), Duration::from_secs(6));
}

#[tokio::test(start_paused = true)]
async fn periodic_ticks_are_immediate_then_fixed_cadence_with_burst_catchup() {
    let mut timer = PeriodicTimer::new(Duration::from_secs(10)).unwrap();
    let start = tokio::time::Instant::now();
    timer.tick().await;
    assert_eq!(start.elapsed(), Duration::ZERO);
    // Dropping a pending tick does not consume its scheduled occurrence.
    assert!(async_engine::timeout(Duration::from_secs(4), timer.tick())
        .await
        .is_err());
    timer.tick().await;
    assert_eq!(start.elapsed(), Duration::from_secs(10));
    async_engine::sleep(Duration::from_secs(25)).await;
    timer.tick().await;
    timer.tick().await;
    assert_eq!(start.elapsed(), Duration::from_secs(35));
    timer.tick().await;
    assert_eq!(start.elapsed(), Duration::from_secs(40));
}

#[tokio::test]
async fn periodic_timer_rejects_zero_and_unbounded_periods() {
    assert!(PeriodicTimer::new(Duration::ZERO).is_err());
    assert!(PeriodicTimer::new(Duration::MAX).is_err());
    let maximum = Duration::from_secs(365 * 24 * 60 * 60);
    assert!(PeriodicTimer::new(maximum).is_ok());
    assert!(PeriodicTimer::new(maximum + Duration::from_nanos(1)).is_err());
}

#[test]
fn periodic_timer_requires_an_entered_runtime() {
    assert!(PeriodicTimer::new(Duration::from_secs(1)).is_err());
    assert!(PeriodicTimer::new_unbounded(Duration::from_secs(1)).is_err());
}

/// Start a 10 s timer, consume its immediate tick, then stall for 35 s so the
/// 10/20/30 s occurrences are missed. Returns the timer and its start instant.
async fn stalled_timer(behavior: MissedTickBehavior) -> (PeriodicTimer, tokio::time::Instant) {
    let mut timer = PeriodicTimer::new(Duration::from_secs(10)).unwrap();
    timer.set_missed_tick_behavior(behavior);
    assert_eq!(timer.missed_tick_behavior(), behavior);
    let start = tokio::time::Instant::now();
    timer.tick().await;
    assert_eq!(start.elapsed(), Duration::ZERO);
    async_engine::sleep(Duration::from_secs(35)).await;
    (timer, start)
}

#[tokio::test(start_paused = true)]
async fn missed_tick_burst_is_the_default_and_catches_up_on_the_original_schedule() {
    assert_eq!(MissedTickBehavior::default(), MissedTickBehavior::Burst);
    let timer = PeriodicTimer::new(Duration::from_secs(1)).unwrap();
    assert_eq!(timer.missed_tick_behavior(), MissedTickBehavior::Burst);
    let (mut timer, start) = stalled_timer(MissedTickBehavior::Burst).await;
    for _ in 0..3 {
        timer.tick().await;
        assert_eq!(start.elapsed(), Duration::from_secs(35));
    }
    timer.tick().await;
    assert_eq!(start.elapsed(), Duration::from_secs(40));
}

#[tokio::test(start_paused = true)]
async fn missed_tick_delay_restarts_the_cadence_from_the_late_tick() {
    let (mut timer, start) = stalled_timer(MissedTickBehavior::Delay).await;
    timer.tick().await;
    assert_eq!(start.elapsed(), Duration::from_secs(35));
    timer.tick().await;
    assert_eq!(start.elapsed(), Duration::from_secs(45));
}

#[tokio::test(start_paused = true)]
async fn missed_tick_skip_drops_missed_ticks_and_keeps_the_original_phase() {
    let (mut timer, start) = stalled_timer(MissedTickBehavior::Skip).await;
    timer.tick().await;
    assert_eq!(start.elapsed(), Duration::from_secs(35));
    timer.tick().await;
    assert_eq!(start.elapsed(), Duration::from_secs(40));
}

#[tokio::test(start_paused = true)]
async fn unbounded_periodic_timer_accepts_legacy_long_periods() {
    assert!(PeriodicTimer::new_unbounded(Duration::ZERO).is_err());
    assert!(PeriodicTimer::new_unbounded(Duration::MAX).is_err());
    let legacy = Duration::from_secs(366 * 24 * 60 * 60);
    assert!(PeriodicTimer::new(legacy).is_err());
    let mut timer = PeriodicTimer::new_unbounded(legacy).unwrap();
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let start = tokio::time::Instant::now();
    timer.tick().await;
    assert_eq!(start.elapsed(), Duration::ZERO);
    timer.tick().await;
    assert_eq!(start.elapsed(), legacy);
}

#[tokio::test(start_paused = true)]
async fn bounded_acquisition_times_out_without_leaking_capacity() {
    let pool = Semaphore::new(2);
    let held = pool.acquire_within(Duration::from_secs(1)).await.unwrap();
    let start = tokio::time::Instant::now();
    assert!(matches!(
        pool.acquire_many_within(2, Duration::from_secs(3)).await,
        Err(SemaphoreAcquireTimedOut)
    ));
    assert_eq!(start.elapsed(), Duration::from_secs(3));
    assert_eq!(
        pool.available_permits(),
        1,
        "an expired waiter must not keep permits"
    );
    drop(held);
    let both = pool
        .acquire_many_within(2, Duration::from_secs(1))
        .await
        .unwrap();
    assert!(matches!(
        pool.acquire_within(Duration::ZERO).await,
        Err(SemaphoreAcquireTimedOut)
    ));
    drop(both);
    assert_eq!(pool.available_permits(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_released_permit_wakes_a_bounded_waiter_before_its_deadline() {
    let pool = Semaphore::new(1);
    let held = pool.try_acquire().unwrap();
    let waiter = {
        let pool = pool.clone();
        tokio::spawn(async move { pool.acquire_within(Duration::from_secs(60)).await })
    };
    async_engine::sleep(Duration::from_secs(5)).await;
    let start = tokio::time::Instant::now();
    drop(held);
    assert!(waiter.await.unwrap().is_ok());
    assert!(start.elapsed() < Duration::from_secs(1));
}
