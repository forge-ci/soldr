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
use globset::{GlobBuilder, GlobSetBuilder};
use prost::Message;

use super::snapshot::{digest, relative_path};
use super::wire::{self, Action, Artifact, Artifacts, Blob, Fetch, ResultFrame, Session, Start};

pub(super) async fn serve(
    listen: std::net::SocketAddr,
    root: PathBuf,
    token: &str,
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
    State((root, expected)): State<(PathBuf, String)>,
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
    match tokio::task::spawn_blocking(move || dispatch(&root, &operation, &body)).await {
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

fn dispatch(root: &Path, operation: &str, body: &[u8]) -> io::Result<Vec<u8>> {
    let parts: Vec<_> = operation.split('/').collect();
    match parts.as_slice() {
        ["start"] => start(root, wire::decode(body)?),
        ["blob", hash] => store_blob(root, hash, wire::decode(body)?),
        ["run", id] => run(root, &root.join("sessions").join(key(id, 32)?), id),
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
    fs::write(directory.join("start.pb"), request.encode_to_vec())?;
    wire::encode(&Session {
        id,
        missing: missing.into_iter().collect(),
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

fn run(root: &Path, session: &Path, id: &str) -> io::Result<Vec<u8>> {
    let request = Start::decode(fs::read(session.join("start.pb"))?.as_slice())?;
    let source = session.join("source");
    let output = session.join("output");
    fs::create_dir(&source)?;
    fs::create_dir(&output)?;
    for file in request.files.iter().filter(|file| !file.symlink) {
        let path = source.join(relative_path(&file.path)?);
        fs::create_dir_all(parent(&path)?)?;
        write_file(
            &path,
            &fs::read(root.join("blobs").join(key(&file.digest, 64)?))?,
            file.executable,
        )?;
    }
    for file in request.files.iter().filter(|file| file.symlink) {
        let relative = relative_path(&file.path)?;
        let path = source.join(relative);
        fs::create_dir_all(parent(&path)?)?;
        let data = fs::read(root.join("blobs").join(key(&file.digest, 64)?))?;
        create_link(&path, &data)?;
    }

    let action = match Action::try_from(request.action).map_err(io::Error::other)? {
        Action::Build => "build",
        Action::Test => "test",
        Action::Dylint => "dylint",
    };
    let soldr = std::env::current_exe()?;
    let result = Command::new(&soldr)
        .current_dir(&source)
        .env("CARGO_TARGET_DIR", output.join("target"))
        .arg(action)
        .args(&request.args)
        .output()?;
    let exit_code = result.status.code().unwrap_or(1);
    if let Some(level) = request.cache_zstd_level {
        let cache = Command::new(soldr)
            .current_dir(&source)
            .args(["save", "--zstd-level"])
            .arg(level.to_string())
            .arg("--out")
            .arg(output.join("cache.tar.zst"))
            .output()?;
        if !cache.status.success() {
            return Err(io::Error::other(format!(
                "soldr save failed: {}",
                String::from_utf8_lossy(&cache.stderr)
            )));
        }
    }
    let attestation = serde_json::to_vec_pretty(&serde_json::json!({
        "schema_version": 1,
        "session": id,
        "commit": request.commit,
        "action": action,
        "args": request.args,
        "exit_code": exit_code,
        "runner": fs::read_to_string("/etc/hostname").ok().map(|name| name.trim().to_owned()),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "source": request.files.iter().map(|file| (&file.path, &file.digest)).collect::<std::collections::BTreeMap<_, _>>(),
    }))?;
    fs::write(output.join("attestation.json"), &attestation)?;
    wire::encode(&ResultFrame {
        exit_code,
        stdout: result.stdout,
        stderr: result.stderr,
        attestation,
    })
}

pub(super) fn parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .ok_or_else(|| io::Error::other("missing parent directory"))
}

fn create_link(path: &Path, data: &[u8]) -> io::Result<()> {
    let target = std::str::from_utf8(data).map_err(io::Error::other)?;
    let target = relative_path(target)?;
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, path);
    #[cfg(not(unix))]
    {
        let _ = (path, target);
        Err(io::Error::other(
            "remote source symlinks require a Unix runner",
        ))
    }
}

pub(super) fn write_file(path: &Path, data: &[u8], executable: bool) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(data)?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
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
        #[cfg(unix)]
        let executable =
            std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o111 != 0;
        #[cfg(not(unix))]
        let executable = false;
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
