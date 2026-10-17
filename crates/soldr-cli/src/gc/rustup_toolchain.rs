//! `soldr gc purge --kind rustup_toolchain` — the real deletion path for
//! installed rustup toolchains (soldr#3507).
//!
//! Until #3507 this kind was report-only: the purge arm rejected it with
//! "cargo/rustup own deletion", `toolchain.rs` only ever installs, and
//! nothing in the tree pruned `$RUSTUP_HOME/toolchains/` — so a long-lived
//! soldr home accumulated every channel any pin had ever named (the ~93 GB
//! in the report). Delegation stays honest under that old message: soldr
//! never `remove_dir_all`s a toolchain directory itself. Each candidate is
//! handed to `rustup toolchain uninstall <name>` and rustup owns the bytes.
//!
//! Two homes are enumerated, and every report says which one (soldr#1799):
//! the caller's own `$RUSTUP_HOME` (or `~/.rustup`) and the managed
//! `<soldr-root>/rustup` where `soldr toolchain prepare` installs. #1799
//! keeps managed homes out of *binary resolution* unless a resolved binary
//! physically lives in them; enumeration is inventory, and the 20 GB of
//! #3507 lives precisely there.
//!
//! Safety rails — a candidate must never be:
//!   (a) the home's default toolchain (`settings.toml`) or one of its
//!       rustup overrides, or the toolchain this invocation resolves to
//!       (`RUSTUP_TOOLCHAIN`, or the `rust-toolchain.toml` channel found
//!       in the working directory's ancestors); or
//!   (b) the stable pin of the repo the command runs in — the same
//!       ancestor read, which inside this repo is `channel = "1.98.1"`.
//!
//! A `settings.toml` that exists but cannot be read or parsed fails
//! closed: every toolchain in that home is protected and the report says
//! so. Selection is a pure function over names ([`plan_home`] /
//! [`select_rustup_toolchain_candidates`]) so the rails are unit-testable
//! without invoking rustup or touching a real home.

use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::walks::fast_directory_size_and_files;
use super::{GC_JSON_SCHEMA_VERSION, KIND_RUSTUP_TOOLCHAIN};
use crate::core::{SoldrError, SoldrPaths};

/// Why a rustup home is in scope for enumeration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) enum RustupHomeOrigin {
    /// The caller's own `$RUSTUP_HOME`, or `~/.rustup` when unset.
    Caller,
    /// soldr's managed `<root>/rustup` (soldr#1799), where
    /// `soldr toolchain prepare` and managed bootstrap install.
    Managed,
}

impl RustupHomeOrigin {
    fn as_str(self) -> &'static str {
        match self {
            RustupHomeOrigin::Caller => "caller",
            RustupHomeOrigin::Managed => "managed",
        }
    }
}

/// One rustup home the GC enumerates. The report always names both the
/// path and the origin so it is unambiguous *which* home lost a toolchain.
#[derive(Clone, Debug, Serialize)]
pub(super) struct EnumeratedRustupHome {
    pub(super) path: PathBuf,
    pub(super) origin: RustupHomeOrigin,
}

/// Homes to enumerate, caller first, managed second (skipped when it is
/// the same directory, or when it does not exist).
///
/// `paths` is optional so a caller without a resolvable soldr root still
/// gets the caller home; a missing root must not block the caller-home
/// purge.
pub(super) fn homes_for_enumeration(paths: Option<&SoldrPaths>) -> Vec<EnumeratedRustupHome> {
    let mut homes = Vec::new();
    if let Some(caller) = crate::core::resolve_rustup_home() {
        if caller.exists() {
            homes.push(EnumeratedRustupHome {
                path: caller,
                origin: RustupHomeOrigin::Caller,
            });
        }
    }
    if let Some(paths) = paths {
        let managed = crate::fetch::managed_rustup_home(paths);
        if managed.exists() && !homes.iter().any(|home| same_home(&home.path, &managed)) {
            homes.push(EnumeratedRustupHome {
                path: managed,
                origin: RustupHomeOrigin::Managed,
            });
        }
    }
    homes
}

/// Path equality with a best-effort canonicalization so a symlinked
/// `$HOME` cannot make one physical home show up twice.
fn same_home(a: &Path, b: &Path) -> bool {
    a == b
        || match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

/// Channels that must never be uninstalled in *any* enumerated home: the
/// `RUSTUP_TOOLCHAIN` override and the `rust-toolchain.toml` channel
/// resolved from the working directory's ancestors (rail (a) active pin +
/// rail (b) repo pin — the same read satisfies both).
///
/// A `rust-toolchain.toml` that exists but cannot be parsed is a hard
/// error: without the pin there is no way to prove a toolchain is not the
/// repo's, and this command deletes.
fn shared_protected_channels() -> Result<Vec<String>, SoldrError> {
    let refuse = |reason: &str| {
        SoldrError::Other(format!(
            "gc purge --kind rustup_toolchain: refusing to purge toolchains because the active pin cannot be resolved ({reason}); fix rust-toolchain.toml or unset the override first"
        ))
    };
    let mut channels = Vec::new();
    if let Some(value) = std::env::var_os(crate::toolchain::RUSTUP_TOOLCHAIN_ENV_VAR) {
        let trimmed = value.to_string_lossy().trim().to_string();
        if !trimmed.is_empty() {
            channels.push(trimmed);
        }
    }
    let cwd = std::env::current_dir().map_err(|e| refuse(&format!("cwd unreadable: {e}")))?;
    let manifest = crate::core::read_rust_toolchain_manifest_from_ancestors(&cwd)
        .map_err(|e| refuse(&e.to_string()))?;
    if let Some(channel) = manifest.channel {
        let trimmed = channel.trim().to_string();
        if !trimmed.is_empty() {
            channels.push(trimmed);
        }
    }
    Ok(channels)
}

/// What `<rustup-home>/settings.toml` declares for the home.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct HomeSettings {
    /// `default_toolchain` — the home's `rustup default`.
    pub(super) default: Option<String>,
    /// Toolchain names bound by `[overrides]` (`rustup override set`).
    pub(super) overrides: Vec<String>,
    /// The file exists but cannot be read or parsed: the home's active
    /// toolchain is unknowable, so the whole home must fail closed.
    pub(super) unreadable: bool,
}

impl HomeSettings {
    fn fail_closed() -> Self {
        HomeSettings {
            unreadable: true,
            ..HomeSettings::default()
        }
    }
}

/// Read `default_toolchain` and the `[overrides]` toolchain names out of
/// rustup's own `settings.toml`. A missing file is normal (a fresh or
/// default-less managed home); anything unreadable or malformed fails
/// closed so a parse drift can never delete the default.
pub(super) fn read_home_settings(settings_toml: &Path) -> HomeSettings {
    let text = match std::fs::read_to_string(settings_toml) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HomeSettings::default(),
        Err(_) => return HomeSettings::fail_closed(),
    };
    let value: toml::Value = match text.parse() {
        Ok(value) => value,
        Err(_) => return HomeSettings::fail_closed(),
    };
    let Some(table) = value.as_table() else {
        return HomeSettings::fail_closed();
    };
    let default = match table.get("default_toolchain") {
        None => None,
        Some(toml::Value::String(name)) if !name.trim().is_empty() => Some(name.clone()),
        // An empty or wrong-typed default is an unknown state, not an
        // absent one: fail closed rather than guess.
        Some(_) => return HomeSettings::fail_closed(),
    };
    let mut overrides = Vec::new();
    match table.get("overrides") {
        None => {}
        Some(toml::Value::Table(map)) => {
            for value in map.values() {
                match value {
                    toml::Value::String(name) if !name.trim().is_empty() => {
                        overrides.push(name.clone());
                    }
                    // Ditto: a malformed override entry means the file no
                    // longer matches what rustup would enforce.
                    _ => return HomeSettings::fail_closed(),
                }
            }
        }
        Some(_) => return HomeSettings::fail_closed(),
    }
    HomeSettings {
        default,
        overrides,
        unreadable: false,
    }
}

/// Installed toolchain directory names under `<rustup-home>/toolchains`,
/// sorted for deterministic output. Symlinks are skipped: rustup only
/// creates real directories for downloaded toolchains (a
/// `rustup toolchain link` entry is a symlink and is never this
/// command's business), matching [`super::walks::walk_rustup_toolchains`].
pub(super) fn installed_toolchain_names(toolchains_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(toolchains_root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_type()
                .map(|file_type| !file_type.is_symlink())
                .unwrap_or(false)
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// The selection outcome for one home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HomePlan {
    /// Sorted, deduplicated names that must not be uninstalled here.
    pub(super) protected: Vec<String>,
    /// Installed toolchains no protection covers, in sorted install order.
    pub(super) candidates: Vec<String>,
}

/// Decide what this home may lose. Pure: no filesystem, no rustup, no
/// environment — the rails live entirely here.
pub(super) fn plan_home(
    installed: &[String],
    settings: &HomeSettings,
    shared_protected: &[String],
) -> HomePlan {
    if settings.unreadable {
        // Fail closed: an unreadable settings file means we cannot know
        // which toolchain is active, so nothing in this home is eligible.
        let protected: BTreeSet<String> = installed
            .iter()
            .chain(shared_protected.iter())
            .cloned()
            .collect();
        return HomePlan {
            protected: protected.into_iter().collect(),
            candidates: Vec::new(),
        };
    }
    let mut protected: BTreeSet<String> = shared_protected.iter().cloned().collect();
    protected.extend(settings.default.iter().cloned());
    protected.extend(settings.overrides.iter().cloned());
    let protected: Vec<String> = protected.into_iter().collect();
    let candidates = select_rustup_toolchain_candidates(installed, &protected);
    HomePlan {
        protected,
        candidates,
    }
}

/// Installed toolchains that no protected name covers, order preserved.
pub(super) fn select_rustup_toolchain_candidates(
    installed: &[String],
    protected: &[String],
) -> Vec<String> {
    installed
        .iter()
        .filter(|name| {
            !protected
                .iter()
                .any(|keep| toolchain_name_is_protected(name, keep))
        })
        .cloned()
        .collect()
}

/// Whether an installed toolchain directory is covered by a protected
/// channel name. Three rules, in order:
///
///   1. exact string equality (settings' default always matches its own
///      directory name verbatim);
///   2. equality after stripping a target-triple suffix from both sides,
///      so a pin of `1.98.1` covers the installed directory
///      `1.98.1-x86_64-unknown-linux-gnu`;
///   3. version-segment prefix in the *protected → installed* direction,
///      so a pin of `1.95` (a `rust-toolchain.toml` minor-channel pin)
///      covers the resolved directory `1.95.0-...`. Deliberately
///      one-directional: `1.95` also covers `1.95.1-...`, which is
///      conservative (an extra toolchain left behind), while the reverse
///      direction would let a pin of `1.98.1` miss `1.98` and delete it.
///
/// Rule 2 never strips a bare `nightly` pin into covering dated
/// nightlies: `nightly-2026-05-28-x86_64-...` normalizes to
/// `nightly-2026-05-28`, not `nightly`, which is exactly the distinction
/// that keeps the six dated nightlies of #3507 eligible while a
/// `channel = "nightly"` default stays protected.
pub(super) fn toolchain_name_is_protected(installed: &str, protected: &str) -> bool {
    if installed == protected {
        return true;
    }
    let installed_normalized = normalize_toolchain_name(installed);
    let protected_normalized = normalize_toolchain_name(protected);
    if installed_normalized == protected_normalized {
        return true;
    }
    match (
        version_segments(installed_normalized),
        version_segments(protected_normalized),
    ) {
        (Some(installed), Some(protected)) if protected.len() <= installed.len() => {
            installed.starts_with(&protected)
        }
        _ => false,
    }
}

/// Strip a trailing `-<target-triple>` (`stable-x86_64-unknown-linux-gnu`
/// → `stable`) so channel pins without a triple still match the directory
/// rustup created for them.
///
/// A triple is recognized as a segment containing both a letter and a
/// digit (`x86_64`, `aarch64`, `i686`, `armv7`, ...) with at least two
/// segments after it (vendor + os). That shape rejects every segment of a
/// dated nightly — `2026`, `05`, `28` are digit-only — so
/// `nightly-2026-05-28-x86_64-...` normalizes to `nightly-2026-05-28`
/// rather than collapsing to `nightly`. Names that do not look like
/// `prefix-triple` (custom links, bare channels) are returned unchanged.
fn normalize_toolchain_name(name: &str) -> &str {
    let mut offset = 0usize;
    for segment in name.split('-') {
        if segment.chars().any(|c| c.is_ascii_alphabetic())
            && segment.chars().any(|c| c.is_ascii_digit())
        {
            let segments_from_here = name[offset..].split('-').count();
            if segments_from_here < 3 {
                return name;
            }
            // `offset` points at this segment; the byte before it is the
            // hyphen that joined it, and everything before that is the
            // channel. At offset 0 there is no channel left — keep the
            // original name (a triple alone is not a normalized name).
            if offset == 0 {
                return name;
            }
            let cut = &name[..offset - 1];
            if cut.is_empty() {
                return name;
            }
            return cut;
        }
        offset += segment.len() + 1;
    }
    name
}

/// `Some([...])` only when every dot-separated segment is non-empty
/// ASCII digits (`1.98.1`), else `None`.
fn version_segments(name: &str) -> Option<Vec<&str>> {
    let segments: Vec<&str> = name.split('.').collect();
    if segments.is_empty()
        || segments
            .iter()
            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some(segments)
}

// ---------------------------------------------------------------------------
// The purge command.
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize)]
struct CandidateReport {
    toolchain: String,
    size_bytes: u64,
    size_human: String,
}

#[derive(Clone, Serialize)]
struct FailureReport {
    toolchain: String,
    error: String,
}

#[derive(Clone, Serialize)]
struct HomePlanReport {
    rustup_home: String,
    /// `"caller"` or `"managed"` — which home this row is (soldr#1799
    /// "say which" rule).
    origin: &'static str,
    installed_count: usize,
    default_toolchain: Option<String>,
    /// The home's `settings.toml` exists but could not be read or
    /// parsed; every toolchain in it was protected (fail closed).
    settings_unreadable: bool,
    protected: Vec<String>,
    /// Every eligible toolchain, whether or not it was chosen.
    candidates: Vec<CandidateReport>,
    /// The candidates chosen for uninstall. In `--dry-run` this is every
    /// candidate (dry-run cannot prompt, so it reports what
    /// `--dry-run --all` would remove).
    selected: Vec<CandidateReport>,
    uninstalled: Vec<String>,
    failures: Vec<FailureReport>,
}

#[derive(Serialize)]
struct GcPurgeRustupToolchainOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    kind: &'static str,
    dry_run: bool,
    selected_count: usize,
    uninstalled_count: usize,
    failed_count: usize,
    reclaimed_bytes: u64,
    reclaimed_human: String,
    homes: Vec<HomePlanReport>,
}

/// `soldr gc purge --kind rustup_toolchain [--all] [--dry-run]`.
///
/// Flow: enumerate both homes → plan candidates against the rails →
/// either report (`--dry_run`) or prompt/select → hand every selection to
/// `rustup toolchain uninstall` with `RUSTUP_HOME` pinned to the home the
/// candidate was enumerated from.
pub(crate) fn run_gc_purge_rustup_toolchain_command(
    purge_all: bool,
    json: bool,
    dry_run: bool,
) -> Result<(), SoldrError> {
    let shared_protected = shared_protected_channels()?;
    let soldr_paths = SoldrPaths::new().ok();
    let mut homes = plan_all_homes(soldr_paths.as_ref(), &shared_protected);

    if !json {
        if homes.is_empty() {
            eprintln!(
                "soldr gc purge --kind rustup_toolchain: no rustup home found ({} or ~/.rustup and the managed home); nothing to do",
                crate::core::RUSTUP_HOME_ENV_VAR
            );
        }
        for home in &homes {
            print_home_scan(home);
        }
    }

    if dry_run {
        let reclaimable: u64 = homes
            .iter()
            .flat_map(|home| home.candidates.iter())
            .map(|candidate| candidate.size_bytes)
            .sum();
        for home in homes.iter_mut() {
            // A dry-run cannot prompt, so it reports the `--all` shape:
            // every eligible candidate is what would go.
            home.selected = home.candidates.clone();
        }
        let selected_count: usize = homes.iter().map(|home| home.selected.len()).sum();
        if !json {
            for home in &homes {
                for candidate in &home.selected {
                    eprintln!(
                        "soldr gc purge --kind rustup_toolchain: would uninstall {} ({}) [rustup_home={}]",
                        candidate.toolchain,
                        candidate.size_human,
                        home.rustup_home
                    );
                }
            }
            eprintln!(
                "soldr gc purge --kind rustup_toolchain: dry-run: {selected_count} eligible ({} reclaimable); nothing deleted, no rustup invoked",
                crate::cache_lib::target_registry::human_size(reclaimable)
            );
        }
        return emit_output(&homes, dry_run, selected_count, 0, 0, 0, json);
    }

    let selected_count = select_candidates(&mut homes, purge_all);
    let (uninstalled_count, failed_count, reclaimed_bytes) = uninstall_selected(&mut homes, json);
    if !json {
        eprintln!(
            "soldr gc purge --kind rustup_toolchain: selected {selected_count}; uninstalled {uninstalled_count}; failed {failed_count}; reclaimed {}",
            crate::cache_lib::target_registry::human_size(reclaimed_bytes)
        );
    }
    emit_output(
        &homes,
        dry_run,
        selected_count,
        uninstalled_count,
        failed_count,
        reclaimed_bytes,
        json,
    )
}

/// Enumerate + plan every home, sizing the candidates as it goes.
fn plan_all_homes(
    soldr_paths: Option<&SoldrPaths>,
    shared_protected: &[String],
) -> Vec<HomePlanReport> {
    homes_for_enumeration(soldr_paths)
        .into_iter()
        .map(|home| {
            let settings = read_home_settings(&home.path.join("settings.toml"));
            let installed = installed_toolchain_names(&home.path.join("toolchains"));
            let plan = plan_home(&installed, &settings, shared_protected);
            let candidates = plan
                .candidates
                .iter()
                .map(|name| {
                    let (size_bytes, _files) =
                        fast_directory_size_and_files(&home.path.join("toolchains").join(name));
                    CandidateReport {
                        toolchain: name.clone(),
                        size_bytes,
                        size_human: crate::cache_lib::target_registry::human_size(size_bytes),
                    }
                })
                .collect();
            HomePlanReport {
                rustup_home: home.path.display().to_string(),
                origin: home.origin.as_str(),
                installed_count: installed.len(),
                default_toolchain: settings.default.clone(),
                settings_unreadable: settings.unreadable,
                protected: plan.protected,
                candidates,
                selected: Vec::new(),
                uninstalled: Vec::new(),
                failures: Vec::new(),
            }
        })
        .collect()
}

fn print_home_scan(home: &HomePlanReport) {
    if home.settings_unreadable {
        eprintln!(
            "soldr gc purge --kind rustup_toolchain: rustup_home={} ({}): {} installed; settings.toml unreadable — protecting all of them (fail closed); 0 eligible",
            home.rustup_home, home.origin, home.installed_count
        );
        return;
    }
    let protected = if home.protected.is_empty() {
        "none".to_string()
    } else {
        home.protected.join(", ")
    };
    eprintln!(
        "soldr gc purge --kind rustup_toolchain: rustup_home={} ({}): {} installed; protected {} ({}); {} eligible",
        home.rustup_home,
        home.origin,
        home.installed_count,
        home.protected.len(),
        protected,
        home.candidates.len()
    );
}

/// Prompt (unless `--all`) and record the accepted candidates in
/// `selected`. The eligible `candidates` list stays complete so the
/// report can distinguish "eligible" from "chosen". Returns how many
/// were selected across all homes.
fn select_candidates(homes: &mut [HomePlanReport], purge_all: bool) -> usize {
    let mut selected_count = 0usize;
    for home in homes.iter_mut() {
        let mut selected = Vec::new();
        for candidate in &home.candidates {
            let keep = purge_all
                || prompt_uninstall(
                    &candidate.toolchain,
                    &candidate.size_human,
                    &home.rustup_home,
                );
            if keep {
                selected_count += 1;
                selected.push(candidate.clone());
            }
        }
        home.selected = selected;
    }
    selected_count
}

/// `soldr gc: uninstall <name> ...? [Y/n] `. Unlike the other gc prompts,
/// EOF (a closed stdin — CI, `</dev/null`) answers **no**: a GB-scale
/// deletion must be opted into with `--all` explicitly, never granted by
/// a missing terminal. Only a line that actually arrived gets the
/// default-yes semantics of [`super::purge::parse_gc_purge_answer`].
fn prompt_uninstall(toolchain: &str, size_human: &str, rustup_home: &str) -> bool {
    use std::io::{BufRead, Write};
    eprint!("soldr gc: uninstall {toolchain} ({size_human}) from {rustup_home}? [Y/n] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => super::purge::parse_gc_purge_answer(&line),
    }
}

/// Uninstall every selected candidate via rustup, lazily resolving the
/// rustup binary only when something is actually selected (so a clean
/// no-op never triggers rustup discovery/bootstrap). Returns
/// `(uninstalled, failed, reclaimed_bytes)`.
fn uninstall_selected(homes: &mut [HomePlanReport], json: bool) -> (usize, usize, u64) {
    let selected_total: usize = homes.iter().map(|home| home.selected.len()).sum();
    if selected_total == 0 {
        return (0, 0, 0);
    }
    let rustup = crate::binaries::rustup_binary();
    let mut uninstalled = 0usize;
    let mut failed = 0usize;
    let mut reclaimed_bytes = 0u64;
    for home in homes.iter_mut() {
        for candidate in home.selected.clone() {
            match uninstall_toolchain(&rustup, &home.rustup_home, &candidate.toolchain, json) {
                Ok(()) => {
                    uninstalled += 1;
                    reclaimed_bytes = reclaimed_bytes.saturating_add(candidate.size_bytes);
                    home.uninstalled.push(candidate.toolchain);
                }
                Err(error) => {
                    failed += 1;
                    if !json {
                        eprintln!(
                            "soldr gc purge --kind rustup_toolchain: failed to uninstall {} in {}: {error}",
                            candidate.toolchain, home.rustup_home
                        );
                    }
                    home.failures.push(FailureReport {
                        toolchain: candidate.toolchain,
                        error,
                    });
                }
            }
        }
    }
    (uninstalled, failed, reclaimed_bytes)
}

/// The actual deletion: rustup owns it, soldr only asks. `RUSTUP_HOME` is
/// pinned to the home the candidate was enumerated from so a caller-home
/// candidate can never be resolved against the managed home (or vice
/// versa).
fn uninstall_toolchain(
    rustup: &Path,
    rustup_home: &str,
    toolchain: &str,
    json: bool,
) -> Result<(), String> {
    let mut command = std::process::Command::new(rustup);
    command.args(["toolchain", "uninstall", toolchain]);
    command.env(crate::core::RUSTUP_HOME_ENV_VAR, rustup_home);
    crate::core::suppress_windows_console_window(&mut command);
    let output = crate::platform::wait::output_for(&mut command, crate::platform::wait::LONG_CHILD)
        .map_err(|err| {
            format!("failed to spawn `rustup toolchain uninstall {toolchain}`: {err}")
        })?;
    // Always surface the child's streams — never swallow them. In JSON
    // mode rustup's stdout would land in the payload's stream, so it is
    // forwarded to stderr there instead.
    use std::io::Write as _;
    if !output.stdout.is_empty() {
        let sink: &mut dyn std::io::Write = if json {
            &mut std::io::stderr()
        } else {
            &mut std::io::stdout()
        };
        let _ = sink.write_all(&output.stdout);
    }
    if !output.stderr.is_empty() {
        let _ = std::io::stderr().write_all(&output.stderr);
    }
    if output.status.success() {
        return Ok(());
    }
    let from_stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let from_stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !from_stderr.is_empty() {
        from_stderr
    } else if !from_stdout.is_empty() {
        from_stdout
    } else {
        match output.status.code() {
            Some(code) => format!("exit code {code}"),
            None => "terminated by signal".to_string(),
        }
    };
    Err(detail)
}

fn emit_output(
    homes: &[HomePlanReport],
    dry_run: bool,
    selected_count: usize,
    uninstalled_count: usize,
    failed_count: usize,
    reclaimed_bytes: u64,
    json: bool,
) -> Result<(), SoldrError> {
    if json {
        let output = GcPurgeRustupToolchainOutput {
            schema_version: GC_JSON_SCHEMA_VERSION,
            command: "gc",
            mode: "purge",
            kind: KIND_RUSTUP_TOOLCHAIN,
            dry_run,
            selected_count,
            uninstalled_count,
            failed_count,
            reclaimed_bytes,
            reclaimed_human: crate::cache_lib::target_registry::human_size(reclaimed_bytes),
            homes: homes.to_vec(),
        };
        crate::cache::print_json(&output)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "rustup_toolchain_tests.rs"]
mod tests;
