//! Facade coverage for a compiler-style session owner: argument lists, spawn
//! admission, best-effort priority, native exit status, and concurrent use of
//! one session from several tasks.
//!
//! These are the operations a client previously took from the substrate's
//! async builder and session types; each is exercised here through the
//! facade-owned surface only.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kernal_api::{
    ProcessOutputChunk, ProcessOutputCompletion, ProcessOutputEvent, ProcessPriority,
    ProcessSession, ProcessSessionOptions, SpawnAdmission, SpawnSpec, StreamMode,
};

const ECHO_HELPER_ENV: &str = "KERNAL_API_SESSION_CONTROL_ECHO_HELPER";
const EXIT_HELPER_ENV: &str = "KERNAL_API_SESSION_CONTROL_EXIT_HELPER";

/// Copy stdin to stdout, then exit; a portable stand-in for `cat`.
#[test]
fn echo_stdin_helper() {
    if std::env::var_os(ECHO_HELPER_ENV).is_none() {
        return;
    }
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&input).unwrap();
    stdout.flush().unwrap();
    std::process::exit(0);
}

/// Exit with a fixed non-zero code, without libtest's own reporting.
#[test]
fn exit_code_helper() {
    if std::env::var_os(EXIT_HELPER_ENV).is_some() {
        std::process::exit(3);
    }
}

fn helper(name: &str, env: &str) -> SpawnSpec {
    SpawnSpec::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(env, "1")
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn session_surface_types_are_send_and_sync() {
    assert_send_sync::<SpawnSpec>();
    assert_send_sync::<SpawnAdmission>();
    assert_send_sync::<ProcessSession>();
    assert_send_sync::<ProcessSessionOptions>();
    assert_send_sync::<ProcessOutputEvent>();
}

#[tokio::test]
#[cfg(unix)]
async fn args_appends_every_argument_in_order() {
    let session = SpawnSpec::new("sh")
        .args(["-c", "printf '%s|' \"$@\"", "sh"])
        .args(vec![String::from("a b"), String::from("c")])
        .stdin(StreamMode::Null)
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Null)
        .spawn_session(ProcessSessionOptions::default())
        .await
        .expect("start session");
    let (stdout, _) = drain(&session).await;
    assert!(session.wait().await.expect("wait child").is_success());
    assert_eq!(stdout, b"a b|c|");
}

/// A permit released on drop, recording how many are held at once.
struct Permit(Arc<AtomicUsize>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn counting_admission(acquired: &Arc<AtomicUsize>, held: &Arc<AtomicUsize>) -> SpawnAdmission {
    let acquired = Arc::clone(acquired);
    let held = Arc::clone(held);
    SpawnAdmission::new(move || {
        acquired.fetch_add(1, Ordering::SeqCst);
        held.fetch_add(1, Ordering::SeqCst);
        Ok::<_, std::io::Error>(Permit(Arc::clone(&held)))
    })
}

#[tokio::test]
async fn spawn_admission_is_acquired_once_and_released_after_the_spawn() {
    let acquired = Arc::new(AtomicUsize::new(0));
    let held = Arc::new(AtomicUsize::new(0));
    let session = helper("process_session_control::exit_code_helper", EXIT_HELPER_ENV)
        .stdin(StreamMode::Null)
        .stdout(StreamMode::Null)
        .stderr(StreamMode::Null)
        .spawn_admission(counting_admission(&acquired, &held))
        .spawn_session(ProcessSessionOptions::default())
        .await
        .expect("start admitted session");

    assert_eq!(acquired.load(Ordering::SeqCst), 1);
    assert_eq!(
        held.load(Ordering::SeqCst),
        0,
        "the permit covers native creation only, not the child's lifetime"
    );
    assert_eq!(
        session.wait().await.expect("wait child").exit_code(),
        Some(3)
    );
}

#[tokio::test]
async fn spawn_admission_is_released_when_the_spawn_fails() {
    let acquired = Arc::new(AtomicUsize::new(0));
    let held = Arc::new(AtomicUsize::new(0));
    let error = SpawnSpec::new("kernal-api-definitely-not-a-program")
        .spawn_admission(counting_admission(&acquired, &held))
        .spawn_session(ProcessSessionOptions::default())
        .await
        .err()
        .expect("a missing program must fail to spawn");

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(acquired.load(Ordering::SeqCst), 1);
    assert_eq!(held.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_refused_admission_fails_the_spawn_with_its_own_error() {
    let error = helper("process_session_control::exit_code_helper", EXIT_HELPER_ENV)
        .spawn_admission(SpawnAdmission::new(|| {
            Err::<(), _>(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "admission refused by fixture",
            ))
        }))
        .spawn_session(ProcessSessionOptions::default())
        .await
        .err()
        .expect("a refused admission must not start the child");

    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert!(error.to_string().contains("admission refused by fixture"));
}

#[tokio::test]
async fn spawn_admission_also_guards_a_plain_child_spawn() {
    let acquired = Arc::new(AtomicUsize::new(0));
    let held = Arc::new(AtomicUsize::new(0));
    let mut child = helper("process_session_control::exit_code_helper", EXIT_HELPER_ENV)
        .stdin(StreamMode::Null)
        .stdout(StreamMode::Null)
        .stderr(StreamMode::Null)
        .spawn_admission(counting_admission(&acquired, &held))
        .spawn()
        .await
        .expect("start admitted child");
    assert_eq!(
        child
            .wait_within(std::time::Duration::from_secs(5))
            .await
            .expect("wait child")
            .code(),
        Some(3)
    );
    assert_eq!(acquired.load(Ordering::SeqCst), 1);
    assert_eq!(held.load(Ordering::SeqCst), 0);
}

#[test]
fn a_bounded_run_refuses_an_admission_it_cannot_hold() {
    let error = kernal_api::run_bounded_command(
        helper("process_session_control::exit_code_helper", EXIT_HELPER_ENV)
            .spawn_admission(SpawnAdmission::new(|| Ok::<_, std::io::Error>(()))),
        Duration::from_secs(10),
        1024,
    )
    .expect_err("a bounded run cannot place a spawn admission");
    let kernal_api::BoundedProcessError::Io(error) = error else {
        panic!("admission refusal is an input error: {error:?}");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[tokio::test]
async fn best_effort_priority_never_fails_the_spawn() {
    for priority in [
        ProcessPriority::High,
        ProcessPriority::Normal,
        ProcessPriority::Low,
        ProcessPriority::Idle,
    ] {
        let session = helper("process_session_control::exit_code_helper", EXIT_HELPER_ENV)
            .stdin(StreamMode::Null)
            .stdout(StreamMode::Null)
            .stderr(StreamMode::Null)
            .priority_best_effort(priority)
            .spawn_session(ProcessSessionOptions::default())
            .await
            .unwrap_or_else(|error| panic!("{priority:?} must not fail the spawn: {error}"));
        assert_eq!(
            session.wait().await.expect("wait child").exit_code(),
            Some(3)
        );
    }
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn best_effort_priority_applies_the_band_when_the_host_permits_it() {
    let session = SpawnSpec::new("sh")
        .args(["-c", "ps -o ni= -p $$"])
        .stdin(StreamMode::Null)
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Null)
        .priority_best_effort(ProcessPriority::Low)
        .spawn_session(ProcessSessionOptions::default())
        .await
        .expect("start low-priority session");
    let (stdout, _) = drain(&session).await;
    assert!(session.wait().await.expect("wait child").is_success());
    assert_eq!(String::from_utf8(stdout).unwrap().trim(), "10");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn best_effort_high_priority_falls_back_to_the_inherited_band_when_denied() {
    let strict = SpawnSpec::new("sh")
        .args(["-c", "exit 0"])
        .priority(ProcessPriority::High)
        .spawn_session(ProcessSessionOptions::default())
        .await;
    let session = SpawnSpec::new("sh")
        .args(["-c", "ps -o ni= -p $$"])
        .stdin(StreamMode::Null)
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Null)
        .priority_best_effort(ProcessPriority::High)
        .spawn_session(ProcessSessionOptions::default())
        .await
        .expect("best-effort high priority must spawn");
    let (stdout, _) = drain(&session).await;
    assert!(session.wait().await.expect("wait child").is_success());
    let nice = String::from_utf8(stdout).unwrap().trim().to_owned();
    match strict {
        // A privileged host grants the band to both requests.
        Ok(strict) => {
            strict.wait().await.expect("wait strict child");
            assert_eq!(nice, "-5");
        }
        // An unprivileged host refuses the strict request, and the
        // best-effort one runs in the band it would have inherited.
        Err(error) => {
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert_ne!(nice, "-5");
        }
    }
}

#[test]
fn a_bounded_run_honours_best_effort_priority() {
    let output = kernal_api::run_bounded_command(
        helper("process_session_control::exit_code_helper", EXIT_HELPER_ENV)
            .priority_best_effort(ProcessPriority::High),
        Duration::from_secs(10),
        64 * 1024,
    )
    .expect("best-effort high priority must not fail a bounded run");
    assert_eq!(output.exit.raw_code(), 3);
}

#[tokio::test]
async fn session_exit_converts_to_the_native_exit_status() {
    let session = helper("process_session_control::exit_code_helper", EXIT_HELPER_ENV)
        .stdin(StreamMode::Null)
        .stdout(StreamMode::Null)
        .stderr(StreamMode::Null)
        .spawn_session(ProcessSessionOptions::default())
        .await
        .expect("start session");
    let exit = session.wait().await.expect("wait child");
    let status = exit.exit_status();
    assert_eq!(status.code(), Some(3));
    assert_eq!(status.code(), exit.exit_code());
    assert!(!status.success());
}

#[tokio::test]
#[cfg(unix)]
async fn killed_session_exit_status_keeps_the_signal() {
    let session = SpawnSpec::new("sleep")
        .arg("30")
        .stdin(StreamMode::Null)
        .stdout(StreamMode::Null)
        .stderr(StreamMode::Null)
        .spawn_session(ProcessSessionOptions::default())
        .await
        .expect("start session");
    session.kill().await.expect("kill child");
    let exit = session.wait().await.expect("wait child");
    let status = exit.exit_status();
    assert_eq!(status.code(), None);
    assert_eq!(exit.signal(), Some(9));
    assert_eq!(status.to_string(), "signal: 9 (SIGKILL)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_session_serves_output_and_control_from_separate_tasks() {
    let session = Arc::new(
        helper(
            "process_session_control::echo_stdin_helper",
            ECHO_HELPER_ENV,
        )
        .stdin(StreamMode::Piped)
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Null)
        .spawn_session(ProcessSessionOptions {
            max_queued_chunks: 4,
            max_chunk_bytes: 1024,
            ..ProcessSessionOptions::default()
        })
        .await
        .expect("start echo session"),
    );

    let reader = tokio::spawn({
        let session = Arc::clone(&session);
        async move { drain(&session).await }
    });
    let controller = tokio::spawn({
        let session = Arc::clone(&session);
        async move {
            assert!(session.cpu_time().await.is_ok());
            for chunk in [b"hello ".as_slice(), b"session".as_slice()] {
                session.write_stdin(chunk).await.expect("write stdin");
            }
            session.close_stdin().await.expect("close stdin");
            session.wait().await.expect("wait child")
        }
    });

    let exit = kernal_api::async_engine::timeout(Duration::from_secs(30), controller)
        .await
        .expect("control task finishes")
        .expect("control task joins");
    assert!(exit.is_success());
    let (stdout, completions) = kernal_api::async_engine::timeout(Duration::from_secs(30), reader)
        .await
        .expect("output task finishes")
        .expect("output task joins");
    assert!(completions.contains(&ProcessOutputCompletion::StdoutEof));
    assert!(
        stdout.windows(13).any(|window| window == b"hello session"),
        "echoed stdin must arrive on stdout: {:?}",
        String::from_utf8_lossy(&stdout)
    );
    assert!(session.poll().await.expect("poll child").is_some());
}

async fn drain(session: &ProcessSession) -> (Vec<u8>, Vec<ProcessOutputCompletion>) {
    let mut stdout = Vec::new();
    let mut completions = Vec::new();
    while let Some(event) = session.next_output().await {
        match event {
            ProcessOutputEvent::Chunk(ProcessOutputChunk::Stdout(bytes)) => {
                stdout.extend(bytes);
            }
            ProcessOutputEvent::Chunk(ProcessOutputChunk::Stderr(_)) => {}
            ProcessOutputEvent::Completion(completion) => completions.push(completion),
        }
    }
    (stdout, completions)
}
