//! Compile-reply stall dump (soldr#3813). A SESSION compile that has not
//! replied for `SOLDR_COMPILE_STALL_DUMP_SECS` (default 300 s) gets a
//! forensic dump of the soldr daemon/broker and this process's descendants,
//! and the dump path is printed on stderr. The wait itself is unchanged: the
//! dump repeats every interval and never kills anything, so a legitimately
//! long compile still finishes while a wedged one leaves evidence behind.

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

/// Await a compile reply, dumping daemon/broker state on each stall interval.
pub(crate) async fn await_compile_reply<F: Future>(work: F) -> F::Output {
    let interval = stall_dump_interval(std::env::var(STALL_DUMP_ENV).ok().as_deref());
    await_with_stall_hook(work, interval, |elapsed| async move {
        let dumped = tokio::task::spawn_blocking(|| {
            let paths = crate::core::SoldrPaths::new().map_err(std::io::Error::other)?;
            crate::cook_watchdog_dump::write_stall_dump(&paths, "compile-reply")
        })
        .await;
        let where_ = match dumped {
            Ok(Ok(dir)) => format!("stack dump written to {}", dir.display()),
            Ok(Err(error)) => format!("stack dump failed: {error}"),
            Err(error) => format!("stack dump task failed: {error}"),
        };
        eprintln!(
            "\x1b[31msoldr: compile reply has made no progress for {}s; {where_}. Still waiting \
             (set {STALL_DUMP_ENV}=0 to disable these dumps).\x1b[0m",
            elapsed.as_secs()
        );
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

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
