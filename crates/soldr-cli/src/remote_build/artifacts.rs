//! Indexed, bounded-memory artifact transfers over independent HTTP/2 streams.

use std::fs::{self, File};
use std::io::{self, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};
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
        let file = open_artifact(output, &path)?;
        let mut hash = Sha256::new();
        let size_bytes = io::copy(&mut io::BufReader::new(file), &mut hash)?;
        entries.push(Entry {
            path,
            size_bytes,
            sha256: hex::encode(hash.finalize()),
            executable: permissions::mode(&absolute).is_some_and(|mode| mode & 0o111 != 0),
        });
    }
    entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    wire::encode(&Blob {
        data: serde_json::to_vec(&Inventory {
            max_parallel: usize::from(artifact_streams),
            files: entries,
        })?,
    })
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
    let file = open_artifact(output, path)?;
    let (tx, rx) = mpsc::channel(CHUNK_QUEUE);
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let writer = ChunkWriter {
            tx: &tx,
            runtime: &runtime,
        };
        let result = zstd::stream::copy_encode(file, &writer, COMPRESSION_LEVEL);
        if let Err(error) = result {
            let _ = writer.send(Err(error));
        }
    });
    let body = Body::from_stream(stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|chunk| (chunk, rx))
    }));
    Ok(([("content-type", "application/zstd")], body).into_response())
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
    client: &Client<'_>,
    session: &str,
    out: &Path,
    globs: Vec<String>,
) -> io::Result<()> {
    let list: Blob = client
        .post(&format!("artifact-list/{session}"), &Fetch { globs })
        .await?;
    let list: Inventory = serde_json::from_slice(&list.data)?;
    let parallel = list.max_parallel.clamp(1, PARALLEL_DOWNLOADS);
    fs::create_dir(out)?;
    stream::iter(list.files.into_iter().map(Ok))
        .try_for_each_concurrent(parallel, |entry| download_one(client, session, out, entry))
        .await
}

async fn download_one(
    client: &Client<'_>,
    session: &str,
    out: &Path,
    entry: Entry,
) -> io::Result<()> {
    let path = out.join(relative_path(&entry.path)?);
    let request = Blob {
        data: entry.path.as_bytes().to_vec(),
    };
    let mut response = tokio::time::timeout(
        IO_IDLE,
        client.http.post_stream(
            &format!("/v1/artifact/{session}"),
            client.token,
            "application/x-protobuf+zstd",
            wire::encode(&request)?,
        ),
    )
    .await??;
    if !response.status().is_success()
        || response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            != Some("application/zstd")
    {
        return Err(io::Error::other(format!(
            "remote artifact {}: expected a zstd response, got HTTP {}",
            entry.path,
            response.status()
        )));
    }
    let (tx, rx) = mpsc::channel(CHUNK_QUEUE);
    let writer = tokio::task::spawn_blocking(move || save(path, &entry, rx));
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

fn save(path: PathBuf, entry: &Entry, rx: mpsc::Receiver<io::Result<Bytes>>) -> io::Result<()> {
    let parent = super::server::parent(&path)?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    let mut decoder = zstd::stream::read::Decoder::new(ChunkReader {
        rx,
        current: Cursor::new(Bytes::new()),
        runtime: tokio::runtime::Handle::current(),
    })?;
    let mut hash = Sha256::new();
    let mut size = 0;
    let mut writer = BufWriter::new(file.as_file_mut());
    let mut buf = [0; 64 * 1024];
    loop {
        let read = decoder.read(&mut buf)?;
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
