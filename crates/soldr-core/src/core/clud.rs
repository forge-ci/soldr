//! Detect a clud-managed agent session.
//!
//! clud exports `CLUD_SESSION_ID` into every agent shell it launches. Soldr
//! uses it to escalate configuration drift an agent can fix in-tree (for
//! example a Dylint pin with no pre-deployed driver) from a warning to an
//! error, so agents fix the pin instead of paying a silent slow path. Human
//! and CI invocations keep the warning.

/// The variable clud sets in every agent session it launches.
pub const CLUD_SESSION_ENV_VAR: &str = "CLUD_SESSION_ID";

/// Is this process running inside a clud agent session?
pub fn running_under_clud() -> bool {
    std::env::var_os(CLUD_SESSION_ENV_VAR).is_some_and(|value| !value.is_empty())
}
