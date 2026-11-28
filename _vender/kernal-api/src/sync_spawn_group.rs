use std::io;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_KILL_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const KILL_DRAIN_TIMEOUT_ENV: &str = "KERNAL_API_KILL_DRAIN_TIMEOUT_MS";

fn kill_drain_deadline() -> Instant {
    let timeout = std::env::var(KILL_DRAIN_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_KILL_DRAIN_TIMEOUT);
    Instant::now() + timeout
}

fn poll_until<T>(
    deadline: Instant,
    interval: Duration,
    mut poll: impl FnMut() -> io::Result<Option<T>>,
) -> io::Result<Option<T>> {
    loop {
        if let Some(value) = poll()? {
            return Ok(Some(value));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        thread::sleep(interval.min(deadline.saturating_duration_since(now)));
    }
}

trait UnixChild: Send {
    fn kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> io::Result<i32>;
    fn try_wait(&mut self) -> io::Result<Option<i32>>;
}

impl UnixChild for std::process::Child {
    fn kill(&mut self) -> io::Result<()> {
        std::process::Child::kill(self)
    }

    fn wait(&mut self) -> io::Result<i32> {
        std::process::Child::wait(self).map(crate::platform::process::exit_code)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(std::process::Child::try_wait(self)?.map(crate::platform::process::exit_code))
    }
}

impl crate::platform::process::DaemonChildControl for std::process::Child {
    fn kill(&mut self) -> io::Result<()> {
        std::process::Child::kill(self)
    }

    fn wait(&mut self) -> io::Result<i32> {
        std::process::Child::wait(self).map(crate::platform::process::exit_code)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(std::process::Child::try_wait(self)?.map(crate::platform::process::exit_code))
    }
}

pub struct SpawnedInner {
    child: Arc<Mutex<Option<Box<dyn UnixChild>>>>,
    pgid: i32,
}

impl SpawnedInner {
    pub fn kill(&self) -> io::Result<()> {
        // Try the child first, then the process group, to make sure
        // any siblings spawned inside go down too.
        let mut guard = self.child.lock().expect("child mutex poisoned");
        if let Some(child) = guard.as_mut() {
            let _ = child.kill();
        }
        drop(guard);
        let _ = crate::platform::process::unix_signal_process_group(
            self.pgid,
            crate::platform::process::UnixSignalKind::Kill,
        );
        Ok(())
    }

    pub fn wait(&self) -> io::Result<i32> {
        // Block for exit without the lock, so a concurrent `kill` is never
        // queued behind this wait (#433); then reap under it.
        wait_for_exit_unreaped(self.pgid);
        let mut guard = self.child.lock().expect("child mutex poisoned");
        let Some(child) = guard.as_mut() else {
            return Err(io::Error::other("child handle absent"));
        };
        child.wait()
    }

    pub fn try_wait(&self) -> io::Result<Option<i32>> {
        let mut guard = self.child.lock().expect("child mutex poisoned");
        let Some(child) = guard.as_mut() else {
            return Ok(None);
        };
        child.try_wait()
    }

    pub fn shutdown(&mut self) {
        self.shutdown_with_deadline(kill_drain_deadline());
    }

    fn shutdown_with_deadline(&mut self, deadline: Instant) {
        let group_signaled = crate::platform::process::unix_signal_process_group(
            self.pgid,
            crate::platform::process::UnixSignalKind::Kill,
        )
        .is_ok();
        let Some(mut child) = self.child.lock().expect("child mutex poisoned").take() else {
            return;
        };
        if !group_signaled {
            let _ = child.kill();
        }
        match poll_until(deadline, Duration::from_millis(10), || child.try_wait()) {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => spawn_background_reaper(child),
        }
    }
}

impl crate::platform::process::SpawnedChildControl for SpawnedInner {
    fn kill(&mut self) -> io::Result<()> {
        SpawnedInner::kill(self)
    }

    fn wait(&mut self) -> io::Result<i32> {
        SpawnedInner::wait(self)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        SpawnedInner::try_wait(self)
    }

    fn shutdown(&mut self) {
        SpawnedInner::shutdown(self);
    }
}

/// Block until the child leading `pid`'s group has exited, without reaping it.
///
/// `WNOWAIT` leaves the zombie for the owner's `wait`, and an unreaped child's
/// PID cannot be reissued, so a concurrent `kill` stays safe throughout. Any
/// error (`ECHILD` once something else reaped it) returns at once; the caller's
/// locked `wait` then reports the real outcome.
fn wait_for_exit_unreaped(pid: i32) {
    loop {
        // SAFETY: zeroed POD out-parameter, written by the kernel only.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: scalar id and flags; `info` outlives the call.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return;
        }
    }
}

fn spawn_background_reaper(mut child: Box<dyn UnixChild>) {
    thread::spawn(move || {
        // Once ownership is off the caller's teardown path, a blocking wait is
        // the most reliable terminal policy: it reaps exactly once without a
        // retry loop, spinning, or retaining the shared child mutex.
        let _ = child.wait();
    });
}

fn slot_to_stdio(slot: &crate::platform::process::StdioSource<'_>) -> io::Result<Stdio> {
    match slot {
        crate::platform::process::StdioSource::Null => Ok(Stdio::null()),
        crate::platform::process::StdioSource::Parent => Ok(Stdio::inherit()),
        crate::platform::process::StdioSource::File(file) => Ok(Stdio::from(file.try_clone()?)),
        crate::platform::process::StdioSource::Pipe => Ok(Stdio::piped()),
    }
}

fn daemon_slot_to_stdio(
    slot: &crate::platform::process::DaemonStdioSource<'_>,
) -> io::Result<Stdio> {
    match slot {
        crate::platform::process::DaemonStdioSource::Null => Ok(Stdio::null()),
        crate::platform::process::DaemonStdioSource::File(file) => {
            Ok(Stdio::from(file.try_clone()?))
        }
    }
}

pub fn spawn_sync_daemon(
    command: &mut Command,
    stdio: crate::platform::process::DaemonStdio<'_>,
    environment: crate::platform::process::SyncEnvironment,
    _breakaway: bool,
) -> io::Result<crate::platform::process::DaemonChild> {
    apply_environment(command, environment)?;
    command
        .stdin(Stdio::null())
        .stdout(daemon_slot_to_stdio(&stdio.stdout)?)
        .stderr(daemon_slot_to_stdio(&stdio.stderr)?);

    crate::platform::process::configure_sync_daemon_command(command)?;

    let child = crate::ape::spawn_std(command, |command| command.spawn())?;
    let pid = child.id();
    Ok(crate::platform::process::DaemonChild {
        pid,
        inner: Box::new(child),
    })
}

pub fn spawn_sync(
    command: &mut Command,
    stdio: crate::platform::process::SpawnStdio<'_>,
    environment: crate::platform::process::SyncEnvironment,
) -> io::Result<crate::platform::process::SpawnedChild> {
    apply_environment(command, environment)?;
    command.stdin(slot_to_stdio(&stdio.stdin)?);
    command.stdout(slot_to_stdio(&stdio.stdout)?);
    command.stderr(slot_to_stdio(&stdio.stderr)?);

    crate::platform::process::configure_sync_contained_command(command)?;

    let mut child = crate::ape::spawn_std(command, |command| command.spawn())?;
    let pid = child.id();
    let pgid = pid as i32;

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let child: Arc<Mutex<Option<Box<dyn UnixChild>>>> = Arc::new(Mutex::new(Some(Box::new(child))));

    // Drain watcher: wait for exit, then sleep `drain_timeout`. We
    // don't proactively close anything on Unix — Rust's ChildStdin/etc.
    // own their fds; once the child exits and the kernel ref-counts
    // its copies to zero, parent reads will EOF naturally.
    if let Some(timeout) = stdio.drain_timeout {
        let child_clone = Arc::clone(&child);
        thread::spawn(move || {
            // Borrow child for try_wait.  We do a polling loop so
            // shutdown() taking the inner Child during Drop doesn't
            // wedge us.
            loop {
                {
                    let mut guard = child_clone.lock().expect("child mutex poisoned");
                    match guard.as_mut() {
                        Some(c) => match c.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) => {}
                            Err(_) => break,
                        },
                        None => return,
                    }
                }
                // #199: intentional — try_wait poll on the contained
                // child, 50ms cadence inside a bounded outer drain
                // loop. waitpid(WNOHANG)-equivalent semantics.
                thread::sleep(std::time::Duration::from_millis(50));
            }
            // #199: intentional — post-mortem pipe drain. Children's
            // write-ends of the captured stdio pipes are still being
            // closed by the kernel after exit; this gives readers a
            // chance to see the final bytes before the watcher
            // releases its keep-alive.
            thread::sleep(timeout);
        });
    }

    Ok(crate::platform::process::SpawnedChild {
        stdin,
        stdout,
        stderr,
        pid,
        inner: Box::new(SpawnedInner { child, pgid }),
    })
}

fn apply_environment(
    command: &mut Command,
    environment: crate::platform::process::SyncEnvironment,
) -> io::Result<()> {
    let Some(base) = environment.into_base()? else {
        return Ok(());
    };

    // `env_clear` also clears Command's mutation map. Preserve additions,
    // overrides, and removals so they are replayed after the selected base.
    let explicit: Vec<_> = command
        .get_envs()
        .map(|(key, value)| (key.to_os_string(), value.map(std::ffi::OsStr::to_os_string)))
        .collect();
    command.env_clear();
    command.envs(base);
    for (key, value) in explicit {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Condvar};

    struct FakeChild {
        wait_gate: Arc<(Mutex<bool>, Condvar)>,
        waits: Arc<AtomicUsize>,
        kills: Arc<AtomicUsize>,
    }

    impl UnixChild for FakeChild {
        fn kill(&mut self) -> io::Result<()> {
            self.kills.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn wait(&mut self) -> io::Result<i32> {
            self.waits.fetch_add(1, Ordering::SeqCst);
            let (lock, condvar) = &*self.wait_gate;
            let mut released = lock.lock().expect("wait gate mutex poisoned");
            while !*released {
                released = condvar.wait(released).expect("wait gate mutex poisoned");
            }
            Ok(0)
        }

        fn try_wait(&mut self) -> io::Result<Option<i32>> {
            self.waits.fetch_add(1, Ordering::SeqCst);
            let released = *self.wait_gate.0.lock().expect("wait gate mutex poisoned");
            Ok(released.then_some(0))
        }
    }

    struct BlockedFixture {
        inner: SpawnedInner,
        child: Arc<Mutex<Option<Box<dyn UnixChild>>>>,
        wait_gate: Arc<(Mutex<bool>, Condvar)>,
        waits: Arc<AtomicUsize>,
        kills: Arc<AtomicUsize>,
    }

    fn blocked_inner() -> BlockedFixture {
        let wait_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let waits = Arc::new(AtomicUsize::new(0));
        let kills = Arc::new(AtomicUsize::new(0));
        let child: Arc<Mutex<Option<Box<dyn UnixChild>>>> =
            Arc::new(Mutex::new(Some(Box::new(FakeChild {
                wait_gate: Arc::clone(&wait_gate),
                waits: Arc::clone(&waits),
                kills: Arc::clone(&kills),
            }))));
        BlockedFixture {
            inner: SpawnedInner {
                child: Arc::clone(&child),
                pgid: i32::MAX,
            },
            child,
            wait_gate,
            waits,
            kills,
        }
    }

    fn release_wait(wait_gate: &Arc<(Mutex<bool>, Condvar)>) {
        let (lock, condvar) = &**wait_gate;
        *lock.lock().expect("wait gate mutex poisoned") = true;
        condvar.notify_all();
    }

    struct ShutdownOnDrop {
        inner: Option<SpawnedInner>,
        deadline: Instant,
    }

    impl Drop for ShutdownOnDrop {
        fn drop(&mut self) {
            self.inner
                .as_mut()
                .expect("test wrapper missing inner")
                .shutdown_with_deadline(self.deadline);
        }
    }

    #[test]
    fn drop_is_bounded_when_child_wait_does_not_complete() {
        // Regression for #619: SpawnedChild::drop delegates directly to
        // SpawnedInner::shutdown, modeled by this wrapper around the fake child.
        let BlockedFixture {
            inner, wait_gate, ..
        } = blocked_inner();
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        let worker = thread::spawn(move || {
            drop(ShutdownOnDrop {
                inner: Some(inner),
                deadline: Instant::now() + Duration::from_millis(50),
            });
            let _ = tx.send(started.elapsed());
        });

        // The property is causal, not a stopwatch reading: Drop must return
        // WITHOUT waiting for the child, so it must report back before the
        // gate is released. A blocked Drop cannot, whatever the machine load.
        //
        // Asserting a wall-clock bound instead conflated that with "finished
        // inside 100ms", which a loaded runner broke by 0.2ms. The window
        // below is generous because it only bounds how long a *failure* takes
        // to detect: a correct Drop returns at its 50ms deadline and never
        // approaches it.
        let timely = rx.recv_timeout(Duration::from_secs(5));
        release_wait(&wait_gate);
        let returned_before_release = timely.is_ok();
        let elapsed = timely
            .or_else(|_| rx.recv_timeout(Duration::from_secs(5)))
            .expect("shutdown did not unblock even after releasing fake child");
        worker.join().expect("shutdown worker panicked");
        assert!(
            returned_before_release,
            "Drop blocked in child.wait() until the fake child was released              (took {elapsed:?}); its deadline should have bounded it"
        );
    }

    #[test]
    fn shutdown_does_not_hold_child_mutex_while_reaping() {
        let BlockedFixture {
            mut inner,
            child,
            wait_gate,
            waits,
            ..
        } = blocked_inner();
        let worker = thread::spawn(move || {
            inner.shutdown_with_deadline(Instant::now() + Duration::from_millis(50));
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while waits.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        // Shutdown polls until its deadline, so the count may exceed one on a
        // slow host (#347); the property is only that reaping has begun.
        assert!(waits.load(Ordering::SeqCst) >= 1, "fake wait never started");

        let child_mutex_available = child.try_lock().is_ok();
        release_wait(&wait_gate);
        worker.join().expect("shutdown worker panicked");
        assert!(
            child_mutex_available,
            "shutdown held the child mutex across reaping"
        );
    }

    struct ReadyChild {
        polls: Arc<AtomicUsize>,
        waits: Arc<AtomicUsize>,
    }

    impl UnixChild for ReadyChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn wait(&mut self) -> io::Result<i32> {
            self.waits.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }

        fn try_wait(&mut self) -> io::Result<Option<i32>> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(0))
        }
    }

    #[test]
    fn shutdown_reaps_ready_child_exactly_once() {
        let polls = Arc::new(AtomicUsize::new(0));
        let waits = Arc::new(AtomicUsize::new(0));
        let child: Arc<Mutex<Option<Box<dyn UnixChild>>>> =
            Arc::new(Mutex::new(Some(Box::new(ReadyChild {
                polls: Arc::clone(&polls),
                waits: Arc::clone(&waits),
            }))));
        let mut inner = SpawnedInner {
            child,
            pgid: i32::MAX,
        };

        inner.shutdown_with_deadline(Instant::now() + Duration::from_secs(1));

        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert_eq!(waits.load(Ordering::SeqCst), 0);
    }

    /// A blocked `wait` must not hold the child lock, or a concurrent `kill`
    /// queues behind it and a child that only exits when killed never does
    /// (#433).
    #[test]
    fn kill_is_not_blocked_by_a_concurrent_wait() {
        use std::os::unix::process::CommandExt as _;
        let child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn sleep");
        let pgid = child.id() as i32;
        let child: Arc<Mutex<Option<Box<dyn UnixChild>>>> = Arc::new(Mutex::new(Some(Box::new(child))));
        let inner = Arc::new(SpawnedInner { child, pgid });

        let waiter = {
            let inner = Arc::clone(&inner);
            thread::spawn(move || inner.wait())
        };
        thread::sleep(Duration::from_millis(100));
        let (killed, kill_returned) = std::sync::mpsc::channel();
        {
            let inner = Arc::clone(&inner);
            thread::spawn(move || {
                let _ = killed.send(inner.kill());
            });
        }
        let kill = kill_returned.recv_timeout(Duration::from_secs(2));
        if kill.is_err() {
            // Unblock the waiter so a failing run does not leak the child.
            let _ = crate::platform::process::unix_signal_process_group(
                pgid,
                crate::platform::process::UnixSignalKind::Kill,
            );
        }
        let status = waiter.join().expect("waiter thread");
        assert!(kill.is_ok(), "kill queued behind a blocked wait");
        assert!(status.is_ok(), "wait reports the killed child: {status:?}");
    }

    #[test]
    fn shutdown_falls_back_to_direct_kill_when_group_signal_fails() {
        let BlockedFixture {
            mut inner,
            wait_gate,
            kills,
            ..
        } = blocked_inner();
        release_wait(&wait_gate);

        inner.shutdown_with_deadline(Instant::now() + Duration::from_secs(1));

        assert_eq!(kills.load(Ordering::SeqCst), 1);
    }
}
