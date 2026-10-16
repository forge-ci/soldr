//! `soldr toolchain prepare` / `ensure` driver, and its reld linker fetch.
//!
//! Split out of `toolchain.rs` to stay under the per-file line ceiling
//! (soldr#3276): this module owns the "install + components + targets +
//! plugins + build tools" pipeline shared by `prepare` and `ensure`, and the
//! build-tool provisioning it reports in the `--json` payload.

use crate::cargo_front_door::provision::BuildToolsSummary;
use crate::core::{SoldrError, SoldrPaths};

use crate::toolchain::{
    rustup_component_add, rustup_target_add, rustup_toolchain_install_with_profile,
};

/// Summary of what `prepare` actually did, used by
/// [`crate::toolchain_ensure`] to populate the JSON payload.
#[derive(Debug, Default)]
pub(crate) struct PrepareSummary {
    pub components_added: Vec<String>,
    pub targets_added: Vec<String>,
    pub plugins_installed: Vec<String>,
    pub linker: Option<LinkerSummary>,
    /// soldr#3802: what build-tool provisioning did (or why it did nothing).
    pub build_tools: Option<BuildToolsSummary>,
}

/// Reported in the `soldr toolchain prepare` / `soldr toolchain ensure`
/// `--json` payload when the project selects the `reld` linker (soldr#3276
/// T4). `None` on [`PrepareSummary`] means no linker fetch was needed --
/// either the project did not select `reld`, or nothing declares a
/// `rust-toolchain.toml` channel at all.
#[derive(serde::Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LinkerSummary {
    pub choice: String,
    pub path: String,
    pub version: String,
}

/// Shared inner driver for `prepare` / `ensure`. Returns the rustup /
/// cargo exit code (0 on success) plus a [`PrepareSummary`] of what was
/// actually attempted (we report every declared component/target/plugin
/// rustup didn't error on; for now we cannot cheaply diff "already
/// installed" from "newly installed" without parsing rustup output,
/// which is fragile across versions).
pub(crate) fn run_prepare_inner(
    channel: &str,
    manifest: &crate::core::RustToolchainManifest,
) -> Result<(i32, PrepareSummary), SoldrError> {
    let mut summary = PrepareSummary::default();

    let install_code = rustup_toolchain_install_with_profile(channel, manifest.profile.as_deref())?;
    if install_code != 0 {
        return Ok((install_code, summary));
    }

    if let Some(components) = manifest.components.as_deref() {
        for component in components {
            let code = rustup_component_add(channel, component)?;
            if code != 0 {
                return Ok((code, summary));
            }
            summary.components_added.push(component.clone());
        }
    }

    if let Some(targets) = manifest.targets.as_deref() {
        for target in targets {
            let code = rustup_target_add(channel, target)?;
            if code != 0 {
                return Ok((code, summary));
            }
            summary.targets_added.push(target.clone());
        }
    }

    if let Some(soldr_section) = manifest.soldr.as_ref() {
        if !soldr_section.plugins.is_empty() {
            for (name, spec) in &soldr_section.plugins {
                let code = crate::toolchain_plugins::install_plugin(name, spec)?;
                if code != 0 {
                    return Ok((code, summary));
                }
                summary
                    .plugins_installed
                    .push(format_plugin_label(name, spec));
            }
        }
    }

    let build_tools = provision_build_tools()?;
    report_build_tools(&build_tools);
    summary.linker = build_tools
        .linker
        .as_ref()
        .and_then(|linker| linker.reld.as_ref().map(|path| (linker, path)))
        .map(|(linker, path)| LinkerSummary {
            choice: linker.choice.clone(),
            path: path.clone(),
            version: crate::fetch::MANAGED_RELD_VERSION.to_string(),
        });
    summary.build_tools = Some(build_tools);

    Ok((0, summary))
}

/// Provision the build tools a native `soldr cargo build` would fetch
/// (soldr#3802): the linker driver's managed LLVM and shim, a selected `reld`,
/// and the managed cmake/ninja. Runs the front door's own functions (see
/// [`crate::cargo_front_door::provision`]), so prepare and build cannot
/// disagree. Idempotent: an already-provisioned root does no network work.
///
/// `run_prepare_inner` is sync but is also called from the async
/// `toolchain ensure` path, so the work runs on a dedicated OS thread with
/// its own current-thread runtime (a nested `block_on` would panic).
pub(crate) fn provision_build_tools() -> Result<BuildToolsSummary, SoldrError> {
    let paths = SoldrPaths::new()?;
    std::thread::spawn(move || -> Result<BuildToolsSummary, SoldrError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                SoldrError::Other(format!("could not create build-tool runtime: {error}"))
            })?;
        runtime.block_on(crate::cargo_front_door::provision::provision_native_build_tools(&paths))
    })
    .join()
    .map_err(|_| SoldrError::Other("build-tool provisioning thread panicked".to_string()))?
}

/// True when build tools may be provisioned without a `rust-toolchain.toml`
/// channel: the same opt-ins `soldr cargo` accepts (`RUSTUP_TOOLCHAIN`,
/// `--allow-unpinned` / `SOLDR_ALLOW_UNPINNED`).
pub(crate) fn unpinned_provisioning_allowed() -> bool {
    std::env::current_dir()
        .ok()
        .is_some_and(|cwd| crate::toolchain::require_toolchain_pin(&cwd).is_ok())
}

/// Print the report for a finished provisioning step to stderr.
pub(crate) fn report_build_tools(summary: &BuildToolsSummary) {
    eprintln!("{}", summary.render());
}

/// Format a plugin entry as `name` or `name@version` for the JSON
/// payload. Detailed specs with no version collapse to `name`; bare
/// `*` specs also collapse to `name`.
fn format_plugin_label(name: &str, spec: &crate::core::PluginSpec) -> String {
    let version = match spec {
        crate::core::PluginSpec::Version(v) => Some(v.as_str()),
        crate::core::PluginSpec::Detailed { version, .. } => version.as_deref(),
    };
    let trimmed = version
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "*");
    match trimmed {
        Some(v) => format!("{name}@{v}"),
        None => name.to_string(),
    }
}
