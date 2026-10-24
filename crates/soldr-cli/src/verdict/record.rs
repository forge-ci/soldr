//! The verdict record (soldr#3894 §4): the small artifact a passing check
//! leaves behind. It carries its own key and the identity it was produced
//! under, so a hit can be re-verified and explained, and the check's output
//! so a skipped run prints what the real run printed.

use serde::{Deserialize, Serialize};

/// Record schema version. Bumping it invalidates every stored record.
pub const SCHEMA: u32 = 1;

/// Recorded output beyond this is not kept; the verdict is still recorded
/// but replay says the output was too large. A passing check under
/// `-D warnings` prints nothing, so this only bounds pathological cases.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub v: u32,
    /// The full key id (`soldr-verdict-v1-<tool>-<scope>-<digest>`).
    pub key: String,
    pub tool: String,
    /// Normalized cargo argv the verdict covers.
    pub argv: Vec<String>,
    /// Repo-relative working directory of the invocation.
    pub cwd: String,
    /// Toolchain identity hashed into the key, kept for `explain`.
    pub toolchain: String,
    /// soldr version that wrote the record (informational).
    pub soldr: String,
    /// Always 0: only passes are recorded.
    pub exit: i32,
    /// Unix seconds.
    pub created: u64,
    /// Captured diagnostic stream of the passing run, when soldr captured
    /// it. `None` with `output_captured == false` means the run's stderr
    /// went straight to a terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default)]
    pub output_captured: bool,
}

impl Record {
    /// Attach captured output, respecting [`MAX_OUTPUT_BYTES`].
    pub fn with_output(mut self, output: Option<&str>) -> Self {
        match output {
            Some(text) if text.len() <= MAX_OUTPUT_BYTES => {
                self.output = (!text.is_empty()).then(|| text.to_string());
                self.output_captured = true;
            }
            Some(_) => {
                self.output = None;
                self.output_captured = false;
            }
            None => {
                self.output = None;
                self.output_captured = false;
            }
        }
        self
    }

    /// Parse and validate a stored record against the key it was looked up
    /// under. A record from another schema, for another key, or claiming a
    /// failure is rejected.
    pub fn parse_for(text: &str, key: &str) -> Option<Self> {
        let record: Self = serde_json::from_str(text).ok()?;
        (record.v == SCHEMA && record.key == key && record.exit == 0).then_some(record)
    }
}
