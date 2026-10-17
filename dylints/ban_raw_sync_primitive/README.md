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

## Blocking waits (timeout-only facade)

The same rule covers every other raw blocking wait. Outside
`soldr_platform::wait` these are rejected, and each replacement takes a
mandatory `Duration`:

| Raw call | Replacement |
|---|---|
| `mpsc::Receiver::recv` / `SyncSender::send` | `wait::recv_for` / `wait::send_for` |
| `JoinHandle::join` | `wait::join_for` |
| `Child::wait` | `wait::wait_for` |
| `Command::output` / `Command::status` | `wait::output_for` / `wait::status_for` (or soldr-core's `command_output_with_timeout*`) |
| `File::lock*`, `fs2::FileExt::lock_exclusive` / `lock_shared` | `wait::lock_exclusive_for` / `lock_shared_for` / `lock_exclusive_announced` |
| `Barrier::wait` | `wait::Barrier::wait_for` |
| `Runtime::block_on` / `Handle::block_on` / `futures::executor::block_on` | `wait::block_on_for` |
| tokio `blocking_recv` / `blocking_send` | `wait::recv_for` / `send_for` on a std channel |

Legitimately long waits pass a named budget (`LONG_CHILD`, `LONG_LOCK`,
`WHOLE_COMMAND`, ...) so each is justified at one definition. The only allows
are process- or thread-lifetime waits (the CLI thread, the broker and daemon
runtimes, the handoff control plane) and the Windows IPC bridge already
bounded by `run_windows_ipc`, each carrying its reason.
