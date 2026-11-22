//! Live JSON Lines control responses.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use serde::Serialize;
use tokio::sync::mpsc;

use super::{wire, Client};
use crate::platform::wait::block_on_handle_for;

const MAX_LINE: usize = 16 * 1024 * 1024;

pub(super) struct Sink {
    tx: mpsc::Sender<io::Result<Bytes>>,
    runtime: tokio::runtime::Handle,
    interrupted: AtomicBool,
}

impl Sink {
    pub(super) fn send(&self, record: &impl Serialize) -> io::Result<()> {
        let mut line = serde_json::to_vec(record)?;
        if line.len() > MAX_LINE {
            return Err(io::Error::other(
                "remote output record exceeds its size limit",
            ));
        }
        line.push(b'\n');
        block_on_handle_for(
            &self.runtime,
            self.tx.send(Ok(Bytes::from(line))),
            Duration::from_secs(5),
        )?
        .map_err(io::Error::other)
    }

    pub(super) fn output(&self, record: &impl Serialize) {
        if !self.interrupted.load(Ordering::Relaxed) && self.send(record).is_err() {
            self.interrupted.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn finish(&self, t: f64, code: i32) -> io::Result<()> {
        if self.interrupted.load(Ordering::Relaxed) {
            return Err(io::Error::other(
                "remote control stream could not deliver all output",
            ));
        }
        self.send(&serde_json::json!({"t": t, "return": code}))?;
        self.send(&serde_json::json!({"artifacts": "artifact-list"}))
    }
}

pub(super) fn stream_run(
    run: impl FnOnce(Arc<Sink>) -> io::Result<()> + Send + 'static,
) -> Response {
    let (tx, rx) = mpsc::channel(32);
    let sink = Arc::new(Sink {
        tx,
        runtime: tokio::runtime::Handle::current(),
        interrupted: AtomicBool::new(false),
    });
    tokio::task::spawn_blocking(move || {
        if let Err(error) = run(Arc::clone(&sink)) {
            let _ = sink.send(&serde_json::json!({"error": error.to_string()}));
        }
    });
    let body = Body::from_stream(stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|chunk| (chunk, rx))
    }));
    ([("content-type", "application/x-ndjson")], body).into_response()
}

pub(super) async fn receive(client: &Client<'_>, session: &str) -> io::Result<i32> {
    let mut response = client
        .http
        .post_stream(
            &format!("/v1/run-stream/{session}"),
            client.token,
            "application/x-protobuf+zstd",
            wire::encode(&wire::Blob::default())?,
        )
        .await?;
    if !response.status().is_success()
        || response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            != Some("application/x-ndjson")
    {
        return Err(io::Error::other(format!(
            "remote control stream returned HTTP {}",
            response.status()
        )));
    }

    let mut pending = Vec::new();
    let mut exit = None;
    let mut artifacts = false;
    while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
        for part in chunk.split_inclusive(|&byte| byte == b'\n') {
            if part.len() > MAX_LINE + 1 - pending.len() {
                return Err(io::Error::other(
                    "remote output record exceeds its size limit",
                ));
            }
            pending.extend_from_slice(part);
            if part.last() == Some(&b'\n') {
                receive_line(&pending, &mut exit, &mut artifacts)?;
                pending.clear();
            }
        }
    }
    if !pending.is_empty() || !artifacts {
        return Err(io::Error::other(
            "remote control stream ended before the postamble",
        ));
    }
    exit.ok_or_else(|| io::Error::other("remote control stream omitted the return code"))
}

fn receive_line(line: &[u8], exit: &mut Option<i32>, artifacts: &mut bool) -> io::Result<()> {
    let record: serde_json::Value = serde_json::from_slice(line)?;
    if let Some(error) = record.get("error").and_then(serde_json::Value::as_str) {
        return Err(io::Error::other(error.to_owned()));
    }
    if let Some(code) = record.get("return").and_then(serde_json::Value::as_i64) {
        if exit.is_some() {
            return Err(io::Error::other("duplicate remote return record"));
        }
        *exit = Some(i32::try_from(code).map_err(io::Error::other)?);
        return Ok(());
    }
    if record.get("artifacts").and_then(serde_json::Value::as_str) == Some("artifact-list") {
        if exit.is_none() || *artifacts {
            return Err(io::Error::other("unexpected remote artifact postamble"));
        }
        *artifacts = true;
        return Ok(());
    }
    let text = record
        .get("v")
        .and_then(serde_json::Value::as_str)
        .filter(|_| exit.is_none())
        .ok_or_else(|| io::Error::other("invalid remote output record"))?;
    match record.get("type").and_then(serde_json::Value::as_str) {
        Some("o") => {
            let mut output = io::stdout().lock();
            output.write_all(text.as_bytes())?;
            output.flush()
        }
        Some("e") => {
            let mut output = io::stderr().lock();
            output.write_all(text.as_bytes())?;
            output.flush()
        }
        _ => Err(io::Error::other("invalid remote output stream code")),
    }
}
