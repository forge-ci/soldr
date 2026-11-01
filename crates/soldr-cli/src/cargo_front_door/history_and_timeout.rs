fn append_cargo_abort_log(request: CargoAbortLogRequest<'_>) -> Result<PathBuf, SoldrError> {
    let CargoAbortLogRequest {
        paths,
        session_id,
        repo_root,
        started_at_ms,
        ended_at_ms,
        args,
        timeout,
        cargo_wait_timeout,
        cleanup,
        message,
        auto_retry_planned,
    } = request;
    let path = paths.cargo_abort_log();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let retry_with_zccache_disabled: Vec<&str> = ["soldr", "cargo"]
        .into_iter()
        .chain(args.iter().map(String::as_str))
        .collect();
    let record = serde_json::json!({
        // soldr#2424: v2 drops the `--no-cache` spellings. soldr#2364 deprecated
        // and hid that flag in favour of `ZCCACHE_DISABLE=1`, and soldr#2777
        // removed it from every printed recovery message, but this record still
        // offered it twice: a `retry_without_cache` duplicate of
        // `retry_with_zccache_disabled`, and a bare-argv `clean_hint`.
        "schema_version": 2,
        "event": "cargo_abort",
        "ts_ms": ended_at_ms,
        "session_id": session_id,
        "repo_root": repo_root.display().to_string(),
        "started_at_ms": started_at_ms,
        "ended_at_ms": ended_at_ms,
        "elapsed_ms": (ended_at_ms - started_at_ms).max(0),
        "timeout": timeout,
        "timeout_config": {
            "explicit": cargo_wait_timeout.is_some(),
            "source": cargo_wait_timeout.map(|_| CARGO_WAIT_TIMEOUT_ENV_VAR),
            "duration_secs": cargo_wait_timeout.map(|duration| duration.as_secs()),
        },
        "cargo_args": args,
        "message": message,
        "auto_retry_planned": auto_retry_planned,
        "cleanup": {
            "orphan_rmetas_pruned": cleanup.orphan_rmetas_pruned,
            "incremental_dirs_removed": cleanup.incremental_dirs_removed,
        },
        "recovery": {
            "inspect_logs": ["soldr", "logs", "paths"],
            "retry_with_zccache_disabled": {
                "env": { "ZCCACHE_DISABLE": "1" },
                "argv": retry_with_zccache_disabled,
            },
            "clean_hint": {
                "env": { "ZCCACHE_DISABLE": "1" },
                "argv": ["soldr", "cargo", "clean", "-p", "<crate>"],
            },
            "timeout_env": {
                "cargo_wait": CARGO_WAIT_TIMEOUT_ENV_VAR,
                "compile_reply": "SOLDR_COMPILE_REPLY_TIMEOUT_SECS",
            },
        },
    });
    let line = serde_json::to_string(&record)
        .map_err(|err| SoldrError::Other(format!("serialize cargo abort log: {err}")))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{line}")?;
    Ok(path)
}

fn cargo_timeout_retry_allowed(cache_enabled_for_cargo: bool, args: &[String]) -> bool {
    if !cache_enabled_for_cargo || env_flag_truthy(CARGO_TIMEOUT_RETRY_DISABLE_ENV_VAR) {
        return false;
    }
    matches!(
        first_cargo_subcommand(args),
        Some("b" | "build" | "c" | "check" | "t" | "test" | "clippy" | "d" | "doc")
    )
}

fn retry_timed_out_cargo_without_cache(
    args: &[String],
    explicit_toolchain: Option<&str>,
) -> Result<std::process::ExitStatus, SoldrError> {
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command.arg("--no-cache").arg("cargo").args(args);
    if let Some(toolchain) = explicit_toolchain {
        command.env("RUSTUP_TOOLCHAIN", toolchain);
    }
    command.env(CARGO_TIMEOUT_RETRY_DISABLE_ENV_VAR, "1");
    // soldr#2739: same as the -Zthreads retry -- a fresh-pid soldr -> soldr
    // spawn needs the edge marker. Bounded by the disable flag above.
    command.env(soldr_core::self_relocate::SELF_SPAWN_EDGE_ENV_VAR, "1");
    nested_cargo_guard::forward_mode_to_front_door_retry(&mut command);
    suppress_windows_console_window(&mut command);
    configure_cargo_child_for_timeout(&mut command);
    let mut child = debug_trace::spawn_traced(&mut command, "soldr no-cache cargo retry")
        .map_err(|err| SoldrError::Other(format!("spawn no-cache cargo retry failed: {err}")))?;
    // The nested soldr invocation inherits the explicit timeout that caused
    // this retry and has recursion disabled above. The outer supervisor must
    // not add a second, implicit deadline of its own.
    wait_for_cargo_child(&mut child, "soldr no-cache cargo retry", None)
}

use build_session::{new_build_record, persist_build_session_end_fallback};

#[derive(Clone, Copy)]
struct BuildLogHistoryRequest<'a> {
    paths: &'a SoldrPaths,
    build_session_id: u64,
    repo_root: &'a Path,
    started_at_ms: i64,
    session: &'a crate::build_cache_session::BuildCacheSession,
    compile_journal_start_len: u64,
    exit_code: i32,
    ended_at_ms: i64,
    /// soldr#1536: true when the daemon acknowledged `BuildSessionEnd`,
    /// meaning the persisted BuildRecord already carries the finalized
    /// crate-count / slowest-crate aggregate and every session event is
    /// durable — the wrapper must NOT redo the O(all-history)
    /// `aggregate_session` scan in that case.
    daemon_finalized: bool,
}

/// Returns the `BuildLogPaths` that were recorded on the build row, so the
/// caller can name them in the end-of-build log summary (soldr#1813). `None`
/// means every attempt failed and nothing was persisted.
fn persist_build_log_history(
    request: BuildLogHistoryRequest<'_>,
) -> Option<crate::daemon::protocol::BuildLogPaths> {
    let build_session_id = request.build_session_id;
    let mut last_error = None;
    for attempt in 0..BUILD_HISTORY_RETRY_ATTEMPTS {
        match persist_build_log_history_inner(&request) {
            Ok(log_paths) => return Some(log_paths),
            Err(err) => {
                last_error = Some(err);
                if attempt + 1 < BUILD_HISTORY_RETRY_ATTEMPTS {
                    std::thread::sleep(BUILD_HISTORY_RETRY_POLL);
                }
            }
        }
    }
    if let Some(err) = last_error {
        eprintln!(
            "soldr warning: failed to persist logs history for build {build_session_id}: {err}"
        );
    }
    None
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn persist_build_log_history_inner(
    request: &BuildLogHistoryRequest<'_>,
) -> Result<crate::daemon::protocol::BuildLogPaths, SoldrError> {
    let BuildLogHistoryRequest {
        paths,
        build_session_id,
        repo_root,
        started_at_ms,
        session,
        compile_journal_start_len,
        exit_code,
        ended_at_ms,
        daemon_finalized,
    } = *request;
    let archive_dir = build_log_history_dir(paths, build_session_id);
    crate::daemon::history_gc::mark_history_publishing(&archive_dir)
        .map_err(|e| SoldrError::Other(format!("mark build history publishing: {e}")))?;
    let db_path = crate::cache_lib::data_db_path(paths);

    let archived_session_stats_path = copy_session_artifact(
        &session.session_stats_path,
        &archive_dir,
        "last-session-stats.json",
    );
    // soldr#3540: only this session's stats may drive the journal-tail wait
    // or enter the build's history — a file left by an earlier invocation
    // (finalize no-op'd) would both wait on compiles that never ran and
    // attach another build's hit/miss figures to this record.
    let cache_summary = cache_states::read_build_cache_summary(
        &session.session_stats_path,
        &session.session_id,
    )
    .fresh();
    let expected_compile_journal_entries = cache_summary
        .as_ref()
        .and_then(|summary| (summary.compilations > 0).then_some(summary.compilations));
    let compile_journal_path = embedded_compile_journal_path(paths);
    if let Some(expected) = expected_compile_journal_entries {
        wait_for_compile_journal_tail(&compile_journal_path, compile_journal_start_len, expected);
    }
    let archived_compile_journal_path = copy_session_artifact_tail(
        &compile_journal_path,
        &archive_dir,
        "compile_journal.jsonl",
        compile_journal_start_len,
    );

    let miss_reasons = read_build_miss_reasons(
        archived_compile_journal_path
            .as_ref()
            .map(|path| Path::new(path.as_str())),
    );
    let log_paths = crate::daemon::protocol::BuildLogPaths {
        zccache_session_id: Some(session.session_id.clone()),
        cache_dir: Some(session.cache_dir.display().to_string()),
        // The embedded service no longer updates zccache's fixed
        // `last-session` files. Recording or archiving them here would attach
        // stale cumulative data (and potentially old environment values) to
        // every new build. The build-scoped compile-journal tail above is the
        // authoritative diagnostic payload.
        session_log_path: None,
        journal_path: None,
        session_stats_path: Some(session.session_stats_path.display().to_string()),
        compile_journal_path: Some(compile_journal_path.display().to_string()),
        archived_session_log_path: None,
        archived_journal_path: None,
        archived_session_stats_path,
        archived_compile_journal_path,
        // soldr#1368: private managed-zccache daemons are gone; the
        // field stays on the wire for older records.
        private_daemon_name: None,
    };
    // soldr#1814 slice 2d: hand the whole read-modify-write to the daemon,
    // which owns this table. Sending intent rather than a get/upsert pair is
    // what makes it atomic — two front doors finishing at once cannot lose
    // each other's fields.
    let update = crate::daemon::protocol::BuildLogHistoryUpdate {
        session_id: build_session_id,
        repo_root: repo_root.display().to_string(),
        started_at_ms,
        ended_at_ms,
        exit_code,
        daemon_finalized,
        cache_summary: cache_summary.clone(),
        miss_reasons: miss_reasons.clone(),
        log_paths: Some(log_paths.clone()),
    };
    let sock = crate::daemon::client::default_sock_path(paths);
    let offline_owner = match crate::daemon::client::attach_build_log_history(&sock, update) {
        Ok(()) => None,
        Err(error) => {
            let owner = crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(paths)
                .map_err(|acquire_error| {
                    SoldrError::Other(format!(
                        "acquire offline build-history ownership after daemon error {error:?}: {acquire_error}"
                    ))
                })?;
            if owner.is_none() {
                tracing::warn!(
                    event = "build_log_history_daemon_unavailable",
                    session_id = build_session_id,
                    error = ?error,
                    "skipping build-history update because the daemon still owns this root"
                );
            }
            owner
        }
    };
    if offline_owner.is_some() {
        // Daemon unreachable — do the merge locally. Safe precisely because
        // the daemon is down: with no second opener there is nobody to race.
        // soldr-state-db: offline-root-owner
        let mut record = crate::daemon::db::get_build(&db_path, build_session_id)
            .map_err(|e| SoldrError::Other(format!("read build history: {e}")))?
            .unwrap_or_else(|| {
                new_build_record(
                    build_session_id,
                    repo_root.display().to_string(),
                    started_at_ms,
                )
            });
        record.cache_summary = cache_summary;
        record.miss_reasons = miss_reasons;
        // soldr#1536: only recompute the aggregate when BuildSessionEnd did
        // not already finalize it.
        if !daemon_finalized {
            let (crate_count, slowest_crate_us, slowest_crate_name) =
                // soldr-state-db: offline-root-owner
                crate::daemon::db::aggregate_session(&db_path, build_session_id)
                    .unwrap_or((0, None, None));
            record.crate_count = crate_count;
            record.slowest_crate_us = slowest_crate_us;
            record.slowest_crate_name = slowest_crate_name;
        }
        record.ended_at_ms = Some(record.ended_at_ms.unwrap_or(ended_at_ms));
        record.exit_code = Some(record.exit_code.unwrap_or(exit_code));
        record.total_wall_ms = Some(
            record
                .ended_at_ms
                .map(|ended| (ended - record.started_at_ms).max(0) as u64)
                .unwrap_or(0),
        );
        record.log_paths = Some(log_paths.clone());
        // soldr-state-db: offline-root-owner
        crate::daemon::db::upsert_build(&db_path, &record)
            .map_err(|e| SoldrError::Other(format!("write build history: {e}")))?;
    }
    // Publish only after the DB row points at every copied payload: a
    // marker-less session reads as active, so a retention pass cannot remove a
    // half-published archive.  Journals are sanitized by zccache#1149.  Copies
    // no-op on a missing source, so an empty archive is discarded (soldr#2186).
    crate::daemon::history_gc::publish_or_discard(&archive_dir)
        .map_err(|e| SoldrError::Other(format!("publish build history: {e}")))?;
    // Enforce the hard 1 GiB cap immediately after publication.  The daemon's
    // daily pass remains the owner of age retention and the one-time removal
    // of pre-redaction archives.
    if offline_owner.is_some() {
        let retention = crate::daemon::history_gc::HistoryGcOptions {
            now: std::time::SystemTime::now(),
            max_age: std::time::Duration::MAX,
            max_bytes: crate::daemon::history_gc::DEFAULT_MAX_BYTES,
            migrate_pre_redaction: false,
        };
        // soldr-state-db: offline-root-owner
        let _ = crate::daemon::history_gc::sweep(paths, &db_path, &retention);
    }
    Ok(log_paths)
}

fn build_log_history_dir(paths: &SoldrPaths, build_session_id: u64) -> PathBuf {
    paths
        .cache
        .join("zccache")
        .join("history")
        .join(build_session_id.to_string())
}

fn embedded_compile_journal_path(paths: &SoldrPaths) -> PathBuf {
    crate::zccache_embedded::embedded_compile_journal_path(paths)
}

/// soldr#1790: write the always-on per-build XML log. Unlike
/// `persist_build_log_history` above, this is called UNCONDITIONALLY at
/// both cargo-run call sites — it is not gated on
/// `cache_plan.zccache_session()` being `Some`, so every managed build
/// (cache enabled or disabled, success or failure) gets a log entry.
/// Best-effort: a write failure is a warning, never a build failure.
#[allow(clippy::too_many_arguments)]
fn write_always_on_build_log(
    paths: &SoldrPaths,
    session_id: u64,
    repo_root: &Path,
    argv: &[String],
    started_at_ms: i64,
    ended_at_ms: i64,
    exit_code: i32,
    compile_journal_start_len: u64,
    // soldr#1799: the already-resolved cargo binary. Passed in rather than
    // re-resolved here -- `resolve_toolchain_binary` costs up to two
    // `rustup which` subprocesses (~65 ms each), and #1843 is specifically
    // about the front door's fixed per-invocation overhead.
    cargo_bin: &Path,
    // soldr#3567: a Dylint-scoped child runs under the Dylint Rustup home
    // whatever its binary's location, and the log must say so.
    dylint_scoped: bool,
    // soldr#2545: the effective wrapper identity the cache plan applied.
    wrapper: Option<crate::build_log::WrapperIdentity>,
    // cargo's `fingerprint dirty for` records, parsed from the captured
    // stderr when the front door captured one; empty otherwise.
    fingerprint_dirty: Vec<crate::build_log::FingerprintDirty>,
) -> Option<PathBuf> {
    let toolchain = crate::dylint_rustup_home::child_home_origin(cargo_bin, dylint_scoped).map(
        |origin| crate::build_log::ToolchainHomes {
            home_origin: origin.as_str(),
            binary: cargo_bin.to_path_buf(),
        },
    );
    let request = crate::build_log::BuildLogRequest {
        paths,
        session_id,
        cwd: repo_root,
        args: argv,
        started_at_ms,
        ended_at_ms,
        exit_code,
        compile_journal_path: Some(embedded_compile_journal_path(paths)),
        compile_journal_start_len,
        toolchain,
        wrapper,
        fingerprint_dirty,
    };
    // soldr#1813: the written path is returned so the end-of-build log summary
    // can name a file it knows exists, rather than a location it guessed.
    match crate::build_log::write_build_log(&request) {
        Ok(path) => Some(path),
        Err(err) => {
            eprintln!("soldr warning: failed to write build log: {err}");
            None
        }
    }
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn compile_fallback_summary_message(count: usize, path: &Path) -> String {
    format!(
        "soldr: compiler cache unavailable for {count} compiler invocation(s); \
         used direct compiler. Full details: {}",
        path.display()
    )
}

/// Returns the fallback-log path only when *this* session appended to it, so
/// the end-of-build summary (soldr#1813) lists it as a log this build wrote
/// rather than as a location that merely exists.
fn emit_compile_fallback_summary(
    paths: &SoldrPaths,
    cursor: &crate::compile_dispatch::CompileFallbackCursor,
    session_id: u64,
) -> Option<PathBuf> {
    match crate::compile_dispatch::compile_daemon_fallback_count_since(paths, cursor, session_id) {
        Ok((0, _)) => None,
        Ok((count, path)) => {
            eprintln!("{}", compile_fallback_summary_message(count, &path));
            Some(path)
        }
        Err(error) => {
            eprintln!("soldr warning: failed to summarize compiler-cache fallbacks: {error}");
            None
        }
    }
}

const FALLBACK_OUTPUT_SCRUB_MARKER: &str = ".soldr-fallback-output-scrub-v1";
const FALLBACK_OUTPUT_SCRUB_LOCK: &str = ".soldr-fallback-output-scrub-v1.lock";

#[derive(Debug, PartialEq, Eq)]
enum FallbackOutputScrub {
    AlreadyDone,
    DeferredForActiveBuild(PathBuf),
    Complete(usize),
}

/// Remove fallback notices persisted by older Soldr versions from Cargo's
/// fingerprint diagnostics. The migration is target-local and marker-gated,
/// so warm builds pay only one metadata lookup after the first successful
/// scan. Replacing changed files via a temporary file deliberately breaks any
/// hardlink instead of mutating a shared cache blob in place.
fn scrub_cached_fallback_diagnostics_once(
    target_dir: &Path,
) -> Result<FallbackOutputScrub, SoldrError> {
    std::fs::create_dir_all(target_dir)?;
    let marker = target_dir.join(FALLBACK_OUTPUT_SCRUB_MARKER);
    if marker.exists() {
        return Ok(FallbackOutputScrub::AlreadyDone);
    }

    let lock_path = target_dir.join(FALLBACK_OUTPUT_SCRUB_LOCK);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    crate::platform::wait::lock_exclusive_for(&lock, crate::platform::wait::SHORT_LOCK)?;
    if marker.exists() {
        return Ok(FallbackOutputScrub::AlreadyDone);
    }

    // Retain Cargo's real lock handles through the complete scan and marker
    // publication. If another build already owns this target, defer without a
    // marker so a later invocation retries after that build has quiesced.
    let _cargo_locks = match crate::cache_lib::cargo_lock::probe(target_dir)? {
        crate::cache_lib::cargo_lock::CargoLockProbe::Idle(guard) => guard,
        crate::cache_lib::cargo_lock::CargoLockProbe::Active(path) => {
            return Ok(FallbackOutputScrub::DeferredForActiveBuild(path));
        }
    };

    let mut scrubbed = 0;
    // soldr#2760: never jwalk's default pool. It aborts the walk after one
    // second when the ambient rayon pool cannot serve it, which on a loaded
    // machine turns a correct scrub into `Io(ThreadpoolBusy)`.
    for entry in jwalk::WalkDir::new(target_dir)
        .follow_links(false)
        .max_depth(6)
        .skip_hidden(false)
        .parallelism(crate::cache_lib::save::walk_parallelism(None))
    {
        let entry = entry.map_err(std::io::Error::other)?;
        if !entry.file_type().is_file()
            || !entry.file_name().to_string_lossy().starts_with("output-")
        {
            continue;
        }
        let path = entry.path();
        if !path
            .components()
            .any(|component| component.as_os_str() == std::ffi::OsStr::new(".fingerprint"))
        {
            continue;
        }

        let original = std::fs::read(&path)?;
        let filtered =
            crate::zccache_embedded::strip_internal_soldr_fallback_notices(original.clone());
        if filtered == original {
            continue;
        }

        no_cache_detach::prepare_path_for_replacement(&path)?;
        let parent = path.parent().unwrap_or(target_dir);
        let permissions = std::fs::metadata(&path)?.permissions();
        let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
        replacement.write_all(&filtered)?;
        replacement.flush()?;
        replacement.persist(&path).map_err(|error| error.error)?;
        std::fs::set_permissions(&path, permissions)?;
        scrubbed += 1;
    }

    std::fs::write(marker, [])?;
    Ok(FallbackOutputScrub::Complete(scrubbed))
}

/// Wait for the embedded compile journal to contain the expected number
/// of entries for this build.
///
/// soldr#1536: the pre-#1536 version demanded three consecutive 25 ms
/// "stable length" polls even when the journal was already complete,
/// putting a fixed ~75 ms floor on every finalization. Completeness is
/// now judged directly: an entry only counts once its line is
/// newline-terminated (the zccache journal thread writes whole lines),
/// so as soon as `expected_entries` complete lines are visible the wait
/// returns without sleeping at all — the common case, since the journal
/// entries were enqueued before the last compile reply and the daemon
/// ack round-trip already happened. The 2 s deadline only bounds the
/// rare case where the journal writer thread lags.
fn wait_for_compile_journal_tail(path: &Path, start_offset: u64, expected_entries: u64) -> bool {
    wait_for_compile_journal_tail_with(
        path,
        start_offset,
        expected_entries,
        COMPILE_JOURNAL_TAIL_WAIT,
        || std::thread::sleep(COMPILE_JOURNAL_TAIL_POLL),
    )
}

/// Testable core of [`wait_for_compile_journal_tail`] with an injected
/// sleep so tests can assert the zero-sleep fast path.
fn wait_for_compile_journal_tail_with(
    path: &Path,
    start_offset: u64,
    expected_entries: u64,
    wait_budget: Duration,
    mut sleep: impl FnMut(),
) -> bool {
    let deadline = Instant::now() + wait_budget;
    loop {
        if expected_entries == 0
            || count_complete_compile_journal_tail_entries(path, start_offset).unwrap_or(0)
                >= expected_entries
        {
            return true;
        }
        if Instant::now() >= deadline {
            // Best effort past the deadline: report whether ANY tail
            // showed up so the caller still archives what exists.
            return file_len(path) > start_offset;
        }
        sleep();
    }
}

/// Count newline-terminated, non-empty journal lines past `start_offset`.
/// A trailing line without its `\n` is still in flight (partial write by
/// the journal thread or a concurrent build) and does not count.
fn count_complete_compile_journal_tail_entries(path: &Path, start_offset: u64) -> Option<u64> {
    let tail = read_file_tail(path, start_offset)?;
    Some(
        tail.split_inclusive('\n')
            .filter(|line| line.ends_with('\n') && !line.trim().is_empty())
            .count() as u64,
    )
}

fn read_file_tail(path: &Path, start_offset: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len <= start_offset {
        return None;
    }
    file.seek(SeekFrom::Start(start_offset)).ok()?;
    let mut body = String::new();
    file.read_to_string(&mut body).ok()?;
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}

fn copy_session_artifact(source: &Path, archive_dir: &Path, file_name: &str) -> Option<String> {
    if !source.is_file() {
        return None;
    }
    std::fs::create_dir_all(archive_dir).ok()?;
    let dest = archive_dir.join(file_name);
    std::fs::copy(source, &dest).ok()?;
    Some(dest.display().to_string())
}

fn copy_session_artifact_tail(
    source: &Path,
    archive_dir: &Path,
    file_name: &str,
    start_offset: u64,
) -> Option<String> {
    let tail = read_file_tail(source, start_offset)?;
    // soldr#1536: drop a trailing partial line (an in-flight write by
    // the journal thread or a concurrent build) so the archive holds
    // only complete JSONL records. A tail with no newline at all is
    // kept whole — better a truncated best-effort record than nothing.
    let complete = match tail.rfind('\n') {
        Some(last_newline) => &tail[..=last_newline],
        None => tail.as_str(),
    };
    let complete = sanitize_compile_journal_jsonl(complete);
    if complete.is_empty() {
        return None;
    }
    std::fs::create_dir_all(archive_dir).ok()?;
    let dest = archive_dir.join(file_name);
    std::fs::write(&dest, complete).ok()?;
    Some(dest.display().to_string())
}

/// Defense-in-depth at the Soldr archive boundary. zccache#1149 sanitizes the
/// live journal before persistence; applying the same shared sanitizer while
/// copying means an unexpected legacy/raw line can never be promoted into a
/// new build-history archive. Invalid JSON is dropped closed.
fn sanitize_compile_journal_jsonl(body: &str) -> String {
    let mut output = String::new();
    for line in body.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(object) = value.as_object_mut() {
            if let Some(raw_env) = object.remove("env") {
                let env = serde_json::from_value::<Vec<(String, String)>>(raw_env).ok();
                if let Some(sanitized) = zccache::daemon::compile_journal::sanitize_journal_env(env)
                {
                    if let Ok(safe) = serde_json::to_value(sanitized) {
                        object.insert("env".to_string(), safe);
                    }
                }
            }
        }
        if let Ok(line) = serde_json::to_string(&value) {
            output.push_str(&line);
            output.push('\n');
        }
    }
    output
}

fn read_build_miss_reasons(
    compile_journal_path: Option<&Path>,
) -> Vec<crate::daemon::protocol::BuildMissReason> {
    compile_journal_path
        .map(read_build_miss_reasons_from_journal)
        .unwrap_or_default()
}

fn read_build_miss_reasons_from_journal(
    journal_path: &Path,
) -> Vec<crate::daemon::protocol::BuildMissReason> {
    let Ok(raw) = std::fs::read_to_string(journal_path) else {
        return Vec::new();
    };
    parse_build_miss_reasons_from_journal(&raw)
}

fn parse_build_miss_reasons_from_journal(
    journal_body: &str,
) -> Vec<crate::daemon::protocol::BuildMissReason> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for line in journal_body.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let outcome = value
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if !matches!(outcome, "miss" | "link_miss") {
            continue;
        }
        let reason = value
            .get("miss_reason")
            .and_then(serde_json::Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or("unknown")
            .to_string();
        *counts.entry(reason).or_insert(0) += 1;
    }
    sorted_miss_reasons(counts)
}

fn sorted_miss_reasons(
    counts: BTreeMap<String, u64>,
) -> Vec<crate::daemon::protocol::BuildMissReason> {
    let mut reasons: Vec<_> = counts
        .into_iter()
        .map(|(reason, count)| crate::daemon::protocol::BuildMissReason { reason, count })
        .collect();
    reasons.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));
    reasons
}

fn cargo_wait_timeout() -> Result<Option<Duration>, SoldrError> {
    let value = match std::env::var(CARGO_WAIT_TIMEOUT_ENV_VAR) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(SoldrError::Other(format!(
                "invalid {CARGO_WAIT_TIMEOUT_ENV_VAR}: expected 0 or a positive integer number of seconds, but the value is not valid Unicode"
            )))
        }
    };
    let seconds = value.parse::<u64>().map_err(|_| {
        SoldrError::Other(format!(
            "invalid {CARGO_WAIT_TIMEOUT_ENV_VAR}={value:?}: expected 0 or a positive integer number of seconds"
        ))
    })?;
    Ok((seconds > 0).then(|| Duration::from_secs(seconds)))
}

fn wait_for_cargo_child(
    child: &mut std::process::Child,
    context: &str,
    timeout: Option<Duration>,
) -> Result<std::process::ExitStatus, SoldrError> {
    wait_for_cargo_child_with_heartbeat(
        child,
        context,
        timeout,
        Duration::from_secs(CARGO_WAIT_HEARTBEAT_SECS),
    )
}

fn wait_for_cargo_child_with_heartbeat(
    child: &mut std::process::Child,
    context: &str,
    timeout: Option<Duration>,
    heartbeat: Duration,
) -> Result<std::process::ExitStatus, SoldrError> {
    wait_for_cargo_child_guarded(child, context, timeout, heartbeat, None)
}

/// Wait for a Cargo child, enforcing the optional wall-clock `timeout` and,
/// when `guard` is present, the nested-Cargo self-lock guard (soldr#2924):
/// the wait is sliced to [`nested_cargo_guard::GUARD_TICK`] so a tripped
/// guard tears the tree down within one tick. Heartbeat lines keep their
/// `heartbeat` cadence either way.
fn wait_for_cargo_child_guarded(
    child: &mut std::process::Child,
    context: &str,
    timeout: Option<Duration>,
    heartbeat: Duration,
    guard: Option<&nested_cargo_guard::NestedCargoGuard>,
) -> Result<std::process::ExitStatus, SoldrError> {
    let start = Instant::now();
    let mut next_heartbeat = heartbeat;
    loop {
        if let Some(violation) = guard.and_then(nested_cargo_guard::NestedCargoGuard::violation) {
            return Err(nested_cargo_guard::teardown_std_child(
                child, context, &violation,
            ));
        }
        let elapsed = start.elapsed();
        if let Some(timeout) = timeout {
            if elapsed >= timeout {
                return Err(cargo_timeout_error(child, context, timeout));
            }
        }
        let mut wait_for = next_heartbeat.saturating_sub(elapsed);
        if let Some(timeout) = timeout {
            wait_for = wait_for.min(timeout.saturating_sub(elapsed));
        }
        if guard.is_some() {
            wait_for = wait_for.min(nested_cargo_guard::GUARD_TICK);
        }
        match child
            .wait_timeout(wait_for)
            .map_err(|err| SoldrError::Other(format!("wait on {context} failed: {err}")))?
        {
            Some(status) => {
                // soldr#2546: pair the `spawned` event from `spawn_traced`.
                debug_trace::child_exited(child.id(), context, &status);
                return Ok(status);
            }
            None => {
                if let Some(timeout) = timeout {
                    if start.elapsed() >= timeout {
                        return Err(cargo_timeout_error(child, context, timeout));
                    }
                }
                if start.elapsed() >= next_heartbeat {
                    // soldr#4005: a porcelain caller reads one JSON line;
                    // a heartbeat in its stderr is pure context cost.
                    if !crate::cargo_front_door::porcelain::enabled() {
                        eprintln!(
                            "{}",
                            cargo_wait_heartbeat_message(context, start.elapsed(), timeout)
                        );
                    }
                    next_heartbeat += heartbeat;
                }
            }
        }
    }
}

fn cargo_wait_heartbeat_message(
    context: &str,
    elapsed: Duration,
    timeout: Option<Duration>,
) -> String {
    match timeout {
        Some(timeout) => format!(
            "soldr: {context} still running after {}s (explicit timeout {}s from {CARGO_WAIT_TIMEOUT_ENV_VAR})",
            elapsed.as_secs(),
            timeout.as_secs()
        ),
        None => format!(
            "soldr: {context} still running after {}s (no wall-clock deadline configured)",
            elapsed.as_secs()
        ),
    }
}

fn cargo_timeout_error(
    child: &mut std::process::Child,
    context: &str,
    timeout: Duration,
) -> SoldrError {
    let kill_result = kill_cargo_process_tree(child);
    let reap_result = child.wait_timeout(Duration::from_secs(KILLED_CARGO_REAP_TIMEOUT_SECS));
    let timeout_secs = timeout.as_secs();
    let mut message = format!(
        "{context} timed out after {timeout_secs} seconds \
         (explicitly configured by {CARGO_WAIT_TIMEOUT_ENV_VAR}; set it to 0 to disable)"
    );
    match kill_result {
        Ok(detail) => message.push_str(&format!("; {detail}")),
        Err(err) => message.push_str(&format!("; kill failed: {err}")),
    }
    match reap_result {
        Ok(Some(_)) => {}
        Ok(None) => message.push_str(&format!(
            "; process did not exit within {KILLED_CARGO_REAP_TIMEOUT_SECS} seconds after kill"
        )),
        Err(err) => message.push_str(&format!("; reap after kill failed: {err}")),
    }
    SoldrError::Other(message)
}

pub(crate) fn kill_cargo_process_tree(
    child: &mut std::process::Child,
) -> std::io::Result<&'static str> {
    use crate::platform::process::terminate::TreeKill;
    match crate::platform::process::terminate::terminate_tree(child)? {
        TreeKill::TreeKilled => Ok("killed child process tree"),
        // soldr#2605: name the weaker outcome instead of letting it read as a
        // clean kill. This arm means the tree could not be enumerated, or
        // descendants were still alive when the verification budget ran out.
        TreeKill::ProcessKilled => Ok("killed child process (descendants may have survived)"),
    }
}

pub(crate) fn configure_cargo_child_for_timeout(command: &mut std::process::Command) {
    if std::env::var_os(INHERIT_PARENT_PROCESS_GROUP_ENV).is_none() {
        crate::platform::process::command::configure_process_group(command);
    } else {
        command.env_remove(INHERIT_PARENT_PROCESS_GROUP_ENV);
    }
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// Acquire the build-activity lease and set the process-wide `build_active`
// flag — the session-start boundary `build_session_order_lint` guards (must run
// only after every fallible pre-cargo step). The paired `BuildSessionStart`
// publish is a separate caller step so the profiler attributes it (soldr#1843).
fn begin_build_activity_lease(
    paths: &SoldrPaths,
    session_id: u64,
) -> Result<crate::cache_lib::build_active::BuildActivityLease, SoldrError> {
    let lease = crate::cache_lib::build_active::BuildActivityLease::acquire(paths, session_id)
        .map_err(|error| {
            SoldrError::Other(format!("failed to acquire build activity lease: {error}"))
        })?;
    crate::cache_lib::build_active::set(true);
    Ok(lease)
}

// Retired target-GC opt-out flags, still stripped for compatibility.
