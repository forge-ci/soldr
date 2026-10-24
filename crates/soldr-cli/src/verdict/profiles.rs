//! Per-tool adapters for the verdict ledger (soldr#3894, soldr#3895).
//!
//! The ledger core (`key`, `record`, `store`, `gate`) knows nothing about any
//! particular checker. Everything tool-specific lives in a [`Profile`]: a
//! plain data row that says which cargo invocations are the check, which
//! extra files and environment variables can change its verdict, how to
//! identify the tool's version, and whether the check is a pure function of
//! its inputs at all. Adding a linter is one row here, or one
//! `[workspace.metadata.soldr.verdict.tools.<name>]` table in the
//! workspace's `Cargo.toml`, never new code.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One checker adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Name used in keys, records and `soldr verdict` output.
    pub name: String,
    /// The cargo subcommand this adapter recognises (`clippy`, `fmt`, ...).
    pub subcommand: String,
    /// Every listed argument must appear somewhere in the invocation.
    #[serde(default)]
    pub requires: Vec<String>,
    /// At least one of these must appear, when non-empty.
    #[serde(default)]
    pub requires_any: Vec<String>,
    /// Any of these makes the invocation something other than a pure check
    /// (`--fix` rewrites sources; `advisories` reads the network).
    #[serde(default)]
    pub forbids: Vec<String>,
    /// Extra input globs beyond the Rust base set, repo-relative.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Extra environment variables hashed into the key; a trailing `*` is a
    /// prefix match.
    #[serde(default)]
    pub env: Vec<String>,
    /// Version probes. `toolchain` is always implied; `soldr` adds the soldr
    /// version; anything else is a command line whose stdout is hashed.
    #[serde(default)]
    pub identity: Vec<String>,
    /// Whether the recorded output is replayed on a hit.
    #[serde(default = "default_true")]
    pub replay: bool,
    /// `false` declares the check impure: it is recognised (so `explain` can
    /// say why) but never skipped.
    #[serde(default = "default_true")]
    pub pure: bool,
}

fn default_true() -> bool {
    true
}

#[allow(clippy::too_many_arguments)]
fn row(
    name: &str,
    subcommand: &str,
    requires: &[&str],
    requires_any: &[&str],
    forbids: &[&str],
    inputs: &[&str],
    identity: &[&str],
    pure: bool,
) -> Profile {
    let owned = |items: &[&str]| items.iter().map(|s| (*s).to_string()).collect();
    Profile {
        name: name.to_string(),
        subcommand: subcommand.to_string(),
        requires: owned(requires),
        requires_any: owned(requires_any),
        forbids: owned(forbids),
        inputs: owned(inputs),
        env: Vec::new(),
        identity: owned(identity),
        replay: true,
        pure,
    }
}

/// The built-in adapter table.
pub fn builtin() -> Vec<Profile> {
    let mut clippy = row(
        "clippy",
        "clippy",
        &[],
        &[],
        &["--fix", "--allow-dirty", "--allow-staged"],
        &["**/clippy.toml", "**/.clippy.toml"],
        &[],
        true,
    );
    clippy.env.push("CLIPPY_CONF_DIR".into());
    vec![
        clippy,
        row(
            "fmt",
            "fmt",
            &["--check"],
            &[],
            &[],
            &["**/rustfmt.toml", "**/.rustfmt.toml"],
            &[],
            true,
        ),
        // The dylint driver is built and resolved by soldr, so soldr's own
        // version is part of the tool identity.
        row(
            "dylint",
            "dylint",
            &[],
            &[],
            &["--fix", "--new", "--upgrade", "--list"],
            &["**/dylint.toml"],
            &["soldr"],
            true,
        ),
        row("doctest", "test", &["--doc"], &[], &[], &[], &[], true),
        // `check` with no named check includes `advisories`, which reads the
        // network, so only the explicitly pure checks are skippable.
        row(
            "deny",
            "deny",
            &["check"],
            &["bans", "licenses", "sources"],
            &["advisories"],
            &["**/deny.toml"],
            &["cargo deny --version"],
            true,
        ),
        row(
            "machete",
            "machete",
            &[],
            &[],
            &["--fix"],
            &["**/.cargo-machete.toml"],
            &["cargo machete --version"],
            true,
        ),
        // Reads the live advisory database: recognised, never skipped.
        row("audit", "audit", &[], &[], &[], &[], &[], false),
    ]
}

#[derive(Debug, Default, Deserialize)]
struct Manifest {
    #[serde(default)]
    workspace: Option<MetaHolder>,
    #[serde(default)]
    package: Option<MetaHolder>,
}

#[derive(Debug, Default, Deserialize)]
struct MetaHolder {
    #[serde(default)]
    metadata: Option<SoldrHolder>,
}

#[derive(Debug, Default, Deserialize)]
struct SoldrHolder {
    #[serde(default)]
    soldr: Option<VerdictHolder>,
}

#[derive(Debug, Default, Deserialize)]
struct VerdictHolder {
    #[serde(default)]
    verdict: Option<WorkspaceConfig>,
}

/// `[workspace.metadata.soldr.verdict]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct WorkspaceConfig {
    /// `false` turns the ledger off for this workspace.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Repo-relative globs dropped from every key (for files known not to
    /// affect any check, such as a docs tree). Use sparingly: an exclusion
    /// that is wrong turns into a false skip.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Extra or overriding adapter rows, keyed by name.
    #[serde(default)]
    pub tools: BTreeMap<String, ToolRow>,
}

/// A user adapter row; every field but `subcommand` defaults.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ToolRow {
    pub subcommand: Option<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub requires_any: Vec<String>,
    #[serde(default)]
    pub forbids: Vec<String>,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub identity: Vec<String>,
    pub replay: Option<bool>,
    pub pure: Option<bool>,
}

/// Read `[workspace.metadata.soldr.verdict]` (or the `package` form for a
/// single-crate repo) from `<root>/Cargo.toml`. A missing or unparseable
/// manifest yields the default configuration.
pub fn workspace_config(root: &Path) -> WorkspaceConfig {
    let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml")) else {
        return WorkspaceConfig::default();
    };
    parse_workspace_config(&text)
}

pub(crate) fn parse_workspace_config(text: &str) -> WorkspaceConfig {
    let Ok(manifest) = toml::from_str::<Manifest>(text) else {
        return WorkspaceConfig::default();
    };
    let pick = |holder: Option<MetaHolder>| {
        holder
            .and_then(|h| h.metadata)
            .and_then(|m| m.soldr)
            .and_then(|s| s.verdict)
    };
    pick(manifest.workspace)
        .or_else(|| pick(manifest.package))
        .unwrap_or_default()
}

/// Built-ins merged with the workspace's rows: a row whose name matches a
/// built-in replaces the fields it sets; a new name adds an adapter.
pub fn effective(config: &WorkspaceConfig) -> Vec<Profile> {
    let mut profiles = builtin();
    for (name, user) in &config.tools {
        let base = profiles.iter().position(|p| &p.name == name);
        let mut profile = match base {
            Some(index) => profiles.remove(index),
            None => row(name, name, &[], &[], &[], &[], &[], true),
        };
        if let Some(sub) = &user.subcommand {
            profile.subcommand.clone_from(sub);
        }
        let set = |field: &mut Vec<String>, value: &Vec<String>| {
            if !value.is_empty() {
                field.clone_from(value);
            }
        };
        set(&mut profile.requires, &user.requires);
        set(&mut profile.requires_any, &user.requires_any);
        set(&mut profile.forbids, &user.forbids);
        set(&mut profile.inputs, &user.inputs);
        set(&mut profile.env, &user.env);
        set(&mut profile.identity, &user.identity);
        if let Some(replay) = user.replay {
            profile.replay = replay;
        }
        if let Some(pure) = user.pure {
            profile.pure = pure;
        }
        profiles.push(profile);
    }
    profiles
}

/// The cargo subcommand of a front-door argv (`["+nightly", "clippy", ...]`
/// → `clippy`), skipping a leading toolchain directive and global flags.
pub fn subcommand_of(args: &[String]) -> Option<&str> {
    let mut iter = args.iter().map(String::as_str);
    while let Some(arg) = iter.next() {
        if arg == "--" {
            return None;
        }
        if arg.starts_with('+') {
            continue;
        }
        if matches!(arg, "-C" | "-Z" | "--config" | "--color") {
            iter.next();
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg);
    }
    None
}

fn mentions(args: &[String], needle: &str) -> bool {
    args.iter().any(|arg| {
        arg == needle
            || arg
                .strip_prefix(needle)
                .is_some_and(|rest| rest.starts_with('='))
    })
}

/// Why an invocation is or is not a ledger-gated check.
#[derive(Debug, PartialEq, Eq)]
pub enum Match<'a> {
    /// A pure check this adapter covers.
    Check(&'a Profile),
    /// An adapter recognised the subcommand but this invocation is not a
    /// skippable check (the reason names the rule).
    NotPure(&'a Profile, String),
    /// No adapter claims the subcommand.
    Unknown,
}

/// Find the adapter for a front-door argv.
pub fn match_invocation<'a>(profiles: &'a [Profile], args: &[String]) -> Match<'a> {
    let Some(sub) = subcommand_of(args) else {
        return Match::Unknown;
    };
    let candidates: Vec<&Profile> = profiles.iter().filter(|p| p.subcommand == sub).collect();
    if candidates.is_empty() {
        return Match::Unknown;
    }
    let mut first_reason = None;
    for profile in candidates {
        match check_rules(profile, args) {
            Ok(()) => return Match::Check(profile),
            Err(reason) => {
                first_reason.get_or_insert((profile, reason));
            }
        }
    }
    let (profile, reason) = first_reason.expect("at least one candidate");
    Match::NotPure(profile, reason)
}

fn check_rules(profile: &Profile, args: &[String]) -> Result<(), String> {
    if !profile.pure {
        return Err(format!("`{}` is declared impure", profile.name));
    }
    if let Some(missing) = profile.requires.iter().find(|r| !mentions(args, r)) {
        return Err(format!("`{}` requires `{missing}`", profile.name));
    }
    if !profile.requires_any.is_empty() && !profile.requires_any.iter().any(|r| mentions(args, r)) {
        return Err(format!(
            "`{}` requires one of {:?}",
            profile.name, profile.requires_any
        ));
    }
    if let Some(found) = profile.forbids.iter().find(|f| mentions(args, f)) {
        return Err(format!("`{found}` is not a pure check"));
    }
    Ok(())
}
