//! Reclaim stale per-release `~/.soldr/v<version>` dirs and superseded
//! `~/.soldr/bin/<tool>-<version>` revisions (soldr#4029).
//!
//! Every soldr release materializes its own `v<version>/` tree (shim images,
//! 0.3-2.2 GB each) and nothing ever deleted the old ones. Likewise a tool
//! revision bump (`llvm-21.1.5` -> `llvm-21.1.5-r2`) left the old install
//! beside the new one.
//!
//! Policy, run from the auto-GC sweep:
//! * version dirs: always keep the running version (including dev
//!   `v<ver>-<hash>` variants) and the newest older version; delete any other
//!   version dir unused for [`DEFAULT_MAX_UNUSED_DAYS`].
//! * tool revisions: an install superseded by a higher `-rN` packaging
//!   revision of the same release is deleted once unused; otherwise per tool
//!   family keep the two most recently used
//!   installs; delete older ones unused for the same window. `dylint*` and
//!   `rustup` entries are left to their own owners.
//!
//! "Used" is the newest mtime/atime among the entry and its children two
//! levels deep, which covers shim `*.materialized-v1.json` refreshes and
//! executable reads under `bin/`.
//!
//! Content-addressed sharing across versions was evaluated: shim images are
//! already keyed by binary hash and no hash recurs across releases, so
//! linking would reclaim nothing; deletion is what bounds the footprint.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::core::SoldrPaths;

pub(super) const DEFAULT_MAX_UNUSED_DAYS: u64 = 14;
const MAX_UNUSED_DAYS_ENV: &str = "SOLDR_VERSION_GC_DAYS";
const KEEP_TOOL_REVISIONS: usize = 2;

#[derive(Debug, Default)]
pub(super) struct StaleVersionReport {
    pub(super) version_dirs_removed: Vec<String>,
    pub(super) tool_revisions_removed: Vec<String>,
}

/// Unused window from `SOLDR_VERSION_GC_DAYS`; `0` disables the sweep.
pub(super) fn max_unused_from_env() -> Option<Duration> {
    let days = std::env::var(MAX_UNUSED_DAYS_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_UNUSED_DAYS);
    (days > 0).then(|| Duration::from_secs(days.saturating_mul(24 * 60 * 60)))
}

pub(super) fn sweep_stale_versions(
    paths: &SoldrPaths,
    current_version: &str,
    now: SystemTime,
    max_unused: Duration,
) -> StaleVersionReport {
    let mut report = StaleVersionReport::default();
    let Ok(current) = semver::Version::parse(current_version) else {
        return report;
    };
    let version_dirs: Vec<(PathBuf, semver::Version)> = list_dirs(&paths.root)
        .into_iter()
        .filter_map(|(path, name)| parse_version_dir(&name).map(|v| (path, v)))
        .collect();
    let previous = version_dirs
        .iter()
        .map(|(_, v)| v)
        .filter(|v| **v < current)
        .max()
        .cloned();
    for (path, version) in version_dirs {
        if version == current || Some(&version) == previous.as_ref() {
            continue;
        }
        if is_stale(&path, now, max_unused) && std::fs::remove_dir_all(&path).is_ok() {
            report.version_dirs_removed.push(file_name(&path));
        }
    }

    let mut families: std::collections::BTreeMap<String, Vec<(PathBuf, SystemTime)>> =
        std::collections::BTreeMap::new();
    let bin_dirs = list_dirs(&paths.bin);
    let bin_names: Vec<String> = bin_dirs.iter().map(|(_, n)| n.clone()).collect();
    for (path, name) in bin_dirs {
        let Some(family) = tool_family(&name) else {
            continue;
        };
        let used = last_used(&path);
        families.entry(family).or_default().push((path, used));
    }
    for (_, mut installs) in families {
        installs.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        // `llvm-21.1.5` is superseded by `llvm-21.1.5-r2`: only the highest
        // packaging revision of a release counts toward the kept set.
        let (superseded, live): (Vec<_>, Vec<_>) = installs.into_iter().partition(|(path, _)| {
            let name = file_name(path);
            let (release, rev) = split_revision(&name);
            bin_names.iter().any(|other| {
                let (other_release, other_rev) = split_revision(other);
                other_release == release && other_rev > rev
            })
        });
        let candidates = superseded
            .into_iter()
            .chain(live.into_iter().skip(KEEP_TOOL_REVISIONS));
        for (path, used) in candidates {
            if !older_than(used, now, max_unused) {
                continue;
            }
            if std::fs::remove_dir_all(&path).is_ok() {
                let mut lock = path.clone().into_os_string();
                lock.push(".lock");
                let _ = std::fs::remove_file(PathBuf::from(lock));
                report.tool_revisions_removed.push(file_name(&path));
            }
        }
    }
    report
}

/// Auto-GC hook: sweep with the env-configured window and log what went.
pub(super) fn run_and_log(paths: &SoldrPaths, log_path: &Path) {
    let Some(max_unused) = max_unused_from_env() else {
        return;
    };
    let report = sweep_stale_versions(
        paths,
        crate::core::MANAGED_SHIM_VERSION,
        SystemTime::now(),
        max_unused,
    );
    if report.version_dirs_removed.is_empty() && report.tool_revisions_removed.is_empty() {
        return;
    }
    let _ = super::auto::append_auto_gc_log_line(
        log_path,
        &format!(
            "auto-gc tier=0 stale_version_dirs={:?} stale_tool_revisions={:?}",
            report.version_dirs_removed, report.tool_revisions_removed
        ),
    );
}

/// `v0.9.46` or dev `v0.9.46-<64 hex>` -> `0.9.46`.
pub(super) fn parse_version_dir(name: &str) -> Option<semver::Version> {
    let rest = name.strip_prefix('v')?;
    let base = match rest.rsplit_once('-') {
        Some((base, hash)) if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            base
        }
        _ => rest,
    };
    let version = semver::Version::parse(base).ok()?;
    version.pre.is_empty().then_some(version)
}

/// `llvm-21.1.5-r2` -> `llvm`; `cargo-nextest-0.9.140` -> `cargo-nextest`.
/// Unversioned dirs and entries owned by other GC passes yield `None`.
pub(super) fn tool_family(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    let split = (1..bytes.len()).find(|&i| bytes[i - 1] == b'-' && bytes[i].is_ascii_digit())?;
    let family = &name[..split - 1];
    if family.is_empty()
        || family == "rustup"
        || family.starts_with("dylint")
        || family.starts_with("cargo-dylint")
    {
        return None;
    }
    Some(family.to_string())
}

/// `llvm-21.1.5-r2` -> (`llvm-21.1.5`, 2); `llvm-21.1.5` -> (`llvm-21.1.5`, 0).
pub(super) fn split_revision(name: &str) -> (&str, u32) {
    if let Some((release, rev)) = name.rsplit_once("-r") {
        if let Ok(n) = rev.parse::<u32>() {
            return (release, n);
        }
    }
    (name, 0)
}

fn list_dirs(root: &Path) -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Some((e.path(), e.file_name().into_string().ok()?)))
        .collect()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_stale(path: &Path, now: SystemTime, max_unused: Duration) -> bool {
    older_than(last_used(path), now, max_unused)
}

fn older_than(used: SystemTime, now: SystemTime, max_unused: Duration) -> bool {
    now.duration_since(used).is_ok_and(|age| age > max_unused)
}

fn last_used(path: &Path) -> SystemTime {
    fn stamp(meta: &std::fs::Metadata) -> SystemTime {
        let m = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let a = meta.accessed().unwrap_or(SystemTime::UNIX_EPOCH);
        m.max(a)
    }
    fn walk(path: &Path, depth: u8, best: &mut SystemTime) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            *best = (*best).max(stamp(&meta));
            if depth > 1 && meta.is_dir() {
                walk(&entry.path(), depth - 1, best);
            }
        }
    }
    let mut best = std::fs::metadata(path)
        .map(|m| stamp(&m))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    walk(path, 2, &mut best);
    best
}

#[cfg(test)]
#[path = "stale_versions_tests.rs"]
mod tests;
