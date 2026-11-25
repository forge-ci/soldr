use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path as RoutePath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use cap_fs_ext::DirExt;
use cap_std::{ambient_authority, fs::Dir};
use globset::{GlobBuilder, GlobSetBuilder};
use prost::Message;

use crate::platform::{fs::permissions, wait};

use super::access::SessionAccess;
use super::auth::RunnerAuth;
use super::slots;
use super::snapshot::{digest, relative_path};
use super::wire::{self, Action, Artifact, Artifacts, Blob, Fetch, ResultFrame, Session, Start};
use super::{artifacts, control, manifest, retention};

type RunnerState = (PathBuf, RunnerAuth, Option<u32>, u8, retention::Connection);

pub(super) async fn serve(
    listener: tokio::net::TcpListener,
    root: PathBuf,
    auth: RunnerAuth,
    slot_limit: Option<u32>,
    artifact_streams: u8,
) -> io::Result<i32> {
    fs::create_dir_all(root.join("blobs"))?;
    fs::create_dir_all(root.join("sessions"))?;
    retention::prepare(&root)?;
    retention::maintain(root.canonicalize()?);
    let router = Router::new()
        .route("/v1/{*operation}", post(handle))
        .route("/v1/sessions/{id}/artifacts.json", get(get_inventory))
        .route("/v1/sessions/{id}/artifact", get(get_artifact))
        .layer(DefaultBodyLimit::max(wire::MAX_FRAME + 1024 * 1024))
        .with_state((
            root.canonicalize()?,
            auth,
            slot_limit,
            artifact_streams,
            retention::Connection::default(),
        ));
    eprintln!(
        "soldr remote listening at http://{}",
        listener.local_addr()?
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<retention::Connection>(),
    )
    .await?;
    Ok(0)
}

async fn handle(
    State((root, auth, slot_limit, artifact_streams, worker)): State<RunnerState>,
    ConnectInfo(connection): ConnectInfo<retention::Connection>,
    RoutePath(operation): RoutePath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        != Some("application/x-protobuf+zstd")
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let Some(access) = SessionAccess::authorize(&auth, &headers, operation == "start") else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let connection = access.outputs(connection, worker);
    if let Some(id) = operation.strip_prefix("run-stream/") {
        let Ok(id) = key(id, 32) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let id = id.to_owned();
        let output = match access.allocate_for_stream(connection, &root, &id).await {
            Ok(output) => output,
            Err(error) => return request_error(error),
        };
        return control::stream_run(move |sink| {
            let result = run_session(&root, &id, slot_limit, Some(sink), &output);
            if let Err(error) = &result {
                if !output.path().join("output/attestation.json").is_file() {
                    if let Err(publish_error) = publish_session_failure(&root, &id, error) {
                        eprintln!("soldr remote session {id}: {error}; recording failure: {publish_error}");
                    }
                }
            }
            result.map(|_| ())
        });
    }
    match tokio::task::spawn_blocking(move || {
        access.check_operation(&root, &operation)?;
        if let Some(id) = operation.strip_prefix("run/") {
            let id = key(id, 32)?;
            let output = connection.allocate(&root, id)?;
            let result = run_session(&root, id, slot_limit, None, &output)?;
            return Ok::<_, io::Error>(
                (
                    [("content-type", "application/x-protobuf+zstd")],
                    wire::encode(&result)?,
                )
                    .into_response(),
            );
        }
        if let Some(id) = operation.strip_prefix("artifact/") {
            return artifacts::serve(&finished_output(&root, id, &connection)?.path, &body);
        }
        let data = dispatch(
            &root,
            &operation,
            &body,
            artifact_streams,
            &connection,
            &access,
        )?;
        Ok::<_, io::Error>(
            ([("content-type", "application/x-protobuf+zstd")], data).into_response(),
        )
    })
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => request_error(error),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct InventoryQuery {
    globs: Option<String>,
}

#[derive(serde::Deserialize)]
struct ArtifactQuery {
    path: String,
}

async fn get_inventory(
    State((root, auth, _, limit, worker)): State<RunnerState>,
    ConnectInfo(connection): ConnectInfo<retention::Connection>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<InventoryQuery>,
    headers: HeaderMap,
) -> axum::response::Response {
    let Some(access) = SessionAccess::authorize(&auth, &headers, false) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let connection = access.outputs(connection, worker);
    artifact_response(move || {
        access.check(&root, &id)?;
        let globs = match query.globs {
            Some(globs) => serde_json::from_str(&globs)?,
            None => vec!["**".into()],
        };
        let output = finished_output(&root, &id, &connection)?;
        let data = artifacts::inventory_json(&output.path, Fetch { globs }, limit)?;
        Ok(([("content-type", "application/json")], data).into_response())
    })
    .await
}

async fn get_artifact(
    State((root, auth, _, _, worker)): State<RunnerState>,
    ConnectInfo(connection): ConnectInfo<retention::Connection>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<ArtifactQuery>,
    headers: HeaderMap,
) -> axum::response::Response {
    let Some(access) = SessionAccess::authorize(&auth, &headers, false) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let connection = access.outputs(connection, worker);
    let compress = headers
        .get_all("accept-encoding")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|encoding| {
            let (name, weight) = encoding.split_once(';').unwrap_or((encoding, "q=1"));
            name.trim().eq_ignore_ascii_case("zstd")
                && weight
                    .trim()
                    .strip_prefix("q=")
                    .and_then(|q| q.parse::<f32>().ok())
                    .is_some_and(|q| q > 0.0 && q <= 1.0)
        });
    artifact_response(move || {
        access.check(&root, &id)?;
        if let Some(path) = query.path.strip_prefix("test-runtime/") {
            return runtime::serve(&root, &id, path, compress);
        }
        let output = finished_output(&root, &id, &connection)?;
        artifacts::serve_file(&output.path, &query.path, compress, false)
    })
    .await
}

async fn artifact_response(
    task: impl FnOnce() -> io::Result<axum::response::Response> + Send + 'static,
) -> axum::response::Response {
    match tokio::task::spawn_blocking(task).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => request_error(error),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

fn request_error(error: io::Error) -> axum::response::Response {
    if error.kind() == io::ErrorKind::NotFound {
        StatusCode::NOT_FOUND.into_response()
    } else {
        (StatusCode::BAD_REQUEST, error.to_string()).into_response()
    }
}

mod execution;
mod output;
mod runtime;
pub(super) use runtime::run_test_program;
mod test_outputs;
use output::{finished_output, publish_session_failure};

pub(super) fn key(value: &str, len: usize) -> io::Result<&str> {
    if value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(value)
    } else {
        Err(io::Error::other("invalid remote build identifier"))
    }
}

fn dispatch(
    root: &Path,
    operation: &str,
    body: &[u8],
    artifact_streams: u8,
    connection: &retention::Connection,
    access: &SessionAccess,
) -> io::Result<Vec<u8>> {
    let parts: Vec<_> = operation.split('/').collect();
    match parts.as_slice() {
        ["capabilities"] => wire::encode(&wire::Capabilities::worker()?),
        ["start"] => start(root, wire::decode(body)?, access),
        ["blob", hash] => store_blob(root, hash, wire::decode(body)?),
        ["workspace", id] => store_workspace(root, key(id, 32)?, wire::decode(body)?),
        ["artifact-list", id] => artifacts::inventory(
            &finished_output(root, id, connection)?.path,
            wire::decode(body)?,
            artifact_streams,
        ),
        ["fetch", id] => fetch(
            &finished_output(root, id, connection)?.path,
            wire::decode(body)?,
        ),
        _ => Err(io::Error::other("unknown remote build operation")),
    }
}

fn store_workspace(root: &Path, id: &str, batch: wire::WorkspaceBatch) -> io::Result<Vec<u8>> {
    if let Some(reply) = batch.runtime_reply {
        if !batch.blobs.is_empty() {
            return Err(io::Error::other(
                "a test runtime reply cannot include workspace blobs",
            ));
        }
        return runtime::store_result(root, id, reply);
    }

    let request =
        Start::decode(fs::read(root.join("sessions").join(id).join("start.pb"))?.as_slice())?;
    let expected: std::collections::HashSet<_> = request
        .files
        .iter()
        .map(|file| file.digest.as_str())
        .collect();
    for entry in batch.blobs {
        if !expected.contains(entry.digest.as_str()) {
            return Err(io::Error::other(
                "workspace blob was not declared in the session",
            ));
        }
        persist_blob(root, &entry.digest, &entry.data)?;
    }
    wire::encode(&Blob::default())
}

fn start(root: &Path, request: Start, access: &SessionAccess) -> io::Result<Vec<u8>> {
    if !request.working_tree || !request.commit.is_empty() {
        if request.commit.len() != 40 && request.commit.len() != 64 {
            return Err(io::Error::other("invalid commit ID"));
        }
        key(&request.commit, request.commit.len())?;
    }
    if request.working_tree && request.manifest_json.is_some() {
        return Err(io::Error::other(
            "worktree builds cannot carry a commit manifest",
        ));
    }
    if let Some(directory) = &request.work_dir {
        relative_path(directory)?;
        if !request.working_tree {
            return Err(io::Error::other("work directory requires a worktree build"));
        }
    }
    let manifest = manifest::decode(request.manifest_json.as_deref(), &request.commit)?;
    Action::try_from(request.action).map_err(io::Error::other)?;
    let mut missing = std::collections::BTreeSet::new();
    let paths: std::collections::BTreeSet<_> = request
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    if paths.len() != request.files.len() {
        return Err(io::Error::other("duplicate remote source path"));
    }
    for file in &request.files {
        let path = relative_path(&file.path)?;
        if path
            .ancestors()
            .skip(1)
            .any(|parent| parent.to_str().is_some_and(|path| paths.contains(path)))
        {
            return Err(io::Error::other(
                "remote source path has a file as an ancestor",
            ));
        }
        key(&file.digest, 64)?;
        if !root.join("blobs").join(&file.digest).is_file() {
            missing.insert(file.digest.clone());
        }
    }

    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let id = hex::encode(random);
    let directory = root.join("sessions").join(&id);
    fs::create_dir(&directory)?;
    access.record(&directory)?;
    fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::write(directory.join("start.pb"), request.encode_to_vec())?;
    wire::encode(&Session {
        id,
        missing: missing.into_iter().collect(),
        manifest_accepted: manifest.is_some(),
        working_tree_accepted: request.working_tree,
    })
}

fn store_blob(root: &Path, hash: &str, blob: Blob) -> io::Result<Vec<u8>> {
    persist_blob(root, hash, &blob.data)?;
    wire::encode(&Blob::default())
}

fn persist_blob(root: &Path, hash: &str, data: &[u8]) -> io::Result<()> {
    key(hash, 64)?;
    if digest(data) != hash {
        return Err(io::Error::other("uploaded blob does not match its digest"));
    }
    let directory = root.join("blobs");
    let mut staging = tempfile::NamedTempFile::new_in(&directory)?;
    staging.write_all(data)?;
    staging.persist(directory.join(hash))?;
    Ok(())
}

fn lock_session(root: &Path, session: &Path, request: &Start) -> io::Result<fs::File> {
    if let Some(source) = request
        .files
        .iter()
        .find(|source| !root.join("blobs").join(&source.digest).is_file())
    {
        return Err(io::Error::other(format!(
            "remote workspace is missing blob {}",
            source.digest
        )));
    }
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("build.lock"))?;
    // A private worktree and output directory can share the compiler daemon.
    // Slot allocation and whole-cache exports need the worker to themselves.
    if request.manifest_json.is_some() || request.cache_zstd_level.is_some() {
        wait::lock_exclusive_announced(&lock, wait::LONG_LOCK, "remote build runner")?;
    } else {
        wait::lock_shared_for(&lock, wait::LONG_LOCK)?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(session.join("started"))
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                io::Error::other("remote build session was already started")
            } else {
                error
            }
        })?;
    Ok(lock)
}

fn run_session(
    root: &Path,
    id: &str,
    slot_limit: Option<u32>,
    sink: Option<std::sync::Arc<control::Sink>>,
    archive: &retention::OutputDirectory,
) -> io::Result<ResultFrame> {
    let result = run(
        root,
        &root.join("sessions").join(id),
        id,
        slot_limit,
        sink,
        archive,
    );
    if result.is_err() {
        archive.discard();
    }
    result
}

fn run(
    root: &Path,
    session: &Path,
    id: &str,
    slot_limit: Option<u32>,
    sink: Option<std::sync::Arc<control::Sink>>,
    archive: &retention::OutputDirectory,
) -> io::Result<ResultFrame> {
    let request = Start::decode(fs::read(session.join("start.pb"))?.as_slice())?;
    let _runner_lock = lock_session(root, session, &request)?;
    let cache_root = root.join("compiler");
    let cache_dir = crate::core::SoldrPaths::with_root(cache_root.clone()).cache;
    let manifest = manifest::decode(request.manifest_json.as_deref(), &request.commit)?;
    let lease = manifest
        .as_ref()
        .filter(|_| slot_limit != Some(0))
        .map(|manifest| slots::Lease::acquire(root, id, manifest, slot_limit))
        .transpose()?;
    let workspace = lease
        .as_ref()
        .map_or(session, |lease| lease.workspace.as_path());
    let output = &archive.path().join("output");
    let (source, build_output) =
        prepare_sources(root, session, workspace, output, &request, lease.is_some())?;
    let changes = lease
        .as_ref()
        .map(|lease| slots::OutputChanges::capture(&lease.workspace, &request))
        .transpose()?;

    let soldr = std::env::current_exe()?;
    let transcript = Transcript::new(parent(output)?, sink)?;
    let (result, action, full_lint) = execution::execute_action(
        &soldr,
        &cache_root,
        &source,
        &build_output,
        &request,
        &transcript,
        &session.join("test-runtime"),
    )?;
    let exit_code = result.status.code().unwrap_or(1);
    let mut stderr = result.stderr;
    let stderr_before_postprocessing = stderr.len();
    let run_cook = lease.is_some() && exit_code == 0 && full_lint;
    let mut cook_archive_sha256 = None;
    if let (Some(lease), Some(changes)) = (&lease, changes) {
        fs::create_dir(output)?;
        changes.publish(output, exit_code == 0)?;
        cook_archive_sha256 = cook_slot_output(root, lease, &soldr, output, run_cook, &mut stderr);
    }
    let cook_ready = run_cook.then_some(cook_archive_sha256.is_some());
    fs::write(
        output.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let cache_saved = request.cache_zstd_level.map(|level| {
        save_cache(
            &soldr,
            &cache_root,
            &source,
            &cache_dir,
            output,
            level,
            &mut stderr,
        )
    });
    let result_code = match (exit_code, cache_saved, cook_ready) {
        (0, Some(false), _) | (0, _, Some(false)) => 1,
        _ => exit_code,
    };
    let attestation = serde_json::to_vec_pretty(&serde_json::json!({
        "schema_version": 1,
        "session": id,
        "commit": (!request.working_tree).then_some(&request.commit),
        "base_commit": (request.working_tree && !request.commit.is_empty()).then_some(&request.commit),
        "manifest": manifest,
        "slot": lease.as_ref().map(|lease| lease.slot),
        "action": action,
        "args": request.args,
        "work_dir": request.work_dir,
        "exit_code": result_code,
        "command_exit_code": exit_code,
        "cache_saved": cache_saved,
        "cook_ready": cook_ready,
        "cook_archive_sha256": cook_archive_sha256,
        "runner": sysinfo::System::host_name(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "source": request.files.iter().map(|file| (&file.path, &file.digest)).collect::<std::collections::BTreeMap<_, _>>(),
    }))?;
    let result = ResultFrame {
        exit_code: result_code,
        stdout: result.stdout,
        stderr,
        attestation,
    };
    publish_result(
        archive,
        &result,
        &transcript,
        id,
        lease.as_ref(),
        stderr_before_postprocessing,
    )?;
    Ok(result)
}

fn publish_result(
    archive: &retention::OutputDirectory,
    result: &ResultFrame,
    transcript: &Transcript,
    id: &str,
    lease: Option<&slots::Lease>,
    stderr_offset: usize,
) -> io::Result<()> {
    let output = &archive.path().join("output");
    if let Some(lease) = lease {
        lease.finish(Some(result.exit_code))?;
    }
    transcript.record_stderr(&result.stderr[stderr_offset..])?;
    transcript.publish(output, result.exit_code)?;

    // Fetch uses the atomically published attestation as its readiness signal.
    let mut staging = tempfile::NamedTempFile::new_in(output)?;
    staging.write_all(&result.attestation)?;
    staging.persist(output.join("attestation.json"))?;
    archive.mark_ready();
    if let Some(sink) = &transcript.sink {
        sink.finish(
            transcript.start.elapsed().as_secs_f64(),
            result.exit_code,
            id,
        );
    }
    Ok(())
}

struct Transcript {
    start: std::time::Instant,
    file: tempfile::NamedTempFile,
    writer: std::sync::Arc<crate::platform::sync::Mutex<std::io::BufWriter<fs::File>>>,
    sink: Option<std::sync::Arc<control::Sink>>,
}

#[derive(serde::Serialize)]
struct OutputLine {
    t: f64,
    #[serde(rename = "type")]
    kind: char,
    v: String,
}

impl Transcript {
    fn new(session: &Path, sink: Option<std::sync::Arc<control::Sink>>) -> io::Result<Self> {
        let file = tempfile::NamedTempFile::new_in(session)?;
        let writer = std::io::BufWriter::new(file.reopen()?);
        Ok(Self {
            start: std::time::Instant::now(),
            file,
            writer: std::sync::Arc::new(crate::platform::sync::Mutex::new(writer)),
            sink,
        })
    }

    fn run(&self, command: &mut Command) -> io::Result<std::process::Output> {
        let writer = self.writer.clone();
        let start = self.start;
        let sink = self.sink.clone();
        let capture_limit = self.sink.is_none().then_some(wire::MAX_FRAME / 4);
        wait::output_for_observed(
            command,
            wait::WHOLE_COMMAND,
            capture_limit,
            move |is_stderr, line| {
                let mut writer = writer.lock();
                let record = OutputLine::new(start, if is_stderr { 'e' } else { 'o' }, line);
                write_json_line(&mut *writer, &record)?;
                if let Some(sink) = &sink {
                    sink.output(&record);
                }
                Ok(())
            },
        )
    }

    fn record_stderr(&self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self.writer.lock();
        for record in bytes
            .split_inclusive(|&byte| byte == b'\n')
            .map(|line| OutputLine::new(self.start, 'e', line))
        {
            write_json_line(&mut *writer, &record)?;
            if let Some(sink) = &self.sink {
                sink.output(&record);
            }
        }
        Ok(())
    }

    fn publish(&self, output: &Path, code: i32) -> io::Result<()> {
        let mut writer = self.writer.lock();
        write_json_line(&mut *writer, &serde_json::json!({"return": code}))?;
        writer.flush()?;
        fs::copy(self.file.path(), output.join("stdout-stderr.jsonl"))?;
        Ok(())
    }
}

fn write_json_line(writer: &mut impl Write, record: &impl serde::Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, record)?;
    writer.write_all(b"\n")
}

impl OutputLine {
    fn new(start: std::time::Instant, kind: char, line: &[u8]) -> Self {
        Self {
            t: (start.elapsed().as_secs_f64() * 100.0).floor() / 100.0,
            kind,
            v: String::from_utf8_lossy(line).into_owned(),
        }
    }
}

fn cook_slot_output(
    root: &Path,
    lease: &slots::Lease,
    soldr: &Path,
    output: &Path,
    run_cook: bool,
    stderr: &mut Vec<u8>,
) -> Option<String> {
    if run_cook {
        warm_cook(
            output,
            lease,
            soldr,
            &root.join("compiler"),
            &lease.workspace.join("source"),
            stderr,
        )
    } else {
        None
    }
}

fn warm_cook(
    output: &Path,
    lease: &slots::Lease,
    soldr: &Path,
    cache_root: &Path,
    source: &Path,
    stderr: &mut Vec<u8>,
) -> Option<String> {
    use crate::cache_lib::cook_archive;

    let run = || -> io::Result<String> {
        let archives = tempfile::tempdir_in(output)?;
        let target = tempfile::tempdir_in(&lease.workspace)?;
        let result = bounded_postprocessing(
            runner_soldr(soldr, cache_root)
                .current_dir(source)
                .env("CARGO_TARGET_DIR", target.path())
                .args(["cook", "--workspace", "--no-index"]),
            wait::WHOLE_COMMAND,
        )?;
        if !result.status.success() {
            return Err(io::Error::other(format!(
                "soldr cook exited with {}: {}",
                result.status,
                String::from_utf8_lossy(&result.stderr)
            )));
        }
        let packed = cook_archive::pack_cook_archive(&target.path().join("debug"), archives.path())
            .map_err(io::Error::other)?;
        let hash = hex::encode(packed.sha256);
        slots::reflink(
            &archives.path().join(format!("{hash}.tar.zst")),
            &output.join("cook.tar.zst"),
        )?;
        Ok(hash)
    };
    match run() {
        Ok(hash) => Some(hash),
        Err(error) => {
            stderr.extend_from_slice(
                format!("\nsoldr remote: cook archive unavailable: {error}\n").as_bytes(),
            );
            None
        }
    }
}

fn prepare_sources(
    root: &Path,
    session: &Path,
    workspace: &Path,
    output: &Path,
    request: &Start,
    slotted: bool,
) -> io::Result<(PathBuf, PathBuf)> {
    let source = workspace.join("source");
    let build_output = if slotted {
        workspace.join("output")
    } else {
        output.to_owned()
    };
    if source.exists() {
        fs::rename(&source, session.join("previous-source"))?;
    }
    fs::create_dir(&source)?;
    fs::create_dir_all(&build_output)?;
    materialize(root, &source, request, slotted)?;
    Ok((source, build_output))
}

fn materialize(root: &Path, source: &Path, request: &Start, reflink: bool) -> io::Result<()> {
    for file in request.files.iter().filter(|file| !file.symlink) {
        let path = source.join(relative_path(&file.path)?);
        fs::create_dir_all(parent(&path)?)?;
        let blob = root.join("blobs").join(key(&file.digest, 64)?);
        if reflink {
            slots::reflink(&blob, &path)?;
            if file.executable {
                permissions::make_executable(&path)?;
            }
        } else {
            write_file(&path, &fs::read(blob)?, file.executable)?;
        }
    }
    for file in request.files.iter().filter(|file| file.symlink) {
        let relative = relative_path(&file.path)?;
        let path = source.join(relative);
        fs::create_dir_all(parent(&path)?)?;
        let data = fs::read(root.join("blobs").join(key(&file.digest, 64)?))?;
        create_link(&path, &data)?;
    }

    Ok(())
}

fn save_cache(
    soldr: &Path,
    cache_root: &Path,
    source: &Path,
    cache_dir: &Path,
    output: &Path,
    level: i32,
    stderr: &mut Vec<u8>,
) -> bool {
    let cache = bounded_postprocessing(
        runner_soldr(soldr, cache_root)
            .current_dir(source)
            .args(["save", "--zstd-level"])
            .arg(level.to_string())
            .arg("--cache-dir")
            .arg(cache_dir)
            .arg("--out")
            .arg(output.join("cache.tar.zst")),
        wait::LONG_CHILD,
    );
    match cache {
        Ok(cache) if cache.status.success() => true,
        Ok(cache) => {
            stderr.extend_from_slice(b"\nRemote soldr save failed:\n");
            stderr.extend_from_slice(&cache.stderr);
            false
        }
        Err(error) => {
            stderr.extend_from_slice(format!("\nRemote soldr save failed: {error}\n").as_bytes());
            false
        }
    }
}

fn bounded_postprocessing(
    command: &mut Command,
    timeout: std::time::Duration,
) -> io::Result<std::process::Output> {
    const LIMIT: usize = 16 * 1024;
    let tail = std::sync::Arc::new(crate::platform::sync::Mutex::new(Vec::new()));
    let observed = tail.clone();
    let mut output = wait::output_for_observed(command, timeout, None, move |stderr, chunk| {
        if stderr {
            let chunk = &chunk[chunk.len().saturating_sub(LIMIT)..];
            let mut tail = observed.lock();
            let discard = (tail.len() + chunk.len()).saturating_sub(LIMIT);
            tail.drain(..discard);
            tail.extend_from_slice(chunk);
        }
        Ok(())
    })?;
    output.stderr = std::mem::take(&mut *tail.lock());
    Ok(output)
}

fn runner_soldr(executable: &Path, cache_root: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .env(soldr_core::self_relocate::SELF_SPAWN_EDGE_ENV_VAR, "1")
        .env("SOLDR_CACHE_DIR", cache_root)
        .env(super::auto::WORKER_BUILD_ENV, "1");
    command
}

fn compile_args(action: Action, original: &[String], output: &Path) -> io::Result<Vec<String>> {
    let mut args = original.to_vec();
    if action == Action::Lint {
        if args.is_empty() {
            args.push("rust".into());
        }
        if args.iter().any(|arg| arg == "--attest") {
            return Err(io::Error::other(
                "remote lint uses the remote session attestation; lint --attest requires a Git checkout",
            ));
        }
        if matches!(args[0].as_str(), "rust" | "all") && !args.iter().any(|arg| arg == "--check") {
            args.push("--check".into());
        }
        return Ok(args);
    }
    let mut cargo_separator = action == Action::Dylint;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if argument == "--" {
            if !cargo_separator {
                break;
            }
            cargo_separator = false;
        } else if argument == "--target-dir" {
            index += 1;
            let value = args
                .get_mut(index)
                .ok_or_else(|| io::Error::other("--target-dir requires a relative path"))?;
            *value = output_path(output, value)?;
        } else if let Some(value) = argument.strip_prefix("--target-dir=") {
            args[index] = format!("--target-dir={}", output_path(output, value)?);
        }
        index += 1;
    }
    Ok(args)
}

fn output_path(output: &Path, value: &str) -> io::Result<String> {
    output
        .join(relative_path(value)?)
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::other("remote output path must be UTF-8"))
}

pub(super) fn parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .ok_or_else(|| io::Error::other("missing parent directory"))
}

fn create_link(path: &Path, data: &[u8]) -> io::Result<()> {
    let target = std::str::from_utf8(data).map_err(io::Error::other)?;
    let target = relative_path(target)?;
    let directory = Dir::open_ambient_dir(parent(path)?, ambient_authority())?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("missing remote symlink file name"))?;
    DirExt::symlink(&directory, target, name)
}

pub(super) fn write_file(path: &Path, data: &[u8], executable: bool) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(data)?;
    if executable {
        permissions::make_executable(path)?;
    }
    Ok(())
}

fn fetch(output: &Path, request: Fetch) -> io::Result<Vec<u8>> {
    let mut builder = GlobSetBuilder::new();
    for pattern in request.globs {
        builder.add(
            GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
                .map_err(io::Error::other)?,
        );
    }
    let patterns = builder.build().map_err(io::Error::other)?;
    let mut files = Vec::new();
    let mut size = 0_u64;
    for entry in jwalk::WalkDir::new(output).skip_hidden(false) {
        let entry = entry.map_err(io::Error::other)?;
        if !entry.file_type().is_file() {
            continue;
        }
        let absolute = entry.path();
        let path = absolute.strip_prefix(output).map_err(io::Error::other)?;
        if !patterns.is_match(path) {
            continue;
        }
        let metadata = fs::symlink_metadata(&absolute)?;
        size = size.saturating_add(metadata.len());
        if size > wire::MAX_FRAME as u64 {
            return Err(io::Error::other(
                "remote artifacts exceed 256 MiB. Fetch a narrower glob.",
            ));
        }
        let executable = permissions::mode(&absolute).is_some_and(|mode| mode & 0o111 != 0);
        let path = path
            .components()
            .map(|part| {
                part.as_os_str()
                    .to_str()
                    .ok_or_else(|| io::Error::other("non-UTF-8 artifact path"))
            })
            .collect::<io::Result<Vec<_>>>()?
            .join("/");
        files.push(Artifact {
            path,
            data: fs::read(absolute)?,
            executable,
        });
    }
    wire::encode(&Artifacts { files })
}

#[cfg(test)]
mod start_tests;
