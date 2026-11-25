use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{
    download_attestation, find_worker, finish_client_timing, start_worktree, uses_direct_run,
    BuildMode, Client,
};
use crate::remote_build::{artifacts, control, snapshot, test_runtime_env, wire};

#[derive(Deserialize)]
struct Request {
    cwd: String,
    args: Vec<String>,
    sha256: String,
    size: u64,
    merged_doctest: bool,
    #[serde(default)]
    package_environment: BTreeMap<String, String>,
}

pub(super) async fn try_relay(
    args: &[String],
    compile: &[String],
    cache_enabled: bool,
) -> io::Result<Option<i32>> {
    let started = Instant::now();
    if [
        "RUSTDOC",
        "RUSTDOCFLAGS",
        "CARGO_ENCODED_RUSTDOCFLAGS",
        "CARGO_BUILD_RUSTDOCFLAGS",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some())
    {
        return Ok(None);
    }
    let mode = BuildMode::CargoTestRuntime;
    let Some((client, root, work_dir)) = find_worker(compile, cache_enabled, mode).await? else {
        return Ok(None);
    };
    if !uses_direct_run(&std::env::current_dir()?, &root) {
        return Ok(None);
    }
    let Some(session) = start_worktree(&client, &root, work_dir, args, mode).await? else {
        return Ok(None);
    };
    eprintln!("Forge CI build session {}", session.id);

    let result = tokio::select! {
        result = control::receive(&client, &session.id) => result,
        result = relay_requests(&client, &root, &session.id) => result,
    };
    let code = match result {
        Err(error) if control::terminal_blocked(&error) => return Ok(Some(1)),
        result => result?,
    };
    download_attestation(&client, &root, &session.id).await?;
    let attestation_ms = started.elapsed().as_millis() as u64;
    finish_client_timing(&root, &session.id, started, attestation_ms, false);
    Ok(Some(code))
}

async fn relay_requests(client: &Client, root: &Path, session: &str) -> io::Result<i32> {
    let session = crate::remote_build::server::key(session, 32)?;
    loop {
        let pending = fetch(
            client,
            session,
            "test-runtime/pending.json",
            wire::MAX_FRAME,
        )
        .await?;
        let pending: BTreeMap<String, Request> = serde_json::from_slice(&pending)?;
        for (id, request) in pending {
            let id = crate::remote_build::server::key(&id, 32)?;
            let reply = run_request(client, root, session, id, request).await?;
            let _: wire::Blob = client
                .post(
                    &format!("workspace/{session}"),
                    &wire::WorkspaceBatch {
                        blobs: Vec::new(),
                        runtime_reply: Some(wire::TestRuntimeReply {
                            invocation: id.into(),
                            result: Some(reply),
                        }),
                    },
                )
                .await?;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn run_request(
    client: &Client,
    root: &Path,
    session: &str,
    id: &str,
    request: Request,
) -> io::Result<wire::TestRuntimeResult> {
    if request.size == 0 {
        return Err(io::Error::other("invalid remote test executable size"));
    }
    let cwd = if request.cwd.is_empty() {
        root.to_owned()
    } else {
        root.join(snapshot::relative_path(&request.cwd)?)
            .canonicalize()?
    };
    if !cwd.starts_with(root) {
        return Err(io::Error::other(
            "remote test directory escapes the checkout",
        ));
    }
    let staging = Arc::new(tempfile::tempdir_in(root)?);
    let executable = artifacts::download_test_executable(
        client,
        session,
        Arc::clone(&staging),
        format!("test-runtime/{id}/executable"),
        request.size,
        request.sha256,
    )
    .await?;
    let mut command = tokio::process::Command::new(&executable);
    command
        .current_dir(cwd)
        .args(request.args)
        .kill_on_drop(true);
    test_runtime_env::apply(&mut command, root, request.package_environment)?;
    if request.merged_doctest {
        command.env("RUSTDOC_DOCTEST_BIN_PATH", &executable);
    }
    let output = command.output().await?;
    Ok(wire::TestRuntimeResult {
        code: output.status.code().unwrap_or(1),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

async fn fetch(client: &Client, session: &str, path: &str, limit: usize) -> io::Result<Vec<u8>> {
    client
        .http
        .get(
            &format!("/v1/sessions/{session}/artifact"),
            &client.token,
            &[("path", path)],
            limit,
            "identity",
        )
        .await
}
