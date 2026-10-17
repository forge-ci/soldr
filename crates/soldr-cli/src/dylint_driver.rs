//! The Dylint *driver* half of what used to be one `dylint_toolchain.rs`
//! (soldr#2945).
//!
//! `dylint_toolchain.rs` answers a single question — *which nightly does this
//! workspace's Dylint run against, and who decided that?* — and hands back a
//! [`DylintToolchainPlan`]. Everything in this file starts where that answer
//! ends: given a settled plan, it locates the matching `dylint-driver` binary
//! on disk, proves the binary is the one the pinned cargo-dylint expects,
//! fetches it from the soldr-toolchain catalogue when it is absent, and builds
//! the runtime environment (`PATH` / `LD_LIBRARY_PATH` / `DYLD_LIBRARY_PATH`)
//! the driver needs to load the nightly's `rustc_private` shared libraries.
//! The two halves share only the plan type and the "is this a fully-qualified
//! nightly name?" predicate, which is what made the seam obvious once the file
//! had to be cut.
//!
//! The cut itself was forced: `.github/scripts/loc_ceiling.py` enforces a
//! **hard** 1,000-physical-line ceiling on every production source under
//! `crates/*/src/`, and unlike `loc_ratchet.py` there is no grandfathering and
//! no "it was already over" escape — a violating file fails the `Lint` job
//! whether or not the PR touched it. soldr#2945 took `dylint_toolchain.rs` from
//! 987 lines to 1,265 by threading `ChannelProvenance` through the resolver
//! and rewriting the driver-gate diagnostic, which put it 265 lines over. This
//! module is that overage, moved rather than trimmed: nothing here changed
//! behaviour on the way across.
//!
//! Three policies live here and are easy to break by accident, so they are
//! stated once, loudly:
//!
//! * **Binary-or-exit, amended by a cached local build (soldr#2432/#2484,
//!   soldr#2349).** Upstream cargo-dylint silently builds a missing or stale
//!   driver from source, uncached, every time. Soldr's original policy was to
//!   refuse outright; [`require_prebuilt_driver`] preflights the *same*
//!   version signal cargo-dylint would have checked so the refusal happens at
//!   Soldr's front door rather than several minutes into a nested build.
//!   soldr#2349 amends "refuse" to "resolve in two tiers": the catalogue
//!   prebuilt stays tier 1, unchanged; when it has no asset for this
//!   (nightly, host) pair — routine on macOS and Windows, since the catalogue
//!   does not backfill every dated nightly for every triple —
//!   [`ensure_prebuilt_driver`] now builds the driver itself as tier 2,
//!   *once*, into the same `driver_root` layout, behind a schema-versioned
//!   marker keyed on the exact compiler + cargo-dylint + host identity (see
//!   [`DylintDriverBuildMarker`]) so every later resolution with a matching
//!   marker and a still-probe-verified binary skips the build entirely. This
//!   is on by default — `SOLDR_DYLINT_DRIVER_FALLBACK=off` restores the
//!   original strict binary-or-exit behavior for hosts that must stay
//!   prebuilt-only. `wrapper::ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR` is
//!   unchanged and now tier 3: the pre-existing escape that lets
//!   cargo-dylint build the driver inline, uncached, the moment it runs —
//!   reached only when tier 2 declined or failed. It is deliberately the same
//!   variable the nested-build guard in `wrapper.rs` honours, so a user who
//!   turns that policy off is not stopped a second time by a guard that
//!   disagrees about which switch turns it off.
//! * **`DYLINT_DRIVER_PATH` belongs to the caller when the caller sets it.**
//!   Every path decision in this file consults it first and never clobbers it.
//! * **A local build's trust is `unverified`, and `SOLDR_TRUST_MODE=strict`
//!   refuses it (soldr#2349).** See the doc comment on
//!   [`refuse_local_build_under_strict_trust`] for the full rationale — in
//!   short, strict's contract is "matches a pinned sha256 of the installed
//!   bytes", and a local build's output has no such pin even though its
//!   top-level input version does.
//!
//! Unit coverage for this module lives in `dylint_toolchain_tests.rs` beside
//! the resolver tests it shares its `EnvVarGuard` / sample-plan scaffolding
//! with, and reaches these items through `crate::dylint_driver::…`. The
//! soldr#2349 local-build-fallback additions have their own coverage in the
//! sibling `dylint_driver_tests.rs`, included at the bottom of this file.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use wait_timeout::ChildExt;

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths, TargetTriple};
use crate::dylint_toolchain::{is_fully_qualified_nightly, DylintToolchainPlan};
use crate::resolve_toolchain_binary_for_channel;

/// Why the bounded `-V` probe did not hand back a usable driver.
///
/// The distinction exists because the two verdicts want opposite responses
/// (soldr#2436 vs. soldr#2349). `NoUsableDriver` means the driver soldr
/// needs is not on this host in a form it can use — exactly the condition
/// a fetch or a build cures. `ProbeTimedOut` means a driver binary *is* there and was
/// executing, and soldr had to kill it to stay inside the deadline; the
/// host, not the artifact, is what failed to answer, and producing another
/// artifact cannot make it answer. Each escalation attempted on that
/// verdict is pure added latency on top of a failure soldr#2436 requires to
/// be bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriverProbeVerdict {
    /// No driver binary at the expected path, one that could not be
    /// spawned at all, or one that answered `-V` with a nonzero exit,
    /// unreadable output, or the wrong version.
    NoUsableDriver,
    /// A driver binary exists and ran, but did not complete the `-V` probe
    /// inside its deadline (or the wait itself failed), so soldr killed and
    /// reaped it.
    ProbeTimedOut,
}

/// Whether a failed probe should escalate to the tier-2 local driver build
/// (soldr#2349). See [`DriverProbeVerdict`] for why a timed-out probe must
/// not.
pub(crate) fn local_build_fallback_applies(verdict: DriverProbeVerdict) -> bool {
    match verdict {
        DriverProbeVerdict::NoUsableDriver => true,
        DriverProbeVerdict::ProbeTimedOut => false,
    }
}

/// Refuse to launch cargo-dylint unless its exact toolchain driver already
/// exists and answers the bounded version probe. Upstream cargo-dylint builds
/// this driver automatically when it is absent or stale; preflighting the same
/// version signal is what makes Soldr's binary-or-exit policy cover the driver,
/// not just the two CLI executables.
pub(crate) fn require_prebuilt_driver(
    plan: &DylintToolchainPlan,
    paths: &SoldrPaths,
) -> Result<PathBuf, SoldrError> {
    probe_prebuilt_driver(plan, paths).map_err(|(_, error)| error)
}

/// [`require_prebuilt_driver`] plus the [`DriverProbeVerdict`] its callers
/// need to decide whether escalating to a fetch or a build can help.
pub(crate) fn probe_prebuilt_driver(
    plan: &DylintToolchainPlan,
    paths: &SoldrPaths,
) -> Result<PathBuf, (DriverProbeVerdict, SoldrError)> {
    let unusable = |error: SoldrError| (DriverProbeVerdict::NoUsableDriver, error);
    let driver_dir = driver_root(paths).join(qualified_driver_channel(plan).map_err(unusable)?);
    let driver = ["dylint-driver", "dylint-driver.exe", "dylint-driver.cmd"]
        .into_iter()
        .map(|name| driver_dir.join(name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            unusable(unavailable_driver_error(
                plan,
                &driver_dir,
                "no driver binary at that path",
            ))
        })?;

    let mut command = std::process::Command::new(&driver);
    command
        .arg("-V")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_driver_runtime_environment(&mut command, plan).map_err(unusable)?;
    suppress_windows_console_window(&mut command);
    // soldr#3098: spawns share, staged writes exclude.
    let spawned = {
        let _spawn = crate::core::spawn_exclusion::spawn_shared();
        command.spawn()
    };
    let mut child = spawned.map_err(|error| {
        unusable(unavailable_driver_error(
            plan,
            &driver_dir,
            &format!("version probe could not start: {error}"),
        ))
    })?;
    // Both non-`Ok(Some(_))` arms below terminate a *running* driver, which
    // is what makes them `ProbeTimedOut` rather than "no driver": the
    // binary is present and was executing, so nothing soldr could fetch or
    // build would change the outcome.
    let status = match child.wait_timeout(Duration::from_secs(2)) {
        Err(error) => {
            let _ = child.kill();
            let _ = crate::platform::wait::wait_for(&mut child, crate::platform::wait::PROBE);
            return Err((
                DriverProbeVerdict::ProbeTimedOut,
                unavailable_driver_error(
                    plan,
                    &driver_dir,
                    &format!("version probe failed: {error}"),
                ),
            ));
        }
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.kill();
            let _ = crate::platform::wait::wait_for(&mut child, crate::platform::wait::PROBE);
            return Err((
                DriverProbeVerdict::ProbeTimedOut,
                unavailable_driver_error(
                    plan,
                    &driver_dir,
                    "version probe exceeded the 2-second deadline",
                ),
            ));
        }
    };
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout).map_err(|error| {
            unusable(unavailable_driver_error(
                plan,
                &driver_dir,
                &format!("version probe output failed: {error}"),
            ))
        })?;
    }
    if !status.success() {
        return Err(unusable(unavailable_driver_error(
            plan,
            &driver_dir,
            &format!("version probe exited with {status}"),
        )));
    }
    let expected = crate::fetch::known_tools::lookup_by_crate("cargo-dylint")
        .and_then(|spec| spec.pinned_version)
        .ok_or_else(|| {
            unusable(SoldrError::Other(
                "cargo-dylint must have a registry pin".into(),
            ))
        })?;
    let actual = dylint_driver_version(&stdout);
    if actual != Some(expected) {
        return Err(unusable(unavailable_driver_error(
            plan,
            &driver_dir,
            &format!("driver version is {actual:?}, expected {expected:?}"),
        )));
    }
    Ok(driver)
}

/// Where cargo-dylint keeps its per-toolchain drivers. An explicit
/// `DYLINT_DRIVER_PATH` always wins; otherwise soldr owns the location.
fn driver_root(paths: &SoldrPaths) -> PathBuf {
    std::env::var_os("DYLINT_DRIVER_PATH")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.root.join("dylint").join("drivers"))
}

/// cargo-dylint's driver directory name: `<nightly>-<host triple>`.
fn qualified_driver_channel(plan: &DylintToolchainPlan) -> Result<String, SoldrError> {
    if is_fully_qualified_nightly(&plan.channel) {
        return Ok(plan.channel.clone());
    }
    Ok(format!(
        "{}-{}",
        plan.channel,
        TargetTriple::host()?.triple()
    ))
}

fn apply_driver_runtime_environment(
    command: &mut std::process::Command,
    plan: &DylintToolchainPlan,
) -> Result<(), SoldrError> {
    apply_driver_runtime_environment_impl(command, plan)
}

fn dylint_toolchain_dirs(plan: &DylintToolchainPlan) -> Result<(PathBuf, PathBuf), SoldrError> {
    let rustc = resolve_toolchain_binary_for_channel("rustc", Some(&plan.channel))?;
    let bin_dir = rustc
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| SoldrError::Other("Dylint rustc has no parent directory".into()))?;
    let toolchain_root = bin_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| SoldrError::Other("Dylint rustc has no toolchain root".into()))?;
    Ok((bin_dir, toolchain_root))
}

fn apply_driver_runtime_environment_impl(
    command: &mut std::process::Command,
    plan: &DylintToolchainPlan,
) -> Result<(), SoldrError> {
    match crate::platform::host::facts::os() {
        crate::platform::host::facts::HostOs::Windows => {
            let (bin_dir, _) = dylint_toolchain_dirs(plan)?;
            prepend_command_paths(command, "PATH", &[bin_dir])
        }
        crate::platform::host::facts::HostOs::Linux => {
            let (_, toolchain_root) = dylint_toolchain_dirs(plan)?;
            let mut library_dirs = vec![toolchain_root.join("lib")];
            if let Some(nix_library_path) = std::env::var_os("NIX_LD_LIBRARY_PATH") {
                library_dirs.extend(
                    std::env::split_paths(&nix_library_path)
                        .filter(|path| !path.as_os_str().is_empty()),
                );
            }
            prepend_command_paths(command, "LD_LIBRARY_PATH", &library_dirs)?;
            Ok(())
        }
        crate::platform::host::facts::HostOs::MacOs => {
            let (_, toolchain_root) = dylint_toolchain_dirs(plan)?;
            prepend_command_paths(command, "DYLD_LIBRARY_PATH", &[toolchain_root.join("lib")])?;
            Ok(())
        }
    }
}

fn prepend_command_paths(
    command: &mut std::process::Command,
    key: &str,
    directories: &[PathBuf],
) -> Result<(), SoldrError> {
    let existing = std::env::var_os(key);
    let paths = directories.iter().cloned().chain(
        existing
            .as_deref()
            .map(std::env::split_paths)
            .into_iter()
            .flatten(),
    );
    let joined = std::env::join_paths(paths)
        .map_err(|error| SoldrError::Other(format!("failed to construct Dylint {key}: {error}")))?;
    command.env(key, joined);
    Ok(())
}

/// Resolve a missing driver from soldr-toolchain before cargo-dylint can
/// trigger its implicit source-build fallback.
///
/// Binary-or-exit stays the default (soldr#2432/#2484). soldr#2945 adds the
/// escape hatch the nested wrapper guard already honoured — the *same*
/// variable, so a user who sets it once is not stopped twice by two guards
/// that disagree about which switch turns them off.
pub(crate) async fn ensure_prebuilt_driver(
    plan: &DylintToolchainPlan,
    paths: &SoldrPaths,
) -> Result<(), SoldrError> {
    // soldr#2436 vs. soldr#2349: classify the entry probe, because the two failure
    // verdicts want opposite responses. A driver that is missing (or
    // answers with the wrong identity) is what the fetch + build tiers
    // below exist to cure. A driver that blew the bounded `-V` deadline is
    // not: the binary is present and was executing, soldr killed it, and
    // neither re-downloading the same asset nor compiling a fresh one makes
    // an unresponsive host answer. Each escalation only extends a failure
    // soldr#2436 requires to be bounded — the local build tier alone costs
    // a `rustup component add` of `rustc-dev` plus a full `rustc_private`
    // compile. So a timed-out probe is terminal, and the caller sees the
    // hung-probe diagnostic instead of waiting minutes for the same verdict.
    let (verdict, probe_error) = match probe_prebuilt_driver(plan, paths) {
        Ok(_) => return Ok(()),
        Err(failure) => failure,
    };
    if !local_build_fallback_applies(verdict) {
        return Err(probe_error);
    }
    let version = crate::fetch::known_tools::lookup_by_crate("cargo-dylint")
        .and_then(|spec| spec.pinned_version)
        .ok_or_else(|| SoldrError::Other("cargo-dylint must have a registry pin".into()))?;
    let root = driver_root(paths);
    let fetched = crate::fetch::ensure_dylint_driver(paths, version, &plan.channel, &root).await;
    let catalogue_outcome = match fetched {
        Ok(_) => probe_prebuilt_driver(plan, paths).map(|_| ()),
        // A catalogue/transport failure is just as much "no driver" as a
        // catalogue miss, and both fall through to the local-build fallback
        // below the same way they used to fall straight through to the
        // opt-in.
        Err(error) => Err((DriverProbeVerdict::NoUsableDriver, error)),
    };
    let (verdict, catalogue_error) = match catalogue_outcome {
        Ok(()) => return Ok(()),
        Err(failure) => failure,
    };
    // Same terminal verdict, now for a driver the catalogue just installed:
    // a freshly staged binary that hangs its own probe is still a hung
    // host, not a build input the local tier can improve on.
    if !local_build_fallback_applies(verdict) {
        return Err(catalogue_error);
    }

    // Tier 2 (soldr#2349): the catalogue has no asset for this nightly +
    // host — routine on macOS/Windows, where the catalogue does not
    // backfill every dated nightly for every triple. Build the driver once,
    // cache it behind a marker keyed on the exact inputs, and reuse it on
    // every later resolution instead of dead-ending, or (tier 3) paying an
    // uncached inline build on every single invocation.
    match ensure_local_driver_build(plan, paths, version) {
        Ok(()) => return Ok(()),
        Err(error) => {
            eprintln!(
                "soldr: local Dylint driver build fallback did not produce a driver: {error}"
            );
        }
    }

    // Tier 3: the pre-existing opt-in (soldr#2432/#2484), unchanged.
    // cargo-dylint builds the driver itself, inline, the moment it runs,
    // uncached. Reached only when tier 2 declined (fallback disabled,
    // refused under strict trust) or genuinely failed (no network, missing
    // rustc-dev, a build error).
    if crate::wrapper::allow_dylint_driver_build() {
        eprintln!(
            "{}",
            driver_source_build_warning(plan, version, &catalogue_error)
        );
        Ok(())
    } else {
        Err(catalogue_error)
    }
}

pub(crate) fn dylint_driver_version(stdout: &str) -> Option<&str> {
    stdout
        .split_whitespace()
        .last()
        .filter(|value| !value.is_empty())
}

fn host_triple_or_unknown() -> String {
    TargetTriple::host()
        .map(|target| target.triple())
        .unwrap_or_else(|_| "unknown-host".to_string())
}

/// The catalogue asset that would satisfy this plan:
/// `dylint-driver <version>-<dated nightly>` for the host triple.
fn missing_driver_asset(plan: &DylintToolchainPlan, version: &str) -> String {
    format!(
        "dylint-driver {version}-{} for {}",
        crate::dylint_libraries::canonical_channel(&plan.channel),
        host_triple_or_unknown()
    )
}

/// soldr#2945: this diagnostic used to open with "Dylint v6.0.3 is not built
/// for this machine" and close by advising the reader to "select a Dylint
/// version that provides <host> prebuilts". Both were usually false. Dylint
/// 6.0.3 publishes driver assets for all eight supported triples; what is
/// actually missing is a driver for the *nightly* that was selected, and the
/// fix depends entirely on which tier selected it. So the message leads with
/// the channel and its provenance, names the exact asset it looked for and the
/// path it looked at, and blames neither the host nor the Dylint version.
pub(crate) fn unavailable_driver_error(
    plan: &DylintToolchainPlan,
    driver_dir: &Path,
    reason: &str,
) -> SoldrError {
    let version = crate::fetch::known_tools::lookup_by_crate("cargo-dylint")
        .and_then(|spec| spec.pinned_version)
        .unwrap_or("unknown");
    SoldrError::Other(format!(
        "no usable Dylint driver for {channel} (channel selected from {provenance}). \
         Cause: {reason}. Missing asset: {asset}; expected on disk under {directory}. \
         Soldr will not build the driver from source unless asked (binary-or-exit, \
         soldr#2432/#2484). Corrective action: select a nightly the catalogue publishes a \
         Dylint v{version} driver for — when the channel came from the lint libraries that \
         means re-pinning every `rust-toolchain.toml` under \
         workspace.metadata.dylint.libraries — or point DYLINT_DRIVER_PATH at a directory \
         that already holds a v{version} driver for {channel}, or set \
         {opt_in}=1 to permit a one-time driver source build.",
        channel = plan.channel,
        provenance = plan.provenance.describe(),
        asset = missing_driver_asset(plan, version),
        directory = driver_dir.display(),
        opt_in = crate::wrapper::ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR,
    ))
}

/// The loud counterpart to [`unavailable_driver_error`] for the opt-in path.
/// A driver source build is not a small thing — it installs `rustc-dev` for
/// the nightly and takes minutes — so proceeding silently would be worse than
/// the refusal it replaces.
pub(crate) fn driver_source_build_warning(
    plan: &DylintToolchainPlan,
    version: &str,
    reason: &SoldrError,
) -> String {
    format!(
        "soldr: WARNING: no prebuilt Dylint driver for {channel} (channel selected from \
         {provenance}); proceeding because {opt_in} is set. Missing asset: {asset} ({reason}). \
         cargo-dylint will now build the driver from source: that installs the rustc-dev \
         component for {channel} and takes minutes, and Soldr does not cache the result. \
         Unset {opt_in} to restore the binary-or-exit default (soldr#2432/#2484).",
        channel = plan.channel,
        provenance = plan.provenance.describe(),
        opt_in = crate::wrapper::ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR,
        asset = missing_driver_asset(plan, version),
    )
}

/// Give the dylint driver cargo-dylint builds a stable soldr-owned
/// home instead of the tool's own unmanaged default (normally
/// `~/.dylint_drivers` or wherever `$DYLINT_DRIVER_PATH` happens to
/// point). A fixed path means warm runs reuse the already-built
/// driver and CI caches have something deterministic to restore.
/// Respects an explicit caller-set `DYLINT_DRIVER_PATH` — soldr never
/// clobbers a user override.
pub(crate) fn apply_dylint_driver_path(command: &mut std::process::Command) {
    if std::env::var_os("DYLINT_DRIVER_PATH").is_some() {
        return;
    }
    let Ok(paths) = crate::core::SoldrPaths::new() else {
        return;
    };
    let driver_dir = paths.root.join("dylint").join("drivers");
    if std::fs::create_dir_all(&driver_dir).is_err() {
        // Best-effort: if the directory cannot be created, fall back
        // to the tool's own default rather than pointing at a
        // nonexistent path.
        return;
    }
    command.env("DYLINT_DRIVER_PATH", driver_dir);
}

mod local_build;
use local_build::*;

#[cfg(test)]
#[path = "dylint_driver_tests.rs"]
mod tests;
