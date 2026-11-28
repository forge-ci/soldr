//! Public conformance checks for the bounded streaming process-session facade.

#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use kernal_api::ProcessPostExitDrain;

#[cfg(target_os = "linux")]
use kernal_api::run_bounded_command;
#[cfg(target_os = "linux")]
use kernal_api::ProcessOutputCompletion;
use kernal_api::{shell_spec, ProcessPriority, ProcessSessionOptions, SpawnSpec, StreamMode};
#[cfg(unix)]
use kernal_api::{ProcessOutputChunk, ProcessOutputEvent};

fn streamed_shell(command: &str) -> SpawnSpec {
    shell_spec(command)
        .stdin(StreamMode::Piped)
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Piped)
}

#[tokio::test]
#[cfg(unix)]
async fn first_output_chunk_arrives_before_direct_child_exit() {
    let session = streamed_shell("printf first; sleep 1")
        .spawn_session(ProcessSessionOptions {
            max_queued_chunks: 2,
            max_chunk_bytes: 64,
            post_exit_drain: ProcessPostExitDrain::AbandonAfter(Duration::from_millis(50)),
            kill_on_drop: true,
        })
        .await
        .expect("start session");

    let first =
        kernal_api::async_engine::timeout(Duration::from_millis(300), session.next_output())
            .await
            .expect("the first chunk must not wait for child exit");
    assert!(matches!(
        first,
        Some(ProcessOutputEvent::Chunk(ProcessOutputChunk::Stdout(bytes))) if bytes == b"first"
    ));
    assert!(session.poll().await.expect("poll child").is_none());
    assert_eq!(
        session.wait().await.expect("wait child").exit_code(),
        Some(0)
    );
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn direct_exit_is_observable_while_output_waits_on_a_descendant_pipe() {
    let session = streamed_shell("(sleep 0.5) &")
        .spawn_session(ProcessSessionOptions {
            max_queued_chunks: 2,
            max_chunk_bytes: 64,
            post_exit_drain: ProcessPostExitDrain::AbandonAfter(Duration::from_millis(75)),
            kill_on_drop: true,
        })
        .await
        .expect("start session");

    let output = session.next_output();
    tokio::pin!(output);
    assert!(
        kernal_api::async_engine::timeout(Duration::from_millis(20), &mut output)
            .await
            .is_err(),
        "output must remain pending while the descendant retains both pipes"
    );

    let exit = kernal_api::async_engine::timeout(Duration::from_secs(1), session.wait())
        .await
        .expect("direct child exit must not wait for inherited pipe EOF")
        .expect("wait child");
    assert_eq!(exit.exit_code(), Some(0));
    let completion = kernal_api::async_engine::timeout(Duration::from_secs(1), &mut output)
        .await
        .expect("the held pipe must complete by explicit grace abandonment")
        .expect("an output completion event");
    assert!(matches!(
        completion,
        ProcessOutputEvent::Completion(
            ProcessOutputCompletion::StdoutAbandoned | ProcessOutputCompletion::StderrAbandoned
        )
    ));
}

// Contract test of `WaitForEof`, deprecated by #433.
#[allow(deprecated)]
#[tokio::test]
#[cfg(target_os = "linux")]
async fn wait_for_eof_preserves_the_explicit_unbounded_post_exit_contract() {
    let session = streamed_shell("(sleep 0.25) &")
        .spawn_session(ProcessSessionOptions {
            max_queued_chunks: 2,
            max_chunk_bytes: 64,
            post_exit_drain: ProcessPostExitDrain::WaitForEof,
            kill_on_drop: true,
        })
        .await
        .expect("start session");

    assert_eq!(
        session.wait().await.expect("wait direct child").exit_code(),
        Some(0)
    );
    assert!(
        kernal_api::async_engine::timeout(Duration::from_millis(20), session.next_output())
            .await
            .is_err(),
        "WaitForEof must not turn a descendant-held pipe into an immediate abandonment"
    );
    let completion =
        kernal_api::async_engine::timeout(Duration::from_secs(1), session.next_output())
            .await
            .expect("the inherited pipe eventually reaches EOF")
            .expect("stream completion");
    assert!(matches!(
        completion,
        ProcessOutputEvent::Completion(
            ProcessOutputCompletion::StdoutEof | ProcessOutputCompletion::StderrEof
        )
    ));
}

#[tokio::test]
#[cfg(unix)]
async fn a_full_output_queue_applies_backpressure_without_losing_chunks() {
    let session = streamed_shell("printf 12345678")
        .spawn_session(ProcessSessionOptions {
            max_queued_chunks: 1,
            max_chunk_bytes: 1,
            post_exit_drain: ProcessPostExitDrain::AbandonAfter(Duration::from_millis(50)),
            kill_on_drop: true,
        })
        .await
        .expect("start session");

    let mut stdout = Vec::new();
    while let Some(event) = session.next_output().await {
        if let ProcessOutputEvent::Chunk(ProcessOutputChunk::Stdout(bytes)) = event {
            stdout.extend(bytes);
        }
    }
    assert_eq!(
        session.wait().await.expect("wait child").exit_code(),
        Some(0)
    );
    assert_eq!(stdout, b"12345678");
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn kill_on_drop_terminates_and_reaps_the_direct_child() {
    let session = streamed_shell("sleep 30")
        .spawn_session(ProcessSessionOptions::default())
        .await
        .expect("start session");
    let pid = session.id();
    drop(session);
    assert_pid_gone(pid).await;
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn detach_on_drop_keeps_draining_then_reaps_the_direct_child() {
    let session = streamed_shell("printf drained; sleep 0.1")
        .spawn_session(ProcessSessionOptions {
            kill_on_drop: false,
            ..ProcessSessionOptions::default()
        })
        .await
        .expect("start session");
    let pid = session.id();
    drop(session);
    assert_pid_gone(pid).await;
}

#[tokio::test]
#[cfg(unix)]
async fn blocked_stdin_never_blocks_direct_kill_or_wait() {
    let session = streamed_shell("sleep 30")
        .spawn_session(ProcessSessionOptions {
            max_queued_chunks: 1,
            max_chunk_bytes: 1024 * 1024,
            ..ProcessSessionOptions::default()
        })
        .await
        .expect("start session");
    let input = vec![b'x'; 1024 * 1024];
    let write = session.write_stdin(&input);
    tokio::pin!(write);
    assert!(
        kernal_api::async_engine::timeout(Duration::from_millis(20), &mut write)
            .await
            .is_err(),
        "a megabyte write to a child that never reads stdin must be pending"
    );
    let (write, kill) = tokio::join!(&mut write, session.kill());
    assert!(
        kill.is_ok(),
        "kill must not wait for a child that ignores stdin: {kill:?}"
    );
    let _ = write;
    assert!(
        kernal_api::async_engine::timeout(Duration::from_secs(1), session.wait())
            .await
            .expect("wait must remain live after a blocked stdin write")
            .is_ok()
    );
}

#[tokio::test]
async fn priority_is_semantic() {
    let session = streamed_shell("exit 0")
        .priority(ProcessPriority::Low)
        .spawn_session(ProcessSessionOptions::default())
        .await
        .expect("start low-priority session");
    let exit = session.wait().await.expect("wait child");
    assert_eq!(exit.exit_code(), Some(0));
}

#[test]
#[cfg(target_os = "linux")]
fn bounded_priority_is_forwarded_to_the_native_launch_policy() {
    let output = run_bounded_command(
        shell_spec("ps -o ni= -p $$").priority(ProcessPriority::Low),
        Duration::from_secs(2),
        1024,
    )
    .expect("run bounded background process");
    assert_eq!(output.exit.raw_code(), 0);
    assert_eq!(
        String::from_utf8(output.stdout)
            .expect("ps output is UTF-8")
            .trim(),
        "10"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn idle_priority_retains_a_distinct_native_band() {
    let output = run_bounded_command(
        shell_spec("ps -o ni= -p $$").priority(ProcessPriority::Idle),
        Duration::from_secs(2),
        1024,
    )
    .expect("run bounded idle process");
    assert_eq!(output.exit.raw_code(), 0);
    assert_eq!(
        String::from_utf8(output.stdout)
            .expect("ps output is UTF-8")
            .trim(),
        "19"
    );
}

#[tokio::test]
#[cfg(not(windows))]
async fn cpu_time_unavailable_is_none() {
    let session = streamed_shell("exit 0")
        .spawn_session(ProcessSessionOptions::default())
        .await
        .expect("start session");
    assert_eq!(
        session.wait().await.expect("wait child").exit_code(),
        Some(0)
    );
    assert_eq!(session.cpu_time().await.expect("sample cpu"), None);
}

#[tokio::test]
#[cfg(unix)]
async fn facade_exit_preserves_signal_and_native_status() {
    let session = streamed_shell("kill -TERM $$")
        .spawn_session(ProcessSessionOptions::default())
        .await
        .expect("start session");
    let exit = session.wait().await.expect("wait child");
    assert_eq!(exit.exit_code(), None);
    assert_eq!(exit.signal(), Some(15));
    assert_ne!(exit.native_status(), 0);
}

#[tokio::test]
#[cfg(windows)]
async fn facade_exit_preserves_windows_raw_status_bits() {
    let session = streamed_shell("exit /b -1073741819")
        .spawn_session(ProcessSessionOptions::default())
        .await
        .expect("start session");
    let exit = session.wait().await.expect("wait child");
    assert_eq!(exit.exit_code(), Some(0xC000_0005_u32 as i32));
    assert_eq!(exit.signal(), None);
    assert_eq!(exit.native_status(), 0xC000_0005);
}

#[cfg(target_os = "linux")]
async fn assert_pid_gone(pid: u32) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "child {pid} survived session cleanup"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
