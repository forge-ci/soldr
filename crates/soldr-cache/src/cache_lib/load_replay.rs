/// Per-phase driver-thread wall-clock collected during a profiled load.
/// Microsecond precision so `format_profile_line` can emit ms with no
/// loss at the conversion boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExtractPhaseTimings {
    pub zstd_decode_us: u64,
    pub tar_parse_us: u64,
    pub extract_total_us: u64,
}

/// Per-load profiling state collected when `LoadOptions::profile_extract`
/// is on (#575). Each worker writes into its own slot — no contention.
/// `emit_to_stderr` formats the summary line at the end of `load()`.
struct ExtractProfile {
    /// Per-worker microsecond latencies for each successful Regular
    /// extraction. Sized once at construction; never resized later.
    per_worker_latencies: Vec<Mutex<Vec<u64>>>,
}

impl ExtractProfile {
    fn new(n_workers: usize) -> Self {
        let mut per_worker_latencies = Vec::with_capacity(n_workers);
        for _ in 0..n_workers {
            per_worker_latencies.push(Mutex::new(Vec::new()));
        }
        ExtractProfile {
            per_worker_latencies,
        }
    }

    fn record(&self, worker_idx: usize, latency_us: u64) {
        // Defensive: rayon promises a stable index in `[0, n_workers)`,
        // but a future re-architecture might break that — fail soft.
        if let Some(slot) = self.per_worker_latencies.get(worker_idx) {
            slot.lock().push(latency_us);
        }
    }

    fn emit_to_stderr(&self, phases: ExtractPhaseTimings, files: u64) {
        let mut all_us: Vec<u64> = Vec::new();
        let mut per_worker_counts = Vec::with_capacity(self.per_worker_latencies.len());
        for slot in &self.per_worker_latencies {
            let v = slot.lock();
            per_worker_counts.push(v.len());
            all_us.extend_from_slice(&v);
        }
        eprintln!(
            "{}",
            format_profile_line(phases, &per_worker_counts, &all_us, files)
        );
    }
}

/// Render the documented `soldr load: profile:` line shape (#575). Exposed
/// at module scope so unit tests can exercise the format independent of a
/// live extract.
///
/// Shape matches the spec in zackees/soldr#575:
///
/// ```text
/// soldr load: profile: zstd_decode=4120ms tar_parse=890ms extract_total=10510ms
///   workers={0:n=12058, 1:n=12090, 2:n=12053, 3:n=12030}
///   per_file_p50_us=180 p95_us=450 p99_us=1200 cache_files=48231
/// ```
///
/// Driven by `extract_total_us` instead of summing — that's the only
/// wall-clock number that includes the workers-drain tail, which is the
/// number tuning anyone actually cares about. Worker indices are 0-based
/// to match rayon's convention.
pub fn format_profile_line(
    phases: ExtractPhaseTimings,
    per_worker_counts: &[usize],
    per_file_latencies_us: &[u64],
    files: u64,
) -> String {
    let mut sorted: Vec<u64> = per_file_latencies_us.to_vec();
    sorted.sort_unstable();
    let pct = |p: f64| -> u64 {
        if sorted.is_empty() {
            return 0;
        }
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    };
    let workers_summary: String = per_worker_counts
        .iter()
        .enumerate()
        .map(|(i, n)| format!("{}:n={}", i, n))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "soldr load: profile: zstd_decode={zstd}ms tar_parse={tar}ms extract_total={total}ms workers={{{workers}}} per_file_p50_us={p50} p95_us={p95} p99_us={p99} cache_files={files}",
        zstd = phases.zstd_decode_us / 1000,
        tar = phases.tar_parse_us / 1000,
        total = phases.extract_total_us / 1000,
        workers = workers_summary,
        p50 = pct(0.50),
        p95 = pct(0.95),
        p99 = pct(0.99),
        files = files,
    )
}

/// RAII guard for `--auto-defender-exclude` (#596).
///
/// When constructed via [`defender_exclusion_guard_for`] on Windows with
/// an admin token and PowerShell available, the cache directory is added
/// to Defender's exclusion list via `Add-MpPreference`. On drop the
/// matching `Remove-MpPreference` runs best-effort. Outside that happy
/// path the guard is a no-op — we never trigger a UAC prompt.
#[derive(Default)]
struct DefenderExclusionGuard {
    tracked: Option<(PathBuf, String)>,
}

impl Drop for DefenderExclusionGuard {
    fn drop(&mut self) {
        let Some((powershell, path)) = self.tracked.take() else {
            return;
        };
        let plan = vec![crate::defender::PathAction {
            path: path.clone(),
            action: crate::defender::ExclusionAction::Remove,
            scope: "soldr-load".into(),
            status: crate::defender::ActionStatus::Planned,
            detail: None,
        }];
        // Why: we just added this path on guard creation, so always
        // attempt removal — don't re-query Defender (which could return
        // stale state under heavy load or contention) and pass the
        // tracked path so apply_exclusions issues `Remove-MpPreference`
        // unconditionally instead of short-circuiting to Skipped.
        let existing = vec![path.clone()];
        let outcomes = crate::defender::apply_exclusions(&powershell, &plan, &existing);
        let status = outcomes
            .first()
            .map(|a| format!("{:?}", a.status))
            .unwrap_or_else(|| "no-op".into());
        eprintln!("soldr load: defender exclusion removed for {path} ({status})");
    }
}

fn defender_exclusion_guard_for(cache_dir: &Path) -> DefenderExclusionGuard {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        let _ = cache_dir;
        return DefenderExclusionGuard::default();
    }
    {
        let Some(powershell) = crate::defender::find_powershell() else {
            eprintln!(
                "soldr load: --auto-defender-exclude requested but no PowerShell on PATH; skipping"
            );
            return DefenderExclusionGuard::default();
        };
        if !crate::defender::is_admin() {
            eprintln!(
                "soldr load: --auto-defender-exclude requested but current process is not elevated; skipping (no UAC prompt)"
            );
            return DefenderExclusionGuard::default();
        }
        let path_str = cache_dir.display().to_string();
        let existing = crate::defender::current_exclusion_list(&powershell);
        let plan = vec![crate::defender::PathAction {
            path: path_str.clone(),
            action: crate::defender::ExclusionAction::Add,
            scope: "soldr-load".into(),
            status: crate::defender::ActionStatus::Planned,
            detail: None,
        }];
        let outcomes = crate::defender::apply_exclusions(&powershell, &plan, &existing);
        let outcome = outcomes.into_iter().next();
        let Some(outcome) = outcome else {
            return DefenderExclusionGuard::default();
        };
        match outcome.status {
            crate::defender::ActionStatus::Applied => {
                eprintln!("soldr load: defender exclusion added for {path_str}");
                DefenderExclusionGuard {
                    tracked: Some((powershell, path_str)),
                }
            }
            crate::defender::ActionStatus::AlreadyApplied => {
                eprintln!(
                    "soldr load: {path_str} already on Defender exclusion list; nothing to do"
                );
                DefenderExclusionGuard::default()
            }
            other => {
                let detail = outcome.detail.unwrap_or_default();
                eprintln!(
                    "soldr load: defender exclusion for {path_str} not applied ({other:?}{}{})",
                    if detail.is_empty() { "" } else { ": " },
                    detail
                );
                DefenderExclusionGuard::default()
            }
        }
    }
}

// soldr#3289 (zccache#1595): the per-file SOURCE mtime replay decision
// (size check, content hash, set-times) is no longer reimplemented here.
// It delegates to `zccache::fingerprint::mtime_replay::replay_one`, which
// is re-exported unconditionally by the amalgamated `zccache` crate (no
// zccache feature required — see the Cargo.toml comment at the
// `zccache` dependency). Preserved safety properties, carried over from
// the deleted soldr implementation:
//
// * size is checked BEFORE hashing, so a resized file is never re-hashed;
// * a hash mismatch, an unsafe/unsafe-shaped path, a missing/non-regular
//   file, or a failed mtime stamp all leave the file's CURRENT
//   (fresh) mtime untouched — never `Applied` — biasing every failure
//   mode toward an extra rebuild rather than a wrong one;
// * only mtime is stamped, through `zccache::core::mtime` (#3462), the one
//   owner of materialized-output mtime policy; nothing in soldr reads
//   these files' atime;
// * a source path that is a symlink (or escapes the workspace) is
//   reported `Missing` and never stamped through, which is strictly more
//   conservative than the deleted implementation. #1548 in-workspace
//   symlinked sources still get the right effective mtime because their
//   target is a regular file with its own manifest entry.
//
// Archive/manifest format is unchanged: soldr's protobuf `SourceFile`
// (ms mtime, raw 32-byte blake3) is adapted to zccache's `MtimeEntry`
// (ns mtime, lowercase hex blake3) at this boundary, so every existing
// `.tar.zst` in users' caches still hydrates. zccache's `replay_one` is
// always called fully qualified so it cannot collide with soldr's own
// `replay_one` adapter below.

/// Adapt a soldr [`SourceFile`] manifest entry into the [`MtimeEntry`]
/// shape zccache's `replay_one` expects.
///
/// Prefer the exact nanosecond field. Older manifests fall back to the
/// legacy millisecond field, saturating multiplication instead of overflowing.
/// Preserve the historical clamp of negative timestamps to the Unix epoch.
///
/// `blake3` converts the stored 32 raw bytes to zccache's lowercase-hex
/// form via `ContentHash::to_hex()`. A malformed/short stored hash (never
/// produced by `save`, but not trusted here) becomes `""`, which can
/// never match a real hex digest — `replay_one`'s normal decision order
/// then yields `Missing` / `SizeMismatch` / `Modified` depending on what
/// else differs, but NEVER `Applied`: errors bias toward leaving the
/// fresh (post-checkout) mtime in place, exactly like every other
/// failure mode.
fn source_file_to_mtime_entry(entry: &SourceFile) -> MtimeEntry {
    let mtime_ns = entry
        .mtime_ns
        .unwrap_or_else(|| entry.mtime_ms.saturating_mul(1_000_000))
        .max(0);
    let blake3 = <[u8; 32]>::try_from(entry.blake3.as_slice())
        .map(|bytes| zccache::hash::ContentHash::from_bytes(bytes).to_hex())
        .unwrap_or_default();
    MtimeEntry {
        path: entry.path.clone(),
        size: entry.size,
        mtime_ns,
        blake3,
    }
}

/// Delegates the per-file SOURCE mtime replay decision to
/// `zccache::fingerprint::mtime_replay::replay_one` (soldr#3289,
/// zccache#1595). See [`source_file_to_mtime_entry`] for the adapter and
/// the module-level comment above for the preserved safety properties.
fn replay_one(workspace: &Path, entry: &SourceFile) -> ReplayOutcome {
    let mtime_entry = source_file_to_mtime_entry(entry);
    let outcome = zccache::fingerprint::mtime_replay::replay_one(workspace, &mtime_entry);
    if outcome == ReplayOutcome::Applied {
        // A successful setter can still round on a lower precision filesystem.
        // Report that limitation instead of promising exact Cargo freshness.
        let path = workspace.join(&entry.path);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                if let Some(warning) = source_mtime_precision_warning(
                    &entry.path, mtime_entry.mtime_ns, mtime_ns(&meta),
                ) {
                    eprintln!("{warning}");
                }
            }
            Err(error) => eprintln!(
                "soldr hydrate: could not verify restored source mtime for {}: {error}; Cargo may rebuild",
                path.display()
            ),
        }
    }
    outcome
}

fn source_mtime_precision_warning(path: &str, expected: i64, actual: i64) -> Option<String> {
    (expected != actual).then(|| format!(
        "soldr hydrate: filesystem timestamp precision limitation for {path}: requested {expected} ns, read back {actual} ns; Cargo may rebuild"
    ))
}

/// Normalize `workspace` before it is joined against manifest entries'
/// POSIX-relative paths by `replay_one`.
///
/// The deleted soldr implementation converted each entry's `/` to
/// `std::path::MAIN_SEPARATOR` itself before joining; `zccache`'s
/// `replay_one` instead joins the POSIX string as-is
/// (`workspace.join(&entry.path)`), which Win32 resolves correctly for
/// an ordinary `C:\...` root (its path parser treats `/` and `\`
/// interchangeably) but NOT under a `\\?\` verbatim root, where `/` is
/// taken as a literal filename character instead of a separator. Since
/// `std::fs::canonicalize` — used upstream when a workspace path is
/// resolved — can hand back a verbatim path on Windows, the verbatim
/// drive prefix is stripped once per load here so POSIX-relative
/// manifest paths keep resolving on restore. A verbatim UNC root
/// (`\\?\UNC\server\share`) is rewritten to its plain `\\server\share`
/// form for the same reason. Pure string manipulation: no
/// `#[cfg(windows)]`, matching the `ban_platform_cfg_outside_boundary`
/// dylint (#2493).
fn replay_workspace_root(workspace: &Path) -> PathBuf {
    let Some(raw) = workspace.to_str() else {
        return workspace.to_path_buf();
    };
    if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{unc}"));
    }
    PathBuf::from(crate::cache_lib::target_registry::strip_verbatim_prefix(raw))
}

fn apply_cache_tombstones(cache_dir: &Path, manifest: &Manifest) -> Result<()> {
    if manifest.deleted_cache_paths.is_empty() {
        return Ok(());
    }
    let restored_paths: BTreeSet<&str> = manifest
        .cache_files
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    for path in &manifest.deleted_cache_paths {
        if restored_paths.contains(path.as_str()) {
            continue;
        }
        let rel = manifest_rel_to_path(path)?;
        if archive_always_excludes_cache_path(&rel) {
            continue;
        }
        let dest = cache_dir.join(rel);
        // symlink_metadata (#1548): a tombstoned SYMLINK must remove the
        // link itself. Following `metadata` here would misclassify a
        // link-to-dir as a directory and try remove_dir_all through it.
        match std::fs::symlink_metadata(&dest) {
            Ok(meta) if meta.file_type().is_symlink() => {
                remove_symlink(&dest).map_err(|e| io(&dest, e))?
            }
            Ok(meta) if meta.is_dir() => {
                std::fs::remove_dir_all(&dest).map_err(|e| io(&dest, e))?
            }
            Ok(_) => std::fs::remove_file(&dest).map_err(|e| io(&dest, e))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(io(&dest, err)),
        }
    }
    Ok(())
}

/// Remove a symlink itself (never its target). On Windows a directory
/// symlink must be removed with `remove_dir`; try file-removal first and
/// fall back so both flavors are covered.
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    crate::platform::fs::links::remove(path)
}

/// Recreate manifest symlink entries below `cache_dir` (#1548). Returns
/// `(restored, skipped)`. Each entry is re-validated lexically against
/// the restore root; entries that fail validation, collide with a real
/// directory, or whose creation fails (e.g. Windows without the symlink
/// privilege) are skipped LOUDLY via stderr — the restore itself never
/// hard-fails on a symlink, missing links merely force a rebuild.
fn restore_cache_symlinks(cache_dir: &Path, entries: &[SymlinkEntry]) -> (u64, u64) {
    let mut restored = 0u64;
    let mut skipped = 0u64;
    for entry in entries {
        if manifest_path_is_daemon_runtime(&entry.path) {
            skipped += 1;
            continue;
        }
        match restore_one_symlink(cache_dir, entry) {
            Ok(()) => restored += 1,
            Err(reason) => {
                eprintln!(
                    "soldr load: refusing to restore symlink {} -> {} ({reason})",
                    entry.path, entry.target
                );
                skipped += 1;
            }
        }
    }
    (restored, skipped)
}

fn restore_one_symlink(cache_dir: &Path, entry: &SymlinkEntry) -> std::result::Result<(), String> {
    let rel = manifest_rel_to_path(&entry.path)
        .map_err(|_| "invalid link path in manifest".to_string())?;
    if resolve_symlink_target_in_root(&rel, &entry.target).is_none() {
        return Err("absolute or root-escaping link target".to_string());
    }
    let dest = cache_dir.join(&rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create parent dirs: {e}"))?;
    }
    match std::fs::symlink_metadata(&dest) {
        Ok(meta) if meta.file_type().is_symlink() => {
            remove_symlink(&dest).map_err(|e| format!("replace existing link: {e}"))?;
        }
        Ok(meta) if meta.is_dir() => {
            // Conservative: never delete a real directory tree to make
            // room for a link. Loud skip; the stale dir stays visible.
            return Err("a real directory occupies the link path".to_string());
        }
        Ok(_) => {
            std::fs::remove_file(&dest).map_err(|e| format!("replace existing file: {e}"))?;
        }
        Err(_) => {}
    }
    create_symlink_at(&entry.target, &dest, entry.is_dir).map_err(|e| format!("create link: {e}"))
}

/// Platform symlink creation. `target` is the manifest's forward-slashed
/// relative string; converted to the native separator on Windows.
fn create_symlink_at(target: &str, dest: &Path, is_dir: bool) -> std::io::Result<()> {
    crate::platform::fs::links::create(target, dest, is_dir)
}

/// Per-manifest-entry slot tracking whether the extract workers already
/// applied the nanosecond mtime for this path (#1541).
struct CacheMtimeSlot {
    mtime_ns: i64,
    size: u64,
    applied: bool,
}

fn build_cache_mtime_index(manifest: &Manifest) -> HashMap<String, CacheMtimeSlot> {
    manifest
        .cache_files
        .iter()
        .filter(|entry| !manifest_path_is_daemon_runtime(&entry.path))
        .map(|entry| {
            (
                entry.path.clone(),
                CacheMtimeSlot {
                    mtime_ns: entry.mtime_ns,
                    size: entry.size,
                    applied: false,
                },
            )
        })
        .collect()
}

/// Replay manifest mtimes for cache files whose payload was NOT carried
/// by the tar stream (delta metadata-only updates, or archives whose
/// manifest arrived after their cache entries). Entries already handled
/// by an extract worker are skipped; the remainder runs in parallel on
/// the load's rayon pool instead of the historical serial stat+set loop
/// over every manifest entry (#1541).
fn replay_pending_cache_file_mtimes(
    pool: &rayon::ThreadPool,
    cache_dir: &Path,
    entries: &[CacheFile],
    index: &HashMap<String, CacheMtimeSlot>,
) -> Result<()> {
    let pending: Vec<&CacheFile> = entries
        .iter()
        .filter(|entry| !manifest_path_is_daemon_runtime(&entry.path))
        .filter(|entry| {
            index
                .get(entry.path.as_str())
                .is_none_or(|slot| !slot.applied)
        })
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    pool.install(|| {
        pending
            .par_iter()
            .try_for_each(|entry| replay_cache_file_mtime(cache_dir, entry))
    })
}

fn replay_cache_file_mtime(cache_dir: &Path, entry: &CacheFile) -> Result<()> {
    let rel = manifest_rel_to_path(&entry.path)?;
    let abs = cache_dir.join(rel);
    let Ok(meta) = std::fs::metadata(&abs) else {
        return Ok(());
    };
    if !meta.is_file() || meta.len() != entry.size {
        return Ok(());
    }
    let mtime = ns_to_systime(entry.mtime_ns);
    let t = zccache::core::mtime::FileTime::from_system_time(mtime);
    zccache::core::mtime::stamp_mtime(&abs, t).map_err(|e| io(&abs, e))
}

// ---------- thread-pool helpers ----------

/// Read the [`LOAD_WORKERS_ENV`] override. Returns `None` when unset,
/// empty, or unparseable as a positive integer. Caller decides how to
/// combine this with the explicit `--threads` knob and rayon's default.
/// (#575)
fn load_worker_count_override() -> Option<usize> {
    let raw = std::env::var(LOAD_WORKERS_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<usize>().ok().filter(|&n| n > 0)
}

fn build_pool(threads: Option<usize>) -> Result<rayon::ThreadPool> {
    let mut builder = rayon::ThreadPoolBuilder::new();
    if let Some(n) = threads {
        builder = builder.num_threads(n);
    }
    builder
        .thread_name(|i| format!("soldr-save-{i}"))
        .build()
        .map_err(|e| SaveLoadError::BareIo(std::io::Error::other(e.to_string())))
}

fn num_cpus_for(threads: Option<usize>) -> u32 {
    threads.map(|n| n as u32).unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1)
    })
}
