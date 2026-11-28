//! `ape`: Actually Portable Executable detection, loader planning, and the
//! recovery every spawn path applies when the host refuses an APE image.
//!
//! The fixture is the smallest image with Cosmopolitan's shape: the APE magic
//! opening a shell prologue. A host without a `binfmt_misc` entry refuses it
//! exactly as it refuses a real APE binary, and the host shell runs it, so it
//! drives the same recovery without shipping a Cosmopolitan build. Where the
//! host runs APE images natively the fixture is not a valid image at all, so
//! the spawn tests there assert only that no plan is made.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kernal_api::ape::{self, ApeLoaderKind, ApeOptions};
use kernal_api::platform::process::{spawn_sync, SpawnStdio, StdioSource, SyncEnvironment};
use kernal_api::{run_bounded_command, BoundedProcessError, SpawnSpec, StreamMode};

const PROLOGUE: &str = "MZqFpD='\n'\nprintf 'ape-ok:%s|' \"$@\"\nexit 0\n";

/// Write the fixture image with the permissions of this (executable) test
/// binary, which is how a neutral test obtains an executable file.
fn image(dir: &Path) -> PathBuf {
    let path = dir.join("tool.com");
    std::fs::copy(std::env::current_exe().unwrap(), &path).expect("copy executable mode");
    std::fs::write(&path, PROLOGUE).expect("write prologue");
    path
}

/// The plan a spawn of `image` would make with an empty environment, or
/// `None` where the fixture cannot be run (native hosts, or a host with an
/// installed `ape` that would try to load the shell-only fixture).
fn shell_plan(image: &Path) -> Option<ape::ApeLaunch> {
    let plan = ape::plan_launch(image.as_os_str(), None, &ApeOptions::default());
    if ape::runs_natively() {
        assert_eq!(
            plan, None,
            "a host that runs APE images natively plans nothing"
        );
        return None;
    }
    let plan = plan.expect("a refused APE image always has a plan where /bin/sh exists");
    (plan.kind() == ApeLoaderKind::Shell).then_some(plan)
}

#[test]
fn every_ape_magic_is_recognized_and_native_headers_are_not() {
    for magic in ["MZqFpD='", "jartsr='", "APEDBG='"] {
        assert!(
            ape::is_ape_header(format!("{magic}\n'\n").as_bytes()),
            "{magic}"
        );
    }
    for other in [&b"\x7fELF\x02\x01"[..], b"MZ\x90\x00", b"#!/bin/sh\n", b""] {
        assert!(!ape::is_ape_header(other), "{other:?}");
    }
}

#[test]
fn this_test_binary_is_native_and_has_no_plan() {
    let exe = std::env::current_exe().unwrap();
    assert!(!ape::is_ape_file(&exe));
    assert_eq!(
        ape::plan_launch(exe.as_os_str(), None, &ApeOptions::inherited()),
        None
    );
}

#[test]
fn an_installed_loader_is_preferred_and_receives_the_image_first() {
    let dir = tempfile::tempdir().unwrap();
    let image = image(dir.path());
    assert!(ape::is_ape_file(&image));
    let loaders = dir.path().join("loaders");
    std::fs::create_dir(&loaders).unwrap();
    std::fs::copy(std::env::current_exe().unwrap(), loaders.join("ape")).unwrap();
    std::fs::write(loaders.join("ape"), b"stand-in loader").unwrap();
    let environment = ApeOptions {
        path: Some(std::env::join_paths([dir.path(), &loaders]).unwrap()),
        ..ApeOptions::default()
    };

    let plan = ape::plan_launch(OsStr::new("tool.com"), None, &environment);
    if ape::runs_natively() {
        assert_eq!(plan, None);
        return;
    }
    let plan = plan.expect("APE image on PATH with a loader beside it");
    assert_eq!(plan.kind(), ApeLoaderKind::Installed);
    assert_eq!(plan.loader(), loaders.join("ape"));
    assert_eq!(plan.image(), image);
    assert_eq!(
        plan.args(["--flag", "two words"]),
        vec![
            image.clone().into_os_string(),
            OsString::from("--flag"),
            OsString::from("two words"),
        ]
    );
    let spec = format!("{:?}", plan.spawn_spec(["--flag"]));
    assert!(spec.contains("--flag"), "{spec}");
}

/// Retry while a sibling test's fork still holds the freshly written
/// fixture open for writing (`ETXTBSY`); the window closes once it execs.
fn when_idle<T, E>(
    mut attempt: impl FnMut() -> Result<T, E>,
    busy: impl Fn(&E) -> bool,
) -> Result<T, E> {
    for _ in 0..50 {
        match attempt() {
            Err(error) if busy(&error) => std::thread::sleep(Duration::from_millis(10)),
            result => return result,
        }
    }
    attempt()
}

fn io_busy(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::ExecutableFileBusy
}

fn bounded_busy(error: &BoundedProcessError) -> bool {
    matches!(error, BoundedProcessError::Spawn(error) if io_busy(error))
}

#[tokio::test]
async fn a_spawn_spec_runs_a_refused_image_even_with_a_cleared_environment() {
    let dir = tempfile::tempdir().unwrap();
    let image = image(dir.path());
    if shell_plan(&image).is_none() {
        return;
    }
    let spec = SpawnSpec::new(&image)
        .args(["one", "two words"])
        .clear_env(true)
        .stdout(StreamMode::Piped);
    let mut busy_retries = 0;
    let child = loop {
        match spec.clone().spawn().await {
            Err(error) if io_busy(&error) && busy_retries < 50 => {
                busy_retries += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            result => break result,
        }
    };
    let output = child
        .expect("refused APE image recovered")
        .wait_with_output_bounded_within(1 << 16, Duration::from_secs(5))
        .await
        .expect("output");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"ape-ok:one|ape-ok:two words|");
}

#[test]
fn a_bounded_run_recovers_a_refused_image() {
    let dir = tempfile::tempdir().unwrap();
    let image = image(dir.path());
    if shell_plan(&image).is_none() {
        return;
    }
    let output = when_idle(
        || {
            run_bounded_command(
                SpawnSpec::new("./tool.com")
                    .arg("x")
                    .current_dir(dir.path()),
                Duration::from_secs(30),
                1 << 16,
            )
        },
        bounded_busy,
    )
    .expect("refused APE image recovered");
    assert_eq!(output.stdout, b"ape-ok:x|");
}

#[test]
fn a_sync_spawn_recovers_a_refused_image() {
    let dir = tempfile::tempdir().unwrap();
    let image = image(dir.path());
    if shell_plan(&image).is_none() {
        return;
    }
    let out_path = dir.path().join("out.txt");
    let out = std::fs::File::create(&out_path).unwrap();
    let mut command = std::process::Command::new(&image);
    command.arg("y");
    let mut child = when_idle(
        || {
            spawn_sync(
                &mut command,
                SpawnStdio {
                    stdin: StdioSource::Null,
                    stdout: StdioSource::File(&out),
                    stderr: StdioSource::Null,
                    drain_timeout: None,
                    show_console: false,
                },
                SyncEnvironment::Inherit,
            )
        },
        io_busy,
    )
    .expect("refused APE image recovered");
    assert_eq!(
        child
            .wait_within(std::time::Duration::from_secs(5))
            .unwrap(),
        0
    );
    assert_eq!(std::fs::read(&out_path).unwrap(), b"ape-ok:y|");
}

#[test]
fn a_refused_image_that_is_not_ape_keeps_its_error() {
    let dir = tempfile::tempdir().unwrap();
    let garbage = image(dir.path());
    std::fs::write(&garbage, b"\x00\x01\x02\x03 neither native nor APE").unwrap();
    assert!(!ape::is_ape_file(&garbage));
    assert_eq!(
        ape::plan_launch(garbage.as_os_str(), None, &ApeOptions::default()),
        None
    );
    // The host may report the refusal itself, or -- where its C library
    // follows POSIX `execvp` -- hand the file to the shell, which rejects it.
    // Either way it must not be run as an APE image.
    let result = when_idle(
        || run_bounded_command(SpawnSpec::new(&garbage), Duration::from_secs(30), 1024),
        bounded_busy,
    );
    if let Ok(output) = result {
        assert!(!output.exit.is_success(), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
    }
}

/// The cosmocc hello-world (`tests/fixtures/ape-hello`): a fat x86_64 +
/// aarch64 APE image that prints `hello world` and its arguments.
fn hello() -> PathBuf {
    // Source-free nextest archives have no checkout at the remapped workspace.
    // Keep the real image in the binary and retain its executable mode.
    static FIXTURE: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let dir = FIXTURE.get_or_init(|| {
        let dir = tempfile::tempdir().expect("APE fixture directory");
        let path = dir.path().join("hello.com");
        std::fs::copy(std::env::current_exe().unwrap(), &path).expect("copy executable mode");
        std::fs::write(&path, include_bytes!("../fixtures/ape-hello/hello.com"))
            .expect("materialize real APE fixture");
        dir
    });
    dir.path().join("hello.com")
}

/// Whether this host runs the real image through its own embedded loader,
/// installing it in `cache`. Elsewhere (Windows natively, macOS through
/// `cc`) the hostile-environment tests below do not apply.
fn embedded_loader_in(cache: &Path) -> bool {
    let options = ApeOptions {
        cache_dirs: vec![cache.to_path_buf()],
        ..ApeOptions::default()
    };
    ape::plan_launch(hello().as_os_str(), None, &options)
        .is_some_and(|plan| plan.kind() == ApeLoaderKind::Embedded)
}

/// No `PATH`, `TMPDIR` or `HOME`: neither `ape`, shell utilities, nor the
/// image's own prologue cache can help, only the extracted loader.
fn hostile(spec: SpawnSpec, cache: &Path) -> SpawnSpec {
    spec.clear_env(true)
        .env("PATH", "/nonexistent")
        .env("TMPDIR", "/nonexistent")
        .env("HOME", "/nonexistent")
        .env(ape::CACHE_DIR_ENV, cache)
}

#[test]
fn the_real_fixture_is_an_ape_image() {
    assert!(ape::is_ape_file(&hello()));
}

#[tokio::test]
async fn a_real_image_runs_through_spawn_spec_in_a_hostile_environment() {
    let cache = tempfile::tempdir().unwrap();
    if !embedded_loader_in(cache.path()) {
        return;
    }
    let output = hostile(SpawnSpec::new(hello()).arg("spec"), cache.path())
        .stdout(StreamMode::Piped)
        .spawn()
        .await
        .expect("real APE image launched")
        .wait_with_output_bounded_within(1 << 16, Duration::from_secs(5))
        .await
        .expect("output");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"hello world spec\n");
}

#[test]
fn a_real_image_runs_through_a_bounded_run_in_a_hostile_environment() {
    let cache = tempfile::tempdir().unwrap();
    if !embedded_loader_in(cache.path()) {
        return;
    }
    let output = run_bounded_command(
        hostile(SpawnSpec::new(hello()).args(["a b", "c"]), cache.path()),
        Duration::from_secs(30),
        1 << 16,
    )
    .expect("real APE image launched");
    assert!(output.exit.is_success(), "{output:?}");
    assert_eq!(output.stdout, b"hello world a b c\n");
}

#[test]
fn command_routes_a_real_image_through_its_loader() {
    if ape::runs_natively() {
        return;
    }
    // Plan first, then hold the fork lock only across the spawn: planning
    // may install the loader under the exclusive half of the same lock.
    let mut command = std::process::Command::new("unused");
    let output = when_idle(
        || {
            command = ape::command(hello());
            command.arg("cmd");
            let _fork = ape::fork_guard();
            command.output()
        },
        io_busy,
    )
    .expect("real APE image launched");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"hello world cmd\n");
}

/// What a cosmocc `gcc` does with `cc1`: a process this crate did not plan
/// execs an APE image itself. The launch's `ape` directory, first on the
/// child's search path, is all that image's prologue needs.
#[test]
fn a_nested_spawn_finds_the_loader_through_the_ape_path_directory() {
    let cache = tempfile::tempdir().unwrap();
    if !embedded_loader_in(cache.path()) {
        return;
    }
    let options = ApeOptions {
        cache_dirs: vec![cache.path().to_path_buf()],
        ..ApeOptions::default()
    };
    let launch = ape::plan_launch(hello().as_os_str(), None, &options).expect("planned");
    let dir = launch
        .ape_path_dir()
        .expect("an ape directory")
        .to_path_buf();
    assert!(dir.join("ape").is_file());
    let search = launch.child_path(None).expect("search path");
    let output = run_bounded_command(
        SpawnSpec::new("/bin/sh")
            .args(["-c", "exec \"$0\" nested"])
            .arg(hello())
            .clear_env(true)
            .env("PATH", search)
            .env("TMPDIR", "/nonexistent")
            .env("HOME", "/nonexistent"),
        Duration::from_secs(30),
        1 << 16,
    )
    .expect("nested APE launch");
    assert!(output.exit.is_success(), "{output:?}");
    assert_eq!(output.stdout, b"hello world nested\n");
}
