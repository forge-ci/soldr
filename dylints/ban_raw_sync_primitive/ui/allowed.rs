use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

// Atomics, Arc, and bounded Condvar waits are not raw unbounded waits. A
// Mutex reached only through a parameter (no construction or acquisition
// here) is not flagged either.
fn bounded(pair: &(Mutex<bool>, Condvar), guard: std::sync::MutexGuard<'_, bool>) {
    let (_, condvar) = pair;
    let _ = condvar.wait_timeout(guard, Duration::from_millis(1));
}

fn main() {
    let counter = Arc::new(AtomicU64::new(0));
    counter.fetch_add(1, Ordering::Relaxed);
    let _ = bounded;
}
