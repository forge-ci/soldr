//! Linux login autostart: a systemd user unit under
//! `$XDG_CONFIG_HOME/systemd/user/`.
//!
//! Registering writes the unit and runs `systemctl --user enable`. If
//! `systemctl` is missing or fails -- a non-systemd Linux, or a session with no
//! sd-bus -- the unit is still written and its path still returned, with a
//! warning. Half a registration the operator can finish by hand beats none at
//! all, and the file on disk is the half that is hard to reproduce.

use std::path::PathBuf;
use std::process::Command;

use crate::platform::autostart::{shell_quote_single, AutostartError, AutostartProgram};

/// Render the unit text without touching the filesystem.
pub fn render_registration(program: &AutostartProgram<'_>) -> String {
    let binary = shell_quote_single(&program.program.to_string_lossy());
    let description = program.description;
    let start = program.start_argument;
    let stop = program.stop_argument;
    format!(
        "[Unit]\n\
         Description={description}\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={binary} {start}\n\
         ExecStop={binary} {stop}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    )
}

pub fn register(program: &AutostartProgram<'_>) -> Result<PathBuf, AutostartError> {
    let path = unit_path(program)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, render_registration(program))?;

    let unit = unit_filename(program);
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    match Command::new("systemctl")
        .args(["--user", "enable", &unit])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!("warning: systemctl --user enable {unit} returned non-zero ({status:?})");
        }
        Err(error) => {
            eprintln!("warning: systemctl --user enable {unit} failed to spawn: {error}");
        }
    }

    Ok(path)
}

pub fn unregister(program: &AutostartProgram<'_>) -> Result<(), AutostartError> {
    let path = unit_path(program)?;
    let unit = unit_filename(program);
    let _ = Command::new("systemctl")
        .args(["--user", "disable", &unit])
        .status();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    Ok(())
}

fn unit_filename(program: &AutostartProgram<'_>) -> String {
    format!("{}.service", program.identifier)
}

/// `$XDG_CONFIG_HOME/systemd/user/<identifier>.service`, falling back to
/// `~/.config/` when `XDG_CONFIG_HOME` is unset.
fn unit_path(program: &AutostartProgram<'_>) -> Result<PathBuf, AutostartError> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => {
            let home = std::env::var_os("HOME").ok_or_else(|| {
                AutostartError::Resolve("neither XDG_CONFIG_HOME nor HOME is set".into())
            })?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base
        .join("systemd")
        .join("user")
        .join(unit_filename(program)))
}
