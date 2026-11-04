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

/// Emit kinds whose inferred output uses the artifact stem. Link products
/// are fresh writes; only explicitly named link outputs need detachment.
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

/// Adapt a direct toolchain launch to the same compiler-output ownership gate.
pub(crate) fn detach_outputs_for_toolchain(tool: &str, args: &[String]) -> Result<(), SoldrError> {
    if !crate::wrapper::is_wrapper_invocation(tool) {
        return Ok(());
    }
    let compiler_args: Vec<_> = std::iter::once(tool.to_owned())
        .chain(args.iter().cloned())
        .collect();
    detach_outputs_for_direct_exec(&compiler_args)
}

/// Derive the declared output family from rustc arguments.
///
/// Reuses zccache's argument parser. Explicit emit paths take precedence;
/// rustc adapts `-o` only when multiple unnamed emit kinds remain. The
/// characterization cases below are captured from real compiler outputs.
fn declared_output_paths(args: &[String], cwd: &Path) -> Vec<PathBuf> {
    let mut parsed = zccache::depgraph::parse_rustc_args(args, cwd);
    // The pinned parser retains historical explicit emit paths. Rustc merges
    // flags by kind, with the last occurrence replacing even an explicit
    // path with an inferred one. Resolve only the effective list through the
    // same parser so superseded files are never detached.
    let emits = effective_emit_specs(args);
    if !emits.is_empty() {
        let effective =
            zccache::depgraph::parse_rustc_args(&[format!("--emit={}", emits.join(","))], cwd);
        parsed.emit_types = effective.emit_types;
        parsed.explicit_emit_paths = effective.explicit_emit_paths;
    }
    let dir: PathBuf = parsed
        .output_file
        .as_ref()
        .and_then(|output| output.as_path().parent().map(Path::to_path_buf))
        .or_else(|| {
            parsed
                .out_dir
                .as_ref()
                .map(|dir| dir.as_path().to_path_buf())
        })
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
    let implicit_count = parsed
        .emit_types
        .iter()
        .filter(|emit| {
            !parsed
                .explicit_emit_paths
                .iter()
                .any(|(kind, _)| kind == *emit)
        })
        .count();
    for emit in &parsed.emit_types {
        if parsed
            .explicit_emit_paths
            .iter()
            .any(|(kind, _)| kind == emit)
        {
            continue;
        }
        if implicit_count == 1 {
            if let Some(output) = &parsed.output_file {
                derived.push((emit.clone(), output.as_path().to_path_buf()));
                continue;
            }
        }
        match emit.as_str() {
            // Inferred metadata names retain the crate name even when
            // other emit kinds adapt the stem of `-o`.
            "metadata" => derived.push((
                emit.clone(),
                dir.join(format!("lib{crate_name}{suffix}.rmeta")),
            )),
            // Fresh inferred links replace the product instead of
            // overwriting its protected inode.
            "link" => {}
            other => {
                if let Some((_, ext)) = EMIT_EXTENSIONS.iter().find(|(kind, _)| *kind == other) {
                    let stem = parsed
                        .output_file
                        .as_ref()
                        .and_then(|output| output.as_path().file_stem())
                        .unwrap_or_else(|| std::ffi::OsStr::new(&crate_name));
                    let mut name = stem.to_os_string();
                    name.push(format!("{suffix}.{ext}"));
                    derived.push((emit.clone(), dir.join(name)));
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
    if parsed.emit_types.is_empty() {
        if let Some(output) = parsed.output_file.as_ref() {
            push_unique(&mut paths, output.as_path().to_path_buf());
        }
    }
    paths
}

fn effective_emit_specs(args: &[String]) -> Vec<&str> {
    let mut effective: Vec<(&str, &str)> = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        let emits = if arg == "--emit" {
            args.next().map(String::as_str)
        } else {
            arg.strip_prefix("--emit=")
        };
        if let Some(emits) = emits {
            for spec in emits.split(',') {
                let kind = spec.split_once('=').map_or(spec, |(kind, _)| kind);
                if let Some((_, current)) = effective.iter_mut().find(|(key, _)| *key == kind) {
                    *current = spec;
                } else {
                    effective.push((kind, spec));
                }
            }
        }
    }
    effective.into_iter().map(|(_, spec)| spec).collect()
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
    fn duplicate_emit_paths_follow_last_occurrence_per_kind() {
        let root = tempfile::tempdir().unwrap();
        for (emits, expected) in [
            (
                vec!["--emit=metadata=old.rmeta,metadata"],
                vec!["libprobe.rmeta"],
            ),
            (
                vec!["--emit=metadata=old.rmeta", "--emit=metadata"],
                vec!["libprobe.rmeta"],
            ),
            (
                vec!["--emit=metadata=old.rmeta,metadata=new.rmeta"],
                vec!["new.rmeta"],
            ),
            (
                vec!["--emit=metadata=old.rmeta", "--emit=obj"],
                vec!["old.rmeta", "probe.o"],
            ),
            (vec!["--emit=link=old.rlib,link"], vec![]),
        ] {
            let mut args = argv(&[
                "rustc",
                "--crate-name=probe",
                "--crate-type=lib",
                "probe.rs",
            ]);
            args.extend(emits.iter().map(|arg| arg.to_string()));
            let mut actual = declared_output_paths(&args, root.path());
            let mut expected: Vec<_> = expected
                .into_iter()
                .map(|path| root.path().join(path))
                .collect();
            actual.sort();
            expected.sort();
            assert_eq!(actual, expected, "{emits:?}");
        }
    }

    #[test]
    fn explicit_output_plans_only_effective_unnamed_emit_products() {
        let cases: &[(&[&str], &[&str])] = &[
            (
                &["--emit=metadata", "-o", "/out/requested.rmeta"],
                &["/out/requested.rmeta"],
            ),
            (
                &[
                    "--emit=metadata=/explicit/kind.rmeta",
                    "-o",
                    "/out/unwritten",
                ],
                &["/explicit/kind.rmeta"],
            ),
            (
                &["--emit=metadata,link", "-o", "/out/custom.output"],
                &["/out/libprobe.rmeta"],
            ),
            (
                &[
                    "--emit=dep-info,obj",
                    "-o",
                    "/out/custom.output",
                    "--out-dir=/ignored",
                    "-Cextra-filename=-xyz",
                ],
                &["/out/custom-xyz.d", "/out/custom-xyz.o"],
            ),
            (
                &[
                    "--emit=metadata=/explicit/kind.rmeta,obj",
                    "-o",
                    "/out/custom.output",
                ],
                &["/explicit/kind.rmeta", "/out/custom.output"],
            ),
            (
                &[
                    "--emit=metadata,link=/explicit/linked",
                    "-o",
                    "/out/custom.output",
                ],
                &["/out/custom.output", "/explicit/linked"],
            ),
            (
                &[
                    "--emit=metadata,obj",
                    "--out-dir=/out",
                    "-Cextra-filename=-xyz",
                ],
                &["/out/libprobe-xyz.rmeta", "/out/probe-xyz.o"],
            ),
            (
                &["--emit=metadata,metadata", "-o", "/out/requested.rmeta"],
                &["/out/requested.rmeta"],
            ),
            (
                &[
                    "--emit=metadata=/explicit/kind.rmeta,link=/explicit/linked",
                    "-o",
                    "/out/unwritten",
                ],
                &["/explicit/kind.rmeta", "/explicit/linked"],
            ),
        ];
        for (flags, expected) in cases {
            let mut args = argv(&["--crate-name=probe", "--crate-type=lib"]);
            args.extend(argv(flags));
            let mut actual = declared_output_paths(&args, Path::new("/cwd"));
            actual.sort();
            let mut expected: Vec<_> = expected.iter().map(PathBuf::from).collect();
            expected.sort();
            assert_eq!(actual, expected, "{flags:?}");
        }
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
