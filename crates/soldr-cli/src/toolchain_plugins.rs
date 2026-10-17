//! `[soldr.plugins]` acquisition: prebuilt `known_tools` binary first,
//! `cargo install` only on a miss (soldr#3699, "Pre-built first").
//!
//! A plugin's version requirement is always honoured. The prebuilt path is
//! taken only when the version soldr's own cargo front door would run for
//! that subcommand (`managed_subcommand_version`: the registry pin, else the
//! latest release) satisfies the declared requirement. When it does not, or
//! when the entry asks for something a prebuilt cannot provide
//! (`features` / `no-default-features`), or the fetch fails, the plugin
//! falls back to `cargo install` with the declared requirement. A different
//! version is never silently installed.

use crate::core::{PluginSpec, SoldrError, SoldrPaths};
use crate::fetch::known_tools::{lookup_by_crate, ToolSpec};
use crate::fetch::{FetchResult, VersionSpec};

/// Decide whether `name`/`spec` is eligible for a prebuilt fetch. Returns
/// the registry entry, the version to fetch, and the requirement the
/// fetched version must satisfy (`None` = any version).
pub(crate) fn prebuilt_plan(
    name: &str,
    spec: &PluginSpec,
) -> Option<(&'static ToolSpec, VersionSpec, Option<semver::VersionReq>)> {
    let tool = lookup_by_crate(name)?;
    // Only cargo subcommands are dispatched through the cargo front door,
    // which is what consumes the cached prebuilt.
    tool.cargo_subcommand?;
    let (version, features, no_default_features) = match spec {
        PluginSpec::Version(v) => (Some(v.as_str()), None, None),
        PluginSpec::Detailed {
            version,
            features,
            no_default_features,
            ..
        } => (
            version.as_deref(),
            features.as_deref(),
            *no_default_features,
        ),
    };
    if features.is_some_and(|f| !f.is_empty()) || no_default_features == Some(true) {
        return None;
    }
    let req = match version
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "*")
    {
        Some(raw) => Some(semver::VersionReq::parse(raw).ok()?),
        None => None,
    };
    let fetch_version = crate::cargo_front_door::managed_subcommand_version(tool);
    if let (Some(req), VersionSpec::Exact(pinned)) = (&req, &fetch_version) {
        let pinned = semver::Version::parse(pinned).ok()?;
        if !req.matches(&pinned) {
            return None;
        }
    }
    Some((tool, fetch_version, req))
}

/// Testable core: `fetch` resolves a prebuilt, `cargo_install` is the
/// legacy fallback. Returns the exit code (0 on success).
pub(crate) fn install_plugin_with(
    name: &str,
    spec: &PluginSpec,
    fetch: impl FnOnce(&str, &VersionSpec) -> Result<FetchResult, SoldrError>,
    cargo_install: impl FnOnce() -> Result<i32, SoldrError>,
) -> Result<i32, SoldrError> {
    let Some((tool, version, req)) = prebuilt_plan(name, spec) else {
        return cargo_install();
    };
    match fetch(tool.crate_name, &version) {
        Ok(result) => {
            let satisfies = match &req {
                None => true,
                Some(req) => semver::Version::parse(result.version.trim_start_matches('v'))
                    .is_ok_and(|v| req.matches(&v)),
            };
            if satisfies {
                eprintln!(
                    "soldr: plugin {name}: using prebuilt v{} ({})",
                    result.version,
                    result.binary_path.display()
                );
                return Ok(0);
            }
            eprintln!(
                "soldr: plugin {name}: prebuilt v{} does not satisfy the declared requirement; falling back to cargo install",
                result.version
            );
        }
        Err(error) => {
            eprintln!("soldr: plugin {name}: prebuilt fetch failed ({error}); falling back to cargo install");
        }
    }
    cargo_install()
}

/// Production entry point used by `soldr toolchain prepare` / `ensure`.
pub(crate) fn install_plugin(name: &str, spec: &PluginSpec) -> Result<i32, SoldrError> {
    install_plugin_with(name, spec, fetch_on_thread, || {
        crate::toolchain::cargo_install_plugin(name, spec)
    })
}

/// `run_prepare_inner` is sync but also runs under the async `ensure`
/// path, so fetch on a dedicated thread with its own runtime (same
/// pattern as the reld fetch in `toolchain_prepare`).
fn fetch_on_thread(crate_name: &str, version: &VersionSpec) -> Result<FetchResult, SoldrError> {
    let paths = SoldrPaths::new()?;
    let crate_name = crate_name.to_string();
    let version = version.clone();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                SoldrError::Other(format!("could not create plugin fetch runtime: {e}"))
            })?;
        crate::platform::wait::block_on_for(
            &runtime,
            crate::fetch::fetch_tool_for_host_with_paths(&crate_name, &version, &paths),
            crate::platform::wait::LONG_CHILD,
        )
        .unwrap_or_else(|error| Err(SoldrError::Other(format!("plugin fetch: {error}"))))
    });
    crate::platform::wait::join_for(handle, crate::platform::wait::LONG_CHILD)
        .map_err(|_| SoldrError::Other("plugin fetch thread panicked or overran".to_string()))?
}

#[cfg(test)]
#[path = "toolchain_plugins_tests.rs"]
mod tests;
