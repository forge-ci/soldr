//! `soldr agent capabilities [--json]` — the capability/version negotiation
//! document a consumer (clud's agent hook) reads before it routes a
//! validation call (soldr#3924).
//!
//! The bounded failure-first contract is opt-in and versioned. A consumer
//! must never infer support from a version string: a build may carry the
//! verb without the machine-facing mode, and a fork may carry the mode with
//! a different schema. The only honest answer is to ask the installed
//! executable what it can do, once, and fall back to the generic local
//! diagnostic path when the answer is missing, malformed, oversized or
//! unsupported.
//!
//! The document is exactly one JSON object on one line on stdout. `--json`
//! is the machine form; without it the same facts print as a short human
//! summary so the verb is usable interactively. Every key is additive: a
//! consumer that does not know a key ignores it, and a consumer that needs a
//! key it does not find must fall back rather than guess.

use serde::Serialize;

use crate::core::SoldrError;

/// Version of the capability document itself. A *breaking* change to the
/// document's shape bumps this; adding a key never does. A consumer that
/// cannot read this version must fall back.
pub const CAPABILITIES_SCHEMA_VERSION: u32 = 1;

const USAGE: &str = "usage: soldr agent capabilities [--json]";

/// What this build advertises to an agent-facing consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// The document's own version; see [`CAPABILITIES_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// `--porcelain` / `SOLDR_PORCELAIN=1` returns one JSON line plus a
    /// durable log, and on failure exposes the actionable diagnostic.
    pub failure_first: bool,
    /// A run's `log_path` outlives the process and can be read after the
    /// one-line result, so a consumer never has to keep the whole log in
    /// context.
    pub artifact_continuation: bool,
    /// The `--porcelain` JSON schema this build emits. A consumer that
    /// knows only an older schema can still read the document and decide.
    pub porcelain_schema_version: u32,
    /// The product name, so a consumer that probes a wrapper does not
    /// mistake it for soldr.
    pub product: &'static str,
    /// The build's own version, informational only — never a capability.
    pub version: &'static str,
}

impl Capabilities {
    /// What this build can do, read from the build itself so the document
    /// cannot drift from the code that implements the modes.
    pub fn current() -> Self {
        Self {
            schema_version: CAPABILITIES_SCHEMA_VERSION,
            failure_first: true,
            artifact_continuation: true,
            porcelain_schema_version: crate::cargo_front_door::porcelain::SCHEMA_VERSION,
            product: "soldr",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    /// The one-line JSON document a consumer parses.
    pub fn to_json_line(&self) -> Result<String, SoldrError> {
        serde_json::to_string(self)
            .map_err(|err| SoldrError::Other(format!("cannot serialize capabilities: {err}")))
    }

    /// The human summary; deliberately the same facts in the same order.
    pub fn to_human(&self) -> String {
        format!(
            "soldr {}\n  capability document: schema_version={}\n  failure_first: {}\n  artifact_continuation: {}\n  porcelain schema_version: {}\n",
            self.version,
            self.schema_version,
            self.failure_first,
            self.artifact_continuation,
            self.porcelain_schema_version,
        )
    }
}

/// Dispatch `soldr agent <verb>`. Unknown verbs print the usage and return 2
/// so a caller can tell "I asked for something this build does not have" from
/// "the probe failed".
pub fn run(args: &[String]) -> Result<i32, SoldrError> {
    let json = args.iter().any(|arg| arg == "--json");
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| *arg != "--json")
        .collect();
    match words.as_slice() {
        ["capabilities"] => {
            let capabilities = Capabilities::current();
            if json {
                println!("{}", capabilities.to_json_line()?);
            } else {
                print!("{}", capabilities.to_human());
            }
            Ok(0)
        }
        _ => {
            // A usage error is a diagnostic: mark the exit as spoken so the
            // guard does not append "this is a fault in soldr itself" to a
            // request for a verb this build does not have.
            crate::exit_guard::mark_spoke();
            eprintln!("{USAGE}");
            Ok(2)
        }
    }
}

#[cfg(test)]
#[path = "agent_cmd_tests.rs"]
mod tests;
