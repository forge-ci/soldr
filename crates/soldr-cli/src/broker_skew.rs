//! Broker/front-door identity skew bookkeeping (soldr#3985): the porcelain
//! `broker_skew` record and the once-per-generation notice marker.

use crate::broker_spawn::BROKER_REMOVE_COMMAND;

/// `soldr-0.9.44-feecbd53...` -> `0.9.44-feecbd53`: the version plus enough of
/// the image digest to tell two same-version images apart.
pub(crate) fn short_broker_identity(instance: &str) -> String {
    let body = instance.strip_prefix("soldr-").unwrap_or(instance);
    match body.rsplit_once('-') {
        Some((version, digest)) => {
            let digest: String = digest.chars().take(8).collect();
            format!("{version}-{digest}")
        }
        None => body.to_string(),
    }
}

/// The broker/front-door identity skew observed by this process, reported in
/// the porcelain JSON as `broker_skew` instead of on stderr (soldr#3985).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct BrokerSkew {
    pub(crate) running_broker: String,
    pub(crate) this_soldr: String,
    pub(crate) remedy: &'static str,
}

pub(crate) static OBSERVED_BROKER_SKEW: crate::platform::sync::Mutex<Option<BrokerSkew>> =
    crate::platform::sync::Mutex::new(None);

/// The skew recorded by this process's broker readiness probe, if any.
pub(crate) fn observed_broker_skew() -> Option<BrokerSkew> {
    OBSERVED_BROKER_SKEW.lock().clone()
}

/// File beside the staged broker image that records the broker identity the
/// notice was last printed for, so it prints once per broker generation.
pub(crate) const BROKER_SKEW_MARKER: &str = "broker-skew-warned";

pub(crate) fn broker_skew_marker_path(
    broker_executable: &std::path::Path,
) -> Option<std::path::PathBuf> {
    broker_executable
        .parent()
        .map(|dir| dir.join(BROKER_SKEW_MARKER))
}

/// True when the notice for `observed` vs `expected` was already printed.
pub(crate) fn skew_already_warned(
    marker: &std::path::Path,
    observed: &str,
    expected: &str,
) -> bool {
    std::fs::read_to_string(marker)
        .map(|content| content == skew_marker_content(observed, expected))
        .unwrap_or(false)
}

pub(crate) fn skew_marker_content(observed: &str, expected: &str) -> String {
    format!("{observed}\n{expected}\n")
}

/// Render the default mismatch diagnostic: one line (soldr#3985). soldr#2549
/// keeps every ordinary identity mismatch actionable and free of lifecycle
/// action; soldr#2920 is the separately classified, evidence-bound `=0.9.0`
/// exception.
pub(crate) fn broker_image_mismatch_warning(observed: &str, expected: &str) -> String {
    format!(
        "soldr: broker is {} (this is {}); work continues; retire with `{BROKER_REMOVE_COMMAND}`",
        short_broker_identity(observed),
        short_broker_identity(expected),
    )
}

pub(crate) fn warn_on_broker_image_mismatch(observed: &str, broker_executable: &std::path::Path) {
    // A local identity that cannot be computed is not evidence of a mismatch.
    // Stay quiet rather than warning about a comparison that never happened.
    let Ok(expected) = crate::broker_server::broker_image_instance_id() else {
        return;
    };
    if observed == expected {
        return;
    }
    let skew = BrokerSkew {
        running_broker: observed.to_string(),
        this_soldr: expected.clone(),
        remedy: BROKER_REMOVE_COMMAND,
    };
    *OBSERVED_BROKER_SKEW.lock() = Some(skew);
    // Porcelain carries the skew in its JSON summary; stderr stays clean.
    if crate::core::flag(crate::cargo_front_door::porcelain::PORCELAIN_ENV_VAR) {
        return;
    }
    let marker = broker_skew_marker_path(broker_executable);
    if let Some(marker) = &marker {
        if skew_already_warned(marker, observed, &expected) {
            return;
        }
    }
    eprintln!("{}", broker_image_mismatch_warning(observed, &expected));
    if let Some(marker) = marker {
        let _ = std::fs::write(marker, skew_marker_content(observed, &expected));
    }
}
