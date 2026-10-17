//! Lazy auto-install of rustup components for `soldr cargo {fmt,clippy,miri}`
//! (issue #597).
//!
//! ## Why
//!
//! `rustfmt`, `clippy`, and `miri` ship as rustup *components*, not as
//! ecosystem cargo subcommands in `crate::fetch::known_tools`. Today, on
//! a fresh toolchain that has not had the component added (e.g. a base
//! `rust:1.94.1-bookworm` Docker image, a stripped CI runner, or a
//! contributor who just ran `rustup toolchain install` without
//! `--profile default`), running `soldr cargo fmt` surfaces:
//!
//! ```text
//! error: 'cargo-fmt' is not installed for the toolchain '1.94.1-...'.
//! help: run `rustup component add rustfmt` to install it
//! ```
//!
//! Soldr's whole story is "tools just work" — that promise already
//! covers ecosystem cargo subcommands (`nextest`, `audit`, ...) and
//! rustup itself (#407 auto-bootstrap), but it fails for rustup
//! components. This module closes the gap with the smallest possible
//! framework: probe → lazy-install → memoize.
//!
//! ## Behavior
//!
//! On every `soldr cargo <sub>` invocation:
//!
//! 1. Look up `<sub>` in the static `SUBCOMMAND_TO_COMPONENT` map. If
//!    not present, no-op.
//! 2. Honor `SOLDR_NO_AUTO_COMPONENT=1` (also accepts `true`/`yes`/`on`).
//!    No-op when set — let the bare rustup error surface so the user
//!    can read it verbatim.
//! 3. Probe with `rustup which <probe_binary>` (cheap: ~10 ms when the
//!    component is installed, ~50 ms when missing).
//! 4. On hit: memoize `(channel, component) → Installed` and return.
//! 5. On miss: emit a one-line stderr notice, run
//!    `rustup component add <component>` (against the resolved channel
//!    when one is declared in `rust-toolchain.toml`, else against
//!    rustup's default toolchain). Re-probe; memoize the outcome.
//!    Any failure is logged once and then ignored — cargo's own error
//!    is what the user sees.
//!
//! `miri` is **nightly-gated**: on a non-nightly resolved channel the
//! probe is skipped entirely so we never try to add a component that
//! upstream only ships for nightly.
//!
//! ## Memo lifetime
//!
//! The memo is *per-process*. A single `soldr cargo` invocation will
//! never re-probe after the first call. Subsequent soldr invocations
//! re-probe (~10 ms penalty per first cargo subcommand call after a
//! fresh shell), which is acceptable — the probe is faster than the
//! cargo startup cost it precedes.

use super::subcommand::first_cargo_subcommand;
use crate::binaries::rustup_binary;
use crate::core::{apply_implicit_toolchain_homes, read_rust_toolchain_manifest, SoldrPaths};
use crate::platform::sync::Mutex;
use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Env var that disables the auto-install. Accepts `1`/`true`/`yes`/`on`.
pub const SOLDR_NO_AUTO_COMPONENT_ENV: &str = "SOLDR_NO_AUTO_COMPONENT";

/// One row in the (cargo subcommand → rustup component) map.
#[derive(Debug, Clone, Copy)]
struct ComponentForSubcommand {
    /// The cargo subcommand the user typed after `soldr cargo`. e.g.
    /// `"fmt"`, `"clippy"`, `"miri"`.
    cargo_sub: &'static str,
    /// The rustup component name as `rustup component add` understands
    /// it. e.g. `"rustfmt"`, `"clippy"`, `"miri"`.
    component: &'static str,
    /// The binary `rustup which <name>` resolves when the component is
    /// installed. e.g. `"cargo-fmt"`, `"cargo-clippy"`, `"cargo-miri"`.
    probe_binary: &'static str,
    /// `true` for components that only exist on the `nightly` channel.
    /// Skipped on stable/beta resolved channels (so we never try to
    /// install a component that upstream does not ship).
    nightly_only: bool,
}

const SUBCOMMAND_TO_COMPONENT: &[ComponentForSubcommand] = &[
    ComponentForSubcommand {
        cargo_sub: "fmt",
        component: "rustfmt",
        probe_binary: "cargo-fmt",
        nightly_only: false,
    },
    ComponentForSubcommand {
        cargo_sub: "clippy",
        component: "clippy",
        probe_binary: "cargo-clippy",
        nightly_only: false,
    },
    ComponentForSubcommand {
        cargo_sub: "miri",
        component: "miri",
        probe_binary: "cargo-miri",
        nightly_only: true,
    },
];

fn opt_out_enabled() -> bool {
    crate::core::flag(SOLDR_NO_AUTO_COMPONENT_ENV)
}

fn lookup_component(sub: &str) -> Option<&'static ComponentForSubcommand> {
    SUBCOMMAND_TO_COMPONENT
        .iter()
        .find(|row| row.cargo_sub == sub)
}

/// Per-process memo so a single `soldr cargo` invocation never re-
/// probes after the first call. Key is `"<channel>|<component>"` so
/// users with multiple toolchains in flight (rare) do not cross-
/// pollinate.
fn memo() -> &'static Mutex<HashSet<String>> {
    static MEMO: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashSet::new()))
}

fn memo_key(channel: &str, component: &str) -> String {
    format!("{channel}|{component}")
}

/// Public entry point. Called from `run_cargo_front_door` after the
/// trampolines fall through and before the cargo `Command` builds.
/// All failure paths are silent — every branch returns to the caller
/// so cargo runs as it would have without us.
pub(crate) fn maybe_install_component_for_subcommand(args: &[String], paths: &SoldrPaths) {
    let _ = paths; // reserved for future tighter resolution; current code path needs no paths
    if opt_out_enabled() {
        return;
    }
    let Some(sub) = first_cargo_subcommand(args) else {
        return;
    };
    let Some(row) = lookup_component(sub) else {
        return;
    };

    let manifest_dir = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let channel = resolve_channel(&manifest_dir);

    if row.nightly_only && !is_nightly_channel(channel.as_deref()) {
        // Skip — the component only exists for nightly. Let cargo/
        // rustup show the real error.
        return;
    }

    let memo_id = memo_key(channel.as_deref().unwrap_or(""), row.component);
    {
        let guard = memo().lock();
        if guard.contains(&memo_id) {
            return;
        }
    }

    if probe_component_installed(row.probe_binary, channel.as_deref()) {
        record_memo(memo_id);
        return;
    }

    emit_install_notice(row.component, channel.as_deref());
    let install_ok = run_component_add(row.component, channel.as_deref());
    if install_ok && probe_component_installed(row.probe_binary, channel.as_deref()) {
        record_memo(memo_id);
    }
    // If the install (or the re-probe) failed, let cargo surface the
    // original "not installed" error to the user. We have already
    // emitted our notice so they can correlate the failure to soldr's
    // auto-bootstrap attempt.
}

fn record_memo(key: String) {
    let mut guard = memo().lock();
    guard.insert(key);
}

fn resolve_channel(manifest_dir: &Path) -> Option<String> {
    read_rust_toolchain_manifest(manifest_dir)
        .ok()
        .and_then(|m| m.channel)
}

fn is_nightly_channel(channel: Option<&str>) -> bool {
    matches!(channel, Some(c) if c.starts_with("nightly"))
}

// reason: `rustup which` is an existence probe; a miss prints "not found" on
// stderr, which is the expected `false` answer that triggers the install below.
#[cfg_attr(
    dylint_lib = "ban_swallowed_child_stdio",
    allow(ban_swallowed_child_stdio)
)]
fn probe_component_installed(probe_binary: &str, channel: Option<&str>) -> bool {
    // Invocation shape: `rustup +<channel> which <probe_binary>` when a
    // channel is resolved, else `rustup which <probe_binary>` so rustup
    // picks the active toolchain itself.
    let mut cmd = Command::new(rustup_binary());
    if let Some(ch) = channel {
        cmd.arg(format!("+{ch}"));
    }
    cmd.args(["which", probe_binary]);
    apply_implicit_toolchain_homes(&mut cmd, None);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    matches!(crate::platform::wait::status_for(&mut cmd, crate::platform::wait::PROBE), Ok(s) if s.success())
}

fn run_component_add(component: &str, channel: Option<&str>) -> bool {
    if let Err(error) =
        crate::core::forbid_toolchain_install_tripwire(&format!("rustup component add {component}"))
    {
        eprintln!("soldr: {error}");
        return false;
    }
    let mut cmd = Command::new(rustup_binary());
    cmd.args(["component", "add"]);
    if let Some(ch) = channel {
        cmd.args(["--toolchain", ch]);
    }
    cmd.arg(component);
    apply_implicit_toolchain_homes(&mut cmd, None);
    // Let rustup print its install progress so users see what is
    // happening. The notice we emitted on stderr above is the
    // soldr-side framing.
    matches!(crate::platform::wait::status_for(&mut cmd, crate::platform::wait::LONG_CHILD), Ok(s) if s.success())
}

fn emit_install_notice(component: &str, channel: Option<&str>) {
    let channel_part = channel
        .map(|c| format!(" for toolchain {c}"))
        .unwrap_or_default();
    eprintln!(
        "soldr cargo: installing rustup component '{component}'{channel_part} (first-use bootstrap; \
         set {SOLDR_NO_AUTO_COMPONENT_ENV}=1 to disable)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_covers_fmt_clippy_miri_only() {
        assert!(lookup_component("fmt").is_some());
        assert!(lookup_component("clippy").is_some());
        assert!(lookup_component("miri").is_some());
        assert!(lookup_component("build").is_none());
        assert!(lookup_component("test").is_none());
        assert!(lookup_component("nextest").is_none());
        assert!(lookup_component("").is_none());
    }

    #[test]
    fn fmt_resolves_to_rustfmt_component_and_cargo_fmt_probe() {
        let row = lookup_component("fmt").expect("fmt entry");
        assert_eq!(row.component, "rustfmt");
        assert_eq!(row.probe_binary, "cargo-fmt");
        assert!(!row.nightly_only);
    }

    #[test]
    fn clippy_resolves_to_clippy_component_and_cargo_clippy_probe() {
        let row = lookup_component("clippy").expect("clippy entry");
        assert_eq!(row.component, "clippy");
        assert_eq!(row.probe_binary, "cargo-clippy");
        assert!(!row.nightly_only);
    }

    #[test]
    fn miri_marked_nightly_only() {
        let row = lookup_component("miri").expect("miri entry");
        assert_eq!(row.component, "miri");
        assert_eq!(row.probe_binary, "cargo-miri");
        assert!(row.nightly_only);
    }

    #[test]
    fn nightly_channel_detection_is_prefix_match() {
        assert!(is_nightly_channel(Some("nightly")));
        assert!(is_nightly_channel(Some("nightly-2026-01-15")));
        assert!(is_nightly_channel(Some("nightly-x86_64-pc-windows-msvc")));
        assert!(!is_nightly_channel(Some("stable")));
        assert!(!is_nightly_channel(Some("beta")));
        assert!(!is_nightly_channel(Some("1.94.1")));
        assert!(!is_nightly_channel(None));
    }

    #[test]
    fn opt_out_shares_canonical_flag_parser() {
        // This switch uses the shared parser, not a hand-rolled match.
        assert!(crate::core::flag_value(" ON "));
    }

    #[test]
    fn opt_out_accepts_canonical_truthy_values() {
        // Save & restore so other tests aren't affected.
        let prior = std::env::var_os(SOLDR_NO_AUTO_COMPONENT_ENV);
        for v in ["1", "true", "yes", "on", "TRUE", "YES"] {
            std::env::set_var(SOLDR_NO_AUTO_COMPONENT_ENV, v);
            assert!(opt_out_enabled(), "expected {v} to enable opt-out");
        }
        for v in ["0", "false", "no", "off", "", "garbage"] {
            std::env::set_var(SOLDR_NO_AUTO_COMPONENT_ENV, v);
            assert!(!opt_out_enabled(), "expected {v} to NOT enable opt-out");
        }
        std::env::remove_var(SOLDR_NO_AUTO_COMPONENT_ENV);
        assert!(!opt_out_enabled());
        // Restore prior.
        if let Some(v) = prior {
            std::env::set_var(SOLDR_NO_AUTO_COMPONENT_ENV, v);
        }
    }

    #[test]
    fn memo_key_format_is_pipe_separated() {
        assert_eq!(memo_key("1.94.1", "rustfmt"), "1.94.1|rustfmt");
        assert_eq!(memo_key("", "clippy"), "|clippy");
    }
}
