//! The cook section of `soldr doctor` (#590), split out of `doctor.rs` to keep
//! it under the 1,000-line production ceiling (soldr#3092).

use crate::core::SoldrPaths;
use serde::Serialize;

/// Cook-cache aggregate counts surfaced in `soldr doctor`. (#590)
///
/// The first three fields mirror [`crate::daemon::protocol::CookStats`]
/// and only populate when the daemon is reachable; the last two come
/// from an on-disk scan of `<soldr_paths.cache>/cook/` and always
/// populate when the directory is reachable.
#[derive(Serialize, Clone)]
pub(crate) struct DoctorCookStats {
    /// Rows in the daemon's `cook_index_v1` redb table. `None` when
    /// the daemon isn't running (the on-disk scan still appears).
    entries: Option<u64>,
    /// Sum of `size_bytes` across the daemon's index rows. `None`
    /// when the daemon isn't running.
    total_bytes: Option<u64>,
    /// CookLookup hits served by the current daemon since last
    /// startup. Resets across restarts. `None` when no daemon.
    hits_this_session: Option<u64>,
    /// Number of `<sha256>.tar.zst` files under the cook cache dir
    /// (excludes `.tmp/` and `.quarantine` files).
    cache_dir_artifacts: u64,
    /// Sum of file sizes counted by [`Self::cache_dir_artifacts`].
    cache_dir_bytes: u64,
    /// Absolute path to the on-disk cook cache dir.
    cache_dir: String,
}

/// Collect cook-cache stats for the doctor surface (#590).
///
/// Combines two cheap probes:
/// 1. Query the running daemon via [`crate::daemon::client::status`]
///    for `CookStats` (entries, total_bytes, hits_this_session). When
///    the daemon isn't running the three fields fall through as
///    `None` and we still produce the on-disk view.
/// 2. Walk `<paths.cache>/cook/` and count `*.tar.zst` files,
///    skipping `.tmp/` and `.quarantine`. Catches drift between the
///    redb index and the artifacts on disk.
///
/// Returns `None` only when `SoldrPaths::new` fails (which would
/// already be visible elsewhere in the doctor output). Any other
/// error is swallowed — doctor is diagnostic and best-effort.
pub(crate) fn collect_cook_stats() -> Option<DoctorCookStats> {
    let paths = SoldrPaths::new().ok()?;
    let cook_dir = crate::cache_lib::cook_archive::cook_cache_dir(&paths);
    let sock = crate::daemon::client::default_sock_path(&paths);

    let from_daemon = crate::daemon::client::status(&sock).ok();
    let cook_stats = from_daemon.as_ref().map(|s| s.cook_stats_or_zero());

    // Scan the cook dir for `<sha256_hex>.tar.zst` files. Skip
    // `.tmp/` (in-flight saves) and `.quarantine` (corrupt artifacts
    // already flagged). Any I/O error → counts stay zero.
    let mut artifacts = 0u64;
    let mut bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(&cook_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.ends_with(".quarantine") || name.starts_with('.') {
                continue;
            }
            if !name.ends_with(".tar.zst") {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                artifacts += 1;
                bytes += meta.len();
            }
        }
    }

    Some(DoctorCookStats {
        entries: cook_stats.as_ref().map(|s| s.entries),
        total_bytes: cook_stats.as_ref().map(|s| s.total_bytes),
        hits_this_session: cook_stats.as_ref().map(|s| s.hits_this_session),
        cache_dir_artifacts: artifacts,
        cache_dir_bytes: bytes,
        cache_dir: cook_dir.display().to_string(),
    })
}

/// Format a byte count for the doctor human output. Matches the
/// existing pattern of `print_managed_zccache_human` etc. — pick the
/// largest binary unit ≤ the value, two decimals.
pub(crate) fn fmt_bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    let n = n as f64;
    if n < KIB {
        return format!("{n} B");
    }
    let kib = n / KIB;
    if kib < KIB {
        return format!("{:.2} KiB", kib);
    }
    let mib = kib / KIB;
    if mib < KIB {
        return format!("{:.2} MiB", mib);
    }
    let gib = mib / KIB;
    format!("{:.2} GiB", gib)
}

/// Render the cook section to stdout. (#590)
pub(crate) fn print_cook_section(cook: &DoctorCookStats) {
    println!("cook:");
    if let (Some(entries), Some(total)) = (cook.entries, cook.total_bytes) {
        println!("  entries:           {}", entries);
        println!("  total bytes:       {}", fmt_bytes(total));
        let hits = cook.hits_this_session.unwrap_or(0);
        println!("  hits this session: {}", hits);
    } else {
        println!("  (daemon not running — index counts unavailable)");
    }
    println!(
        "  cache dir:         {}  ({} artifacts, {} on disk)",
        cook.cache_dir,
        cook.cache_dir_artifacts,
        fmt_bytes(cook.cache_dir_bytes),
    );
}
