//! Native authenticated entry-to-Blob bridge on the tracked archive job lane.
use super::*;
use std::io;

impl OperationHub {
    pub(crate) fn submit_archive_entry_open(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        store: u64,
        entry: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        let producer_runtime = runtime.clone();
        self.submit_archive_work(
            runtime,
            store,
            (
                entry,
                archive_inventory::ENTRY_KIND,
                archive_inventory::ENTRY_RIGHT,
            ),
            move |hub, operation| {
                let (archive, index) = {
                    let state = hub.state.lock().map_err(|_| HubError::Closed)?;
                    let slot = state.resources.get(&entry).ok_or(HubError::Closed)?;
                    Self::validate_resource(
                        slot,
                        store,
                        archive_inventory::ENTRY_KIND,
                        archive_inventory::ENTRY_RIGHT,
                    )?;
                    let ResourceValue::ArchiveEntry(entry) = &slot.value else {
                        return Err(HubError::WrongKind);
                    };
                    (Arc::clone(&entry.archive), entry.index)
                };
                let mut sink = NativeArchiveSink::new(Arc::clone(hub), producer_runtime, store)?;
                let mut state = hub.state.lock().map_err(|_| HubError::Closed)?;
                if !state.resources.contains_key(&entry)
                    || state
                        .operations
                        .get(&operation)
                        .is_none_or(|slot| slot.terminal.is_some())
                {
                    return Err(HubError::Closed);
                }
                state
                    .operations
                    .get_mut(&operation)
                    .ok_or(HubError::Closed)?
                    .created_resource = Some(sink.blob());
                let notify = Self::terminal_locked(
                    &mut state,
                    operation,
                    TerminalResult {
                        terminal: Terminal::Completed,
                        resource: Some(sink.blob()),
                    },
                )?;
                // Transfer entry authority exactly once. Publication must
                // precede copying: the consumer releases producer capacity.
                let notifications =
                    Self::close_resource_with_terminal_locked(&mut state, entry, Terminal::Closed)?;
                drop(state);
                if let Some(notify) = notify {
                    notify.notify_one();
                }
                for notify in notifications {
                    notify.notify_one();
                }
                {
                    // Never acquire this reader mutex while holding hub state.
                    let mut reader = archive.lock().map_err(|_| HubError::Closed)?;
                    reader
                        .reader
                        .copy_entry(index, &mut sink)
                        .map_err(|_| HubError::Invalid)?;
                }
                // Successful EOF only follows a fully checked entry copy.
                sink.finish().map_err(|_| HubError::Closed)
            },
        )
    }
}

/// Producer authority is this non-cloneable value, never a consumer token.
/// Constructed only alongside a new read-only blob; no token-adoption method.
struct NativeArchiveSink {
    hub: Arc<OperationHub>,
    runtime: crate::async_engine::RuntimeHandle,
    store: u64,
    blob: OpaqueToken,
    failed: bool,
    finished: bool,
}

impl NativeArchiveSink {
    fn new(
        hub: Arc<OperationHub>,
        runtime: crate::async_engine::RuntimeHandle,
        store: u64,
    ) -> Result<Self, HubError> {
        let blob = hub.create_resource_value(
            store,
            BLOB_RESOURCE_KIND,
            BLOB_RIGHT_READ,
            false,
            ResourceValue::Blob {
                buffer: VecDeque::new(),
                sealed: false,
            },
        )?;
        {
            let mut state = hub.state.lock().map_err(|_| HubError::Closed)?;
            state
                .resources
                .get_mut(&blob)
                .ok_or(HubError::Closed)?
                .reserved = false;
        }
        Ok(Self {
            hub,
            runtime,
            store,
            blob,
            failed: false,
            finished: false,
        })
    }

    fn blob(&self) -> OpaqueToken {
        self.blob
    }

    /// Called from the supplied runtime's blocking worker. The owned reader
    /// retains authenticated staging while bounded writes wait for capacity.
    fn copy_archive(
        mut self,
        mut reader: crate::archive::authenticated_staging::AuthenticatedReader,
        index: usize,
    ) -> io::Result<u64> {
        let copied = reader.copy_entry(index, &mut self)?;
        self.finish()?;
        Ok(copied)
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.failed || self.finished {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        self.failed = true;
        self.hub
            .seal_blob_checked(self.store, self.blob, BLOB_RIGHT_READ)
            .map_err(transfer_error)?;
        self.failed = false;
        self.finished = true;
        Ok(())
    }
}

fn transfer_error(error: HubError) -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        format!("archive blob transfer: {error:?}"),
    )
}

struct PendingArchiveWrite<'a> {
    hub: &'a OperationHub,
    store: u64,
    operation: OpaqueToken,
}

impl Drop for PendingArchiveWrite<'_> {
    fn drop(&mut self) {
        // Collection may already have removed it. Otherwise revoke both the
        // operation and retained bytes on every early return or unwind.
        let _ = self
            .hub
            .abandon_transfer_wire(self.store, self.operation.wire());
    }
}

impl io::Write for NativeArchiveSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.failed || self.finished {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        self.failed = true;
        let count = bytes.len().min(self.hub.blob_limits.maximum_chunk_bytes);
        // Only the private producer reaches this path. Ordinary guest writes
        // always require WRITE rights; the consumer has READ rights only.
        let operation = self
            .hub
            .submit_blob_write_checked(self.store, self.blob, count, BLOB_RIGHT_READ, || {
                bytes[..count].to_vec()
            })
            .map_err(transfer_error)?;
        let pending = PendingArchiveWrite {
            hub: &self.hub,
            store: self.store,
            operation,
        };
        self.runtime
            .block_on_wasm(async {
                loop {
                    if let Some(result) = self.hub.observe_terminal(self.store, operation)? {
                        return if result.terminal == Terminal::Completed {
                            Ok(())
                        } else {
                            Err(HubError::Closed)
                        };
                    }
                    // suspend handles completion between observation and waiter
                    // registration. No hub mutex is held while waiting.
                    self.hub
                        .wait_external_operation(self.store, operation)?
                        .notified()
                        .await;
                }
            })
            .map_err(transfer_error)?;
        drop(pending);
        self.failed = false;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.failed {
            Err(io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        }
    }
}

impl Drop for NativeArchiveSink {
    fn drop(&mut self) {
        if !self.finished {
            // A failed/trapped producer is closure, never successful EOF.
            let _ = self.hub.abandon_blob_wire(self.store, self.blob.wire());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{authenticated_staging::reader_tests::fixture, ExtractionLimits};
    use crate::async_engine::{timeout, RuntimeBuilder};
    use std::time::Duration;

    const CHUNK: usize = 64 * 1024;
    struct Cleanup(Arc<OperationHub>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.close_all(Terminal::Cancelled);
        }
    }

    struct ResumeOnDrop(Arc<Notify>);
    impl Drop for ResumeOnDrop {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    async fn wait_full(hub: &OperationHub) {
        timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = hub.snapshot();
                if snapshot.buffered_blob_bytes == 2 * CHUNK && snapshot.pending_blob_writes == 1 {
                    return;
                }
                crate::async_engine::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    async fn read(hub: &OperationHub, blob: OpaqueToken) -> Result<usize, HubError> {
        let operation = hub.submit_blob_read(1, blob, CHUNK)?;
        loop {
            let result = hub.collect_blob_read_wire(1, operation.wire(), CHUNK, |bytes| {
                assert!(bytes.iter().all(|byte| *byte == 0x5a));
            })?;
            match result as u8 {
                STATUS_PENDING => hub.wait_external_operation(1, operation)?.notified().await,
                STATUS_COMPLETED => return Ok((result >> 8) as usize),
                _ => return Err(HubError::Closed),
            }
        }
    }

    #[test]
    fn authenticated_blob_stops_at_capacity_and_streams_every_large_entry_byte() {
        const LENGTH: u64 = 17 * 1024 * 1024;
        let (authenticated, storage) = fixture("payload", LENGTH);
        let reader = authenticated
            .into_reader(ExtractionLimits::default())
            .unwrap();
        let limits = BlobLimits::new(CHUNK, 2 * CHUNK, 4 * CHUNK).unwrap();
        let hub = OperationHub::with_blob_limits(8, 2, limits).unwrap();
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _cleanup = Cleanup(Arc::clone(&hub));
        let sink = NativeArchiveSink::new(Arc::clone(&hub), runtime.handle(), 1).unwrap();
        let blob = sink.blob();
        assert_eq!(
            hub.submit_blob_write(1, blob, b"forged"),
            Err(HubError::WrongRights)
        );
        assert_eq!(hub.seal_blob(1, blob), Err(HubError::WrongRights));
        let job = runtime
            .handle()
            .launch_blocking(move || sink.copy_archive(reader, 0));
        runtime
            .run_within(
                async {
                    wait_full(&hub).await;
                    let paused = hub.snapshot();
                    assert_eq!(paused.pending_write_bytes, CHUNK);
                    assert!(!job.is_finished());
                    assert!(storage.used() > 16 * 1024 * 1024);
                    crate::async_engine::sleep(Duration::from_millis(20)).await;
                    assert_eq!(
                        hub.snapshot().buffered_blob_bytes,
                        paused.buffered_blob_bytes
                    );
                    assert_eq!(
                        hub.snapshot().pending_write_bytes,
                        paused.pending_write_bytes
                    );
                    let total = timeout(Duration::from_secs(10), async {
                        let mut total = 0_u64;
                        loop {
                            let count = read(&hub, blob).await.unwrap();
                            if count == 0 {
                                break;
                            }
                            total += count as u64;
                            assert!(total <= LENGTH);
                        }
                        total
                    })
                    .await
                    .unwrap();
                    assert_eq!(total, LENGTH);
                    assert_eq!(job.await.unwrap().unwrap(), LENGTH);
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(storage.used(), 0);
        hub.abandon_blob_wire(1, blob.wire()).unwrap();
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.live_resources, 0);
        assert_eq!(snapshot.pending_operations, 0);
        assert_eq!(snapshot.retained_transfer_capacity, 0);
        assert!(snapshot.peak_retained_transfer_capacity <= limits.maximum_transfer_bytes);
        assert!(snapshot.peak_buffered_blob_bytes <= 2 * CHUNK);
    }

    #[test]
    fn authenticated_blob_consumer_drop_and_teardown_unblock_producer() {
        for terminal in [None, Some(Terminal::Trapped), Some(Terminal::TimedOut)] {
            let (authenticated, storage) = fixture("payload", (4 * CHUNK) as u64);
            let reader = authenticated
                .into_reader(ExtractionLimits::default())
                .unwrap();
            let hub = OperationHub::with_blob_limits(
                8,
                2,
                BlobLimits::new(CHUNK, 2 * CHUNK, 4 * CHUNK).unwrap(),
            )
            .unwrap();
            let runtime = RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _cleanup = Cleanup(Arc::clone(&hub));
            let sink = NativeArchiveSink::new(Arc::clone(&hub), runtime.handle(), 1).unwrap();
            let blob = sink.blob();
            let job = runtime
                .handle()
                .launch_blocking(move || sink.copy_archive(reader, 0));
            runtime
                .run_within(
                    async {
                        wait_full(&hub).await;
                        assert!(storage.used() > 0);
                        if let Some(terminal) = terminal {
                            hub.close_all(terminal);
                        } else {
                            hub.abandon_blob_wire(1, blob.wire()).unwrap();
                        }
                        assert!(timeout(Duration::from_secs(5), job)
                            .await
                            .unwrap()
                            .unwrap()
                            .is_err());
                    },
                    std::time::Duration::from_secs(60),
                )
                .expect("test runtime budget");
            assert_eq!(storage.used(), 0);
            let snapshot = hub.snapshot();
            assert_eq!(snapshot.live_resources, 0);
            assert_eq!(snapshot.pending_operations, 0);
            assert_eq!(snapshot.retained_transfer_capacity, 0);
        }
    }

    #[test]
    fn authenticated_blob_failed_or_panicked_producer_never_publishes_eof() {
        use std::io::Write as _;
        for panic_after_prefix in [false, true] {
            let (authenticated, storage) = fixture("payload", (2 * CHUNK) as u64);
            let reader = authenticated
                .into_reader(ExtractionLimits::default())
                .unwrap();
            let hub = OperationHub::with_blob_limits(
                1,
                2,
                BlobLimits::new(CHUNK, 2 * CHUNK, 4 * CHUNK).unwrap(),
            )
            .unwrap();
            let runtime = RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _cleanup = Cleanup(Arc::clone(&hub));
            let mut sink = NativeArchiveSink::new(Arc::clone(&hub), runtime.handle(), 1).unwrap();
            let blob = sink.blob();
            let prefix = Arc::new(Notify::new());
            let resume = Arc::new(Notify::new());
            let _resume_on_failure = ResumeOnDrop(Arc::clone(&resume));
            let worker_prefix = Arc::clone(&prefix);
            let worker_resume = Arc::clone(&resume);
            let handle = runtime.handle();
            let job = handle.clone().launch_blocking(move || {
                let _reader = reader;
                sink.write_all(&[0x5a; CHUNK]).unwrap();
                sink.flush().unwrap(); // flush must not seal the stream.
                worker_prefix.notify_one();
                handle.block_on_wasm(worker_resume.notified());
                if panic_after_prefix {
                    panic!("synthetic archive producer unwind");
                }
                // The consumer's pending read occupies the only operation
                // slot. Submission fails without granting reusable EOF.
                assert!(sink.write_all(&[0x5a; CHUNK]).is_err());
                assert!(sink.finish().is_err());
            });
            runtime
                .run_within(
                    async {
                        timeout(Duration::from_secs(5), prefix.notified())
                            .await
                            .unwrap();
                        assert_eq!(read(&hub, blob).await.unwrap(), CHUNK);
                        let waiting = hub.submit_blob_read(1, blob, CHUNK).unwrap();
                        assert_eq!(
                            hub.collect_blob_read_wire(1, waiting.wire(), CHUNK, |_| panic!(
                                "premature EOF"
                            ))
                            .unwrap(),
                            0
                        );
                        assert!(storage.used() > 0);
                        resume.notify_one();
                        let result = timeout(Duration::from_secs(5), job).await.unwrap();
                        assert_eq!(result.is_err(), panic_after_prefix);
                        let closed = hub
                            .collect_blob_read_wire(1, waiting.wire(), CHUNK, |_| {
                                panic!("failed producer published EOF")
                            })
                            .unwrap();
                        assert_eq!(closed as u8, STATUS_CLOSED);
                    },
                    std::time::Duration::from_secs(60),
                )
                .expect("test runtime budget");
            assert_eq!(storage.used(), 0);
            assert_eq!(hub.snapshot().live_resources, 0);
            assert_eq!(hub.snapshot().pending_operations, 0);
            assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        }
    }
}
