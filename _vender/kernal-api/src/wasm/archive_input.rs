//! Host-owned encrypted input grant for the test-only guest archive experiment.
use super::*;
use std::io::{self, Read, Seek};

pub(crate) const MAX_HEADER_BYTES: usize = 16 * 1024 + 12;
const INPUT_KIND: u8 = 7;
const READ_HEADER: u8 = 1;

#[derive(Default)]
pub(super) struct ArchiveJobs {
    tasks: Vec<crate::async_engine::Task<()>>,
    active: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
}

impl ArchiveJobs {
    pub(super) fn active(&self) -> usize {
        self.active.load(Ordering::Acquire) as usize
    }
}

struct ArchiveJobLease {
    hub: Arc<OperationHub>,
    operation: OpaqueToken,
    active: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
}

impl Drop for ArchiveJobLease {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.failed.store(true, Ordering::Release);
            let _ = self.hub.terminal(
                self.operation,
                TerminalResult {
                    terminal: Terminal::Rejected,
                    resource: None,
                },
            );
        }
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct EncryptedInput {
    file: File,
    key: [u8; 16],
    header: Vec<u8>,
    ciphertext_bytes: u64,
}

impl EncryptedInput {
    fn authenticate_into(
        mut self,
        hub: &OperationHub,
        store: u64,
        operation: OpaqueToken,
        nonce: [u8; 12],
        after_started: impl FnOnce(),
    ) -> Result<(), HubError> {
        hub.start_archive_authentication(
            store,
            operation,
            &self.key,
            &nonce,
            &self.header,
            self.ciphertext_bytes,
        )?;
        after_started();
        let active = || {
            let state = hub.state.lock().map_err(|_| HubError::Closed)?;
            if state.closed
                || state
                    .operations
                    .get(&operation)
                    .is_none_or(|slot| slot.terminal.is_some())
            {
                Err(HubError::Closed)
            } else {
                Ok(())
            }
        };
        let mut chunk = [0; 64 * 1024];
        let mut remaining = self.ciphertext_bytes;
        while remaining != 0 {
            active()?;
            let count = remaining.min(chunk.len() as u64) as usize;
            self.file
                .read_exact(&mut chunk[..count])
                .map_err(|_| HubError::Invalid)?;
            hub.update_archive_authentication(store, operation, &chunk[..count])?;
            remaining -= count as u64;
        }
        active()?;
        let mut tag = [0; 16];
        self.file
            .read_exact(&mut tag)
            .map_err(|_| HubError::Invalid)?;
        if self.file.read(&mut [0; 1]).map_err(|_| HubError::Invalid)? != 0 {
            return Err(HubError::Invalid);
        }
        hub.finish_archive_authentication(store, operation, &tag)
    }

    /// Read only the bounded public envelope prefix before the guest starts.
    /// Retain the opened source and key on the host, never in guest memory.
    pub(crate) fn open(mut file: File, key: [u8; 16], maximum_bytes: u64) -> io::Result<Self> {
        let length = file.metadata()?.len();
        if length > maximum_bytes {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        file.rewind()?;
        let mut prefix = [0; 12];
        file.read_exact(&mut prefix)?;
        if &prefix[..8] != b"TWPV1AES" {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let header_length = u32::from_be_bytes(
            prefix[8..]
                .try_into()
                .map_err(|_| io::ErrorKind::InvalidData)?,
        ) as usize;
        if header_length > MAX_HEADER_BYTES - 12 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let header_end = (12 + header_length) as u64;
        let ciphertext_bytes = length
            .checked_sub(header_end)
            .and_then(|bytes| bytes.checked_sub(16))
            .ok_or(io::ErrorKind::InvalidData)?;
        let mut header = Vec::with_capacity(12 + header_length);
        header.extend_from_slice(&prefix);
        header.resize(12 + header_length, 0);
        file.read_exact(&mut header[12..])?;
        Ok(Self {
            file,
            key,
            header,
            ciphertext_bytes,
        })
    }
}

impl OperationHub {
    pub(crate) fn submit_encrypted_authentication(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        store: u64,
        token: OpaqueToken,
        nonce: [u8; 12],
    ) -> Result<OpaqueToken, HubError> {
        self.submit_encrypted_authentication_with(runtime, store, token, nonce, || {})
    }

    fn submit_encrypted_authentication_with(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        store: u64,
        token: OpaqueToken,
        nonce: [u8; 12],
        after_started: impl FnOnce() + Send + 'static,
    ) -> Result<OpaqueToken, HubError> {
        // The operation does not borrow the input: successful admission moves
        // the entire authority into one worker and invalidates the old token.
        let (operation, _) = self.submit(store, None, 0, 0)?;
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let mut jobs = self.archive_jobs.lock().map_err(|_| HubError::Closed)?;
        let admission = (|| {
            if state.closed {
                return Err(HubError::Closed);
            }
            if jobs.active.load(Ordering::Acquire) >= self.maximum_operations as u64 {
                return Err(HubError::Quota);
            }
            let slot = state.resources.get_mut(&token).ok_or(HubError::Invalid)?;
            Self::validate_resource(slot, store, INPUT_KIND, READ_HEADER)?;
            if !matches!(slot.value, ResourceValue::EncryptedInput(_)) {
                return Err(HubError::WrongKind);
            }
            let ResourceValue::EncryptedInput(input) =
                std::mem::replace(&mut slot.value, ResourceValue::Synthetic)
            else {
                return Err(HubError::WrongKind);
            };
            let notifications =
                Self::close_resource_with_terminal_locked(&mut state, token, Terminal::Closed)?;
            Ok((input, notifications))
        })();
        let (input, notifications) = match admission {
            Ok(value) => value,
            Err(error) => {
                state.operations.remove(&operation);
                return Err(error);
            }
        };
        state
            .operations
            .get_mut(&operation)
            .ok_or(HubError::Closed)?
            .is_archive_operation = true;
        jobs.tasks.retain(|task| !task.is_finished());
        jobs.active.fetch_add(1, Ordering::AcqRel);
        let lease = ArchiveJobLease {
            hub: Arc::clone(self),
            operation,
            active: Arc::clone(&jobs.active),
            failed: Arc::clone(&jobs.failed),
        };
        let hub = Arc::clone(self);
        jobs.tasks.push(runtime.launch_blocking(move || {
            let _lease = lease;
            if input
                .authenticate_into(&hub, store, operation, nonce, after_started)
                .is_err()
            {
                let _ = hub.terminal(
                    operation,
                    TerminalResult {
                        terminal: Terminal::Rejected,
                        resource: None,
                    },
                );
            }
        }));
        drop(jobs);
        drop(state);
        for notify in notifications {
            notify.notify_one();
        }
        Ok(operation)
    }

    pub(crate) async fn join_archive_jobs(&self) -> Result<(), HubError> {
        if !self.state.lock().map_err(|_| HubError::Closed)?.closed {
            return Err(HubError::WrongRights);
        }
        let (tasks, failed) = {
            let mut jobs = self.archive_jobs.lock().map_err(|_| HubError::Closed)?;
            (std::mem::take(&mut jobs.tasks), Arc::clone(&jobs.failed))
        };
        let mut join_failed = false;
        for task in tasks {
            join_failed |= task.await.is_err();
        }
        if join_failed || failed.load(Ordering::Acquire) {
            Err(HubError::Closed)
        } else {
            Ok(())
        }
    }

    pub(super) fn submit_archive_work(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        store: u64,
        authority: (OpaqueToken, u8, u8),
        work: impl FnOnce(&Arc<Self>, OpaqueToken) -> Result<(), HubError> + Send + 'static,
    ) -> Result<OpaqueToken, HubError> {
        let (operation, _) = self.submit(store, Some(authority.0), authority.1, authority.2)?;
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let mut jobs = self.archive_jobs.lock().map_err(|_| HubError::Closed)?;
        if state.closed || jobs.active.load(Ordering::Acquire) >= self.maximum_operations as u64 {
            state.operations.remove(&operation);
            return Err(if state.closed {
                HubError::Closed
            } else {
                HubError::Quota
            });
        }
        state
            .operations
            .get_mut(&operation)
            .ok_or(HubError::Closed)?
            .is_archive_operation = true;
        jobs.tasks.retain(|task| !task.is_finished());
        jobs.active.fetch_add(1, Ordering::AcqRel);
        let lease = ArchiveJobLease {
            hub: Arc::clone(self),
            operation,
            active: Arc::clone(&jobs.active),
            failed: Arc::clone(&jobs.failed),
        };
        let hub = Arc::clone(self);
        jobs.tasks.push(runtime.launch_blocking(move || {
            let _lease = lease;
            if work(&hub, operation).is_err() {
                let _ = hub.terminal(
                    operation,
                    TerminalResult {
                        terminal: Terminal::Rejected,
                        resource: None,
                    },
                );
            }
        }));
        Ok(operation)
    }

    pub(crate) fn grant_encrypted_input(
        &self,
        store: u64,
        input: EncryptedInput,
    ) -> Result<OpaqueToken, HubError> {
        let token = self.create_resource_value(
            store,
            INPUT_KIND,
            READ_HEADER,
            false,
            ResourceValue::EncryptedInput(input),
        )?;
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        state
            .resources
            .get_mut(&token)
            .ok_or(HubError::Closed)?
            .reserved = false;
        Ok(token)
    }

    pub(crate) fn read_encrypted_header(
        &self,
        store: u64,
        token: u64,
        capacity: usize,
        copy: impl FnOnce(&[u8]),
    ) -> Result<usize, HubError> {
        let state = self.state.lock().map_err(|_| HubError::Closed)?;
        let resource = state
            .resources
            .get(&OpaqueToken(token))
            .ok_or(HubError::Invalid)?;
        Self::validate_resource(resource, store, INPUT_KIND, READ_HEADER)?;
        let ResourceValue::EncryptedInput(input) = &resource.value else {
            return Err(HubError::WrongKind);
        };
        if capacity < input.header.len() {
            return Err(HubError::Quota);
        }
        copy(&input.header);
        Ok(input.header.len())
    }

    pub(crate) fn abandon_encrypted_input(&self, store: u64, token: u64) -> Result<(), HubError> {
        let notifications = {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            let token = OpaqueToken(token);
            let resource = state.resources.get(&token).ok_or(HubError::Invalid)?;
            Self::validate_resource(resource, store, INPUT_KIND, READ_HEADER)?;
            Self::close_resource_with_terminal_locked(&mut state, token, Terminal::Closed)?
        };
        for notify in notifications {
            notify.notify_one();
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn encrypted_zip() -> (EncryptedInput, u64) {
        encrypted_zip_with_header(b"{}", [7; 16], [8; 12], false)
    }

    pub(crate) fn encrypted_zip_with_header(
        header: &[u8],
        key: [u8; 16],
        nonce: [u8; 12],
        corrupt_tag: bool,
    ) -> (EncryptedInput, u64) {
        encrypted_zip_entry(
            header,
            key,
            nonce,
            corrupt_tag,
            "payload",
            17 * 1024 * 1024,
            0x5a,
        )
    }

    pub(crate) fn encrypted_zip_entry(
        header: &[u8],
        key: [u8; 16],
        nonce: [u8; 12],
        corrupt_tag: bool,
        name: &str,
        bytes: u64,
        value: u8,
    ) -> (EncryptedInput, u64) {
        encrypted_zip_entries(
            header,
            key,
            nonce,
            corrupt_tag,
            [(name.to_owned(), bytes, value)],
        )
    }

    pub(crate) fn encrypted_zip_entries(
        header: &[u8],
        key: [u8; 16],
        nonce: [u8; 12],
        corrupt_tag: bool,
        entries: impl IntoIterator<Item = (String, u64, u8)>,
    ) -> (EncryptedInput, u64) {
        use openssl::symm::{Cipher, Crypter, Mode};
        let mut zip = zip::ZipWriter::new(tempfile::tempfile().unwrap());
        let mut chunk = [0; 64 * 1024];
        for (name, bytes, value) in entries {
            zip.start_file(
                name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
            chunk.fill(value);
            let mut remaining = bytes;
            while remaining != 0 {
                let count = remaining.min(chunk.len() as u64) as usize;
                zip.write_all(&chunk[..count]).unwrap();
                remaining -= count as u64;
            }
        }
        let mut zip = zip.finish().unwrap();
        let length = zip.metadata().unwrap().len();
        zip.rewind().unwrap();
        let mut source = source(header, 0);
        source.rewind().unwrap();
        let mut aad = vec![0; 12 + header.len()];
        source.read_exact(&mut aad).unwrap();
        let mut encoder =
            Crypter::new(Cipher::aes_128_gcm(), Mode::Encrypt, &key, Some(&nonce)).unwrap();
        encoder.aad_update(&aad).unwrap();
        let mut encrypted = [0; 64 * 1024 + 16];
        loop {
            let count = zip.read(&mut chunk).unwrap();
            if count == 0 {
                break;
            }
            let count = encoder.update(&chunk[..count], &mut encrypted).unwrap();
            source.write_all(&encrypted[..count]).unwrap();
        }
        assert_eq!(encoder.finalize(&mut encrypted).unwrap(), 0);
        let mut tag = [0; 16];
        encoder.get_tag(&mut tag).unwrap();
        if corrupt_tag {
            tag[0] ^= 1;
        }
        source.write_all(&tag).unwrap();
        (
            EncryptedInput::open(source, key, 512 * 1024 * 1024).unwrap(),
            length,
        )
    }

    async fn terminal(hub: &OperationHub, operation: OpaqueToken) -> TerminalResult {
        loop {
            if let Some(result) = hub.observe_terminal(1, operation).unwrap() {
                return result;
            }
            hub.suspend(operation, 1).unwrap().notified().await;
        }
    }

    #[test]
    fn authenticated_input_job_authenticates_large_zip_before_resource_publication() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = OperationHub::new(2, 2).unwrap();
        let (input, length) = encrypted_zip();
        let token = hub.grant_encrypted_input(1, input).unwrap();
        assert!(hub
            .submit_encrypted_authentication(runtime.handle(), 2, token, [8; 12])
            .is_err());
        let operation = hub
            .submit_encrypted_authentication(runtime.handle(), 1, token, [8; 12])
            .unwrap();
        assert!(hub
            .read_encrypted_header(1, token.wire(), MAX_HEADER_BYTES, |_| panic!("consumed"))
            .is_err());
        let result = runtime
            .run_within(
                terminal(&hub, operation),
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(result.terminal, Terminal::Completed);
        assert_eq!(hub.staging_budget.used(), length);
        let archive = {
            let mut state = hub.state.lock().unwrap();
            let slot = state.resources.get_mut(&result.resource.unwrap()).unwrap();
            let ResourceValue::AuthenticatedArchive(archive) =
                std::mem::replace(&mut slot.value, ResourceValue::Synthetic)
            else {
                panic!("wrong resource")
            };
            archive
        };
        let mut reader = archive
            .into_reader(crate::archive::ExtractionLimits::default())
            .unwrap();
        assert_eq!(reader.entry(0).unwrap().unwrap().name, "payload");
        struct Sink(u64);
        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                assert!(bytes.len() <= 64 * 1024);
                assert!(bytes.iter().all(|byte| *byte == 0x5a));
                self.0 += bytes.len() as u64;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut sink = Sink(0);
        reader.copy_entry(0, &mut sink).unwrap();
        assert_eq!(sink.0, 17 * 1024 * 1024);
        drop(reader);
        hub.close_all(Terminal::Closed);
        runtime
            .run_within(hub.join_archive_jobs(), std::time::Duration::from_secs(60))
            .expect("test runtime budget")
            .unwrap();
        assert_eq!(hub.staging_budget.used(), 0);
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.snapshot().pending_operations, 0);
    }

    #[test]
    fn authenticated_input_cancelled_job_keeps_worker_quota_until_exit() {
        for abandon in [false, true] {
            let runtime = crate::async_engine::RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .unwrap();
            let hub = OperationHub::new(1, 2).unwrap();
            let input = EncryptedInput::open(source(b"{}", 32), [0; 16], 1024).unwrap();
            let token = hub.grant_encrypted_input(1, input).unwrap();
            let (started, ready) = std::sync::mpsc::channel();
            let (release, resume) = std::sync::mpsc::channel();
            struct Resume(Option<std::sync::mpsc::Sender<()>>);
            impl Drop for Resume {
                fn drop(&mut self) {
                    if let Some(sender) = self.0.take() {
                        let _ = sender.send(());
                    }
                }
            }
            let resume_on_drop = Resume(Some(release));
            let operation = hub
                .submit_encrypted_authentication_with(
                    runtime.handle(),
                    1,
                    token,
                    [0; 12],
                    move || {
                        started.send(()).unwrap();
                        resume.recv().unwrap();
                    },
                )
                .unwrap();
            ready
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert_eq!(hub.staging_budget.used(), 16);
            assert_eq!(hub.snapshot().live_resources, 0);
            if abandon {
                assert!(hub
                    .abandon_archive_authentication(2, operation.wire())
                    .is_err());
                hub.abandon_archive_authentication(1, operation.wire())
                    .unwrap();
                assert!(hub.observe_terminal(1, operation).is_err());
            } else {
                hub.cancel_wire(1, operation.wire()).unwrap();
                assert_eq!(
                    runtime
                        .run_within(
                            terminal(&hub, operation),
                            std::time::Duration::from_secs(60)
                        )
                        .expect("test runtime budget")
                        .terminal,
                    Terminal::Cancelled
                );
            }
            let another = EncryptedInput::open(source(b"{}", 32), [0; 16], 1024).unwrap();
            let another = hub.grant_encrypted_input(1, another).unwrap();
            assert_eq!(
                hub.submit_encrypted_authentication(runtime.handle(), 1, another, [0; 12]),
                Err(HubError::Quota)
            );
            assert_eq!(
                hub.archive_jobs
                    .lock()
                    .unwrap()
                    .active
                    .load(Ordering::Acquire),
                1
            );
            hub.close_all(Terminal::Closed);
            drop(resume_on_drop);
            runtime
                .run_within(hub.join_archive_jobs(), std::time::Duration::from_secs(60))
                .expect("test runtime budget")
                .unwrap();
            assert_eq!(
                hub.archive_jobs
                    .lock()
                    .unwrap()
                    .active
                    .load(Ordering::Acquire),
                0
            );
            assert_eq!(hub.staging_budget.used(), 0);
            assert_eq!(hub.snapshot().live_resources, 0);
            assert_eq!(hub.snapshot().pending_operations, 0);
        }
    }

    pub(crate) fn source(header: &[u8], tail_bytes: usize) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"TWPV1AES").unwrap();
        file.write_all(&(header.len() as u32).to_be_bytes())
            .unwrap();
        file.write_all(header).unwrap();
        file.set_len((12 + header.len() + tail_bytes) as u64)
            .unwrap();
        file
    }

    #[test]
    fn authenticated_input_abandonment_reclaims_completed_uncollected_archive() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = OperationHub::new(2, 2).unwrap();
        let (input, length) = encrypted_zip();
        let token = hub.grant_encrypted_input(1, input).unwrap();
        let operation = hub
            .submit_encrypted_authentication(runtime.handle(), 1, token, [8; 12])
            .unwrap();
        runtime
            .run_within(
                async {
                    crate::async_engine::timeout(std::time::Duration::from_secs(5), async {
                        loop {
                            if hub
                                .state
                                .lock()
                                .unwrap()
                                .operations
                                .get(&operation)
                                .unwrap()
                                .terminal
                                .is_some()
                            {
                                break;
                            }
                            hub.suspend(operation, 1).unwrap().notified().await;
                        }
                    })
                    .await
                    .unwrap();
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(hub.snapshot().live_resources, 1);
        assert_eq!(hub.staging_budget.used(), length);
        assert!(hub
            .abandon_archive_authentication(2, operation.wire())
            .is_err());
        hub.abandon_archive_authentication(1, operation.wire())
            .unwrap();
        assert!(hub
            .abandon_archive_authentication(1, operation.wire())
            .is_err());
        assert_eq!(hub.staging_budget.used(), 0);
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.snapshot().pending_operations, 0);
        let (ordinary, _) = hub.submit(1, None, 0, 0).unwrap();
        assert_eq!(
            hub.abandon_archive_authentication(1, ordinary.wire()),
            Err(HubError::WrongKind)
        );
        hub.close_all(Terminal::Closed);
        runtime
            .run_within(hub.join_archive_jobs(), std::time::Duration::from_secs(60))
            .expect("test runtime budget")
            .unwrap();
    }

    #[test]
    fn authenticated_input_header_is_bounded_scoped_and_revocable() {
        let header = b"{ \"original\": true }";
        let input = EncryptedInput::open(
            source(header, 17 * 1024 * 1024 + 16),
            [9; 16],
            512 * 1024 * 1024,
        )
        .unwrap();
        assert_eq!(input.ciphertext_bytes, 17 * 1024 * 1024);
        assert_eq!(
            input.file.metadata().unwrap().len(),
            (12 + header.len() + 17 * 1024 * 1024 + 16) as u64
        );
        let hub = OperationHub::new(1, 2).unwrap();
        let token = hub.grant_encrypted_input(1, input).unwrap().wire();
        assert!(hub
            .read_encrypted_header(2, token, MAX_HEADER_BYTES, |_| panic!("foreign copy"))
            .is_err());
        assert!(hub
            .read_encrypted_header(1, token, 12, |_| panic!("short copy"))
            .is_err());
        assert_eq!(
            hub.read_encrypted_header(1, token, MAX_HEADER_BYTES, |bytes| assert_eq!(
                &bytes[12..],
                header
            ))
            .unwrap(),
            12 + header.len()
        );
        assert!(hub.abandon_encrypted_input(2, token).is_err());
        hub.abandon_encrypted_input(1, token).unwrap();
        assert!(hub
            .read_encrypted_header(1, token, MAX_HEADER_BYTES, |_| panic!("stale copy"))
            .is_err());
        assert_eq!(hub.snapshot().live_resources, 0);
    }

    #[test]
    fn authenticated_input_rejects_oversized_header_input_and_truncated_tag() {
        assert!(EncryptedInput::open(source(b"{}", 15), [0; 16], 1024).is_err());
        assert!(EncryptedInput::open(source(b"{}", 16), [0; 16], 1).is_err());
        assert!(
            EncryptedInput::open(source(&vec![b' '; 16 * 1024 + 1], 16), [0; 16], 1024 * 1024)
                .is_err()
        );
        let mut wrong = source(b"{}", 16);
        wrong.rewind().unwrap();
        wrong.write_all(b"BADMAGIC").unwrap();
        assert!(EncryptedInput::open(wrong, [0; 16], 1024).is_err());
    }

    #[test]
    fn authenticated_input_job_rejects_bad_tag_and_drains_storage() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = OperationHub::new(2, 2).unwrap();
        let input = EncryptedInput::open(source(b"{}", 32), [0; 16], 1024).unwrap();
        let token = hub.grant_encrypted_input(1, input).unwrap();
        let operation = hub
            .submit_encrypted_authentication(runtime.handle(), 1, token, [0; 12])
            .unwrap();
        runtime
            .run_within(
                async {
                    loop {
                        if let Some(result) = hub.observe_terminal(1, operation).unwrap() {
                            assert_eq!(result.terminal, Terminal::Rejected);
                            assert_eq!(result.resource, None);
                            break;
                        }
                        hub.suspend(operation, 1).unwrap().notified().await;
                    }
                    hub.close_all(Terminal::Closed);
                    hub.join_archive_jobs().await.unwrap();
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(hub.staging_budget.used(), 0);
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.snapshot().pending_operations, 0);
    }

    #[test]
    fn authenticated_input_job_panic_is_reported_after_finished_handle_pruning() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = OperationHub::new(1, 2).unwrap();
        let input = EncryptedInput::open(source(b"{}", 32), [0; 16], 1024).unwrap();
        let token = hub.grant_encrypted_input(1, input).unwrap();
        let operation = hub
            .submit_encrypted_authentication_with(runtime.handle(), 1, token, [0; 12], || {
                panic!("injected worker unwind")
            })
            .unwrap();
        runtime
            .run_within(
                async {
                    assert_eq!(terminal(&hub, operation).await.terminal, Terminal::Rejected);
                    crate::async_engine::timeout(std::time::Duration::from_secs(5), async {
                        loop {
                            if hub
                                .archive_jobs
                                .lock()
                                .unwrap()
                                .tasks
                                .iter()
                                .all(|task| task.is_finished())
                            {
                                break;
                            }
                            crate::async_engine::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        let input = EncryptedInput::open(source(b"{}", 32), [0; 16], 1024).unwrap();
        let token = hub.grant_encrypted_input(1, input).unwrap();
        let operation = hub
            .submit_encrypted_authentication(runtime.handle(), 1, token, [0; 12])
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
        hub.close_all(Terminal::Closed);
        assert_eq!(
            runtime
                .run_within(hub.join_archive_jobs(), std::time::Duration::from_secs(60))
                .expect("test runtime budget"),
            Err(HubError::Closed)
        );
        assert_eq!(hub.staging_budget.used(), 0);
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.snapshot().pending_operations, 0);
    }
}
