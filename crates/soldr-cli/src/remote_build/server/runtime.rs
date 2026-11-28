use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::response::{IntoResponse, Response};

use super::{artifacts, key};
use crate::remote_build::wire::{self, Blob, TestRuntimeReply, TestRuntimeResult};

pub(super) fn serve(
    root: &Path,
    session: &str,
    path: &str,
    compress: bool,
) -> io::Result<Response> {
    if path == "pending.json" {
        let pending = read_pending(root, session)?;
        return Ok((
            [("content-type", "application/json")],
            serde_json::to_vec(&pending)?,
        )
            .into_response());
    }
    let invocation = path
        .strip_suffix("/executable")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown test runtime artifact"))?;
    let directory = request_directory(root, session, invocation)?;
    artifacts::serve_file(&directory, "executable", compress, false)
}

fn read_pending(
    root: &Path,
    session: &str,
) -> io::Result<std::collections::BTreeMap<String, serde_json::Value>> {
    let directory = session_directory(root, session)?;
    let entries = match fs::read_dir(directory) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut pending = std::collections::BTreeMap::new();
    for entry in entries.into_iter().flatten() {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        let request = entry.path();
        if key(&id, 32).is_err() || request.join("result").is_file() {
            continue;
        }
        match fs::read(request.join("request.json")) {
            Ok(bytes) => {
                pending.insert(id, serde_json::from_slice(&bytes)?);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(pending)
}

pub(super) fn store_result(
    root: &Path,
    session: &str,
    reply: TestRuntimeReply,
) -> io::Result<Vec<u8>> {
    let directory = request_directory(root, session, &reply.invocation)?;
    let result = reply
        .result
        .ok_or_else(|| io::Error::other("missing test runtime result"))?;
    write_result(&directory, &wire::encode(&result)?)?;
    wire::encode(&Blob::default())
}

fn session_directory(root: &Path, session: &str) -> io::Result<PathBuf> {
    Ok(root
        .join("sessions")
        .join(key(session, 32)?)
        .join("test-runtime"))
}

fn request_directory(root: &Path, session: &str, invocation: &str) -> io::Result<PathBuf> {
    let directory = session_directory(root, session)?.join(key(invocation, 32)?);
    if !directory.join("request.json").symlink_metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "unknown remote test invocation",
        ));
    }
    Ok(directory)
}

fn write_result(directory: &Path, result: &[u8]) -> io::Result<()> {
    let path = directory.join("result");
    let mut staging = tempfile::NamedTempFile::new_in(directory)?;
    staging.write_all(result)?;
    match staging.persist_noclobber(&path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            if std::fs::read(path)? == result {
                Ok(())
            } else {
                Err(io::Error::other(
                    "remote test invocation already has a different result",
                ))
            }
        }
        Err(error) => Err(error.error),
    }
}

#[derive(serde::Serialize)]
struct Request<'a> {
    cwd: &'a Path,
    args: &'a [String],
    sha256: String,
    size: u64,
    merged_doctest: bool,
    package_environment: std::collections::BTreeMap<String, String>,
}

pub(super) fn configure_runner(
    arguments: &mut Vec<String>,
    soldr: &Path,
    source: &Path,
    requests: &Path,
) -> io::Result<()> {
    let host = crate::core::TargetTriple::host()
        .map_err(io::Error::other)?
        .triple();
    let path = |value: &Path| {
        value
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other("non-UTF-8 test runner path"))
    };
    let runner = [
        path(soldr)?,
        "remote".into(),
        "test-runtime".into(),
        "--source".into(),
        path(source)?,
        "--requests".into(),
        path(requests)?,
        "--".into(),
    ];
    fs::create_dir_all(requests)?;
    let config = format!(
        "target.{host}.runner = {}",
        toml::Value::try_from(runner).map_err(io::Error::other)?
    );
    arguments.splice(0..0, ["--config".into(), config]);
    Ok(())
}

pub(in crate::remote_build) async fn run_test_program(
    source: &Path,
    requests: &Path,
    argv: &[String],
) -> io::Result<i32> {
    let (executable, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::other("missing test executable"))?;
    let source = source.canonicalize()?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let cwd = cwd.strip_prefix(&source).map_err(io::Error::other)?;
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let directory = requests.join(crate::core::hex_lower(&random));
    fs::create_dir(&directory)?;
    let staged = directory.join("executable");
    fs::copy(executable, &staged)?;
    let (size, sha256) = artifacts::checksum(fs::File::open(staged)?)?;
    let request = Request {
        cwd,
        args,
        sha256,
        size,
        package_environment: crate::remote_build::test_runtime_env::capture(&source),
        merged_doctest: Path::new(executable)
            .file_stem()
            .is_some_and(|stem| stem == "rust_out"),
    };
    let mut metadata = tempfile::NamedTempFile::new_in(&directory)?;
    serde_json::to_writer(&mut metadata, &request)?;
    metadata.persist(directory.join("request.json"))?;
    let result = await_result(&directory.join("result")).await?;
    crate::exit_guard::mark_spoke();
    io::stdout().write_all(&result.stdout)?;
    io::stderr().write_all(&result.stderr)?;
    Ok(result.code)
}

async fn await_result(path: &Path) -> io::Result<TestRuntimeResult> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1800);
    tokio::time::timeout_at(deadline, async {
        loop {
            match tokio::fs::read(path).await {
                Ok(bytes) => return wire::decode(&bytes),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "local test result timed out"))?
}
