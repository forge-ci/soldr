//! Broker route-acquisition deadlines and their `soldr doctor` surface.
//!
//! Split out of `broker_server.rs` (soldr#2493) to keep that file under the
//! workspace's 1,000-line production-source ceiling — this cluster has no
//! access to the broker's private connection-handling state, so it moves
//! cleanly.

use std::time::Duration;

/// A broker that has accepted the connection but has not answered yet is busy,
/// not dead (soldr#3449): on a loaded 4-vCPU Windows runner the first reply was
/// observed at 3.4 s. A dead broker is caught earlier, by a refused or closed
/// connection, so this only bounds a hung one -- and must not be shorter than the
/// silence budget that bounds a stalled one.
const DEFAULT_FIRST_RESPONSE_MS: u64 = 10_000;
const DEFAULT_PROGRESS_SILENCE_MS: u64 = 5_000;
const DEFAULT_ROUTE_CEILING_MS: u64 = 120_000;
const DEFAULT_BUSY_BUDGET_MS: u64 = 1_000;

// soldr#3449: a 3.4 s first reply on a loaded runner is a busy broker, so the
// first-response budget may not undercut the stalled-broker silence budget and
// must stay inside the route ceiling.
const _: () = assert!(DEFAULT_FIRST_RESPONSE_MS >= 4_000);
const _: () = assert!(DEFAULT_FIRST_RESPONSE_MS >= DEFAULT_PROGRESS_SILENCE_MS);
const _: () = assert!(DEFAULT_FIRST_RESPONSE_MS < DEFAULT_ROUTE_CEILING_MS);

#[derive(Clone, Copy, Debug)]
pub(crate) struct BrokerDeadlines {
    pub(crate) busy_budget: Duration,
    pub(crate) first_response: Duration,
    pub(crate) progress_silence: Duration,
    pub(crate) route_ceiling: Duration,
}

impl BrokerDeadlines {
    /// The built-in budgets, ignoring the environment. Tests use this so a CI
    /// override such as `SOLDR_BROKER_FIRST_RESPONSE_MS` cannot change what
    /// they prove about the defaults.
    #[cfg(test)]
    pub(crate) fn defaults() -> Self {
        Self {
            busy_budget: Duration::from_millis(DEFAULT_BUSY_BUDGET_MS),
            first_response: Duration::from_millis(DEFAULT_FIRST_RESPONSE_MS),
            progress_silence: Duration::from_millis(DEFAULT_PROGRESS_SILENCE_MS),
            route_ceiling: Duration::from_millis(DEFAULT_ROUTE_CEILING_MS),
        }
    }

    pub(crate) fn from_env() -> Self {
        Self {
            busy_budget: env_duration("SOLDR_BROKER_BUSY_BUDGET_MS", DEFAULT_BUSY_BUDGET_MS),
            first_response: env_duration(
                "SOLDR_BROKER_FIRST_RESPONSE_MS",
                DEFAULT_FIRST_RESPONSE_MS,
            ),
            progress_silence: env_duration(
                "SOLDR_BROKER_PROGRESS_SILENCE_MS",
                DEFAULT_PROGRESS_SILENCE_MS,
            ),
            route_ceiling: env_duration("SOLDR_ROUTE_ACQUIRE_CEILING_MS", DEFAULT_ROUTE_CEILING_MS),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct DoctorBrokerDeadline {
    pub(crate) name: &'static str,
    pub(crate) env_var: &'static str,
    pub(crate) default_ms: u64,
    pub(crate) effective_ms: u64,
    pub(crate) source: &'static str,
}

pub(crate) fn doctor_deadlines() -> Vec<DoctorBrokerDeadline> {
    let effective = BrokerDeadlines::from_env();
    [
        (
            "broker busy retry",
            "SOLDR_BROKER_BUSY_BUDGET_MS",
            DEFAULT_BUSY_BUDGET_MS,
            effective.busy_budget,
        ),
        (
            "broker first response",
            "SOLDR_BROKER_FIRST_RESPONSE_MS",
            DEFAULT_FIRST_RESPONSE_MS,
            effective.first_response,
        ),
        (
            "broker progress silence",
            "SOLDR_BROKER_PROGRESS_SILENCE_MS",
            DEFAULT_PROGRESS_SILENCE_MS,
            effective.progress_silence,
        ),
        (
            "broker route ceiling",
            "SOLDR_ROUTE_ACQUIRE_CEILING_MS",
            DEFAULT_ROUTE_CEILING_MS,
            effective.route_ceiling,
        ),
    ]
    .into_iter()
    .map(
        |(name, env_var, default_ms, duration)| DoctorBrokerDeadline {
            name,
            env_var,
            default_ms,
            effective_ms: duration.as_millis() as u64,
            source: match std::env::var(env_var) {
                Ok(value) if value.trim().parse::<u64>().is_ok_and(|value| value > 0) => "override",
                Ok(_) => "default (override ignored: expected positive milliseconds)",
                Err(_) => "default",
            },
        },
    )
    .collect()
}

/// The tuning variable for a deadline class named by a timeout error.
pub(crate) fn deadline_env_var(class: &str) -> &'static str {
    match class {
        "route acquisition ceiling" => "SOLDR_ROUTE_ACQUIRE_CEILING_MS",
        "first-response deadline" => "SOLDR_BROKER_FIRST_RESPONSE_MS",
        _ => "SOLDR_BROKER_PROGRESS_SILENCE_MS",
    }
}

pub(crate) fn print_doctor_deadlines() {
    println!("\nbroker route deadlines:");
    for row in doctor_deadlines() {
        println!(
            "  {:<24} {:>7} ms  [{} via {}]",
            row.name, row.effective_ms, row.source, row.env_var
        );
    }
}

fn env_duration(name: &str, default_ms: u64) -> Duration {
    Duration::from_millis(
        std::env::var(name)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(default_ms),
    )
}

pub(crate) fn handshake_context(
    request_id: u64,
    elapsed: Duration,
    last_progress: Option<&crate::broker_server::RouteProgress>,
    handoff: Option<(u64, &str, &str)>,
) -> String {
    let phase = handoff.map_or_else(
        || "phase=route-acquisition".into(),
        |(id, backend, version)| {
            format!("phase=handoff connection_id={id} backend={backend} daemon_version={version}")
        },
    );
    let progress = last_progress.map_or_else(
        || "no progress received".into(),
        |last| {
            format!(
                "stage={} attempt={} broker_elapsed_ms={} result={}",
                last.stage, last.attempt, last.elapsed_ms, last.latest_result
            )
        },
    );
    format!(
        "request_id={request_id} {phase} elapsed_ms={} last_progress=[{progress}]",
        elapsed.as_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_deadline_class_names_its_tuning_variable() {
        assert_eq!(
            deadline_env_var("first-response deadline"),
            "SOLDR_BROKER_FIRST_RESPONSE_MS"
        );
        assert_eq!(
            deadline_env_var("route acquisition ceiling"),
            "SOLDR_ROUTE_ACQUIRE_CEILING_MS"
        );
        assert_eq!(
            deadline_env_var("progress-silence deadline"),
            "SOLDR_BROKER_PROGRESS_SILENCE_MS"
        );
    }
}
