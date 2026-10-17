#[derive(Debug, Clone)]
pub struct LoadOptions<'a> {
    pub archive: &'a Path,
    /// Destination cache directory. `None` is only permitted when
    /// `mtimes_only` is `true`; in that mode any `cache/...` tar entry
    /// is treated as a hard error since the archive should not have
    /// contained one.
    pub cache_dir: Option<&'a Path>,
    /// Workspace whose source-file mtimes should be replayed. `None`
    /// skips the mtime-replay step (cache-only load). Required when
    /// `mtimes_only` is `true`.
    pub workspace: Option<&'a Path>,
    pub threads: Option<usize>,
    /// Treat the archive as a manifest-only snapshot. Skip the cache
    /// extraction step (the archive should not contain any `cache/...`
    /// entries; one is an error). Requires `workspace` to be `Some` —
    /// otherwise the load is a no-op.
    pub mtimes_only: bool,
    /// Emit a per-phase profile line to stderr after the load finishes:
    /// zstd decode time, tar parse + dispatch time, total extract time,
    /// per-worker job count, and per-file extract latency percentiles.
    /// Useful for tuning the parallel-extract worker count (#575).
    pub profile_extract: bool,
    /// On Windows, when the current process is admin, briefly add the
    /// cache directory to the Defender exclusion list for the duration
    /// of the load. No-op on non-Windows or when not admin — never
    /// triggers a UAC prompt. Default off; setup-soldr passes this on
    /// Windows runners. (#575)
    pub auto_defender_exclude: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LoadReport {
    pub cache_files_restored: u64,
    pub source_files_in_manifest: u64,
    pub mtimes_applied: u64,
    pub mtimes_skipped_missing: u64,
    pub mtimes_skipped_size_mismatch: u64,
    pub mtimes_skipped_modified: u64,
    pub elapsed_ms: u64,
    /// Manifest symlinks recreated inside the restore root (#1548).
    pub cache_symlinks_restored: u64,
    /// Manifest symlinks NOT recreated: invalid/escaping target on
    /// re-validation, a real directory in the way, or symlink creation
    /// failed (e.g. missing Windows privilege). Each skip warns on
    /// stderr — never silent (#1548).
    pub cache_symlinks_skipped: u64,
}

/// Validate load inputs:
/// * When `mtimes_only`, `workspace` MUST be `Some` and `cache_dir`
///   MUST be `None`. Mixing the two is a CLI mistake.
/// * Otherwise `cache_dir` MUST be `Some` (the load has to know where
///   to extract cache entries).
fn validate_load_inputs(opts: &LoadOptions<'_>) -> Result<()> {
    if opts.mtimes_only {
        if opts.workspace.is_none() {
            return Err(SaveLoadError::BareIo(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "soldr load --mtimes-only requires a --workspace to replay into",
            )));
        }
        if opts.cache_dir.is_some() {
            return Err(SaveLoadError::BareIo(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "soldr load --mtimes-only must NOT be combined with --cache-dir",
            )));
        }
    } else if opts.cache_dir.is_none() {
        return Err(SaveLoadError::BareIo(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "soldr load requires either --cache-dir or --mtimes-only",
        )));
    }
    Ok(())
}

/// Decompress + restore an archive produced by [`save`].
///
/// The implementation pipelines THREE operations on the existing rayon pool:
///   1. **Stream-decompress + tar-header parsing** in the driver thread —
///      zstd's read-side decoder is single-threaded by design.
///   2. **Per-file extraction** (CreateFile + write + set_mtime) dispatched
///      via a bounded `sync_channel` to N rayon workers. On Windows this
///      pipelines Defender real-time-scan callbacks across cores instead of
///      serializing them on a single thread — same insight as zackees/zccache#189
///      for the analogous walk problem. (zackees/soldr#575)
///   3. **Mtime replay onto workspace sources** runs concurrently with (2)
///      on its own rayon task once the manifest is parsed.
///
/// Per-file mtime preservation: each worker restores the manifest's
/// nanosecond mtime right after the write completes (#1541) — the tar
/// header's second-truncated mtime is only a fallback for entries the
/// manifest doesn't cover. Manifest entries whose payload was not in
/// the tar (delta metadata-only updates) get their mtimes replayed in
/// a parallel pass after extraction instead of the historical serial
/// stat+set loop over every manifest entry.
///
/// When [`LoadOptions::mtimes_only`] is `true`, only the manifest entry
/// is consumed; any `cache/...` entry in the archive is rejected as a
/// hard error (the producer should not have included one).
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub fn load(opts: &LoadOptions<'_>) -> Result<LoadReport> {
    validate_load_inputs(opts)?;
    let start = std::time::Instant::now();

    if let Some(dir) = opts.cache_dir {
        std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    }

    let in_file = File::open(opts.archive).map_err(|e| io(opts.archive, e))?;
    let buf = BufReader::with_capacity(16 * 1024 * 1024, in_file);
    let zstd_reader = zstd::stream::read::Decoder::new(buf).map_err(SaveLoadError::Zstd)?;
    let mut tar_reader = tar::Archive::new(zstd_reader);
    // Belt-and-suspenders (soldr#1144): historically we set
    // preserve_mtime(false) here on the theory that our per-worker
    // filetime::set_file_mtime path would handle the restore. That
    // path only runs for the parallel-extract dispatch; if any code
    // path drains via tar's own unpack (e.g. a future refactor, an
    // error-recovery fallback, or a mtimes_only load whose payload
    // grew a cache/ entry) the mtime silently defaults to
    // extraction-wall-clock. Cargo's incremental fingerprint records
    // an artifact's mtime at first compile and treats a later
    // "newer" mtime as evidence of external modification, forcing
    // re-link + re-fingerprint on every hit — the exact 20x/hit
    // slowdown seen in perf-matrix run 28497381630 (medium/cold-
    // tar-untar-warm at 1.22x speedup vs the 3.0x floor). Setting
    // this to `true` makes tar's built-in mtime restore the
    // baseline; per-worker restore stays as the fast path.
    //
    // Permissions still get restored by the worker (see extract_one
    // — it chmods from job.mode_bits after write). tar's
    // preserve_permissions would clobber that on Windows (where
    // Unix mode bits are meaningless).
    tar_reader.set_preserve_mtime(true);
    tar_reader.set_preserve_permissions(false);

    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut manifest_decoded: Option<Manifest> = None;
    // #1541: manifest-driven cache-file mtimes. Keyed by the manifest's
    // POSIX-relative path; populated once the manifest entry is parsed
    // (archives produced by `save` always put it first). Slots flip to
    // `applied` when the corresponding tar entry is dispatched with the
    // nanosecond mtime attached, so the post-extract replay pass only
    // has to touch manifest entries whose payload was NOT in the tar
    // (delta metadata-only updates).
    let mut cache_mtime_index: HashMap<String, CacheMtimeSlot> = HashMap::new();
    // Env override (LOAD_WORKERS_ENV / SOLDR_LOAD_WORKERS) wins over the
    // caller-supplied --threads; otherwise the explicit --threads wins;
    // otherwise rayon picks its default (num_cpus). The pool we build here
    // is also what the per-file extract workers run on, so this single
    // knob governs all load-time parallelism.
    let effective_threads = load_worker_count_override().or(opts.threads);
    let pool = build_pool(effective_threads)?;
    // Holds the mtime-replay job once we've parsed the manifest and
    // dispatched the work onto rayon. We poll on it after the tar
    // stream is fully drained.
    let mut replay_handle: Option<std::sync::mpsc::Receiver<Vec<ReplayOutcome>>> = None;

    // #575 parallel extraction infrastructure. Spun up lazily on the first
    // cache entry so mtimes_only loads pay zero overhead.
    let mut extract_dispatch: Option<ExtractDispatch> = None;
    let extract_error: Arc<Mutex<Option<SaveLoadError>>> = Arc::new(Mutex::new(None));
    let cache_files_counter = Arc::new(AtomicU64::new(0));
    let profile = if opts.profile_extract {
        Some(Arc::new(ExtractProfile::new(pool.current_num_threads())))
    } else {
        None
    };
    let driver_loop_start = std::time::Instant::now();
    // Cumulative microseconds the driver spent inside `entry.read_to_end`
    // for cache-file bodies. That call drives the zstd decoder, so this
    // is the closest cheap approximation of "zstd_decode" wall-clock the
    // streaming API gives us. tar_parse_us is the loop's remaining time.
    let mut zstd_decode_us: u64 = 0;

    // #575/#596 Defender auto-exclusion (Windows + admin only — never
    // UAC-prompts). The guard removes the exclusion on drop.
    let _defender_guard = if opts.auto_defender_exclude {
        opts.cache_dir
            .map(defender_exclusion_guard_for)
            .unwrap_or_default()
    } else {
        DefenderExclusionGuard::default()
    };

    for entry in tar_reader.entries().map_err(SaveLoadError::BareIo)? {
        let mut entry = entry.map_err(SaveLoadError::BareIo)?;
        let path = entry.path().map_err(SaveLoadError::BareIo)?.into_owned();

        if path.as_os_str() == MANIFEST_NAME {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(SaveLoadError::BareIo)?;
            let manifest: Manifest = prost::Message::decode(&buf[..])?;
            if let Some(cache_dir) = opts.cache_dir {
                apply_cache_tombstones(cache_dir, &manifest)?;
                cache_mtime_index = build_cache_mtime_index(&manifest);
            }
            manifest_bytes = Some(buf);
            // Kick off the mtime replay NOW so it runs in parallel
            // with the rest of the tar extraction. The cache files
            // and workspace source files live on disjoint trees so
            // their I/O doesn't fight.
            if let Some(ws) = opts.workspace {
                let manifest_for_replay = manifest.clone();
                let ws_owned = replay_workspace_root(ws);
                let (tx, rx) = std::sync::mpsc::channel();
                pool.spawn(move || {
                    let outcomes: Vec<ReplayOutcome> = manifest_for_replay
                        .files
                        .par_iter()
                        .map(|e| replay_one(&ws_owned, e))
                        .collect();
                    let _ = tx.send(outcomes);
                });
                replay_handle = Some(rx);
            }
            manifest_decoded = Some(manifest);
            continue;
        }

        // Expect everything else under `cache/`. In mtimes_only mode
        // there should be no such entries — a producer-side bug if
        // there is one, so reject it loudly.
        let stripped = match path.strip_prefix(CACHE_DIR_NAME) {
            Ok(p) => archive_rel_to_path(p)?,
            Err(_) => {
                return Err(SaveLoadError::BadArchivePath(path.display().to_string()));
            }
        };
        // Compatibility with archives produced before daemon runtime state
        // became reserved: drain but never materialize those entries. A load
        // must not overwrite the live PID/lock/socket namespace of this host.
        if archive_always_excludes_cache_path(&stripped) {
            continue;
        }
        if opts.mtimes_only {
            return Err(SaveLoadError::BadArchivePath(format!(
                "mtimes_only load refuses cache entry: {}",
                path.display()
            )));
        }
        // cache_dir is guaranteed Some by validate_load_inputs when we
        // reach this branch.
        let cache_dir = opts.cache_dir.expect("cache_dir checked at entry");
        let dest = cache_dir.join(&stripped);
        let entry_type = entry.header().entry_type();

        // Directories: create immediately on the driver thread. Cheap; this
        // also guarantees the directory exists by the time workers race to
        // write files inside it (we still call create_dir_all on the parent
        // in the worker as a belt-and-suspenders, since tar doesn't
        // guarantee directory entries precede their contents).
        if entry_type == tar::EntryType::Directory {
            std::fs::create_dir_all(&dest).map_err(|e| io(&dest, e))?;
            continue;
        }

        // Read the body fully into memory + capture mtime, then dispatch to
        // a worker. For the cache-archive use case bodies are typically
        // small (<MiB each) and the bounded channel caps how many are
        // resident at once, so memory usage stays bounded.
        let mut body = Vec::new();
        let body_read_start = opts.profile_extract.then(std::time::Instant::now);
        entry
            .read_to_end(&mut body)
            .map_err(SaveLoadError::BareIo)?;
        if let Some(t0) = body_read_start {
            zstd_decode_us = zstd_decode_us.saturating_add(t0.elapsed().as_micros() as u64);
        }
        let mtime_secs = entry.header().mtime().ok();
        // Capture the tar header's Unix mode so the worker can chmod
        // the file after writing. Without this, executable scripts
        // and binaries (e.g. cargo `build-script-build`) lose +x on
        // restore and fail with EACCES — see #587.
        let mode_bits = entry.header().mode().ok();
        // #1541: prefer the manifest's nanosecond mtime over the tar
        // header's second-truncated one. Guarded by a size match so a
        // payload that diverged from the manifest (e.g. mutated mid-
        // save) falls back to the header mtime, exactly like the old
        // replay pass would have skipped it on its size check.
        let mut mtime_ns: Option<i64> = None;
        if !cache_mtime_index.is_empty() {
            let rel_posix = rel_to_posix(&stripped);
            if let Some(slot) = cache_mtime_index.get_mut(&rel_posix) {
                if slot.size == body.len() as u64 {
                    mtime_ns = Some(slot.mtime_ns);
                    slot.applied = true;
                }
            }
        }

        // Lazy-start the dispatch on first cache entry.
        let dispatch = extract_dispatch.get_or_insert_with(|| {
            ExtractDispatch::start(
                &pool,
                opts.threads,
                Arc::clone(&extract_error),
                Arc::clone(&cache_files_counter),
                profile.as_ref().map(Arc::clone),
            )
        });

        let job = ExtractJob {
            dest,
            entry_type,
            body,
            mtime_secs,
            mtime_ns,
            mode_bits,
        };
        if dispatch.send(job).is_err() {
            // Receivers are gone — there must be a stored error already.
            break;
        }
    }

    let driver_loop_us = driver_loop_start.elapsed().as_micros() as u64;
    let workers_drain_start = std::time::Instant::now();
    // Close the dispatch channel and wait for workers to drain. Any error
    // from a worker is surfaced here (first-error-wins).
    if let Some(dispatch) = extract_dispatch {
        dispatch.finish()?;
    }
    let workers_drain_us = workers_drain_start.elapsed().as_micros() as u64;
    if let Some(err) = extract_error.lock().take() {
        return Err(err);
    }
    let cache_files_restored = cache_files_counter.load(Ordering::Relaxed);
    if let Some(profile) = profile {
        let phases = ExtractPhaseTimings {
            zstd_decode_us,
            // tar_parse_us = driver loop time minus the read_to_end accounting.
            // Saturates to 0 if profile noise (timer jitter, scheduler) pushed
            // accumulated zstd_decode_us slightly past driver_loop_us.
            tar_parse_us: driver_loop_us.saturating_sub(zstd_decode_us),
            extract_total_us: driver_loop_us.saturating_add(workers_drain_us),
        };
        profile.emit_to_stderr(phases, cache_files_restored);
    }

    let manifest = match manifest_decoded {
        Some(manifest) => manifest,
        None => {
            let manifest_bytes = manifest_bytes.ok_or(SaveLoadError::MissingManifest)?;
            prost::Message::decode(&manifest_bytes[..])?
        }
    };

    let mut report = LoadReport {
        cache_files_restored,
        source_files_in_manifest: manifest.files.len() as u64,
        ..LoadReport::default()
    };

    if let Some(cache_dir) = opts.cache_dir {
        // #1548: recreate manifest symlinks AFTER extraction so their
        // targets already exist. Every entry is re-validated against the
        // restore root here — a crafted manifest can never make load
        // create a link that points outside `cache_dir`.
        let (restored, skipped) = restore_cache_symlinks(cache_dir, &manifest.cache_symlinks);
        report.cache_symlinks_restored = restored;
        report.cache_symlinks_skipped = skipped;
        replay_pending_cache_file_mtimes(
            &pool,
            cache_dir,
            &manifest.cache_files,
            &cache_mtime_index,
        )?;
    }

    // If we kicked off the replay early, wait for it. Otherwise (no
    // workspace, or first-run before manifest seen) run it inline
    // here for completeness.
    if let Some(rx) = replay_handle {
        let outcomes = rx.recv_timeout(REPLAY_WORKER_RECV_TIMEOUT).map_err(|err| {
            SaveLoadError::BareIo(std::io::Error::other(format!(
                "replay worker did not finish within {}s: {err}",
                REPLAY_WORKER_RECV_TIMEOUT.as_secs()
            )))
        })?;
        for o in outcomes {
            match o {
                ReplayOutcome::Applied => report.mtimes_applied += 1,
                ReplayOutcome::Missing => report.mtimes_skipped_missing += 1,
                ReplayOutcome::SizeMismatch => report.mtimes_skipped_size_mismatch += 1,
                ReplayOutcome::Modified => report.mtimes_skipped_modified += 1,
            }
        }
    }

    report.elapsed_ms = start.elapsed().as_millis() as u64;
    Ok(report)
}

// ---------------------------------------------------------------------------
// #575 parallel cache-file extraction
// ---------------------------------------------------------------------------

/// In-flight work item dispatched from the tar driver to a rayon worker.
struct ExtractJob {
    dest: PathBuf,
    entry_type: tar::EntryType,
    body: Vec<u8>,
    mtime_secs: Option<u64>,
    /// Nanosecond-precision mtime from the archive manifest (#1541).
    /// When present it wins over `mtime_secs` (the tar header only
    /// carries seconds) and the post-extract manifest replay pass is
    /// skipped for this file — the worker's write is the final word,
    /// eliminating one stat + one utimensat per restored file.
    mtime_ns: Option<i64>,
    /// Unix file mode from the tar header, used to restore the
    /// executable bit on Unix. None when the header lacked a mode.
    /// Ignored on Windows (NTFS uses ACLs, not Unix modes; tar
    /// archives don't carry meaningful NTFS permissions). (#587)
    mode_bits: Option<u32>,
}

/// Bounded-channel + worker-thread bundle that owns the parallel extraction
/// of cache-file entries. Wraps the existing rayon pool — no new thread
/// runtime, no new direct deps. Bounded so the driver pauses if workers
/// can't keep up (caps in-memory body buffer at ~`bound × entry_size`).
struct ExtractDispatch {
    /// `Option` so shutdown can close the channel exactly once, whether it
    /// is reached through [`ExtractDispatch::finish`] or through `Drop`.
    tx: Option<std::sync::mpsc::SyncSender<ExtractJob>>,
    /// Barrier joined when every worker exits; size = num_workers + 1
    /// (workers + the driver caller).
    barrier: Arc<crate::platform::wait::Barrier>,
    /// Guards against waiting on `barrier` twice. A `Barrier` is reusable:
    /// a second wait opens a new generation that only `n_workers + 1`
    /// further arrivals could release, so double-waiting would hang.
    shutdown_done: bool,
}

impl ExtractDispatch {
    fn start(
        pool: &rayon::ThreadPool,
        _threads: Option<usize>,
        err_slot: Arc<Mutex<Option<SaveLoadError>>>,
        counter: Arc<AtomicU64>,
        profile: Option<Arc<ExtractProfile>>,
    ) -> Self {
        // Worker count = pool size. The pool was built by `build_pool` with
        // the user's `--threads` value (or rayon's default), so trusting it
        // here keeps the user's intent intact AND guarantees we never spawn
        // more closures than the pool has threads to run (which would
        // deadlock our barrier if the spawned closures wait for siblings
        // that never get scheduled).
        let n_workers = pool.current_num_threads().max(1);
        // Bounded queue. 64 = trade-off between (1) the driver getting
        // throttled by slow workers vs (2) the resident memory of in-flight
        // bodies. With cache-archive entries averaging <200 KiB, 64 caps
        // memory at ~13 MiB which is negligible.
        let (tx, rx) = sync_channel::<ExtractJob>(64);
        let rx = Arc::new(Mutex::new(rx));
        let barrier = Arc::new(crate::platform::wait::Barrier::new(n_workers + 1));

        for worker_idx in 0..n_workers {
            let rx = Arc::clone(&rx);
            let err_slot = Arc::clone(&err_slot);
            let counter = Arc::clone(&counter);
            let barrier = Arc::clone(&barrier);
            let profile = profile.as_ref().map(Arc::clone);
            pool.spawn(move || {
                loop {
                    let job = {
                        // soldr#3836: bounded on both sides -- a waiter gives
                        // up after one poll interval and loops, and the
                        // holder polls for at most that long, so a slow
                        // driver never trips the lock watchdog.
                        let Ok(guard) = rx.lock_for(EXTRACT_WORKER_RECV_TIMEOUT) else {
                            continue;
                        };
                        match guard.recv_timeout(EXTRACT_WORKER_RECV_TIMEOUT) {
                            Ok(j) => j,
                            // Only a closed channel means "no more work".
                            // A timeout means the driver is merely slow --
                            // a big zstd frame, a stalled disk. Treating it
                            // as disconnect (the previous behaviour) retired
                            // every worker permanently, after which the
                            // SyncSender still accepted up to `bound` jobs
                            // that nobody would ever extract; `finish()`
                            // then returned Ok and `load()` reported success
                            // with files silently absent from the tree.
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    };
                    // If a sibling already recorded an error, drain remaining
                    // jobs without doing more I/O (keeps the driver moving
                    // toward the finish line so we can surface the error
                    // promptly).
                    if err_slot.lock().is_some() {
                        continue;
                    }
                    let is_regular = job.entry_type == tar::EntryType::Regular;
                    let job_start = profile.is_some().then(std::time::Instant::now);
                    if let Err(e) = extract_one(&job) {
                        let mut slot = err_slot.lock();
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                        continue;
                    }
                    if is_regular {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    if let (Some(prof), Some(t0)) = (profile.as_ref(), job_start) {
                        let us = t0.elapsed().as_micros() as u64;
                        prof.record(worker_idx, us);
                    }
                }
                let _ = barrier.wait_for(crate::platform::wait::HELPER_JOIN);
            });
        }

        ExtractDispatch {
            tx: Some(tx),
            barrier,
            shutdown_done: false,
        }
    }

    fn send(
        &self,
        job: ExtractJob,
    ) -> std::result::Result<(), std::sync::mpsc::SendError<ExtractJob>> {
        match self.tx.as_ref() {
            Some(tx) => crate::platform::wait::send_for(tx, job, crate::platform::wait::LONG_CHILD)
                .map_err(|(job, _)| std::sync::mpsc::SendError(job)),
            // Only reachable after shutdown, which the driver never does
            // before its last send. Report it as a send failure rather
            // than panicking so a future refactor degrades loudly but
            // safely.
            None => Err(std::sync::mpsc::SendError(job)),
        }
    }

    /// Close the channel and block until every worker has exited.
    /// Idempotent, and shared with `Drop` so the wait cannot be skipped.
    fn shutdown(&mut self) {
        if self.shutdown_done {
            return;
        }
        self.shutdown_done = true;
        // Dropping the sender is what lets workers observe `Disconnected`
        // and leave their receive loop.
        self.tx = None;
        let _ = self.barrier.wait_for(crate::platform::wait::LONG_CHILD);
    }

    /// Close the channel and block until every worker has exited.
    /// Returns Ok(()) regardless of worker errors — those land in the
    /// shared err_slot the caller passed to `start`.
    fn finish(mut self) -> Result<()> {
        self.shutdown();
        Ok(())
    }
}

/// Sibling path used to stage a restored file before it is renamed into
/// place (#1909).
///
/// Must live in the same directory as `dest`: `rename` is only atomic within
/// a filesystem, and a temp dir can easily be on a different one. The name
/// carries pid + a process-local counter so concurrent extract workers — and
/// concurrent `soldr load` processes sharing a target dir — never collide.
fn staging_path_for(dest: &Path) -> PathBuf {
    static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "entry".to_string());
    dest.with_file_name(format!(
        ".{name}.soldr-tmp-{pid}-{seq}",
        pid = std::process::id()
    ))
}

/// #1909: the driver loop reaches `load()`'s error paths through `?`, which
/// drops the dispatch without calling [`ExtractDispatch::finish`]. Without a
/// `Drop` impl those rayon workers kept running after `load()` returned,
/// still writing into the cache tree the caller was about to use -- and
/// cargo exec'ing a build script while a worker held it open for write is
/// `ETXTBSY` ("Text file busy"), the failure this fixes.
///
/// They also leaked: workers park on a barrier sized `n_workers + 1` whose
/// final party (the driver) had already returned, so nothing ever released
/// them. `rayon::ThreadPool::drop` does not join outstanding `spawn`ed
/// closures, so dropping the pool did not rescue this either.
impl Drop for ExtractDispatch {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Test seam for soldr#3098: lets a test pause an extraction worker while
/// the staged file is open for writing. A no-op in production builds.
mod extract_test_hooks {
    #[cfg(test)]
    pub(crate) type Hook = Box<dyn Fn(&std::path::Path) + Send + Sync + 'static>;
    #[cfg(test)]
    pub(crate) static HOOK: crate::platform::sync::Mutex<Option<Hook>> = crate::platform::sync::Mutex::new(None);

    #[cfg(test)]
    pub(crate) fn staged_file_open_for_write(staged: &std::path::Path) {
        // The hook runs while the registry lock is held; hooks are
        // test-owned, short, and only ever installed by one test.
        if let Some(hook) = HOOK.lock().as_ref() {
            hook(staged);
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    pub(crate) fn staged_file_open_for_write(_staged: &std::path::Path) {}
}

/// Worker-side per-entry extraction. Splits Regular vs Directory handling
/// (Directories are created by the driver thread, so we only see Regular
/// + the long tail of tar entry types here).
fn extract_one(job: &ExtractJob) -> Result<()> {
    if let Some(parent) = job.dest.parent() {
        // Belt-and-suspenders: tar doesn't guarantee directory entries
        // precede their contents, so workers that race ahead of a sibling
        // directory entry still get their parent dir created.
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    match job.entry_type {
        tar::EntryType::Regular => {
            // #1909: write to a sibling temp path and rename into place,
            // rather than writing `dest` directly, so `dest` never exists in
            // a partially-written state.
            //
            // soldr#3098: that alone does NOT close the ETXTBSY race, though
            // an earlier version of this comment claimed it did. `execve`
            // fails with ETXTBSY if *any* process holds the file's inode open
            // for writing. soldr spawns children throughout a build (cargo,
            // the broker, the gc sweeper, installers); a child forked while
            // our write descriptor is open inherits it and keeps it until
            // its own exec (`O_CLOEXEC` closes on exec, not on fork). The
            // staged file *is* the inode that lands at `dest` -- `rename`
            // moves a directory entry, never the inode -- so the inherited
            // descriptor stays a writable descriptor on the published file
            // for the child's whole fork-to-exec window, and cargo exec'ing
            // a restored build script in that window gets "Text file busy".
            //
            // The write therefore runs under the process-wide exclusive
            // guard: every soldr spawn funnel holds the shared side across
            // its `spawn()`, so no child can be forked while this descriptor
            // exists. The guard is dropped as soon as the descriptor is
            // closed -- metadata and the rename need no exclusion. Nothing
            // in this section may spawn (it would deadlock on the lock).
            let staged = staging_path_for(&job.dest);
            {
                let _write = crate::platform::process::spawn_exclusion::write_exclusive();
                let mut file = std::fs::File::create(&staged).map_err(|e| io(&staged, e))?;
                extract_test_hooks::staged_file_open_for_write(&staged);
                if let Err(e) = std::io::Write::write_all(&mut file, &job.body) {
                    drop(file);
                    let _ = std::fs::remove_file(&staged);
                    return Err(io(&staged, e));
                }
                drop(file);
            }

            // Apply metadata to the staged file, so the entry becomes visible
            // at `dest` already complete. `rename` preserves both.
            //
            // #587: restore +x (and other Unix permission bits) from
            // the tar header. Without this, cargo build-script-build
            // binaries restored from cache fail execve with EACCES.
            // Windows ignores the mode — NTFS uses ACLs, and the tar
            // header's Unix mode isn't meaningful there.
            if let Some(mode) = job.mode_bits {
                if let Err(e) = crate::platform::fs::permissions::restore_mode(&staged, Some(mode))
                {
                    let _ = std::fs::remove_file(&staged);
                    return Err(io(&staged, e));
                }
            }
            let stamp = if let Some(ns) = job.mtime_ns {
                // Manifest-driven metadata application (#1541): restore
                // the exact nanosecond mtime here. mtime policy is owned by
                // `zccache::core::mtime` (#3462); atime is left to the
                // filesystem (nothing in soldr reads these files' atime).
                Some(zccache::core::mtime::FileTime::from_system_time(
                    ns_to_systime(ns),
                ))
            } else {
                job.mtime_secs
                    .map(|secs| zccache::core::mtime::FileTime::from_unix_time(secs as i64, 0))
            };
            if let Some(stamp) = stamp {
                if let Err(e) = zccache::core::mtime::stamp_mtime(&staged, stamp) {
                    let _ = std::fs::remove_file(&staged);
                    return Err(io(&staged, e));
                }
            }

            if let Err(e) = std::fs::rename(&staged, &job.dest) {
                // Never leave the staging file behind on failure; a stray
                // `.soldr-tmp` in `target/` would confuse cargo and survive
                // into the next build.
                let _ = std::fs::remove_file(&staged);
                return Err(io(&job.dest, e));
            }
            // soldr#3350: the exclusive guard above excludes only spawns that
            // take soldr's lock. This process also hosts embedded zccache and
            // libraries that fork under their own lock or none, and such a
            // child inherits the staged write descriptor across the rename.
            // Before an executable is handed back to cargo, wait until no
            // process holds it open for writing.
            if job.mode_bits.is_some_and(|mode| mode & 0o111 != 0) {
                await_restored_executable(&job.dest);
            }
        }
        tar::EntryType::Directory => {
            // Already handled by the driver, but if somehow we got here,
            // ensure idempotency.
            std::fs::create_dir_all(&job.dest).map_err(|e| io(&job.dest, e))?;
        }
        other => {
            // Symlinks / hard links / device nodes etc. are not produced
            // by `save` (only Regular + Directory; symlinks travel as
            // manifest-only `cache_symlinks` entries — #1548). Reject
            // loudly so we don't silently swallow a future archive shape
            // change.
            return Err(SaveLoadError::BareIo(std::io::Error::other(format!(
                "unexpected tar entry type {other:?} at {}",
                job.dest.display()
            ))));
        }
    }
    Ok(())
}

/// Longest `load` waits for a foreign child to exec. A fork-to-exec window is
/// normally microseconds; this bounds a pathological one.
const RESTORED_WRITER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Best effort: a timeout or an unobservable host is logged, never an error.
fn await_restored_executable(path: &Path) {
    use crate::platform::fs::writers::{await_no_writers, WriterWait};

    match await_no_writers(path, RESTORED_WRITER_TIMEOUT) {
        Ok(WriterWait::Clear { waited }) if !waited.is_zero() => tracing::info!(
            event = "load_awaited_inherited_writer",
            path = %path.display(),
            waited_us = waited.as_micros() as u64,
            "a forked child held the restored executable open for writing"
        ),
        Ok(WriterWait::TimedOut) => tracing::warn!(
            event = "load_inherited_writer_timeout",
            path = %path.display(),
            "the restored executable is still open for writing by another process"
        ),
        Ok(_) => {}
        Err(error) => tracing::debug!(
            event = "load_writer_probe_failed",
            path = %path.display(),
            %error
        ),
    }
}
