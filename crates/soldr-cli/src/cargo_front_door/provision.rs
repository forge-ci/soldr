//! Build-tool provisioning for `soldr toolchain prepare` (soldr#3802).
//!
//! `soldr cargo build` fetches its build tools lazily, inside the timed build:
//! the linker driver's managed LLVM (plus the shim that names it) and the
//! managed cmake/ninja. This module runs the *same* two front-door steps
//! against a throwaway command so a pre-flight can pay that cost up front,
//! without running cargo. It reimplements no decision:
//!
//! * [`target::apply_linker_override_reporting`] is the function
//!   `apply_child_path_and_target` calls (through the thin
//!   `apply_linker_override` wrapper), so `rustc_lld::for_command`, the
//!   `LldNeed` logic and `materialize_linker_driver_shim_resolving` decide
//!   identically for prepare and build.
//! * [`host_tooling::inject`] is the cmake/ninja step, including the
//!   Cargo.lock proof that skips it.
//!
//! The args are exactly `["build", "--release"]`: both gates depend on args
//! (`cargo_args_are_cacheable`, `lockfile_lacks_package`), and empty args would
//! silently skip cmake.

use serde::Serialize;

use crate::core::{SoldrError, SoldrPaths};

use super::target::{self, LinkerOutcome};
use super::{host_tooling, SubcommandToolBootstrap};

/// What provisioning did, for the human report and the additive
/// `build_tools` key of `toolchain ensure --json`.
#[derive(Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BuildToolsSummary {
    /// True when the linker driver or cmake step resolved something.
    pub provisioned: bool,
    pub linker: Option<BuildToolsLinker>,
    pub cmake: bool,
    /// Why nothing (or only part) was provisioned. `None` when both steps ran.
    pub note: Option<String>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildToolsLinker {
    pub choice: String,
    pub target: String,
    /// The linker (driver shim) the build would put in
    /// `CARGO_TARGET_<TRIPLE>_LINKER`.
    pub driver: Option<String>,
    pub rustflags: Option<String>,
    /// The managed `reld`, when the selection fetched one.
    pub reld: Option<String>,
}

impl BuildToolsSummary {
    /// One line per fact, for `soldr toolchain prepare` on stderr.
    pub(crate) fn render(&self) -> String {
        let mut parts = Vec::new();
        if let Some(linker) = &self.linker {
            let driver = linker.driver.as_deref().unwrap_or("(no driver shim)");
            parts.push(format!(
                "linker {} for {} -> {driver}",
                linker.choice, linker.target
            ));
        }
        if self.cmake {
            parts.push("managed cmake/ninja".to_string());
        }
        if parts.is_empty() {
            format!(
                "soldr: nothing provisioned ({})",
                self.note.as_deref().unwrap_or("no build tools needed")
            )
        } else {
            let mut line = format!("soldr: build tools ready: {}", parts.join(", "));
            if let Some(note) = &self.note {
                line.push_str(&format!(" [{note}]"));
            }
            line
        }
    }
}

/// The exact arguments both front-door gates are evaluated with.
pub(crate) fn provisioning_args() -> Vec<String> {
    vec!["build".to_string(), "--release".to_string()]
}

/// Run the front door's native-host build-tool provisioning without cargo.
pub(crate) async fn provision_native_build_tools(
    paths: &SoldrPaths,
) -> Result<BuildToolsSummary, SoldrError> {
    let args = provisioning_args();
    let mut summary = BuildToolsSummary::default();
    let mut notes = Vec::new();

    // Same env preparation order as `run_prepare.rs`: the channel pin and the
    // installed-toolchain dir name first, then the (Windows-only) default
    // target, then the linker override.
    let mut command = std::process::Command::new("cargo");
    if std::env::var_os("RUSTUP_TOOLCHAIN").is_none() {
        crate::toolchain_dir_name::export_manifest_channel(&mut command);
    }
    crate::toolchain_dir_name::apply_installed_toolchain_dir_name(&mut command);
    let explicit_target = target::default_cargo_build_target(&args, false)?;
    if let Some(target) = explicit_target.as_deref() {
        command.env("CARGO_BUILD_TARGET", target);
    }

    match target::apply_linker_override_reporting(
        &mut command,
        &args,
        explicit_target.as_deref(),
        paths,
        false,
        crate::linker::FetchMode::Await,
    )
    .await?
    {
        LinkerOutcome::Skipped(reason) => notes.push(format!("linker: {reason}")),
        LinkerOutcome::Applied(applied) => {
            summary.linker = Some(BuildToolsLinker {
                choice: format!("{:?}", applied.choice).to_ascii_lowercase(),
                target: applied.target,
                driver: applied.injection.linker,
                rustflags: applied.injection.rustflags,
                reld: applied.reld.map(|path| path.to_string_lossy().into_owned()),
            });
        }
    }

    let mut bootstrap = SubcommandToolBootstrap {
        bin_dirs: Vec::new(),
        env: Vec::new(),
        cargo_args: Vec::new(),
    };
    host_tooling::inject(&args, paths, &mut bootstrap).await;
    summary.cmake = !bootstrap.bin_dirs.is_empty();
    if !summary.cmake {
        notes.push(cmake_skip_reason(&args));
    }

    summary.provisioned = summary.linker.is_some() || summary.cmake;
    if !notes.is_empty() {
        summary.note = Some(notes.join("; "));
    }
    Ok(summary)
}

/// Why `host_tooling::inject` left the bootstrap empty. Asks the same gates
/// the injection asks, so the report cannot drift from the decision.
fn cmake_skip_reason(args: &[String]) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    if !host_tooling::should_inject_native_cmake(args) {
        "cmake: not a native compile".to_string()
    } else if !super::cmake_need::managed_cmake_needed(args, &cwd) {
        "cmake: Cargo.lock proves no cmake crate".to_string()
    } else {
        "cmake: managed cmake unavailable or opted out (CMAKE / SOLDR_USE_SYSTEM_CMAKE)".to_string()
    }
}

#[cfg(test)]
#[path = "provision_tests.rs"]
mod tests;
