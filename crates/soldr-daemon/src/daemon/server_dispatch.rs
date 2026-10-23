async fn handle_connection<S>(
    mut stream: S,
    mut control: ControlContext,
    peer: crate::daemon::ipc_peer::PeerIdentity,
) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use running_process::broker::backend_sdk::MuxPoll;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    let mux = soldr_backend_endpoint_mux(control.daemon_identity.clone());
    let mut prefix = [0_u8; CONTROL_FRAME_HEADER_BYTES];
    if !matches!(
        timeout(HANDSHAKE_READ_TIMEOUT, stream.read_exact(&mut prefix)).await,
        Ok(Ok(_))
    ) {
        return Ok(());
    }
    let mut buffered = prefix.to_vec();
    loop {
        match mux.poll(&buffered) {
            Ok(MuxPoll::Legacy) => break,
            Ok(MuxPoll::NeedMoreBytes) => {
                let mut chunk = [0_u8; 4096];
                let read = match timeout(HANDSHAKE_READ_TIMEOUT, stream.read(&mut chunk)).await {
                    Ok(Ok(0)) | Err(_) => return Ok(()),
                    Ok(Ok(n)) => n,
                    Ok(Err(_)) => return Ok(()),
                };
                buffered.extend_from_slice(&chunk[..read]);
            }
            Ok(MuxPoll::ProbeAnswered { reply, .. }) => {
                // A probe is how a route is judged live, so it still waits for
                // `State`: soldr#3169 moves receipt earlier, not readiness.
                if control.state().await.is_none() {
                    return Ok(());
                }
                let _ = timeout(HANDSHAKE_READ_TIMEOUT, stream.write_all(&reply)).await;
                let _ = timeout(HANDSHAKE_READ_TIMEOUT, stream.flush()).await;
                return Ok(());
            }
            Ok(MuxPoll::Payload { .. }) | Err(_) => {
                // #1853: never drop a socket with the peer's bytes still
                // queued — see `drain_then_close`.
                drain_then_close(&mut stream).await;
                return Ok(());
            }
        }
    }

    // #1853: check the peer's version explicitly rather than letting the
    // decode fail. A version mismatch is a known, diagnosable condition, and
    // reporting it as an opaque transport reset is what made this cost a day
    // to attribute downstream.
    if buffered.len() >= CONTROL_FRAME_HEADER_BYTES {
        let peer_version = u32::from_le_bytes(
            buffered[4..CONTROL_FRAME_HEADER_BYTES]
                .try_into()
                .expect("slice of CONTROL_FRAME_HEADER_BYTES-4 bytes is always 4 bytes wide"),
        );
        if peer_version != PROTOCOL_VERSION {
            reject_version_mismatch(&mut stream, peer_version).await;
            return Ok(());
        }
    }

    let req: Request = match read_frame_async_with_prefix(&mut stream, &buffered).await {
        Ok(r) => r,
        Err(error) => {
            tracing::debug!(%error, "soldr-daemon: dropping undecodable IPC frame");
            drain_then_close(&mut stream).await;
            return Ok(());
        }
    };
    // soldr#2558: fire-and-forget requests acknowledge RECEIPT before anything
    // else. On macOS/BSD a connection the client closes before this server
    // accepts it is discarded together with its buffered frame, so pure
    // write-then-close lost every touch that raced the accept loop; the client
    // now holds the connection until this ack (bounded). soldr#3169: receipt
    // needs no state, so it also precedes waiting for `State` -- a touch that
    // arrives during bringup is acknowledged instead of timing out the bound.
    // A client that already closed makes this write fail, which is fine: its
    // frame was received, which is all the ack exists to prove.
    if matches!(
        req,
        Request::RecordTargetTouch { .. } | Request::CookTouch { .. }
    ) {
        let _ = write_frame_async(&mut stream, &Response::Ack).await;
    }
    if !request_is_cancel_safe(&req) {
        return dispatch_request(req, &mut control, peer, &mut stream).await;
    }
    // soldr#3839: a read-only request whose client hangs up is abandoned
    // instead of holding its task and socket until a wedged handler returns.
    // Clients never write after the request frame (they never half-close), so
    // any read completing here -- EOF or stray bytes -- ends the request.
    let (mut reader, writer) = tokio::io::split(stream);
    let mut writer = tokio::io::join(tokio::io::empty(), writer);
    let mut probe = [0_u8; 1];
    tokio::select! {
        result = dispatch_request(req, &mut control, peer, &mut writer) => result,
        _ = reader.read(&mut probe) => {
            tracing::debug!("soldr-daemon: client hung up; abandoning read-only request");
            Ok(())
        }
    }
}

/// Requests that only read daemon state, so dropping their handler when the
/// client hangs up loses nothing (soldr#3839). Mutating, fire-and-forget and
/// lease requests always run to completion.
fn request_is_cancel_safe(req: &Request) -> bool {
    matches!(
        req,
        Request::ListTargetRegistry
            | Request::Status
            | Request::CompileStats
            | Request::BuildLogInputs { .. }
            | Request::ListBuilds { .. }
            | Request::ListSlowBuilds { .. }
    )
}

#[expect(clippy::cognitive_complexity, reason = "baseline, zackees/ci.yml#229")]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
async fn dispatch_request<S>(
    req: Request,
    control: &mut ControlContext,
    peer: crate::daemon::ipc_peer::PeerIdentity,
    mut stream: &mut S,
) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncReadExt;
    let Some(state) = control.state().await else {
        return Ok(());
    };
    state.request_count.fetch_add(1, Ordering::Relaxed);
    state.touch_activity();
    match req {
        Request::ListTargetRegistry => {
            let db_path = state.db_path.clone();
            let response = tokio::task::spawn_blocking(move || {
                TargetRegistry::open(&db_path)
                    .and_then(|registry| registry.list())
                    .map(|rows| {
                        Response::TargetRegistryRows(
                            rows.into_iter()
                                .map(|row| crate::daemon::protocol::TargetRegistryRow {
                                    path: row.path.display().to_string(),
                                    last_used: row.last_used,
                                })
                                .collect(),
                        )
                    })
                    .unwrap_or_else(|error| {
                        Response::Error(format!("list target registry: {error}"))
                    })
            })
            .await
            .unwrap_or_else(|error| Response::Error(format!("list target registry task: {error}")));
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::RemoveTargetRegistry { paths } => {
            let db_path = state.db_path.clone();
            let response = tokio::task::spawn_blocking(move || {
                let paths = paths
                    .into_iter()
                    .map(std::path::PathBuf::from)
                    .collect::<Vec<_>>();
                TargetRegistry::open(&db_path)
                    .and_then(|registry| registry.remove_many(&paths))
                    .map(|removed| Response::TargetRegistryRemoved {
                        removed: removed as u32,
                    })
                    .unwrap_or_else(|error| {
                        Response::Error(format!("remove target registry rows: {error}"))
                    })
            })
            .await
            .unwrap_or_else(|error| {
                Response::Error(format!("remove target registry task: {error}"))
            });
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::AcquireResidentCapacity { permits } => {
            if permits == 0 {
                let _ = write_frame_async(
                    &mut stream,
                    &Response::Error(
                        "resident capacity requires at least one permit".to_string(),
                    ),
                )
                .await;
                return Ok(());
            }

            // The embedded service owns the canonical compile-capacity
            // semaphore. Retaining this opaque guard in the connection task
            // makes both explicit release and transport disconnect release the
            // same permits with no polling or daemon-global lease registry.
            // The semaphore is FIFO, so a waiting multi-permit request holds
            // every later single-permit compile behind it. A client sends
            // nothing until it is granted, so any read completing here means
            // it hung up (or broke protocol): leave the queue at once rather
            // than blocking compiles for a request nobody will use (soldr#3836
            // audit).
            let acquire = state.compile_service.acquire_resident_capacity(permits);
            let mut probe = [0_u8; 1];
            let acquired = tokio::select! {
                acquired = acquire => acquired,
                _ = stream.read(&mut probe) => {
                    tracing::debug!(permits, "resident-capacity client left while queued");
                    return Ok(());
                }
            };
            let resident_capacity = match acquired {
                Ok(capacity) => capacity,
                Err(error) => {
                    let _ = write_frame_async(
                        &mut stream,
                        &Response::Error(format!(
                            "acquire resident compile capacity: {error}"
                        )),
                    )
                    .await;
                    return Ok(());
                }
            };

            // `Ok(false)` (lease declined) and `Err(_)` (transport failure)
            // are both "nothing to record"; only a granted lease counts as a
            // served request. Written as `if let` because `-D warnings`
            // promotes clippy::single_match to an error.
            if let Ok(true) =
                serve_resident_capacity_lease(&mut stream, permits, resident_capacity).await
            {
                state.request_count.fetch_add(1, Ordering::Relaxed);
                state.touch_activity();
            }
        }
        Request::ReleaseResidentCapacity => {
            let _ = write_frame_async(
                &mut stream,
                &Response::Error(
                    "ReleaseResidentCapacity requires an active lease on this connection"
                        .to_string(),
                ),
            )
            .await;
        }
        Request::RecordTargetTouch { path, unix_seconds } => {
            // Receipt was acknowledged before dispatch (soldr#2558, soldr#3169).
            // Fire-and-forget for the WRITE half: errors are silent by
            // design and the client never learns the outcome.
            //
            // soldr#2224: on the blocking pool. This request arrives once
            // per rustc invocation, and a contended open waits seconds —
            // exactly the stall that must not land on a tokio worker.
            //
            // The open is retried briefly: a concurrent writer holding the
            // store used to make a single silent attempt drop the touch
            // permanently. Waiting out a transient holder keeps the write;
            // a wedged holder still ends the attempt silently at the
            // deadline, unchanged semantics.
            let db_path = state.db_path.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    match TargetRegistry::open(&db_path) {
                        Ok(registry) => {
                            // Silent to the CLIENT (fire-and-forget), but a
                            // failed write is worth one daemon-stderr line:
                            // an every-time failure here looked like a plain
                            // missing row and took several blind rounds to
                            // localize on the darwin target-run lanes.
                            if let Err(error) =
                                registry.upsert_with_time(Path::new(&path), unix_seconds)
                            {
                                eprintln!(
                                    "soldr-daemon: target-touch upsert failed for {path}: {error}"
                                );
                            }
                            return;
                        }
                        Err(error) => {
                            if std::time::Instant::now() >= deadline {
                                eprintln!(
                                    "soldr-daemon: target-touch dropped; could not open {} after retries: {error}",
                                    db_path.display()
                                );
                                return;
                            }
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            })
            .await;
        }
        Request::Status => {
            let info = state.status();
            let _ = write_frame_async(&mut stream, &Response::Status(info)).await;
        }
        Request::Shutdown => {
            peer.record_shutdown_requested(&state.paths, state.daemon_identity.started_at_unix_ms);
            let _ = write_frame_async(
                &mut stream,
                &Response::ShuttingDown(ShutdownAck {
                    pid: std::process::id(),
                    generation: state.daemon_identity.started_at_unix_ms,
                }),
            )
            .await;
            state.shutdown.request();
        }
        Request::FlushCaches => {
            // Issue #1286 (F1): checkpoint the embedded zccache state
            // (artifact index, depgraph snapshot, metadata cache) to
            // disk without shutting down. `soldr save` / `soldr cache
            // flush` call this before archiving — otherwise the state
            // is memory-only until a graceful daemon exit and archives
            // taken from a live daemon restore with zero rustc hits.
            // soldr#3288/#3290: an event-batcher flush failure used to
            // short-circuit here with `Response::Error` and skip the
            // zccache checkpoint below entirely — so a momentary
            // contention blip on a *diagnostics* table (`daemon_events`)
            // could fail a build whose cargo run had already finished
            // successfully. Event rows are reconstructible bookkeeping,
            // not the thing this request exists to persist, and
            // `flush_batch` already retains them on failure so the next
            // flush retries the same rows (`event_batcher.rs`). The
            // identical flush reached via `BuildSessionEnd`
            // (`cargo_front_door/run.rs`) is already explicitly
            // best-effort and never fails the build over this; this path
            // now matches that severity. The response is determined only
            // by whether the zccache checkpoint itself succeeded.
            if let Err(err) = state.event_batcher.flush().await {
                tracing::warn!(
                    "event persistence flush failed (non-fatal; rows retained for the next \
                     flush): {err}"
                );
            }
            let response = match state.compile_service.flush().await {
                Ok(report) => Response::CacheFlushed(report),
                Err(err) => Response::Error(format!("embedded zccache flush failed: {err}")),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::CompileStats => {
            // soldr#1368: return the embedded zccache service's cumulative
            // compile counters so `soldr session start/end` can diff two
            // snapshots into per-session hit/miss stats.
            let response = match state.compile_service.stats().await {
                Ok(info) => Response::CompileStats(info),
                Err(err) => Response::Error(format!("embedded zccache stats failed: {err}")),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::BuildSessionStart {
            session_id,
            repo_root,
            started_at_ms,
        } => {
            // The cargo front door owns an OS-held BuildActivityLease before
            // sending this request and retains it through history
            // publication. Do not mirror that lease in the daemon: this
            // request has no durable connection, so a crashed client could
            // otherwise block maintenance for the daemon's whole lifetime.
            let response = async {
                // soldr#2224: read + merge + write under ONE handle, on the
                // blocking pool. Two path-taking calls here were two
                // exclusive-lock acquisitions on a tokio worker thread.
                let repo_root = repo_root.clone();
                db_async::with_handle(&state.db_path, move |db| {
                    let existing = db::get_build_in(db, session_id)?;
                    let record =
                        merge_build_session_start(existing, session_id, repo_root, started_at_ms);
                    db::upsert_build_in(db, &record)
                })
                .await
                .map_err(|err| format!("persist build session start: {err}"))?;
                // L4 (issue soldr#980): route the SessionStart event through
                // the batcher so we don't compete with the build's first
                // compile-event burst for the redb writer.
                state
                    .event_batcher
                    .record(db::Event {
                        ts_ms: started_at_ms,
                        session_id: Some(session_id),
                        kind: db::EventKind::SessionStart,
                        crate_name: None,
                        duration_us: None,
                        target_dir: None,
                        exit_code: None,
                    })
                    .await
                    .map_err(|err| format!("queue session start event: {err}"))?;
                state
                    .event_batcher
                    .flush()
                    .await
                    .map_err(|err| format!("flush session start event: {err}"))?;
                Ok::<(), String>(())
            }
            .await;
            let response = match response {
                // soldr#2023: the ack reports the limit this daemon is
                // running with, so the front door can warn on drift.
                Ok(()) => crate::compile_limit::build_session_started(
                    state.compile_service.applied_jobs(),
                ),
                Err(err) => Response::Error(err),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::BuildSessionEnd {
            session_id,
            exit_code,
            ended_at_ms,
        } => {
            let response = match finalize_build_session(
                &state.db_path,
                &state.event_batcher,
                session_id,
                exit_code,
                ended_at_ms,
            )
            .await
            {
                Ok(()) => Response::Ack,
                Err(err) => Response::Error(err),
            };
            // soldr#1536: acknowledge the finalization. When the wrapper
            // sees this Ack, the BuildRecord aggregate is persisted and
            // every staged session event is durable in redb, so it can
            // skip its own full-table aggregate re-scan.
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::BuildLogInputs { session_id } => {
            // soldr#1814 slice 2a: the daemon owns these tables, so it answers
            // both reads in one round trip. The CLI previously opened
            // state.sqlite3 twice per build to get them.
            // soldr#2224: one handle for both reads, on the blocking pool.
            let inputs = db_async::with_handle(&state.db_path, move |db| {
                let events = db::list_events_for_session_in(db, session_id)?;
                // A missing row is normal, not an error — the log renders
                // without it. Only a genuine read failure is reported.
                let record = db::get_build_in(db, session_id).ok().flatten();
                Ok((events, record))
            })
            .await;
            let response = match inputs {
                Ok((events, record)) => Response::BuildLogInputs {
                    events,
                    record: record.map(Box::new),
                },
                Err(err) => Response::Error(format!("build log inputs: {err}")),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::AttachBuildLogHistory(update) => {
            // soldr#1814 slice 2d. The whole get/mutate/upsert runs here, under
            // the daemon's own ownership of the table, so two processes cannot
            // interleave a read and a write and lose each other's fields.
            //
            // The merge deliberately reproduces the semantics the CLI-side code
            // had: `ended_at_ms` / `exit_code` keep an already-recorded value
            // rather than being overwritten (first writer wins), and the
            // crate-count aggregate is only recomputed when the client says
            // `BuildSessionEnd` did not already finalize it (soldr#1536).
            let response = db_async::with_handle(&state.db_path, move |db| {
                Ok(attach_build_log_history(db, &update))
            })
            .await
            .unwrap_or_else(|err| Response::Error(format!("read build history: {err}")));
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::ShouldWarnCargoDebugDefault { repo_root } => {
            // soldr#1814 slice 2c: the daemon owns state_db's tables, so it
            // performs this read-modify-write (record the repo, prune expired
            // rows) instead of every front-door invocation opening the file.
            let db_path = crate::cache_lib::state_db_path(&state.paths);
            // soldr#2224: `StateDb::open` is the same exclusive `state.sqlite3`
            // open as everything else here, so it belongs on the blocking
            // pool rather than a tokio worker.
            let emit = tokio::task::spawn_blocking(move || {
                crate::cache_lib::state_db::StateDb::open(&db_path)
                    .and_then(|db| {
                        db.should_emit_cargo_debug_default_warning(std::path::Path::new(&repo_root))
                    })
                    // Fail open, matching the pre-#1814 caller: a state-DB
                    // problem must not silently suppress a warning the user
                    // should see.
                    .unwrap_or(true)
            })
            .await
            .unwrap_or(true);
            let _ = write_frame_async(&mut stream, &Response::CargoDebugWarning { emit }).await;
        }
        Request::ListBuilds { limit, since_ms } => {
            let response = match db_async::list_builds(&state.db_path, limit, since_ms).await {
                Ok(rows) => Response::Builds(rows),
                Err(err) => Response::Error(format!("list builds: {err}")),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::ListSlowBuilds {
            threshold_ms,
            limit,
        } => {
            let response =
                match db_async::list_slow_builds(&state.db_path, threshold_ms, limit).await {
                    Ok(rows) => Response::Builds(rows),
                    Err(err) => Response::Error(format!("list slow builds: {err}")),
                };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::CookLookup {
            recipe_hash,
            target_triple,
            profile,
            channel,
            rustc_version,
            origin_url_normalized,
            branch_lineage,
        } => {
            let key = CookKey {
                recipe_hash,
                target_triple,
                profile,
                channel,
                rustc_version,
            };
            // Each lookup uses its own SQLite connection. WAL permits
            // concurrent readers; the gaps between queries can admit a
            // writer, so the advisory drift list is not one atomic snapshot.
            let lookup_state = state.clone();
            let reply = tokio::task::spawn_blocking(move || {
                let state = lookup_state;
                match cook_index::lookup(&state.db_path, &key) {
                    Ok(Some(entry)) => {
                        state.cook_hits_this_session.fetch_add(1, Ordering::Relaxed);
                        let path = cook_artifact_path(&state.paths, &entry.sha256)
                            .display()
                            .to_string();
                        Response::CookHit {
                            sha256: entry.sha256,
                            path,
                            size_bytes: entry.size_bytes,
                            origin_url_normalized: entry.origin_url_normalized,
                            matched_recipe_hash: Some(recipe_hash),
                            exact_recipe_match: true,
                            branch_name: entry.branch_name,
                            compile_duration_ms: entry.compile_duration_ms,
                            save_elapsed_ms: entry.save_elapsed_ms,
                        }
                    }
                    Ok(None) => {
                        match cook_index::lookup_origin_fallback(
                            &state.db_path,
                            &key,
                            origin_url_normalized.as_deref(),
                            &branch_lineage,
                        ) {
                            Ok(Some((matched_key, entry))) => {
                                state.cook_hits_this_session.fetch_add(1, Ordering::Relaxed);
                                let path = cook_artifact_path(&state.paths, &entry.sha256)
                                    .display()
                                    .to_string();
                                Response::CookHit {
                                    sha256: entry.sha256,
                                    path,
                                    size_bytes: entry.size_bytes,
                                    origin_url_normalized: entry.origin_url_normalized,
                                    matched_recipe_hash: Some(matched_key.recipe_hash),
                                    exact_recipe_match: false,
                                    branch_name: entry.branch_name,
                                    compile_duration_ms: entry.compile_duration_ms,
                                    save_elapsed_ms: entry.save_elapsed_ms,
                                }
                            }
                            Ok(None) => {
                                let previous = cook_index::drift_recipe_hashes(
                                    &state.db_path,
                                    &key,
                                    origin_url_normalized.as_deref(),
                                    COOK_DRIFT_LIMIT,
                                )
                                .unwrap_or_default();
                                Response::CookMiss {
                                    previous_origin_recipe_hashes: previous,
                                }
                            }
                            Err(e) => {
                                Response::Error(format!("cook_index fallback lookup failed: {e}"))
                            }
                        }
                    }
                    Err(e) => Response::Error(format!("cook_index lookup failed: {e}")),
                }
            })
            .await
            .unwrap_or_else(|error| Response::Error(format!("cook lookup task failed: {error}")));
            let _ = write_frame_async(&mut stream, &reply).await;
        }
        Request::CookRecord {
            recipe_hash,
            target_triple,
            profile,
            channel,
            rustc_version,
            sha256,
            size_bytes,
            origin_url_normalized,
            branch_name,
            cook_cmd_summary,
            compile_duration_ms,
            save_elapsed_ms,
        } => {
            let key = CookKey {
                recipe_hash,
                target_triple,
                profile,
                channel,
                rustc_version,
            };
            let now_ms = current_unix_ms();
            let entry = CookEntry {
                sha256,
                size_bytes,
                created_unix_ms: now_ms,
                last_used_unix_ms: now_ms,
                origin_url_normalized,
                cook_cmd_summary,
                branch_name,
                compile_duration_ms,
                save_elapsed_ms,
            };
            let db_path = state.db_path.clone();
            let result = tokio::task::spawn_blocking(move || {
                cook_index::upsert(&db_path, &key, &entry)
            })
            .await
            .unwrap_or_else(|error| {
                Err(crate::cache_lib::target_registry::RegistryError::Io(
                    std::io::Error::other(format!("cook record task failed: {error}")),
                ))
            });
            match result {
                Ok(()) => {
                    let _ = write_frame_async(&mut stream, &Response::Ack).await;
                }
                Err(e) => {
                    let _ = write_frame_async(
                        &mut stream,
                        &Response::Error(format!("cook_index upsert failed: {e}")),
                    )
                    .await;
                }
            }
        }
        Request::CookTouch { sha256 } => {
            // Fire-and-forget bump of last_used_unix_ms, its receipt already
            // acknowledged (soldr#3169). Silent on failure — the caller
            // already moved on.
            let db_path = state.db_path.clone();
            let _ = tokio::task::spawn_blocking(move || {
                cook_index::touch(&db_path, &sha256, current_unix_ms())
            })
            .await;
        }
    }
    Ok(())
}

/// Hold an opaque embedded-service capacity guard for the lifetime of one
/// control connection. `Ok(true)` means the peer explicitly released it;
/// `Ok(false)` means it sent a different frame. Transport errors include EOF.
async fn serve_resident_capacity_lease<S, G>(
    stream: &mut S,
    permits: u32,
    resident_capacity: G,
) -> std::io::Result<bool>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    write_frame_async(
        &mut *stream,
        &Response::ResidentCapacityAcquired { permits },
    )
    .await?;

    match crate::daemon::ipc::read_frame_async::<_, Request>(&mut *stream).await? {
        Request::ReleaseResidentCapacity => {
            // Ack means release is complete, so drop before writing it rather
            // than relying on scope exit after the response is in flight.
            drop(resident_capacity);
            write_frame_async(&mut *stream, &Response::Ack).await?;
            Ok(true)
        }
        other => {
            write_frame_async(
                &mut *stream,
                &Response::Error(format!(
                    "expected ReleaseResidentCapacity, received {other:?}"
                )),
            )
            .await?;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod resident_capacity_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn explicit_release_drops_capacity_before_ack() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let dropped = Arc::new(AtomicBool::new(false));
        let server_dropped = dropped.clone();
        let task = tokio::spawn(async move {
            serve_resident_capacity_lease(&mut server, 2, DropProbe(server_dropped)).await
        });

        assert!(matches!(
            crate::daemon::ipc::read_frame_async::<_, Response>(&mut client)
                .await
                .expect("acquired response"),
            Response::ResidentCapacityAcquired { permits: 2 }
        ));
        assert!(!dropped.load(Ordering::SeqCst));
        crate::daemon::ipc::write_frame_async(
            &mut client,
            &Request::ReleaseResidentCapacity,
        )
        .await
        .expect("release request");
        assert!(matches!(
            crate::daemon::ipc::read_frame_async::<_, Response>(&mut client)
                .await
                .expect("release ack"),
            Response::Ack
        ));
        assert!(dropped.load(Ordering::SeqCst));
        assert!(task.await.expect("server task").expect("serve lease"));
    }

    #[tokio::test]
    async fn disconnect_drops_capacity_without_release() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let dropped = Arc::new(AtomicBool::new(false));
        let server_dropped = dropped.clone();
        let task = tokio::spawn(async move {
            serve_resident_capacity_lease(&mut server, 1, DropProbe(server_dropped)).await
        });

        assert!(matches!(
            crate::daemon::ipc::read_frame_async::<_, Response>(&mut client)
                .await
                .expect("acquired response"),
            Response::ResidentCapacityAcquired { permits: 1 }
        ));
        drop(client);
        assert!(task.await.expect("server task").is_err());
        assert!(dropped.load(Ordering::SeqCst));
    }
}
