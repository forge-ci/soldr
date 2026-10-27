//! Strip cargo-recreatable noise from a cargo `target/` directory.
//!
//! Complement to [`super::prune_target`]: where `prune_target` deletes
//! orphan hash-suffixed siblings, this module removes whole categories
//! of files that cargo can rebuild cheaply from cached artifacts. The
//! intent is to shrink a `target/` snapshot before it is tarred up for
//! transport across CI runners, so the rehydrate ships dramatically
//! fewer bytes.
//!
//! Every rule is opt-in. Default `StripTargetOptions::none()` produces
//! a no-op walk. Callers select rules explicitly.
//!
//! Categories
//! ----------
//!
//! - **Large build-script stderr** (`target/<profile>/build/*/stderr`
//!   above [`STDERR_KEEP_BELOW_BYTES`]). Crates like `ring`, `bindgen`,
//!   `aws-lc-sys` dump tens of MB of inline-asm logspam here. Cargo
//!   never reads it. Files below the threshold are preserved so a human
//!   can still grep them.
//! - **Build-script-build binaries** (`target/<profile>/build/*/build-script-build*`
//!   and `.pdb` siblings). Cargo recompiles these from cached `.rlib`s
//!   in seconds.
//! - **Profile subdirs** (`target/<profile>/{examples,doc,tests}/`).
//!   Useful for local dev, dead weight for CI cache archives.
//! - **Debug sidecars** (`.dwo`, `.pdb`, `.dSYM/` under
//!   `target/<profile>/deps/`). Cargo regenerates these on the next
//!   build that asks for debuginfo.
//! - **Incremental dirs** (`target/<profile>/incremental/`). Cargo's
//!   incremental compilation state. Tied to specific source files;
//!   downstream builds against different sources discard it anyway.
//!   Safe to drop from snapshot tarballs (cook output, CI caches).
//! - **Stub binaries** (top-level executables at `target/<profile>/`).
//!   Cargo-chef builds a synthetic stub project whose executable lands
//!   here; the downstream user's real binary has a different name and
//!   this artifact is dead weight in a cook tarball. Use sparingly:
//!   only enable in contexts where you know the only top-level
//!   binary is the stub (e.g. cook output).
//!
//! The caller is responsible for the `.cargo-lock` active-build guard;
//! reuse [`super::prune_target::find_active_cargo_lock`] before calling
//! this module so the strip never races a live build.

use std::{
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

/// Build-script `stderr` files **strictly larger** than this many bytes
/// are eligible for stripping. The threshold (128 KiB) keeps small
/// "no warnings" stubs around for grep-debuggability.
pub const STDERR_KEEP_BELOW_BYTES: u64 = 128 * 1024;

/// Top-level profile subdir names that the
/// `strip_examples_doc_tests` rule clears wholesale.
pub const EXAMPLES_DOC_TESTS_SUBDIRS: &[&str] = &["examples", "doc", "tests"];

#[derive(Debug, Error)]
pub enum StripError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone)]
pub struct StripTargetOptions {
    /// Path to a cargo `target/` directory to scan. Subdirectories
    /// matching profile-shaped layouts (`target/<profile>/`) are
    /// walked; everything else is left alone.
    pub target_dir: PathBuf,
    /// When `true`, no entries are deleted. The returned report still
    /// describes the would-be action.
    pub dry_run: bool,
    /// Strip build-script `stderr` files larger than
    /// [`STDERR_KEEP_BELOW_BYTES`].
    pub strip_large_stderr: bool,
    /// Strip `build-script-build*` binaries and their `.pdb` siblings.
    pub strip_build_script_binaries: bool,
    /// Strip `examples/`, `doc/`, `tests/` profile subdirs.
    pub strip_examples_doc_tests: bool,
    /// Strip `.dwo`, `.pdb`, `.dSYM/` debug sidecars under
    /// `target/<profile>/deps/`.
    pub strip_debug_sidecars: bool,
    /// Strip `target/<profile>/incremental/` entirely. Cargo's
    /// incremental cache is invalidated by downstream source changes,
    /// so it has no value in a snapshot tarball.
    pub strip_incremental: bool,
    /// Strip top-level executable files at `target/<profile>/<name>`
    /// (and their `.pdb`/`.dSYM/` siblings). Only safe in contexts
    /// where the only top-level binary is a synthetic stub — e.g.
    /// `soldr cook` output, where cargo-chef builds an `app`-style
    /// stub project but the downstream consumer's real binary has a
    /// different name.
    pub strip_stub_binaries: bool,
}

impl StripTargetOptions {
    /// All rules off — dry-run by default. Use this and flip the
    /// rules you want; mirrors `PruneTargetOptions::new`.
    pub fn none(target_dir: PathBuf) -> Self {
        Self {
            target_dir,
            dry_run: true,
            strip_large_stderr: false,
            strip_build_script_binaries: false,
            strip_examples_doc_tests: false,
            strip_debug_sidecars: false,
            strip_incremental: false,
            strip_stub_binaries: false,
        }
    }

    /// Every legacy rule on; dry-run by default. The CI-profile preset
    /// for `cache trim-target`.
    ///
    /// `strip_incremental` and `strip_stub_binaries` are intentionally
    /// left off here. The incremental rule is functionally covered by
    /// the inline `remove_incremental_dirs` call inside the
    /// `cache trim-target` pipeline, and stub-binary stripping is only
    /// safe in cook-output contexts; callers that know either
    /// invariant enable them explicitly.
    pub fn all(target_dir: PathBuf) -> Self {
        Self {
            target_dir,
            dry_run: true,
            strip_large_stderr: true,
            strip_build_script_binaries: true,
            strip_examples_doc_tests: true,
            strip_debug_sidecars: true,
            strip_incremental: false,
            strip_stub_binaries: false,
        }
    }

    /// Preset for `soldr cook` post-build trim (issue #459). Strips
    /// cargo-recreatable noise that does not participate in dependency
    /// freshness — incremental state, the synthetic stub binary, large stderr
    /// blobs, debug sidecars, and the per-profile `examples/`, `doc/`, `tests/`
    /// subdirs. Build-script executables are deliberately retained: Cargo
    /// requires them as part of a fully rematerialized dependency closure.
    ///
    /// Dry-run defaults to `false` because the caller is `cook` —
    /// the trim is part of the cook contract, not a manual audit.
    pub fn cook(target_dir: PathBuf) -> Self {
        Self {
            target_dir,
            dry_run: false,
            strip_large_stderr: true,
            strip_build_script_binaries: false,
            strip_examples_doc_tests: true,
            strip_debug_sidecars: true,
            strip_incremental: true,
            strip_stub_binaries: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
pub enum StripCategory {
    #[strum(serialize = "large_stderr")]
    LargeStderr,
    #[strum(serialize = "build_script_binary")]
    BuildScriptBinary,
    #[strum(serialize = "examples_doc_tests")]
    ExamplesDocTests,
    #[strum(serialize = "debug_sidecar")]
    DebugSidecar,
    #[strum(serialize = "incremental_dir")]
    IncrementalDir,
    #[strum(serialize = "stub_binary")]
    StubBinary,
}

impl StripCategory {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(Debug, Clone)]
pub struct StripEntry {
    pub path: PathBuf,
    pub category: StripCategory,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct StripReport {
    /// Number of profile dirs walked.
    pub profiles_scanned: usize,
    /// Number of entries deleted (or that would be deleted in dry-run).
    pub deleted: usize,
    /// Bytes reclaimed (or reclaimable) by the deletions.
    pub reclaimed_bytes: u64,
    /// Every entry the scan classified for deletion, in walk order.
    pub entries: Vec<StripEntry>,
}

/// Walk a cargo `target/` directory and strip the categories enabled
/// in `opts`.
///
/// Idempotent: a second run on the same directory finds nothing because
/// the targeted files no longer exist.
pub fn strip_target(opts: &StripTargetOptions) -> Result<StripReport, StripError> {
    let target_dir = &opts.target_dir;
    let metadata = fs::metadata(target_dir).map_err(|source| StripError::Io {
        path: target_dir.clone(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(StripError::Io {
            path: target_dir.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "strip-target requires a directory",
            ),
        });
    }

    let profiles = read_profile_dirs(target_dir).map_err(|source| StripError::Io {
        path: target_dir.clone(),
        source,
    })?;

    let mut entries: Vec<StripEntry> = Vec::new();
    let mut profiles_scanned = 0usize;

    for profile in &profiles {
        profiles_scanned += 1;
        if opts.strip_large_stderr || opts.strip_build_script_binaries {
            collect_build_subdir(profile, opts, &mut entries)?;
        }
        if opts.strip_examples_doc_tests {
            collect_profile_subdirs(profile, EXAMPLES_DOC_TESTS_SUBDIRS, &mut entries)?;
        }
        if opts.strip_debug_sidecars {
            collect_debug_sidecars_under_deps(profile, &mut entries)?;
        }
        if opts.strip_incremental {
            collect_incremental_dir(profile, &mut entries)?;
        }
        if opts.strip_stub_binaries {
            collect_stub_binaries(profile, &mut entries)?;
        }
    }

    let mut report = StripReport {
        profiles_scanned,
        ..StripReport::default()
    };

    if !opts.dry_run {
        for entry in &entries {
            delete_entry(&entry.path).map_err(|source| StripError::Io {
                path: entry.path.clone(),
                source,
            })?;
        }
    }

    report.deleted = entries.len();
    for entry in &entries {
        report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(entry.size_bytes);
    }
    report.entries = entries;
    Ok(report)
}

/// Enumerate `target/<profile>/` dirs. Skips dotted entries
/// (`.fingerprint` is one of cargo's own state dirs at the top level,
/// not a profile).
fn read_profile_dirs(target_dir: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut out = Vec::new();
    let read = match fs::read_dir(target_dir) {
        Ok(it) => it,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err),
    };
    for entry in read.flatten() {
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !ft.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        out.push(entry.path());
    }
    Ok(out)
}

fn collect_build_subdir(
    profile: &Path,
    opts: &StripTargetOptions,
    entries: &mut Vec<StripEntry>,
) -> Result<(), StripError> {
    let build = profile.join("build");
    let read = match fs::read_dir(&build) {
        Ok(it) => it,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(StripError::Io {
                path: build,
                source,
            })
        }
    };
    for unit in read.flatten() {
        let unit_path = unit.path();
        let ft = match unit.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !ft.is_dir() {
            continue;
        }
        let unit_read = match fs::read_dir(&unit_path) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for child in unit_read.flatten() {
            let child_path = child.path();
            let Some(name) = child_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if opts.strip_large_stderr && name == "stderr" {
                let len = file_len(&child_path).unwrap_or(0);
                if len > STDERR_KEEP_BELOW_BYTES {
                    entries.push(StripEntry {
                        path: child_path.clone(),
                        category: StripCategory::LargeStderr,
                        size_bytes: len,
                    });
                }
                continue;
            }
            if opts.strip_build_script_binaries && is_build_script_binary(name) {
                let len = file_len(&child_path).unwrap_or(0);
                entries.push(StripEntry {
                    path: child_path,
                    category: StripCategory::BuildScriptBinary,
                    size_bytes: len,
                });
            }
        }
    }
    Ok(())
}

fn is_build_script_binary(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    stem == "build-script-build" || stem.starts_with("build-script-build-")
}

fn collect_profile_subdirs(
    profile: &Path,
    subdirs: &[&str],
    entries: &mut Vec<StripEntry>,
) -> Result<(), StripError> {
    for sub in subdirs {
        let path = profile.join(sub);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(StripError::Io { path, source }),
        };
        if !metadata.is_dir() {
            continue;
        }
        let size = directory_size(&path);
        entries.push(StripEntry {
            path,
            category: StripCategory::ExamplesDocTests,
            size_bytes: size,
        });
    }
    Ok(())
}

fn collect_debug_sidecars_under_deps(
    profile: &Path,
    entries: &mut Vec<StripEntry>,
) -> Result<(), StripError> {
    let deps = profile.join("deps");
    let read = match fs::read_dir(&deps) {
        Ok(it) => it,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(StripError::Io { path: deps, source }),
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            if name.ends_with(".dSYM") {
                let size = directory_size(&path);
                entries.push(StripEntry {
                    path,
                    category: StripCategory::DebugSidecar,
                    size_bytes: size,
                });
            }
            continue;
        }
        if name.ends_with(".dwo") || name.ends_with(".pdb") {
            let size = file_len(&path).unwrap_or(0);
            entries.push(StripEntry {
                path,
                category: StripCategory::DebugSidecar,
                size_bytes: size,
            });
        }
    }
    Ok(())
}

fn collect_incremental_dir(
    profile: &Path,
    entries: &mut Vec<StripEntry>,
) -> Result<(), StripError> {
    let path = profile.join("incremental");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(StripError::Io { path, source }),
    };
    if !metadata.is_dir() {
        return Ok(());
    }
    let size = directory_size(&path);
    entries.push(StripEntry {
        path,
        category: StripCategory::IncrementalDir,
        size_bytes: size,
    });
    Ok(())
}

fn collect_stub_binaries(profile: &Path, entries: &mut Vec<StripEntry>) -> Result<(), StripError> {
    // The synthetic stub binary cargo-chef produces lives at the
    // top of `target/<profile>/<name>(.exe)`, alongside `.pdb` and
    // `.dSYM/` debug siblings. Cargo's own state files start with
    // `.` (e.g. `.fingerprint`, `.cargo-lock`) and the deps/build
    // siblings are directories — so we only delete *files* at the
    // profile root, plus `.dSYM/` dirs (Apple debuginfo bundles).
    let read = match fs::read_dir(profile) {
        Ok(it) => it,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(StripError::Io {
                path: profile.to_path_buf(),
                source,
            })
        }
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            if name.ends_with(".dSYM") {
                let size = directory_size(&path);
                entries.push(StripEntry {
                    path,
                    category: StripCategory::StubBinary,
                    size_bytes: size,
                });
            }
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let size = file_len(&path).unwrap_or(0);
        entries.push(StripEntry {
            path,
            category: StripCategory::StubBinary,
            size_bytes: size,
        });
    }
    Ok(())
}

fn file_len(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|m| m.len())
}

fn directory_size(path: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match fs::read_dir(&dir) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    total
}

fn delete_entry(path: &Path) -> Result<(), std::io::Error> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    fn touch_dir(path: &Path) {
        fs::create_dir_all(path).unwrap();
    }

    fn write_bytes(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = File::create(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    #[test]
    fn category_strings_are_stable() {
        let expected = [
            (StripCategory::LargeStderr, "large_stderr"),
            (StripCategory::BuildScriptBinary, "build_script_binary"),
            (StripCategory::ExamplesDocTests, "examples_doc_tests"),
            (StripCategory::DebugSidecar, "debug_sidecar"),
            (StripCategory::IncrementalDir, "incremental_dir"),
            (StripCategory::StubBinary, "stub_binary"),
        ];
        for (category, name) in expected {
            assert_eq!(category.as_str(), name);
        }
    }

    #[test]
    fn empty_target_is_a_noop() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let opts = StripTargetOptions {
            dry_run: false,
            ..StripTargetOptions::all(target)
        };
        let report = strip_target(&opts).unwrap();
        assert_eq!(report.profiles_scanned, 0);
        assert_eq!(report.deleted, 0);
        assert!(report.entries.is_empty());
    }

    #[test]
    fn large_stderr_dropped_small_kept() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let small = target.join("debug/build/ring-aaaa/stderr");
        let large = target.join("debug/build/bindgen-bbbb/stderr");
        write_bytes(&small, &[0u8; 1024]);
        // > 128 KiB so it should be stripped.
        write_bytes(&large, &vec![0u8; 200 * 1024]);

        let opts = StripTargetOptions {
            dry_run: false,
            strip_large_stderr: true,
            ..StripTargetOptions::none(target.clone())
        };
        let report = strip_target(&opts).unwrap();

        assert_eq!(report.deleted, 1);
        assert_eq!(report.entries[0].category, StripCategory::LargeStderr);
        assert!(small.exists(), "small stderr must survive");
        assert!(!large.exists(), "large stderr must be deleted");
    }

    #[test]
    fn stderr_exactly_at_threshold_kept() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let at_threshold = target.join("debug/build/foo-aaaa/stderr");
        write_bytes(&at_threshold, &vec![0u8; STDERR_KEEP_BELOW_BYTES as usize]);
        let opts = StripTargetOptions {
            dry_run: false,
            strip_large_stderr: true,
            ..StripTargetOptions::none(target)
        };
        let report = strip_target(&opts).unwrap();
        assert_eq!(report.deleted, 0, "exact-threshold must survive");
        assert!(at_threshold.exists());
    }

    #[test]
    fn build_script_binary_dropped() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let bin_unix = target.join("debug/build/foo-aaaa/build-script-build");
        let bin_win = target.join("debug/build/foo-aaaa/build-script-build.exe");
        write_bytes(&bin_unix, b"unix binary");
        write_bytes(&bin_win, b"win binary");
        // Sibling that must NOT be touched.
        let output = target.join("debug/build/foo-aaaa/output");
        write_bytes(&output, b"sibling output");
        let opts = StripTargetOptions {
            dry_run: false,
            strip_build_script_binaries: true,
            ..StripTargetOptions::none(target)
        };
        let report = strip_target(&opts).unwrap();
        assert_eq!(report.deleted, 2);
        for e in &report.entries {
            assert_eq!(e.category, StripCategory::BuildScriptBinary);
        }
        assert!(!bin_unix.exists());
        assert!(!bin_win.exists());
        assert!(output.exists(), "non-matching siblings stay");
    }

    #[test]
    fn examples_doc_tests_dropped() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let ex = target.join("debug/examples");
        let doc = target.join("debug/doc");
        let tests = target.join("debug/tests");
        let deps = target.join("debug/deps");
        touch_dir(&ex);
        touch_dir(&doc);
        touch_dir(&tests);
        touch_dir(&deps);
        write_bytes(&ex.join("a"), b"x");
        write_bytes(&doc.join("index.html"), b"x");
        write_bytes(&tests.join("a"), b"x");
        write_bytes(&deps.join("libfoo-aaaaaaaaaaaaa.rlib"), b"x");

        let opts = StripTargetOptions {
            dry_run: false,
            strip_examples_doc_tests: true,
            ..StripTargetOptions::none(target)
        };
        let report = strip_target(&opts).unwrap();
        assert_eq!(report.deleted, 3);
        for e in &report.entries {
            assert_eq!(e.category, StripCategory::ExamplesDocTests);
        }
        assert!(!ex.exists());
        assert!(!doc.exists());
        assert!(!tests.exists());
        assert!(deps.exists(), "deps/ must survive examples/doc/tests strip");
    }

    #[test]
    fn debug_sidecars_dropped() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let deps = target.join("debug/deps");
        touch_dir(&deps);
        let rlib = deps.join("libfoo-aaaaaaaaaaaaa.rlib");
        let pdb = deps.join("libfoo-aaaaaaaaaaaaa.pdb");
        let dwo = deps.join("libfoo-aaaaaaaaaaaaa.dwo");
        let dsym = deps.join("libfoo-aaaaaaaaaaaaa.dSYM");
        write_bytes(&rlib, b"keep");
        write_bytes(&pdb, b"drop pdb");
        write_bytes(&dwo, b"drop dwo");
        touch_dir(&dsym);
        write_bytes(&dsym.join("Contents/Info.plist"), b"...");

        let opts = StripTargetOptions {
            dry_run: false,
            strip_debug_sidecars: true,
            ..StripTargetOptions::none(target)
        };
        let report = strip_target(&opts).unwrap();
        assert_eq!(report.deleted, 3);
        for e in &report.entries {
            assert_eq!(e.category, StripCategory::DebugSidecar);
        }
        assert!(rlib.exists(), ".rlib must survive sidecar strip");
        assert!(!pdb.exists());
        assert!(!dwo.exists());
        assert!(!dsym.exists());
    }

    #[test]
    fn dry_run_reports_but_does_not_delete() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let large = target.join("debug/build/foo-aaaa/stderr");
        write_bytes(&large, &vec![0u8; 200 * 1024]);
        let opts = StripTargetOptions {
            dry_run: true,
            strip_large_stderr: true,
            ..StripTargetOptions::none(target)
        };
        let report = strip_target(&opts).unwrap();
        assert_eq!(report.deleted, 1);
        assert!(large.exists(), "dry-run preserves files");
    }

    #[test]
    fn none_opts_is_no_op_even_with_garbage_target() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let large = target.join("debug/build/foo-aaaa/stderr");
        write_bytes(&large, &vec![0u8; 200 * 1024]);
        let ex = target.join("debug/examples");
        touch_dir(&ex);
        write_bytes(&ex.join("a"), b"x");

        let opts = StripTargetOptions {
            dry_run: false,
            ..StripTargetOptions::none(target)
        };
        let report = strip_target(&opts).unwrap();
        assert_eq!(report.deleted, 0);
        assert!(large.exists());
        assert!(ex.exists());
    }

    #[test]
    fn second_run_is_idempotent() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let large = target.join("debug/build/foo-aaaa/stderr");
        write_bytes(&large, &vec![0u8; 200 * 1024]);

        let opts = StripTargetOptions {
            dry_run: false,
            strip_large_stderr: true,
            ..StripTargetOptions::none(target.clone())
        };
        let first = strip_target(&opts).unwrap();
        assert_eq!(first.deleted, 1);
        let second = strip_target(&opts).unwrap();
        assert_eq!(second.deleted, 0, "second run finds nothing");
    }

    #[test]
    fn rejects_non_directory_target() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("not-a-dir");
        write_bytes(&file, b"x");
        let opts = StripTargetOptions::all(file);
        let err = strip_target(&opts).expect_err("file path must error");
        assert!(format!("{err}").contains("not-a-dir"));
    }

    #[test]
    fn build_script_build_suffix_variants() {
        assert!(is_build_script_binary("build-script-build"));
        assert!(is_build_script_binary("build-script-build.exe"));
        assert!(is_build_script_binary("build-script-build.pdb"));
        assert!(is_build_script_binary("build-script-build-suffix.exe"));
        assert!(!is_build_script_binary("build-script-other"));
        assert!(!is_build_script_binary("invoked.timestamp"));
    }

    // -------------------------------------------------------------
    // Incremental dir + stub binary strip rules (issue #459).
    //
    // Both rules are intentionally NOT enabled by `all()` outside the
    // cook context: the incremental rule is safe in tarball-snapshot
    // contexts but actively harmful in interactive dev (cargo will
    // refuse to incremental-recompile), and the stub-binary rule is
    // only safe when the caller knows no real user binary exists at
    // the profile root.
    // -------------------------------------------------------------

    #[test]
    fn incremental_dir_dropped() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let inc = target.join("release/incremental");
        let deps = target.join("release/deps");
        touch_dir(&inc);
        write_bytes(
            &inc.join("s-abc/work-product.bin"),
            b"\0".repeat(1024).as_slice(),
        );
        write_bytes(&inc.join("s-abc/dep-graph.bin"), &[0u8; 256]);
        touch_dir(&deps);
        let rlib = deps.join("libfoo-aaaaaaaaaaaaa.rlib");
        write_bytes(&rlib, b"keep");

        let opts = StripTargetOptions {
            dry_run: false,
            strip_incremental: true,
            ..StripTargetOptions::none(target)
        };
        let report = strip_target(&opts).unwrap();
        assert_eq!(report.deleted, 1, "the incremental/ dir is one entry");
        assert_eq!(report.entries[0].category, StripCategory::IncrementalDir);
        assert!(
            report.entries[0].size_bytes > 0,
            "size should account for contents"
        );
        assert!(!inc.exists(), "incremental/ must be removed");
        assert!(rlib.exists(), "deps/ must survive incremental strip");
    }

    #[test]
    fn incremental_strip_no_op_when_dir_missing() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let deps = target.join("release/deps");
        touch_dir(&deps);
        write_bytes(&deps.join("libfoo.rlib"), b"keep");

        let opts = StripTargetOptions {
            dry_run: false,
            strip_incremental: true,
            ..StripTargetOptions::none(target)
        };
        let report = strip_target(&opts).unwrap();
        assert_eq!(report.deleted, 0);
        assert!(deps.exists());
    }

    #[test]
    fn stub_binary_dropped_at_profile_root() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        // Profile root: the cargo-chef stub lives directly here.
        let stub_unix = target.join("release/app");
        let stub_win = target.join("release/app.exe");
        let stub_pdb = target.join("release/app.pdb");
        let stub_dsym = target.join("release/app.dSYM");
        let deps = target.join("release/deps");
        let fingerprint = target.join("release/.fingerprint");
        let cargo_lock = target.join("release/.cargo-lock");
        write_bytes(&stub_unix, b"unix stub");
        write_bytes(&stub_win, b"win stub");
        write_bytes(&stub_pdb, b"pdb");
        touch_dir(&stub_dsym);
        write_bytes(&stub_dsym.join("Contents/Info.plist"), b"...");
        touch_dir(&deps);
        write_bytes(&deps.join("libfoo.rlib"), b"keep");
        touch_dir(&fingerprint);
        write_bytes(&fingerprint.join("foo-aaaa/invoked.timestamp"), b"");
        write_bytes(&cargo_lock, b"");

        let opts = StripTargetOptions {
            dry_run: false,
            strip_stub_binaries: true,
            ..StripTargetOptions::none(target)
        };
        let report = strip_target(&opts).unwrap();
        // Three files + one .dSYM directory.
        assert_eq!(report.deleted, 4);
        for e in &report.entries {
            assert_eq!(e.category, StripCategory::StubBinary);
        }
        assert!(!stub_unix.exists());
        assert!(!stub_win.exists());
        assert!(!stub_pdb.exists());
        assert!(!stub_dsym.exists());
        // Cargo state and the deps/ artifacts must be untouched.
        assert!(deps.exists());
        assert!(fingerprint.exists());
        assert!(cargo_lock.exists(), ".cargo-lock survives stub strip");
    }

    #[test]
    fn stub_binary_strip_ignores_subdirs_other_than_dsym() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        // Cargo creates plenty of subdirs at the profile root that
        // are NOT stub artifacts: deps/, build/, examples/, etc.
        // The stub-binary rule must never touch them.
        touch_dir(&target.join("release/deps"));
        touch_dir(&target.join("release/build"));
        touch_dir(&target.join("release/examples"));
        let stub = target.join("release/app.exe");
        write_bytes(&stub, b"stub");

        let opts = StripTargetOptions {
            dry_run: false,
            strip_stub_binaries: true,
            ..StripTargetOptions::none(target.clone())
        };
        let report = strip_target(&opts).unwrap();
        assert_eq!(report.deleted, 1);
        assert!(!stub.exists());
        assert!(target.join("release/deps").exists());
        assert!(target.join("release/build").exists());
        assert!(target.join("release/examples").exists());
    }

    #[test]
    fn cook_preset_preserves_build_script_binaries_for_rematerialization() {
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let build_script = target.join("release/build/native-sys-aaaa/build-script-build");
        let build_output = target.join("release/build/native-sys-aaaa/output");
        write_bytes(&build_script, b"build script executable");
        write_bytes(&build_output, b"cargo:rerun-if-changed=native.c\n");

        let opts = StripTargetOptions::cook(target);
        strip_target(&opts).unwrap();

        assert!(
            build_script.exists(),
            "a rematerialized target must retain Cargo's build-script executable"
        );
        assert!(
            build_output.exists(),
            "build-script output must also survive"
        );
    }

    #[test]
    fn cook_preset_enables_incremental_and_stub_strip() {
        // Smoke-test the `cook()` constructor — the rules it enables
        // are the contract surface for `soldr cook` post-build trim.
        let temp = tempdir().unwrap();
        let target = temp.path().to_path_buf();
        let inc = target.join("release/incremental");
        touch_dir(&inc);
        write_bytes(&inc.join("foo"), &[0u8; 64]);
        let stub = target.join("release/app.exe");
        write_bytes(&stub, b"stub");
        let deps = target.join("release/deps");
        touch_dir(&deps);
        write_bytes(&deps.join("libfoo.rlib"), b"keep");

        let opts = StripTargetOptions::cook(target.clone());
        let report = strip_target(&opts).unwrap();
        assert!(report.deleted >= 2);
        assert!(!inc.exists());
        assert!(!stub.exists());
        assert!(deps.exists(), "deps/ survives cook trim");
        assert!(
            !opts.dry_run,
            "cook preset commits — caller controls dry-run separately"
        );
    }
}
