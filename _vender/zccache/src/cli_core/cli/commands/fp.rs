//! `zccache fp` subcommands — fingerprint check / mark / invalidate.

use std::path::Path;
use std::process::ExitCode;

use crate::cli_core::cli::runtime::ensure_daemon;

pub(crate) async fn cmd_fp_check(
    endpoint: &str,
    cache_file: &Path,
    cache_type: &str,
    root: &Path,
    ext: &[String],
    include: &[String],
    exclude: &[String],
) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache fp: failed to start daemon: {e}");
        return ExitCode::from(2);
    }

    let request = crate::cli_core::protocol::Request::FingerprintCheck {
        cache_file: cache_file.into(),
        cache_type: cache_type.to_string(),
        root: root.into(),
        extensions: ext.to_vec(),
        include_globs: include.to_vec(),
        exclude: exclude.to_vec(),
    };

    match crate::cli_core::ipc::full_family_roundtrip(endpoint, &request, None).await {
        Ok(Some(crate::cli_core::protocol::Response::FingerprintCheckResult {
            decision,
            reason,
            changed_files,
        })) => {
            if decision == "skip" {
                eprintln!("zccache fp: skip (no changes)");
                ExitCode::from(1)
            } else {
                let reason_str = reason.as_deref().unwrap_or("unknown");
                if changed_files.is_empty() {
                    eprintln!("zccache fp: run ({reason_str})");
                } else {
                    eprintln!(
                        "zccache fp: run ({reason_str}, {} file(s) changed)",
                        changed_files.len()
                    );
                }
                ExitCode::SUCCESS
            }
        }
        Ok(Some(crate::cli_core::protocol::Response::Error { message })) => {
            eprintln!("zccache fp: daemon error: {message}");
            ExitCode::from(2)
        }
        Ok(other) => {
            eprintln!("zccache fp: unexpected response: {other:?}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("zccache fp: recv error: {e}");
            ExitCode::from(2)
        }
    }
}

pub(crate) async fn cmd_fp_mark(endpoint: &str, cache_file: &Path, success: bool) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache fp: failed to start daemon: {e}");
        return ExitCode::from(2);
    }

    let request = if success {
        crate::cli_core::protocol::Request::FingerprintMarkSuccess {
            cache_file: cache_file.into(),
        }
    } else {
        crate::cli_core::protocol::Request::FingerprintMarkFailure {
            cache_file: cache_file.into(),
        }
    };

    match crate::cli_core::ipc::full_family_roundtrip(endpoint, &request, None).await {
        Ok(Some(crate::cli_core::protocol::Response::FingerprintAck)) => {
            let label = if success {
                "mark-success"
            } else {
                "mark-failure"
            };
            eprintln!("zccache fp: {label}");
            ExitCode::SUCCESS
        }
        Ok(Some(crate::cli_core::protocol::Response::Error { message })) => {
            eprintln!("zccache fp: daemon error: {message}");
            ExitCode::from(2)
        }
        Ok(other) => {
            eprintln!("zccache fp: unexpected response: {other:?}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("zccache fp: recv error: {e}");
            ExitCode::from(2)
        }
    }
}

pub(crate) async fn cmd_fp_invalidate(endpoint: &str, cache_file: &Path) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache fp: failed to start daemon: {e}");
        return ExitCode::from(2);
    }

    let request = crate::cli_core::protocol::Request::FingerprintInvalidate {
        cache_file: cache_file.into(),
    };

    match crate::cli_core::ipc::full_family_roundtrip(endpoint, &request, None).await {
        Ok(Some(crate::cli_core::protocol::Response::FingerprintAck)) => {
            eprintln!("zccache fp: invalidated");
            ExitCode::SUCCESS
        }
        Ok(Some(crate::cli_core::protocol::Response::Error { message })) => {
            eprintln!("zccache fp: daemon error: {message}");
            ExitCode::from(2)
        }
        Ok(other) => {
            eprintln!("zccache fp: unexpected response: {other:?}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("zccache fp: recv error: {e}");
            ExitCode::from(2)
        }
    }
}
