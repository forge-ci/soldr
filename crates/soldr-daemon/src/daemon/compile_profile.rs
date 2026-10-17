//! Per-compile daemon timing under `SOLDR_PROFILE_STARTUP` (soldr#3800 PR 1).
//!
//! The SESSION serve path opens a [`scope`] around one compile when the
//! request's forwarded environment turns profiling on (the shim sends its whole
//! environment, so an already-running daemon needs no env of its own). The
//! embedded zccache service marks the instants just before and after its
//! `inner.compile(...)` call through [`mark_compile_start`] /
//! [`mark_compile_end`]; with no scope open, both are no-ops.
//!
//! zccache takes its compile permit inside `compile`, so the permit wait is
//! part of `compile_end - compile_start`; soldr cannot separate it.

use crate::platform::sync::Mutex;
use std::future::Future;
use std::sync::Arc;

use soldr_core::startup_profile::{
    emit_line, format_daemon_compile_line, unix_ms_now, DaemonCompileTimings,
};

tokio::task_local! {
    static CURRENT: Arc<Mutex<DaemonCompileTimings>>;
}

/// Run `fut` with a timing slot open and print the `daemon-compile` line once
/// it finishes (`reply_sent` is the instant the reply has been written).
pub(crate) async fn scope<F: Future>(crate_name: String, recv_unix_ms: u128, fut: F) -> F::Output {
    let slot = Arc::new(Mutex::new(DaemonCompileTimings {
        crate_name,
        recv_unix_ms,
        ..DaemonCompileTimings::default()
    }));
    let out = CURRENT.scope(Arc::clone(&slot), fut).await;
    let mut timings = slot.lock().clone();
    timings.reply_sent_unix_ms = unix_ms_now();
    emit_line(&format_daemon_compile_line(&timings));
    out
}

fn mark(set: impl FnOnce(&mut DaemonCompileTimings)) {
    let _ = CURRENT.try_with(|slot| set(&mut slot.lock()));
}

pub(crate) fn mark_compile_start() {
    mark(|t| t.compile_start_unix_ms = unix_ms_now());
}

pub(crate) fn mark_compile_end() {
    mark(|t| t.compile_end_unix_ms = unix_ms_now());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_are_no_ops_without_a_scope() {
        mark_compile_start();
        mark_compile_end();
    }

    #[tokio::test]
    async fn scope_collects_marks_from_inside() {
        let seen = scope("syn".to_string(), 7, async {
            mark_compile_start();
            mark_compile_end();
            CURRENT.with(|s| s.lock().clone())
        })
        .await;
        assert_eq!(seen.crate_name, "syn");
        assert_eq!(seen.recv_unix_ms, 7);
        assert!(seen.compile_start_unix_ms > 0);
        assert!(seen.compile_end_unix_ms >= seen.compile_start_unix_ms);
    }
}
