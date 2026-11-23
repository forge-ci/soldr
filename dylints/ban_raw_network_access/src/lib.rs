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
    /// Rejects direct reqwest transport operations in production soldr-fetch
    /// code. The `fetch::stream_download` / `fetch::segmented_download`
    /// boundary is the one blessed place that owns timeout policy, streaming,
    /// retry-compatible diagnostics, and hashing (see `is_boundary_file`).
    pub BAN_RAW_NETWORK_ACCESS,
    Deny,
    "require production fetch network I/O to use fetch::stream_download"
}

const BANNED_REQWEST_METHODS: &[&str] = &[
    "builder",
    "new",
    "get",
    "post",
    "put",
    "patch",
    "delete",
    "head",
    "request",
    "send",
    "execute",
    "text",
    "bytes",
    "chunk",
    "bytes_stream",
    "json",
];

impl<'tcx> LateLintPass<'tcx> for BanRawNetworkAccess {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        if !span_is_in_scope(cx, expr.span) {
            return;
        }
        let def_id = match expr.kind {
            ExprKind::MethodCall(..) => cx.typeck_results().type_dependent_def_id(expr.hir_id),
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
        let is_reqwest = path.iter().any(|part| *part == Symbol::intern("reqwest"));
        let Some(method) = path.last().map(Symbol::as_str) else {
            return;
        };
        if is_reqwest && BANNED_REQWEST_METHODS.contains(&method) {
            cx.opt_span_lint(
                BAN_RAW_NETWORK_ACCESS,
                Some(expr.span),
                DiagDecorator(move |diag| {
                    diag.primary_message(format!(
                        "reqwest `{method}` bypasses fetch::stream_download; use its control or asset request helpers"
                    ));
                }),
            );
        }
    }
}

fn span_is_in_scope(cx: &LateContext<'_>, span: rustc_span::Span) -> bool {
    let filename = source_filename(cx, span).replace('\\', "/");
    if filename.starts_with("ui/") || filename.contains("/ui/") {
        return true;
    }
    let is_fetch_source = filename.contains("/crates/soldr-fetch/src/")
        || filename.starts_with("crates/soldr-fetch/src/");
    is_fetch_source && !is_boundary_file(&filename)
}

/// The blessed fetch network boundary: the files that own timeout policy,
/// streaming, retry-compatible diagnostics, and hashing, and are therefore
/// allowed to call raw reqwest. Originally a single file; the segmented
/// downloader (soldr#2320) was extracted into its own module and test
/// helpers to stay under the per-file line ceiling (soldr#1966), so the
/// boundary now spans those files too. Matching on the path suffix keeps
/// both `/crates/.../fetch/x.rs` and a bare `fetch/x.rs` in scope.
fn is_boundary_file(filename: &str) -> bool {
    const BOUNDARY_SUFFIXES: &[&str] = &[
        "fetch/stream_download.rs",
        "fetch/segmented_download.rs",
        "fetch/segmented_download_tests.rs",
        "fetch/segmented_download_tests_extra.rs",
    ];
    BOUNDARY_SUFFIXES
        .iter()
        .any(|&suffix| filename.ends_with(suffix))
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

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}
