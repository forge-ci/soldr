//! Strict-validation gate for serving a cached clang PCH (#1952).
//!
//! A cached PCH is only correct in the current workspace when every input
//! header matches what the PCH recorded: spelling, size, content and mtime.
//! clang's `#pragma once` lookup keys on (size, mtime), so a byte-identical
//! header with a newer mtime makes a served PCH load and then fail with
//! `redefinition of ...`, even under `-fpch-validate-input-files-content`.
//!
//! Before a PCH hit is materialized this gate reads the cached PCH's
//! `INPUT_FILE` records and checks them against the workspace. A missing
//! input, a size or content change, or a PCH whose own header is another
//! file (another checkout) turns the hit into a miss, so the PCH is rebuilt.
//! An mtime-only mismatch on a content-verified input under the request's key
//! root is repaired by setting the header back to the recorded mtime
//! (`ZCCACHE_PCH_MTIME_REPLAY`, on by default). Every mtime set is logged.
//!
//! The returned guard serializes PCH serving within the daemon, and the
//! caller holds it until the PCH file has been written, so replay and
//! materialization complete before the compile request returns: a build tool
//! cannot start a consumer between the two.

use super::super::*;
use super::cached_hit::CachedHitFailure;
use crate::daemon_core::fingerprint::pch_inputs::{
    apply_pch_replay, inputs_stat_match, parse_pch_inputs, plan_pch_serve, read_pch_inputs,
    PchInputsError, PchServeOptions,
};

/// Serializes PCH gate + materialization across concurrent hits.
static PCH_SERVE_LOCK: crate::sync::Mutex<()> = crate::sync::Mutex::new(());

/// Whether `output` names a precompiled header (clang writes `CPCH` to both
/// `.pch` and `.gch`; a GCC `.gch` is recognised by content and passed).
pub(super) fn is_pch_output(output: &Path) -> bool {
    matches!(
        output.extension().and_then(|ext| ext.to_str()),
        Some("pch" | "gch")
    )
}

pub(super) struct PchGateRequest<'a> {
    pub(super) state: &'a SharedState,
    pub(super) sid: &'a SessionId,
    pub(super) payload: &'a CachedPayload,
    pub(super) source_path: &'a NormalizedPath,
    pub(super) output_path: &'a NormalizedPath,
    pub(super) replay_root: Option<&'a NormalizedPath>,
    pub(super) replay_enabled: bool,
}

/// Validate a cached PCH payload before it is served. `Ok(guard)` means
/// serve (holding the guard until the output is written); `Err` means
/// treat the hit as a miss.
pub(super) fn gate_cached_pch(
    request: PchGateRequest<'_>,
) -> Result<crate::sync::MutexGuard<'static, ()>, CachedHitFailure> {
    let PchGateRequest {
        state,
        sid,
        payload,
        source_path,
        output_path,
        replay_root,
        replay_enabled,
    } = request;
    let guard = PCH_SERVE_LOCK.lock();
    let log = |line: String| write_session_log(&state.sessions, sid, &line);
    let parsed = match payload {
        CachedPayload::Bytes(bytes) => parse_pch_inputs(bytes),
        CachedPayload::File(path) => read_pch_inputs(path),
    };
    let inputs = match parsed {
        Ok(Some(inputs)) => inputs,
        // Not a clang PCH (e.g. a GCC `.gch`): nothing recorded to check.
        Ok(None) => return Ok(guard),
        Err(error) => {
            log(format!(
                "[PCH_REFUSED] {} unreadable_records: {error}",
                output_path.display()
            ));
            return Err(match error {
                PchInputsError::Io(_) => CachedHitFailure::CacheRead,
                _ => CachedHitFailure::PchInputsStale,
            });
        }
    };
    let root = replay_root
        .map(NormalizedPath::as_path)
        .or_else(|| source_path.parent())
        .unwrap_or_else(|| source_path.as_path());
    let plan = plan_pch_serve(
        &inputs,
        PchServeOptions {
            replay_root: root,
            expected_top_level: Some(source_path.as_path()),
            replay_enabled,
        },
    )
    .map_err(|refusal| {
        log(format!(
            "[PCH_REFUSED] {} input {refusal}",
            output_path.display()
        ));
        tracing::info!(pch = %output_path.display(), %refusal, "cached PCH refused");
        CachedHitFailure::PchInputsStale
    })?;
    if plan.replays.is_empty() {
        return Ok(guard);
    }
    let applied = apply_pch_replay(&plan).map_err(|refusal| {
        log(format!(
            "[PCH_REFUSED] {} replay {refusal}",
            output_path.display()
        ));
        tracing::warn!(pch = %output_path.display(), %refusal, "PCH input mtime replay failed");
        CachedHitFailure::PchInputsStale
    })?;
    for replay in &applied {
        log(format!(
            "[PCH_MTIME_REPLAY] {} mtime {} -> {} (content verified) for {}",
            replay.path.display(),
            replay.current_secs,
            replay.recorded_secs,
            output_path.display()
        ));
        tracing::info!(
            input = %replay.path.display(),
            from = replay.current_secs,
            to = replay.recorded_secs,
            pch = %output_path.display(),
            "PCH input mtime replayed"
        );
    }
    Ok(guard)
}

/// The PCH binaries a compile loads: every `-include-pch` / `.gch` force
/// include, plus the `<header>.pch` clang substitutes for `-include <header>`.
fn loaded_pchs(force_includes: &[NormalizedPath]) -> Vec<NormalizedPath> {
    let mut pchs: Vec<NormalizedPath> = Vec::new();
    for path in force_includes {
        let candidate = if is_pch_output(path) {
            path.clone()
        } else {
            let mut implicit = path.as_os_str().to_owned();
            implicit.push(".pch");
            NormalizedPath::new(Path::new(&implicit))
        };
        if candidate.is_file() && !pchs.contains(&candidate) {
            pchs.push(candidate);
        }
    }
    pchs
}

/// Repair the inputs of every PCH a compile is about to load (#1952).
///
/// A PCH already on disk breaks exactly like a served one when an identical
/// header's mtime moved (checkout, cache restore, `touch`), and a build tool
/// that thinks the PCH is current never asks zccache to rebuild it. So before
/// clang runs, each loaded PCH's records are checked: a stat-only pass when
/// nothing drifted, else the same content-verified replay as a served PCH.
/// A real change is left alone for clang's own validation to reject; this
/// never fails the compile.
pub(in crate::daemon_core::daemon::server) fn repair_loaded_pch_inputs(
    state: &SharedState,
    sid: &SessionId,
    force_includes: &[NormalizedPath],
    replay_root: &Path,
    client_env: Option<&[(String, String)]>,
) {
    if force_includes.is_empty() || !replay_enabled_for(client_env) {
        return;
    }
    for pch in loaded_pchs(force_includes) {
        let Ok(Some(inputs)) = read_pch_inputs(&pch) else {
            continue;
        };
        if inputs_stat_match(&inputs) {
            continue;
        }
        let _guard = PCH_SERVE_LOCK.lock();
        let log = |line: String| write_session_log(&state.sessions, sid, &line);
        let outcome = plan_pch_serve(
            &inputs,
            PchServeOptions {
                replay_root,
                expected_top_level: None,
                replay_enabled: true,
            },
        )
        .and_then(|plan| apply_pch_replay(&plan));
        match outcome {
            Ok(applied) => {
                for replay in &applied {
                    log(format!(
                        "[PCH_MTIME_REPLAY] {} mtime {} -> {} (content verified) for loaded {}",
                        replay.path.display(),
                        replay.current_secs,
                        replay.recorded_secs,
                        pch.display()
                    ));
                }
            }
            Err(refusal) => log(format!(
                "[PCH_NOT_REPAIRED] loaded {} input {refusal}",
                pch.display()
            )),
        }
    }
}

/// `ZCCACHE_PCH_MTIME_REPLAY` from the requesting client's environment.
pub(super) fn replay_enabled_for(client_env: Option<&[(String, String)]>) -> bool {
    use crate::daemon_core::fingerprint::pch_inputs::{pch_mtime_replay_enabled, PCH_MTIME_REPLAY_ENV};
    pch_mtime_replay_enabled(client_env.and_then(|env| {
        env.iter()
            .find_map(|(key, value)| (key == PCH_MTIME_REPLAY_ENV).then_some(value.as_str()))
    }))
}

/// Key a PCH generation on its exact invocation (#1952): a PCH embeds its
/// inputs' spellings, so the key takes the cwd and every argument in order,
/// as spelled, not the normalized flag set ordinary objects share.
pub(super) fn salt_pch_key(
    ctx: &mut crate::daemon_core::depgraph::CompileContext,
    source_mode: crate::daemon_core::compiler::SourceMode,
    cwd: &Path,
    args: &[String],
) {
    if matches!(source_mode, crate::daemon_core::compiler::SourceMode::Header) {
        ctx.unknown_flags.push(exact_invocation_key_flag(cwd, args));
    }
}

/// Cache-key material: the working directory and every argument in order.
fn exact_invocation_key_flag(cwd: &Path, args: &[String]) -> String {
    let mut material = Vec::new();
    material.extend_from_slice(cwd.to_string_lossy().as_bytes());
    for arg in args {
        material.push(0);
        material.extend_from_slice(arg.as_bytes());
    }
    format!(
        "zccache-pch-invocation={}",
        crate::daemon_core::hash::hash_bytes(&material).to_hex()
    )
}
