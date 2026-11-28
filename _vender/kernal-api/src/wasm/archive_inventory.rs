//! Bounded authenticated ZIP inventory on the existing native blocking lane.
use super::*;

pub(super) const ENTRY_KIND: u8 = 8;
pub(super) const ENTRY_RIGHT: u8 = 1;
const ARCHIVE_KIND: u8 = 6;
const ARCHIVE_RIGHT: u8 = 1;
pub(super) const MAX_RECORD: usize = 12 + 4096;

pub(super) struct OpenArchive {
    pub(super) reader: crate::archive::authenticated_staging::AuthenticatedReader,
    next: usize,
}

pub(super) type SharedArchive = Arc<Mutex<OpenArchive>>;

pub(super) struct ArchiveEntry {
    pub(super) archive: SharedArchive,
    pub(super) index: usize,
    name: String,
    bytes: u64,
}

struct InventoryLease {
    hub: Arc<OperationHub>,
    store: u64,
    archive: OpaqueToken,
    finished: bool,
}

impl Drop for InventoryLease {
    fn drop(&mut self) {
        // A failed/panicked open must not leave a consumed file hidden behind
        // a permanently busy archive token. No hub lock is held here.
        if !self.finished {
            let _ = self
                .hub
                .abandon_authenticated_archive(self.store, self.archive.wire());
        }
    }
}

impl OperationHub {
    pub(crate) fn submit_archive_next_entry(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        store: u64,
        archive: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        self.submit_archive_work(
            runtime,
            store,
            (archive, ARCHIVE_KIND, ARCHIVE_RIGHT),
            move |hub, operation| hub.read_next_archive_entry(store, archive, operation),
        )
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Existing function complexity measured at ratchet adoption"
    )]
    fn read_next_archive_entry(
        self: &Arc<Self>,
        store: u64,
        archive: OpaqueToken,
        operation: OpaqueToken,
    ) -> Result<(), HubError> {
        enum Source {
            Sealed(crate::archive::authenticated_staging::Authenticated),
            Open(SharedArchive),
        }
        let source = {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            if state
                .operations
                .get(&operation)
                .is_none_or(|slot| slot.terminal.is_some())
            {
                return Err(HubError::Closed);
            }
            let slot = state.resources.get_mut(&archive).ok_or(HubError::Closed)?;
            Self::validate_resource(slot, store, ARCHIVE_KIND, ARCHIVE_RIGHT)?;
            if slot.committing {
                return Err(HubError::WrongRights);
            }
            let source = match &slot.value {
                ResourceValue::ArchiveReader(reader) => Source::Open(Arc::clone(reader)),
                ResourceValue::AuthenticatedArchive(_) => {
                    let ResourceValue::AuthenticatedArchive(sealed) =
                        std::mem::replace(&mut slot.value, ResourceValue::Synthetic)
                    else {
                        return Err(HubError::WrongKind);
                    };
                    Source::Sealed(sealed)
                }
                _ => return Err(HubError::WrongKind),
            };
            slot.committing = true;
            source
        };
        let mut lease = InventoryLease {
            hub: Arc::clone(self),
            store,
            archive,
            finished: false,
        };
        let shared = match source {
            Source::Open(reader) => reader,
            Source::Sealed(sealed) => {
                let reader = sealed
                    .into_reader(crate::archive::ExtractionLimits {
                        max_input_bytes: 512 * 1024 * 1024,
                        max_output_bytes: 512 * 1024 * 1024,
                        max_entry_bytes: 32 * 1024 * 1024,
                        max_entries: 16_384,
                        max_path_bytes: 4096,
                        ..crate::archive::ExtractionLimits::default()
                    })
                    .map_err(|_| HubError::Invalid)?;
                Arc::new(Mutex::new(OpenArchive { reader, next: 0 }))
            }
        };
        // ZIP metadata I/O and the per-reader lock stay on the blocking lane.
        // No path, ZIP type or native file enters the ABI record.
        let mut reader = shared.lock().map_err(|_| HubError::Closed)?;
        let index = reader.next;
        let record = reader.reader.entry(index).map_err(|_| HubError::Invalid)?;
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let slot = state.resources.get_mut(&archive).ok_or(HubError::Closed)?;
        slot.value = ResourceValue::ArchiveReader(Arc::clone(&shared));
        slot.committing = false;
        lease.finished = true;
        if state
            .operations
            .get(&operation)
            .is_none_or(|slot| slot.terminal.is_some())
        {
            return Err(HubError::Closed);
        }
        let entry = match record {
            Some(record) => {
                let token = self.create_resource_value_locked(
                    &mut state,
                    store,
                    ENTRY_KIND,
                    ENTRY_RIGHT,
                    false,
                    ResourceValue::ArchiveEntry(ArchiveEntry {
                        archive: Arc::clone(&shared),
                        index,
                        name: record.name,
                        bytes: record.bytes,
                    }),
                )?;
                state
                    .operations
                    .get_mut(&operation)
                    .ok_or(HubError::Closed)?
                    .created_resource = Some(token);
                reader.next += 1;
                Some(token)
            }
            None => None,
        };
        let notify = Self::terminal_locked(
            &mut state,
            operation,
            TerminalResult {
                terminal: Terminal::Completed,
                resource: entry,
            },
        )?;
        drop(state);
        drop(reader);
        if let Some(notify) = notify {
            notify.notify_one();
        }
        Ok(())
    }

    pub(crate) fn read_archive_entry_metadata(
        &self,
        store: u64,
        token: u64,
        capacity: usize,
        copy: impl FnOnce(&[u8]),
    ) -> Result<usize, HubError> {
        let state = self.state.lock().map_err(|_| HubError::Closed)?;
        let slot = state
            .resources
            .get(&OpaqueToken(token))
            .ok_or(HubError::Invalid)?;
        Self::validate_resource(slot, store, ENTRY_KIND, ENTRY_RIGHT)?;
        let ResourceValue::ArchiveEntry(entry) = &slot.value else {
            return Err(HubError::WrongKind);
        };
        let length = 12_usize
            .checked_add(entry.name.len())
            .filter(|length| *length <= MAX_RECORD && *length <= capacity)
            .ok_or(HubError::Quota)?;
        let mut record = [0; MAX_RECORD];
        record[..8].copy_from_slice(&entry.bytes.to_le_bytes());
        record[8..12].copy_from_slice(&(entry.name.len() as u32).to_le_bytes());
        record[12..length].copy_from_slice(entry.name.as_bytes());
        copy(&record[..length]);
        Ok(length)
    }

    pub(crate) fn abandon_archive_entry(&self, store: u64, token: u64) -> Result<(), HubError> {
        let notifications = {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            let token = OpaqueToken(token);
            let slot = state.resources.get(&token).ok_or(HubError::Invalid)?;
            Self::validate_resource(slot, store, ENTRY_KIND, ENTRY_RIGHT)?;
            Self::close_resource_with_terminal_locked(&mut state, token, Terminal::Closed)?
        };
        for notify in notifications {
            notify.notify_one();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Cleanup(Arc<OperationHub>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.close_all(Terminal::Cancelled);
        }
    }

    async fn terminal(hub: &OperationHub, operation: OpaqueToken) -> TerminalResult {
        loop {
            if let Some(result) = hub.observe_terminal(1, operation).unwrap() {
                return result;
            }
            hub.suspend(operation, 1).unwrap().notified().await;
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Existing end-to-end streaming proof is 125 lines"
    )]
    #[test]
    fn authenticated_inventory_entry_open_streams_large_payload_with_bounded_capacity() {
        for read_all in [true, false] {
            let runtime = crate::async_engine::RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .unwrap();
            let chunk = 64 * 1024;
            let limits = BlobLimits::new(chunk, 2 * chunk, 4 * chunk).unwrap();
            let hub = OperationHub::with_blob_limits(8, 4, limits).unwrap();
            let _cleanup = Cleanup(Arc::clone(&hub));
            let (input, _) =
                archive_input::tests::encrypted_zip_with_header(b"{}", [7; 16], [8; 12], false);
            let input = hub.grant_encrypted_input(1, input).unwrap();
            let operation = hub
                .submit_encrypted_authentication(runtime.handle(), 1, input, [8; 12])
                .unwrap();
            let archive = runtime
                .run_within(
                    terminal(&hub, operation),
                    std::time::Duration::from_secs(60),
                )
                .expect("test runtime budget")
                .resource
                .unwrap();
            let operation = hub
                .submit_archive_next_entry(runtime.handle(), 1, archive)
                .unwrap();
            let entry = runtime
                .run_within(
                    terminal(&hub, operation),
                    std::time::Duration::from_secs(60),
                )
                .expect("test runtime budget")
                .resource
                .unwrap();
            assert!(hub
                .submit_archive_entry_open(runtime.handle(), 2, entry)
                .is_err());
            let operation = hub
                .submit_archive_entry_open(runtime.handle(), 1, entry)
                .unwrap();
            let blob = runtime
                .run_within(
                    terminal(&hub, operation),
                    std::time::Duration::from_secs(60),
                )
                .expect("test runtime budget")
                .resource
                .unwrap();
            assert!(hub
                .submit_archive_entry_open(runtime.handle(), 1, entry)
                .is_err());
            assert_eq!(
                hub.submit_blob_write(1, blob, b"forged"),
                Err(HubError::WrongRights)
            );
            hub.abandon_authenticated_archive(1, archive.wire())
                .unwrap();
            if read_all {
                runtime
                    .run_within(
                        crate::async_engine::timeout(std::time::Duration::from_secs(10), async {
                            let mut total = 0;
                            loop {
                                let operation = hub.submit_blob_read(1, blob, chunk).unwrap();
                                let count = loop {
                                    let result = hub
                                        .collect_blob_read_wire(
                                            1,
                                            operation.wire(),
                                            chunk,
                                            |bytes| {
                                                assert!(bytes.iter().all(|byte| *byte == 0x5a));
                                            },
                                        )
                                        .unwrap();
                                    match result as u8 {
                                        STATUS_PENDING => {
                                            hub.wait_external_operation(1, operation)
                                                .unwrap()
                                                .notified()
                                                .await
                                        }
                                        STATUS_COMPLETED => break (result >> 8) as usize,
                                        status => panic!("unexpected stream status {status}"),
                                    }
                                };
                                if count == 0 {
                                    break;
                                }
                                total += count;
                                assert!(total <= 17 * 1024 * 1024);
                            }
                            assert_eq!(total, 17 * 1024 * 1024);
                        }),
                        std::time::Duration::from_secs(60),
                    )
                    .expect("test runtime budget")
                    .unwrap();
            } else {
                runtime
                    .run_within(
                        crate::async_engine::timeout(std::time::Duration::from_secs(5), async {
                            while hub.snapshot().pending_blob_writes == 0 {
                                crate::async_engine::yield_now().await;
                            }
                        }),
                        std::time::Duration::from_secs(60),
                    )
                    .expect("test runtime budget")
                    .unwrap();
                assert!(hub.staging_budget.used() > 16 * 1024 * 1024);
            }
            hub.abandon_blob_wire(1, blob.wire()).unwrap();
            hub.close_all(Terminal::Closed);
            runtime
                .run_within(hub.join_archive_jobs(), std::time::Duration::from_secs(60))
                .expect("test runtime budget")
                .unwrap();
            let snapshot = hub.snapshot();
            assert_eq!(snapshot.live_resources, 0);
            assert_eq!(snapshot.pending_operations, 0);
            assert_eq!(snapshot.retained_transfer_capacity, 0);
            assert!(snapshot.peak_retained_transfer_capacity <= limits.maximum_transfer_bytes);
            assert_eq!(hub.staging_budget.used(), 0);
        }
    }

    #[test]
    fn authenticated_inventory_quota_preserves_cursor_and_abandon_reclaims_uncollected_entry() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = OperationHub::new(4, 2).unwrap();
        let (input, _) =
            archive_input::tests::encrypted_zip_with_header(b"{}", [7; 16], [8; 12], false);
        let input = hub.grant_encrypted_input(1, input).unwrap();
        let operation = hub
            .submit_encrypted_authentication(runtime.handle(), 1, input, [8; 12])
            .unwrap();
        let archive = runtime
            .run_within(
                terminal(&hub, operation),
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget")
            .resource
            .unwrap();
        let filler = hub
            .create_resource(1, SYNTHETIC_RESOURCE_KIND, 1, false)
            .unwrap();
        let operation = hub
            .submit_archive_next_entry(runtime.handle(), 1, archive)
            .unwrap();
        assert_eq!(
            runtime
                .run_within(
                    terminal(&hub, operation),
                    std::time::Duration::from_secs(60)
                )
                .expect("test runtime budget")
                .terminal,
            Terminal::Rejected
        );
        hub.close_resource(filler).unwrap();
        let operation = hub
            .submit_archive_next_entry(runtime.handle(), 1, archive)
            .unwrap();
        // Observe without collecting: Drop must reclaim the unpublished entry.
        runtime
            .run_within(
                async {
                    loop {
                        let finished = hub
                            .state
                            .lock()
                            .unwrap()
                            .operations
                            .get(&operation)
                            .unwrap()
                            .terminal
                            .is_some();
                        if finished {
                            break;
                        }
                        hub.suspend(operation, 1).unwrap().notified().await;
                    }
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        {
            let state = hub.state.lock().unwrap();
            let result = state.operations.get(&operation).unwrap().terminal.unwrap();
            assert_eq!(result.terminal, Terminal::Completed);
            assert!(
                result.resource.is_some(),
                "quota failure must not skip the entry"
            );
        }
        assert_eq!(hub.snapshot().live_resources, 2);
        hub.abandon_archive_authentication(1, operation.wire())
            .unwrap();
        assert_eq!(hub.snapshot().live_resources, 1);
        assert_eq!(hub.snapshot().pending_operations, 0);
        hub.abandon_authenticated_archive(1, archive.wire())
            .unwrap();
        hub.close_all(Terminal::Closed);
        runtime
            .run_within(hub.join_archive_jobs(), std::time::Duration::from_secs(60))
            .expect("test runtime budget")
            .unwrap();
        assert_eq!(hub.staging_budget.used(), 0);
    }

    #[test]
    fn authenticated_inventory_reads_one_bounded_record_and_preserves_entry_ownership() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = OperationHub::new(4, 4).unwrap();
        let (input, length) =
            archive_input::tests::encrypted_zip_with_header(b"{}", [7; 16], [8; 12], false);
        let input = hub.grant_encrypted_input(1, input).unwrap();
        let operation = hub
            .submit_encrypted_authentication(runtime.handle(), 1, input, [8; 12])
            .unwrap();
        let archive = runtime
            .run_within(
                terminal(&hub, operation),
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget")
            .resource
            .unwrap();
        assert!(hub
            .submit_archive_next_entry(runtime.handle(), 2, archive)
            .is_err());
        let operation = hub
            .submit_archive_next_entry(runtime.handle(), 1, archive)
            .unwrap();
        let entry = runtime
            .run_within(
                terminal(&hub, operation),
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget")
            .resource
            .unwrap();
        assert!(hub
            .read_archive_entry_metadata(2, entry.wire(), MAX_RECORD, |_| panic!("foreign copy"))
            .is_err());
        assert!(hub
            .read_archive_entry_metadata(1, entry.wire(), 12, |_| panic!("short copy"))
            .is_err());
        let mut record = Vec::new();
        hub.read_archive_entry_metadata(1, entry.wire(), 4108, |bytes| {
            record.extend_from_slice(bytes)
        })
        .unwrap();
        assert_eq!(
            u64::from_le_bytes(record[..8].try_into().unwrap()),
            17 * 1024 * 1024
        );
        assert_eq!(&record[12..], b"payload");
        let operation = hub
            .submit_archive_next_entry(runtime.handle(), 1, archive)
            .unwrap();
        let eof = runtime
            .run_within(
                terminal(&hub, operation),
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(eof.terminal, Terminal::Completed);
        assert_eq!(eof.resource, None);
        hub.abandon_authenticated_archive(1, archive.wire())
            .unwrap();
        assert_eq!(hub.staging_budget.used(), length);
        assert!(hub.abandon_archive_entry(2, entry.wire()).is_err());
        assert_eq!(
            hub.read_archive_entry_metadata(1, entry.wire(), MAX_RECORD, |_| {})
                .unwrap(),
            19
        );
        hub.abandon_archive_entry(1, entry.wire()).unwrap();
        assert!(hub
            .read_archive_entry_metadata(1, entry.wire(), MAX_RECORD, |_| panic!("stale copy"))
            .is_err());
        hub.close_all(Terminal::Closed);
        runtime
            .run_within(hub.join_archive_jobs(), std::time::Duration::from_secs(60))
            .expect("test runtime budget")
            .unwrap();
        assert_eq!(hub.staging_budget.used(), 0);
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.snapshot().pending_operations, 0);
    }
}
