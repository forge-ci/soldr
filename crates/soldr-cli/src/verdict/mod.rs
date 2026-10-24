//! Verdict ledger (soldr#3894): skip a deterministic check whose exact
//! inputs already passed.
//!
//! The ledger is one core plus per-tool adapters:
//!
//! - [`profiles`] — the adapters: data rows saying which invocations are a
//!   check and what, beyond the Rust sources, can change their result.
//! - [`key`] — the core's input digest, shared by every adapter.
//! - [`context`] — one checkout's snapshot, configuration and adapters.
//! - [`record`] — the small artifact a pass leaves behind.
//! - [`store`] — the local store, plus the spool and `seen.jsonl` a remote
//!   tier (setup-soldr) uses to publish and predict verdicts.
//! - [`gate`] — the cargo front-door hook: replay on a hit, record on a pass.
//! - [`cli`] — `soldr verdict ...`.
//!
//! `SOLDR_VERDICTS=off` disables the ledger; `read` replays existing
//! verdicts but records none.

pub mod cli;
pub mod context;
pub mod gate;
pub mod key;
pub mod profiles;
pub mod record;
pub mod store;

#[cfg(test)]
mod tests;

/// `on` (default), `off`, or `read`.
pub const MODE_ENV: &str = "SOLDR_VERDICTS";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    On,
    Off,
    Read,
}

impl Mode {
    pub fn from_env() -> Self {
        Self::parse(std::env::var(MODE_ENV).ok().as_deref())
    }

    pub fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if v == "read" || v == "readonly" || v == "read-only" => Self::Read,
            Some(v) if soldr_core::core::env_flag::is_off_value(&v) => Self::Off,
            _ => Self::On,
        }
    }
}

/// The directory whose `Cargo.toml` configures the ledger: the nearest
/// ancestor of `cwd` (up to `repo_root`) holding a `[workspace]`, else the
/// repo root.
pub fn workspace_root(cwd: &std::path::Path, repo_root: &std::path::Path) -> std::path::PathBuf {
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        let declares_workspace = std::fs::read_to_string(current.join("Cargo.toml"))
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .is_some_and(|table| table.contains_key("workspace"));
        if declares_workspace {
            return current.to_path_buf();
        }
        if current == repo_root {
            break;
        }
        dir = current.parent();
    }
    repo_root.to_path_buf()
}
