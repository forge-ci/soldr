//! COW-detach declared rustc outputs before a direct compiler invocation.
//!
//! The wrapper also uses this transition when caching is explicitly disabled
//! or an invocation is not cacheable. Those paths can overwrite outputs from
//! an earlier cached build just as a mid-build fallback can (issue #1817).
//!
//! When the compile daemon becomes unavailable *mid-build*, soldr falls back to
//! running rustc directly. The build began with a managed zccache session, so
//! the front door's whole-tree no-cache preflight
//! (`no_cache_detach::prepare_target_for_unmediated_build`) was skipped — it
//! only runs when the finalized plan has no session. The outputs already in
//! `target/` are therefore still zccache's protected read-only hardlinks, and
//! the direct compiler dies before compiling anything:
//!
//! ```text
//! error: ...\libindexmap-<hash>.rmeta is not writeable
//! ```
//!
//! A hardlink has no separate content-bearing link: every alias names the same
//! file identity. So the fix is not "make it writable" — clearing the read-only
//! attribute in place would unprotect the *cache blob* too. The fix is an
//! ownership transition: replace the local directory entry with a private
//! writable copy, leaving the blob and every sibling alias untouched.
//!
//! ## Why derive the output set instead of scanning
//!
//! This runs inside a live Cargo build that owns the target lock, once per
//! compiler process. Reusing the whole-tree preflight here would contend with
//! that lock and rescan the tree per rustc. Instead the declared output family
//! is derived from the rustc argv, exactly as the mediated path does.
//!
//! ## Why not reuse zccache's detach helper
//!
//! zccache's `break_output_hardlink_before_compile` is ledger-aware and may
//! briefly make the shared identity writable before removing the alias. That is
//! safe for the daemon, which owns the ledger, but not from an unregistered
//! fallback. `no_cache_detach` has the stronger standalone primitive — on
//! Windows it removes only the local entry via `FileDispositionInfoEx` with
//! `FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE`, never touching the shared
//! inode's attributes — so this reuses that.

use std::path::{Path, PathBuf};

use crate::core::SoldrError;

/// Emit kinds whose output path is a simple extension swap on the primary
/// artifact stem. `link` is deliberately absent: the parser's `output_file` /
/// `-o` is authoritative for it, and a link product is a fresh write rather
/// than an overwrite of a delivered cache artifact.
const EMIT_EXTENSIONS: &[(&str, &str)] = &[
    ("dep-info", "d"),
    ("obj", "o"),
    ("asm", "s"),
    ("llvm-ir", "ll"),
    ("llvm-bc", "bc"),
    ("bitcode", "bc"),
    ("mir", "mir"),
];

/// Detach every declared output of `rustc_argv` that is currently a protected
/// hardlink, so a direct compiler can overwrite it.
///
/// `rustc_argv` is `[compiler_path, ...rustc_args]` — the same shape
/// `direct_exec_rustc` receives.
///
/// # Errors
///
/// Returns an error if any declared output cannot be safely detached. Per
/// issue #1817 this is a hard failure: launching the compiler with a partially
/// detached output set risks mutating the cache blob, and the alternative is
/// the misleading `is not writeable` error with nothing pointing at the cache.
pub(crate) fn detach_outputs_for_direct_exec(rustc_argv: &[String]) -> Result<(), SoldrError> {
    let Some((_compiler, args)) = rustc_argv.split_first() else {
        return Ok(());
    };
    let cwd = std::env::current_dir().map_err(|error| {
        SoldrError::Other(format!("resolve cwd for output detachment: {error}"))
    })?;
    let outputs = declared_output_paths(args, &cwd);
    if outputs.is_empty() {
        return Ok(());
    }
    let summary = super::cargo_front_door::no_cache_detach::detach_declared_outputs(&outputs)?;
    if summary.changed_anything() {
        // Loud by design: a silent ownership transition would hide the fact
        // that this build lost its daemon and is now writing outputs the cache
        // had delivered.
        tracing::warn!(
            event = "fallback_output_detach",
            detached_shared = summary.detached_shared,
            made_writable = summary.made_writable,
            outputs = outputs.len(),
            "detached zccache-delivered outputs before a direct compiler run \
             (issue #1817)"
        );
    }
    Ok(())
}

/// Derive the declared output family from rustc arguments.
///
/// Mirrors the mediated path's `rustc_expected_output_paths`. Uses zccache's
/// own argument parser (re-exported as `zccache::depgraph`) so the two sides
/// cannot drift on `--emit`, `--out-dir`, or `-C extra-filename`.
fn declared_output_paths(args: &[String], cwd: &Path) -> Vec<PathBuf> {
    let parsed = zccache::depgraph::parse_rustc_args(args, cwd);
    let dir: PathBuf = parsed
        .out_dir
        .as_ref()
        .map(|d| d.as_path().to_path_buf())
        .unwrap_or_else(|| cwd.to_path_buf());
    let suffix = parsed.extra_filename.clone().unwrap_or_default();
    let crate_name = parsed
        .crate_name
        .clone()
        .unwrap_or_else(|| "unknown".into());

    // Keyed by emit kind so an explicit `--emit=kind=path` replaces the
    // inferred entry for *that kind* precisely. Matching on extension instead
    // would be ambiguous (`llvm-bc` and `bitcode` both produce `.bc`) and
    // would leave the inferred path behind whenever the explicit one
    // redirects to a different directory — which is the normal case.
    let mut derived: Vec<(String, PathBuf)> = Vec::new();
    for emit in &parsed.emit_types {
        match emit.as_str() {
            // rustc names the metadata product `lib<crate><suffix>.rmeta`
            // regardless of crate type, which is exactly the file in the
            // reported failures.
            "metadata" => derived.push((
                emit.clone(),
                dir.join(format!("lib{crate_name}{suffix}.rmeta")),
            )),
            // The parser's `output_file` / `-o` is authoritative for the link
            // product, and a fresh link is not an overwrite of a delivered
            // cache artifact.
            "link" => {}
            other => {
                if let Some((_, ext)) = EMIT_EXTENSIONS.iter().find(|(kind, _)| *kind == other) {
                    derived.push((
                        emit.clone(),
                        dir.join(format!("{crate_name}{suffix}.{ext}")),
                    ));
                }
            }
        }
    }
    for (kind, explicit) in &parsed.explicit_emit_paths {
        let explicit = explicit.as_path().to_path_buf();
        match derived.iter_mut().find(|(k, _)| k == kind) {
            Some(entry) => entry.1 = explicit,
            // A kind we could not infer (or `--emit=link=path`) is still a
            // declared output that the compiler will overwrite.
            None => derived.push((kind.clone(), explicit)),
        }
    }

    let mut paths: Vec<PathBuf> = Vec::new();
    for (_, path) in derived {
        push_unique(&mut paths, path);
    }
    if let Some(output) = parsed.output_file.as_ref() {
        push_unique(&mut paths, output.as_path().to_path_buf());
    }
    paths
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    #[test]
    fn metadata_emit_derives_the_rmeta_that_broke_builds() {
        // The exact shape from the #1817 report: `libindexmap-<hash>.rmeta`
        // delivered as a protected hardlink, then overwritten by a direct rustc.
        let args = argv(&[
            "--crate-name",
            "indexmap",
            "--emit=dep-info,metadata,link",
            "--out-dir",
            "/w/target/debug/deps",
            "-C",
            "extra-filename=-abc123",
        ]);
        let paths = declared_output_paths(&args, Path::new("/w"));

        assert!(
            paths.contains(&PathBuf::from(
                "/w/target/debug/deps/libindexmap-abc123.rmeta"
            )),
            "metadata output missing from {paths:?}"
        );
        assert!(
            paths.contains(&PathBuf::from("/w/target/debug/deps/indexmap-abc123.d")),
            "dep-info output missing from {paths:?}"
        );
        // `link` contributes nothing on its own — `-o` / output_file owns it.
        assert!(
            !paths.iter().any(|p| p.extension().is_none()),
            "link should not have produced an extension-less guess: {paths:?}"
        );
    }

    #[test]
    fn explicit_emit_path_replaces_the_inferred_one() {
        let args = argv(&[
            "--crate-name",
            "demo",
            "--emit=dep-info=/w/custom/demo.d",
            "--out-dir",
            "/w/target/debug/deps",
        ]);
        let paths = declared_output_paths(&args, Path::new("/w"));

        assert!(
            paths.contains(&PathBuf::from("/w/custom/demo.d")),
            "{paths:?}"
        );
        assert!(
            !paths.contains(&PathBuf::from("/w/target/debug/deps/demo.d")),
            "the inferred path must not linger alongside the explicit one: {paths:?}"
        );
    }

    #[test]
    fn no_emit_flags_declares_no_overwrite_targets() {
        // A `--print`/version probe declares no outputs, so the fallback must
        // not invent paths and must not fail.
        let paths = declared_output_paths(&argv(&["--print", "cfg"]), Path::new("/w"));
        assert!(paths.is_empty(), "{paths:?}");
    }

    #[test]
    fn detach_is_a_noop_for_an_empty_argv() {
        // Guards the `split_first` early return: an empty argv must not panic
        // or error on the fallback path.
        detach_outputs_for_direct_exec(&[]).expect("empty argv must be tolerated");
    }

    #[test]
    fn protected_hardlink_output_becomes_writable_without_touching_blob() {
        // The #1817 bug end-to-end: an output delivered by zccache as a
        // read-only hardlink must become privately writable, while the cache
        // blob keeps both its content and its read-only protection. Before the
        // fix, the direct compiler hit `<file> is not writeable` here.
        let root = tempfile::tempdir().expect("tempdir");
        let cache = root.path().join("cache");
        let out = root.path().join("target/debug/deps");
        std::fs::create_dir_all(&cache).expect("mkdir cache");
        std::fs::create_dir_all(&out).expect("mkdir out");

        let blob = cache.join("blob");
        let delivered = out.join("libindexmap-abc123.rmeta");
        std::fs::write(&blob, b"cached bytes").expect("write blob");
        std::fs::hard_link(&blob, &delivered).expect("hard link");
        let mut perms = std::fs::metadata(&blob).expect("stat blob").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&blob, perms).expect("protect blob");

        let args = argv(&[
            "rustc",
            "--crate-name",
            "indexmap",
            "--emit=metadata",
            "--out-dir",
            &out.display().to_string(),
            "-C",
            "extra-filename=-abc123",
        ]);
        detach_outputs_for_direct_exec(&args).expect("detach must succeed");

        // The compiler can now write its output...
        std::fs::write(&delivered, b"new compiler bytes").expect("output must be writable");
        // ...and the cache blob is untouched, still protected, still its own
        // content. This is the invariant that makes the fix safe: writing
        // through the alias, or clearing read-only in place, would corrupt it.
        assert_eq!(
            std::fs::read(&blob).expect("read blob"),
            b"cached bytes",
            "the cache blob must not have been overwritten through the alias"
        );
        assert!(
            std::fs::metadata(&blob)
                .expect("stat blob")
                .permissions()
                .readonly(),
            "the cache blob must remain read-only"
        );
    }

    #[test]
    fn missing_outputs_are_tolerated() {
        // First compile in a fresh target: nothing is materialized yet, so
        // there is nothing protected to detach and this must succeed quietly.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("target/debug/deps");
        let args = argv(&[
            "rustc",
            "--crate-name",
            "ghost",
            "--emit=metadata",
            "--out-dir",
            &out.display().to_string(),
        ]);
        detach_outputs_for_direct_exec(&args).expect("absent outputs must be tolerated");
    }
}
