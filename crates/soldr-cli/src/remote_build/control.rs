//! Live JSON Lines control responses.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

use super::{wire, Client};
use crate::platform::wait::block_on_handle_for;

const MAX_LINE: usize = 16 * 1024 * 1024;
const HEARTBEAT: Duration = Duration::from_secs(15);
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

struct Terminal(mpsc::Sender<TerminalLine>);

struct TerminalLine {
    stderr: bool,
    text: String,
    done: oneshot::Sender<io::Result<()>>,
}

#[derive(Debug)]
struct TerminalBlocked;

impl std::fmt::Display for TerminalBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("local output is blocked")
    }
}

impl std::error::Error for TerminalBlocked {}

pub(super) fn terminal_blocked(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<TerminalBlocked>())
}

impl Terminal {
    fn start() -> Self {
        let (tx, mut rx) = mpsc::channel::<TerminalLine>(1);
        let runtime = tokio::runtime::Handle::current();
        // Keep terminal writes out of Tokio's blocking pool. An external
        // pipe can stay full after a timeout and must not hold runtime exit.
        std::thread::spawn(move || {
            while let Ok(Some(line)) =
                block_on_handle_for(&runtime, rx.recv(), crate::platform::wait::WHOLE_COMMAND)
            {
                let result = if line.stderr {
                    print_line(io::stderr().lock(), &line.text)
                } else {
                    print_line(io::stdout().lock(), &line.text)
                };
                let _ = line.done.send(result);
            }
        });
        Self(tx)
    }

    async fn write(&self, stderr: bool, text: &str) -> io::Result<()> {
        let (done, result) = oneshot::channel();
        tokio::time::timeout(IDLE_TIMEOUT, async {
            self.0
                .send(TerminalLine {
                    stderr,
                    text: text.to_owned(),
                    done,
                })
                .await
                .map_err(io::Error::other)?;
            result.await.map_err(io::Error::other)?
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, TerminalBlocked))?
    }
}

fn print_line(mut output: impl Write, text: &str) -> io::Result<()> {
    output.write_all(text.as_bytes())?;
    output.flush()
}

async fn body_read<T>(future: impl std::future::Future<Output = T>) -> io::Result<T> {
    tokio::time::timeout(IDLE_TIMEOUT, future)
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "remote control stream was idle for 60 seconds",
            )
        })
}

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

    pub(super) fn finish(&self, t: f64, code: i32, session: &str) -> io::Result<()> {
        if self.interrupted.load(Ordering::Relaxed) {
            return Err(io::Error::other(
                "remote control stream could not deliver all output",
            ));
        }
        self.send(&serde_json::json!({"t": t, "return": code}))?;
        self.send(&serde_json::json!({"artifacts": super::artifacts::manifest_path(session)}))
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
    (
        [
            ("content-type", "application/x-ndjson"),
            ("cache-control", "no-store"),
        ],
        heartbeat_body(rx),
    )
        .into_response()
}

fn heartbeat_body(rx: mpsc::Receiver<io::Result<Bytes>>) -> Body {
    let heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    Body::from_stream(stream::unfold(
        (rx, heartbeat),
        |(mut rx, mut heartbeat)| async {
            let chunk = tokio::select! {
                biased;
                chunk = rx.recv() => chunk?,
                _ = heartbeat.tick() => Ok(Bytes::from_static(b"\n")),
            };
            Some((chunk, (rx, heartbeat)))
        },
    ))
}

pub(super) async fn receive(client: &Client<'_>, session: &str) -> io::Result<i32> {
    let mut response = tokio::time::timeout(
        HEADER_TIMEOUT,
        client.http.post_stream(
            &format!("/v1/run-stream/{session}"),
            client.token,
            "application/x-protobuf+zstd",
            wire::encode(&wire::Blob::default())?,
        ),
    )
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "remote control response headers timed out",
        )
    })??;
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
    let terminal = Terminal::start();
    let manifest_path = super::artifacts::manifest_path(session);
    while let Some(chunk) = body_read(response.chunk())
        .await?
        .map_err(io::Error::other)?
    {
        for part in chunk.split_inclusive(|&byte| byte == b'\n') {
            if part.len() > MAX_LINE + 1 - pending.len() {
                return Err(io::Error::other(
                    "remote output record exceeds its size limit",
                ));
            }
            pending.extend_from_slice(part);
            if part.last() == Some(&b'\n') {
                receive_line(
                    &pending,
                    &mut exit,
                    &mut artifacts,
                    &manifest_path,
                    &terminal,
                )
                .await?;
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

async fn receive_line(
    line: &[u8],
    exit: &mut Option<i32>,
    artifacts: &mut bool,
    manifest_path: &str,
    terminal: &Terminal,
) -> io::Result<()> {
    if line == b"\n" {
        return Ok(());
    }
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
    if record.get("artifacts").and_then(serde_json::Value::as_str) == Some(manifest_path) {
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
        Some("o") => terminal.write(false, text).await,
        Some("e") => terminal.write(true, text).await,
        _ => Err(io::Error::other("invalid remote output stream code")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[tokio::test(start_paused = true)]
    async fn quiet_stream_emits_heartbeats_and_closes_with_the_sender() {
        let (sender, receiver) = mpsc::channel(1);
        let mut body = heartbeat_body(receiver).into_data_stream();
        assert_eq!(
            body.next().await.unwrap().unwrap(),
            Bytes::from_static(b"\n")
        );
        let mut exit = None;
        let mut artifacts = false;
        receive_line(
            b"\n",
            &mut exit,
            &mut artifacts,
            "/artifacts.json",
            &Terminal::start(),
        )
        .await
        .unwrap();
        assert_eq!(exit, None);
        assert!(!artifacts);
        drop(sender);
        assert!(body.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_control_body_errors_after_the_idle_limit() {
        let error = body_read(std::future::pending::<()>()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
