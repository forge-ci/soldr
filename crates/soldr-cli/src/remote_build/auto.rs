//! Dispatch supported ordinary builds to the signed-in Forge CI worker.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::Duration;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};

use super::wire::{Action, Blob, Capabilities, Start};
use super::{artifacts, control, snapshot, Client};

pub(crate) const WORKER_BUILD_ENV: &str = "SOLDR_FORGE_WORKER_BUILD";
const PROBE_DEADLINE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuildMode {
    Blessed,
    Cargo,
    CargoCheck,
    CargoClippy,
    CargoTestCompile,
    Lint,
}

impl BuildMode {
    fn for_cargo(verb: &str, args: &[String]) -> Option<Self> {
        match verb {
            "build" => Some(Self::Cargo),
            "check" => Some(Self::CargoCheck),
            "clippy" => Some(Self::CargoClippy),
            "test" if args.iter().any(|arg| arg == "--no-run") => Some(Self::CargoTestCompile),
            _ => None,
        }
    }

    fn action(self) -> Action {
        match self {
            Self::Blessed => Action::Build,
            Self::Cargo => Action::CargoBuild,
            Self::CargoCheck => Action::CargoCheck,
            Self::CargoClippy => Action::CargoClippy,
            Self::CargoTestCompile => Action::CargoTestCompile,
            Self::Lint => Action::Lint,
        }
    }
}

pub(crate) async fn try_cargo(
    verb: &str,
    args: &[String],
    cache_enabled: bool,
) -> io::Result<Option<i32>> {
    let Some(mode) = BuildMode::for_cargo(verb, args) else {
        return Ok(None);
    };
    try_build(args, cache_enabled, mode).await
}

pub(crate) async fn try_build(
    args: &[String],
    cache_enabled: bool,
    mode: BuildMode,
) -> io::Result<Option<i32>> {
    let Some((client, root)) = find_worker(args, cache_enabled, mode).await? else {
        return Ok(None);
    };
    let Some(session) = start_worktree(&client, &root, args, mode).await? else {
        return Ok(None);
    };
    eprintln!("Forge CI build session {}", session.id);
    let code = match control::receive(&client, &session.id).await {
        Err(error) if control::terminal_blocked(&error) => return Ok(Some(1)),
        result => result?,
    };
    if code == 0
        && matches!(
            mode,
            BuildMode::Blessed | BuildMode::Cargo | BuildMode::CargoTestCompile
        )
    {
        download_target(&root, &client, &session.id, mode).await?;
    }
    Ok(Some(code))
}

pub(crate) struct LintWorker {
    client: Client,
    root: PathBuf,
}

impl LintWorker {
    pub(crate) async fn connect(args: &[String], cache_enabled: bool) -> io::Result<Option<Self>> {
        if crate::core::flag("CARGO_NET_OFFLINE") {
            return Ok(None);
        }
        let flags = match args.split_first() {
            Some((suite, flags)) if suite == "rust" => flags,
            _ => args,
        };
        Ok(find_worker(flags, cache_enabled, BuildMode::Lint)
            .await?
            .map(|(client, root)| Self { client, root }))
    }

    pub(crate) async fn run(self, args: &[String]) -> io::Result<Option<i32>> {
        let mut remote_args = args.to_vec();
        if remote_args.first().is_none_or(|arg| arg != "rust") {
            remote_args.insert(0, "rust".into());
        }
        if !remote_args.iter().any(|arg| arg == "--check") {
            remote_args.push("--check".into());
        }
        if io::stderr().is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && !remote_args
                .iter()
                .any(|arg| arg == "--color" || arg.starts_with("--color="))
        {
            remote_args.push("--color=always".into());
        }
        let Some(session) =
            start_worktree(&self.client, &self.root, &remote_args, BuildMode::Lint).await?
        else {
            return Ok(None);
        };
        eprintln!("Forge CI lint session {}", session.id);
        let path = self.root.join("target/attestation.json");
        let code = match self.client.finish_run(&session.id, &path).await {
            Err(error) if control::terminal_blocked(&error) => return Ok(Some(1)),
            result => result?,
        };
        crate::exit_guard::mark_spoke();
        eprintln!("Forge CI lint attestation: {}", path.display());
        Ok(Some(code))
    }
}

async fn find_worker(
    args: &[String],
    cache_enabled: bool,
    mode: BuildMode,
) -> io::Result<Option<(Client, PathBuf)>> {
    if !cache_enabled || !can_delegate(args, mode) {
        return Ok(None);
    }
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
    match worker(mode).await {
        Ok(client) => Ok(client.map(|client| (client, root))),
        Err(error) => {
            eprintln!("Forge CI worker unavailable ({error}). Running locally.");
            Ok(None)
        }
    }
}

async fn start_worktree(
    client: &Client,
    root: &Path,
    args: &[String],
    mode: BuildMode,
) -> io::Result<Option<super::wire::Session>> {
    let snapshot = match snapshot::capture_worktree(root) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("Forge CI could not capture this worktree ({error}). Running locally.");
            return Ok(None);
        }
    };
    client
        .start_session(
            Start {
                commit: snapshot.commit,
                files: snapshot.files,
                action: mode.action() as i32,
                args: args
                    .iter()
                    .filter(|arg| mode != BuildMode::CargoTestCompile || *arg != "--no-run")
                    .cloned()
                    .collect(),
                working_tree: true,
                ..Start::default()
            },
            snapshot.blobs,
        )
        .await
        .map(Some)
}

async fn worker(mode: BuildMode) -> io::Result<Option<Client>> {
    let request = async {
        let Some(grant) = crate::forge_sign_in::discover_worker().await? else {
            return Ok(None);
        };
        let client = Client::forge(grant)?;
        let capabilities: Capabilities = client.post("capabilities", &Blob::default()).await?;
        let host = crate::core::TargetTriple::host().map_err(io::Error::other)?;
        Ok(supports_build(&capabilities, &host.triple(), mode).then_some(client))
    };
    tokio::time::timeout(PROBE_DEADLINE, request)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "worker probe expired"))?
}

fn supports_build(capabilities: &Capabilities, host: &str, mode: BuildMode) -> bool {
    capabilities.working_tree_builds
        && capabilities.host == host
        && match mode {
            BuildMode::Blessed | BuildMode::Cargo | BuildMode::Lint => true,
            BuildMode::CargoCheck => capabilities.cargo_check,
            BuildMode::CargoClippy | BuildMode::CargoTestCompile => capabilities
                .compile_actions
                .contains(&(mode.action() as i32)),
        }
}

fn can_delegate(args: &[String], mode: BuildMode) -> bool {
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
        && supported_args(args, mode)
}

fn supported_args(args: &[String], mode: BuildMode) -> bool {
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
            | "--all-targets"
            | "--tests"
            | "--benches"
                if value.is_none() => {}
            "--no-run" if mode == BuildMode::CargoTestCompile && value.is_none() => {}
            "--check" | "--host-only" | "--cross-dylint"
                if mode == BuildMode::Lint && value.is_none() => {}
            "--color" if mode == BuildMode::Lint => {
                if !matches!(
                    value.or_else(|| args.next().map(String::as_str)),
                    Some("auto" | "always" | "never")
                ) {
                    return false;
                }
            }
            "--target" => {
                if !value
                    .or_else(|| args.next().map(String::as_str))
                    .is_some_and(portable_target)
                {
                    return false;
                }
            }
            "-p" | "--package" | "--features" | "-F" | "--profile" | "--bin" | "--example"
            | "--test" | "--bench" => {
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

fn portable_target(target: &str) -> bool {
    target
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && crate::target_alias::resolve_soldr_target(target).is_ok()
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

async fn download_target(
    root: &Path,
    client: &Client,
    session: &str,
    mode: BuildMode,
) -> io::Result<()> {
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
    if mode == BuildMode::CargoTestCompile {
        let tests = local_target(&target, Path::new("forge-ci-tests"))?;
        source.rename("target", &tests, session)?;
        eprintln!(
            "Forge CI test artifacts: {}",
            root.join("target/forge-ci-tests").join(session).display()
        );
        return Ok(());
    }
    publish_build_products(&remote, &target, true)
}

fn local_target(parent: &Dir, name: &Path) -> io::Result<Dir> {
    match parent.create_dir(name) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    parent.open_dir_nofollow(name)
}

fn publish_build_products(source: &Dir, local: &Dir, top_level: bool) -> io::Result<()> {
    for entry in source.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        if !entry.file_type()?.is_dir() || name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = Path::new(&name);
        let source_profile = source.open_dir_nofollow(path)?;
        let local_profile = local_target(local, path)?;
        if top_level && name.to_str().is_some_and(portable_target) {
            publish_build_products(&source_profile, &local_profile, false)?;
        } else {
            publish_locked_profile(&source_profile, &local_profile)?;
        }
    }
    Ok(())
}

fn publish_locked_profile(source: &Dir, local: &Dir) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .follow(FollowSymlinks::No);
    let mut locks = Vec::new();
    // Cargo uses separate build and artifact locks on newer toolchains.
    for name in [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
        let lock = local.open_with(name, &options)?.into_std();
        lock.try_lock().map_err(|error| {
            io::Error::other(format!(
                "Could not lock local Cargo outputs ({name}): {error}"
            ))
        })?;
        locks.push(lock);
    }
    publish_profile_products(source, local, true)
}

fn publish_profile_products(source: &Dir, local: &Dir, include_examples: bool) -> io::Result<()> {
    for entry in source.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let path = Path::new(&name);
        let kind = entry.file_type()?;
        if kind.is_file()
            && !name.to_string_lossy().starts_with('.')
            && path.extension().is_none_or(|extension| extension != "d")
        {
            source.rename(path, local, path)?;
        } else if include_examples && name == "examples" && kind.is_dir() {
            publish_profile_products(
                &source.open_dir_nofollow(path)?,
                &local_target(local, path)?,
                false,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_build_routing_only_accepts_supported_arguments() {
        let supported_args = |args: &[String]| super::supported_args(args, BuildMode::Cargo);
        assert!(supported_args(&[]));
        assert!(supported_args(&["--release".into(), "--locked".into()]));
        assert!(supported_args(&[
            "-p".into(),
            "package".into(),
            "--features=one,two".into()
        ]));
        assert!(!supported_args(&["--package".into(), "--release".into()]));
        assert!(!supported_args(&["--target-dir=/other".into()]));
        assert!(supported_args(&["--target=wasm32-unknown-unknown".into()]));
        assert!(supported_args(&["--target".into(), "win-x64".into()]));
        assert!(!supported_args(&["--target=../custom-target.json".into()]));
    }

    #[test]
    fn remote_build_checks_require_a_worker_that_advertises_cargo_check() {
        let host = "x86_64-unknown-linux-gnu";
        let mut capabilities = Capabilities {
            host: host.into(),
            working_tree_builds: true,
            ..Capabilities::default()
        };
        assert!(supports_build(&capabilities, host, BuildMode::Cargo));
        assert!(!supports_build(&capabilities, host, BuildMode::CargoCheck));
        capabilities.cargo_check = true;
        assert!(supports_build(&capabilities, host, BuildMode::CargoCheck));
        assert!(!supports_build(
            &capabilities,
            "aarch64-apple-darwin",
            BuildMode::CargoCheck
        ));
    }

    #[test]
    fn remote_build_clippy_and_test_compilation_require_worker_support() {
        let host = "x86_64-unknown-linux-gnu";
        let mut capabilities = Capabilities {
            host: host.into(),
            working_tree_builds: true,
            cargo_check: true,
            ..Capabilities::default()
        };
        let no_run = ["--no-run".into(), "--test".into(), "example".into()];
        assert_eq!(
            BuildMode::for_cargo("clippy", &[]),
            Some(BuildMode::CargoClippy)
        );
        assert_eq!(BuildMode::for_cargo("test", &[]), None);
        assert_eq!(
            BuildMode::for_cargo("test", &no_run),
            Some(BuildMode::CargoTestCompile)
        );
        assert!(supported_args(&no_run, BuildMode::CargoTestCompile));
        for mode in [BuildMode::CargoClippy, BuildMode::CargoTestCompile] {
            assert!(!supports_build(&capabilities, host, mode));
            capabilities.compile_actions.push(mode.action() as i32);
            assert!(supports_build(&capabilities, host, mode));
        }
        let mut args = vec!["--locked".into()];
        assert_eq!(Action::CargoTestCompile.worker_command(&mut args), "cargo");
        assert_eq!(args, ["test", "--no-run", "--locked"]);
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
        let fingerprint = local_target(&stage, Path::new(".fingerprint")).unwrap();
        fingerprint.write("worker", b"remote-only state").unwrap();
        publish_locked_profile(&stage, &target).unwrap();
        assert_eq!(target.read("binary").unwrap(), b"new");
        assert_eq!(target.read("other").unwrap(), b"still here");
        assert!(target.read(".cargo-lock").unwrap().is_empty());
        assert!(!target.exists(".fingerprint"));
    }
}
