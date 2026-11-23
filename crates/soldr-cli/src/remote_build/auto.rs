//! Dispatch supported ordinary builds to the signed-in Forge CI worker.

use std::io;
use std::path::Path;
use std::time::Duration;

use cap_fs_ext::DirExt;
use cap_std::{ambient_authority, fs::Dir};

use super::wire::{Action, Blob, Capabilities, Start};
use super::{artifacts, control, snapshot, Client};

pub(crate) const WORKER_BUILD_ENV: &str = "SOLDR_FORGE_WORKER_BUILD";
const PROBE_DEADLINE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy)]
pub(crate) enum BuildMode {
    Blessed,
    Cargo,
}

pub(crate) async fn try_build(
    args: &[String],
    cache_enabled: bool,
    mode: BuildMode,
) -> io::Result<Option<i32>> {
    if !cache_enabled || !can_delegate(args) {
        return Ok(None);
    }
    let client = match worker().await {
        Ok(Some(client)) => client,
        Ok(None) => return Ok(None),
        Err(error) => {
            eprintln!("Forge CI worker unavailable ({error}). Building locally.");
            return Ok(None);
        }
    };
    let cwd = std::env::current_dir()?;
    let Ok(root) = snapshot::worktree_root(&cwd) else {
        return Ok(None);
    };
    if root != cwd
        || !root.join("Cargo.toml").is_file()
        || !uses_default_target_dir(&root)
        || std::fs::symlink_metadata(root.join("target")).is_ok_and(|entry| !entry.is_dir())
    {
        return Ok(None);
    }
    let snapshot = match snapshot::capture_worktree(&root) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("Forge CI could not capture this worktree ({error}). Building locally.");
            return Ok(None);
        }
    };
    let session = client
        .start_session(
            Start {
                commit: snapshot.commit,
                files: snapshot.files,
                action: match mode {
                    BuildMode::Blessed => Action::Build,
                    BuildMode::Cargo => Action::CargoBuild,
                } as i32,
                args: args.to_vec(),
                working_tree: true,
                ..Start::default()
            },
            snapshot.blobs,
        )
        .await?;
    eprintln!("Forge CI build session {}", session.id);
    let code = match control::receive(&client, &session.id).await {
        Err(error) if control::terminal_blocked(&error) => return Ok(Some(1)),
        result => result?,
    };
    if code == 0 {
        download_target(&root, &client, &session.id).await?;
    }
    Ok(Some(code))
}

async fn worker() -> io::Result<Option<Client>> {
    let request = async {
        let Some(grant) = crate::forge_sign_in::discover_worker().await? else {
            return Ok(None);
        };
        let client = Client::forge(grant)?;
        let capabilities: Capabilities = client.post("capabilities", &Blob::default()).await?;
        let host = crate::core::TargetTriple::host().map_err(io::Error::other)?;
        Ok(
            (capabilities.working_tree_builds && capabilities.host == host.triple())
                .then_some(client),
        )
    };
    tokio::time::timeout(PROBE_DEADLINE, request)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "worker probe expired"))?
}

fn can_delegate(args: &[String]) -> bool {
    std::env::var_os(WORKER_BUILD_ENV).is_none()
        && [
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_TARGET_DIR",
            "CARGO_BUILD_TARGET",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_BUILD_RUSTFLAGS",
            "RUSTFLAGS",
            "RUSTC",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTUP_TOOLCHAIN",
        ]
        .iter()
        .all(|key| std::env::var_os(key).is_none())
        && supported_args(args)
}

fn supported_args(args: &[String]) -> bool {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (flag, value) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(f, v)| (f, Some(v)));
        match flag {
            "--release"
            | "--locked"
            | "--workspace"
            | "--all-features"
            | "--no-default-features"
            | "--lib"
            | "--bins"
            | "--examples"
                if value.is_none() => {}
            "-p" | "--package" | "--features" | "-F" | "--profile" | "--bin" | "--example" => {
                if !value
                    .or_else(|| args.next().map(String::as_str))
                    .is_some_and(|value| !value.is_empty() && !value.starts_with('-'))
                {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

fn uses_default_target_dir(root: &Path) -> bool {
    let cargo_home = crate::core::resolve_cargo_home();
    let directories = root
        .ancestors()
        .map(|ancestor| (ancestor.join(".cargo"), ancestor == root))
        .chain(cargo_home.into_iter().map(|home| (home, false)));
    directories.into_iter().all(|(directory, in_repo)| {
        ["config.toml", "config"].iter().all(|name| {
            let path = directory.join(name);
            let content = match std::fs::read_to_string(path) {
                Ok(content) => content,
                Err(error) => return error.kind() == io::ErrorKind::NotFound,
            };
            let Ok(config) = content.parse::<toml::Table>() else {
                return false;
            };
            config
                .get("build")
                .and_then(|build| build.get("target-dir"))
                .is_none()
                && (in_repo || config.is_empty())
        })
    })
}

async fn download_target(root: &Path, client: &Client, session: &str) -> io::Result<()> {
    let local = Dir::open_ambient_dir(root, ambient_authority())?;
    let staging = tempfile::tempdir_in(root)?;
    let out = staging.path().join("artifacts");
    artifacts::download(client, session, &out, vec!["target/**".into()]).await?;
    let source = Dir::open_ambient_dir(&out, ambient_authority())?;
    let remote = match source.open_dir_nofollow("target") {
        Ok(remote) => remote,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let target = local_target(&local, Path::new("target"))?;
    merge_target(&remote, &target)
}

fn local_target(parent: &Dir, name: &Path) -> io::Result<Dir> {
    match parent.create_dir(name) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    parent.open_dir_nofollow(name)
}

fn merge_target(source: &Dir, local: &Dir) -> io::Result<()> {
    for entry in source.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let path = Path::new(&name);
        let kind = entry.file_type()?;
        if name == ".cargo-lock" || name == ".rustc_info.json" {
            continue;
        }
        if kind.is_dir() {
            merge_target(
                &source.open_dir_nofollow(path)?,
                &local_target(local, path)?,
            )?;
        } else if kind.is_file() {
            source.rename(path, local, path)?;
        } else {
            return Err(io::Error::other("Unexpected Forge CI artifact type."));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_build_routing_only_accepts_supported_arguments() {
        assert!(supported_args(&[]));
        assert!(supported_args(&["--release".into(), "--locked".into()]));
        assert!(supported_args(&[
            "-p".into(),
            "package".into(),
            "--features=one,two".into()
        ]));
        assert!(!supported_args(&["--package".into(), "--release".into()]));
        assert!(!supported_args(&["--target-dir=/other".into()]));
        assert!(!supported_args(&["--target=wasm32-unknown-unknown".into()]));
    }

    #[test]
    fn remote_build_outputs_replace_matching_files_and_preserve_local_outputs() {
        let root = tempfile::tempdir().unwrap();
        let root = Dir::open_ambient_dir(root.path(), ambient_authority()).unwrap();
        let stage = local_target(&root, Path::new("stage")).unwrap();
        let target = local_target(&root, Path::new("target")).unwrap();
        stage.write("binary", b"new").unwrap();
        stage.write(".cargo-lock", b"remote lock").unwrap();
        target.write("binary", b"old").unwrap();
        target.write("other", b"still here").unwrap();
        merge_target(&stage, &target).unwrap();
        assert_eq!(target.read("binary").unwrap(), b"new");
        assert_eq!(target.read("other").unwrap(), b"still here");
        assert!(!target.exists(".cargo-lock"));
    }
}
