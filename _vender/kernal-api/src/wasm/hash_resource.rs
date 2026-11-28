//! Scoped incremental hashing backed by the kernel's existing BLAKE3 facade.

use super::*;

const HASH_KIND: u8 = 9;
const HASH_RIGHT: u8 = 1;
pub(super) const MAX_HASH_CHUNK: usize = 64 * 1024;

impl OperationHub {
    pub(crate) fn submit_hash_create(&self, store: u64) -> Result<OpaqueToken, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let (operation, _) = self.submit_locked(&mut state, store, None, HASH_KIND, HASH_RIGHT)?;
        let resource = match self.create_resource_value_locked(
            &mut state,
            store,
            HASH_KIND,
            HASH_RIGHT,
            false,
            ResourceValue::Blake3(Box::default()),
        ) {
            Ok(resource) => resource,
            Err(error) => {
                state.operations.remove(&operation);
                return Err(error);
            }
        };
        let slot = state
            .operations
            .get_mut(&operation)
            .ok_or(HubError::Closed)?;
        slot.is_hash_operation = true;
        slot.created_resource = Some(resource);
        // terminal_locked publishes and activates the newly owned resource.
        let notify = Self::terminal_locked(
            &mut state,
            operation,
            TerminalResult {
                terminal: Terminal::Completed,
                resource: Some(resource),
            },
        )?;
        drop(state);
        if let Some(notify) = notify {
            notify.notify_one();
        }
        Ok(operation)
    }

    pub(crate) fn submit_hash_update(
        &self,
        store: u64,
        hash: OpaqueToken,
        bytes: &[u8],
    ) -> Result<OpaqueToken, HubError> {
        if bytes.len() > MAX_HASH_CHUNK {
            return Err(HubError::Quota);
        }
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let resource = state.resources.get(&hash).ok_or(HubError::Closed)?;
        Self::validate_resource(resource, store, HASH_KIND, HASH_RIGHT)?;
        if !matches!(resource.value, ResourceValue::Blake3(_)) {
            return Err(HubError::WrongKind);
        }
        let (operation, _) =
            self.submit_locked(&mut state, store, Some(hash), HASH_KIND, HASH_RIGHT)?;
        let resource = state.resources.get_mut(&hash).ok_or(HubError::Closed)?;
        if let ResourceValue::Blake3(hasher) = &mut resource.value {
            hasher.update(bytes);
        }
        state
            .operations
            .get_mut(&operation)
            .ok_or(HubError::Closed)?
            .is_hash_operation = true;
        let notify = Self::terminal_locked(
            &mut state,
            operation,
            TerminalResult {
                terminal: Terminal::Completed,
                resource: None,
            },
        )?;
        drop(state);
        if let Some(notify) = notify {
            notify.notify_one();
        }
        Ok(operation)
    }

    /// Discard uncollected creation, or revoke an update's uncertain hash state.
    /// This uses no additional operation slot and validates before any removal.
    pub(crate) fn abandon_hash_operation(
        &self,
        store: u64,
        token: OpaqueToken,
    ) -> Result<(), HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let operation = state.operations.get(&token).ok_or(HubError::Invalid)?;
        if operation.owner.store != store {
            return Err(HubError::Stale);
        }
        if !operation.is_hash_operation {
            return Err(HubError::WrongKind);
        }
        let operation = state.operations.remove(&token).ok_or(HubError::Invalid)?;
        let mut notifications = Vec::new();
        if let Some(resource) = operation.created_resource.or(operation.resource) {
            if state.resources.contains_key(&resource) {
                notifications = Self::close_resource_with_terminal_locked(
                    &mut state,
                    resource,
                    Terminal::Closed,
                )?;
            }
        }
        drop(state);
        operation.notify.notify_one();
        for notify in notifications {
            notify.notify_one();
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn create_hash(&self, store: u64) -> Result<OpaqueToken, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let token = self.create_resource_value_locked(
            &mut state,
            store,
            HASH_KIND,
            HASH_RIGHT,
            false,
            ResourceValue::Blake3(Box::default()),
        )?;
        state
            .resources
            .get_mut(&token)
            .ok_or(HubError::Closed)?
            .reserved = false;
        Ok(token)
    }

    #[cfg(test)]
    pub(crate) fn update_hash(
        &self,
        store: u64,
        token: OpaqueToken,
        bytes: &[u8],
    ) -> Result<(), HubError> {
        if bytes.len() > MAX_HASH_CHUNK {
            return Err(HubError::Quota);
        }
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let resource = state.resources.get_mut(&token).ok_or(HubError::Closed)?;
        Self::validate_resource(resource, store, HASH_KIND, HASH_RIGHT)?;
        let ResourceValue::Blake3(hasher) = &mut resource.value else {
            return Err(HubError::WrongKind);
        };
        hasher.update(bytes);
        Ok(())
    }

    pub(crate) fn finish_hash_into(
        &self,
        store: u64,
        token: OpaqueToken,
        destination: &mut [u8],
    ) -> Result<(), HubError> {
        if destination.len() != 32 {
            return Err(HubError::Invalid);
        }
        let digest = self.finish_hash(store, token)?;
        destination.copy_from_slice(&digest);
        Ok(())
    }

    /// Native consuming finalization. ABI callers must validate output memory
    /// before calling this method; rejection must not consume retry authority.
    pub(crate) fn finish_hash(&self, store: u64, token: OpaqueToken) -> Result<[u8; 32], HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let resource = state.resources.get(&token).ok_or(HubError::Closed)?;
        Self::validate_resource(resource, store, HASH_KIND, HASH_RIGHT)?;
        let ResourceValue::Blake3(hasher) = &resource.value else {
            return Err(HubError::WrongKind);
        };
        let digest = *hasher.finalize().as_bytes();
        let notifications =
            Self::close_resource_with_terminal_locked(&mut state, token, Terminal::Closed)?;
        drop(state);
        for notify in notifications {
            notify.notify_one();
        }
        Ok(digest)
    }

    pub(crate) fn abandon_hash(&self, store: u64, token: OpaqueToken) -> Result<(), HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let resource = state.resources.get(&token).ok_or(HubError::Closed)?;
        Self::validate_resource(resource, store, HASH_KIND, HASH_RIGHT)?;
        let notifications =
            Self::close_resource_with_terminal_locked(&mut state, token, Terminal::Closed)?;
        drop(state);
        for notify in notifications {
            notify.notify_one();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;

    #[test]
    fn hash_finish_rejects_wrong_capacity_without_consuming_or_writing() {
        let hub = OperationHub::new(1, 1).unwrap();
        let hash = hub.create_hash(1).unwrap();
        hub.update_hash(1, hash, b"abc").unwrap();
        for length in [0, 31, 33, 64] {
            let mut destination = vec![0xa5; length];
            assert!(hub.finish_hash_into(1, hash, &mut destination).is_err());
            assert!(destination.iter().all(|byte| *byte == 0xa5));
            assert_eq!(hub.snapshot().live_resources, 1);
        }
        let mut digest = [0; 32];
        hub.finish_hash_into(1, hash, &mut digest).unwrap();
        assert_eq!(digest, *crate::hash::blake3_bytes(b"abc").as_bytes());
        assert_eq!(hub.snapshot().live_resources, 0);
    }

    #[test]
    fn hash_operation_collection_preserves_hash_and_reclaims_slots() {
        let hub = OperationHub::new(1, 1).unwrap();
        let create = hub.submit_hash_create(1).unwrap();
        assert!(hub.take_terminal(create, 2).is_err());
        let hash = hub
            .take_terminal(create, 1)
            .unwrap()
            .unwrap()
            .resource
            .unwrap();
        assert!(hub.abandon_hash_operation(1, create).is_err());
        let update = hub.submit_hash_update(1, hash, b"abc").unwrap();
        assert_eq!(
            hub.take_terminal(update, 1).unwrap().unwrap().terminal,
            Terminal::Completed
        );
        assert!(hub.abandon_hash_operation(1, update).is_err());
        assert!(hub.state.lock().unwrap().operations.is_empty());
        assert_eq!(
            hub.finish_hash(1, hash).unwrap(),
            *crate::hash::blake3_bytes(b"abc").as_bytes()
        );
    }

    #[test]
    fn hash_operation_resource_quota_rolls_back_operation_reservation() {
        let hub = OperationHub::new(1, 1).unwrap();
        let hash = hub.create_hash(1).unwrap();
        assert!(hub.submit_hash_create(1).is_err());
        assert!(hub.state.lock().unwrap().operations.is_empty());
        hub.abandon_hash(1, hash).unwrap();
        assert!(hub.submit_hash_create(1).is_ok());
    }

    #[test]
    fn hash_operation_drop_reclaims_uncollected_creation_at_quota() {
        let hub = OperationHub::new(1, 1).unwrap();
        let operation = hub.submit_hash_create(1).unwrap();
        assert_eq!(hub.snapshot().live_resources, 1);
        assert!(hub.submit_hash_create(1).is_err());
        assert!(hub.abandon_hash_operation(2, operation).is_err());
        hub.abandon_hash_operation(1, operation).unwrap();
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.snapshot().pending_operations, 0);
        assert!(hub.submit_hash_create(1).is_ok());
    }

    #[test]
    fn hash_operation_quota_rejects_update_before_mutation() {
        let hub = OperationHub::new(1, 2).unwrap();
        let hash = hub.create_hash(1).unwrap();
        let occupied = hub.submit_hash_create(1).unwrap();
        assert!(hub
            .submit_hash_update(1, hash, b"must not be hashed")
            .is_err());
        hub.abandon_hash_operation(1, occupied).unwrap();
        assert_eq!(
            hub.finish_hash(1, hash).unwrap(),
            *crate::hash::blake3_bytes(b"").as_bytes()
        );
    }

    #[test]
    fn hash_operation_abandoned_update_revokes_uncertain_state() {
        let hub = OperationHub::new(1, 1).unwrap();
        let hash = hub.create_hash(1).unwrap();
        let operation = hub.submit_hash_update(1, hash, b"committed").unwrap();
        hub.abandon_hash_operation(1, operation).unwrap();
        assert!(hub.update_hash(1, hash, b"cannot replay").is_err());
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.snapshot().pending_operations, 0);
    }

    #[test]
    fn hash_resource_bounds_updates_and_consumes_finish() {
        let hub = OperationHub::new(4, 1).unwrap();
        let hash = hub.create_hash(1).unwrap();
        assert!(hub.create_hash(1).is_err());
        hub.update_hash(1, hash, b"a").unwrap();
        assert!(hub.update_hash(1, hash, &[0; 65537]).is_err());
        hub.update_hash(1, hash, b"bc").unwrap();
        assert_eq!(
            hub.finish_hash(1, hash).unwrap(),
            *crate::hash::blake3_bytes(b"abc").as_bytes()
        );
        assert!(hub.finish_hash(1, hash).is_err());
        let replacement = hub.create_hash(1).unwrap();
        assert_ne!(hash, replacement);
        assert!(hub.update_hash(1, hash, b"stale").is_err());
        hub.abandon_hash(1, replacement).unwrap();
        assert!(hub.create_hash(1).is_ok());
    }

    #[test]
    fn hash_resource_rejects_foreign_owners_and_hubs() {
        let hub = OperationHub::new(4, 1).unwrap();
        let other = OperationHub::new(4, 1).unwrap();
        let hash = hub.create_hash(1).unwrap();
        assert!(hub.update_hash(2, hash, b"foreign").is_err());
        assert!(hub.finish_hash(2, hash).is_err());
        assert!(hub.abandon_hash(2, hash).is_err());
        assert!(other.update_hash(1, hash, b"foreign").is_err());
        assert_eq!(
            hub.finish_hash(1, hash).unwrap(),
            *crate::hash::blake3_bytes(b"").as_bytes()
        );
    }

    #[test]
    fn hash_resource_rejects_other_resource_kinds_without_closing_them() {
        let hub = OperationHub::new(4, 1).unwrap();
        let blob = hub.create_blob(1).unwrap();
        assert!(hub.update_hash(1, blob, b"wrong kind").is_err());
        assert!(hub.finish_hash(1, blob).is_err());
        assert!(hub.abandon_hash(1, blob).is_err());
        assert_eq!(hub.snapshot().live_resources, 1);
        hub.close_all(Terminal::Closed);
    }

    #[test]
    fn hash_resource_streams_large_input_and_root_close_revokes_it() {
        let hub = OperationHub::new(4, 2).unwrap();
        let hash = hub.create_hash(1).unwrap();
        let chunk = [0x5a; 65536];
        let mut expected = crate::hash::Blake3Hasher::new();
        for _ in 0..1024 {
            hub.update_hash(1, hash, &chunk).unwrap();
            expected.update(&chunk);
        }
        assert_eq!(
            hub.finish_hash(1, hash).unwrap(),
            *expected.finalize().as_bytes()
        );
        assert_eq!(hub.snapshot().live_resources, 0);
        let unfinished = hub.create_hash(1).unwrap();
        hub.close_all(Terminal::Cancelled);
        assert_eq!(hub.snapshot().live_resources, 0);
        assert!(hub.update_hash(1, unfinished, b"late").is_err());
        assert!(hub.create_hash(1).is_err());
    }
}
