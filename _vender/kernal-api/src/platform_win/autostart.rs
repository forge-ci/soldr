//! Windows login autostart: a Task Scheduler ONLOGON task, created through
//! the `schtasks.exe` CLI.
//!
//! The Task Scheduler XML schema is verbose and changes shape between Windows
//! versions, while `schtasks /Create` has been stable since Windows 7. So the
//! CLI is the interface, and `render_registration` returns the exact command
//! line [`register`] would run -- the same argv, so a test that reads the
//! rendering is reading the real thing.

use std::path::PathBuf;
use std::process::Command;

use crate::platform::autostart::{AutostartError, AutostartProgram};

/// Render the `schtasks /Create` invocation without running it.
pub fn render_registration(program: &AutostartProgram<'_>) -> String {
    let binary = program.program.to_string_lossy();
    let run = cmd_quote(&format!("{binary} {}", program.start_argument));
    format!(
        "schtasks /Create /SC ONLOGON /TN {task} /TR {run} /RL HIGHEST /F",
        task = cmd_quote(program.identifier),
    )
}

pub fn register(program: &AutostartProgram<'_>) -> Result<PathBuf, AutostartError> {
    let binary = program.program.to_string_lossy().into_owned();
    let run = format!("{binary} {}", program.start_argument);
    let status = Command::new("schtasks")
        .args([
            "/Create",
            "/SC",
            "ONLOGON",
            "/TN",
            program.identifier,
            "/TR",
            &run,
            "/RL",
            "HIGHEST",
            "/F",
        ])
        .status()
        .map_err(|error| AutostartError::InitSystem(format!("schtasks /Create failed: {error}")))?;
    if !status.success() {
        return Err(AutostartError::InitSystem(format!(
            "schtasks /Create exited non-zero ({status})"
        )));
    }
    Ok(task_location(program))
}

pub fn unregister(program: &AutostartProgram<'_>) -> Result<(), AutostartError> {
    let status = Command::new("schtasks")
        .args(["/Delete", "/TN", program.identifier, "/F"])
        .status()
        .map_err(|error| AutostartError::InitSystem(format!("schtasks /Delete failed: {error}")))?;
    if !status.success() {
        // A missing task is not a failure: the caller asked for it to not be
        // registered, and it is not.
        eprintln!("warning: schtasks /Delete returned non-zero ({status:?}) (already removed?)");
    }
    Ok(())
}

/// Where the registration lives, as far as an operator is concerned.
///
/// Task Scheduler keeps tasks in the registry, not on disk, so this is not a
/// real path -- it is the name to look for in `schtasks /Query`, in the shape
/// the caller's contract asks for.
fn task_location(program: &AutostartProgram<'_>) -> PathBuf {
    PathBuf::from(format!(r"\Task Scheduler\{}", program.identifier))
}

/// Wrap in CMD-style double quotes, doubling any embedded double quote, which
/// is what `schtasks /TR` expects when a path or argument contains spaces.
fn cmd_quote(value: &str) -> String {
    let escaped = value.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_quote_doubles_embedded_quotes() {
        assert_eq!(
            cmd_quote(r#"C:\path with "quotes"\runpm.exe"#),
            "\"C:\\path with \"\"quotes\"\"\\runpm.exe\""
        );
    }
}
