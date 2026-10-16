//! Broker route-retry ceilings (soldr#3800): the in-flight daemon launch
//! is re-polled quickly instead of after a random up-to-1 s back-off.

/// running-process `hello_router` refusal text for the single-flight case: another
/// requester already began launching this service's backend (`ErrorRateLimited`,
/// `retry_after` 1000). The reason text is the only discriminator the wire exposes;
/// the other rate-limited refusals (budget exhausted, storm trip) use other reasons.
pub(crate) const IN_FLIGHT_LAUNCH_REASON: &str = "backend spawn already in progress";

/// Re-poll ceiling for the in-flight case (soldr#3800). The launch finishes in a few
/// hundred ms, so the old uniform 5..1000 ms back-off was mostly idle time added to every
/// later first wrapper. `route_retry_delay` draws uniformly from 5..=ceiling (jitter kept).
pub(crate) const IN_FLIGHT_LAUNCH_RETRY_MAX_MS: u64 = 50;

/// Ceiling to feed `route_retry_delay`: short for an in-flight launch, the
/// server-provided hint for every other rate-limited refusal.
pub(crate) fn effective_retry_after_ms(reason: &str, retry_after_ms: u64) -> u64 {
    if reason.contains(IN_FLIGHT_LAUNCH_REASON) {
        retry_after_ms.min(IN_FLIGHT_LAUNCH_RETRY_MAX_MS)
    } else {
        retry_after_ms
    }
}
