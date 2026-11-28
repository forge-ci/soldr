//! Native registry experiment only; no generated guest operations yet.
use super::*;
use crate::archive::authenticated_staging::{Authenticated, Authentication};
use std::io;

const ARCHIVE_KIND: u8 = 6;
const EXTRACT_RIGHT: u8 = 1;

impl OperationHub {
    pub(crate) fn abandon_archive_authentication(
        &self,
        store: u64,
        token: u64,
    ) -> Result<(), HubError> {
        let operation = OpaqueToken(token);
        let (slot, notifications) = {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            let slot = state.operations.get(&operation).ok_or(HubError::Invalid)?;
            if slot.owner.store != store {
                return Err(HubError::WrongRights);
            }
            if !slot.is_archive_operation {
                return Err(HubError::WrongKind);
            }
            let resource = slot.created_resource;
            let notifications = if let Some(resource) = resource {
                if state.resources.contains_key(&resource) {
                    Self::close_resource_with_terminal_locked(
                        &mut state,
                        resource,
                        Terminal::Closed,
                    )?
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            (
                state
                    .operations
                    .remove(&operation)
                    .ok_or(HubError::Invalid)?,
                notifications,
            )
        };
        slot.notify.notify_one();
        drop(slot);
        for notify in notifications {
            notify.notify_one();
        }
        Ok(())
    }

    pub(crate) fn abandon_authenticated_archive(
        &self,
        store: u64,
        token: u64,
    ) -> Result<(), HubError> {
        let notifications = {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            let token = OpaqueToken(token);
            let slot = state.resources.get(&token).ok_or(HubError::Invalid)?;
            Self::validate_resource(slot, store, ARCHIVE_KIND, EXTRACT_RIGHT)?;
            Self::close_resource_with_terminal_locked(&mut state, token, Terminal::Closed)?
        };
        for notify in notifications {
            notify.notify_one();
        }
        Ok(())
    }

    fn begin_archive_authentication(
        &self,
        store: u64,
        key: &[u8; 16],
        nonce: &[u8; 12],
        aad: &[u8],
        expected: u64,
    ) -> Result<OpaqueToken, HubError> {
        // Reserve operation authority before touching crypto or storage.
        let (operation, _) = self.submit(store, None, 0, 0)?;
        if let Err(error) =
            self.start_archive_authentication(store, operation, key, nonce, aad, expected)
        {
            self.state
                .lock()
                .map_err(|_| HubError::Closed)?
                .operations
                .remove(&operation);
            return Err(error);
        }
        Ok(operation)
    }

    pub(super) fn start_archive_authentication(
        &self,
        store: u64,
        operation: OpaqueToken,
        key: &[u8; 16],
        nonce: &[u8; 12],
        aad: &[u8],
        expected: u64,
    ) -> Result<(), HubError> {
        {
            let state = self.state.lock().map_err(|_| HubError::Closed)?;
            let slot = state.operations.get(&operation).ok_or(HubError::Closed)?;
            if slot.owner.store != store || slot.terminal.is_some() {
                return Err(HubError::Closed);
            }
        }
        let pending = Authentication::begin(key, nonce, aad, expected, &self.staging_budget);
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let active = state
            .operations
            .get(&operation)
            .is_some_and(|slot| slot.terminal.is_none());
        match pending {
            Ok(pending) if active => {
                state
                    .operations
                    .get_mut(&operation)
                    .ok_or(HubError::Closed)?
                    .pending_authentication = Some(pending);
                Ok(())
            }
            result => {
                drop(result);
                Err(if active {
                    HubError::Invalid
                } else {
                    HubError::Closed
                })
            }
        }
    }

    pub(super) fn update_archive_authentication(
        &self,
        store: u64,
        operation: OpaqueToken,
        ciphertext: &[u8],
    ) -> Result<(), HubError> {
        let mut pending = {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            let slot = state
                .operations
                .get_mut(&operation)
                .ok_or(HubError::Invalid)?;
            if slot.owner.store != store {
                return Err(HubError::WrongRights);
            }
            if slot.terminal.is_some() {
                return Err(HubError::Closed);
            }
            slot.pending_authentication
                .take()
                .ok_or(HubError::Invalid)?
        };
        // A concurrent terminal event can run without waiting for this write.
        // Until it returns, the local owner retains the staging reservation.
        let result = pending.update(ciphertext);
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let slot = state
            .operations
            .get_mut(&operation)
            .ok_or(HubError::Closed)?;
        if slot.terminal.is_some() {
            return Err(HubError::Closed);
        }
        if result.is_ok() {
            slot.pending_authentication = Some(pending);
            return Ok(());
        }
        let notify = Self::terminal_locked(
            &mut state,
            operation,
            TerminalResult {
                terminal: Terminal::Rejected,
                resource: None,
            },
        )?;
        drop(state);
        if let Some(notify) = notify {
            notify.notify_one();
        }
        Err(HubError::Invalid)
    }

    pub(super) fn finish_archive_authentication(
        &self,
        store: u64,
        operation: OpaqueToken,
        tag: &[u8; 16],
    ) -> Result<(), HubError> {
        self.finish_archive_authentication_with(store, operation, tag, || {})
    }

    fn finish_archive_authentication_with(
        &self,
        store: u64,
        operation: OpaqueToken,
        tag: &[u8; 16],
        after_authentication: impl FnOnce(),
    ) -> Result<(), HubError> {
        let pending = {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            let slot = state
                .operations
                .get_mut(&operation)
                .ok_or(HubError::Invalid)?;
            if slot.owner.store != store {
                return Err(HubError::WrongRights);
            }
            if slot.terminal.is_some() {
                return Err(HubError::Closed);
            }
            slot.pending_authentication
                .take()
                .ok_or(HubError::Invalid)?
        };
        // Final crypto, flush, and seek are outside the authority mutex.
        let authenticated = pending.authenticate(tag);
        // Deterministic test seam for cancellation after verification but
        // before publication. Ordinary finalization supplies an empty closure.
        if authenticated.is_ok() {
            after_authentication();
        }
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        if state.closed
            || state
                .operations
                .get(&operation)
                .is_none_or(|slot| slot.terminal.is_some())
        {
            return Err(HubError::Closed);
        }
        let resource = match authenticated {
            Ok(archive) => self.create_resource_value_locked(
                &mut state,
                store,
                ARCHIVE_KIND,
                EXTRACT_RIGHT,
                false,
                ResourceValue::AuthenticatedArchive(archive),
            ),
            Err(_) => Err(HubError::Invalid),
        };
        let completion = match resource {
            Ok(resource) => {
                state
                    .operations
                    .get_mut(&operation)
                    .ok_or(HubError::Closed)?
                    .created_resource = Some(resource);
                TerminalResult {
                    terminal: Terminal::Completed,
                    resource: Some(resource),
                }
            }
            Err(_) => TerminalResult {
                terminal: Terminal::Rejected,
                resource: None,
            },
        };
        // Insertion, linkage, activation, and terminal publication share one
        // lock interval. Cancellation cannot observe an unowned live file.
        let notify = Self::terminal_locked(&mut state, operation, completion)?;
        drop(state);
        if let Some(notify) = notify {
            notify.notify_one();
        }
        resource.map(|_| ())
    }

    fn register_authenticated_archive(
        &self,
        store: u64,
        archive: Authenticated,
    ) -> Result<OpaqueToken, HubError> {
        // A file authenticated under a different quota cannot be imported to
        // bypass this logical sketch's storage ceiling.
        if !archive.belongs_to(&self.staging_budget) {
            return Err(HubError::WrongRights);
        }
        let token = self.create_resource_value(
            store,
            ARCHIVE_KIND,
            EXTRACT_RIGHT,
            false,
            ResourceValue::AuthenticatedArchive(archive),
        )?;
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        state
            .resources
            .get_mut(&token)
            .ok_or(HubError::Closed)?
            .reserved = false;
        Ok(token)
    }

    fn extract_authenticated_archive(
        &self,
        store: u64,
        token: OpaqueToken,
        destination: &std::path::Path,
        limits: crate::archive::ExtractionLimits,
    ) -> io::Result<()> {
        let invalid = |error: HubError| io::Error::other(format!("archive resource: {error:?}"));
        let mut state = self.state.lock().map_err(|_| invalid(HubError::Closed))?;
        let slot = state
            .resources
            .get_mut(&token)
            .ok_or_else(|| invalid(HubError::Invalid))?;
        Self::validate_resource(slot, store, ARCHIVE_KIND, EXTRACT_RIGHT).map_err(invalid)?;
        if !matches!(slot.value, ResourceValue::AuthenticatedArchive(_)) {
            return Err(invalid(HubError::WrongKind));
        }
        let ResourceValue::AuthenticatedArchive(archive) =
            std::mem::replace(&mut slot.value, ResourceValue::Synthetic)
        else {
            return Err(invalid(HubError::WrongKind));
        };
        let notifications =
            Self::close_resource_with_terminal_locked(&mut state, token, Terminal::Closed)
                .map_err(invalid)?;
        drop(state);
        for notify in notifications {
            notify.notify_one();
        }
        // All filesystem work is outside the hub mutex. The consumed file
        // retains the storage charge until the extractor returns.
        archive.extract(destination, limits)
    }
}

fn authenticated(hub: &OperationHub) -> Authenticated {
    let mut pending =
        Authentication::begin(&[0; 16], &[0; 12], &[], 16, &hub.staging_budget).unwrap();
    assert_eq!(hub.snapshot().live_resources, 0);
    pending
        .update(&[
            0x03, 0x88, 0xda, 0xce, 0x60, 0xb6, 0xa3, 0x92, 0xf3, 0x28, 0xc2, 0xb9, 0x71, 0xb2,
            0xfe, 0x78,
        ])
        .unwrap();
    pending
        .authenticate(&[
            0xab, 0x6e, 0x47, 0xd4, 0x2c, 0xec, 0x13, 0xbd, 0xf5, 0x3a, 0x67, 0xb2, 0x12, 0x57,
            0xbd, 0xdf,
        ])
        .unwrap()
}

#[test]
fn authenticated_archive_registry_rejects_foreign_owners_and_stale_tokens() {
    let hub = OperationHub::new(8, 2).unwrap();
    let foreign = OperationHub::new(8, 2).unwrap();
    let token = hub
        .register_authenticated_archive(1, authenticated(&hub))
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("output");
    let limits = crate::archive::ExtractionLimits::default();
    assert!(hub
        .extract_authenticated_archive(2, token, &output, limits)
        .is_err());
    assert!(foreign
        .extract_authenticated_archive(1, token, &output, limits)
        .is_err());
    assert!(!output.exists());
    assert_eq!(hub.staging_budget.used(), 16);
    assert_eq!(hub.snapshot().live_resources, 1);
    hub.close_resource(token).unwrap();
    assert_eq!(hub.staging_budget.used(), 0);
    let replacement = hub
        .register_authenticated_archive(1, authenticated(&hub))
        .unwrap();
    assert_ne!(token, replacement);
    assert!(hub
        .extract_authenticated_archive(1, token, &output, limits)
        .is_err());
    assert_eq!(hub.staging_budget.used(), 16);
    // The authenticated NIST plaintext is not a ZIP; extraction failure must
    // still consume the live generation and release its file/reservation.
    assert!(hub
        .extract_authenticated_archive(1, replacement, &output, limits)
        .is_err());
    assert_eq!(hub.staging_budget.used(), 0);
    assert_eq!(hub.snapshot().live_resources, 0);
}

#[test]
fn authenticated_archive_registry_rejection_and_teardown_release_storage() {
    let hub = OperationHub::new(8, 2).unwrap();
    let foreign = OperationHub::new(8, 2).unwrap();
    assert_eq!(
        hub.register_authenticated_archive(1, authenticated(&foreign)),
        Err(HubError::WrongRights)
    );
    assert_eq!(foreign.staging_budget.used(), 0);
    let full = OperationHub::new(8, 0).unwrap();
    assert_eq!(
        full.register_authenticated_archive(1, authenticated(&full)),
        Err(HubError::Quota)
    );
    assert_eq!(full.staging_budget.used(), 0);
    for terminal in [
        Terminal::Cancelled,
        Terminal::Trapped,
        Terminal::OwnerExited,
    ] {
        let hub = OperationHub::new(8, 2).unwrap();
        let token = hub
            .register_authenticated_archive(1, authenticated(&hub))
            .unwrap();
        assert_eq!(hub.staging_budget.used(), 16);
        hub.close_all(terminal);
        assert_eq!(hub.staging_budget.used(), 0);
        assert_eq!(hub.snapshot().live_resources, 0);
        assert!(hub.close_resource(token).is_err());
        assert_eq!(
            hub.register_authenticated_archive(1, authenticated(&hub)),
            Err(HubError::Closed)
        );
        assert_eq!(hub.staging_budget.used(), 0);
    }
}

#[test]
#[expect(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn authenticated_archive_registry_extracts_large_zip_with_bounded_transfers() {
    use crate::archive::authenticated_staging::StagingBudget;
    use openssl::symm::{Cipher, Crypter, Mode};
    use std::io::{Read, Seek, Write};

    const CHUNK: usize = 64 * 1024;
    const LENGTH: u64 = 17 * 1024 * 1024;
    // Successful extraction, failed authentication, and entry-size rejection.
    for case in 0..3 {
        let mut writer = zip::ZipWriter::new(tempfile::tempfile().unwrap());
        writer
            .start_file(
                "payload",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        let payload = [0x5a; CHUNK];
        for _ in 0..LENGTH / CHUNK as u64 {
            writer.write_all(&payload).unwrap();
        }
        let mut source = writer.finish().unwrap();
        let length = source.metadata().unwrap().len();
        assert!(length > 16 * 1024 * 1024);
        source.rewind().unwrap();

        let mut hub = OperationHub::new(8, 2).unwrap();
        Arc::get_mut(&mut hub).unwrap().staging_budget = StagingBudget::new(length);
        let key = [19; 16];
        let nonce = [23; 12];
        let aad = b"synthetic registered ZIP";
        let operation = hub
            .begin_archive_authentication(1, &key, &nonce, aad, length)
            .unwrap();
        let mut encoder =
            Crypter::new(Cipher::aes_128_gcm(), Mode::Encrypt, &key, Some(&nonce)).unwrap();
        encoder.aad_update(aad).unwrap();
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        let mut input = [0; CHUNK];
        let mut ciphertext = [0; CHUNK + 16];
        loop {
            let count = source.read(&mut input).unwrap();
            if count == 0 {
                break;
            }
            let encrypted = encoder.update(&input[..count], &mut ciphertext).unwrap();
            hub.update_archive_authentication(1, operation, &ciphertext[..encrypted])
                .unwrap();
            assert_eq!(hub.snapshot().live_resources, 0);
            assert_eq!(hub.staging_budget.used(), length);
            assert!(!output.exists());
        }
        assert_eq!(encoder.finalize(&mut ciphertext).unwrap(), 0);
        let mut tag = [0; 16];
        encoder.get_tag(&mut tag).unwrap();
        if case == 1 {
            tag[0] ^= 1;
        }
        let authenticated = hub.finish_archive_authentication(1, operation, &tag);
        let completion = hub.observe_terminal(1, operation).unwrap().unwrap();
        if case == 1 {
            assert_eq!(authenticated, Err(HubError::Invalid));
            assert_eq!(completion.terminal, Terminal::Rejected);
            assert_eq!(completion.resource, None);
            assert!(!output.exists());
        } else {
            authenticated.unwrap();
            assert_eq!(completion.terminal, Terminal::Completed);
            let token = completion.resource.unwrap();
            assert_eq!(hub.snapshot().live_resources, 1);
            assert_eq!(hub.staging_budget.used(), length);
            assert!(Authentication::begin(&key, &nonce, aad, 1, &hub.staging_budget).is_err());
            let limits = crate::archive::ExtractionLimits {
                max_input_bytes: length,
                max_output_bytes: LENGTH,
                max_entry_bytes: if case == 2 { LENGTH - 1 } else { LENGTH },
                max_entries: 1,
                ..crate::archive::ExtractionLimits::default()
            };
            let result = hub.extract_authenticated_archive(1, token, &output, limits);
            if case == 2 {
                assert!(result.is_err());
                assert!(!output.join("payload").exists());
            } else {
                result.unwrap();
                let mut file = std::fs::File::open(output.join("payload")).unwrap();
                let mut total = 0;
                loop {
                    let count = file.read(&mut input).unwrap();
                    if count == 0 {
                        break;
                    }
                    assert!(input[..count].iter().all(|byte| *byte == 0x5a));
                    total += count as u64;
                }
                assert_eq!(total, LENGTH);
            }
            assert!(hub
                .extract_authenticated_archive(1, token, &output, limits)
                .is_err());
        }
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.staging_budget.used(), 0);
    }
}

#[test]
fn authenticated_archive_pending_operation_cancellation_reclaims_staging() {
    let hub = OperationHub::new(2, 2).unwrap();
    let operation = hub
        .begin_archive_authentication(1, &[0; 16], &[0; 12], &[], 16)
        .unwrap();
    hub.update_archive_authentication(1, operation, &[0; 7])
        .unwrap();
    assert_eq!(hub.staging_budget.used(), 16);
    assert_eq!(hub.snapshot().live_resources, 0);
    assert_eq!(hub.snapshot().pending_operations, 1);
    assert_eq!(
        hub.update_archive_authentication(2, operation, &[]),
        Err(HubError::WrongRights)
    );
    assert!(hub.cancel_wire(2, operation.wire()).is_err());
    assert_eq!(hub.staging_budget.used(), 16);
    hub.cancel_wire(1, operation.wire()).unwrap();
    assert_eq!(hub.staging_budget.used(), 0);
    assert_eq!(
        hub.update_archive_authentication(1, operation, &[]),
        Err(HubError::Closed)
    );
    assert_eq!(
        hub.observe_terminal(1, operation)
            .unwrap()
            .unwrap()
            .terminal,
        Terminal::Cancelled
    );
    assert_eq!(hub.snapshot().pending_operations, 0);
    assert_eq!(hub.snapshot().live_resources, 0);
}

#[test]
fn authenticated_archive_pending_errors_and_teardown_reclaim_staging() {
    for terminal in [Terminal::Trapped, Terminal::OwnerExited, Terminal::TimedOut] {
        let hub = OperationHub::new(1, 1).unwrap();
        let operation = hub
            .begin_archive_authentication(1, &[0; 16], &[0; 12], &[], 16)
            .unwrap();
        hub.update_archive_authentication(1, operation, &[0; 7])
            .unwrap();
        hub.close_all(terminal);
        assert_eq!(hub.staging_budget.used(), 0);
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(
            hub.observe_terminal(1, operation)
                .unwrap()
                .unwrap()
                .terminal,
            terminal
        );
        assert_eq!(
            hub.begin_archive_authentication(1, &[0; 16], &[0; 12], &[], 16),
            Err(HubError::Closed)
        );
        assert_eq!(hub.staging_budget.used(), 0);
    }
    let hub = OperationHub::new(1, 1).unwrap();
    assert!(hub
        .begin_archive_authentication(1, &[0; 16], &[0; 12], &[], u64::MAX)
        .is_err());
    assert_eq!(hub.snapshot().pending_operations, 0);
    let operation = hub
        .begin_archive_authentication(1, &[0; 16], &[0; 12], &[], 16)
        .unwrap();
    assert!(hub
        .update_archive_authentication(1, operation, &[0; 17])
        .is_err());
    assert_eq!(hub.staging_budget.used(), 0);
    assert_eq!(
        hub.observe_terminal(1, operation)
            .unwrap()
            .unwrap()
            .terminal,
        Terminal::Rejected
    );
}

#[test]
fn authenticated_archive_finalization_does_not_publish_after_cancellation() {
    for collect in [false, true] {
        let hub = OperationHub::new(2, 2).unwrap();
        let (operation, tag) = pending_nist_operation(&hub);
        let verified = std::cell::Cell::new(false);
        assert_eq!(
            hub.finish_archive_authentication_with(1, operation, &tag, || {
                verified.set(true);
                assert_eq!(hub.staging_budget.used(), 16);
                hub.cancel_wire(1, operation.wire()).unwrap();
                if collect {
                    assert_eq!(
                        hub.observe_terminal(1, operation)
                            .unwrap()
                            .unwrap()
                            .terminal,
                        Terminal::Cancelled
                    );
                }
            }),
            Err(HubError::Closed)
        );
        assert!(verified.get());
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.staging_budget.used(), 0);
        if !collect {
            assert_eq!(
                hub.observe_terminal(1, operation)
                    .unwrap()
                    .unwrap()
                    .terminal,
                Terminal::Cancelled
            );
        }
    }
}

#[test]
fn authenticated_archive_finalization_resource_quota_rejects_and_cleans_up() {
    let hub = OperationHub::new(2, 0).unwrap();
    let (operation, tag) = pending_nist_operation(&hub);
    assert_eq!(
        hub.finish_archive_authentication(2, operation, &tag),
        Err(HubError::WrongRights)
    );
    assert_eq!(hub.staging_budget.used(), 16);
    assert_eq!(
        hub.finish_archive_authentication(1, operation, &tag),
        Err(HubError::Quota)
    );
    let completion = hub.observe_terminal(1, operation).unwrap().unwrap();
    assert_eq!(completion.terminal, Terminal::Rejected);
    assert_eq!(completion.resource, None);
    assert_eq!(hub.snapshot().live_resources, 0);
    assert_eq!(hub.staging_budget.used(), 0);
}

fn pending_nist_operation(hub: &OperationHub) -> (OpaqueToken, [u8; 16]) {
    let operation = hub
        .begin_archive_authentication(1, &[0; 16], &[0; 12], &[], 16)
        .unwrap();
    hub.update_archive_authentication(
        1,
        operation,
        &[
            0x03, 0x88, 0xda, 0xce, 0x60, 0xb6, 0xa3, 0x92, 0xf3, 0x28, 0xc2, 0xb9, 0x71, 0xb2,
            0xfe, 0x78,
        ],
    )
    .unwrap();
    (
        operation,
        [
            0xab, 0x6e, 0x47, 0xd4, 0x2c, 0xec, 0x13, 0xbd, 0xf5, 0x3a, 0x67, 0xb2, 0x12, 0x57,
            0xbd, 0xdf,
        ],
    )
}
