//! Shared Windows Defender exclusion plumbing.
//!
//! Both the `soldr optimize` CLI flow (bin-only modules
//! `optimize`, `optimize_windows`, `optimize_detect`) and the
//! `soldr load --auto-defender-exclude` flow inside
//! `cache_lib::save::load` need to invoke
//! `Add-MpPreference` / `Remove-MpPreference` through PowerShell.
//!
//! The CLI surface used to own all of this directly, but `cache_lib`
//! lives in both the bin and lib trees while the optimize modules are
//! bin-only. To let `cache_lib` reuse the same primitives without
//! pulling the entire CLI optimize surface into `lib.rs`, the minimal
//! shared pieces (data types, admin detection, PowerShell discovery,
//! exclusion list query / apply, and the test seams) are extracted
//! here and re-declared in both `lib.rs` and `main.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Test seam: when set, the helper that would normally call
/// `Add-MpPreference` / `Remove-MpPreference` is replaced with a no-op
/// that records what it would have done into the file pointed to by
/// this env var (one TSV line per call: `op\tpath\n`).
pub const SOLDR_TEST_DEFENDER_LOG_ENV: &str = "SOLDR_TEST_DEFENDER_LOG";

/// Test seam: when set, treats the current process as if it is running
/// with administrator privileges regardless of the real token. Bypasses
/// the UAC self-relaunch path in tests.
pub const SOLDR_TEST_ASSUME_ADMIN_ENV: &str = "SOLDR_TEST_ASSUME_ADMIN";

/// Test seam: when set, returns the contents of this file as the
/// "current Defender exclusion list" (one path per line) instead of
/// running `Get-MpPreference`.
pub const SOLDR_TEST_DEFENDER_EXISTING_ENV: &str = "SOLDR_TEST_DEFENDER_EXISTING";

/// What the action layer plans to do (or did) for a single path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExclusionAction {
    Add,
    Remove,
}

/// Outcome of a single per-path action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    /// Action would run; `--dry-run` exited before invoking it.
    Planned,
    /// Action ran and reported success.
    Applied,
    /// Action ran but Defender reported the path was already excluded.
    AlreadyApplied,
    /// Action was skipped (e.g. undo on a path Defender no longer has).
    Skipped,
    /// Action ran but failed; see `detail`.
    Failed,
}

/// One row in the optimize plan / outcome.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathAction {
    pub path: String,
    pub action: ExclusionAction,
    pub scope: String,
    pub status: ActionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Check whether the current process holds an administrator token.
///
/// The platform crate owns the probe: on Windows a registry read of
/// `HKLM\SECURITY` (only admin processes can open this key); other
/// hosts answer `false` because the callers gate the Windows-only
/// optimize path. The test seam stays here, in the caller.
pub fn is_admin() -> bool {
    if std::env::var_os(SOLDR_TEST_ASSUME_ADMIN_ENV).is_some() {
        return true;
    }
    crate::platform::host::user::is_elevated()
}

/// Resolve a usable PowerShell binary by checking `pwsh` then
/// `powershell.exe` on `PATH`. Returns `None` when neither is present.
pub fn find_powershell() -> Option<PathBuf> {
    for candidate in ["pwsh", "powershell"] {
        if let Some(path) = which_on_path(candidate) {
            return Some(path);
        }
    }
    None
}

fn which_on_path(tool: &str) -> Option<PathBuf> {
    // The platform owns PATH/PATHEXT candidate generation; this
    // resolves the same way on every host.
    let path = std::env::var_os("PATH")?;
    crate::platform::executable::search::find_on_path(tool, &path)
}

/// Apply the given exclusion plan via PowerShell. Returns the per-path
/// outcomes for the JSON / human-readable output. The caller is
/// responsible for deciding whether elevation is needed before this is
/// invoked.
pub fn apply_exclusions(
    powershell: &Path,
    plan: &[PathAction],
    existing_exclusions: &[String],
) -> Vec<PathAction> {
    let test_log_path = std::env::var_os(SOLDR_TEST_DEFENDER_LOG_ENV).map(PathBuf::from);
    plan.iter()
        .map(|action| {
            let mut out = action.clone();
            let already_excluded = exclusion_list_contains(existing_exclusions, &action.path);
            match action.action {
                ExclusionAction::Add => {
                    if already_excluded {
                        out.status = ActionStatus::AlreadyApplied;
                        out.detail = Some("already on Defender exclusion list".into());
                        return out;
                    }
                    match run_defender_command(
                        powershell,
                        "Add-MpPreference",
                        &action.path,
                        test_log_path.as_deref(),
                    ) {
                        Ok(()) => {
                            out.status = ActionStatus::Applied;
                        }
                        Err(err) => {
                            out.status = ActionStatus::Failed;
                            out.detail = Some(err);
                        }
                    }
                }
                ExclusionAction::Remove => {
                    if !already_excluded {
                        out.status = ActionStatus::Skipped;
                        out.detail = Some("not present in Defender exclusion list".into());
                        return out;
                    }
                    match run_defender_command(
                        powershell,
                        "Remove-MpPreference",
                        &action.path,
                        test_log_path.as_deref(),
                    ) {
                        Ok(()) => {
                            out.status = ActionStatus::Applied;
                        }
                        Err(err) => {
                            out.status = ActionStatus::Failed;
                            out.detail = Some(err);
                        }
                    }
                }
            }
            out
        })
        .collect()
}

fn exclusion_list_contains(list: &[String], path: &str) -> bool {
    let needle = normalize_path(path);
    list.iter().any(|p| normalize_path(p) == needle)
}

fn normalize_path(path: &str) -> String {
    path.replace('/', "\\").to_ascii_lowercase()
}

fn run_defender_command(
    powershell: &Path,
    cmdlet: &str,
    path: &str,
    test_log: Option<&Path>,
) -> Result<(), String> {
    if let Some(log) = test_log {
        let line = format!("{cmdlet}\t{path}\n");
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| format!("failed to open test log: {e}"))?
            .write_all_str(&line)?;
        return Ok(());
    }
    let escaped = path.replace('\'', "''");
    let script = format!("{cmdlet} -ExclusionPath '{escaped}'");
    let output = crate::platform::wait::output_for(
        Command::new(powershell).args(["-NoProfile", "-Command", &script]),
        crate::platform::wait::PROBE,
    )
    .map_err(|e| format!("failed to invoke PowerShell: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "{cmdlet} exited with {}: {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        ))
    }
}

trait WriteAllStr {
    fn write_all_str(&mut self, s: &str) -> Result<(), String>;
}

impl WriteAllStr for std::fs::File {
    fn write_all_str(&mut self, s: &str) -> Result<(), String> {
        use std::io::Write;
        self.write_all(s.as_bytes())
            .map_err(|e| format!("failed to write defender test log: {e}"))
    }
}

/// Read the current Defender exclusion list. Requires admin in production;
/// tests can shim the result via `SOLDR_TEST_DEFENDER_EXISTING`.
pub fn current_exclusion_list(powershell: &Path) -> Vec<String> {
    if let Some(path) = std::env::var_os(SOLDR_TEST_DEFENDER_EXISTING_ENV) {
        return std::fs::read_to_string(&path)
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();
    }
    let output = crate::platform::wait::output_for(
        Command::new(powershell).args([
            "-NoProfile",
            "-Command",
            "(Get-MpPreference).ExclusionPath | ForEach-Object { $_ }",
        ]),
        crate::platform::wait::PROBE,
    );
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusion_list_contains_normalizes_separators_and_case() {
        let list = vec!["C:\\Users\\You\\.soldr\\Cache".to_string()];
        assert!(exclusion_list_contains(&list, "c:/users/you/.soldr/cache"));
    }
}
