//! Detection of rustc's warning-only strip-subprocess failure.
//!
//! rustc intentionally leaves a failed `rust-objcopy` invocation as a warning,
//! even when the selected profile requests stripping.  Soldr owns the artifact
//! contract at the cargo boundary, so this narrow classifier promotes that one
//! compiler diagnostic into a build failure without treating look-alike user
//! warnings as fatal.

/// The exact rustc warning emitted when its requested stripping subprocess
/// fails.  The utility name is retained for an actionable Soldr error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StripFailure {
    pub(super) utility: String,
    /// rustc's warning plus immediately-associated notes, preserved so the
    /// loader diagnostic reaches the user even though Cargo itself succeeded.
    pub(super) diagnostic: String,
}

pub(super) struct StripOutcome(Option<StripFailure>);

impl StripOutcome {
    pub(super) fn from_cargo(status_success: bool, stderr: Option<&str>) -> Self {
        Self(
            status_success
                .then(|| stderr.and_then(detect_requested_strip_failure))
                .flatten(),
        )
    }

    pub(super) fn effective_exit_code(&self, status: &std::process::ExitStatus) -> i32 {
        if self.0.is_some() {
            1
        } else {
            status.code().unwrap_or_else(|| {
                let returncode = soldr_platform::process::test_child::returncode(status);
                if returncode < 0 {
                    returncode.saturating_sub(128)
                } else {
                    -1
                }
            })
        }
    }

    pub(super) fn permits_artifact_publication(&self) -> bool {
        self.0.is_none()
    }

    pub(super) fn into_result(self) -> Result<(), crate::core::SoldrError> {
        let Some(failure) = self.0 else {
            return Ok(());
        };
        Err(crate::core::SoldrError::Other(format!(
            "requested artifact stripping with `{}` failed; Soldr refuses to publish an unstripped artifact.\n{}\n\
             Fix the `{}` runtime (for a managed Rust toolchain, verify its library directory is available to the dynamic loader).",
            failure.utility, failure.diagnostic, failure.utility,
        )))
    }
}

pub(super) fn should_capture(
    build_like_cargo: bool,
    stderr_is_terminal: bool,
    zthreads_requested: bool,
) -> bool {
    build_like_cargo || !stderr_is_terminal || zthreads_requested
}

/// Append rendered rustc diagnostics from Cargo's JSON stream. Such warnings
/// exist in `compiler-message.message.rendered`, not Cargo's stderr.
pub(super) fn merge_cargo_json_diagnostics(stderr: &[u8], stdout: &[u8]) -> String {
    let mut diagnostics = String::from_utf8_lossy(stderr).into_owned();
    let rendered = String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|value| {
            value.get("reason").and_then(serde_json::Value::as_str) == Some("compiler-message")
        })
        .filter_map(|value| {
            value
                .pointer("/message/rendered")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !rendered.is_empty() {
        if !diagnostics.is_empty() {
            diagnostics.push('\n');
        }
        diagnostics.push_str(&rendered);
    }
    diagnostics
}

/// Return the first rustc strip-subprocess failure in Cargo stderr.
///
/// Match the complete stable warning shape instead of loose `strip`,
/// `objcopy`, or `libLLVM` keywords so unrelated compiler and build-script
/// warnings retain their normal warning-only behaviour.
pub(super) fn detect_requested_strip_failure(stderr: &str) -> Option<StripFailure> {
    const PREFIX: &str = "warning: stripping debug info with `";
    const SUFFIX: &str = "` failed:";

    let lines: Vec<_> = stderr.lines().collect();
    lines.iter().enumerate().find_map(|(index, line)| {
        let warning = line.trim_start();
        let rest = warning.strip_prefix(PREFIX)?;
        let (utility, failure) = rest.split_once(SUFFIX)?;
        if utility.is_empty() || failure.trim().is_empty() {
            return None;
        }
        Some(StripFailure {
            utility: utility.to_owned(),
            diagnostic: std::iter::once(warning)
                .chain(
                    lines[index + 1..]
                        .iter()
                        .map(|line| line.trim_start())
                        .take_while(|line| line.starts_with("= note:")),
                )
                .collect::<Vec<_>>()
                .join("\n"),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_rustcs_requested_strip_failure() {
        let stderr = "warning: stripping debug info with `rust-objcopy` failed: exit status: 127\n\
            = note: rust-objcopy: error while loading shared libraries: libLLVM.so: cannot open shared object file\n";

        assert_eq!(
            detect_requested_strip_failure(stderr),
            Some(StripFailure {
                utility: "rust-objcopy".to_owned(),
                diagnostic: "warning: stripping debug info with `rust-objcopy` failed: exit status: 127\n= note: rust-objcopy: error while loading shared libraries: libLLVM.so: cannot open shared object file"
                    .to_owned(),
            })
        );
    }

    #[test]
    fn ignores_lookalike_warnings() {
        for warning in [
            "warning: rust-objcopy failed in a build script",
            "warning: stripping debug info was requested",
            "warning: libLLVM could not be loaded",
            "warning: stripping debug info with `rust-objcopy` failed:",
        ] {
            assert_eq!(detect_requested_strip_failure(warning), None, "{warning}");
        }
    }

    #[test]
    fn finds_strip_failure_in_cargo_json_compiler_messages() {
        let stdout = br#"{"reason":"compiler-message","message":{"rendered":"warning: stripping debug info with `rust-objcopy` failed: exit status: 127\n= note: rust-objcopy: error while loading shared libraries: libLLVM.so\n"}}
{"reason":"compiler-artifact","filenames":[]}"#;

        let diagnostics = merge_cargo_json_diagnostics(b"", stdout);
        assert_eq!(
            detect_requested_strip_failure(&diagnostics).map(|failure| failure.utility),
            Some("rust-objcopy".to_owned()),
        );
    }
}
