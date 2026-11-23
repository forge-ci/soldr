//! Indexed, bounded-memory artifact transfers over independent HTTP/2 streams.

use std::fs::{self, File};
use std::io::{self, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::response::{IntoResponse, Response};
use futures_util::{stream, TryStreamExt};
use globset::{GlobBuilder, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use super::snapshot::relative_path;
use super::wire::{self, Blob, Fetch};
use super::Client;
use crate::platform::fs::permissions;
use crate::platform::wait::block_on_handle_for;

const PARALLEL_DOWNLOADS: usize = 8;
const COMPRESSION_LEVEL: i32 = 9;
const CHUNK_QUEUE: usize = 4;
const IO_IDLE: Duration = Duration::from_secs(120);

#[derive(Deserialize, Serialize)]
struct Entry {
    path: String,
    size_bytes: u64,
    sha256: String,
    executable: bool,
    #[serde(default = "default_mime_type")]
    mime_type: String,
}

fn default_mime_type() -> String {
    "application/octet-stream".into()
}

#[derive(Deserialize, Serialize)]
struct Inventory {
    max_parallel: usize,
    files: Vec<Entry>,
}

pub(super) fn inventory(
    output: &Path,
    request: Fetch,
    artifact_streams: u8,
) -> io::Result<Vec<u8>> {
    wire::encode(&Blob {
        data: inventory_json(output, request, artifact_streams)?,
    })
}

pub(super) fn inventory_json(
    output: &Path,
    request: Fetch,
    artifact_streams: u8,
) -> io::Result<Vec<u8>> {
    let mut builder = GlobSetBuilder::new();
    for glob in request.globs {
        builder.add(
            GlobBuilder::new(&glob)
                .literal_separator(true)
                .build()
                .map_err(io::Error::other)?,
        );
    }
    let globs = builder.build().map_err(io::Error::other)?;
    let mut entries = Vec::new();
    for entry in jwalk::WalkDir::new(output).skip_hidden(false) {
        let entry = entry.map_err(io::Error::other)?;
        if !entry.file_type().is_file() {
            continue;
        }
        let absolute = entry.path();
        let path = absolute.strip_prefix(output).map_err(io::Error::other)?;
        if !globs.is_match(path) {
            continue;
        }
        let path = path
            .iter()
            .map(|part| {
                part.to_str()
                    .ok_or_else(|| io::Error::other("non-UTF-8 artifact path"))
            })
            .collect::<io::Result<Vec<_>>>()?
            .join("/");
        let (size_bytes, sha256) = checksum(open_artifact(output, &path)?)?;
        entries.push(Entry {
            mime_type: artifact_mime_type(&absolute).into(),
            path,
            size_bytes,
            sha256,
            executable: permissions::mode(&absolute).is_some_and(|mode| mode & 0o111 != 0),
        });
    }
    entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(serde_json::to_vec(&Inventory {
        max_parallel: usize::from(artifact_streams),
        files: entries,
    })?)
}

pub(super) fn checksum(file: File) -> io::Result<(u64, String)> {
    let mut hash = Sha256::new();
    let size = io::copy(&mut io::BufReader::new(file), &mut hash)?;
    Ok((size, hex::encode(hash.finalize())))
}

pub(super) fn manifest_path(session: &str) -> String {
    format!("/v1/sessions/{session}/artifacts.json")
}

fn artifact_mime_type(path: &Path) -> &'static str {
    let extension = path.extension().and_then(|suffix| suffix.to_str());
    match extension.map(str::to_ascii_lowercase).as_deref() {
        Some("json") => "application/json",
        Some("jsonl" | "ndjson") => "application/x-ndjson",
        Some("zst") => "application/zstd",
        Some("gz") => "application/gzip",
        Some("zip") => "application/zip",
        Some("tar") => "application/x-tar",
        Some("txt" | "log") => "text/plain",
        Some("html" | "htm") => "text/html",
        Some("xml") => "application/xml",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn open_artifact(output: &Path, requested: &str) -> io::Result<File> {
    let path = output.join(relative_path(requested)?);
    if !fs::symlink_metadata(&path)?.is_file() || path.canonicalize()? != path {
        return Err(io::Error::other(
            "remote artifact must be a regular file inside the snapshot",
        ));
    }
    File::open(path)
}

pub(super) fn serve(output: &Path, body: &[u8]) -> io::Result<Response> {
    let request: Blob = wire::decode(body)?;
    let path = std::str::from_utf8(&request.data).map_err(io::Error::other)?;
    serve_file(output, path, true, true)
}

pub(super) fn serve_file(
    output: &Path,
    path: &str,
    compress: bool,
    legacy: bool,
) -> io::Result<Response> {
    let mime_type = artifact_mime_type(Path::new(path));
    let mut file = open_artifact(output, path)?;
    let (tx, rx) = mpsc::channel(CHUNK_QUEUE);
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let writer = ChunkWriter {
            tx: &tx,
            runtime: &runtime,
        };
        let result = if compress {
            zstd::stream::copy_encode(file, &writer, COMPRESSION_LEVEL)
        } else {
            io::copy(&mut file, &mut &writer).map(|_| ())
        };
        if let Err(error) = result {
            let _ = writer.send(Err(error));
        }
    });
    let body = Body::from_stream(stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|chunk| (chunk, rx))
    }));
    if legacy {
        return Ok((
            [
                ("content-type", "application/zstd"),
                ("x-soldr-artifact-content-type", mime_type),
            ],
            body,
        )
            .into_response());
    }
    let mut response = (
        [("content-type", mime_type), ("vary", "accept-encoding")],
        body,
    )
        .into_response();
    if compress {
        response.headers_mut().insert(
            "content-encoding",
            axum::http::HeaderValue::from_static("zstd"),
        );
    }
    Ok(response)
}

struct ChunkWriter<'a> {
    tx: &'a mpsc::Sender<io::Result<Bytes>>,
    runtime: &'a tokio::runtime::Handle,
}

impl ChunkWriter<'_> {
    fn send(&self, chunk: io::Result<Bytes>) -> io::Result<()> {
        block_on_handle_for(self.runtime, self.tx.send(chunk), IO_IDLE)?
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }
}

impl Write for &ChunkWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.send(Ok(Bytes::copy_from_slice(bytes)))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) async fn download(
    client: &Client,
    session: &str,
    out: &Path,
    globs: Vec<String>,
) -> io::Result<()> {
    if fs::symlink_metadata(out).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "artifact destination already exists",
        ));
    }
    let globs = serde_json::to_string(&globs)?;
    let list = client
        .http
        .get(
            &manifest_path(session),
            &client.token,
            &[("globs", &globs)],
            wire::MAX_FRAME,
            "identity",
        )
        .await?;
    let list: Inventory = serde_json::from_slice(&list)?;
    let parallel = list.max_parallel.clamp(1, PARALLEL_DOWNLOADS);
    let parent = out
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging = Arc::new(tempfile::tempdir_in(parent)?);
    stream::iter(list.files.into_iter().map(Ok))
        .try_for_each_concurrent(parallel, |entry| {
            download_one(client, session, Arc::clone(&staging), entry, false)
        })
        .await?;
    fs::rename(staging.path(), out)
}

pub(super) async fn download_test_executable(
    client: &Client,
    session: &str,
    staging: Arc<tempfile::TempDir>,
    path: String,
    size_bytes: u64,
    sha256: String,
) -> io::Result<PathBuf> {
    let staged = staging.path().join(relative_path(&path)?);
    let executable = staging
        .path()
        .join(crate::platform::executable::name::native("test"));
    download_one(
        client,
        session,
        staging,
        Entry {
            path,
            size_bytes,
            sha256,
            executable: true,
            mime_type: default_mime_type(),
        },
        true,
    )
    .await?;
    tokio::fs::rename(staged, &executable).await?;
    Ok(executable)
}

async fn download_one(
    client: &Client,
    session: &str,
    staging: Arc<tempfile::TempDir>,
    entry: Entry,
    accept_identity: bool,
) -> io::Result<()> {
    let path = staging.path().join(relative_path(&entry.path)?);
    let mut response = tokio::time::timeout(
        IO_IDLE,
        client.http.get_stream(
            &format!("/v1/sessions/{session}/artifact"),
            &client.token,
            &[("path", &entry.path)],
        ),
    )
    .await??;
    let encoding = response
        .headers()
        .get("content-encoding")
        .and_then(|value| value.to_str().ok());
    let compressed = encoding == Some("zstd");
    let identity = accept_identity && matches!(encoding, None | Some("identity"));
    if !response.status().is_success() || !(compressed || identity) {
        return Err(io::Error::other(format!(
            "remote artifact {}: expected a zstd response, got HTTP {}",
            entry.path,
            response.status()
        )));
    }
    let (tx, rx) = mpsc::channel(CHUNK_QUEUE);
    let writer = tokio::task::spawn_blocking(move || {
        let _staging = staging;
        save(path, &entry, rx, compressed)
    });
    let transfer = async {
        while let Some(chunk) = tokio::time::timeout(IO_IDLE, response.chunk())
            .await?
            .map_err(io::Error::other)?
        {
            if tx.send(Ok(chunk)).await.is_err() {
                break;
            }
        }
        Ok::<_, io::Error>(())
    }
    .await;
    if transfer.is_err() {
        let _ = tx
            .send(Err(io::Error::other("remote artifact stream interrupted")))
            .await;
    }
    drop(tx);
    transfer?;
    writer.await.map_err(io::Error::other)?
}

fn save(
    path: PathBuf,
    entry: &Entry,
    rx: mpsc::Receiver<io::Result<Bytes>>,
    compressed: bool,
) -> io::Result<()> {
    let parent = super::server::parent(&path)?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    let chunks = ChunkReader {
        rx,
        current: Cursor::new(Bytes::new()),
        runtime: tokio::runtime::Handle::current(),
    };
    let mut reader: Box<dyn Read> = if compressed {
        Box::new(zstd::stream::read::Decoder::new(chunks)?)
    } else {
        Box::new(chunks)
    };
    let mut hash = Sha256::new();
    let mut size = 0;
    let mut writer = BufWriter::new(file.as_file_mut());
    let mut buf = [0; 64 * 1024];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        if size > entry.size_bytes {
            return Err(io::Error::other("remote artifact exceeds advertised size"));
        }
        hash.update(&buf[..read]);
        writer.write_all(&buf[..read])?;
    }
    writer.flush()?;
    drop(writer);
    if size != entry.size_bytes || hex::encode(hash.finalize()) != entry.sha256 {
        return Err(io::Error::other(format!(
            "remote artifact checksum mismatch: {}",
            entry.path
        )));
    }
    if entry.executable {
        permissions::make_executable(file.path())?;
    }
    file.persist_noclobber(path)?;
    Ok(())
}

struct ChunkReader {
    rx: mpsc::Receiver<io::Result<Bytes>>,
    current: Cursor<Bytes>,
    runtime: tokio::runtime::Handle,
}

impl Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let size = self.current.read(buf)?;
            if size != 0 {
                return Ok(size);
            }
            match block_on_handle_for(&self.runtime, self.rx.recv(), IO_IDLE)? {
                Some(chunk) => self.current = Cursor::new(chunk?),
                None => return Ok(0),
            }
        }
    }
}
