//! Shared compiler capacity for cache misses and resident compiler processes.
//!
//! zccache calls [`HostAdmissionClassifier::acquire`] only after every cache
//! lookup has missed and retains the returned permit until the real compiler
//! exits. Resident processes reserve weighted permits from the same semaphore,
//! so a reservation of `N` leaves exactly `max - N` cache-miss compiler slots.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use zccache::embedded::{
    HostAdmissionClassifier, HostAdmissionError, HostAdmissionPermit, HostCompilerRequest,
};

/// The daemon-owned capacity shared by cache-miss compilers and resident work.
#[derive(Debug)]
pub(crate) struct ResidentCompileAdmission {
    capacity: Arc<Semaphore>,
    max: usize,
    /// soldr#3152 shadow mode: where each admitted Rust compile's estimated
    /// peak memory is logged. `None` disables the log.
    estimate_log: Option<std::path::PathBuf>,
    /// soldr#3152 step 4: each unit's remembered peak, logged in shadow mode.
    unit_history: Option<crate::daemon::unit_memory_history::UnitMemoryHistory>,
}

impl ResidentCompileAdmission {
    pub(crate) fn new(max: usize) -> Self {
        assert!(max > 0, "compiler capacity must have at least one slot");
        Self {
            capacity: Arc::new(Semaphore::new(max)),
            max,
            estimate_log: None,
            unit_history: None,
        }
    }

    /// Log a shadow-mode memory estimate for every compile this classifies
    /// (soldr#3152). Admission decisions are unchanged.
    /// Include each unit's remembered peak in the shadow estimate rows.
    #[must_use]
    pub(crate) fn with_unit_history(
        mut self,
        history: crate::daemon::unit_memory_history::UnitMemoryHistory,
    ) -> Self {
        self.unit_history = Some(history);
        self
    }

    #[must_use]
    pub(crate) fn with_estimate_log(mut self, log: std::path::PathBuf) -> Self {
        self.estimate_log = Some(log);
        self
    }

    async fn acquire_compiler(&self) -> Result<OwnedSemaphorePermit, HostAdmissionError> {
        Arc::clone(&self.capacity)
            .acquire_owned()
            .await
            .map_err(|error| HostAdmissionError::new(error.to_string()))
    }

    /// Reserve weighted capacity while always leaving one slot for compilers.
    pub(crate) async fn acquire_resident(
        &self,
        permits: u32,
    ) -> Result<OwnedSemaphorePermit, ResidentCapacityError> {
        let permits_usize = usize::try_from(permits).unwrap_or(usize::MAX);
        if permits == 0 {
            return Err(ResidentCapacityError::Zero);
        }
        if permits_usize >= self.max {
            return Err(ResidentCapacityError::ExhaustsCompilerCapacity {
                requested: permits,
                max: self.max,
            });
        }
        Arc::clone(&self.capacity)
            .acquire_many_owned(permits)
            .await
            .map_err(|error| ResidentCapacityError::Closed(error.to_string()))
    }

    /// soldr#3152 step 4b: additive exclusivity from the unit's measured
    /// history. Only a trusted, large remembered unit pays for a memory probe.
    fn history_requires_exclusive(&self, request: &HostCompilerRequest<'_>) -> bool {
        if request.family() != zccache::compiler::CompilerFamily::Rustc {
            return false;
        }
        let Some(history) = self.unit_history.as_ref() else {
            return false;
        };
        let Some(unit_key) = crate::memory_estimate::unit_key(request.args()) else {
            return false;
        };
        // soldr#3686: right after a restart, wait (bounded) for the warm load so
        // the trusted history is not answered from an empty map.
        let remembered = history.lookup_after_load(
            &unit_key,
            crate::daemon::unit_memory_history::ADMISSION_LOAD_WAIT,
        );
        if !crate::history_admission::worth_probing(remembered) {
            return false;
        }
        let snapshot = soldr_platform::host::resources::HostResourceSnapshot::capture();
        let available = crate::history_admission::available_bytes(&snapshot);
        let exclusive = crate::history_admission::history_requires_exclusive(remembered, available);
        if exclusive {
            // Rare and decision-relevant, like the classifier's own line.
            eprintln!(
                "soldr-daemon: compiler admission requests exclusive access for {unit_key}: remembered tree peak {} MiB does not fit twice in {} MiB available",
                remembered.map_or(0, |unit| unit.tree_peak_rss_bytes) / (1024 * 1024),
                available.unwrap_or(0) / (1024 * 1024)
            );
        }
        exclusive
    }

    #[cfg(test)]
    fn available_permits(&self) -> usize {
        self.capacity.available_permits()
    }
}

impl HostAdmissionClassifier for ResidentCompileAdmission {
    fn requires_exclusive(
        &self,
        request: &HostCompilerRequest<'_>,
    ) -> Result<bool, HostAdmissionError> {
        let classified =
            crate::amalgamation::SoldrHostAdmissionClassifier.requires_exclusive(request)?;
        let exclusive = classified || self.history_requires_exclusive(request);
        if let Some(log) = &self.estimate_log {
            crate::memory_estimate::shadow_with_history(
                log,
                request.family() == zccache::compiler::CompilerFamily::Rustc,
                request.args(),
                exclusive,
                self.unit_history.as_ref(),
            );
        }
        Ok(exclusive)
    }

    fn acquire<'a>(
        &'a self,
        _request: HostCompilerRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<HostAdmissionPermit, HostAdmissionError>> + Send + 'a>>
    {
        Box::pin(async move {
            crate::admission_progress::phase(1);
            let permit = self.acquire_compiler().await?;
            crate::admission_progress::phase(2);
            Ok(HostAdmissionPermit::new(permit))
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ResidentCapacityError {
    #[error("resident capacity requires at least one permit")]
    Zero,
    #[error(
        "resident capacity reservation of {requested} permits would leave no compiler slot (maximum capacity is {max})"
    )]
    ExhaustsCompilerCapacity { requested: u32, max: usize },
    #[error("compiler capacity semaphore closed: {0}")]
    Closed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_reservation_allows_full_compiler_concurrency() {
        let admission = ResidentCompileAdmission::new(4);
        let mut compilers = Vec::new();
        for expected_available in (0..4).rev() {
            compilers.push(admission.acquire_compiler().await.expect("compiler permit"));
            assert_eq!(admission.available_permits(), expected_available);
        }
        assert_eq!(compilers.len(), 4);
    }

    #[tokio::test]
    async fn resident_reservation_reduces_and_release_restores_compiler_capacity() {
        let admission = ResidentCompileAdmission::new(5);
        let resident = admission
            .acquire_resident(2)
            .await
            .expect("resident reservation");
        assert_eq!(admission.available_permits(), 3);

        let mut compilers = Vec::new();
        for expected_available in (0..3).rev() {
            compilers.push(admission.acquire_compiler().await.expect("compiler permit"));
            assert_eq!(admission.available_permits(), expected_available);
        }

        drop(compilers);
        assert_eq!(admission.available_permits(), 3);
        drop(resident);
        assert_eq!(admission.available_permits(), 5);
    }

    #[tokio::test]
    async fn resident_reservation_rejects_zero_and_the_last_compiler_slot() {
        let admission = ResidentCompileAdmission::new(4);
        assert!(matches!(
            admission.acquire_resident(0).await,
            Err(ResidentCapacityError::Zero)
        ));
        assert!(matches!(
            admission.acquire_resident(4).await,
            Err(ResidentCapacityError::ExhaustsCompilerCapacity { .. })
        ));
        assert!(matches!(
            admission.acquire_resident(5).await,
            Err(ResidentCapacityError::ExhaustsCompilerCapacity { .. })
        ));
        assert_eq!(admission.available_permits(), 4);
    }
}
