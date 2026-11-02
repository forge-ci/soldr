//! Tier 2 of the Dylint driver resolution ladder: a cached local build of
//! `dylint_driver` (soldr#2349).
//!
//! Split out of `dylint_driver.rs` by `.github/scripts/loc_ceiling.py`'s
//! hard 1,000-physical-line ceiling — this module is the moved tier-2
//! implementation, not a rewrite; behaviour is unchanged from before the
//! cut. See `dylint_driver.rs`'s module doc for the three-policy overview
//! (binary-or-exit amended by this cached local build, `DYLINT_DRIVER_PATH`
//! caller ownership, and strict-trust refusal) and
//! [`super::ensure_prebuilt_driver`] for how this tier is reached. Unit
//! coverage lives in `dylint_driver_tests.rs`, included as a submodule of
//! the parent `dylint_driver` so its `use super::*;` still resolves these
//! items via the parent's `use local_build::*;` re-export.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths, TargetTriple};
use crate::dylint_toolchain::DylintToolchainPlan;
use crate::resolve_toolchain_binary_for_channel;

use super::{
    driver_root, dylint_toolchain_dirs, qualified_driver_channel, require_prebuilt_driver,
};

// ---------------------------------------------------------------------
// Tier 2: cached local driver build (soldr#2349).
//
// Resolution order implemented by `ensure_prebuilt_driver`:
//   1. catalogue prebuilt asset (unchanged, sha256-verified fetch + `-V`
//      identity probe);
//   2. this section — a cached local build, attempted at most once per
//      (nightly identity, cargo-dylint version, host triple);
//   3. the pre-existing `SOLDR_ALLOW_DYLINT_DRIVER_BUILD` opt-in, unchanged.
// ---------------------------------------------------------------------

/// The exact input set that produced an installed local-build driver.
///
/// Schema-versioned like `dylint_cook.rs`'s `DylintCookMarker`, for the same
/// reason: a future change to the key set (or the install layout) must be
/// able to tell an old marker from a new one instead of misreading it.
/// `schema_version` participates in equality (via `derive(PartialEq)`) so a
/// stale-schema marker on disk never satisfies a fresh key by accident.
///
/// The key set is deliberately everything `require_prebuilt_driver`'s `-V`
/// probe cannot itself observe cheaply: `channel` + `compiler_release` +
/// `compiler_commit` identify the exact nightly (mirroring
/// `DylintToolchainPlan`'s own `PartialEq`), `cargo_dylint_version` guards a
/// cargo-dylint version bump landing while `driver_root`'s per-channel
/// directory name stays the same, and `host_triple` guards a marker
/// surviving a copy (or a shared, non-host-scoped `DYLINT_DRIVER_PATH`)
/// onto a different machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct DylintDriverBuildMarker {
    pub(super) schema_version: u32,
    pub(super) channel: String,
    pub(super) compiler_release: String,
    pub(super) compiler_commit: String,
    pub(super) cargo_dylint_version: String,
    pub(super) host_triple: String,
}

pub(super) const DRIVER_BUILD_MARKER_SCHEMA_VERSION: u32 = 1;
pub(super) const DRIVER_BUILD_MARKER_NAME: &str = ".soldr-dylint-driver-build-v1.json";
pub(super) const DRIVER_BUILD_LOCK_NAME: &str = ".soldr-dylint-driver-build.lock";
const DRIVER_BUILD_TIMEOUT_ENV_VAR: &str = "SOLDR_DYLINT_DRIVER_BUILD_TIMEOUT_SECS";

/// Opt-out for the local-build fallback (soldr#2349). Owned, default **on**:
/// this env var absent, or set to anything but an explicit off spelling,
/// permits the fallback. An explicit off spelling (`0`/`false`/`no`/`off`,
/// [`crate::core::is_off_value`]) disables it — for a locked-down host that
/// must stay prebuilt-only (no network access to crates.io, or a policy
/// against on-host compilation of `rustc_private` code), which then falls
/// straight through to tier 3's existing refusal/opt-in.
///
/// This is a *default-on* switch, so it is read via `is_off_value` rather
/// than [`crate::core::flag`]: `flag`'s allowlist defaults an unrecognised
/// value to *off*, which would invert this switch's intended default (the
/// same distinction `env_flag`'s module docs draw between an owned
/// default-off switch and an owned default-on one).
pub(super) const DYLINT_DRIVER_FALLBACK_ENV_VAR: &str = "SOLDR_DYLINT_DRIVER_FALLBACK";

pub(super) fn local_driver_build_fallback_enabled() -> bool {
    match std::env::var(DYLINT_DRIVER_FALLBACK_ENV_VAR) {
        Ok(value) => !crate::core::is_off_value(&value),
        Err(_) => true,
    }
}

/// Whether `SOLDR_TRUST_MODE=strict` permits the local-build fallback, and
/// why it does not.
///
/// **Decision: strict refuses it.** `crates/soldr-fetch/src/fetch/trust.rs`
/// defines strict as "every installed artifact matches a pinned sha256" — a
/// promise about the *output bytes*, not the inputs that produced them. A
/// catalogue driver satisfies that today: its sha256 is computed over the
/// exact bytes soldr-toolchain published, and a pin (or the catalogue's own
/// manifest verification) can hold that hash accountable indefinitely.
///
/// A locally built driver cannot make the same promise, even though its
/// *top-level* input is pinned exactly as tightly as the catalogue's:
/// `dylint_driver = "={version}"` in [`driver_build_cargo_toml`] is the same
/// exact-match spec upstream cargo-dylint's own `driver_builder` uses. What
/// is unpinned is everything downstream of that one line — the transitive
/// dependency graph resolves at build time with no lockfile soldr records or
/// verifies, and the resulting binary's bytes vary with the host toolchain
/// build, timestamps, and embedded rpath. There is no stable hash to pin, so
/// calling it "verified" would silently redefine what strict means for this
/// one code path — from "matches a recorded hash" to "resolved from a pinned
/// top-level version". A caller who set `SOLDR_TRUST_MODE=strict`
/// specifically to require hash-verified artifacts should get exactly that
/// guarantee everywhere it applies, or a loud, named refusal on the one path
/// that cannot provide it — not a quietly weaker guarantee wearing the same
/// label.
///
/// Strict users are not stuck: `DYLINT_DRIVER_PATH` (this file's other
/// standing policy — "belongs to the caller when the caller sets it") still
/// accepts a driver placed there by whatever verified-supply-chain process
/// the strict policy trusts, and the catalogue tier is completely unaffected
/// by this decision.
pub(super) fn refuse_local_build_under_strict_trust() -> Result<(), SoldrError> {
    if crate::fetch::TrustMode::from_env() == crate::fetch::TrustMode::Strict {
        return Err(SoldrError::Other(format!(
            "local Dylint driver build fallback refused under {trust_var}=strict: a local \
             build's output cannot be sha256-pinned the way a catalogue asset can (see the doc \
             comment on `refuse_local_build_under_strict_trust` in dylint_driver.rs for the \
             full rationale). Point DYLINT_DRIVER_PATH at a driver from a verified supply \
             chain, or drop {trust_var}=strict for this run.",
            trust_var = crate::fetch::TRUST_MODE_ENV_VAR,
        )));
    }
    Ok(())
}

/// Components the temporary driver-build package needs for the pinned
/// nightly. Mirrors upstream cargo-dylint's own recipe (trailofbits/dylint
/// v6.0.3, `dylint/src/driver_builder.rs`): that crate's own temporary
/// package carries a `rust-toolchain` file declaring these same two
/// components and lets rustup's proxy auto-install them lazily. Soldr
/// instead invokes the resolved toolchain's `cargo` binary directly
/// (bypassing the rustup proxy, consistent with [`dylint_toolchain_dirs`]),
/// so that auto-install never fires; ensuring the components explicitly
/// first is the direct replacement for it.
pub(super) const DRIVER_BUILD_COMPONENTS: &[&str] = &["rustc-dev", "llvm-tools-preview"];

/// Package (and `[[bin]]`) name for the temporary crate soldr builds
/// `dylint_driver` inside, chosen so the compiled output has a predictable
/// path regardless of hyphen/underscore mangling.
pub(super) const DRIVER_BUILD_PACKAGE_NAME: &str = "soldr-dylint-driver-build";

/// The temporary package's `Cargo.toml`. Structurally mirrors upstream
/// cargo-dylint's own `driver_builder::cargo_toml` template — same
/// dependency shape, same exact-version pin on `dylint_driver` — because the
/// binary this produces is upstream's own published driver crate, not a
/// soldr reimplementation of it.
pub(super) fn driver_build_cargo_toml(cargo_dylint_version: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2018\"\npublish = false\n\n\
         [[bin]]\nname = \"{name}\"\npath = \"src/main.rs\"\n\n\
         [dependencies]\nanyhow = \"1.0\"\nenv_logger = \"0.11\"\ndylint_driver = \"={version}\"\n",
        name = DRIVER_BUILD_PACKAGE_NAME,
        version = cargo_dylint_version,
    )
}

/// Verbatim (structurally) match of upstream cargo-dylint's own driver
/// `main.rs` (trailofbits/dylint v6.0.3, `dylint/src/driver_builder.rs`):
/// the published `dylint_driver` crate's entry point is
/// `dylint_driver::dylint_driver(&args)`, so the binary this builds is
/// upstream's own driver, not a soldr reimplementation of it.
pub(super) const DRIVER_BUILD_MAIN_RS: &str = r#"#![feature(rustc_private)]

use anyhow::Result;
use std::env;

pub fn main() -> Result<()> {
    env_logger::init();
    let args: Vec<_> = env::args_os().collect();
    dylint_driver::dylint_driver(&args)
}
"#;

/// Compile the pinned `dylint_driver` crate from source into a temporary
/// package, mirroring upstream cargo-dylint's own build recipe (see
/// [`DRIVER_BUILD_COMPONENTS`] and [`DRIVER_BUILD_MAIN_RS`]), then install
/// the result into `driver_dir` and return the installed path. The build
/// keeps the temporary package directory alive for its whole duration so the
/// freshly built binary can be copied out before the directory is dropped.
fn build_and_install_driver_from_source(
    plan: &DylintToolchainPlan,
    driver_dir: &Path,
    qualified_channel: &str,
    cargo_dylint_version: &str,
) -> Result<PathBuf, SoldrError> {
    for component in DRIVER_BUILD_COMPONENTS {
        let code = crate::toolchain::rustup_component_add(&plan.channel, component)?;
        if code != 0 {
            return Err(SoldrError::Other(format!(
                "local Dylint driver build: `rustup component add --toolchain {} {component}` \
                 exited with {code}",
                plan.channel,
            )));
        }
    }

    let tempdir = tempfile::tempdir()
        .map_err(|error| SoldrError::Other(format!("driver build tempdir failed: {error}")))?;
    let package = tempdir.path();
    std::fs::write(
        package.join("Cargo.toml"),
        driver_build_cargo_toml(cargo_dylint_version),
    )?;
    let src = package.join("src");
    std::fs::create_dir_all(&src)?;
    std::fs::write(src.join("main.rs"), DRIVER_BUILD_MAIN_RS)?;

    let cargo = resolve_toolchain_binary_for_channel("cargo", Some(&plan.channel))?;
    let (_, toolchain_root) = dylint_toolchain_dirs(plan)?;

    let mut command = std::process::Command::new(&cargo);
    command.arg("build").current_dir(package);
    crate::binaries::apply_resolved_toolchain_homes(&mut command, &cargo);
    crate::binaries::apply_managed_cargo_home_if_available(&mut command);
    // This build must never re-enter Soldr's own rustc wrapper: a wrapper
    // inherited from an ambient `soldr cargo` session would route this
    // build's rustc invocations back through `wrapper.rs`, and
    // `nested_dylint_driver_build` refuses exactly this crate name unless
    // the tier-3 opt-in is set (soldr#2432/#2484). This build *is* the
    // sanctioned, cached, one-time source build that opt-in exists to gate
    // — it does not need to pass through that guard a second time, so it
    // runs outside the wrapper entirely rather than setting the opt-in
    // globally, which would also loosen unrelated nested builds elsewhere
    // in the same process.
    command
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    let rustc = resolve_toolchain_binary_for_channel("rustc", Some(&plan.channel))?;
    pin_driver_build_toolchain(&mut command, &plan.channel, &rustc);
    // Unix only: bake the *local* toolchain's lib directory into the
    // driver's rpath, mirroring upstream's own `-C link-args=-Wl,-rpath,...`
    // RUSTFLAGS. Unlike the catalogue-fetched driver — built on a CI machine
    // whose rustup layout the install host does not share (soldr#2634) —
    // this binary is built and run on the same host, so the rpath is
    // correct by construction. `install_local_driver` additionally wraps it
    // in the same loader-env script the catalogue path uses, so a
    // `RUSTUP_HOME` relocated between build and run is still covered.
    // Skipped on Windows: DLL resolution there is PATH-based rather than
    // rpath-based, and MSVC's linker does not accept GNU `-Wl,` syntax.
    command.env_remove("RUSTFLAGS");
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        let ambient = std::env::var("RUSTFLAGS").unwrap_or_default();
        let rustflags = format!(
            "{ambient} -C link-args=-Wl,-rpath,{}",
            toolchain_root.join("lib").display()
        );
        command.env("RUSTFLAGS", rustflags);
    }
    suppress_windows_console_window(&mut command);

    let status = crate::exit_guard::run_child_command(
        &mut command,
        &format!("dylint driver build for {}", plan.channel),
        "dylint-driver-build",
        crate::core::InstallerWatchdogConfig::from_env(DRIVER_BUILD_TIMEOUT_ENV_VAR),
    )?;
    if !status.success() {
        return Err(SoldrError::Other(format!(
            "local Dylint driver build failed for {} ({status})",
            plan.channel
        )));
    }

    let built = package.join("target").join("debug").join(format!(
        "{DRIVER_BUILD_PACKAGE_NAME}{}",
        std::env::consts::EXE_SUFFIX
    ));
    if !built.is_file() {
        return Err(SoldrError::Other(format!(
            "local Dylint driver build exited 0 but no binary at {}",
            built.display()
        )));
    }
    install_local_driver(&built, driver_dir, qualified_channel)
}

/// Pin the driver build to the plan's nightly (soldr#4002). The resolved
/// `cargo` is the nightly's own binary, but it runs `rustc` through rustup's
/// proxy, and an ambient `RUSTUP_TOOLCHAIN` (setup-soldr exports the project's
/// stable pin) would hand `dylint_driver`'s `#![feature]` crates to stable and
/// fail with E0554. Naming the nightly's `rustc` and toolchain explicitly
/// means the build either uses the nightly or fails on it, never on stable.
pub(super) fn pin_driver_build_toolchain(
    command: &mut std::process::Command,
    channel: &str,
    rustc: &Path,
) {
    command.env("RUSTUP_TOOLCHAIN", channel).env("RUSTC", rustc);
}

/// Install a freshly built driver into `driver_dir`, using the exact same
/// extensionless-name convention and Unix loader-env wrapper the catalogue
/// path installs under (`toolchain_packaged::install_extensionless_driver`,
/// soldr#2634) — replicated here rather than called because that function is
/// private to the soldr-fetch crate. Keeping the on-disk shape identical
/// means whatever already knows how to exec a catalogue-fetched driver execs
/// a locally built one exactly the same way.
fn install_local_driver(
    built_binary: &Path,
    driver_dir: &Path,
    qualified_channel: &str,
) -> Result<PathBuf, SoldrError> {
    std::fs::create_dir_all(driver_dir)?;
    let destination = driver_dir.join("dylint-driver");
    let payload_destination =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            destination.clone()
        } else {
            driver_dir.join("dylint-driver-real")
        };
    install_driver_file_atomically(driver_dir, &payload_destination, |tmp| {
        std::fs::copy(built_binary, tmp).map(|_| ())
    })?;
    if payload_destination != destination {
        let script = local_driver_loader_wrapper_script(qualified_channel);
        install_driver_file_atomically(driver_dir, &destination, |tmp| {
            std::fs::write(tmp, &script)
        })?;
    }
    Ok(destination)
}

/// Write one staged driver file through a part-file + rename so a concurrent
/// reader never observes a torn executable — or a missing one (soldr#3538):
/// the rename goes over the destination (atomic replace on POSIX), and the
/// destination is deleted only inside the Windows-style fallback. Mirrors
/// `toolchain_packaged::install_driver_file_atomically`.
fn install_driver_file_atomically(
    driver_dir: &Path,
    destination: &Path,
    materialize: impl Fn(&Path) -> std::io::Result<()>,
) -> Result<(), SoldrError> {
    let file_name = destination
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("dylint-driver");
    let temporary = driver_dir.join(format!(".{file_name}.part-{}", std::process::id()));
    materialize(&temporary)?;
    crate::platform::fs::permissions::make_executable(&temporary)?;
    replace_installed_file(&temporary, destination, |from, to| {
        std::fs::rename(from, to)
    })
    .map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        SoldrError::Io(error)
    })
}

/// Move `temporary` onto `destination` without ever leaving the destination
/// absent on the happy path: plain `rename` first (atomic replace on POSIX),
/// remove-then-rename only when the rename failed while a destination still
/// exists (Windows refuses to replace). Same seam-and-contract as
/// `toolchain_packaged::replace_installed_file` (soldr#3538); `rename` is
/// injected so tests can assert the ordering.
pub(super) fn replace_installed_file(
    temporary: &Path,
    destination: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if let Err(error) = rename(temporary, destination) {
        if destination.exists() {
            std::fs::remove_file(destination)?;
            rename(temporary, destination)?;
        } else {
            return Err(error);
        }
    }
    Ok(())
}

/// The Unix `dylint-driver` slot for a locally built driver: export the
/// toolchain's shared-library directory for the loader, then exec the real
/// binary. Byte-for-byte the same script `toolchain_packaged::
/// driver_loader_wrapper_script` writes for the catalogue path (soldr#2634)
/// — duplicated rather than shared because that function is private to the
/// soldr-fetch crate.
pub(super) fn local_driver_loader_wrapper_script(qualified_channel: &str) -> String {
    format!(
        r#"#!/bin/sh
# soldr: loader-env wrapper for a locally built Dylint driver (soldr#2349,
# mirroring soldr#2634's catalogue wrapper).
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
lib="${{RUSTUP_HOME:-$HOME/.rustup}}/toolchains/{qualified_channel}/lib"
if [ -d "$lib" ]; then
  if [ "$(uname)" = "Darwin" ]; then
    DYLD_FALLBACK_LIBRARY_PATH="$lib${{DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}}"
    export DYLD_FALLBACK_LIBRARY_PATH
  else
    # Nix keeps runtime closure libraries (such as libz) in a separate
    # variable; retain it after rustc's libs, ahead of the caller's entries.
    if [ -n "${{NIX_LD_LIBRARY_PATH:-}}" ]; then
      LD_LIBRARY_PATH="$lib:$NIX_LD_LIBRARY_PATH${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}"
    else
      LD_LIBRARY_PATH="$lib${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}"
    fi
    export LD_LIBRARY_PATH
  fi
fi
exec "$dir/dylint-driver-real" "$@"
"#
    )
}

/// The marker this exact `(plan, cargo_dylint_version, host)` combination
/// must match for a cached build to be reused.
pub(super) fn local_driver_build_marker(
    plan: &DylintToolchainPlan,
    cargo_dylint_version: &str,
    host_triple: &str,
) -> DylintDriverBuildMarker {
    DylintDriverBuildMarker {
        schema_version: DRIVER_BUILD_MARKER_SCHEMA_VERSION,
        channel: plan.channel.clone(),
        compiler_release: plan.compiler_release.clone(),
        compiler_commit: plan.compiler_commit.clone(),
        cargo_dylint_version: cargo_dylint_version.to_string(),
        host_triple: host_triple.to_string(),
    }
}

pub(super) fn read_driver_build_marker(path: &Path) -> Option<DylintDriverBuildMarker> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub(super) fn write_driver_build_marker(
    path: &Path,
    marker: &DylintDriverBuildMarker,
) -> Result<(), SoldrError> {
    let bytes = serde_json::to_vec(marker).map_err(|error| {
        SoldrError::Other(format!("driver build marker encoding failed: {error}"))
    })?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    replace_installed_file(&temporary, path, |from, to| std::fs::rename(from, to))?;
    Ok(())
}

/// Advisory lock guarding a build of the driver at `driver_dir`, mirroring
/// `dylint_cook.rs`'s `lock_target` (`fs2::FileExt::lock_exclusive`, held for
/// the caller's guard lifetime). Two soldr processes racing to build the
/// same (nightly, cargo-dylint version, host) driver serialize here instead
/// of both compiling at once.
pub(super) fn lock_driver_build(driver_dir: &Path) -> Result<File, SoldrError> {
    let path = driver_dir.join(DRIVER_BUILD_LOCK_NAME);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    crate::platform::wait::lock_exclusive_announced(
        &file,
        crate::platform::wait::LONG_LOCK,
        "the local dylint-driver build",
    )
    .map_err(|error| SoldrError::Other(format!("failed to lock {}: {error}", path.display())))?;
    Ok(file)
}

/// Skip condition for the local-build fallback: the on-disk marker must
/// match this exact `(plan, cargo_dylint_version, host)` key, *and* the
/// binary it names must still exist and answer the same `-V` identity probe
/// [`require_prebuilt_driver`] uses for the catalogue path. A matching
/// marker next to a missing or now-broken binary is not a skip condition —
/// it triggers a rebuild.
pub(super) fn driver_build_satisfied(
    driver_dir: &Path,
    marker: &DylintDriverBuildMarker,
    plan: &DylintToolchainPlan,
    paths: &SoldrPaths,
) -> bool {
    read_driver_build_marker(&driver_dir.join(DRIVER_BUILD_MARKER_NAME)).as_ref() == Some(marker)
        && require_prebuilt_driver(plan, paths).is_ok()
}

/// Attempt tier 2 (soldr#2349): a cached local build of the driver.
///
/// Returns `Ok(())` only when a probe-verified driver is installed at the
/// path [`require_prebuilt_driver`] expects — whether by a marker-satisfied
/// skip, or by this call actually building and installing it. Every reason
/// the fallback did not happen (declined by env var, refused under strict
/// trust, or a genuine build failure) comes back as `Err` with the reason in
/// the message; the caller logs it and falls through to tier 3.
pub(super) fn ensure_local_driver_build(
    plan: &DylintToolchainPlan,
    paths: &SoldrPaths,
    cargo_dylint_version: &str,
) -> Result<(), SoldrError> {
    if !local_driver_build_fallback_enabled() {
        return Err(SoldrError::Other(format!(
            "declined: {DYLINT_DRIVER_FALLBACK_ENV_VAR} is off"
        )));
    }
    refuse_local_build_under_strict_trust()?;

    let qualified_channel = qualified_driver_channel(plan)?;
    let driver_dir = driver_root(paths).join(&qualified_channel);
    let host_triple = TargetTriple::host()?.triple();
    let marker = local_driver_build_marker(plan, cargo_dylint_version, &host_triple);

    if driver_build_satisfied(&driver_dir, &marker, plan, paths) {
        return Ok(());
    }

    std::fs::create_dir_all(&driver_dir)?;
    let _lock = lock_driver_build(&driver_dir)?;

    // A racing process may have finished the build (and written the marker)
    // while this one waited for the lock — re-check before building again.
    if driver_build_satisfied(&driver_dir, &marker, plan, paths) {
        return Ok(());
    }

    eprintln!(
        "soldr: no catalogued Dylint driver for {qualified_channel}; building dylint_driver \
         v{cargo_dylint_version} locally for {} (installs rustc-dev; happens at most once per \
         nightly + cargo-dylint version + host)",
        plan.channel
    );

    build_and_install_driver_from_source(
        plan,
        &driver_dir,
        &qualified_channel,
        cargo_dylint_version,
    )?;

    // Verify with the exact same identity probe the catalogue path uses — a
    // build that exits 0 but produces a driver cargo-dylint would refuse
    // must not be recorded as satisfied.
    require_prebuilt_driver(plan, paths)?;

    write_driver_build_marker(&driver_dir.join(DRIVER_BUILD_MARKER_NAME), &marker)?;

    let payload =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            driver_dir.join("dylint-driver")
        } else {
            driver_dir.join("dylint-driver-real")
        };
    match std::fs::read(&payload) {
        Ok(bytes) => eprintln!(
            "soldr: trust: unverified dylint-driver v{cargo_dylint_version} local-build \
             {qualified_channel} sha256={sha256} (built locally from pinned sources; the \
             catalogue publishes no asset for this nightly+host; run with {trust_var}=strict \
             to refuse this fallback)",
            sha256 = crate::fetch::sha256_of(&bytes),
            trust_var = crate::fetch::TRUST_MODE_ENV_VAR,
        ),
        Err(error) => eprintln!(
            "soldr: trust: unverified dylint-driver v{cargo_dylint_version} local-build \
             {qualified_channel} (built locally from pinned sources; sha256 of the installed \
             binary could not be computed: {error})"
        ),
    }

    Ok(())
}
