//! Host-neutral streaming/reaping prerequisites for the compiler guest grant.
//! This exercises the native facade, not the unfinished Wasm cache workflow.

use std::io::Write;
use std::time::Duration;

use kernal_api::{
    ProcessOutputChunk, ProcessOutputCompletion, ProcessOutputEvent, ProcessPostExitDrain,
    ProcessSession, ProcessSessionOptions, SpawnSpec, StreamMode,
};

const HELPER_ENV: &str = "KERNAL_API_STREAMING_PROCESS_HELPER";
const SILENT_HELPER_ENV: &str = "KERNAL_API_SILENT_PROCESS_HELPER";
const STREAM_BYTES: usize = 32 * 1024 * 1024;
const CHUNK_BYTES: usize = 4096;

#[test]
fn streaming_process_helper() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }
    // Non-ASCII bytes distinguish payload from the child libtest harness.
    // Both pipes exceed OS buffering; no shell or platform utility is needed.
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    for _ in 0..STREAM_BYTES / CHUNK_BYTES {
        stdout.write_all(&[0xf1; CHUNK_BYTES]).unwrap();
        stderr.write_all(&[0xf2; CHUNK_BYTES]).unwrap();
    }
    stdout.flush().unwrap();
    stderr.flush().unwrap();
}

#[test]
fn silent_process_helper() {
    if std::env::var_os(SILENT_HELPER_ENV).is_some() {
        std::thread::sleep(Duration::from_secs(30));
    }
}

#[test]
fn output_shutdown_wakes_a_parked_receiver_and_all_callers() {
    assert_output_shutdown(false);
}

#[test]
fn cancelled_output_shutdown_observer_can_be_retried() {
    assert_output_shutdown(true);
}

fn assert_output_shutdown(cancel_first: bool) {
    runtime()
        .run_within(
            async {
                let session = SpawnSpec::new(std::env::current_exe().unwrap())
                    .arg("--exact")
                    .arg("process_session_streaming::silent_process_helper")
                    .arg("--nocapture")
                    .env(SILENT_HELPER_ENV, "1")
                    .stdin(StreamMode::Null)
                    .stdout(StreamMode::Null)
                    .stderr(StreamMode::Piped)
                    .spawn_session(ProcessSessionOptions {
                        kill_on_drop: true,
                        ..ProcessSessionOptions::default()
                    })
                    .await
                    .unwrap();

                // Poll the receive once so it holds the private output mutex while
                // waiting on the silent pipe. Shutdown must signal before taking it.
                let mut receive = std::pin::pin!(session.next_output());
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                assert!(std::future::Future::poll(receive.as_mut(), &mut context).is_pending());
                if cancel_first {
                    let mut abandoned = std::pin::pin!(session.shutdown_output());
                    assert!(
                        std::future::Future::poll(abandoned.as_mut(), &mut context).is_pending()
                    );
                    // The receive still owns the mutex. Dropping this observer must
                    // leave its shutdown request active for subsequent callers.
                }
                let mut first = std::pin::pin!(session.shutdown_output());
                let mut second = std::pin::pin!(session.shutdown_output());
                let result = kernal_api::async_engine::timeout(Duration::from_secs(5), async {
                    let mut done = [false; 3];
                    std::future::poll_fn(|cx| {
                        if !done[0] {
                            if let std::task::Poll::Ready(event) =
                                std::future::Future::poll(receive.as_mut(), cx)
                            {
                                assert!(event.is_none());
                                done[0] = true;
                            }
                        }
                        for (index, shutdown) in
                            [first.as_mut(), second.as_mut()].into_iter().enumerate()
                        {
                            if !done[index + 1] {
                                if let std::task::Poll::Ready(result) =
                                    std::future::Future::poll(shutdown, cx)
                                {
                                    result.unwrap();
                                    done[index + 1] = true;
                                }
                            }
                        }
                        if done.into_iter().all(|value| value) {
                            std::task::Poll::Ready(())
                        } else {
                            std::task::Poll::Pending
                        }
                    })
                    .await;
                    assert!(session.next_output().await.is_none());
                    session.shutdown_output().await.unwrap();
                    assert!(session.poll().await.unwrap().is_none());
                })
                .await;
                // Reap before asserting the deadline, including the failure path.
                session.kill().await.unwrap();
                session.wait().await.unwrap();
                result.expect("output shutdown must wake a parked receiver and every caller");
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

async fn session() -> ProcessSession {
    SpawnSpec::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("process_session_streaming::streaming_process_helper")
        .arg("--nocapture")
        .env(HELPER_ENV, "1")
        .stdin(StreamMode::Null)
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Piped)
        .spawn_session(ProcessSessionOptions {
            max_queued_chunks: 1,
            max_chunk_bytes: CHUNK_BYTES,
            post_exit_drain: ProcessPostExitDrain::AbandonAfter(Duration::from_secs(1)),
            kill_on_drop: true,
        })
        .await
        .unwrap()
}

fn runtime() -> kernal_api::async_engine::Runtime {
    kernal_api::async_engine::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn tagged_output_streams_64_mib_without_whole_output_collection() {
    runtime()
        .run_within(
            async {
                kernal_api::async_engine::timeout(Duration::from_secs(30), async {
                    let session = session().await;
                    let mut counts = [0usize; 2];
                    let mut eof = [false; 2];
                    while let Some(event) = session.next_output().await {
                        match event {
                            ProcessOutputEvent::Chunk(chunk) => {
                                assert!(chunk.bytes().len() <= CHUNK_BYTES);
                                let (stream, payload) = match &chunk {
                                    ProcessOutputChunk::Stdout(_) => (0, 0xf1),
                                    ProcessOutputChunk::Stderr(_) => (1, 0xf2),
                                };
                                assert!(!eof[stream], "payload after stream completion");
                                for byte in chunk.bytes() {
                                    if *byte == payload {
                                        counts[stream] += 1;
                                    } else {
                                        assert_eq!(stream, 0, "unexpected stderr payload");
                                        assert!(byte.is_ascii(), "wrong-stream payload");
                                    }
                                }
                                assert!(counts[stream] <= STREAM_BYTES);
                            }
                            ProcessOutputEvent::Completion(completion) => {
                                let stream = match completion {
                                    ProcessOutputCompletion::StdoutEof => 0,
                                    ProcessOutputCompletion::StderrEof => 1,
                                    other => panic!("unexpected output completion: {other:?}"),
                                };
                                assert!(!eof[stream], "duplicate completion");
                                eof[stream] = true;
                            }
                        }
                    }
                    assert_eq!(counts, [STREAM_BYTES; 2]);
                    assert_eq!(eof, [true; 2]);
                    assert_eq!(session.wait().await.unwrap().exit_code(), Some(0));
                })
                .await
                .expect("bounded-output fixture deadline");
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn kill_reaps_without_draining_backpressured_output() {
    runtime()
        .run_within(
            async {
                let session = session().await;
                kernal_api::async_engine::timeout(Duration::from_secs(10), async {
                    // Wait for actual fixture output rather than racing process startup.
                    loop {
                        let event = session.next_output().await.expect("fixture output");
                        if let ProcessOutputEvent::Chunk(chunk) = event {
                            if chunk.bytes().iter().any(|byte| matches!(byte, 0xf1 | 0xf2)) {
                                break;
                            }
                        }
                    }
                    // Stop receiving: the one-chunk queue and pipes cannot retain the
                    // fixture's remaining output. Lifecycle control must stay usable.
                    kernal_api::async_engine::sleep(Duration::from_millis(50)).await;
                    assert!(session.poll().await.unwrap().is_none());
                    session.kill().await.unwrap();
                    assert!(session.poll().await.unwrap().is_some());
                    assert_ne!(session.wait().await.unwrap().exit_code(), Some(0));
                })
                .await
                .expect("kill and reap must not require output delivery");
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

/// Consumers build the fault their own completion handling receives, so that
/// handling is testable without provoking a real pipe failure.
#[test]
fn an_output_fault_is_constructible_from_its_portable_parts() {
    let fault = kernal_api::ProcessOutputFault::new(
        std::io::ErrorKind::BrokenPipe,
        "pipe closed",
        Some(32),
    );
    assert_eq!(fault.kind(), std::io::ErrorKind::BrokenPipe);
    assert_eq!(fault.message(), "pipe closed");
    assert_eq!(fault.raw_os_error(), Some(32));
    let completion = ProcessOutputCompletion::StderrError(fault.clone());
    assert_eq!(completion, ProcessOutputCompletion::StderrError(fault));
}
