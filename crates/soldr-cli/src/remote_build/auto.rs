//! Dispatch supported ordinary builds to the signed-in Forge CI worker.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};

use super::wire::{Action, Blob, Capabilities, Start};
use super::{artifacts, control, snapshot, Client};

mod test_local;
mod test_runtime;

pub(crate) const WORKER_BUILD_ENV: &str = "SOLDR_FORGE_WORKER_BUILD";

/// Cargo invokes this callback solely to return the local test result.
pub(crate) fn runtime_edge(args: &[String]) -> Option<&'static str> {
    (crate::core::flag(WORKER_BUILD_ENV)
        && args.get(1).is_some_and(|arg| arg == "remote")
        && args.get(2).is_some_and(|arg| arg == "test-runtime"))
    .then_some("forge-test-runtime")
}

const PROBE_DEADLINE: Duration = Duration::from_secs(3);
const WORKER_PROBE_ATTEMPTS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuildMode {
    Blessed,
    Cargo,
    CargoCheck,
    CargoClippy,
    CargoTestCompile,
    CargoTestLocal,
    CargoTestRuntime,
    Lint,
}

impl BuildMode {
    fn for_cargo(verb: &str, args: &[String]) -> Option<Self> {
        let no_run = args
            .iter()
            .take_while(|arg| *arg != "--")
            .any(|arg| arg == "--no-run");
        match verb {
            "build" => Some(Self::Cargo),
            "check" => Some(Self::CargoCheck),
            "clippy" => Some(Self::CargoClippy),
            "test" if no_run => Some(Self::CargoTestCompile),
            _ => None,
        }
    }

    fn action(self) -> Action {
        match self {
            Self::Blessed => Action::Build,
            Self::Cargo => Action::CargoBuild,
            Self::CargoCheck => Action::CargoCheck,
            Self::CargoClippy => Action::CargoClippy,
            Self::CargoTestCompile | Self::CargoTestLocal => Action::CargoTestCompile,
            Self::CargoTestRuntime => Action::CargoTestRuntime,
            Self::Lint => Action::Lint,
        }
    }
}

pub(crate) async fn try_cargo(
    verb: &str,
    args: &[String],
    cache_enabled: bool,
) -> io::Result<Option<i32>> {
    if verb == "run"
        && !crate::cargo_front_door::porcelain::enabled()
        && args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .any(|arg| arg == "--target" || arg.starts_with("--target="))
    {
        // Local target preparation adds Cargo config and compiler environment.
        // Offer the original arguments to Forge before configuring this host.
        let mut invocation = vec!["run".to_owned()];
        invocation.extend_from_slice(args);
        let plan = crate::trampoline::plan_run(&invocation);
        return try_run(&plan, cache_enabled).await;
    }
    let Some(mode) = BuildMode::for_cargo(verb, args) else {
        return if verb == "test" {
            let args = normalize_workspace_alias(args, BuildMode::CargoTestLocal);
            test_local::try_test(&args, cache_enabled).await
        } else {
            Ok(None)
        };
    };
    try_build(args, cache_enabled, mode).await
}

pub(crate) async fn try_run(
    plan: &crate::trampoline::FellThroughPlan,
    cache_enabled: bool,
) -> io::Result<Option<i32>> {
    let (Some(parsed), Some(binary)) = (&plan.parsed, &plan.binary_path) else {
        return Ok(None);
    };
    if parsed.toolchain.is_some()
        || crate::core::flag("CARGO_NET_OFFLINE")
        || std::env::vars_os().any(|(name, _)| {
            let name = name.to_string_lossy();
            name.starts_with("CARGO_TARGET_") && name.ends_with("_RUNNER")
        })
    {
        return Ok(None);
    }

    let Some((_, arguments)) = plan.cleaned_args.split_first() else {
        return Ok(None);
    };
    let build_end = arguments
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(arguments.len());
    let mut build_args = arguments[..build_end].to_vec();
    if let Some(target) = parsed.target.as_deref() {
        let Ok(host) = crate::core::TargetTriple::host() else {
            return Ok(None);
        };
        if target != host.triple()
            || build_args
                .iter()
                .filter(|arg| arg.as_str() == "--target" || arg.starts_with("--target="))
                .count()
                != 1
        {
            return Ok(None);
        }
    }
    if parsed.bin.is_none() {
        let Some(name) = crate::trampoline::resolve_bin_name(parsed).ok().flatten() else {
            return Ok(None);
        };
        build_args.extend(["--bin".into(), name]);
    }
    let Some((client, root, work_dir)) =
        find_worker(&build_args, cache_enabled, BuildMode::Cargo).await?
    else {
        return Ok(None);
    };
    let cwd = std::env::current_dir()?;
    if !uses_direct_run(&cwd, &root) {
        return Ok(None);
    }
    let Some(relative) = binary
        .strip_prefix(&root)
        .ok()
        .and_then(artifact_executable_path)
    else {
        return Ok(None);
    };
    let Some(session) =
        start_worktree(&client, &root, work_dir, &build_args, BuildMode::Cargo).await?
    else {
        return Ok(None);
    };
    eprintln!("Forge CI build session {}", session.id);
    let code = receive_build(&client, &root, &session.id).await?;
    if code != 0 {
        return Ok(Some(code));
    }

    let staging = tempfile::tempdir_in(&root)?;
    let output = staging.path().join("artifacts");
    let executable = output.join(&relative);
    artifacts::download(&client, &session.id, &output, vec![relative]).await?;
    // Keep the downloaded executable alive until the user's program exits.
    crate::exit_guard::mark_spoke();
    let status = tokio::process::Command::new(&executable)
        .args(&parsed.trailing)
        .status()
        .await?;
    Ok(Some(status.code().unwrap_or(1)))
}

fn artifact_executable_path(path: &Path) -> Option<String> {
    let components: Option<Vec<_>> = path
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => name.to_str().filter(|name| {
                name.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            }),
            _ => None,
        })
        .collect();
    let components = components?;
    (components.first() == Some(&"target")).then(|| components.join("/"))
}

fn uses_direct_run(cwd: &Path, root: &Path) -> bool {
    cwd.ancestors()
        .take_while(|directory| directory.starts_with(root))
        .all(|directory| {
            ["config.toml", "config"].iter().all(|filename| {
                match std::fs::read_to_string(directory.join(".cargo").join(filename)) {
                    Ok(text) => text.parse::<toml::Table>().is_ok_and(|config| {
                        !config.contains_key("target")
                            && !config
                                .get("build")
                                .is_some_and(|build| build.get("target").is_some())
                    }),
                    Err(error) => error.kind() == io::ErrorKind::NotFound,
                }
            })
        })
}

async fn receive_build(client: &Client, root: &Path, session: &str) -> io::Result<i32> {
    let session = super::server::key(session, 32)?;
    let code = match control::receive(client, session).await {
        Err(error) if control::terminal_blocked(&error) => return Ok(1),
        result => result?,
    };

    download_attestation(client, root, session).await?;
    Ok(code)
}

async fn download_attestation(client: &Client, root: &Path, session: &str) -> io::Result<()> {
    let session = super::server::key(session, 32)?;
    crate::exit_guard::mark_spoke();
    let staging = tempfile::tempdir_in(root)?;
    let output = staging.path().join("artifacts");
    artifacts::download(client, session, &output, vec!["attestation.json".into()]).await?;

    let source = Dir::open_ambient_dir(&output, ambient_authority())?;
    let local = Dir::open_ambient_dir(root, ambient_authority())?;
    let target = local_target(&local, Path::new("target"))?;
    let attestations = local_target(&target, Path::new("forge-ci-attestations"))?;
    let name = format!("{session}.json");
    source.rename("attestation.json", &attestations, &name)?;
    eprintln!(
        "Forge CI build attestation: {}",
        root.join("target/forge-ci-attestations")
            .join(name)
            .display()
    );
    Ok(())
}

pub(crate) async fn try_build(
    args: &[String],
    cache_enabled: bool,
    mode: BuildMode,
) -> io::Result<Option<i32>> {
    let args = normalize_workspace_alias(args, mode);
    let args = args.as_ref();
    let Some((client, root, work_dir)) = find_worker(args, cache_enabled, mode).await? else {
        return Ok(None);
    };
    let Some(session) = start_worktree(&client, &root, work_dir, args, mode).await? else {
        return Ok(None);
    };
    eprintln!("Forge CI build session {}", session.id);
    let code = receive_build(&client, &root, &session.id).await?;
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

fn normalize_workspace_alias(args: &[String], mode: BuildMode) -> std::borrow::Cow<'_, [String]> {
    if !matches!(
        mode,
        BuildMode::Cargo
            | BuildMode::CargoCheck
            | BuildMode::CargoClippy
            | BuildMode::CargoTestCompile
            | BuildMode::CargoTestLocal
    ) || !args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--all")
    {
        return std::borrow::Cow::Borrowed(args);
    }
    let mut normalized = args.to_vec();
    for arg in normalized.iter_mut().take_while(|arg| arg.as_str() != "--") {
        if arg == "--all" {
            *arg = "--workspace".into();
        }
    }
    std::borrow::Cow::Owned(normalized)
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
            .map(|(client, root, _)| Self { client, root }))
    }

    pub(crate) async fn run(self, args: &[String]) -> io::Result<Option<i32>> {
        let mut remote_args = args.to_vec();
        if remote_args.first().is_none_or(|arg| arg != "rust") {
            remote_args.insert(0, "rust".into());
        }
        if !remote_args.iter().any(|arg| arg == "--check") {
            remote_args.push("--check".into());
        }
        if crate::color_choice::stderr_enabled()
            && !remote_args
                .iter()
                .any(|arg| arg == "--color" || arg.starts_with("--color="))
        {
            remote_args.push("--color=always".into());
        }
        let Some(session) = start_worktree(
            &self.client,
            &self.root,
            None,
            &remote_args,
            BuildMode::Lint,
        )
        .await?
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
) -> io::Result<Option<(Client, PathBuf, Option<String>)>> {
    if !cache_enabled || !can_delegate(args, mode) {
        return Ok(None);
    }
    let cwd = std::env::current_dir()?;
    let Ok(root) = snapshot::worktree_root(&cwd) else {
        return Ok(None);
    };
    if !root.join("Cargo.toml").is_file()
        || !uses_default_target_dir(&cwd, &root)
        || std::fs::symlink_metadata(root.join("target")).is_ok_and(|entry| !entry.is_dir())
    {
        return Ok(None);
    }
    let work_dir = if cwd == root {
        None
    } else {
        if mode == BuildMode::Lint {
            return Ok(None);
        }
        let Some(directory) = workspace_subdirectory(&root, &cwd).await else {
            return Ok(None);
        };
        Some(directory)
    };
    let Ok(commit) = snapshot::worktree_commit(&root) else {
        return Ok(None);
    };
    match worker(mode, work_dir.is_some(), commit.is_empty()).await {
        Ok(client) => Ok(client.map(|client| (client, root, work_dir))),
        Err(error) => {
            eprintln!("Forge CI worker unavailable ({error}). Running locally.");
            Ok(None)
        }
    }
}

async fn start_worktree(
    client: &Client,
    root: &Path,
    work_dir: Option<String>,
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
    if work_dir.as_ref().is_some_and(|directory| {
        !snapshot
            .files
            .iter()
            .any(|file| Path::new(&file.path).starts_with(directory))
    }) {
        return Ok(None);
    }
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
                work_dir,
                ..Start::default()
            },
            snapshot.blobs,
        )
        .await
        .map(Some)
}

async fn worker(
    mode: BuildMode,
    subdirectory: bool,
    without_commit: bool,
) -> io::Result<Option<Client>> {
    retry_worker(crate::forge_sign_in::discover_worker, |grant| async move {
        let client = Client::forge(grant)?;
        let capabilities: Capabilities = client.post("capabilities", &Blob::default()).await?;
        let host = crate::core::TargetTriple::host().map_err(io::Error::other)?;
        let supported = (!subdirectory || capabilities.worktree_subdirectories)
            && (!without_commit || capabilities.worktree_without_commit)
            && (mode != BuildMode::CargoTestLocal || capabilities.test_executables)
            && supports_build(&capabilities, &host.triple(), mode);
        Ok(supported.then_some(client))
    })
    .await
}

async fn retry_worker<G, T, D, P>(
    mut discover: impl FnMut() -> D,
    mut probe: impl FnMut(G) -> P,
) -> io::Result<Option<T>>
where
    D: Future<Output = io::Result<Option<G>>>,
    P: Future<Output = io::Result<Option<T>>>,
{
    let mut result = Ok(None);
    for _ in 0..WORKER_PROBE_ATTEMPTS {
        // Each fresh grant gives the gateway another chance to route the build.
        let grant = match probe_request(discover()).await {
            Ok(Some(grant)) => grant,
            Ok(None) => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Err(error),
            Err(error) => {
                result = Err(error);
                continue;
            }
        };
        result = probe_request(probe(grant)).await;
        if matches!(result, Ok(Some(_))) {
            return result;
        }
    }
    result
}

async fn probe_request<T>(request: impl Future<Output = io::Result<T>>) -> io::Result<T> {
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
            BuildMode::CargoClippy
            | BuildMode::CargoTestCompile
            | BuildMode::CargoTestLocal
            | BuildMode::CargoTestRuntime => capabilities
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
            "--" if mode == BuildMode::CargoClippy && value.is_none() => {
                return supported_clippy_lints(args);
            }
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

fn supported_clippy_lints<'a>(mut args: impl Iterator<Item = &'a String>) -> bool {
    while let Some(arg) = args.next() {
        let name = if let Some((flag, name)) = arg.split_once('=') {
            if !matches!(flag, "--allow" | "--warn" | "--deny" | "--forbid") {
                return false;
            }
            name
        } else if matches!(
            arg.as_str(),
            "-A" | "-W" | "-D" | "-F" | "--allow" | "--warn" | "--deny" | "--forbid"
        ) {
            let Some(name) = args.next() else {
                return false;
            };
            name
        } else if let Some(name) = ["-A", "-W", "-D", "-F"]
            .into_iter()
            .find_map(|flag| arg.strip_prefix(flag))
        {
            name
        } else {
            return false;
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-'))
            || name.starts_with('-')
        {
            return false;
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

async fn workspace_subdirectory(root: &Path, cwd: &Path) -> Option<String> {
    let relative = cwd
        .strip_prefix(root)
        .ok()?
        .to_str()?
        .replace(std::path::MAIN_SEPARATOR, "/");
    snapshot::relative_path(&relative).ok()?;
    let tool = crate::binaries::resolve_toolchain_binary("cargo").ok()?;
    let mut probe = std::process::Command::new(&tool);
    crate::binaries::apply_resolved_toolchain_homes(&mut probe, &tool);
    probe
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .env_remove("MAKEFLAGS")
        .env_remove("CARGO_MAKEFLAGS")
        .current_dir(cwd);
    let result = tokio::process::Command::from(probe).output().await.ok()?;
    let manifest = std::str::from_utf8(&result.stdout)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    (result.status.success() && Path::new(manifest) == root.join("Cargo.toml")).then_some(relative)
}

fn uses_default_target_dir(cwd: &Path, root: &Path) -> bool {
    let cargo_home = crate::core::resolve_cargo_home();
    let directories = cwd
        .ancestors()
        .map(|ancestor| (ancestor.join(".cargo"), ancestor.starts_with(root)))
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

    #[tokio::test]
    async fn discovery_and_probe_failures_are_retried_with_a_fresh_grant() {
        let mut grants = [
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "discovery timed out",
            )),
            Ok(Some("first")),
            Ok(Some("second")),
        ]
        .into_iter();
        let selected = retry_worker(
            || std::future::ready(grants.next().unwrap()),
            |grant| async move {
                match grant {
                    "first" => Err(io::Error::new(io::ErrorKind::ConnectionRefused, "offline")),
                    _ => Ok(Some(grant)),
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(selected, Some("second"));
    }

    #[test]
    fn remote_build_routing_only_accepts_supported_arguments() {
        let supported_args = |args: &[String]| super::supported_args(args, BuildMode::Cargo);
        let workspace_alias = ["--all".to_owned(), "--locked".to_owned()];
        let normalized = normalize_workspace_alias(&workspace_alias, BuildMode::Cargo);
        assert_eq!(normalized.as_ref(), ["--workspace", "--locked"]);
        assert!(supported_args(&normalized));
        let test_args = ["--all".to_owned(), "--".to_owned(), "--all".to_owned()];
        let normalized = normalize_workspace_alias(&test_args, BuildMode::CargoTestLocal);
        assert_eq!(normalized.as_ref(), ["--workspace", "--", "--all"]);
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
        for args in [
            vec!["--", "-D", "warnings"],
            vec!["--", "-Dwarnings", "--allow=clippy::match_same_arms"],
        ] {
            let args = args.into_iter().map(String::from).collect::<Vec<_>>();
            assert!(supported_args(&args, BuildMode::CargoClippy));
            assert!(!supported_args(&args, BuildMode::Cargo));
        }
        for args in [vec!["--", "-D"], vec!["--", "--emit=metadata"]] {
            let args = args.into_iter().map(String::from).collect::<Vec<_>>();
            assert!(!supported_args(&args, BuildMode::CargoClippy));
        }
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
