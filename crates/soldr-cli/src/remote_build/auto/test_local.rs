use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{artifact_executable_path, artifacts, find_worker, receive_build, snapshot};
use super::{start_worktree, supported_args, test_runtime, uses_direct_run, BuildMode};

const INVENTORY: &str = "test-executables.json";

#[derive(Deserialize)]
struct TestExecutable {
    executable: String,
    manifest: String,
}

pub(super) async fn try_test(args: &[String], cache_enabled: bool) -> io::Result<Option<i32>> {
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let (compile, test) = args.split_at(separator);
    if crate::core::flag("CARGO_NET_OFFLINE")
        || std::env::vars_os().any(|(name, _)| {
            let name = name.to_string_lossy();
            name.starts_with("CARGO_TARGET_") && name.ends_with("_RUNNER")
        })
    {
        return Ok(None);
    }
    let no_fail_fast = compile.iter().any(|arg| arg == "--no-fail-fast");
    let doc_only = compile.iter().any(|arg| arg == "--doc");
    let mut compile: Vec<_> = compile
        .iter()
        .filter(|arg| *arg != "--no-fail-fast" && *arg != "--doc")
        .cloned()
        .collect();
    let Ok(host) = crate::core::TargetTriple::host() else {
        return Ok(None);
    };
    if !targets_match_host(&compile, &host.triple()) {
        return Ok(None);
    }
    let mut test_args = test.get(1..).unwrap_or_default().to_vec();
    if !supported_args(&compile, BuildMode::CargoTestLocal) {
        let Some(filter) = compile.pop().filter(|arg| !arg.starts_with('-')) else {
            return Ok(None);
        };
        test_args.insert(0, filter);
    }
    if !crate::core::flag("SOLDR_FORGE_LOCAL_TEST_RUNTIME") {
        return run_on_worker(args, &compile, cache_enabled).await;
    }
    if let Some(code) = test_runtime::try_relay(args, &compile, cache_enabled).await? {
        return Ok(Some(code));
    }
    if doc_only {
        return Ok(None);
    }
    let Some((client, root, work_dir)) =
        find_worker(&compile, cache_enabled, BuildMode::CargoTestLocal).await?
    else {
        return Ok(None);
    };
    if !selects_test_targets(&compile) && (work_dir.is_some() || !package_has_no_doctests(&root)) {
        return Ok(None);
    }
    if !uses_direct_run(&std::env::current_dir()?, &root) {
        return Ok(None);
    }

    compile.push("--message-format=json".into());
    let Some(session) = start_worktree(
        &client,
        &root,
        work_dir,
        &compile,
        BuildMode::CargoTestLocal,
    )
    .await?
    else {
        return Ok(None);
    };
    eprintln!("Forge CI build session {}", session.id);
    let code = receive_build(&client, &root, &session.id).await?;
    if code != 0 {
        return Ok(Some(code));
    }

    let staging = tempfile::tempdir_in(&root)?;
    let metadata = staging.path().join("metadata");
    artifacts::download(&client, &session.id, &metadata, vec![INVENTORY.into()]).await?;
    let inventory = serde_json::from_slice(&std::fs::read(metadata.join(INVENTORY))?)?;
    let tests = resolve_tests(&root, inventory)?;
    if tests.is_empty() {
        return Ok(Some(0));
    }
    let output = staging.path().join("artifacts");
    let requested = tests
        .iter()
        .map(|(executable, _)| executable.clone())
        .collect();
    artifacts::download(&client, &session.id, &output, requested).await?;
    run_downloaded_tests(&output, tests, &test_args, no_fail_fast).await
}

async fn run_downloaded_tests(
    output: &Path,
    tests: Vec<(String, PathBuf)>,
    test_args: &[String],
    no_fail_fast: bool,
) -> io::Result<Option<i32>> {
    let mut exit = 0;
    for (executable, cwd) in tests {
        let path = output.join(&executable);
        eprintln!("Running {}", executable);
        let mut command = tokio::process::Command::new(path);
        command.current_dir(cwd).args(test_args);
        crate::exit_guard::mark_spoke();
        let status = command.status().await?;
        if !status.success() {
            exit = 101;
            if !no_fail_fast {
                break;
            }
        }
    }
    Ok(Some(exit))
}

async fn run_on_worker(
    args: &[String],
    compile: &[String],
    cache_enabled: bool,
) -> io::Result<Option<i32>> {
    let mode = BuildMode::CargoTestWorker;
    let Some((client, root, work_dir)) = find_worker(compile, cache_enabled, mode).await? else {
        return Ok(None);
    };
    if !uses_direct_run(&std::env::current_dir()?, &root) {
        return Ok(None);
    }
    let Some(session) = start_worktree(&client, &root, work_dir, args, mode).await? else {
        return Ok(None);
    };
    eprintln!("Forge CI build session {}", session.id);
    receive_build(&client, &root, &session.id).await.map(Some)
}

fn resolve_tests(
    root: &Path,
    inventory: Vec<TestExecutable>,
) -> io::Result<Vec<(String, PathBuf)>> {
    inventory
        .into_iter()
        .map(|entry| {
            let executable = artifact_executable_path(snapshot::relative_path(&entry.executable)?)
                .ok_or_else(|| io::Error::other("invalid Forge CI test executable path"))?;
            let manifest = snapshot::relative_path(&entry.manifest)?;
            let cwd = root.join(manifest.parent().ok_or_else(|| {
                io::Error::other("Forge CI test manifest omitted its package directory")
            })?);
            let cwd = cwd.canonicalize()?;
            if !cwd.starts_with(root) {
                return Err(io::Error::other("Forge CI test path escapes the checkout"));
            }
            Ok((executable, cwd))
        })
        .collect()
}

fn targets_match_host(args: &[String], host: &str) -> bool {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let target = if arg == "--target" {
            let Some(value) = args.next() else {
                return false;
            };
            Some(value.as_str())
        } else {
            arg.strip_prefix("--target=")
        };
        if target.is_some_and(|target| target != host) {
            return false;
        }
    }
    true
}

fn selects_test_targets(args: &[String]) -> bool {
    args.iter().any(|arg| {
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        matches!(
            flag,
            "--lib"
                | "--bin"
                | "--bins"
                | "--example"
                | "--examples"
                | "--test"
                | "--tests"
                | "--bench"
                | "--benches"
                | "--all-targets"
        )
    })
}

fn package_has_no_doctests(root: &Path) -> bool {
    if root
        .ancestors()
        .skip(1)
        .any(|ancestor| !matches!(ancestor.join("Cargo.toml").try_exists(), Ok(false)))
    {
        return false;
    }
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok());
    let Some(manifest) = manifest else {
        return false;
    };
    if manifest.contains_key("workspace")
        || manifest
            .get("package")
            .and_then(|p| p.get("workspace"))
            .is_some()
    {
        return false;
    }
    if let Some(library) = manifest.get("lib") {
        return library.get("doctest").and_then(toml::Value::as_bool) == Some(false);
    }
    manifest.contains_key("package") && matches!(root.join("src/lib.rs").try_exists(), Ok(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_native_explicit_targets_can_execute_locally() {
        let host = "x86_64-unknown-linux-gnu";
        assert!(targets_match_host(&[], host));
        assert!(targets_match_host(&["--target".into(), host.into()], host));
        assert!(targets_match_host(&[format!("--target={host}")], host));
        assert!(!targets_match_host(&["--target".into()], host));
        assert!(!targets_match_host(
            &[
                format!("--target={host}"),
                "--target=aarch64-apple-darwin".into()
            ],
            host,
        ));
    }

    #[test]
    fn explicit_target_selection_excludes_doctests() {
        for selector in ["--all-targets", "--tests", "--example=demo", "--bin=cli"] {
            assert!(selects_test_targets(&[selector.into()]));
        }
        assert!(!selects_test_targets(&[
            "--workspace".into(),
            "--all-features".into(),
        ]));
    }

    #[test]
    fn standalone_test_routing_preserves_implicit_doctests() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        let manifest = project.join("Cargo.toml");
        let package = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n";
        std::fs::write(&manifest, package).unwrap();
        assert!(package_has_no_doctests(&project));

        std::fs::write(project.join("src/lib.rs"), "").unwrap();
        assert!(!package_has_no_doctests(&project));

        std::fs::write(&manifest, format!("{package}\n[lib]\ndoctest = false\n")).unwrap();
        assert!(package_has_no_doctests(&project));

        std::fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"project\"]\n",
        )
        .unwrap();
        assert!(!package_has_no_doctests(&project));
    }
}
