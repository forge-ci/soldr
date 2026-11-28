use super::*;

async fn process(hub: &Arc<OperationHub>, runtime: &Runtime, mode: &str) -> OpaqueToken {
    let grant = hub
        .grant_compiler(7, spec(mode), Duration::from_secs(15))
        .unwrap();
    let spawn = hub
        .submit_compiler_spawn(runtime.handle(), 7, grant)
        .unwrap();
    let (process, _) = published(hub, spawn).await;
    assert_eq!(hub.poll_wire(7, spawn.wire()) >> 8, process.wire());
    process
}

async fn ready(hub: &OperationHub, operation: OpaqueToken) {
    crate::async_engine::timeout(Duration::from_secs(5), async {
        loop {
            if hub.state.lock().unwrap().operations[&operation]
                .terminal
                .is_some()
            {
                break;
            }
            crate::async_engine::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn compiler_wire_wait_publishes_exit_and_ready_revocation_preserves_reason() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                let hub = OperationHub::new(4, 2).unwrap();
                let process = process(&hub, &runtime, "exit").await;
                loop {
                    let output = hub
                        .begin_compiler_output(7, process)
                        .unwrap()
                        .receive()
                        .await
                        .unwrap();
                    if output.collect(|event| event.is_none()).unwrap() {
                        break;
                    }
                }
                let wait = hub
                    .submit_compiler_wait(runtime.handle(), 7, process)
                    .unwrap();
                ready(&hub, wait).await;
                assert_eq!(hub.poll_wire(7, wait.wire()), 0x80);
                assert_eq!(hub.collect_compiler_wait(8, wait), Err(HubError::Stale));
                assert_eq!(hub.collect_compiler_wait(7, wait), Ok((3_u64 << 40) | 1));
                let wait = hub
                    .submit_compiler_wait(runtime.handle(), 7, process)
                    .unwrap();
                ready(&hub, wait).await;
                hub.revoke_external_resource(process, Terminal::TimedOut)
                    .unwrap();
                assert_eq!(hub.collect_compiler_wait(7, wait), Ok(3));
                hub.close_all(Terminal::Closed);
                hub.join_process_jobs().await.unwrap();
                assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn compiler_wire_cancelled_wait_retains_producer_quota_without_revoking_process() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                let hub = OperationHub::new(1, 2).unwrap();
                let process = process(&hub, &runtime, "silent").await;
                let wait = hub
                    .submit_compiler_wait(runtime.handle(), 7, process)
                    .unwrap();
                assert_eq!(
                    hub.abandon_compiler_scalar(7, wait, true),
                    Err(HubError::WrongKind)
                );
                hub.abandon_compiler_scalar(7, wait, false).unwrap();
                // No scheduling turn: dropping the guest observation cannot recycle
                // a producer that has not yet processed its cancellation.
                assert_eq!(
                    hub.submit_compiler_wait(runtime.handle(), 7, process),
                    Err(HubError::Quota)
                );
                let read = hub.begin_compiler_output(7, process).unwrap();
                drop(read);
                hub.close_all(Terminal::Closed);
                hub.join_process_jobs().await.unwrap();
                assert_eq!(hub.snapshot().retained_process_jobs, 0);
                assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn compiler_wire_close_waits_for_acknowledgement_without_closing_its_own_operation() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                let hub = OperationHub::new(4, 2).unwrap();
                let process = process(&hub, &runtime, "silent").await;
                let (started, entered) = crate::async_engine::oneshot_channel();
                let (resume, resumed) = crate::async_engine::oneshot_channel();
                *hub.process_cleanup_checkpoint.lock().unwrap() = Some(CleanupCheckpoint {
                    started,
                    resume: resumed,
                });
                let close = hub
                    .submit_compiler_close(runtime.handle(), 7, process)
                    .unwrap();
                assert!(!hub.state.lock().unwrap().resources.contains_key(&process));
                entered.await.unwrap();
                assert_eq!(hub.poll_wire(7, close.wire()), 0);
                assert_eq!(
                    hub.abandon_compiler_scalar(7, close, false),
                    Err(HubError::WrongKind)
                );
                resume.send(CleanupFault::None).ok().unwrap();
                ready(&hub, close).await;
                assert_eq!(hub.poll_wire(7, close.wire()), 1);
                assert_eq!(hub.snapshot().reserved_native_process_output_bytes, 0);
                hub.close_all(Terminal::Closed);
                hub.join_process_jobs().await.unwrap();
                assert_eq!(hub.snapshot().retained_process_jobs, 0);
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn compiler_wire_output_moves_credit_and_invalid_collection_is_retryable() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                let hub = OperationHub::new(4, 2).unwrap();
                let process = process(&hub, &runtime, "exit").await;
                let read = hub
                    .submit_compiler_output(runtime.handle(), 7, process)
                    .unwrap();
                assert_eq!(
                    hub.snapshot().reserved_process_output_bytes,
                    MAX_PROCESS_OUTPUT_CHUNK
                );
                ready(&hub, read).await;
                assert_eq!(
                    hub.snapshot().reserved_process_output_bytes,
                    MAX_PROCESS_OUTPUT_CHUNK
                );
                assert_eq!(
                    hub.snapshot().retained_transfer_capacity,
                    NATIVE_PROCESS_OUTPUT_ALLOWANCE + MAX_PROCESS_OUTPUT_CHUNK
                );
                assert_eq!(hub.poll_wire(7, read.wire()), 0x80);
                assert_eq!(
                    hub.collect_compiler_output_wire(
                        8,
                        read,
                        MAX_PROCESS_OUTPUT_CHUNK,
                        |_| panic!("foreign copy")
                    ),
                    Err(HubError::Stale)
                );
                assert_eq!(
                    hub.collect_compiler_output_wire(
                        7,
                        read,
                        MAX_PROCESS_OUTPUT_CHUNK - 1,
                        |_| panic!("short copy")
                    ),
                    Err(HubError::Quota)
                );
                assert_eq!(
                    hub.submit_compiler_output(runtime.handle(), 7, process),
                    Err(HubError::Quota)
                );
                assert_eq!(
                    hub.collect_compiler_output_wire(7, read, MAX_PROCESS_OUTPUT_CHUNK, |event| {
                        assert!(event.is_some());
                        17
                    }),
                    Ok((17 << 8) | 1)
                );
                assert_eq!(
                    hub.snapshot().retained_transfer_capacity,
                    NATIVE_PROCESS_OUTPUT_ALLOWANCE
                );
                assert!(!hub.state.lock().unwrap().operations.contains_key(&read));
                let next = hub
                    .submit_compiler_output(runtime.handle(), 7, process)
                    .unwrap();
                hub.abandon_compiler_output_wire(7, next).unwrap();
                hub.close_all(Terminal::Closed);
                hub.join_process_jobs().await.unwrap();
                assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
                assert_eq!(hub.snapshot().retained_process_jobs, 0);
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn compiler_wire_ready_output_is_revocable_with_each_terminal_reason() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                for terminal in [
                    Terminal::Cancelled,
                    Terminal::TimedOut,
                    Terminal::Trapped,
                    Terminal::OwnerExited,
                    Terminal::Closed,
                ] {
                    let hub = OperationHub::new(4, 2).unwrap();
                    let process = process(&hub, &runtime, "exit").await;
                    let read = hub
                        .submit_compiler_output(runtime.handle(), 7, process)
                        .unwrap();
                    ready(&hub, read).await;
                    if terminal == Terminal::Cancelled {
                        hub.cancel_wire(7, read.wire()).unwrap();
                    } else {
                        hub.revoke_external_resource(process, terminal).unwrap();
                    }
                    assert!(hub.state.lock().unwrap().operations[&read]
                        .compiler_output
                        .is_none());
                    assert_eq!(
                        hub.collect_compiler_output_wire(
                            7,
                            read,
                            MAX_PROCESS_OUTPUT_CHUNK,
                            |_| panic!("revoked payload")
                        ),
                        Ok(u64::from(status(terminal)))
                    );
                    assert_eq!(hub.snapshot().reserved_process_output_bytes, 0);
                    hub.close_all(Terminal::Closed);
                    hub.join_process_jobs().await.unwrap();
                    assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
                    assert_eq!(hub.snapshot().retained_process_jobs, 0);
                }
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn compiler_wire_pending_and_ready_teardown_join_their_producers() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                for wait_for_ready in [false, true] {
                    let hub = OperationHub::new(4, 2).unwrap();
                    let process = process(
                        &hub,
                        &runtime,
                        if wait_for_ready { "exit" } else { "silent" },
                    )
                    .await;
                    let read = hub
                        .submit_compiler_output(runtime.handle(), 7, process)
                        .unwrap();
                    if wait_for_ready {
                        ready(&hub, read).await;
                    }
                    // No scheduler turn between submit and close for the pending case.
                    hub.close_all(Terminal::Trapped);
                    hub.join_process_jobs().await.unwrap();
                    assert_eq!(
                        hub.collect_compiler_output_wire(
                            7,
                            read,
                            MAX_PROCESS_OUTPUT_CHUNK,
                            |_| panic!("closed payload")
                        ),
                        Ok(u64::from(status(Terminal::Trapped)))
                    );
                    assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
                    assert_eq!(hub.snapshot().retained_process_jobs, 0);
                    assert_eq!(hub.snapshot().pending_operations, 0);
                    assert!(hub.state.lock().unwrap().operations.is_empty());
                }
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn compiler_wire_exhaustion_keeps_a_full_credit_until_abandonment() {
    let runtime = runtime();
    runtime
        .run_within(
            async {
                let hub = OperationHub::new(4, 2).unwrap();
                let process = process(&hub, &runtime, "exit").await;
                loop {
                    let read = hub
                        .submit_compiler_output(runtime.handle(), 7, process)
                        .unwrap();
                    ready(&hub, read).await;
                    let exhausted = hub.state.lock().unwrap().operations[&read]
                        .compiler_output
                        .as_ref()
                        .unwrap()
                        .is_none();
                    if exhausted {
                        assert_eq!(
                            hub.snapshot().retained_transfer_capacity,
                            NATIVE_PROCESS_OUTPUT_ALLOWANCE + MAX_PROCESS_OUTPUT_CHUNK
                        );
                        hub.abandon_compiler_output_wire(7, read).unwrap();
                        break;
                    }
                    hub.collect_compiler_output_wire(7, read, MAX_PROCESS_OUTPUT_CHUNK, |_| 0)
                        .unwrap();
                }
                hub.close_all(Terminal::Closed);
                hub.join_process_jobs().await.unwrap();
                assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
                assert_eq!(hub.snapshot().retained_process_jobs, 0);
                assert!(hub.state.lock().unwrap().operations.is_empty());
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}
