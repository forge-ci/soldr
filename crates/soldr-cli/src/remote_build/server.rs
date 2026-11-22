use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as RoutePath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use cap_fs_ext::DirExt;
use cap_std::{ambient_authority, fs::Dir};
use globset::{GlobBuilder, GlobSetBuilder};
use prost::Message;

use crate::platform::{fs::permissions, wait};

use super::manifest;
use super::slots;
use super::snapshot::{digest, relative_path};
use super::wire::{self, Action, Artifact, Artifacts, Blob, Fetch, ResultFrame, Session, Start};

pub(super) async fn serve(
    listen: std::net::SocketAddr,
    root: PathBuf,
    token: &str,
    slot_limit: Option<u32>,
) -> io::Result<i32> {
    if !listen.ip().is_loopback() {
        return Err(io::Error::other(
            "Remote build must bind to loopback. Connect through an SSH tunnel.",
        ));
    }
    fs::create_dir_all(root.join("blobs"))?;
    fs::create_dir_all(root.join("sessions"))?;
    let router = Router::new()
        .route("/v1/{*operation}", post(handle))
        .layer(DefaultBodyLimit::max(wire::MAX_FRAME + 1024 * 1024))
        .with_state((
            root.canonicalize()?,
            digest(format!("Bearer {token}").as_bytes()),
            slot_limit,
        ));
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!(
        "soldr remote listening at http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, router).await?;
    Ok(0)
}

async fn handle(
    State((root, expected, slot_limit)): State<(PathBuf, String, Option<u32>)>,
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
    let provided = headers
        .get("authorization")
        .map(|value| digest(value.as_bytes()));
    if provided.as_ref() != Some(&expected) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match tokio::task::spawn_blocking(move || dispatch(&root, &operation, &body, slot_limit)).await
    {
        Ok(Ok(data)) => (
            StatusCode::OK,
            [("content-type", "application/x-protobuf+zstd")],
            data,
        )
            .into_response(),
        Ok(Err(error)) => (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

fn key(value: &str, len: usize) -> io::Result<&str> {
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
    slot_limit: Option<u32>,
) -> io::Result<Vec<u8>> {
    let parts: Vec<_> = operation.split('/').collect();
    match parts.as_slice() {
        ["start"] => start(root, wire::decode(body)?),
        ["blob", hash] => store_blob(root, hash, wire::decode(body)?),
        ["run", id] => run(
            root,
            &root.join("sessions").join(key(id, 32)?),
            id,
            slot_limit,
        ),
        ["fetch", id] => fetch(
            &root.join("sessions").join(key(id, 32)?),
            wire::decode(body)?,
        ),
        _ => Err(io::Error::other("unknown remote build operation")),
    }
}

fn start(root: &Path, request: Start) -> io::Result<Vec<u8>> {
    if request.commit.len() != 40 && request.commit.len() != 64 {
        return Err(io::Error::other("invalid commit ID"));
    }
    key(&request.commit, request.commit.len())?;
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
    fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::write(directory.join("start.pb"), request.encode_to_vec())?;
    wire::encode(&Session {
        id,
        missing: missing.into_iter().collect(),
        manifest_accepted: manifest.is_some(),
    })
}

fn store_blob(root: &Path, hash: &str, blob: Blob) -> io::Result<Vec<u8>> {
    key(hash, 64)?;
    if digest(&blob.data) != hash {
        return Err(io::Error::other("uploaded blob does not match its digest"));
    }
    let directory = root.join("blobs");
    let mut staging = tempfile::NamedTempFile::new_in(&directory)?;
    staging.write_all(&blob.data)?;
    staging.persist(directory.join(hash))?;
    wire::encode(&Blob::default())
}

fn lock_session(root: &Path, session: &Path) -> io::Result<fs::File> {
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("build.lock"))?;
    wait::lock_exclusive_announced(&lock, wait::LONG_LOCK, "remote build runner")?;
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

fn run(root: &Path, session: &Path, id: &str, slot_limit: Option<u32>) -> io::Result<Vec<u8>> {
    let request = Start::decode(fs::read(session.join("start.pb"))?.as_slice())?;
    let _runner_lock = lock_session(root, session)?;
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
    let source = workspace.join("source");
    let output = session.join("output");
    let build_output = workspace.join("output");
    prepare_sources(
        root,
        session,
        &source,
        &build_output,
        &request,
        lease.is_some(),
    )?;

    let action = Action::try_from(request.action).map_err(io::Error::other)?;
    let arguments = compile_args(action, &request.args, &build_output)?;
    let full_lint = action == Action::Lint && arguments == ["rust", "--check"];
    let action = match action {
        Action::Build => "build",
        Action::Test => "test",
        Action::Dylint => "dylint",
        Action::Lint => "lint",
    };
    let soldr = std::env::current_exe()?;
    let transcript = Transcript::new();
    let result = transcript.run(
        runner_soldr(&soldr, &cache_root)
            .current_dir(&source)
            .env("CARGO_TARGET_DIR", build_output.join("target"))
            .arg(action)
            .args(&arguments),
    )?;
    let exit_code = result.status.code().unwrap_or(1);
    let mut stderr = result.stderr;
    let stderr_before_postprocessing = stderr.len();
    let run_cook = lease.is_some() && exit_code == 0 && full_lint;
    let mut cook_archive_sha256 = None;
    if let Some(lease) = &lease {
        cook_archive_sha256 =
            collect_slot_output(root, lease, &soldr, &output, run_cook, &mut stderr)?;
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
            &output,
            level,
            &mut stderr,
        )
    });
    let attestation = serde_json::to_vec_pretty(&serde_json::json!({
        "schema_version": 1,
        "session": id,
        "commit": request.commit,
        "manifest": manifest,
        "slot": lease.as_ref().map(|lease| lease.slot),
        "action": action,
        "args": request.args,
        "exit_code": exit_code,
        "cache_saved": cache_saved,
        "cook_ready": cook_ready,
        "cook_archive_sha256": cook_archive_sha256,
        "runner": sysinfo::System::host_name(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "source": request.files.iter().map(|file| (&file.path, &file.digest)).collect::<std::collections::BTreeMap<_, _>>(),
    }))?;
    let result_code = match (exit_code, cache_saved, cook_ready) {
        (0, Some(false), _) | (0, _, Some(false)) => 1,
        _ => exit_code,
    };
    if let Some(lease) = &lease {
        lease.finish(Some(result_code))?;
    }
    let result = ResultFrame {
        exit_code: result_code,
        stdout: result.stdout,
        stderr,
        attestation,
    };
    transcript.record_stderr(&result.stderr[stderr_before_postprocessing..]);
    publish_result(&output, &result, &transcript)?;
    wire::encode(&result)
}

fn publish_result(output: &Path, result: &ResultFrame, transcript: &Transcript) -> io::Result<()> {
    let mut logs = std::io::BufWriter::new(fs::File::create(output.join("stdout-stderr.jsonl"))?);
    transcript.write_to(&mut logs)?;
    serde_json::to_writer(&mut logs, &serde_json::json!({"return": result.exit_code}))?;
    logs.write_all(b"\n")?;
    logs.flush()?;

    // Fetch uses the atomically published attestation as its readiness signal.
    let mut staging = tempfile::NamedTempFile::new_in(output)?;
    staging.write_all(&result.attestation)?;
    staging.persist(output.join("attestation.json"))?;
    Ok(())
}

struct Transcript {
    start: std::time::Instant,
    lines: std::sync::Arc<crate::platform::sync::Mutex<Vec<OutputLine>>>,
}

#[derive(serde::Serialize)]
struct OutputLine {
    t: f64,
    #[serde(rename = "type")]
    kind: char,
    v: String,
}

impl Transcript {
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            lines: std::sync::Arc::new(crate::platform::sync::Mutex::new(Vec::new())),
        }
    }

    fn run(&self, command: &mut Command) -> io::Result<std::process::Output> {
        let lines = self.lines.clone();
        let start = self.start;
        wait::output_for_observed(command, wait::WHOLE_COMMAND, move |is_stderr, line| {
            lines.lock().push(OutputLine::new(
                start,
                if is_stderr { 'e' } else { 'o' },
                line,
            ));
        })
    }

    fn record_stderr(&self, bytes: &[u8]) {
        self.lines.lock().extend(
            bytes
                .split_inclusive(|&byte| byte == b'\n')
                .map(|line| OutputLine::new(self.start, 'e', line)),
        );
    }

    fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        let lines = std::mem::take(&mut *self.lines.lock());
        for line in lines {
            serde_json::to_writer(&mut *writer, &line)?;
            writer.write_all(b"\n")?;
        }
        Ok(())
    }
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

fn collect_slot_output(
    root: &Path,
    lease: &slots::Lease,
    soldr: &Path,
    output: &Path,
    run_cook: bool,
    stderr: &mut Vec<u8>,
) -> io::Result<Option<String>> {
    fs::create_dir(output)?;
    slots::snapshot_output(&lease.workspace.join("output"), output)?;
    let hash = if run_cook {
        warm_cook(
            root,
            lease,
            soldr,
            &root.join("compiler"),
            &lease.workspace.join("source"),
            stderr,
        )
    } else {
        None
    };
    if let Some(hash) = &hash {
        let archive = root.join("cook").join(format!("{hash}.tar.zst"));
        slots::reflink(&archive, &output.join("cook.tar.zst"))?;
    }
    Ok(hash)
}

fn warm_cook(
    root: &Path,
    lease: &slots::Lease,
    soldr: &Path,
    cache_root: &Path,
    source: &Path,
    stderr: &mut Vec<u8>,
) -> Option<String> {
    use crate::cache_lib::cook_archive;

    let run = || -> io::Result<String> {
        let recipe = cook_archive::compute_recipe_hash_proxy(source)
            .ok_or_else(|| io::Error::other("cannot identify the workspace cook recipe"))?;
        let recipe = hex::encode(recipe);
        let archives = root.join("cook");
        let marker = lease.workspace.join(format!("cook-{recipe}"));
        if let Ok(hash) = fs::read_to_string(&marker) {
            if key(&hash, 64).is_ok() && archives.join(format!("{hash}.tar.zst")).is_file() {
                return Ok(hash);
            }
        }
        let target = lease.workspace.join("cook-target");
        let result = wait::output_for(
            runner_soldr(soldr, cache_root)
                .current_dir(source)
                .env("CARGO_TARGET_DIR", &target)
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
        fs::create_dir_all(&archives)?;
        let packed = cook_archive::pack_cook_archive(&target.join("debug"), &archives)
            .map_err(io::Error::other)?;
        let hash = hex::encode(packed.sha256);
        fs::write(marker, &hash)?;
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
    source: &Path,
    build_output: &Path,
    request: &Start,
    slotted: bool,
) -> io::Result<()> {
    if source.exists() {
        fs::rename(source, session.join("previous-source"))?;
    }
    fs::create_dir(source)?;
    fs::create_dir_all(build_output)?;
    materialize(root, source, request, slotted)
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
    let cache = wait::output_for(
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

fn runner_soldr(executable: &Path, cache_root: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .env(soldr_core::self_relocate::SELF_SPAWN_EDGE_ENV_VAR, "1")
        .env("SOLDR_CACHE_DIR", cache_root);
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

fn fetch(session: &Path, request: Fetch) -> io::Result<Vec<u8>> {
    let output = session.join("output");
    if !output.join("attestation.json").is_file() {
        return Err(io::Error::other("remote build session has not finished"));
    }
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
    for entry in jwalk::WalkDir::new(&output).skip_hidden(false) {
        let entry = entry.map_err(io::Error::other)?;
        if !entry.file_type().is_file() {
            continue;
        }
        let absolute = entry.path();
        let path = absolute.strip_prefix(&output).map_err(io::Error::other)?;
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
mod start_tests {
    use super::*;

    #[test]
    fn every_session_persists_its_manifest_before_source_upload() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("sessions")).unwrap();
        let commit = "a".repeat(40);
        let populated = serde_json::json!({
            "github": {"url": "https://github.com/forge-ci/soldr"},
            "user_id": 1234,
            "parent-commit-hash": "b".repeat(40),
            "last-commit-hash": commit,
        });
        for (value, accepted) in [(serde_json::Value::Null, false), (populated, true)] {
            let response = start(
                root.path(),
                Start {
                    commit: commit.clone(),
                    manifest_json: accepted.then(|| serde_json::to_vec(&value).unwrap()),
                    ..Start::default()
                },
            )
            .unwrap();
            let session: Session = wire::decode(&response).unwrap();
            assert_eq!(session.manifest_accepted, accepted);
            let path = root
                .path()
                .join("sessions")
                .join(session.id)
                .join("manifest.json");
            let stored: serde_json::Value =
                serde_json::from_reader(fs::File::open(path).unwrap()).unwrap();
            assert_eq!(stored, value);
        }
    }
}
