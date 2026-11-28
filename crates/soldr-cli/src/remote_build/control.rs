//! Live JSON Lines control responses.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use serde::Serialize;
use soldr_fetch::RemoteBuildHttpResponse;
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

    pub(super) fn finish(&self, t: f64, code: i32, session: &str) {
        // The durable result has already been published. A disconnected client
        // can fetch it later, so live delivery must not invalidate the result.
        self.output(&serde_json::json!({"t": t, "return": code}));
        self.output(&serde_json::json!({"artifacts": super::artifacts::manifest_path(session)}));
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

pub(super) async fn receive(client: &Client, session: &str) -> io::Result<i32> {
    let response = tokio::time::timeout(
        HEADER_TIMEOUT,
        client.http.post_stream(
            &format!("/v1/run-stream/{session}"),
            &client.token,
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
    if !(200..300).contains(&response.status())
        || response.header("content-type") != Some(b"application/x-ndjson".as_slice())
    {
        return Err(io::Error::other(format!(
            "remote control stream returned HTTP {}",
            response.status()
        )));
    }

    let terminal = Terminal::start();
    let mut progress = Progress::default();
    let manifest_path = super::artifacts::manifest_path(session);
    let result = consume_stream(response, &terminal, &manifest_path, &mut progress, false).await;
    if let Err(error) = result {
        if error.kind() != io::ErrorKind::UnexpectedEof {
            return Err(error);
        }
        terminal
            .write(
                true,
                "Forge CI output stream disconnected. Waiting for the existing build result.\n",
            )
            .await?;
        let transcript = finished_transcript(client, session).await?;
        consume_stream(transcript, &terminal, &manifest_path, &mut progress, true).await?;
    }
    progress
        .exit
        .ok_or_else(|| io::Error::other("remote control stream omitted the return code"))
}

#[derive(Default)]
struct Progress {
    exit: Option<i32>,
    artifacts: bool,
    records: usize,
}

async fn consume_stream(
    mut response: RemoteBuildHttpResponse,
    terminal: &Terminal,
    manifest_path: &str,
    progress: &mut Progress,
    replay: bool,
) -> io::Result<()> {
    let mut pending = Vec::new();
    let mut skip = if replay { progress.records } else { 0 };
    while let Some(chunk) = body_read(response.next_chunk())
        .await
        .map_err(interrupted_stream)?
        .map_err(interrupted_stream)?
    {
        for part in chunk.as_ref().split_inclusive(|&byte| byte == b'\n') {
            if part.len() > MAX_LINE + 1 - pending.len() {
                return Err(io::Error::other(
                    "remote output record exceeds its size limit",
                ));
            }
            pending.extend_from_slice(part);
            if part.last() == Some(&b'\n') {
                if pending != b"\n" {
                    if skip > 0 {
                        skip -= 1;
                    } else {
                        receive_line(
                            &pending,
                            &mut progress.exit,
                            &mut progress.artifacts,
                            manifest_path,
                            terminal,
                        )
                        .await?;
                        if !progress.artifacts {
                            progress.records += 1;
                        }
                    }
                }
                pending.clear();
            }
        }
    }
    if !pending.is_empty() || skip != 0 || !replay && !progress.artifacts {
        return Err(interrupted_stream(
            "remote control stream ended before the result",
        ));
    }
    Ok(())
}

fn interrupted_stream(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, error.to_string())
}

async fn finished_transcript(
    client: &Client,
    session: &str,
) -> io::Result<RemoteBuildHttpResponse> {
    let path = format!("/v1/sessions/{session}/artifact");
    let window = Duration::from_secs(super::auth::SESSION_WINDOW_SECONDS);
    tokio::time::timeout(window, async {
        loop {
            let request = client.http.get_with_encoding(
                &path,
                &client.token,
                &[("path", "stdout-stderr.jsonl")],
                "identity",
            );
            if let Ok(Ok(mut response)) = tokio::time::timeout(HEADER_TIMEOUT, request).await {
                let status = response.status();
                if (200..300).contains(&status) {
                    return Ok(response);
                }
                if !matches!(status, 404 | 502 | 503 | 504) {
                    let detail = body_read(response.next_chunk())
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .flatten();
                    let detail = detail.as_ref().map(AsRef::as_ref).unwrap_or_default();
                    let detail = String::from_utf8_lossy(&detail[..detail.len().min(4096)]);
                    return Err(io::Error::other(format!(
                        "remote transcript request returned HTTP {status}: {detail}"
                    )));
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "remote build result recovery timed out",
        )
    })?
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

    #[tokio::test]
    async fn disconnected_build_recovers_the_existing_transcript() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let endpoint = listener.local_addr().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let launches = calls.clone();
        let router = axum::Router::new()
            .route(
                "/v1/run-stream/test-session",
                axum::routing::post(move || {
                    let launches = launches.clone();
                    async move {
                        launches.fetch_add(1, Ordering::Relaxed);
                        (
                            [("content-type", "application/x-ndjson")],
                            "{\"type\":\"o\",\"v\":\"\"}\n{\"type\":",
                        )
                    }
                }),
            )
            .route(
                "/v1/sessions/test-session/artifact",
                axum::routing::get(|| async { "{\"type\":\"o\",\"v\":\"\"}\n{\"return\":7}\n" }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = Client::new(endpoint, "test".into()).unwrap();
        assert_eq!(receive(&client, "test-session").await.unwrap(), 7);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        server.abort();
    }

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
