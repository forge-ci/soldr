//! Zccache build-session orchestration and compatibility subprocess helpers.
//! Extracted from `main.rs` as part of issue #339.

use crate::core::{SoldrError, SoldrPaths};
use crate::{non_empty_env_path, RUSTC_WRAPPER_OVERRIDE_ENV_VAR};
use std::ffi::OsStr;

pub(crate) use crate::build_cache_session::BuildCacheSession;

pub(crate) const SOLDR_CACHE_LIFECYCLE_ENV_VAR: &str = "SOLDR_CACHE_LIFECYCLE";
pub(crate) const SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR: &str =
    "SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS";
pub(crate) const SOLDR_ZCCACHE_SESSION_DIR_ENV_VAR: &str = "SOLDR_ZCCACHE_SESSION_DIR";

/// Opt-in: when set to a truthy value (`1`, `true`, `yes`, `on`), route
/// per-command session artifacts, rust-plan/native-cache state, and the
/// default `soldr save`/`soldr load --cache-dir` to `<cwd>/.zccache`.
///
/// Rust compiler artifacts still live in soldr-daemon's isolated embedded
/// cache root; this compatibility flag does not relocate the daemon's live
/// artifact index. Issue #802.
pub(crate) const SOLDR_ZCCACHE_PRIVATE_ENV_VAR: &str = "SOLDR_ZCCACHE_PRIVATE";

/// Directory name used under the cwd when `SOLDR_ZCCACHE_PRIVATE` is on.
pub(crate) const PRIVATE_SESSION_CACHE_DIR_NAME: &str = ".zccache";

/// Parse `SOLDR_ZCCACHE_PRIVATE` truthiness. Truthy: `1`, `true`,
/// `yes`, `on` (case-insensitive, trimmed). Anything else (including
/// `0`, `false`, empty, unset) is falsy.
pub(crate) fn parse_private_session_flag(value: Option<&str>) -> bool {
    value.is_some_and(crate::core::flag_value)
}

/// True when `SOLDR_ZCCACHE_PRIVATE` resolves truthy in the current env.
pub(crate) fn private_session_requested() -> bool {
    parse_private_session_flag(std::env::var(SOLDR_ZCCACHE_PRIVATE_ENV_VAR).ok().as_deref())
}

/// `<cwd>/.zccache`. The well-known cache directory for private sessions.
pub(crate) fn private_session_cache_dir() -> Result<std::path::PathBuf, SoldrError> {
    Ok(std::env::current_dir()?.join(PRIVATE_SESSION_CACHE_DIR_NAME))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheLifecycle {
    Job,
    Command,
}

pub(crate) fn cache_lifecycle_from_env() -> Result<CacheLifecycle, SoldrError> {
    cache_lifecycle_from_env_value(std::env::var_os(SOLDR_CACHE_LIFECYCLE_ENV_VAR).as_deref())
}

pub(crate) fn cache_lifecycle_from_env_value(
    value: Option<&OsStr>,
) -> Result<CacheLifecycle, SoldrError> {
    let Some(value) = value else {
        return Ok(CacheLifecycle::Job);
    };
    let value = value.to_string_lossy();
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "" | "job" | "job-long" | "job-long-cache" | "default" => Ok(CacheLifecycle::Job),
        "command" | "command-lifetime" | "self-build" => Ok(CacheLifecycle::Command),
        _ => Err(SoldrError::Other(format!(
            "{SOLDR_CACHE_LIFECYCLE_ENV_VAR} must be 'job' or 'command' (got {value:?})"
        ))),
    }
}

pub(crate) fn command_lifetime_shutdown_timeout() -> Result<std::time::Duration, SoldrError> {
    Ok(std::time::Duration::from_secs(
        command_lifetime_shutdown_timeout_seconds()?,
    ))
}

const DEFAULT_CACHE_SHUTDOWN_TIMEOUT_SECS: u64 = 300;

fn command_lifetime_shutdown_timeout_seconds() -> Result<u64, SoldrError> {
    match std::env::var(SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR) {
        Ok(raw) => parse_shutdown_timeout_seconds(&raw),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_CACHE_SHUTDOWN_TIMEOUT_SECS),
        Err(err) => Err(SoldrError::Other(format!(
            "{SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR} is not valid Unicode: {err}"
        ))),
    }
}

/// Parse `SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS`.
///
/// Per #3647 and docs/DAEMON_TIMEOUTS.md, empty, malformed, or zero values
/// fall back to the documented default; malformed and zero values also warn
/// on stderr so the misconfiguration is visible.
pub(crate) fn parse_shutdown_timeout_seconds(raw: &str) -> Result<u64, SoldrError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_CACHE_SHUTDOWN_TIMEOUT_SECS);
    }
    match trimmed.parse::<u64>() {
        Ok(seconds) if seconds > 0 => Ok(seconds),
        _ => {
            eprintln!(
                "soldr: warning: {SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR}={raw:?} is not a positive integer number of seconds; using the default {DEFAULT_CACHE_SHUTDOWN_TIMEOUT_SECS} s"
            );
            Ok(DEFAULT_CACHE_SHUTDOWN_TIMEOUT_SECS)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RustcWrapperMode {
    ManagedZccache,
    Custom(std::ffi::OsString),
    Disabled,
}

pub(crate) fn rustc_wrapper_mode_from_env_var(value: Option<&std::ffi::OsStr>) -> RustcWrapperMode {
    match value.and_then(std::ffi::OsStr::to_str) {
        None => value
            .map(|value| RustcWrapperMode::Custom(value.to_os_string()))
            .unwrap_or(RustcWrapperMode::ManagedZccache),
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
                RustcWrapperMode::Disabled
            } else {
                RustcWrapperMode::Custom(trimmed.into())
            }
        }
    }
}

pub(crate) fn rustc_wrapper_mode() -> RustcWrapperMode {
    rustc_wrapper_mode_from_env_var(std::env::var_os(RUSTC_WRAPPER_OVERRIDE_ENV_VAR).as_deref())
}

/// Decide what value (if any) soldr should set for `ZCCACHE_PATH_REMAP` on
/// the spawned child cargo. Returns `Some("auto")` if soldr should inject
/// the default parent-cache remap, or `None` if no injection is required
/// (either the user already set it, or the soldr-side escape hatch
/// `SOLDR_PATH_REMAP=off` is active).
///
/// Issue #352 (Tier L1.x).
pub(crate) fn resolve_path_remap_env(
    user_zccache: Option<&str>,
    soldr_override: Option<&str>,
) -> Option<&'static str> {
    // Rule 1: if the user already exported ZCCACHE_PATH_REMAP, never
    // overwrite. zccache itself decides what to do with their value
    // (except that an empty/whitespace-only value is treated as unset).
    if let Some(value) = user_zccache {
        if !value.trim().is_empty() {
            return None;
        }
    }

    // Rule 2: SOLDR_PATH_REMAP=off (case-insensitive) suppresses the
    // injection. Anything else, or unset, falls through to auto.
    if let Some(value) = soldr_override {
        if value.trim().eq_ignore_ascii_case("off") {
            return None;
        }
    }

    Some("auto")
}

pub(crate) fn path_remap_auto_active(
    user_zccache: Option<&str>,
    soldr_override: Option<&str>,
) -> bool {
    match user_zccache
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => value.eq_ignore_ascii_case("auto"),
        None => resolve_path_remap_env(None, soldr_override).is_some(),
    }
}

pub(crate) fn resolve_worktree_root_env(
    user_worktree_root: Option<&std::ffi::OsStr>,
    cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    if user_worktree_root.is_some_and(|value| !value.is_empty()) {
        return None;
    }

    find_git_worktree_root(cwd).or_else(|| Some(cwd.to_path_buf()))
}

pub(crate) fn find_git_worktree_root(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    for candidate in cwd.ancestors() {
        let dot_git = candidate.join(".git");
        if dot_git.is_dir() || dot_git.is_file() {
            return Some(candidate.to_path_buf());
        }
    }
    None
}

/// [`soldr_platform::platform::fs::delivery_probe::DeliveryCapability`] ->
/// the [`crate::core::materialization_mode::ZccacheMode`] soldr injects for
/// it. Lives here, not in either lower-level crate, because it is the only
/// place that depends on both (soldr-platform never depends on soldr-core's
/// domain types, and soldr-core never depends on soldr-platform's).
fn delivery_capability_to_mode(
    capability: crate::platform::fs::delivery_probe::DeliveryCapability,
) -> crate::core::materialization_mode::ZccacheMode {
    use crate::core::materialization_mode::ZccacheMode;
    use crate::platform::fs::delivery_probe::DeliveryCapability;
    match capability {
        DeliveryCapability::Reflink => ZccacheMode::Reflink,
        DeliveryCapability::Link => ZccacheMode::Link,
        DeliveryCapability::Copy => ZccacheMode::Copy,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ZccacheChildEnv {
    pub(crate) path_remap: Option<&'static str>,
    pub(crate) worktree_root: Option<std::path::PathBuf>,
    /// Canonical `ZCCACHE_MODE` soldr sets on the child (soldr#3407,
    /// zccache#1792): for a soldr-owned source (`--zccache-mode`,
    /// `SOLDR_ZCCACHE_MODE`, `[zccache] mode`); or when the resolved mode
    /// is `AUTO` (from any tier, including the user's own `ZCCACHE_MODE`)
    /// or nothing is configured at all, soldr probes the `(zccache cache
    /// dir, cargo target dir)` pair once
    /// ([`crate::platform::fs::delivery_probe::probe_delivery_capability`])
    /// and injects the single explicit answer -- `REFLINK`, `LINK`, or
    /// `COPY` -- instead of the chain zccache's own `AUTO` no longer runs.
    /// An explicit user `LINK`/`COPY`/`REFLINK`/`REFLINK_OR_LINK_OR_COPY`
    /// still reaches the embedded service unaided and is never rewritten.
    pub(crate) materialization_mode: Option<&'static str>,
}

impl ZccacheChildEnv {
    pub(crate) fn from_current_process() -> Result<Self, SoldrError> {
        let user_zccache = std::env::var(crate::cache_lib::ZCCACHE_PATH_REMAP_ENV_VAR).ok();
        let soldr_override = std::env::var(crate::cache_lib::SOLDR_PATH_REMAP_ENV_VAR).ok();
        let user_worktree_root = std::env::var_os(crate::cache_lib::ZCCACHE_WORKTREE_ROOT_ENV_VAR);
        let cwd = std::env::current_dir()?;
        // A missing config file is the default config; an unreadable one is
        // ignored here like the other build-time tiers, but an invalid mode
        // value in it — or in either environment tier — fails the build
        // rather than silently delivering the wrong way.
        let paths = crate::core::SoldrPaths::new().ok();
        let config_mode = paths
            .as_ref()
            .and_then(|paths| paths.load_config().ok())
            .and_then(|config| config.zccache.mode);
        let mode = crate::core::materialization_mode::resolve_zccache_mode(config_mode.as_deref())
            .map_err(|error| SoldrError::Other(error.to_string()))?;
        // soldr#3440/zccache#1792: only ever probes the filesystem when the
        // resolved mode needs it (AUTO or unset) -- never for an explicit
        // LINK/COPY/REFLINK/REFLINK_OR_LINK_OR_COPY.
        let cache_probe_dir = paths
            .as_ref()
            .map(crate::cache_lib::zccache_dir)
            .unwrap_or_else(|| cwd.clone());
        let target_probe_dir =
            non_empty_env_path("CARGO_TARGET_DIR").unwrap_or_else(|| cwd.join("target"));
        Ok(Self::from_inputs(
            user_zccache.as_deref(),
            soldr_override.as_deref(),
            user_worktree_root.as_deref(),
            &cwd,
            mode,
            || {
                delivery_capability_to_mode(
                    crate::platform::fs::delivery_probe::probe_delivery_capability(
                        &cache_probe_dir,
                        &target_probe_dir,
                    ),
                )
            },
        ))
    }

    pub(crate) fn from_inputs(
        user_zccache: Option<&str>,
        soldr_override: Option<&str>,
        user_worktree_root: Option<&std::ffi::OsStr>,
        cwd: &std::path::Path,
        mode: Option<crate::core::materialization_mode::ResolvedZccacheMode>,
        probe: impl FnOnce() -> crate::core::materialization_mode::ZccacheMode,
    ) -> Self {
        let path_remap = resolve_path_remap_env(user_zccache, soldr_override);
        let worktree_root = if path_remap_auto_active(user_zccache, soldr_override) {
            resolve_worktree_root_env(user_worktree_root, cwd)
        } else {
            None
        };
        Self {
            path_remap,
            worktree_root,
            materialization_mode: crate::core::materialization_mode::child_env_value_for(
                mode, probe,
            ),
        }
    }

    pub(crate) fn apply_to_command(&self, cargo: &mut std::process::Command) {
        if let Some(value) = self.path_remap {
            cargo.env(crate::cache_lib::ZCCACHE_PATH_REMAP_ENV_VAR, value);
        }
        if let Some(root) = self.worktree_root.as_ref() {
            cargo.env(crate::cache_lib::ZCCACHE_WORKTREE_ROOT_ENV_VAR, root);
        }
        if let Some(mode) = self.materialization_mode {
            cargo.env(
                crate::core::materialization_mode::ZCCACHE_MODE_ENV_VAR,
                mode,
            );
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedZccacheWrapperPlan {
    /// Parent-side build session carrier (soldr#1368). It no longer
    /// tracks an external zccache daemon — rustc compiles are cached by
    /// the soldr-daemon embedded service — but it still carries the
    /// zccache cache dir + per-build paths that the in-process rust-plan
    /// save/restore (rust_plan.rs) and native-C caching (native_cc.rs)
    /// need. `wrapper_path` is a compiler-named soldr multicall shim so the
    /// wrapper contract cannot be confused with a different multicall verb.
    pub(crate) session: BuildCacheSession,
    pub(crate) child_env: ZccacheChildEnv,
    pub(crate) wrapper_path: std::path::PathBuf,
    /// Canonically named daemon image inherited by compiler shims. A wrapper's
    /// own executable is named `rustc`/`clang`; launching that image directly
    /// would make the PID safety gate correctly reject the daemon.
    pub(crate) daemon_path: std::path::PathBuf,
    /// Route registered by the front door for the broker. Compiler shims carry
    /// only this opaque identity; they never place or spawn the daemon.
    pub(crate) broker_service_name: String,
}

#[derive(Debug, Clone)]
pub(crate) enum RustcWrapperPlan {
    ManagedZccache(Box<ManagedZccacheWrapperPlan>),
    Custom {
        wrapper: std::ffi::OsString,
        sccache_dir: Option<std::path::PathBuf>,
    },
    Disabled,
}

impl RustcWrapperPlan {
    pub(crate) fn is_managed_zccache(&self) -> bool {
        matches!(self, Self::ManagedZccache(_))
    }

    pub(crate) fn session(&self) -> Option<&BuildCacheSession> {
        match self {
            Self::ManagedZccache(plan) => Some(&plan.session),
            Self::Custom { .. } | Self::Disabled => None,
        }
    }

    pub(crate) fn apply_to_command(
        &self,
        cargo: &mut std::process::Command,
    ) -> Result<(), SoldrError> {
        match self {
            Self::ManagedZccache(plan) => {
                // soldr#1368: point cargo's RUSTC_WRAPPER at soldr so
                // rustc invocations route to the daemon's embedded
                // zccache service. There is no externally-resolved
                // zccache binary or managed session to plumb, so clear
                // the legacy session env and only seed the parent-cache
                // path-remap vars.
                crate::wrapper_identity::set_owned_rustc_wrapper(
                    cargo,
                    plan.wrapper_path.as_os_str(),
                    crate::wrapper_identity::WrapperOrigin::SoldrManaged,
                );
                cargo.env(
                    crate::daemon::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR,
                    &plan.broker_service_name,
                );
                cargo.env_remove(crate::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR);
                remove_managed_zccache_env(cargo);
                cargo.env_remove(SOLDR_ZCCACHE_SESSION_DIR_ENV_VAR);
                plan.child_env.apply_to_command(cargo);
            }
            Self::Custom {
                wrapper,
                sccache_dir,
            } => {
                if let Some(sccache_dir) = sccache_dir {
                    cargo.env("SCCACHE_DIR", sccache_dir);
                }
                crate::wrapper_identity::set_owned_rustc_wrapper(
                    cargo,
                    wrapper,
                    crate::wrapper_identity::WrapperOrigin::CustomOverride,
                );
                remove_managed_zccache_env(cargo);
            }
            Self::Disabled => {
                crate::wrapper_identity::remove_owned_rustc_wrapper(cargo);
                remove_managed_zccache_env(cargo);
            }
        }
        Ok(())
    }
}

fn remove_managed_zccache_env(cargo: &mut std::process::Command) {
    cargo.env_remove(crate::cache_lib::ZCCACHE_BINARY_ENV_VAR);
    cargo.env_remove(crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR);
    cargo.env_remove(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR);
}

pub(crate) async fn prepare_rustc_wrapper_plan(
    paths: &SoldrPaths,
) -> Result<RustcWrapperPlan, SoldrError> {
    match rustc_wrapper_mode() {
        RustcWrapperMode::ManagedZccache => prepare_zccache_build(paths)
            .await
            .map(|plan| RustcWrapperPlan::ManagedZccache(Box::new(plan))),
        RustcWrapperMode::Custom(wrapper) => {
            // `SOLDR_RUSTC_WRAPPER` is also the supported way for source CI
            // to pin compiler re-entries to the exact Soldr binary under
            // test. Treat that one identity as the managed embedded-cache
            // path, not as an opaque third-party wrapper. Otherwise we omit
            // the broker service registration and child route export that a
            // Soldr wrapper requires. An in-place rebuild then changes the
            // sibling `soldr-daemon` image hash, the wrapper requests that new
            // route, and the stable broker correctly refuses it because no
            // matching service definition was written.
            //
            // File identity is intentional: a basename check would mistake
            // an unrelated executable named `soldr` for this process, while
            // canonical path equality would miss hardlinked Soldr shims.
            if custom_wrapper_is_current_soldr(&wrapper) {
                let mut plan = prepare_zccache_build(paths).await?;
                plan.wrapper_path = std::path::PathBuf::from(wrapper);
                return Ok(RustcWrapperPlan::ManagedZccache(Box::new(plan)));
            }
            let sccache_dir =
                if is_sccache_wrapper(&wrapper) && std::env::var_os("SCCACHE_DIR").is_none() {
                    let sccache_dir = crate::cache_lib::sccache_dir(paths);
                    std::fs::create_dir_all(&sccache_dir)?;
                    Some(sccache_dir)
                } else {
                    None
                };
            Ok(RustcWrapperPlan::Custom {
                wrapper,
                sccache_dir,
            })
        }
        RustcWrapperMode::Disabled => Ok(RustcWrapperPlan::Disabled),
    }
}

fn custom_wrapper_is_current_soldr(wrapper: &std::ffi::OsStr) -> bool {
    let Ok(current) = std::env::current_exe() else {
        return false;
    };
    crate::platform::fs::identity::same_file(std::path::Path::new(wrapper), &current)
}

pub(crate) fn is_sccache_wrapper(wrapper: &std::ffi::OsStr) -> bool {
    std::path::Path::new(wrapper)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|stem| stem.eq_ignore_ascii_case("sccache"))
}

/// Which kind of wrapper plan a front door is finalising, for the pre-warm gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrewarmPlanKind {
    /// Managed embedded zccache (including a custom wrapper that is this soldr).
    Managed,
    /// A third-party wrapper such as sccache: no soldr-daemon route is used.
    Custom,
    /// `RUSTC_WRAPPER` disabled.
    Disabled,
    /// `--no-cache`: the build must not touch the daemon.
    NoCache,
}

/// Opt-out env var for the daemon-route pre-warm (soldr#3800). Default on.
pub(crate) const BROKER_PREWARM_ENV_VAR: &str = "SOLDR_BROKER_PREWARM";

/// Pure gate for the pre-warm: only managed-zccache plans, and only when
/// `SOLDR_BROKER_PREWARM` is not explicitly off. Uses the canonical
/// default-on reading (`!is_off_value`, soldr#2740) so an unrecognised value
/// never silently disables it.
pub(crate) fn should_prewarm(kind: PrewarmPlanKind, optout_value: Option<&str>) -> bool {
    kind == PrewarmPlanKind::Managed && !optout_value.is_some_and(crate::core::is_off_value)
}

/// Hook signature for the actual route acquisition, injectable in tests.
pub(crate) type PrewarmHook = fn(&str) -> std::io::Result<()>;

fn ensure_route_hook(service_name: &str) -> std::io::Result<()> {
    crate::session_transport::ensure_broker_route(service_name, std::time::Duration::from_secs(30))
        .map(drop)
}

/// Start the daemon route in the background so the first rustc wrappers find it
/// ready instead of racing the cold launch (soldr#3800). Detached: never joined by
/// production code, and failure is ignored (logged only under session debug) because
/// the wrappers acquire the route themselves. The returned handle exists for tests.
pub(crate) fn spawn_route_prewarm(
    service_name: &str,
    hook: PrewarmHook,
) -> Option<std::thread::JoinHandle<()>> {
    let service_name = service_name.to_string();
    std::thread::Builder::new()
        .name("soldr-route-prewarm".into())
        .spawn(move || {
            if let Err(err) = hook(&service_name) {
                if crate::session_transport::session_debug_enabled() {
                    eprintln!("soldr: broker route pre-warm failed (ignored): {err}");
                }
            }
        })
        .ok()
}

fn prewarm_if_enabled(service_name: &str) {
    // soldr#3823: off unless explicitly enabled while the target-run lanes'
    // missing route claim is diagnosed.
    if !crate::core::env_flag::flag(BROKER_PREWARM_ENV_VAR) {
        return;
    }
    let optout = std::env::var(BROKER_PREWARM_ENV_VAR).ok();
    if should_prewarm(PrewarmPlanKind::Managed, optout.as_deref()) {
        drop(spawn_route_prewarm(service_name, ensure_route_hook));
    }
}

async fn prepare_zccache_build(
    paths: &SoldrPaths,
) -> Result<ManagedZccacheWrapperPlan, SoldrError> {
    // soldr#1368: rustc compiles are cached by the soldr-daemon's
    // embedded zccache service over the RUSTC_WRAPPER=soldr IPC path
    // (see `wrapper.rs` + `compile_dispatch.rs`). The front door no
    // longer downloads a managed zccache binary or spawns a separate
    // zccache daemon. It still builds a lightweight build *session*
    // carrier so the in-process rust-plan save/restore and native-C
    // caching keep working against the shared zccache cache dir.
    let zccache_base_dir = managed_zccache_cache_dir(paths)?;
    let child_env = ZccacheChildEnv::from_current_process()?;

    // Session/auxiliary cache-dir resolution mirrors the pre-#1368 logic: an
    // explicit ZCCACHE_CACHE_DIR wins; otherwise `SOLDR_ZCCACHE_PRIVATE`
    // selects `<cwd>/.zccache`; otherwise use the shared Soldr-owned zccache
    // directory. The embedded daemon's Rust artifact root is configured
    // independently in `zccache_embedded.rs`.
    let inherited_soldr_managed_dir =
        non_empty_env_path(crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR)
            .map(|path| normalize_path_for_compare(&path))
            .transpose()?;
    let explicit_zccache_cache_dir =
        non_empty_env_path(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR)
            .map(|path| normalize_path_for_compare(&path))
            .transpose()?
            .filter(|path| inherited_soldr_managed_dir.as_ref() != Some(path));
    let private_override = if explicit_zccache_cache_dir.is_none() && private_session_requested() {
        Some(normalize_path_for_compare(&private_session_cache_dir()?)?)
    } else {
        None
    };
    let zccache_dir = explicit_zccache_cache_dir
        .or(private_override)
        .unwrap_or(zccache_base_dir);
    std::fs::create_dir_all(&zccache_dir)?;
    std::fs::create_dir_all(zccache_dir.join("logs"))?;

    let session = BuildCacheSession {
        // Retained for struct compatibility only — nothing spawns it now.
        cache_dir: zccache_dir.clone(),
        session_id: synthetic_build_session_id(),
        session_log_path: crate::cache_lib::session_log_path(&zccache_dir),
        journal_path: crate::cache_lib::session_journal_path(&zccache_dir),
        session_stats_path: crate::cache_lib::session_stats_path(&zccache_dir),
    };

    let wrapper_path = crate::binaries::rustc_wrapper_shim_binary(paths)?;
    let (daemon_path, broker_service_name) = register_broker_daemon_service()?;
    prewarm_if_enabled(&broker_service_name);
    Ok(ManagedZccacheWrapperPlan {
        session,
        child_env,
        wrapper_path,
        daemon_path,
        broker_service_name,
    })
}

/// Materialize + register the soldr-daemon image with the broker and return
/// `(daemon_path, service_name)`.
///
/// soldr#2451: shared by [`prepare_rustc_wrapper_plan`] and the
/// caller-provided-`RUSTC_WRAPPER` maturin path. Any build front door that
/// hands a child `RUSTC_WRAPPER=soldr` must also hand it `SOLDR_BROKER_SERVICE`,
/// because the cargo wrapper re-entries resolve the broker route by that name.
/// Without it they fall back to hashing a sibling `soldr-daemon` next to the
/// wrapper — which does not exist in a wheel install — and fail as
/// "cannot resolve the broker daemon route (os error 2)".
pub(crate) fn register_broker_daemon_service() -> Result<(std::path::PathBuf, String), SoldrError> {
    let daemon_path = crate::binaries::soldr_daemon_binary()?;
    let installed = crate::daemon::service_definition::install_service_definition(&daemon_path)
        .map_err(|err| {
            SoldrError::Other(format!(
                "failed to register soldr-daemon image {} with the broker: {err}",
                daemon_path.display()
            ))
        })?;
    if let Ok(paths) = crate::core::SoldrPaths::new() {
        crate::daemon::lifecycle::preflight_displace_stale_daemon_for_service(
            &paths,
            Some(installed.definition.service_name.as_ref()),
        );
    }
    Ok((daemon_path, installed.definition.service_name))
}

/// Generate a short, unique-enough id for a front-door build session
/// (soldr#1368). It is no longer a real zccache daemon session id — it's
/// a cosmetic correlation handle — so a blake3 of (pid, monotonic nanos)
/// is plenty and avoids a `uuid` dependency.
fn synthetic_build_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = zccache::hash::StreamHasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    hex::encode(&hasher.finalize().as_bytes()[..12])
}

pub(crate) fn managed_zccache_cache_dir(
    paths: &SoldrPaths,
) -> Result<std::path::PathBuf, SoldrError> {
    normalize_path_for_compare(&crate::cache_lib::zccache_dir(paths))
}

pub(crate) fn normalize_path_for_compare(
    path: &std::path::Path,
) -> Result<std::path::PathBuf, SoldrError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
#[path = "zccache_tests.rs"]
mod tests;
