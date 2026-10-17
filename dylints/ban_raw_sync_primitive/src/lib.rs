#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_span;

use rustc_errors::DiagDecorator;
use rustc_hir::def::Res;
use rustc_hir::{Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_span::{symbol::Symbol, FileName, RemapPathScopeComponents};

dylint_linting::declare_late_lint! {
    /// ### What it does
    ///
    /// Rejects raw concurrency primitives that can wait without a bound --
    /// `std::sync::{Mutex, RwLock}` acquisition, `Condvar::wait`, and the
    /// `tokio::sync` / `parking_lot` lock equivalents -- outside Soldr's
    /// `soldr_platform::sync` boundary module.
    ///
    /// ### Why is this bad?
    ///
    /// A raw wait that never ends is invisible. On 2026-10-16 a lock guard
    /// held across a blocking child wait wedged the soldr daemon for half an
    /// hour while every build on the host stalled, and nothing named the
    /// culprit (soldr#3836). `soldr_platform::sync` offers two flavors: a
    /// critical section (`lock()` / `read()` / `write()`) whose every wait and
    /// hold is registered and aborts with the call sites once it exceeds the
    /// hold limit, and a timeout flavor (`lock_for` / `read_for` /
    /// `write_for`) that returns `LockTimeout`.
    pub BAN_RAW_SYNC_PRIMITIVE,
    Deny,
    "use soldr_platform::sync bounded-wait locks instead of raw concurrency primitives"
}

/// `(crate, type, method)` triples. The crate is the first def-path segment;
/// the type and method are the last two, so private module paths inside the
/// crate (`std::sync::poison::mutex::Mutex::lock`) still match.
const BANNED: &[(&str, &str, &str)] = &[
    ("std", "Mutex", "new"),
    ("std", "Mutex", "lock"),
    ("std", "Mutex", "try_lock"),
    ("std", "RwLock", "new"),
    ("std", "RwLock", "read"),
    ("std", "RwLock", "write"),
    ("std", "RwLock", "try_read"),
    ("std", "RwLock", "try_write"),
    ("std", "Condvar", "wait"),
    ("std", "Condvar", "wait_while"),
    ("tokio", "Mutex", "lock"),
    ("tokio", "Mutex", "blocking_lock"),
    ("tokio", "RwLock", "read"),
    ("tokio", "RwLock", "write"),
    ("tokio", "RwLock", "blocking_read"),
    ("tokio", "RwLock", "blocking_write"),
    ("lock_api", "Mutex", "lock"),
    ("lock_api", "RwLock", "read"),
    ("lock_api", "RwLock", "write"),
];

/// Raw blocking waits with no bound, each with its timeout-only replacement in
/// `soldr_platform::wait` (or soldr-core's bounded command runners).
/// `(crate, type-or-module, method, replacement)`.
const BANNED_WAITS: &[(&str, &str, &str, &str)] = &[
    ("std", "Receiver", "recv", "wait::recv_for"),
    ("std", "SyncSender", "send", "wait::send_for"),
    ("std", "JoinHandle", "join", "wait::join_for"),
    ("std", "Child", "wait", "wait::wait_for"),
    ("std", "Child", "wait_with_output", "core::command_output_with_timeout_duration"),
    ("std", "Command", "output", "wait::output_for (or core::command_output_with_timeout*)"),
    ("std", "Command", "status", "wait::status_for"),
    ("std", "File", "lock", "wait::lock_exclusive_for"),
    ("std", "File", "lock_shared", "wait::lock_shared_for"),
    ("fs2", "FileExt", "lock_exclusive", "wait::lock_exclusive_for"),
    ("fs2", "FileExt", "lock_shared", "wait::lock_shared_for"),
    ("std", "Barrier", "wait", "wait::Barrier::wait_for"),
    ("tokio", "Runtime", "block_on", "wait::block_on_for"),
    ("tokio", "Handle", "block_on", "wait::block_on_for"),
    ("futures_executor", "local_pool", "block_on", "wait::block_on_for"),
    ("tokio", "Receiver", "blocking_recv", "wait::recv_for on a std channel"),
    ("tokio", "Sender", "blocking_send", "wait::send_for on a std channel"),
];

impl<'tcx> LateLintPass<'tcx> for BanRawSyncPrimitive {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        if !span_is_in_scope(cx, expr.span) {
            return;
        }
        let def_id = match expr.kind {
            ExprKind::MethodCall(..) => cx.typeck_results().type_dependent_def_id(expr.hir_id),
            // UFCS calls and function items resolve through a path; matching
            // only method-call syntax would be bypassable mechanically.
            ExprKind::Path(qpath) => match cx.qpath_res(&qpath, expr.hir_id) {
                Res::Def(_, def_id) => Some(def_id),
                _ => None,
            },
            _ => None,
        };
        let Some(def_id) = def_id else {
            return;
        };
        let path = cx.get_def_path(def_id);
        if let Some((krate, ty, method)) = BANNED.iter().find(|(krate, ty, method)| {
            path.first() == Some(&Symbol::intern(krate)) && path_ends_with(&path, &[ty, method])
        }) {
            let shown = match *krate {
                "std" => "std::sync",
                "tokio" => "tokio::sync",
                _ => "parking_lot",
            };
            let message = if *method == "new" {
                format!(
                    "raw `{shown}::{ty}` constructed outside the bounded-wait boundary; use \
                     `soldr_platform::sync::{ty}` (soldr#3836)"
                )
            } else if method.starts_with("try_") {
                format!(
                    "raw `{shown}::{ty}::{method}` bypasses the bounded-wait boundary; use \
                     `soldr_platform::sync` (`try_lock`, or `lock_for`/`read_for`/`write_for` \
                     for a timeout) (soldr#3836)"
                )
            } else {
                format!(
                    "raw `{shown}::{ty}::{method}` can wait without a bound; use \
                     `soldr_platform::sync` -- `lock()`/`read()`/`write()` for a critical section \
                     (watchdog-bounded) or `lock_for`/`read_for`/`write_for` for a timeout \
                     (soldr#3836)"
                )
            };
            cx.opt_span_lint(
                BAN_RAW_SYNC_PRIMITIVE,
                Some(expr.span),
                DiagDecorator(move |diag| {
                    diag.primary_message(message);
                }),
            );
            return;
        }
        if let Some((krate, ty, method, replacement)) =
            BANNED_WAITS.iter().find(|(krate, ty, method, _)| {
                path.first() == Some(&Symbol::intern(krate)) && path_ends_with(&path, &[ty, method])
            })
        {
            let message = format!(
                "raw `{krate}::..::{ty}::{method}` blocks without a bound; use `{replacement}`, \
                 which takes a mandatory timeout (soldr#3836)"
            );
            cx.opt_span_lint(
                BAN_RAW_SYNC_PRIMITIVE,
                Some(expr.span),
                DiagDecorator(move |diag| {
                    diag.primary_message(message);
                }),
            );
        }
    }
}

/// Production sources of every workspace crate, minus the boundary module
/// itself and test-only files. The lint's own UI fixtures stay in scope.
///
/// A test crate (`--test` harness build) is out of scope, as in
/// `ban_swallowed_child_stdio`: inline `#[cfg(test)]` modules exist only
/// there, and every production item it contains is also compiled -- and
/// linted -- by the same crate's non-test build. Test barriers such as
/// `TEST_PROCESS_ENV_LOCK` are held for a whole test body, which a
/// critical-section watchdog must not bound.
fn span_is_in_scope(cx: &LateContext<'_>, span: rustc_span::Span) -> bool {
    let filename = source_filename(cx, span).replace('\\', "/");
    let is_ui_fixture = filename.starts_with("ui/") || filename.contains("/ui/");
    if is_ui_fixture {
        return true;
    }
    if cx.sess().is_test_crate() {
        return false;
    }
    let in_crate_src = filename.contains("crates/") && filename.contains("/src/");
    let is_boundary = filename.ends_with("crates/soldr-platform/src/platform/sync.rs")
        || filename.ends_with("crates/soldr-platform/src/platform/wait.rs");
    let is_test_only = filename.contains("/tests/")
        || filename.ends_with("_tests.rs")
        || filename.ends_with("/tests.rs");
    in_crate_src && !is_boundary && !is_test_only
}

fn source_filename(cx: &LateContext<'_>, span: rustc_span::Span) -> String {
    match cx.sess().source_map().span_to_filename(span) {
        FileName::Real(real_filename) => real_filename
            .local_path()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                real_filename
                    .path(RemapPathScopeComponents::DIAGNOSTICS)
                    .to_string_lossy()
                    .into_owned()
            }),
        filename => filename
            .display(RemapPathScopeComponents::DIAGNOSTICS)
            .to_string(),
    }
}

fn path_ends_with(path: &[Symbol], expected: &[&&str]) -> bool {
    path.len() >= expected.len()
        && path[path.len() - expected.len()..]
            .iter()
            .zip(expected)
            .all(|(actual, expected)| *actual == Symbol::intern(expected))
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}
