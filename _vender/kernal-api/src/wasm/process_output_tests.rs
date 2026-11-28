use super::super::output::CompilerOutputError;
use super::*;
use crate::{ProcessOutputChunk, ProcessOutputEvent};

#[path = "process_output_wire_tests.rs"]
mod wire;

#[test]
fn output_is_tagged_bounded_and_one_uncollected_event_per_process() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                let hub = OperationHub::new(4, 2).unwrap();
                let grant = hub
                    .grant_compiler(7, spec("dual"), Duration::from_secs(15))
                    .unwrap();
                let operation = hub
                    .submit_compiler_spawn(runtime.handle(), 7, grant)
                    .unwrap();
                let (process, session) = published(&hub, operation).await;
                let mut counts = [0usize; 2];
                let mut total = 0usize;
                loop {
                    let read = hub.begin_compiler_output(7, process).unwrap();
                    assert!(matches!(
                        hub.begin_compiler_output(7, process),
                        Err(HubError::Quota)
                    ));
                    let output = read.receive().await.unwrap();
                    assert_eq!(
                        hub.snapshot().reserved_process_output_bytes,
                        MAX_PROCESS_OUTPUT_CHUNK
                    );
                    assert!(matches!(
                        hub.begin_compiler_output(7, process),
                        Err(HubError::Quota)
                    ));
                    let ended = output
                        .collect(|event| {
                            let ended = event.is_none();
                            if let Some(ProcessOutputEvent::Chunk(chunk)) = event {
                                total += chunk.bytes().len();
                                let (index, marker) = match chunk {
                                    ProcessOutputChunk::Stdout(_) => (0, 0xf1),
                                    ProcessOutputChunk::Stderr(_) => (1, 0xf2),
                                };
                                counts[index] +=
                                    chunk.bytes().iter().filter(|byte| **byte == marker).count();
                                assert!(chunk.bytes().len() <= MAX_PROCESS_OUTPUT_CHUNK);
                            }
                            ended
                        })
                        .unwrap();
                    {
                        let state = hub.state.lock().unwrap();
                        let ResourceValue::CompilerProcess(value) =
                            &state.resources[&process].value
                        else {
                            panic!("process");
                        };
                        assert_eq!(value.output_bytes, total);
                    }
                    assert_eq!(hub.snapshot().reserved_process_output_bytes, 0);
                    if ended {
                        break;
                    }
                }
                assert_eq!(counts, [2 * 1024 * 1024; 2]);
                let exit = hub.wait_compiler(7, process).await.unwrap();
                assert_eq!(exit.exit_code(), Some(0));
                assert_eq!(hub.wait_compiler(7, process).await.unwrap(), exit);
                assert_eq!(session.wait().await.unwrap(), exit);
                hub.close_all(Terminal::Closed);
                hub.join_process_jobs().await.unwrap();
                assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn unacknowledged_output_revokes_process_and_holds_credit_until_drop() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                for revoke_first in [false, true] {
                    let hub = OperationHub::new(4, 2).unwrap();
                    let grant = hub
                        .grant_compiler(7, spec("blocked"), Duration::from_secs(10))
                        .unwrap();
                    let operation = hub
                        .submit_compiler_spawn(runtime.handle(), 7, grant)
                        .unwrap();
                    let (process, session) = published(&hub, operation).await;
                    let output = hub
                        .begin_compiler_output(7, process)
                        .unwrap()
                        .receive()
                        .await
                        .unwrap();
                    if revoke_first {
                        hub.close_resource(process).unwrap();
                    }
                    assert_eq!(
                        hub.snapshot().reserved_process_output_bytes,
                        MAX_PROCESS_OUTPUT_CHUNK
                    );
                    drop(output);
                    assert_eq!(hub.snapshot().reserved_process_output_bytes, 0);
                    assert!(matches!(
                        hub.begin_compiler_output(7, process),
                        Err(HubError::Closed)
                    ));
                    hub.close_all(Terminal::Closed);
                    hub.join_process_jobs().await.unwrap();
                    assert!(session.poll().await.unwrap().is_some());
                }
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn cumulative_output_rejects_before_exposing_the_overflow_event() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                let hub = OperationHub::new(4, 2).unwrap();
                let grant = hub
                    .grant_compiler(7, spec("blocked"), Duration::from_secs(10))
                    .unwrap();
                let operation = hub
                    .submit_compiler_spawn(runtime.handle(), 7, grant)
                    .unwrap();
                let (process, session) = published(&hub, operation).await;
                {
                    let mut state = hub.state.lock().unwrap();
                    let ResourceValue::CompilerProcess(value) =
                        &mut state.resources.get_mut(&process).unwrap().value
                    else {
                        panic!("process");
                    };
                    value.output_limit = 1;
                }
                assert!(matches!(
                    hub.begin_compiler_output(7, process)
                        .unwrap()
                        .receive()
                        .await,
                    Err(CompilerOutputError::Admission(HubError::Quota))
                ));
                assert!(!hub.state.lock().unwrap().resources.contains_key(&process));
                assert_eq!(hub.snapshot().reserved_process_output_bytes, 0);
                hub.close_all(Terminal::Closed);
                hub.join_process_jobs().await.unwrap();
                assert!(session.poll().await.unwrap().is_some());
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn transfer_budget_and_owner_reject_before_output_is_consumed() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                let hub = OperationHub::new(4, 2).unwrap();
                let grant = hub
                    .grant_compiler(7, spec("exit"), Duration::from_secs(10))
                    .unwrap();
                let operation = hub
                    .submit_compiler_spawn(runtime.handle(), 7, grant)
                    .unwrap();
                let (process, _) = published(&hub, operation).await;
                assert!(matches!(
                    hub.begin_compiler_output(8, process),
                    Err(HubError::WrongRights)
                ));
                hub.state.lock().unwrap().native_transfer_capacity =
                    hub.blob_limits.maximum_sketch_bytes;
                assert!(matches!(
                    hub.begin_compiler_output(7, process),
                    Err(HubError::Quota)
                ));
                hub.state.lock().unwrap().native_transfer_capacity = 0;
                let read = hub.begin_compiler_output(7, process).unwrap();
                drop(read);
                assert_eq!(hub.snapshot().reserved_process_output_bytes, 0);
                hub.close_all(Terminal::Closed);
                hub.join_process_jobs().await.unwrap();
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn cancellation_after_receive_cannot_acknowledge_uncertain_output() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                let hub = OperationHub::new(4, 2).unwrap();
                let grant = hub
                    .grant_compiler(7, spec("blocked"), Duration::from_secs(10))
                    .unwrap();
                let spawn = hub
                    .submit_compiler_spawn(runtime.handle(), 7, grant)
                    .unwrap();
                let (process, _) = published(&hub, spawn).await;
                let output = hub
                    .begin_compiler_output(7, process)
                    .unwrap()
                    .receive()
                    .await
                    .unwrap();
                let operation = {
                    let state = hub.state.lock().unwrap();
                    *state
                        .operations
                        .iter()
                        .find(|(_, op)| op.resource == Some(process))
                        .unwrap()
                        .0
                };
                hub.cancel_wire(7, operation.wire()).unwrap();
                let mut delivered = false;
                assert_eq!(
                    output.collect(|_| {
                        delivered = true;
                    }),
                    Err(CompilerOutputError::Terminal(Terminal::Cancelled))
                );
                assert!(!delivered);
                assert!(!hub.state.lock().unwrap().resources.contains_key(&process));
                hub.close_all(Terminal::Closed);
                hub.join_process_jobs().await.unwrap();
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn dropping_a_parked_output_read_revokes_and_releases_its_credit() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                let hub = OperationHub::new(4, 2).unwrap();
                let grant = hub
                    .grant_compiler(7, spec("silent"), Duration::from_secs(10))
                    .unwrap();
                let spawn = hub
                    .submit_compiler_spawn(runtime.handle(), 7, grant)
                    .unwrap();
                let (process, session) = published(&hub, spawn).await;
                loop {
                    let read = hub.begin_compiler_output(7, process).unwrap();
                    let mut receive = Box::pin(read.receive());
                    match crate::async_engine::timeout(Duration::from_millis(20), &mut receive)
                        .await
                    {
                        Ok(result) => {
                            result.unwrap().collect(|_| ()).unwrap();
                        }
                        Err(_) => {
                            assert_eq!(
                                hub.snapshot().reserved_process_output_bytes,
                                MAX_PROCESS_OUTPUT_CHUNK
                            );
                            drop(receive);
                            break;
                        }
                    }
                }
                assert!(!hub.state.lock().unwrap().resources.contains_key(&process));
                assert_eq!(hub.snapshot().reserved_process_output_bytes, 0);
                hub.close_all(Terminal::Closed);
                hub.join_process_jobs().await.unwrap();
                assert!(session.poll().await.unwrap().is_some());
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn collection_panic_reclaims_without_poisoning_the_authority() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                let hub = OperationHub::new(4, 2).unwrap();
                let grant = hub
                    .grant_compiler(7, spec("blocked"), Duration::from_secs(10))
                    .unwrap();
                let spawn = hub
                    .submit_compiler_spawn(runtime.handle(), 7, grant)
                    .unwrap();
                let (process, session) = published(&hub, spawn).await;
                let output = hub
                    .begin_compiler_output(7, process)
                    .unwrap()
                    .receive()
                    .await
                    .unwrap();
                assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = output.collect::<()>(|_| panic!("injected collection failure"));
                }))
                .is_err());
                assert!(!hub.state.lock().unwrap().resources.contains_key(&process));
                assert_eq!(hub.snapshot().reserved_process_output_bytes, 0);
                hub.close_all(Terminal::Closed);
                hub.join_process_jobs().await.unwrap();
                assert!(session.poll().await.unwrap().is_some());
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn pending_receive_preserves_each_revocation_terminal() {
    let runtime = runtime();
    runtime.run_within(async {
        for terminal in [Terminal::Cancelled, Terminal::TimedOut, Terminal::Trapped, Terminal::OwnerExited, Terminal::Closed] {
            let hub = OperationHub::new(4, 2).unwrap();
            let grant = hub.grant_compiler(7, spec("silent"), Duration::from_secs(10)).unwrap();
            let spawn = hub.submit_compiler_spawn(runtime.handle(), 7, grant).unwrap();
            let (process, _) = published(&hub, spawn).await;
            let read = hub.begin_compiler_output(7, process).unwrap();
            hub.revoke_external_resource(process, terminal).unwrap();
            assert!(matches!(read.receive().await, Err(CompilerOutputError::Terminal(actual)) if actual == terminal));
            hub.close_all(Terminal::Closed);
            hub.join_process_jobs().await.unwrap();
            assert_eq!(hub.snapshot().reserved_process_output_bytes, 0);
        }
    }, std::time::Duration::from_secs(60)).expect("test runtime budget");
}
