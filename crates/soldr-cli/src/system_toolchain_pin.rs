//! A host toolchain that already satisfies an exact-version pin (soldr#3786).
//!
//! `rust-toolchain.toml` pins a version, not a source. When no rustup exists
//! (a Nix sandbox, an offline distro build) and the `rustc` on PATH reports
//! exactly the pinned `X.Y.Z` for this host, with a sibling `cargo`, that
//! toolchain satisfies the pin: soldr uses it instead of bootstrapping rustup
//! and fetching the channel. A mismatch keeps today's fetch path. Named
//! channels (`stable`, `nightly-*`) never take this shortcut, and a host with
//! rustup keeps rustup in charge so a PATH proxy is never mistaken for a
//! system toolchain.

use crate::core::{SoldrPaths, TargetTriple};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// True for an exact `X.Y.Z` release channel.
pub(crate) fn is_exact_version_channel(channel: &str) -> bool {
    let parts: Vec<&str> = channel.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// `(release, host)` from `rustc -vV` output.
pub(crate) fn parse_rustc_vv(stdout: &str) -> Option<(String, String)> {
    let field = |name: &str| {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(|value| value.trim().to_string())
    };
    Some((field("release:")?, field("host:")?))
}

/// The PATH `rustc` with a sibling `cargo`, as `(dir, rustc)`.
fn path_rustc_with_cargo() -> Option<(PathBuf, PathBuf)> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let rustc = crate::binaries::executable_in_dir(&dir, "rustc")?;
        crate::binaries::executable_in_dir(&dir, "cargo")?;
        Some((dir, rustc))
    })
}

fn rustc_version_and_host(rustc: &Path) -> Option<(String, String)> {
    let mut command = std::process::Command::new(rustc);
    command.arg("-vV");
    crate::core::suppress_windows_console_window(&mut command);
    let output = crate::core::command_output_with_timeout(&mut command, "rustc -vV").ok()?;
    if !output.status.success() {
        return None;
    }
    parse_rustc_vv(&String::from_utf8_lossy(&output.stdout))
}

/// What the PATH toolchain looks like, probed once per process: `None`
/// when rustup is available (rustup owns the pin) or no rustc+cargo pair
/// is on PATH.
struct SystemToolchain {
    bin_dir: PathBuf,
    release: String,
    host: String,
}

fn system_toolchain() -> Option<&'static SystemToolchain> {
    static PROBE: OnceLock<Option<SystemToolchain>> = OnceLock::new();
    PROBE
        .get_or_init(|| {
            if std::env::var_os(crate::TEST_RUSTUP_BIN_ENV_VAR).is_some() {
                return None;
            }
            let paths = SoldrPaths::new().ok()?;
            if crate::fetch::discover_rustup(&paths).is_some() {
                return None;
            }
            let (bin_dir, rustc) = path_rustc_with_cargo()?;
            let (release, host) = rustc_version_and_host(&rustc)?;
            Some(SystemToolchain {
                bin_dir,
                release,
                host,
            })
        })
        .as_ref()
}

/// Pure decision behind [`matching_bin_dir`].
pub(crate) fn satisfies_pin(channel: &str, release: &str, host: &str, want_host: &str) -> bool {
    is_exact_version_channel(channel) && release == channel && host == want_host
}

/// The bin dir of a host toolchain that satisfies the exact pin `channel`.
pub(crate) fn matching_bin_dir(channel: &str) -> Option<&'static Path> {
    if !is_exact_version_channel(channel) {
        return None;
    }
    let found = system_toolchain()?;
    let want_host = TargetTriple::host().ok()?;
    satisfies_pin(channel, &found.release, &found.host, &want_host.triple())
        .then_some(found.bin_dir.as_path())
}

/// `tool` from the matching host toolchain for the pin nearest `start_dir`.
pub(crate) fn pinned_system_binary(tool: &str, start_dir: Option<&Path>) -> Option<PathBuf> {
    let manifest = crate::core::read_rust_toolchain_manifest_from_ancestors(start_dir?).ok()?;
    let channel = manifest.channel?;
    crate::binaries::executable_in_dir(matching_bin_dir(channel.trim())?, tool)
}

/// True when a rustup-less host toolchain satisfies the whole manifest:
/// components and targets still need rustup.
pub(crate) fn satisfies_manifest(
    channel: &str,
    manifest: &crate::core::RustToolchainManifest,
) -> bool {
    let needs_rustup = |list: &Option<Vec<String>>| list.as_ref().is_some_and(|l| !l.is_empty());
    !needs_rustup(&manifest.components)
        && !needs_rustup(&manifest.targets)
        && matching_bin_dir(channel).is_some()
}

/// Run a pinned install; on failure, name a mismatching PATH rustc.
pub(crate) fn with_mismatch_note(
    channel: &str,
    install: impl FnOnce() -> Result<(), crate::core::SoldrError>,
) -> Result<(), crate::core::SoldrError> {
    install().map_err(|err| match mismatch_note(channel) {
        Some(note) => crate::core::SoldrError::Other(format!("{err}{note}")),
        None => err,
    })
}

/// The PATH rustc version when it does not satisfy `channel`, for the
/// install-failure diagnostic.
fn mismatch_note(channel: &str) -> Option<String> {
    let found = system_toolchain()?;
    (found.release != channel).then(|| {
        format!(
            "; the rustc on PATH ({}) is {} for {}, which does not satisfy the pin {channel}",
            found.bin_dir.display(),
            found.release,
            found.host
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_versions_qualify() {
        assert!(is_exact_version_channel("1.95.0"));
        for channel in [
            "stable",
            "1.95",
            "nightly-2026-05-28",
            "1.95.0-beta",
            "1..0",
            "",
        ] {
            assert!(!is_exact_version_channel(channel), "{channel}");
        }
    }

    #[test]
    fn parses_release_and_host() {
        let out = "rustc 1.95.0 (abc 2026-01-01)\nbinary: rustc\nhost: x86_64-unknown-linux-gnu\nrelease: 1.95.0\n";
        assert_eq!(
            parse_rustc_vv(out),
            Some(("1.95.0".into(), "x86_64-unknown-linux-gnu".into()))
        );
        assert_eq!(parse_rustc_vv("rustc 1.95.0\n"), None);
    }

    #[test]
    fn pin_needs_exact_release_and_host() {
        let host = "x86_64-unknown-linux-gnu";
        assert!(satisfies_pin("1.95.0", "1.95.0", host, host));
        assert!(!satisfies_pin("1.95.0", "1.95.1", host, host));
        assert!(!satisfies_pin(
            "1.95.0",
            "1.95.0",
            "aarch64-unknown-linux-gnu",
            host
        ));
        assert!(!satisfies_pin("stable", "stable", host, host));
    }
}
