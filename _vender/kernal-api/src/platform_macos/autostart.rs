//! macOS login autostart: a launchd user agent plist under
//! `~/Library/LaunchAgents/`.
//!
//! Registering writes the plist and runs `launchctl bootstrap gui/<uid>`.
//! `bootstrap`/`bootout` is launchd's per-domain model and has been the modern
//! interface since 10.10; `load -w` still works everywhere we support but is
//! deprecated, so it is kept only as the fallback when `bootstrap` is refused.

use std::path::PathBuf;
use std::process::Command;

use crate::platform::autostart::{xml_escape, AutostartError, AutostartProgram};

/// Render the plist text without touching the filesystem.
pub fn render_registration(program: &AutostartProgram<'_>) -> String {
    let binary = xml_escape(&program.program.to_string_lossy());
    let label = xml_escape(program.label);
    let start = xml_escape(program.start_argument);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>{start}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
"#,
    )
}

pub fn register(program: &AutostartProgram<'_>) -> Result<PathBuf, AutostartError> {
    let path = plist_path(program)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, render_registration(program))?;

    let domain = format!("gui/{}", current_uid()?);
    match Command::new("launchctl")
        .args(["bootstrap", &domain, &path.to_string_lossy()])
        .status()
    {
        Ok(status) if status.success() => {}
        _ => {
            // Legacy interface. A non-zero status is ignored so the written
            // path is still reported.
            let _ = Command::new("launchctl")
                .args(["load", "-w", &path.to_string_lossy()])
                .status();
        }
    }

    Ok(path)
}

pub fn unregister(program: &AutostartProgram<'_>) -> Result<(), AutostartError> {
    let path = plist_path(program)?;
    let target = format!("gui/{}/{}", current_uid()?, program.label);
    let modern = Command::new("launchctl").args(["bootout", &target]).status();
    if !matches!(modern, Ok(status) if status.success()) {
        let _ = Command::new("launchctl")
            .args(["unload", "-w", &path.to_string_lossy()])
            .status();
    }
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// `$HOME/Library/LaunchAgents/<label>.plist`.
fn plist_path(program: &AutostartProgram<'_>) -> Result<PathBuf, AutostartError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| AutostartError::Resolve("HOME is not set".into()))?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", program.label)))
}

/// The uid launchd wants for the `gui/<uid>` domain.
///
/// Read through `id -u` rather than `getuid`, which is what this has always
/// done: the launchd domain is a string built for a CLI either way, and the
/// value has to agree with what `launchctl` itself would compute.
fn current_uid() -> Result<u32, AutostartError> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|error| AutostartError::InitSystem(format!("id -u failed: {error}")))?;
    if !output.status.success() {
        return Err(AutostartError::InitSystem(format!(
            "id -u exited non-zero: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    text.parse::<u32>()
        .map_err(|error| AutostartError::InitSystem(format!("could not parse uid {text:?}: {error}")))
}
