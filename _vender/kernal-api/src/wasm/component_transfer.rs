//! Component table/return-value reservations, retained across Store teardown.
use super::*;

struct BudgetState {
    resources: usize,
    charged_bytes: usize,
    draining: bool,
}

/// Owned outside the Store as well as by its typed resources. Teardown closes
/// admission first, destroys the entire Store, then releases deferred credits.
pub(crate) struct ComponentResourceBudget {
    hub: Arc<OperationHub>,
    state: Mutex<BudgetState>,
}

pub(crate) struct ComponentResourceLease {
    budget: Arc<ComponentResourceBudget>,
    bytes: usize,
}

impl ComponentResourceLease {
    pub(crate) fn covers(&self, hub: &OperationHub, bytes: usize) -> bool {
        std::ptr::eq(Arc::as_ptr(&self.budget.hub), hub) && self.bytes >= bytes
    }
}

impl ComponentResourceBudget {
    pub(crate) fn new(hub: Arc<OperationHub>) -> Arc<Self> {
        Arc::new(Self {
            hub,
            state: Mutex::new(BudgetState {
                resources: 0,
                charged_bytes: 0,
                draining: false,
            }),
        })
    }

    /// Includes pending async returns, not merely entries already in the table.
    /// Non-payload resources request zero bytes but still consume a table slot.
    pub(crate) fn acquire(
        self: &Arc<Self>,
        bytes: usize,
    ) -> Result<ComponentResourceLease, HubError> {
        let mut budget = self.state.lock().map_err(|_| HubError::Closed)?;
        if budget.draining {
            return Err(HubError::Closed);
        }
        if budget.resources >= self.hub.maximum_resources {
            return Err(HubError::Quota);
        }
        let mut state = self.hub.state.lock().map_err(|_| HubError::Closed)?;
        if state.closed {
            return Err(HubError::Closed);
        }
        if OperationHub::transfer_capacity(&state).saturating_add(bytes)
            > self.hub.blob_limits.maximum_sketch_bytes
        {
            return Err(HubError::Quota);
        }
        state.native_transfer_capacity = state
            .native_transfer_capacity
            .checked_add(bytes)
            .ok_or(HubError::Quota)?;
        budget.charged_bytes += bytes;
        budget.resources += 1;
        OperationHub::record_transfer_capacity(&mut state);
        Ok(ComponentResourceLease {
            budget: Arc::clone(self),
            bytes,
        })
    }

    pub(crate) fn begin_teardown(&self) -> Result<(), HubError> {
        self.state.lock().map_err(|_| HubError::Closed)?.draining = true;
        Ok(())
    }

    /// Call only after the whole Wasmtime Store and its canonical lowering
    /// state have been destroyed. Live host futures/resources reject release.
    pub(crate) fn finish_teardown(&self) -> Result<(), HubError> {
        let mut budget = self.state.lock().map_err(|_| HubError::Closed)?;
        if !budget.draining || budget.resources != 0 {
            return Err(HubError::WrongRights);
        }
        let mut state = self.hub.state.lock().map_err(|_| HubError::Closed)?;
        state.native_transfer_capacity = state
            .native_transfer_capacity
            .checked_sub(budget.charged_bytes)
            .ok_or(HubError::Closed)?;
        budget.charged_bytes = 0;
        drop(state);
        drop(budget);
        self.hub.drive_blob_writes()?;
        self.hub.drive_blob_reads()
    }

    #[cfg(test)]
    pub(crate) fn live_resources(&self) -> usize {
        self.state.lock().unwrap().resources
    }
}

impl Drop for ComponentResourceLease {
    fn drop(&mut self) {
        let Ok(mut budget) = self.budget.state.lock() else {
            return;
        };
        budget.resources = budget.resources.saturating_sub(1);
        if budget.draining {
            // Store field order is not evidence that a returned host Vec has
            // finished canonical lowering. The outer owner releases later.
            return;
        }
        let Ok(mut state) = self.budget.hub.state.lock() else {
            return;
        };
        state.native_transfer_capacity = state.native_transfer_capacity.saturating_sub(self.bytes);
        budget.charged_bytes = budget.charged_bytes.saturating_sub(self.bytes);
        drop(state);
        drop(budget);
        let _ = self.budget.hub.drive_blob_writes();
        let _ = self.budget.hub.drive_blob_reads();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_return_credit_is_not_refunded_by_store_field_drop() {
        let hub = OperationHub::new(4, 2).unwrap();
        let budget = ComponentResourceBudget::new(Arc::clone(&hub));
        let first = budget.acquire(65536).unwrap();
        let second = budget.acquire(0).unwrap();
        assert!(matches!(budget.acquire(0), Err(HubError::Quota)));
        assert_eq!(hub.snapshot().retained_transfer_capacity, 65536);
        assert_eq!(budget.finish_teardown(), Err(HubError::WrongRights));
        budget.begin_teardown().unwrap();
        hub.close_all(Terminal::Trapped);
        assert!(matches!(budget.acquire(0), Err(HubError::Closed)));
        assert_eq!(budget.finish_teardown(), Err(HubError::WrongRights));
        drop(first);
        drop(second);
        assert_eq!(budget.live_resources(), 0);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 65536);
        budget.finish_teardown().unwrap();
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        budget.finish_teardown().unwrap();
    }

    #[test]
    fn component_resource_drop_reuses_slot_and_shared_capacity_before_teardown() {
        let hub = OperationHub::new(4, 1).unwrap();
        let budget = ComponentResourceBudget::new(Arc::clone(&hub));
        assert!(matches!(budget.acquire(usize::MAX), Err(HubError::Quota)));
        assert_eq!(budget.live_resources(), 0);
        let lease = budget.acquire(65536).unwrap();
        assert_eq!(hub.snapshot().native_transfer_capacity, 65536);
        drop(lease);
        assert_eq!(hub.snapshot().native_transfer_capacity, 0);
        drop(budget.acquire(65536).unwrap());
        assert_eq!(budget.live_resources(), 0);
    }
}
