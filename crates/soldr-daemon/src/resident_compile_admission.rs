//! Shared compiler capacity for cache misses and resident compiler processes.
//!
//! zccache calls [`HostAdmissionClassifier::acquire`] only after every cache
//! lookup has missed and retains the returned permit until the real compiler
//! exits. Resident processes reserve weighted capacity from the same queue,
//! so a reservation of `N` leaves exactly `max - N` cache-miss compiler slots.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zccache::embedded::{
    HostAdmissionClassifier, HostAdmissionError, HostAdmissionPermit, HostCompilerRequest,
};

mod queue;
use queue::{AdmissionClass, CapacityQueue, Permit, WorkKind};

/// The daemon-owned capacity shared by cache-miss compilers and resident work.
pub(crate) struct ResidentCompileAdmission {
    capacity: Arc<CapacityQueue>,
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
            capacity: Arc::new(CapacityQueue::new(max)),
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

    #[cfg(test)]
    async fn acquire_compiler(&self) -> Permit {
        self.acquire_compiler_for(AdmissionClass::Ordinary, false)
            .await
    }

    async fn acquire_compiler_for(&self, class: AdmissionClass, exclusive: bool) -> Permit {
        self.capacity
            .acquire(1, class, WorkKind::Compiler { exclusive })
            .await
    }

    /// Reserve weighted capacity while always leaving one slot for compilers.
    pub(crate) async fn acquire_resident(
        &self,
        permits: u32,
        lint: bool,
    ) -> Result<(u32, Option<Permit>), ResidentCapacityError> {
        let permits_usize = usize::try_from(permits).unwrap_or(usize::MAX);
        if permits == 0 {
            return Err(ResidentCapacityError::Zero);
        }
        let class = if lint {
            AdmissionClass::Lint
        } else {
            AdmissionClass::Ordinary
        };
        if permits == 1 && self.max == 1 {
            // Rank the launch, then release the slot so a resident process
            // can start a nested compiler on a one-slot daemon.
            drop(self.acquire_compiler_for(class, false).await);
            return Ok((0, None));
        }
        if permits_usize >= self.max {
            return Err(ResidentCapacityError::ExhaustsCompilerCapacity {
                requested: permits,
                max: self.max,
            });
        }
        Ok((
            permits,
            Some(
                self.capacity
                    .acquire(permits_usize, class, WorkKind::Resident)
                    .await,
            ),
        ))
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
        self.capacity.available()
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
        request: HostCompilerRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<HostAdmissionPermit, HostAdmissionError>> + Send + 'a>>
    {
        let class =
            if request.host_admission_class() == Some(crate::core::lint_priority::HOST_CLASS) {
                AdmissionClass::Lint
            } else {
                AdmissionClass::Ordinary
            };
        let exclusive = request.is_exclusive();
        Box::pin(async move {
            Ok(HostAdmissionPermit::new(
                self.acquire_compiler_for(class, exclusive).await,
            ))
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lint_overtakes_queued_ordinary_work_without_preempting_a_running_compile() {
        use std::task::{Context, Poll, Waker};

        let admission = ResidentCompileAdmission::new(1);
        let running = admission.acquire_compiler().await;
        let mut ordinary = Box::pin(admission.acquire_compiler());
        let mut lint = Box::pin(admission.acquire_compiler_for(AdmissionClass::Lint, false));
        let mut context = Context::from_waker(Waker::noop());
        assert!(ordinary.as_mut().poll(&mut context).is_pending());
        assert!(lint.as_mut().poll(&mut context).is_pending());

        drop(running);
        assert!(ordinary.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(lint_permit) = lint.as_mut().poll(&mut context) else {
            panic!("lint should receive the first available slot");
        };
        drop(lint_permit);
        assert!(matches!(
            ordinary.as_mut().poll(&mut context),
            Poll::Ready(_)
        ));
    }

    #[tokio::test]
    async fn dropping_queued_lint_returns_the_next_slot_to_ordinary_work() {
        use std::task::{Context, Poll, Waker};

        let admission = ResidentCompileAdmission::new(1);
        let running = admission.acquire_compiler().await;
        let mut lint = Box::pin(admission.acquire_compiler_for(AdmissionClass::Lint, false));
        let mut ordinary = Box::pin(admission.acquire_compiler());
        let mut context = Context::from_waker(Waker::noop());
        assert!(lint.as_mut().poll(&mut context).is_pending());
        assert!(ordinary.as_mut().poll(&mut context).is_pending());

        drop(running);
        drop(lint);
        assert!(matches!(
            ordinary.as_mut().poll(&mut context),
            Poll::Ready(_)
        ));
    }

    #[tokio::test]
    async fn no_reservation_allows_full_compiler_concurrency() {
        let admission = ResidentCompileAdmission::new(4);
        let mut compilers = Vec::new();
        for expected_available in (0..4).rev() {
            compilers.push(admission.acquire_compiler().await);
            assert_eq!(admission.available_permits(), expected_available);
        }
        assert_eq!(compilers.len(), 4);
    }

    #[tokio::test]
    async fn resident_reservation_reduces_and_release_restores_compiler_capacity() {
        let admission = ResidentCompileAdmission::new(5);
        let resident = admission
            .acquire_resident(2, false)
            .await
            .expect("resident reservation");
        assert_eq!(admission.available_permits(), 3);

        let mut compilers = Vec::new();
        for expected_available in (0..3).rev() {
            compilers.push(admission.acquire_compiler().await);
            assert_eq!(admission.available_permits(), expected_available);
        }

        drop(compilers);
        assert_eq!(admission.available_permits(), 3);
        drop(resident);
        assert_eq!(admission.available_permits(), 5);
    }

    #[tokio::test]
    async fn competing_reservations_leave_a_slot_for_nested_compiler_work() {
        use std::task::{Context, Poll, Waker};

        let admission = ResidentCompileAdmission::new(2);
        let external_lint = admission
            .acquire_resident(1, true)
            .await
            .expect("lint lease");
        let mut another_resident = Box::pin(admission.acquire_resident(1, true));
        let mut nested_compile =
            Box::pin(admission.acquire_compiler_for(AdmissionClass::Lint, false));
        let mut context = Context::from_waker(Waker::noop());

        assert!(another_resident.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(nested) = nested_compile.as_mut().poll(&mut context) else {
            panic!("a resident lease must leave a compiler slot available");
        };
        drop(nested);
        assert!(another_resident.as_mut().poll(&mut context).is_pending());
        drop(external_lint);
        assert!(matches!(
            another_resident.as_mut().poll(&mut context),
            Poll::Ready(Ok(_))
        ));
    }

    #[tokio::test]
    async fn exclusive_lint_drains_compilers_without_waiting_for_its_resident_parent() {
        use std::task::{Context, Poll, Waker};

        let admission = ResidentCompileAdmission::new(3);
        let parent = admission
            .acquire_resident(1, true)
            .await
            .expect("lint parent");
        let running = admission.acquire_compiler().await;
        let mut exclusive = Box::pin(admission.acquire_compiler_for(AdmissionClass::Lint, true));
        let mut ordinary = Box::pin(admission.acquire_compiler());
        let mut context = Context::from_waker(Waker::noop());

        assert!(exclusive.as_mut().poll(&mut context).is_pending());
        assert!(ordinary.as_mut().poll(&mut context).is_pending());
        drop(running);
        let Poll::Ready(permit) = exclusive.as_mut().poll(&mut context) else {
            panic!("the resident lease must not block its exclusive child");
        };
        drop(parent);
        assert!(ordinary.as_mut().poll(&mut context).is_pending());
        drop(permit);
        assert!(ordinary.as_mut().poll(&mut context).is_ready());
    }

    #[tokio::test]
    async fn resident_reservation_rejects_zero_and_the_last_compiler_slot() {
        let admission = ResidentCompileAdmission::new(4);
        assert!(matches!(
            admission.acquire_resident(0, false).await,
            Err(ResidentCapacityError::Zero)
        ));
        assert!(matches!(
            admission.acquire_resident(4, false).await,
            Err(ResidentCapacityError::ExhaustsCompilerCapacity { .. })
        ));
        assert!(matches!(
            admission.acquire_resident(5, false).await,
            Err(ResidentCapacityError::ExhaustsCompilerCapacity { .. })
        ));
        assert_eq!(admission.available_permits(), 4);
    }
}
