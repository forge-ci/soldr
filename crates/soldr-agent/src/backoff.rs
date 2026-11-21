//! Retry schedule is pure; the run loop owns the clock and random source.
use std::time::Duration;
#[derive(Default)]
pub struct Backoff {
    failures: u32,
}
impl Backoff {
    /// 1, 2, 4, 8, 16, 30 seconds, with up to 20% downward jitter.
    pub fn next_delay(&mut self, entropy: u32) -> Duration {
        let base = (1u64 << self.failures.min(5)).min(30) * 1000;
        self.failures = self.failures.saturating_add(1);
        Duration::from_millis(base - u64::from(entropy) % (base / 5 + 1))
    }
    pub fn reset(&mut self) {
        self.failures = 0;
    }
}
