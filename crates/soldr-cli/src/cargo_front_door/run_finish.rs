// The build session of `run_cargo_front_door` (run.rs): session start, the
// child cargo run, and its completion -- the aborted path and the normal one.

/// The build session wrapped around one child cargo run.
struct FrontDoorBuildSession {
    session_id: u64,
    started_at_ms: i64,
    repo_root: PathBuf,
    /// soldr#1790: full invoked argv (the soldr binary + every arg),
    /// captured once and reused by `write_always_on_build_log` at both the
    /// cargo-run-error and normal-completion call sites.
    invoked_argv: Vec<String>,
    build_activity_lease: Option<crate::cache_lib::build_active::BuildActivityLease>,
    publish: Option<std::thread::JoinHandle<()>>,
    compile_journal_start_len: u64,
    compile_fallback_cursor: crate::compile_dispatch::CompileFallbackCursor,
    cache_state_tail: Option<cache_states::CacheStateTail>,
    /// Where a `--porcelain` run's per-unit cache annotations went instead of
    /// stderr. `None` for a default run (its annotations streamed live) and for
    /// a run with no cache session.
    cache_aux: Option<cache_annotations::DeferredCacheLines>,
}

impl FrontDoorBuildSession {
    fn begin(
        prepared: &mut PreparedCargo,
        profile: &mut crate::startup_profile::WrapperProfile,
    ) -> Result<Self, SoldrError> {
        // Phase 2: start session correlation only after every fallible pre-cargo
        // preparation step (especially no-cache ownership detachment) succeeds.
        // From here, the cargo runner's success/error paths always pair this with
        // BuildSessionEnd and clear build_active, so a rejected preflight cannot
        // strand daemon maintenance in the "build active" state.
        let session_id = generate_build_session_id();
        prepared.command.env(
            crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR,
            session_id.to_string(),
        );
        let started_at_ms = current_unix_ms();
        let repo_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let invoked_argv: Vec<String> = std::env::args().collect();
        let paths = &prepared.paths;
        let build_activity_lease = begin_build_activity_lease(paths, session_id)?;
        profile.mark("build_activity_lease");
        // soldr#1843: publish BuildSessionStart concurrently with cargo (its ~740 ms IPC off the critical path), joined before BuildSessionEnd below.
        let publish = build_session::spawn_start_and_warn_on_jobs_drift(
            paths,
            session_id,
            &repo_root,
            started_at_ms,
        );
        profile.mark("build_session_ipc_spawn");
        // soldr#1368 observability restore: snapshot the embedded zccache
        // compile counters just before cargo runs so `finish_zccache_session`
        // can diff start-vs-end into the per-build hit/miss summary written to
        // `last-session-stats.json`.
        if let Some(session) = prepared.cache_plan.zccache_session() {
            crate::cache::capture_build_baseline(&session.cache_dir, &session.session_id);
        }
        let compile_journal_start_len = file_len(&embedded_compile_journal_path(paths));
        let compile_fallback_cursor =
            crate::compile_dispatch::compile_daemon_fallback_cursor(paths);
        // soldr#2302: live per-unit HIT/MISS annotations (no-op for a --no-cache run).
        // Under --porcelain they are deferred into the durable run log rather
        // than printed: the caller asked for one JSON line and its counts.
        let (cache_state_tail, cache_aux) = match cache_states::start_tail(
            &prepared.cache_plan,
            paths,
            compile_journal_start_len,
            &prepared.args,
            prepared.porcelain,
        ) {
            Some(started) => (started.tail, started.deferred),
            None => (None, None),
        };
        Ok(Self {
            session_id,
            started_at_ms,
            repo_root,
            invoked_argv,
            build_activity_lease: Some(build_activity_lease),
            publish: Some(publish),
            compile_journal_start_len,
            compile_fallback_cursor,
            cache_state_tail,
            cache_aux,
        })
    }

    /// Persist the zccache build-log history, when a cache session ran.
    fn persist_history(
        &self,
        prepared: &PreparedCargo,
        exit_code: i32,
        ended_at_ms: i64,
        daemon_finalized: bool,
    ) -> Option<crate::daemon::protocol::BuildLogPaths> {
        let session = prepared.cache_plan.zccache_session()?;
        persist_build_log_history(BuildLogHistoryRequest {
            paths: &prepared.paths,
            build_session_id: self.session_id,
            repo_root: &self.repo_root,
            started_at_ms: self.started_at_ms,
            session,
            compile_journal_start_len: self.compile_journal_start_len,
            exit_code,
            ended_at_ms,
            daemon_finalized,
        })
    }

    /// Write the always-on per-build log (soldr#1790).
    fn write_build_log(
        &self,
        prepared: &PreparedCargo,
        ended_at_ms: i64,
        exit_code: i32,
        fingerprint_dirty: Vec<crate::build_log::FingerprintDirty>,
    ) -> Option<PathBuf> {
        write_always_on_build_log(
            &prepared.paths,
            self.session_id,
            &self.repo_root,
            &self.invoked_argv,
            self.started_at_ms,
            ended_at_ms,
            exit_code,
            self.compile_journal_start_len,
            &prepared.cargo,
            prepared.dylint_plan.is_some(),
            prepared.cache_plan.wrapper_identity(),
            fingerprint_dirty,
        )
    }
}

fn run_child_cargo(prepared: &mut PreparedCargo, setup: &FrontDoorSetup) -> CargoRunResult {
    // soldr#2924: one nested-Cargo guard, shared by whichever mode spawns Cargo.
    let guard = NestedCargoGuard::for_front_door(setup.nested_cargo_mode);
    let guard = guard.as_ref();
    let cargo_wait_timeout = setup.cargo_wait_timeout;
    let command = &mut prepared.command;
    if prepared.porcelain {
        // `--porcelain` / `SOLDR_PORCELAIN` takes precedence over every other
        // capture mode: its whole contract is that the child's bytes never
        // reach the caller's stdio. It still recovers the artifact closure
        // from the (already `--message-format=json`) stdout when the
        // packed-DWARF capture is on, so the two compose rather than cancel.
        let target_dir = prepared
            .cache_plan
            .target_dir_for_hooks(&prepared.args)
            .unwrap_or_else(|| disk::cargo_disk_space_probe_path(&prepared.args));
        let label = porcelain::command_string(&prepared.args);
        run_command_capturing_porcelain(
            command,
            &target_dir,
            cargo_wait_timeout,
            guard,
            prepared.capture_cargo_artifacts,
            &prepared.paths,
            &label,
        )
        .map(|(status, captured, paths, log)| (status, Some(captured), paths, log))
    } else if prepared.capture_cargo_artifacts {
        let target_dir = prepared
            .cache_plan
            .target_dir_for_hooks(&prepared.args)
            .unwrap_or_else(|| disk::cargo_disk_space_probe_path(&prepared.args));
        run_command_capturing_cargo_json(command, &target_dir, cargo_wait_timeout, guard)
            .map(|(status, captured, paths)| (status, Some(captured), Some(paths), None))
    } else if prepared.capture_for_diagnostics && !debug_trace::observed_spawn_required() {
        // soldr#2546 slice 3: the diagnostic-tail capture observes
        // descendants by attaching the monitor to the spawned pid
        // post-hoc (running-process#1026), so on Unix the slice-2 trade —
        // which skipped this mode under --debug and with it the
        // post-failure diagnostics summary — is repaid: headless --debug
        // builds keep both the diagnostics and the descendant timeline.
        // Windows keeps the observed inherited-stdio spawn under --debug
        // instead: its descendant discovery is the Job Object wired at
        // spawn, so a post-hoc attach observes nothing there.
        run_command_capturing_diagnostic_tail(command, cargo_wait_timeout, guard)
            .map(|(status, captured)| (status, Some(captured), None, None))
    } else {
        // soldr#2546 slice 2: under --debug on a terminal, builds run
        // inherited-stdio through the running-process observer
        // (`with_observer_and_command`) so the timeline records
        // descendants without touching cargo's TTY output. The JSON
        // artifact-capture mode above keeps its load-bearing pipe
        // plumbing and observes via the same post-hoc attach.
        run_command_inheriting_stdio(command, cargo_wait_timeout, guard)
            .map(|status| (status, None, None, None))
    }
}

/// The child cargo run failed to complete (spawn error, timeout, ...):
/// close the session, report, and retry without cache when allowed.
fn handle_aborted_cargo_run(
    err: SoldrError,
    prepared: &PreparedCargo,
    mut session: FrontDoorBuildSession,
    setup: &FrontDoorSetup,
) -> Result<i32, SoldrError> {
    let paths = &prepared.paths;
    let args = prepared.args.as_slice();
    let timeout = cargo_run_error_is_timeout(&err);
    let ended_at_ms = current_unix_ms();
    let session_id = session.session_id;
    let daemon_finalized =
        crate::daemon::client::build_session_end(paths, session_id, -1, ended_at_ms).is_ok();
    if !daemon_finalized {
        persist_build_session_end_fallback(paths, session_id, -1, ended_at_ms);
    }
    let cleanup = cleanup_after_aborted_cargo_run(&prepared.cache_plan, args, timeout);
    let finish_result = prepared
        .cache_plan
        .finish_zccache_session(setup.command_lifetime_shutdown_timeout);
    let build_log_paths = session.persist_history(prepared, -1, ended_at_ms, daemon_finalized);
    let build_log = session.write_build_log(prepared, ended_at_ms, -1, Vec::new());
    crate::cache_lib::build_active::set(false);
    drop(session.build_activity_lease.take());
    let compile_fallback_log =
        emit_compile_fallback_summary(paths, &session.compile_fallback_cursor, session.session_id);
    // soldr#2302: whatever the cache managed before the abort.
    cache_states::emit_build_stats(&prepared.cache_plan);
    // soldr#1813: an aborted/timed-out cargo run is exactly when the
    // user most needs the log paths, and this arm always returns early —
    // so the summary is emitted here too rather than at the shared tail.
    if !prepared.porcelain {
        log_summary::emit_session_log_summary(
            &log_summary::SessionLogs {
                build_log,
                build_log_paths,
                compile_fallback_log,
            },
            -1,
        );
    }
    if let Err(finish_err) = finish_result {
        eprintln!(
            "soldr warning: failed to finish zccache session after aborted cargo run: {finish_err}"
        );
    }
    let augmented = augment_aborted_cargo_error(err, cleanup, timeout);
    let auto_retry_planned =
        timeout && cargo_timeout_retry_allowed(prepared.cache_enabled_for_cargo, args);
    match append_cargo_abort_log(CargoAbortLogRequest {
        paths,
        session_id: session.session_id,
        repo_root: &session.repo_root,
        started_at_ms: session.started_at_ms,
        ended_at_ms,
        args,
        timeout,
        cargo_wait_timeout: setup.cargo_wait_timeout,
        cleanup,
        message: &augmented.to_string(),
        auto_retry_planned,
    }) {
        Ok(path) => eprintln!("soldr: cargo abort details written to {}", path.display()),
        Err(log_err) => {
            eprintln!("soldr warning: failed to write cargo abort log: {log_err}")
        }
    }
    if auto_retry_planned {
        if !prepared.porcelain {
            eprintln!(
                "soldr: retrying timed-out cargo run without cache: soldr --no-cache cargo <same args>"
            );
        }
        return match retry_timed_out_cargo_without_cache(
            args,
            prepared.explicit_toolchain.as_deref(),
        ) {
            Ok(status) => {
                let code = status
                    .code()
                    .unwrap_or(if status.success() { 0 } else { 1 });
                if !prepared.porcelain {
                    eprintln!("soldr: no-cache cargo retry exited with code {code}");
                }
                Ok(code)
            }
            Err(retry_err) => Err(SoldrError::Other(format!(
                "{augmented}; no-cache retry failed: {retry_err}"
            ))),
        };
    }
    // Porcelain: the cargo run never produced a status, so there is no run
    // log from the capture path. Write a small failure log holding soldr's
    // own diagnosis and emit the single JSON line, then fall through to the
    // error that sets the process exit code.
    //
    // The reported code is the one this process will exit with: `Err` below
    // reaches `report_and_exit`, which exits 1. Emitting `-1` here made the
    // JSON disagree with `$?` for the same failed run.
    if prepared.porcelain {
        let message = augmented.to_string();
        let failure_log =
            porcelain::write_failure_log(paths, &porcelain::command_string(args), &message);
        // The tail was already stopped, so these annotations are complete; a
        // run that aborted must not lose them just because it has no capture
        // log to append to.
        let deferred = session.cache_aux.take().map(|aux| aux.drain());
        porcelain::append_deferred_annotations_to(failure_log.as_deref(), deferred.as_ref());
        porcelain::emit(&porcelain::report_for_failure(
            &porcelain::command_string(args),
            porcelain::FAILURE_EXIT_CODE,
            failure_log,
            Some(&message),
        ));
    }
    Err(augmented)
}

/// Append the annotations a `--porcelain` run deferred from stderr to its
/// durable log, returning how many moved.
///
/// run.rs stops the tail as soon as cargo exits, so the buffer is complete by
/// the time this runs. Keeping the move here rather than inline leaves
/// `complete_cargo_run` inside clippy's `too_many_lines` ceiling.
fn append_deferred_annotations(
    porcelain_log: Option<&porcelain::PorcelainLogArtifact>,
    session: &mut FrontDoorBuildSession,
) -> usize {
    let deferred = session.cache_aux.take().map(|aux| aux.drain());
    porcelain::append_deferred_annotations(porcelain_log, deferred.as_ref())
}

/// Report a signal-terminated child before the exit code loses the signal
/// (soldr#3938). Extracted so `complete_cargo_run` stays inside clippy's
/// `too_many_lines` ceiling.
fn warn_on_signal_termination(status: &std::process::ExitStatus) {
    if status.code().is_none() {
        let returncode = soldr_platform::process::test_child::returncode(status);
        if returncode < 0 {
            eprintln!(
                "soldr: cargo terminated by signal {}",
                returncode.saturating_neg()
            );
        }
    }
}

/// The child cargo ran to completion: close the session, publish logs and
/// artifacts, and apply the post-build fallbacks.
fn complete_cargo_run(
    (status, diagnostic_capture, cargo_artifact_paths, porcelain_log): (
        std::process::ExitStatus,
        Option<String>,
        Option<Vec<String>>,
        Option<porcelain::PorcelainLogArtifact>,
    ),
    prepared: &PreparedCargo,
    mut session: FrontDoorBuildSession,
    setup: &FrontDoorSetup,
    normalized: NormalizedArgs,
) -> Result<i32, SoldrError> {
    let paths = &prepared.paths;
    warn_on_signal_termination(&status);
    // Before the report is built, so the log and the count agree.
    let deferred_cache_lines = append_deferred_annotations(porcelain_log.as_ref(), &mut session);
    let captured_stderr_for_diagnosis = diagnostic_capture;
    crate::verdict::gate::note_output(captured_stderr_for_diagnosis.as_deref());
    let compile_fallback_log =
        emit_compile_fallback_summary(paths, &session.compile_fallback_cursor, session.session_id);
    let strip_outcome = strip_diagnostics::StripOutcome::from_cargo(
        status.success(),
        captured_stderr_for_diagnosis.as_deref(),
    );
    let effective_exit_code = strip_outcome.effective_exit_code(&status);
    let porcelain_report = porcelain::captured_run_report(
        porcelain_log.as_ref(),
        &porcelain::command_string(&prepared.args),
        &status,
        effective_exit_code,
        captured_stderr_for_diagnosis.as_deref(),
        deferred_cache_lines,
    );

    // Phase 2: send BuildSessionEnd before the success/failure
    // branches do any further work. Best-effort — never affects the
    // build's own outcome. soldr#1536: the daemon acknowledges once the
    // finalized aggregate and every session event are durable; on any
    // error we fall back to the direct-redb finalization below.
    let ended_at_ms = current_unix_ms();
    let session_id = session.session_id;
    let daemon_finalized = crate::daemon::client::build_session_end(
        paths,
        session_id,
        effective_exit_code,
        ended_at_ms,
    )
    .is_ok();
    if !daemon_finalized {
        persist_build_session_end_fallback(paths, session_id, effective_exit_code, ended_at_ms);
    }
    if let Some(warning) = &prepared.unrecorded_debug_warning {
        warning.record(paths);
    }
    let post_cargo_result = publish_cargo_outputs(
        &status,
        &strip_outcome,
        cargo_artifact_paths.as_deref(),
        prepared,
        normalized.trampoline_plan.as_deref(),
    );
    if !status.success() {
        diagnose_cargo_failure(
            captured_stderr_for_diagnosis.as_deref(),
            prepared.cargo_job_budget.as_ref(),
        );
    }

    let finish_result = prepared
        .cache_plan
        .finish_zccache_session(setup.command_lifetime_shutdown_timeout);
    let build_log_paths =
        session.persist_history(prepared, effective_exit_code, ended_at_ms, daemon_finalized);
    let build_log = session.write_build_log(
        prepared,
        ended_at_ms,
        effective_exit_code,
        captured_stderr_for_diagnosis
            .as_deref()
            .map(fingerprint_noise::extract_dirty_records)
            .unwrap_or_default(),
    );
    // soldr#2302: automatic cache-stats summary from the session baseline-diff
    // (precisely build-scoped), printed just above the log-paths block.
    cache_states::emit_build_stats(&prepared.cache_plan);
    // soldr#1813: tell the user where the logs went. Printed here because this
    // is the last point both the success and the compiler-failure paths pass
    // through — everything below can bail out via `?` or the zthreads retry.
    //
    // Porcelain suppresses this block: the run log's path is already the
    // JSON `log_path`, so the human summary would be pure noise a caller
    // asked not to receive.
    if !prepared.porcelain {
        log_summary::emit_session_log_summary(
            &log_summary::SessionLogs {
                build_log,
                build_log_paths,
                compile_fallback_log,
            },
            effective_exit_code,
        );
    }
    // History is now copied, sanitized, indexed, and marked complete. Keep the
    // lease through that publication boundary so migration GC cannot remove a
    // half-written archive.
    crate::cache_lib::build_active::set(false);
    drop(session.build_activity_lease.take());
    if prepared.build_like_cargo {
        gc::maybe_spawn_auto_gc_sweeper(paths);
    }
    finish_result?;
    post_cargo_result?;
    strip_outcome.into_result()?;
    if status.success() && prepared.dylint_entrypoint {
        if let Some(plan) = prepared.dylint_plan.as_ref() {
            crate::dylint_toolchain::write_success_marker(plan)?;
        }
    }
    if !status.success() {
        if let Some(retried) = maybe_retry_without_zthreads(
            captured_stderr_for_diagnosis.as_deref(),
            &setup.zthreads_retry_context,
            normalized.explicit_toolchain.as_deref(),
        ) {
            return retried;
        }
    }
    emit_final_reports(&status, prepared, session.started_at_ms, porcelain_report.as_ref());
    drop(normalized);
    Ok(status.code().unwrap_or(1))
}

/// The run's last words: the `cargo install` notice (soldr#3960), then the
/// single porcelain JSON line, emitted last (after every early `?` and after
/// the retry decision) so the caller always sees it exactly once.
fn emit_final_reports(
    status: &std::process::ExitStatus,
    prepared: &PreparedCargo,
    started_at_ms: i64,
    porcelain_report: Option<&porcelain::PorcelainReport>,
) {
    if status.success() {
        install_notice::report(
            &prepared.args,
            &prepared.command,
            started_at_ms,
            prepared.porcelain,
        );
    }
    if let Some(report) = porcelain_report {
        porcelain::emit(report);
    }
}

/// After a successful cargo run, embed packed DWARF and refresh the `cargo
/// run` trampoline sidecar; after a failed one, prune orphan `.rmeta`s.
fn publish_cargo_outputs(
    status: &std::process::ExitStatus,
    strip_outcome: &strip_diagnostics::StripOutcome,
    cargo_artifact_paths: Option<&[String]>,
    prepared: &PreparedCargo,
    trampoline_plan: Option<&FellThroughPlan>,
) -> Result<(), SoldrError> {
    if status.success() && strip_outcome.permits_artifact_publication() {
        if let Some(paths) = cargo_artifact_paths {
            darwin_embed::embed_packed_dwarf_for_artifacts(
                prepared
                    .cache_plan
                    .target_dir_for_hooks(&prepared.args)
                    .as_deref(),
                paths,
            )?;
        }
        if let Some(plan) = trampoline_plan {
            refresh_sidecar_after_cargo(plan);
        }
    } else if !status.success() {
        // A non-zero cargo exit can leave orphan `.rmeta` files (rmeta
        // emitted, then rustc aborted before the `.rlib` codegen pass)
        // in `target/<triple>/<profile>/deps/`. Subsequent invocations
        // then fail with `E0463: can't find crate` because cargo passes
        // `--extern X=orphan.rmeta` to dependents and rustc cannot link
        // an rmeta-only crate. Sweep them so the next build rebuilds
        // cleanly. See soldr#410.
        if let Some(target_dir) = prepared.cache_plan.target_dir_for_hooks(&prepared.args) {
            orphan_rmeta::prune_orphan_rmetas_after_failed_build(&target_dir);
        }
    }
    Ok(())
}

/// After cargo fails, look at whatever stderr we captured for a
/// recognizable build-script-spawn-ENOENT pattern (#422 — minimal
/// Rust containers without a host C toolchain). The capture
/// source is the diagnostic-tail buffer. TTY users captured
/// nothing — they see cargo's own error untouched and skip this
/// path.
fn diagnose_cargo_failure(
    captured_stderr: Option<&str>,
    cargo_job_budget: Option<&job_budget::AppliedCargoJobBudget>,
) {
    let Some(stderr_text) = captured_stderr else {
        return;
    };
    if let Some(diagnostic) =
        cargo_job_budget.and_then(|budget| budget.diagnose_failure(stderr_text))
    {
        eprintln!("{diagnostic}");
    }
    if let Some(diag) = crate::cargo_diagnostics::detect_build_script_failure(stderr_text) {
        let rendered = crate::cargo_diagnostics::render_diagnosis(&diag);
        let stderr = std::io::stderr();
        let _ = stderr.lock().write_all(rendered.as_bytes());
    }
}

/// A failed build whose `-Zthreads` diagnostic matches retries once without
/// the flag on a stable toolchain; otherwise the config hint is printed.
/// `None` means no retry happened.
fn maybe_retry_without_zthreads(
    captured_stderr: Option<&str>,
    zthreads_retry_context: &ZthreadsRetryContext,
    explicit_toolchain: Option<&str>,
) -> Option<Result<i32, SoldrError>> {
    if let Some(plan) = zthreads_fallback::plan_from_environment() {
        if zthreads_fallback::diagnostic_matches(captured_stderr.unwrap_or_default())
            && !resolved_toolchain_is_nightly(explicit_toolchain)
        {
            emit_zthreads_fallback_warning(&plan.value);
            return Some(retry_zthreads_without_flag(
                zthreads_retry_context,
                explicit_toolchain,
                &plan,
            ));
        }
    } else if !env_flag_truthy(zthreads_fallback::ATTEMPTED_ENV)
        && zthreads_fallback::diagnostic_matches(captured_stderr.unwrap_or_default())
    {
        eprintln!("{}", zthreads_fallback::render_config_hint());
    }
    None
}
