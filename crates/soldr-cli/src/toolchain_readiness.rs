//! soldr#2618 — explicit pinned-toolchain provisioning for the blessed
//! build path.
//!
//! Nothing on the blessed `soldr build --target <T>` path installed the
//! pinned toolchain explicitly: `bootstrap_rustup` runs rustup-init with
//! `--default-toolchain none`, so on a fresh root the toolchain was
//! installed implicitly by whichever child process first invoked a rustup
//! proxy — usually the soldr#1543 dependency prefetch, spawned
//! concurrently with SDK preparation. `rustup target add` (which does not
//! auto-install) then raced that in-flight install and failed with
//! `Missing manifest in toolchain`, soldr exited, and the prefetch child
//! was reaped mid-extraction via `kill_on_drop` — leaving a toolchain
//! directory with some component manifests but no rustc and no channel
//! manifest. rustup proxies treat directory-exists as installed, so every
//! later run kept failing the same way: a permanent wedge.
//!
//! This module makes the install explicit and sequential
//! ([`ensure_pinned_toolchain_installed`]), and recovers a wedged partial
//! state only in Soldr-managed private homes. Caller-selected shared homes
//! fail closed with explicit recovery guidance instead of automatic removal.
//! The probe, the install, and
//! [`crate::prepare_cmd::rustup_add_target`] all force the same
//! `RUSTUP_HOME` (caller env if set, soldr-managed otherwise), so the
//! toolchain the probe inspects is the toolchain the install writes and
//! the target add mutates.
//!
//! ## Shared filesystem contract (soldr#2977)
//!
//! All callers classify a selected directory with the same evidence: `Missing`
//! means the directory is absent; `Ready` requires
//! `lib/rustlib/multirust-channel-manifest.toml`, native `bin/rustc` and, for a
//! directory named for this host, its `libcore`/`libstd` files (soldr#3376); and
//! `Partial` means the directory exists but either or both files are absent.
//! Cargo's preparation memo additionally requires and fingerprints its
//! `lib/rustlib/components` manifest. A caller-selected `RUSTUP_HOME` is never
//! destructively repaired automatically: the error names the exact directory
//! and explicit recovery command.

use crate::core::{run_installer_command, InstallerWatchdogConfig, SoldrError, SoldrPaths};
use std::path::{Path, PathBuf};

pub(crate) const TOOLCHAIN_CHANNEL_MANIFEST: &str = "lib/rustlib/multirust-channel-manifest.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MissingToolchainEvidence {
    pub(crate) channel_manifest: bool,
    pub(crate) native_rustc: bool,
    /// The host `libcore`/`libstd` files are absent while the directory is
    /// named for this host (soldr#3376): `components` still claims `rust-std`,
    /// and rustup reports "up to date", but a build fails with `E0463`.
    pub(crate) host_std: bool,
}

impl MissingToolchainEvidence {
    pub(crate) fn paths(self) -> Vec<&'static str> {
        let mut paths = Vec::new();
        if self.channel_manifest {
            paths.push(TOOLCHAIN_CHANNEL_MANIFEST);
        }
        if self.native_rustc {
            paths.push("bin/rustc");
        }
        if self.host_std {
            paths.push("lib/rustlib/<host>/lib/{libcore,libstd}-*.rlib");
        }
        paths
    }
}

/// Where the pinned toolchain stands in the effective `RUSTUP_HOME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolchainReadiness {
    /// Both authoritative base evidence files are present and reusable.
    Ready,
    /// No toolchain directory at all — plain install.
    Missing,
    /// Toolchain directory exists but lacks one or both base evidence files.
    /// It must never be reused; caller-owned state gets recovery guidance,
    /// while Soldr-managed private state may be repaired.
    Partial(MissingToolchainEvidence),
}

/// Directory name rustup gives a toolchain: bare channels
/// (`1.95.0`, `stable`, `nightly-2026-01-01`) get `-<host>` appended;
/// an already host-qualified channel maps to itself.
pub(crate) fn toolchain_dir_name(channel: &str, host: &str) -> String {
    if host.is_empty() || channel.ends_with(host) {
        channel.to_string()
    } else {
        format!("{channel}-{host}")
    }
}

pub(crate) fn classify(
    dir_exists: bool,
    channel_manifest_exists: bool,
    rustc_exists: bool,
    host_std_exists: bool,
) -> ToolchainReadiness {
    if !dir_exists {
        return ToolchainReadiness::Missing;
    }
    let missing = MissingToolchainEvidence {
        channel_manifest: !channel_manifest_exists,
        native_rustc: !rustc_exists,
        host_std: !host_std_exists,
    };
    if missing.channel_manifest || missing.native_rustc || missing.host_std {
        ToolchainReadiness::Partial(missing)
    } else {
        ToolchainReadiness::Ready
    }
}

fn rustc_filename() -> &'static str {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        "rustc.exe"
    } else {
        "rustc"
    }
}

pub(crate) fn native_rustc_path(toolchain_dir: &Path) -> PathBuf {
    toolchain_dir.join("bin").join(rustc_filename())
}

/// Filesystem-only classification of one already-selected toolchain
/// directory. Selection policy belongs to the caller; readiness does not.
pub(crate) fn classify_toolchain_dir(toolchain_dir: &Path) -> ToolchainReadiness {
    let channel_manifest = toolchain_dir.join(TOOLCHAIN_CHANNEL_MANIFEST);
    let rustc = native_rustc_path(toolchain_dir);
    // A directory named for another host (or with no host suffix) is not judged
    // against this host's standard library.
    let host_std_exists = crate::toolchain_std_libs::host_of_toolchain_dir(
        toolchain_dir,
        crate::pyo3_detect::host_triple(),
    )
    .is_none_or(|host| {
        crate::toolchain_std_libs::missing_std_triples(toolchain_dir, Some(&host), &[]).is_empty()
    });
    classify(
        toolchain_dir.is_dir(),
        channel_manifest.is_file(),
        rustc.is_file(),
        host_std_exists,
    )
}

/// Filesystem-only probe against an explicit home — no process spawns, so
/// it can never itself trigger a rustup proxy auto-install.
pub(crate) fn probe_toolchain_state(
    rustup_home: &Path,
    channel: &str,
    host: &str,
) -> ToolchainReadiness {
    let dir = rustup_home
        .join("toolchains")
        .join(toolchain_dir_name(channel, host));
    classify_toolchain_dir(&dir)
}

/// The `RUSTUP_HOME` that [`crate::prepare_cmd::rustup_add_target`] forces
/// on its child: the caller's env value when set, the soldr-managed home
/// otherwise.
fn effective_rustup_home() -> Result<PathBuf, SoldrError> {
    if let Some(home) = std::env::var_os(crate::core::RUSTUP_HOME_ENV_VAR) {
        if !home.is_empty() {
            return Ok(PathBuf::from(home));
        }
    }
    let paths = SoldrPaths::new()?;
    Ok(crate::fetch::managed_rustup_home(&paths))
}

fn pinned_channel() -> Result<Option<String>, SoldrError> {
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    // soldr#3633: nearest pin in cwd or ancestors, like cargo/rustup.
    Ok(crate::core::read_rust_toolchain_manifest_from_ancestors(&workspace_root)?.channel)
}

/// Ensure the pinned toolchain (from `rust-toolchain.toml`) is fully
/// installed in the effective `RUSTUP_HOME` before any rustup subcommand
/// that requires it runs. No-op when no channel is pinned or the
/// toolchain is already usable; installs sequentially otherwise; heals a
/// partial toolchain by uninstalling the corrupt directory first.
pub(crate) fn ensure_pinned_toolchain_installed() -> Result<(), SoldrError> {
    let Some(channel) = pinned_channel()? else {
        return Ok(());
    };
    let rustup_home = effective_rustup_home()?;
    match probe_toolchain_state(&rustup_home, &channel, crate::pyo3_detect::host_triple()) {
        ToolchainReadiness::Ready => Ok(()),
        ToolchainReadiness::Missing => {
            if !crate::core::quiet::diagnostics_suppressed() {
                eprintln!(
                    "soldr: pinned toolchain {channel} is not installed; \
                     installing it before continuing (soldr#2618)"
                );
            }
            run_pinned_rustup(&["toolchain", "install", &channel], "toolchain-install")
        }
        ToolchainReadiness::Partial(missing) => {
            if std::env::var_os(crate::core::RUSTUP_HOME_ENV_VAR)
                .is_some_and(|home| !home.is_empty())
            {
                let directory = rustup_home.join("toolchains").join(toolchain_dir_name(
                    &channel,
                    crate::pyo3_detect::host_triple(),
                ));
                return Err(shared_home_partial_toolchain_error(
                    &channel, &directory, missing,
                ));
            }
            if !crate::core::quiet::diagnostics_suppressed() {
                eprintln!(
                    "soldr: pinned toolchain {channel} is partially installed \
                     (no rustc — an earlier install was interrupted); \
                     reinstalling it (soldr#2618); missing {}",
                    missing.paths().join(", ")
                );
            }
            // Best-effort cleanup: the partial directory is unusable, and
            // leaving it makes rustup skip the reinstall.
            let _ = run_pinned_rustup(&["toolchain", "uninstall", &channel], "toolchain-repair");
            run_pinned_rustup(&["toolchain", "install", &channel], "toolchain-install")
        }
    }
}

/// Whether a toolchain `rustup toolchain list` reports as installed is really
/// a partial tree that must be reinstalled before components can be added.
///
/// `rustup toolchain list` reports any directory under `toolchains/`, including
/// one left behind by an interrupted `rustup toolchain install`. That tree has no
/// channel manifest, so a later `rustup component add` fails with "missing
/// manifest in toolchain" instead of repairing it. Only rustup's own copies
/// (real directories) are checked: a `rustup toolchain link` entry is a symlink
/// and never has a channel manifest. A missing host std alone is left to
/// `toolchain_std_libs::repair_missing_std_libs`.
///
/// Caller-selected `RUSTUP_HOME` state is never removed automatically; that case
/// is an error naming the directory and the recovery commands.
pub(crate) fn listed_toolchain_needs_reinstall(
    rustup_home: &Path,
    toolchain_name: &str,
    caller_selected_home: bool,
) -> Result<bool, SoldrError> {
    let directory = rustup_home.join("toolchains").join(toolchain_name);
    let is_real_dir = std::fs::symlink_metadata(&directory).is_ok_and(|meta| meta.is_dir());
    if !is_real_dir {
        return Ok(false);
    }
    let missing = MissingToolchainEvidence {
        channel_manifest: !directory.join(TOOLCHAIN_CHANNEL_MANIFEST).is_file(),
        native_rustc: !native_rustc_path(&directory).is_file(),
        host_std: false,
    };
    if !missing.channel_manifest && !missing.native_rustc {
        return Ok(false);
    }
    if caller_selected_home {
        return Err(shared_home_partial_toolchain_error(
            toolchain_name,
            &directory,
            missing,
        ));
    }
    Ok(true)
}

fn shared_home_partial_toolchain_error(
    channel: &str,
    directory: &Path,
    missing: MissingToolchainEvidence,
) -> SoldrError {
    let manager = ["rust", "up"].concat();
    SoldrError::Other(format!(
        "Rust toolchain {channel} is partial at {}: missing {}. This is caller-selected \
         RUSTUP_HOME state, so Soldr will not uninstall or reinstall it automatically. \
         Run `soldr {manager} toolchain uninstall {channel}` then `soldr toolchain install`, \
         or repair that exact directory with your toolchain manager before retrying.",
        directory.display(),
        missing.paths().join(", "),
    ))
}

/// Run a rustup subcommand under the same forced homes as
/// [`crate::prepare_cmd::rustup_add_target`], so probe, install, and
/// target add all act on one toolchain store.
fn run_pinned_rustup(args: &[&str], kind: &str) -> Result<(), SoldrError> {
    if args.first() == Some(&"toolchain") && args.get(1) == Some(&"install") {
        // soldr#3195: an install under test would download a toolchain.
        crate::core::forbid_toolchain_install_tripwire(&format!("rustup {}", args.join(" ")))?;
    }
    let paths = SoldrPaths::new()?;
    let rustup = crate::binaries::rustup_binary();
    let mut command = std::process::Command::new(rustup);
    let mut full_args: Vec<&str> = args.to_vec();
    if args.first() == Some(&"toolchain") && args.get(1) == Some(&"install") {
        full_args.extend(["--profile", "minimal", "--no-self-update"]);
    }
    command.args(&full_args);
    command.env(
        crate::core::CARGO_HOME_ENV_VAR,
        std::env::var_os(crate::core::CARGO_HOME_ENV_VAR)
            .unwrap_or_else(|| crate::fetch::managed_cargo_home(&paths).into_os_string()),
    );
    command.env(
        crate::core::RUSTUP_HOME_ENV_VAR,
        std::env::var_os(crate::core::RUSTUP_HOME_ENV_VAR)
            .unwrap_or_else(|| crate::fetch::managed_rustup_home(&paths).into_os_string()),
    );
    crate::core::suppress_windows_console_window(&mut command);
    let context = format!("rustup {}", full_args.join(" "));
    let status = run_installer_command(
        &mut command,
        &context,
        kind,
        InstallerWatchdogConfig::from_env(crate::toolchain::TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR),
    )?;
    if !status.success() {
        return Err(SoldrError::Other(format!("{context} exited with {status}")));
    }
    Ok(())
}

#[cfg(test)]
#[path = "toolchain_readiness_tests.rs"]
mod tests;
