//! Catalogue-driven nightly selection for Dylint and its nested commands.
//!
//! This file answers *which nightly, and who chose it*, and stops there. Once a
//! [`DylintToolchainPlan`] is settled, locating and validating the matching
//! `dylint-driver` binary — the binary-or-exit gate, the catalogue fetch, and
//! the `PATH` / `LD_LIBRARY_PATH` / `DYLD_LIBRARY_PATH` the driver needs to load
//! the nightly's `rustc_private` libraries — belongs to `dylint_driver.rs`,
//! split out under soldr#2945 when the provenance work pushed this file past
//! the hard 1,000-line ceiling in `.github/scripts/loc_ceiling.py`.

use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use crate::core::{
    command_output_with_timeout, suppress_windows_console_window, SoldrError, SoldrPaths,
};
use crate::dylint_libraries::is_dated_nightly;
use crate::dylint_toolchain_readiness::{
    dylint_toolchain_readiness_at, ensure_dylint_toolchain_ready_at,
    partial_dylint_toolchain_error, DylintToolchainReadiness,
};
#[cfg(test)]
use crate::toolchain_readiness::TOOLCHAIN_CHANNEL_MANIFEST;
use crate::{apply_implicit_toolchain_homes, resolve_toolchain_binary_for_channel, rustup_binary};

pub(crate) const TOOLCHAIN_ENV_VAR: &str = "SOLDR_DYLINT_TOOLCHAIN";
pub(crate) const COMPILER_RELEASE_ENV_VAR: &str = "SOLDR_DYLINT_RUSTC_RELEASE";
pub(crate) const COMPILER_COMMIT_ENV_VAR: &str = "SOLDR_DYLINT_RUSTC_COMMIT_HASH";
pub(crate) const CACHE_IDENTITY_ENV_VAR: &str = "SOLDR_DYLINT_CACHE_IDENTITY";
pub(crate) const PREPARED_IDENTITY_ENV_VAR: &str = "SOLDR_DYLINT_PREPARED_IDENTITY";
pub(crate) const SUCCESS_MARKER_ENV_VAR: &str = "SOLDR_DYLINT_SUCCESS_MARKER";
pub(crate) const CONFIGURED_TOOLCHAIN_ENV_VAR: &str = "SOLDR_DYLINT_CONFIGURED_TOOLCHAIN";
pub(crate) const CONFIGURED_COMPILER_RELEASE_ENV_VAR: &str =
    "SOLDR_DYLINT_CONFIGURED_RUSTC_RELEASE";
pub(crate) const CONFIGURED_COMPILER_COMMIT_ENV_VAR: &str =
    "SOLDR_DYLINT_CONFIGURED_RUSTC_COMMIT_HASH";

/// Overrides the freshness window (seconds) for the warm-run prepared-plan
/// marker consulted by [`prepare`]. `0` means "never trust the marker" —
/// every run pays the full catalogue-fetch + rustup-probe path. Unset
/// falls back to [`DEFAULT_PREPARE_TTL`].
pub(crate) const PREPARE_TTL_ENV_VAR: &str = "SOLDR_DYLINT_PREPARE_TTL_SECS";
/// When truthy (`1` / `true`, case-insensitive), skips the prepared-plan
/// marker entirely and always re-runs the full catalogue-fetch + rustup
/// verification path, even if a fresh marker exists on disk.
pub(crate) const REVERIFY_ENV_VAR: &str = "SOLDR_DYLINT_REVERIFY";

const MAP_ASSET: &str = "rust-nightly-versions.v1.json";
const REQUIRED_COMPONENTS: &[&str] = &["rustc-dev", "rust-src", "llvm-tools-preview"];

/// Default freshness window for the warm-run prepared-plan marker: 24
/// hours. Long enough that a normal dev inner loop (many `soldr cargo
/// dylint` invocations across a day) skips the network + subprocess
/// probes entirely, short enough that a stale marker is naturally
/// reclaimed without operator intervention.
const DEFAULT_PREPARE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Schema tag for the on-disk marker layout
/// (`<soldr_root>/dylint/prepared/<PREPARE_MARKER_SCHEMA>/<version>.identity`).
/// Bump this if the marker file format ever changes shape so stale
/// markers from an older soldr build are never misread.
const PREPARE_MARKER_SCHEMA: &str = "v1";

/// Which tier of the precedence chain chose the Dylint channel (soldr#2945).
///
/// Threaded onto the resolved plan so a driver-gate failure can *state* where
/// the channel came from rather than guessing at the error site. The tier is
/// the single most useful fact in that diagnostic: a missing driver for a
/// channel the lint libraries pinned is a pin problem, the same failure for a
/// channel derived from the version map is a derivation problem, and the two
/// have completely different fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChannelProvenance {
    /// Tier 1 — an explicit `+toolchain` / `--toolchain` argument.
    Explicit,
    /// Tier 2 — `SOLDR_DYLINT_TOOLCHAIN`, the `SOLDR_DYLINT_CONFIGURED_*`
    /// identity triple, or `RUSTUP_TOOLCHAIN`.
    Environment,
    /// Tier 3 — the workspace's own Dylint lint libraries. This is the
    /// authority whenever the workspace has lints: Dylint builds one driver
    /// per library toolchain.
    LintLibraries,
    /// Tier 4 — the workspace root `rust-toolchain.toml` `[toolchain].channel`.
    RootManifest,
    /// Tier 5 — derived from a Rust `major.minor` through the catalogue's
    /// `rust-nightly-versions.v1.json` map.
    VersionMap,
    /// Not chosen by the precedence chain at all: the channel was carried in
    /// from a plan frozen earlier in the run (`soldr ci-test` rebuilds one
    /// from its plan document) or constructed by a test.
    Unresolved,
}

impl ChannelProvenance {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Explicit => "an explicit +toolchain argument",
            Self::Environment => {
                "the environment (SOLDR_DYLINT_TOOLCHAIN / \
                 SOLDR_DYLINT_CONFIGURED_TOOLCHAIN / RUSTUP_TOOLCHAIN)"
            }
            Self::LintLibraries => {
                "this workspace's Dylint lint libraries \
                 (workspace.metadata.dylint.libraries -> rust-toolchain.toml)"
            }
            Self::RootManifest => "the workspace root rust-toolchain.toml [toolchain].channel",
            Self::VersionMap => {
                "rust-nightly-versions.v1.json, derived from the workspace's Rust version"
            }
            Self::Unresolved => "a plan frozen earlier in this run (not re-derived here)",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DylintToolchainPlan {
    pub channel: String,
    pub compiler_release: String,
    pub compiler_commit: String,
    /// Diagnostic-only: which precedence tier selected `channel`.
    pub provenance: ChannelProvenance,
}

/// Two plans are equal when they name the same compiler. `provenance` records
/// *how* the channel was chosen, not *what* was chosen, so it is deliberately
/// outside the identity — a plan restored from the prepared marker describes
/// the same compiler as the plan that wrote it (soldr#2945).
impl PartialEq for DylintToolchainPlan {
    fn eq(&self, other: &Self) -> bool {
        self.channel == other.channel
            && self.compiler_release == other.compiler_release
            && self.compiler_commit == other.compiler_commit
    }
}

impl Eq for DylintToolchainPlan {}

impl DylintToolchainPlan {
    /// A plan carrying only a compiler identity. Every resolver path inside
    /// this module settles the identity first and stamps the tier last with
    /// [`Self::with_provenance`], so none of them has to guess a tier at the
    /// point it builds the plan.
    pub(crate) fn identity(channel: String, release: String, commit: String) -> Self {
        Self {
            channel,
            compiler_release: release,
            compiler_commit: commit,
            provenance: ChannelProvenance::Unresolved,
        }
    }

    fn with_provenance(mut self, provenance: ChannelProvenance) -> Self {
        self.provenance = provenance;
        self
    }

    pub(crate) fn cache_identity(&self) -> String {
        format!(
            "{}|{}|{}",
            self.channel, self.compiler_release, self.compiler_commit
        )
    }

    pub(crate) fn apply_to_command(&self, command: &mut std::process::Command) {
        // The toolchain name only means something relative to a home: pin the
        // one the nightly and its targets were provisioned in (soldr#3567).
        crate::dylint_rustup_home::apply_dylint_rustup_home(command);
        command.env("RUSTUP_TOOLCHAIN", &self.channel);
        command.env(TOOLCHAIN_ENV_VAR, &self.channel);
        command.env(COMPILER_RELEASE_ENV_VAR, &self.compiler_release);
        command.env(COMPILER_COMMIT_ENV_VAR, &self.compiler_commit);
        command.env(CACHE_IDENTITY_ENV_VAR, self.cache_identity());
        command.env(PREPARED_IDENTITY_ENV_VAR, self.cache_identity());
        crate::dylint_driver::apply_dylint_driver_path(command);
    }
}

pub(crate) fn write_success_marker(plan: &DylintToolchainPlan) -> Result<(), SoldrError> {
    let Some(path) = std::env::var_os(SUCCESS_MARKER_ENV_VAR)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
    else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", plan.cache_identity()))?;
    Ok(())
}

pub(crate) fn clear_success_marker() -> Result<(), SoldrError> {
    let Some(path) = std::env::var_os(SUCCESS_MARKER_ENV_VAR)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
    else {
        return Ok(());
    };
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Deserialize)]
struct NightlyVersionMap {
    schema_version: u32,
    nightlies: BTreeMap<String, NightlyIdentity>,
    versions: BTreeMap<String, VersionBucket>,
}

#[derive(Debug, Deserialize)]
struct NightlyIdentity {
    rust_version: String,
    rustc_release: String,
    rustc_commit_hash: String,
}

#[derive(Debug, Deserialize)]
struct VersionBucket {
    nightlies: Vec<String>,
    selected: String,
}

pub(crate) async fn prepare(
    requested_channel: Option<&str>,
    workspace_root: &Path,
) -> Result<DylintToolchainPlan, SoldrError> {
    if non_empty_env(TOOLCHAIN_ENV_VAR).is_none()
        && plan_from_configured_environment()?.is_none()
        && !crate::core::flag(REVERIFY_ENV_VAR)
    {
        let requested = requested_toolchain_channel(requested_channel, workspace_root)?;
        if requested
            .channel
            .as_deref()
            .is_none_or(|c| !is_dated_nightly(c))
        {
            let version = requested_rust_version(requested.channel.as_deref())?;
            if let Some(mut plan) = load_prepared_marker(&version) {
                plan.channel = qualify_toolchain_name(&plan.channel)?;
                // The marker stores a compiler identity, not how the channel
                // was chosen; restore the tier this run actually resolved so a
                // later driver diagnostic stays truthful (soldr#2945).
                plan.provenance = requested.provenance;
                eprintln!(
                    "soldr: dylint toolchain {} ready (cached identity)",
                    plan.channel
                );
                return Ok(plan);
            }
        }
    }
    let plan = resolve_plan_inner(requested_channel, workspace_root, true).await?;
    prepare_resolved(plan)
}

pub(crate) async fn resolve_plan(
    requested_channel: Option<&str>,
    workspace_root: &Path,
) -> Result<DylintToolchainPlan, SoldrError> {
    resolve_plan_inner(requested_channel, workspace_root, false).await
}

async fn resolve_plan_inner(
    requested_channel: Option<&str>,
    workspace_root: &Path,
    install_explicit_if_unmapped: bool,
) -> Result<DylintToolchainPlan, SoldrError> {
    let requested = requested_toolchain_channel(requested_channel, workspace_root)?;
    // The retained / configured environment carries a full compiler identity,
    // so reusing it skips both the catalogue fetch and the rustc probe. It is
    // consulted only when it names the channel the precedence chain already
    // selected: that is what makes an explicit `+toolchain` beat an inherited
    // `SOLDR_DYLINT_TOOLCHAIN` (soldr#2945) while leaving the nested-invocation
    // fast path — where the two always agree — exactly as it was.
    if let Some(plan) = plan_from_environment_identity(requested.channel.as_deref())? {
        // Attribute to the tier that *chose* the channel, not to the
        // environment that merely happened to already know its identity.
        return Ok(plan.with_provenance(requested.provenance));
    }
    let paths = crate::core::SoldrPaths::new()?;
    let bytes = crate::fetch::fetch_verified_catalogue_asset(
        &paths,
        "zackees",
        "soldr-toolchain",
        "assets",
        MAP_ASSET,
    )
    .await?;
    let plan = if let Some(channel) = requested.channel.as_deref().filter(|c| is_dated_nightly(c)) {
        let plan = match select_explicit_from_map(&bytes, channel) {
            Ok(plan) => plan,
            Err(map_error) => {
                if let Some(plan) = plan_from_installed_explicit_nightly(channel)? {
                    plan
                } else if install_explicit_if_unmapped {
                    install_and_observe_explicit_nightly(channel)?
                } else {
                    return Err(map_error);
                }
            }
        };
        plan.with_provenance(requested.provenance)
    } else {
        // No tier named a nightly outright, so the channel that comes back is
        // the map's own choice, whatever the Rust version was derived from.
        let version = requested_rust_version(requested.channel.as_deref())?;
        select_from_map(&bytes, &version)?.with_provenance(ChannelProvenance::VersionMap)
    };
    Ok(plan)
}

/// Reuse the compiler identity the environment already carries, but only when
/// it describes `selected` — the channel the precedence chain chose. `None`
/// means "the environment cannot answer for this channel", not "the
/// environment is unset".
fn plan_from_environment_identity(
    selected: Option<&str>,
) -> Result<Option<DylintToolchainPlan>, SoldrError> {
    let names_selected = |channel: &str| {
        selected.is_none_or(|value| {
            crate::dylint_libraries::canonical_channel(value)
                == crate::dylint_libraries::canonical_channel(channel)
        })
    };
    if let Some(channel) = non_empty_env(TOOLCHAIN_ENV_VAR) {
        if !names_selected(&channel) {
            return Ok(None);
        }
        return plan_from_retained_environment(&channel).map(Some);
    }
    if let Some(plan) = plan_from_configured_environment()? {
        if !names_selected(&plan.channel) {
            return Ok(None);
        }
        return Ok(Some(plan));
    }
    Ok(None)
}

fn plan_from_installed_explicit_nightly(
    channel: &str,
) -> Result<Option<DylintToolchainPlan>, SoldrError> {
    let manager_home = crate::dylint_rustup_home::dylint_rustup_home()?;
    match dylint_toolchain_readiness_at(&manager_home, channel) {
        DylintToolchainReadiness::Missing => return Ok(None),
        DylintToolchainReadiness::Partial {
            qualified,
            directory,
            missing,
        } => {
            return Err(partial_dylint_toolchain_error(
                &qualified, &directory, &missing,
            ))
        }
        DylintToolchainReadiness::Ready { .. } => {}
    }
    if resolve_toolchain_binary_for_channel(concat!("rust", "c"), Some(channel)).is_err() {
        return Ok(None);
    }
    let (compiler_release, compiler_commit) = observe_compiler(channel)?;
    let identity = NightlyIdentity {
        rust_version: major_minor(&compiler_release).unwrap_or_default(),
        rustc_release: compiler_release.clone(),
        rustc_commit_hash: compiler_commit.clone(),
    };
    validate_identity(channel, &identity)?;
    Ok(Some(DylintToolchainPlan::identity(
        channel.to_string(),
        compiler_release,
        compiler_commit,
    )))
}

fn install_and_observe_explicit_nightly(channel: &str) -> Result<DylintToolchainPlan, SoldrError> {
    if resolve_toolchain_binary_for_channel(concat!("rust", "c"), Some(channel)).is_err() {
        let code =
            crate::toolchain::rustup_toolchain_install_with_profile(channel, Some("minimal"))?;
        if code != 0 {
            return Err(SoldrError::Other(format!(
                "rustup failed to install {channel} (exit {code})"
            )));
        }
    }
    plan_from_installed_explicit_nightly(channel)?.ok_or_else(|| {
        SoldrError::Other(format!(
            "installed explicit Dylint toolchain `{channel}` could not be resolved"
        ))
    })
}

pub(crate) fn prepare_resolved(
    mut plan: DylintToolchainPlan,
) -> Result<DylintToolchainPlan, SoldrError> {
    let prepared_identity = plan.cache_identity();
    if non_empty_env(PREPARED_IDENTITY_ENV_VAR).as_deref() != Some(prepared_identity.as_str()) {
        ensure_installed(&plan)?;
        verify_installed_identity(&plan)?;
    }
    plan.channel = qualify_toolchain_name(&plan.channel)?;
    if let Some(version) = major_minor(&plan.compiler_release) {
        if let Err(error) = write_prepared_marker(&version, &plan) {
            eprintln!("soldr: failed to write dylint prepared marker: {error}");
        }
    }
    Ok(plan)
}

pub(crate) fn verify_if_installed(plan: &DylintToolchainPlan) -> Result<bool, SoldrError> {
    if resolve_toolchain_binary_for_channel(concat!("rust", "c"), Some(&plan.channel)).is_err() {
        return Ok(false);
    }
    verify_installed_identity(plan)?;
    Ok(true)
}

pub(crate) fn verify_observed_identity(plan: &DylintToolchainPlan) -> Result<(), SoldrError> {
    verify_installed_identity(plan)
}

fn qualify_toolchain_name(channel: &str) -> Result<String, SoldrError> {
    if is_fully_qualified_nightly(channel) {
        return Ok(channel.to_string());
    }
    let binary = resolve_toolchain_binary_for_channel(concat!("rust", "c"), Some(channel))?;
    let host = observe_compiler_host(&binary, channel)?;
    Ok(format!("{channel}-{host}"))
}

/// `pub(crate)` for `dylint_driver`, which derives cargo-dylint's
/// `<nightly>-<host triple>` driver directory name from the same predicate
/// (soldr#2945 split).
pub(crate) fn is_fully_qualified_nightly(channel: &str) -> bool {
    channel
        .strip_prefix("nightly-")
        .and_then(|value| value.get(10..))
        .is_some_and(|suffix| suffix.starts_with('-') && suffix.len() > 1)
}

/// `<soldr_root>/dylint/prepared/<PREPARE_MARKER_SCHEMA>/<sanitized version>.identity`
fn prepared_marker_path(base_dir: &Path, version: &str) -> PathBuf {
    base_dir
        .join("dylint")
        .join("prepared")
        .join(PREPARE_MARKER_SCHEMA)
        .join(format!("{}.identity", sanitize_marker_key(version)))
}

/// Replace any character that is not filesystem-safe across every
/// soldr-supported host with `_`. `version` is normally a plain
/// `major.minor` string, but this also tolerates an explicit channel
/// string being threaded through in the future without risking path
/// traversal or reserved-character issues on Windows.
fn sanitize_marker_key(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn prepare_ttl() -> Duration {
    non_empty_env(PREPARE_TTL_ENV_VAR)
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_PREPARE_TTL)
}

/// Production entry point for the warm-path marker lookup: resolves the
/// real soldr root + `RUSTUP_HOME` and delegates to
/// [`load_prepared_marker_from`] with the current TTL policy and wall
/// clock. Returns `None` on any resolution failure so callers always
/// fall through to the full cold path rather than erroring.
fn load_prepared_marker(version: &str) -> Option<DylintToolchainPlan> {
    let base_dir = SoldrPaths::new().ok()?.root;
    let rustup_home = crate::dylint_rustup_home::dylint_rustup_home().ok()?;
    load_prepared_marker_from(
        &base_dir,
        &rustup_home,
        version,
        prepare_ttl(),
        SystemTime::now(),
    )
}

/// Testable core of the warm-path marker lookup. A marker "hits" only
/// when all of the following hold:
///
/// * `ttl` is non-zero (zero means "never trust the marker"),
/// * the marker file exists and its mtime is within `ttl` of `now`,
/// * its single line parses into a shape-valid identity
///   (`validate_identity`'s channel / release / commit checks), and
/// * the toolchain directory the identity names is still present under
///   `rustup_home/toolchains/` (a cheap directory-existence check —
///   deliberately no subprocess spawn).
fn load_prepared_marker_from(
    base_dir: &Path,
    rustup_home: &Path,
    version: &str,
    ttl: Duration,
    now: SystemTime,
) -> Option<DylintToolchainPlan> {
    if ttl.is_zero() {
        return None;
    }
    let path = prepared_marker_path(base_dir, version);
    let metadata = std::fs::metadata(&path).ok()?;
    let modified = metadata.modified().ok()?;
    let age = now.duration_since(modified).ok()?;
    if age > ttl {
        return None;
    }
    let contents = std::fs::read_to_string(&path).ok()?;
    let plan = parse_marker_identity(contents.lines().next()?)?;
    if !matches!(
        dylint_toolchain_readiness_at(rustup_home, &plan.channel),
        DylintToolchainReadiness::Ready { .. }
    ) {
        return None;
    }
    Some(plan)
}

/// Parse a single `<channel>|<rustc_release>|<commit_hash>` marker line
/// (the same shape [`DylintToolchainPlan::cache_identity`] produces)
/// back into a plan, reusing [`validate_identity`]'s shape checks so a
/// corrupted or foreign-format marker is rejected rather than trusted.
fn parse_marker_identity(line: &str) -> Option<DylintToolchainPlan> {
    let mut parts = line.splitn(3, '|');
    let channel = parts.next()?.to_string();
    let compiler_release = parts.next()?.to_string();
    let compiler_commit = parts.next()?.to_string();
    if parts.next().is_some() || channel.is_empty() {
        return None;
    }
    let identity = NightlyIdentity {
        rust_version: major_minor(&compiler_release).unwrap_or_default(),
        rustc_release: compiler_release.clone(),
        rustc_commit_hash: compiler_commit.clone(),
    };
    validate_identity(&channel, &identity).ok()?;
    Some(DylintToolchainPlan::identity(
        channel,
        compiler_release,
        compiler_commit,
    ))
}

/// Cheap on-disk sanity check standing in for a `rustup component list`
/// subprocess: does `rustup_home/toolchains/` contain a directory for
/// `channel`? Tries the exact `<channel>-<host triple>` name first (the
/// standard rustup toolchain directory naming); falls back to accepting
/// any directory whose name starts with `<channel>-` if the host triple
/// cannot be determined, so an unusual host doesn't spuriously miss a
/// perfectly good warm cache.
/// Production entry point for persisting the warm-path marker after a
/// successful cold-path prepare. Best-effort by contract — callers must
/// not fail the run on a write error, only log it.
fn write_prepared_marker(version: &str, plan: &DylintToolchainPlan) -> Result<(), SoldrError> {
    let base_dir = SoldrPaths::new()?.root;
    write_prepared_marker_at(&base_dir, version, plan)
}

fn write_prepared_marker_at(
    base_dir: &Path,
    version: &str,
    plan: &DylintToolchainPlan,
) -> Result<(), SoldrError> {
    let path = prepared_marker_path(base_dir, version);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", plan.cache_identity()))?;
    Ok(())
}

fn plan_from_configured_environment() -> Result<Option<DylintToolchainPlan>, SoldrError> {
    let values = (
        non_empty_env(CONFIGURED_TOOLCHAIN_ENV_VAR),
        non_empty_env(CONFIGURED_COMPILER_RELEASE_ENV_VAR),
        non_empty_env(CONFIGURED_COMPILER_COMMIT_ENV_VAR),
    );
    match values {
        (None, None, None) => Ok(None),
        (Some(channel), Some(compiler_release), Some(compiler_commit)) => {
            let identity = NightlyIdentity {
                rust_version: major_minor(&compiler_release).unwrap_or_default(),
                rustc_release: compiler_release.clone(),
                rustc_commit_hash: compiler_commit.clone(),
            };
            validate_identity(&channel, &identity)?;
            Ok(Some(DylintToolchainPlan::identity(
                channel,
                compiler_release,
                compiler_commit,
            )))
        }
        _ => Err(SoldrError::Other(
            "configured Dylint compiler identity is incomplete".into(),
        )),
    }
}

fn plan_from_retained_environment(channel: &str) -> Result<DylintToolchainPlan, SoldrError> {
    match (
        non_empty_env(COMPILER_RELEASE_ENV_VAR),
        non_empty_env(COMPILER_COMMIT_ENV_VAR),
    ) {
        (Some(compiler_release), Some(compiler_commit)) => Ok(DylintToolchainPlan::identity(
            channel.to_string(),
            compiler_release,
            compiler_commit,
        )),
        _ => {
            let (compiler_release, compiler_commit) = observe_compiler(channel)?;
            Ok(DylintToolchainPlan::identity(
                channel.to_string(),
                compiler_release,
                compiler_commit,
            ))
        }
    }
}

fn requested_rust_version(requested_channel: Option<&str>) -> Result<String, SoldrError> {
    if let Some(channel) = requested_channel {
        if let Some(version) = major_minor(channel) {
            return Ok(version);
        }
        let (release, _) = observe_compiler(channel)?;
        return major_minor(&release).ok_or_else(|| {
            SoldrError::Other(format!(
                "could not derive a major.minor Rust version from `{release}`"
            ))
        });
    }
    let binary = crate::resolve_toolchain_binary(concat!("rust", "c"))?;
    let (release, _) = observe_compiler_binary(&binary, "the default Rust toolchain")?;
    major_minor(&release).ok_or_else(|| {
        SoldrError::Other(format!(
            "could not derive a major.minor Rust version from `{release}`"
        ))
    })
}

/// The channel the precedence chain selected, plus which tier selected it.
struct RequestedChannel {
    /// `None` only in the last tier, where there is no channel to name and the
    /// version map derives one from the default toolchain's Rust version.
    channel: Option<String>,
    provenance: ChannelProvenance,
}

/// The Dylint channel precedence chain (soldr#2945):
///
/// 1. an explicit `+toolchain` / `--toolchain` argument,
/// 2. the environment (`SOLDR_DYLINT_TOOLCHAIN`, the
///    `SOLDR_DYLINT_CONFIGURED_*` identity triple, `RUSTUP_TOOLCHAIN`),
/// 3. **the workspace's Dylint lint libraries** (their shared pin, or a dated
///    nightly deliberately inherited from the root),
/// 4. the workspace root `rust-toolchain.toml` when no lint libraries exist,
/// 5. the version -> nightly map.
///
/// Tier 3 is the fix. Dylint builds one driver per *library* toolchain, so
/// when a workspace has lint libraries they are the authority — deriving a
/// nightly from the root's stable channel (tiers 4 + 5) produced a channel for
/// which no driver had ever been published, and the run died at the driver
/// gate. Libraries that all inherit may use a dated-nightly root, but a stable,
/// undated, or missing root is rejected before map/catalogue access because no
/// driver can be published for it. Tiers 4 and 5 otherwise survive only for a
/// workspace with no lint libraries to read, where derivation remains the best
/// available guess.
fn requested_toolchain_channel(
    requested_channel: Option<&str>,
    workspace_root: &Path,
) -> Result<RequestedChannel, SoldrError> {
    if let Some(channel) = requested_channel
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(RequestedChannel {
            channel: Some(channel.to_string()),
            provenance: ChannelProvenance::Explicit,
        });
    }
    for key in [
        TOOLCHAIN_ENV_VAR,
        CONFIGURED_TOOLCHAIN_ENV_VAR,
        "RUSTUP_TOOLCHAIN",
    ] {
        if let Some(channel) = non_empty_env(key) {
            return Ok(RequestedChannel {
                channel: Some(channel),
                provenance: ChannelProvenance::Environment,
            });
        }
    }
    match crate::dylint_libraries::toolchain_state(workspace_root)? {
        crate::dylint_libraries::LibraryToolchainState::Pinned { channel, .. } => {
            return Ok(RequestedChannel {
                channel: Some(channel),
                provenance: ChannelProvenance::LintLibraries,
            });
        }
        crate::dylint_libraries::LibraryToolchainState::InheritRoot { libraries } => {
            return Ok(RequestedChannel {
                channel: Some(crate::dylint_libraries::inherited_root_channel(
                    workspace_root,
                    &libraries,
                )?),
                provenance: ChannelProvenance::RootManifest,
            });
        }
        crate::dylint_libraries::LibraryToolchainState::NoLibraries => {}
    }
    let manifest = crate::core::read_rust_toolchain_manifest(workspace_root)?;
    Ok(match manifest.channel {
        Some(channel) => RequestedChannel {
            channel: Some(channel),
            provenance: ChannelProvenance::RootManifest,
        },
        None => RequestedChannel {
            channel: None,
            provenance: ChannelProvenance::VersionMap,
        },
    })
}

fn select_explicit_from_map(
    bytes: &[u8],
    requested_channel: &str,
) -> Result<DylintToolchainPlan, SoldrError> {
    let map: NightlyVersionMap = serde_json::from_slice(bytes)
        .map_err(|error| SoldrError::Other(format!("failed to parse {MAP_ASSET}: {error}")))?;
    if map.schema_version != 1 {
        return Err(SoldrError::Other(format!(
            "{MAP_ASSET} has unsupported schema_version {}",
            map.schema_version
        )));
    }
    let map_channel = requested_channel
        .get(..18)
        .filter(|channel| is_dated_nightly(channel))
        .unwrap_or(requested_channel);
    let identity = map.nightlies.get(map_channel).ok_or_else(|| {
        SoldrError::Other(format!(
            "{MAP_ASSET} has no identity for explicitly configured {requested_channel}"
        ))
    })?;
    validate_identity(map_channel, identity)?;
    Ok(DylintToolchainPlan::identity(
        requested_channel.to_string(),
        identity.rustc_release.clone(),
        identity.rustc_commit_hash.clone(),
    ))
}

fn select_from_map(bytes: &[u8], version: &str) -> Result<DylintToolchainPlan, SoldrError> {
    let map: NightlyVersionMap = serde_json::from_slice(bytes)
        .map_err(|error| SoldrError::Other(format!("failed to parse {MAP_ASSET}: {error}")))?;
    if map.schema_version != 1 {
        return Err(SoldrError::Other(format!(
            "{MAP_ASSET} has unsupported schema_version {}",
            map.schema_version
        )));
    }
    let bucket = map.versions.get(version).ok_or_else(|| {
        SoldrError::Other(format!(
            "{MAP_ASSET} has no nightly mapping for Rust {version}"
        ))
    })?;
    if bucket.nightlies.first() != Some(&bucket.selected) {
        return Err(SoldrError::Other(format!(
            "{MAP_ASSET} violates its newest-first contract for Rust {version}"
        )));
    }
    if !bucket.nightlies.windows(2).all(|pair| pair[0] > pair[1]) {
        return Err(SoldrError::Other(format!(
            "{MAP_ASSET} nightlies for Rust {version} are not descending"
        )));
    }
    let identity = map.nightlies.get(&bucket.selected).ok_or_else(|| {
        SoldrError::Other(format!(
            "{MAP_ASSET} selects missing nightly {}",
            bucket.selected
        ))
    })?;
    if identity.rust_version != version {
        return Err(SoldrError::Other(format!(
            "{} is indexed under Rust {version} but reports Rust {}",
            bucket.selected, identity.rust_version
        )));
    }
    validate_identity(&bucket.selected, identity)?;
    Ok(DylintToolchainPlan::identity(
        bucket.selected.clone(),
        identity.rustc_release.clone(),
        identity.rustc_commit_hash.clone(),
    ))
}

fn validate_identity(channel: &str, identity: &NightlyIdentity) -> Result<(), SoldrError> {
    if !channel.starts_with("nightly-")
        || !identity.rustc_release.ends_with("-nightly")
        || identity.rustc_commit_hash.len() != 40
        || !identity
            .rustc_commit_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(SoldrError::Other(format!(
            "{MAP_ASSET} contains a malformed identity for {channel}"
        )));
    }
    Ok(())
}

fn ensure_installed(plan: &DylintToolchainPlan) -> Result<(), SoldrError> {
    let manager_home = crate::dylint_rustup_home::dylint_rustup_home()?;
    ensure_dylint_toolchain_ready_at(&manager_home, &plan.channel, || {
        crate::toolchain::rustup_toolchain_install_with_profile(&plan.channel, Some("minimal"))
    })?;
    let installed = installed_components(&plan.channel).unwrap_or_default();
    for component in REQUIRED_COMPONENTS {
        if !installed.iter().any(|line| line.starts_with(component)) {
            let code = crate::toolchain::rustup_component_add(&plan.channel, component)?;
            if code != 0 {
                return Err(SoldrError::Other(format!(
                    "rustup failed to add {component} to {} (exit {code}). If rustup output above says `detected conflict`, run `soldr rustup toolchain uninstall {}` then rerun `soldr dylint`.",
                    plan.channel, plan.channel
                )));
            }
        }
    }
    Ok(())
}

fn installed_components(channel: &str) -> Result<Vec<String>, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["component", "list", "--toolchain", channel, "--installed"]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command_output_with_timeout(&mut command, "rustup component list for Dylint")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn verify_installed_identity(plan: &DylintToolchainPlan) -> Result<(), SoldrError> {
    let observed = observe_compiler(&plan.channel)?;
    if observed.0 != plan.compiler_release || observed.1 != plan.compiler_commit {
        return Err(SoldrError::Other(format!(
            "installed {} differs from catalogue: expected {} {}, got {} {}",
            plan.channel, plan.compiler_release, plan.compiler_commit, observed.0, observed.1
        )));
    }
    Ok(())
}

fn observe_compiler(channel: &str) -> Result<(String, String), SoldrError> {
    let binary = resolve_toolchain_binary_for_channel(concat!("rust", "c"), Some(channel))?;
    observe_compiler_binary(&binary, channel)
}

fn observe_compiler_binary(
    binary: &Path,
    description: &str,
) -> Result<(String, String), SoldrError> {
    let mut command = std::process::Command::new(binary);
    command.arg("-vV");
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command_output_with_timeout(&mut command, "Dylint compiler identity")?;
    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "{description} identity probe failed with {}",
            output.status
        )));
    }
    parse_compiler_verbose(&String::from_utf8_lossy(&output.stdout))
}

fn observe_compiler_host(binary: &Path, description: &str) -> Result<String, SoldrError> {
    let mut command = std::process::Command::new(binary);
    command.arg("-vV");
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command_output_with_timeout(&mut command, "Dylint compiler host identity")?;
    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "{description} host identity probe failed with {}",
            output.status
        )));
    }
    parse_compiler_host(&String::from_utf8_lossy(&output.stdout))
}

fn parse_compiler_verbose(output: &str) -> Result<(String, String), SoldrError> {
    let mut release = None;
    let mut commit = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("release:") {
            release = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("commit-hash:") {
            commit = Some(value.trim().to_string());
        }
    }
    match (release, commit) {
        (Some(release), Some(commit)) if commit.len() == 40 => Ok((release, commit)),
        _ => Err(SoldrError::Other(
            "compiler identity lacks a release or full commit hash".into(),
        )),
    }
}

fn parse_compiler_host(output: &str) -> Result<String, SoldrError> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("host:").map(str::trim))
        .filter(|host| !host.is_empty())
        .map(str::to_string)
        .ok_or_else(|| SoldrError::Other("compiler identity lacks a host triple".into()))
}

fn major_minor(value: &str) -> Option<String> {
    let value = value
        .strip_prefix(concat!("rust", "c", " "))
        .unwrap_or(value);
    let version = value
        .split_whitespace()
        .next()?
        .trim_end_matches("-nightly");
    let mut parts = version.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    (major.bytes().all(|byte| byte.is_ascii_digit())
        && minor.bytes().all(|byte| byte.is_ascii_digit()))
    .then(|| format!("{major}.{minor}"))
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
#[path = "dylint_toolchain_tests.rs"]
mod tests;
