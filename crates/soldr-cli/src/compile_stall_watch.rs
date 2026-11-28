//! Admission-aware compile supervision (soldr#4186).
//! Queueing is healthy while the selected daemon admits or completes work.
//! A daemon-wide no-progress interval produces bounded evidence and fails
//! loudly. Host admission alone does not prove compiler execution: zccache's
//! final shared/exclusive resource gate lies behind that boundary.

use std::future::Future;
use std::time::Duration;

pub(crate) const STALL_DUMP_ENV: &str = "SOLDR_COMPILE_STALL_DUMP_SECS";
const DEFAULT_STALL_DUMP: Duration = Duration::from_secs(300);

/// Dump interval from the env value; `None` disables (`0`). An unparsable
/// value falls back to the default rather than silently disabling.
pub(crate) fn stall_dump_interval(raw: Option<&str>) -> Option<Duration> {
    match raw.map(str::trim).filter(|v| !v.is_empty()) {
        None => Some(DEFAULT_STALL_DUMP),
        Some(value) => match value.parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(Duration::from_secs(secs)),
            Err(_) => Some(DEFAULT_STALL_DUMP),
        },
    }
}

/// Await `work`, calling `on_stall(elapsed)` each time `interval` passes
/// without it completing.
#[cfg(test)]
pub(crate) async fn await_with_stall_hook<F, H, Fut>(
    work: F,
    interval: Option<Duration>,
    mut on_stall: H,
) -> F::Output
where
    F: Future,
    H: FnMut(Duration) -> Fut,
    Fut: Future<Output = ()>,
{
    let Some(interval) = interval else {
        return work.await;
    };
    tokio::pin!(work);
    let mut elapsed = Duration::ZERO;
    loop {
        tokio::select! {
            output = &mut work => return output,
            () = tokio::time::sleep(interval) => {
                elapsed += interval;
                on_stall(elapsed).await;
            }
        }
    }
}

/// Await a compile reply with admission-aware daemon liveness checks.
pub(crate) async fn await_compile_reply<F: Future>(work: F) -> std::io::Result<F::Output> {
    let Some(threshold) = stall_dump_interval(std::env::var(STALL_DUMP_ENV).ok().as_deref()) else {
        return Ok(work.await);
    };
    tokio::pin!(work);
    let mut classifier = StallClassifier::default();
    let token = std::process::id().to_string();
    let mut informed = false;
    let mut disconnected_since = None;
    loop {
        tokio::select! {
            biased;
            output = &mut work => return Ok(output),
            () = tokio::time::sleep(Duration::from_secs(5).min(threshold)) => {}
        }
        let probe_started = std::time::Instant::now();
        let since = disconnected_since.unwrap_or(probe_started);
        let budget =
            crate::session_transport::compile_liveness_deadline().saturating_sub(since.elapsed());
        let probe = tokio::task::spawn_blocking(daemon_progress);
        let status = tokio::select! {
            biased;
            output = &mut work => return Ok(output),
            result = tokio::time::timeout(budget, probe) => result
                .map_err(std::io::Error::other)
                .and_then(|r| r.map_err(std::io::Error::other))
                .and_then(|r| r),
        };
        let status = match status {
            Ok(status) => {
                disconnected_since = None;
                status
            }
            Err(error) => {
                let since = disconnected_since.get_or_insert(probe_started);
                if since.elapsed() < crate::session_transport::compile_liveness_deadline() {
                    continue;
                }
                let message = format!("soldr: daemon progress connection is unavailable: {error}; run `soldr daemon status`, `soldr logs paths`, then `soldr daemon stop` and `soldr daemon start`");
                eprintln!("\x1b[31m{message}\x1b[0m");
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, message));
            }
        };
        let elapsed = classifier.observe(&status, &token, threshold);
        if let Some(wait) = status
            .admission
            .as_ref()
            .and_then(|a| a.waits.iter().find(|w| w.token == token && w.phase != 2))
        {
            if !informed && wait.waited_secs >= 60 {
                let a = status.admission.as_ref().expect("admission exists");
                eprintln!("\x1b[2msoldr: queued for a compile slot (#{} of {}, {} running, waited {}s)\x1b[0m",
                    wait.position, a.queued, a.running, wait.waited_secs);
                informed = true;
            }
        }
        if let Some(reason) = elapsed {
            let dump = tokio::task::spawn_blocking(|| {
                let paths = crate::core::SoldrPaths::new().map_err(std::io::Error::other)?;
                crate::cook_watchdog_dump::write_stall_dump(&paths, "compile-reply")
            })
            .await;
            let message = format!("soldr: {reason}; diagnostic dump: {dump:?}. Run `soldr daemon status`, `soldr logs paths`, then `soldr daemon stop` and `soldr daemon start` for recovery");
            eprintln!("\x1b[31m{message}\x1b[0m");
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, message));
        }
    }
}

pub(crate) fn daemon_progress() -> std::io::Result<crate::daemon::protocol::StatusInfo> {
    let paths = crate::core::SoldrPaths::new().map_err(std::io::Error::other)?;
    crate::daemon::client::status(&crate::daemon::client::default_sock_path(&paths))
        .map_err(|e| std::io::Error::other(format!("{e:?}")))
}

#[derive(Default)]
struct StallClassifier {
    counters: Option<(u64, u64, u64)>,
    unchanged_since: Option<std::time::Instant>,
}
impl StallClassifier {
    fn observe(
        &mut self,
        status: &crate::daemon::protocol::StatusInfo,
        _token: &str,
        threshold: Duration,
    ) -> Option<&'static str> {
        let now = std::time::Instant::now();
        let Some(a) = &status.admission else {
            return Some("daemon does not report admission progress; update daemon");
        };
        let counters = (status.generation, a.admissions, a.completions);
        if self.counters != Some(counters) {
            self.counters = Some(counters);
            self.unchanged_since = Some(now);
        }
        // A host permit is acquired BEFORE zccache's exclusive resource lock.
        // Without a final-admission/output callback it is unsafe to interpret
        // phase 2 elapsed time as compiler silence. Keep daemon-wide liveness.
        if self
            .unchanged_since
            .is_some_and(|t| now.duration_since(t) >= threshold)
        {
            return Some(
                "daemon has made no admissions or completions within the no-progress threshold",
            );
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn status() -> crate::daemon::protocol::StatusInfo {
        crate::daemon::protocol::StatusInfo {
            version: crate::daemon::protocol::PROTOCOL_VERSION,
            pid: 1,
            generation: 1,
            uptime_secs: 0,
            request_count: 0,
            cook_stats: None,
            compile_backend: "embedded".into(),
            compile_jobs: 1,
            compile_jobs_source: "test".into(),
            in_flight_compiles: Some(3),
            maintenance_active: Some(false),
            admission: Some(crate::core::wire::proto::WireAdmissionProgress {
                queued: 2,
                running: 1,
                waits: vec![crate::core::wire::proto::WireCompileWait {
                    token: "client".into(),
                    phase: 1,
                    position: 2,
                    waited_secs: 900,
                }],
                ..Default::default()
            }),
        }
    }
    #[test]
    fn queued_moving_is_healthy_but_daemon_without_progress_stalls() {
        let mut classifier = StallClassifier::default();
        let mut status = status();
        let threshold = Duration::from_secs(300);
        assert_eq!(classifier.observe(&status, "client", threshold), None);
        classifier.unchanged_since = Some(std::time::Instant::now() - threshold);
        status.admission.as_mut().unwrap().completions += 1;
        assert_eq!(classifier.observe(&status, "client", threshold), None);
        classifier.unchanged_since = Some(std::time::Instant::now() - threshold);
        assert!(classifier
            .observe(&status, "client", threshold)
            .unwrap()
            .contains("no admissions or completions"));
    }
    #[test]
    fn host_admitted_wait_does_not_stall_while_other_work_completes() {
        let mut classifier = StallClassifier::default();
        let mut status = status();
        status.admission.as_mut().unwrap().waits[0].phase = 2;
        assert_eq!(
            classifier.observe(&status, "client", Duration::from_secs(300)),
            None
        );
    }

    #[test]
    fn interval_parses_default_disable_and_value() {
        assert_eq!(stall_dump_interval(None), Some(DEFAULT_STALL_DUMP));
        assert_eq!(stall_dump_interval(Some(" ")), Some(DEFAULT_STALL_DUMP));
        assert_eq!(stall_dump_interval(Some("0")), None);
        assert_eq!(
            stall_dump_interval(Some("12")),
            Some(Duration::from_secs(12))
        );
        assert_eq!(stall_dump_interval(Some("soon")), Some(DEFAULT_STALL_DUMP));
    }

    #[test]
    fn hook_fires_per_interval_and_work_still_completes() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        let fired = AtomicU32::new(0);
        let output = rt.block_on(await_with_stall_hook(
            async {
                tokio::time::sleep(Duration::from_secs(35)).await;
                7
            },
            Some(Duration::from_secs(10)),
            |_| {
                fired.fetch_add(1, Ordering::SeqCst);
                async {}
            },
        ));
        assert_eq!(output, 7);
        assert_eq!(fired.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn disabled_interval_never_fires() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        let fired = AtomicU32::new(0);
        rt.block_on(await_with_stall_hook(
            async { tokio::time::sleep(Duration::from_secs(900)).await },
            None,
            |_| {
                fired.fetch_add(1, Ordering::SeqCst);
                async {}
            },
        ));
        assert_eq!(fired.load(Ordering::SeqCst), 0);
    }
}
