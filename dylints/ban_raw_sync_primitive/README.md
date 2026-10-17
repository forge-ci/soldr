# `ban_raw_sync_primitive`

Rejects raw concurrency primitives that can wait without a bound -- acquiring a
`std::sync::{Mutex, RwLock}`, `Condvar::wait`/`wait_while`, and the
`tokio::sync` / `parking_lot` lock equivalents -- in production source of every
workspace crate, outside the `soldr_platform::sync` boundary module.

`soldr_platform::sync` offers two flavors:

- **Critical section** -- `lock()` / `read()` / `write()` (and the async
  `AsyncMutex`/`AsyncRwLock` equivalents). Every wait and hold is registered
  with its call site; a watchdog aborts with every registered site once any of
  them exceeds `SOLDR_LOCK_HOLD_LIMIT_SECS` (default 15 s).
- **Timeout** -- `lock_for` / `read_for` / `write_for`, returning `LockTimeout`.

Motivation: on 2026-10-16 a guard held across a blocking child wait wedged the
soldr daemon for half an hour, silently (soldr#3836). Test-only code
(`*_tests.rs`, `tests/`, and any test-harness build, which is where inline
`#[cfg(test)]` modules live) is outside the production scope; the lint's own UI
fixtures stay in scope and prove each banned path.
