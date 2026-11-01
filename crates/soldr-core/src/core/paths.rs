//! `~/.soldr/` layout, `SoldrConfig` (`config.toml`), and the
//! `resolve_cargo_home` / `resolve_rustup_home` helpers.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use super::{home_dir, non_empty_env_path, soldr_root_from_env_var};
use super::{SoldrError, CARGO_HOME_ENV_VAR, RUSTUP_HOME_ENV_VAR};

pub const SOLDR_CACHE_DIR_ENV_VAR: &str = "SOLDR_CACHE_DIR";
const CARGO_ABORT_LOG_FILE: &str = "cargo-aborts.jsonl";
/// Persistent record of every small-tool probe (soldr#3389).
pub const SMALL_TOOL_LOG_FILE: &str = "small-tools.jsonl";

/// The home-anchored default root name: `~/.soldr`, for every build.
///
/// soldr#1597 once sent dev/manual builds to a separate `~/.soldr-dev` so a
/// locally built daemon could not collide with a managed one on the shared
/// state. That isolation now lives where it belongs: daemon routes, route
/// claims and the root-owner lock are keyed per daemon-image generation
/// (soldr#3374), so two builds share one root without stomping each other.
/// The separate root only cost a second, always-cold compile cache -- a source
/// build installed for daily use hit nothing the release build had cached.
/// An explicit `SOLDR_CACHE_DIR` still wins.
fn default_home_dir_name() -> &'static str {
    ".soldr"
}

/// The soldr version segment used by per-version `~/.soldr/v<X.Y.Z>/**`
/// state. Source of truth for `SoldrPaths::versioned_root` and
/// `SoldrPaths::versioned_shims_dir`. See zackees/soldr#743 for the
/// layout RFC and zackees/soldr#742 for the first consumer.
///
/// Lives in `core` (not `fetch`, its historical home) so that `core`
/// has no upward edge into `fetch` (#1490 Phase 0, edge E1);
/// `fetch::MANAGED_SHIM_VERSION` remains as a re-export.
pub const MANAGED_SHIM_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone)]
pub struct SoldrPaths {
    pub root: PathBuf,
    pub bin: PathBuf,
    /// Directory holding the pinned-zccache install (issue #426). Lives at
    /// the user's home-anchored `~/.soldr/bin/` independent of
    /// `SOLDR_CACHE_DIR`, so a pin registered against the default cache dir
    /// remains visible to builds that re-root with `SOLDR_CACHE_DIR=/x`.
    /// For synthetic-root construction (`SoldrPaths::with_root`, used by
    /// tests) this collapses into `bin` so test isolation is preserved.
    pub pinned_bin: PathBuf,
    pub cache: PathBuf,
    pub config_file: PathBuf,
}

impl SoldrPaths {
    pub fn new() -> Result<Self, SoldrError> {
        Self::from_root_env_value(std::env::var_os(SOLDR_CACHE_DIR_ENV_VAR).as_deref())
    }

    fn from_root_env_value(value: Option<&OsStr>) -> Result<Self, SoldrError> {
        let env_root = soldr_root_from_env_var(value).transpose()?;
        Self::from_env_root_and_home(
            env_root,
            home_dir().map(|h| h.join(default_home_dir_name())),
        )
    }

    /// Inner constructor split out so tests can inject explicit `env_root`
    /// + `home_root` values without mutating the process env. Other tests
    ///   in this binary read HOME / USERPROFILE too, so global env mutation
    ///   in a unit test races with parallel cases.
    fn from_env_root_and_home(
        env_root: Option<PathBuf>,
        home_root: Result<PathBuf, SoldrError>,
    ) -> Result<Self, SoldrError> {
        let root = match (&env_root, &home_root) {
            (Some(env), _) => env.clone(),
            (None, Ok(home)) => home.clone(),
            (None, Err(_)) => return Err(SoldrError::NoHomeDir),
        };
        // The pin must NOT move when SOLDR_CACHE_DIR overrides the rest of
        // the install root (issue #426 — pinned binaries are a machine-level
        // user preference, not per-cache-dir state). When no $HOME / no
        // USERPROFILE is available — e.g. a headless CI sandbox that relies
        // entirely on SOLDR_CACHE_DIR — fall back to the env-rooted bin/ so
        // the pin lives somewhere accessible; that env does lose the
        // cross-SOLDR_CACHE_DIR survival property, but it matches today's
        // behavior so nothing regresses.
        let pinned_bin = match &home_root {
            Ok(home) => home.join("bin"),
            Err(_) => root.join("bin"),
        };
        Ok(Self {
            bin: root.join("bin"),
            pinned_bin,
            cache: root.join("cache"),
            config_file: root.join("config.toml"),
            root,
        })
    }

    pub fn with_root(root: PathBuf) -> Self {
        let bin = root.join("bin");
        Self {
            bin: bin.clone(),
            // Tests use synthetic roots and rely on the pin landing inside
            // the test workspace. Collapse pinned_bin into bin so isolation
            // works; production goes through `from_root_env_value` which
            // anchors pinned_bin at the user's home.
            pinned_bin: bin,
            cache: root.join("cache"),
            config_file: root.join("config.toml"),
            root,
        }
    }

    pub fn ensure_dirs(&self) -> Result<(), SoldrError> {
        std::fs::create_dir_all(&self.bin)?;
        std::fs::create_dir_all(&self.pinned_bin)?;
        std::fs::create_dir_all(&self.cache)?;
        Ok(())
    }

    /// `<root>/logs/small-tools.jsonl` (soldr#3389).
    pub fn small_tool_log(&self) -> PathBuf {
        self.root.join("logs").join(SMALL_TOOL_LOG_FILE)
    }

    pub fn cargo_abort_log(&self) -> PathBuf {
        self.root.join("logs").join(CARGO_ABORT_LOG_FILE)
    }

    /// Per-version state root — `<root>/v<MANAGED_SHIM_VERSION>/`.
    ///
    /// Reserved for the layout RFC tracked at zackees/soldr#743 (per-soldr-version
    /// isolation of `shims/`, future `bin/`, `cache/`, `state.sqlite3`, etc.).
    /// This PR only exposes the accessor; existing top-level subdirs
    /// (`bin`, `cache`, `config_file`) are NOT migrated here. The first
    /// real consumer is `versioned_shims_dir` (zackees/soldr#742).
    pub fn versioned_root(&self) -> PathBuf {
        self.root.join(format!("v{}", MANAGED_SHIM_VERSION))
    }

    /// Per-version shim directory — `<versioned_root>/shims/`.
    ///
    /// Owned and populated by the `soldr shims` verb (zackees/soldr#742).
    /// Holds multicall `soldr` shims (hardlinked/copied under each tool
    /// name) for THIS soldr version. Multi-version soldr installs coexist
    /// cleanly because each version has its own subdir.
    pub fn versioned_shims_dir(&self) -> PathBuf {
        self.versioned_root().join("shims")
    }

    /// Load the soldr config from `config.toml` if it exists.
    ///
    /// A missing file intentionally keeps the historical default behavior,
    /// but an unreadable or malformed file is an error. Cleanup callers must
    /// never mistake a broken config for an absent one.
    // Cold path (config load happens once); boxing the error is not worth it.
    #[allow(clippy::result_large_err)]
    pub fn load_config(&self) -> Result<SoldrConfig, SoldrConfigLoadError> {
        SoldrConfig::load(&self.config_file)
    }
}

// ---------------------------------------------------------------------------
// Config — `~/.soldr/config.toml`
// ---------------------------------------------------------------------------

/// Top-level soldr configuration. Currently carries the `[gc]` section
/// (issue #234), the `[auto_gc]` section (issue #323) and an optional
/// top-level `linker = "..."` field (issue #285); future sections can
/// be added freely.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SoldrConfig {
    #[serde(default)]
    pub gc: GcConfig,
    /// Automatic GC under disk pressure (issue #323).
    #[serde(default)]
    pub auto_gc: AutoGcConfig,
    /// User-configured linker choice for `soldr cargo ...`. Mirrors the
    /// `SOLDR_LINKER` env var; the env var wins when both are set.
    /// Accepted values: `default`, `ld`, `mold`, `rust-lld`, `reld`, `fast`.
    /// Unset defaults to `fast`, which resolves to reld (soldr#3262).
    #[serde(default)]
    pub linker: Option<String>,
    /// Cross-repo shared `soldr cook` artifact cache (issue #578, meta
    /// #579).
    #[serde(default)]
    pub cook: CookConfig,
    /// Per-tool optional manifest pins (issue #861). Keyed by tool name
    /// (`zccache`, `crgx`, `cargo-zigbuild`, ...). When a key is
    /// present, the v6 manifest resolver fetches that exact version
    /// instead of the leaf's `latest`. Missing keys = track `latest`.
    ///
    /// ```toml
    /// [pins]
    /// zccache = "1.12.5"
    /// crgx    = "0.1.0"
    /// ```
    #[serde(default)]
    pub pins: PinsConfig,
    /// Compile-concurrency limit (issue #1761). See
    /// [`crate::core::jobs`] for the full precedence chain.
    #[serde(default)]
    pub jobs: crate::core::jobs::JobsConfig,
    /// zccache cache-hit delivery mode (soldr#3407). See
    /// [`crate::core::materialization_mode`] for the precedence chain.
    #[serde(default)]
    pub zccache: crate::core::materialization_mode::ZccacheModeConfig,
    /// `soldr install` source-cache retention knobs (soldr#2310).
    #[serde(default)]
    pub install: InstallConfig,
}

/// `install` section of `config.toml` (soldr#2310).
///
/// ```toml
/// [install]
/// source_ttl_days = 2   # eager TTL for the git source cache
/// source_max_gb   = 5   # soft size cap, coldest-first eviction
/// ```
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstallConfig {
    /// Delete source-cache entries whose last-use is older than this,
    /// regardless of disk pressure (disposable cache).
    #[serde(default = "InstallConfig::default_source_ttl_days")]
    pub source_ttl_days: u64,
    /// Soft size cap for the install source cache, in gibibytes.
    #[serde(default = "InstallConfig::default_source_max_gb")]
    pub source_max_gb: u64,
}

impl InstallConfig {
    pub const fn default_source_ttl_days() -> u64 {
        2
    }
    pub const fn default_source_max_gb() -> u64 {
        5
    }
}

impl Default for InstallConfig {
    fn default() -> Self {
        Self {
            source_ttl_days: Self::default_source_ttl_days(),
            source_max_gb: Self::default_source_max_gb(),
        }
    }
}

/// `pins` section of `config.toml` (issue #861).
///
/// A flat `tool -> version` table that the v6 manifest resolver
/// consults before falling back to the leaf's `latest`. Stored
/// case-sensitively; soldr's tool keys are lowercased by convention.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct PinsConfig {
    pub tools: std::collections::HashMap<String, String>,
}

impl PinsConfig {
    /// Look up the pinned version for `tool`, returning `None` when
    /// the tool is not pinned (track `latest`) or the pin is empty.
    pub fn pin_for(&self, tool: &str) -> Option<&str> {
        match self.tools.get(tool) {
            Some(v) if !v.is_empty() => Some(v.as_str()),
            _ => None,
        }
    }

    /// True when no pins are declared.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// `cook` section of `config.toml` (issue #578).
///
/// ```toml
/// [cook]
/// auto_hydrate     = true   # default ON
/// max_total_gb     = 10
/// max_age_days     = 30
/// keep_per_origin  = 3
/// zstd_level       = 19
/// zstd_long_window = 27
/// ```
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CookConfig {
    /// Whether `soldr cargo ...` should pre-flight `CookLookup` against
    /// the daemon and hydrate `target/` on hit. The env var
    /// `SOLDR_COOK_AUTO_HYDRATE` and `rust-toolchain.toml`'s
    /// `[soldr.cook] auto_hydrate` setting override this. Default
    /// `true` (opt-out).
    #[serde(default = "CookConfig::default_auto_hydrate")]
    pub auto_hydrate: bool,
    /// Auto-GC size cap for `~/.soldr/cache/cook/`, in gibibytes.
    #[serde(default = "CookConfig::default_max_total_gb")]
    pub max_total_gb: u64,
    /// Auto-GC time bound; entries older than this are evicted unless
    /// protected by `keep_per_origin`.
    #[serde(default = "CookConfig::default_max_age_days")]
    pub max_age_days: u64,
    /// Auto-GC per-origin protection: the N newest entries for each
    /// normalized `origin_url` are always retained, even under size
    /// pressure.
    #[serde(default = "CookConfig::default_keep_per_origin")]
    pub keep_per_origin: u32,
    /// zstd compression level for cook artifacts. The default matches
    /// `cache_lib::cook_archive::COOK_ZSTD_LEVEL`. Surface here lets
    /// power users tune the trade-off; PR 3 reads the constant.
    #[serde(default = "CookConfig::default_zstd_level")]
    pub zstd_level: i32,
    /// zstd `--long=N` window log. The default matches
    /// `cache_lib::cook_archive::COOK_ZSTD_LONG_WINDOW`.
    #[serde(default = "CookConfig::default_zstd_long_window")]
    pub zstd_long_window: u32,
}

impl CookConfig {
    pub const fn default_auto_hydrate() -> bool {
        true
    }
    pub const fn default_max_total_gb() -> u64 {
        10
    }
    pub const fn default_max_age_days() -> u64 {
        30
    }
    pub const fn default_keep_per_origin() -> u32 {
        3
    }
    pub const fn default_zstd_level() -> i32 {
        19
    }
    pub const fn default_zstd_long_window() -> u32 {
        27
    }
}

impl Default for CookConfig {
    fn default() -> Self {
        Self {
            auto_hydrate: Self::default_auto_hydrate(),
            max_total_gb: Self::default_max_total_gb(),
            max_age_days: Self::default_max_age_days(),
            keep_per_origin: Self::default_keep_per_origin(),
            zstd_level: Self::default_zstd_level(),
            zstd_long_window: Self::default_zstd_long_window(),
        }
    }
}

/// `gc` section of `config.toml`.
///
/// ```toml
/// [gc]
/// allowlist_roots = ["~/dev", "/work/repos"]
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcConfig {
    /// Roots that `soldr gc` will consider for reclamation. If unset
    /// (or empty after expansion), callers should fall back to
    /// `~/dev`. `~` is expanded to the user's home directory.
    #[serde(default)]
    pub allowlist_roots: Option<Vec<String>>,
}

/// `auto_gc` section of `config.toml` (issue #323 — automatic GC under
/// disk pressure).
///
/// ```toml
/// [auto_gc]
/// enabled = true            # opt-out; on by default
/// trigger_free_gb = 20      # start GC when free space < this
/// target_free_gb  = 30      # stop GC when free space >= this
/// min_age_secs    = 3600    # never touch files modified in this window
/// ```
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AutoGcConfig {
    /// Whether soldr should automatically reclaim disk space when free
    /// space on a soldr-relevant volume drops below `trigger_free_gb`.
    /// Defaults to `true` (opt-out).
    #[serde(default = "AutoGcConfig::default_enabled")]
    pub enabled: bool,
    /// Auto-GC fires when free space on a soldr-relevant volume drops
    /// below this threshold (in GiB).
    #[serde(default = "AutoGcConfig::default_trigger_free_gb")]
    pub trigger_free_gb: u64,
    /// Auto-GC stops escalating once free space reaches this number of
    /// GiB on the affected volume. Must be >= `trigger_free_gb`.
    #[serde(default = "AutoGcConfig::default_target_free_gb")]
    pub target_free_gb: u64,
    /// Floor applied uniformly to every age-based filter used by
    /// auto-GC. Files / directories modified within this many seconds
    /// of "now" are never touched.
    #[serde(default = "AutoGcConfig::default_min_age_secs")]
    pub min_age_secs: u64,
    /// Age-based eviction of idle workspace `target/` directories
    /// (soldr#4031). When non-zero, the background sweeper deletes every
    /// registered or discovered `target/` (under the GC allowlist roots)
    /// that has not been used for this many days, regardless of free
    /// space. `0` (the default) disables the pass.
    #[serde(default)]
    pub stale_target_days: u64,
}

impl AutoGcConfig {
    pub const fn default_enabled() -> bool {
        true
    }
    pub const fn default_trigger_free_gb() -> u64 {
        20
    }
    pub const fn default_target_free_gb() -> u64 {
        30
    }
    pub const fn default_min_age_secs() -> u64 {
        3600
    }
}

impl Default for AutoGcConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            trigger_free_gb: Self::default_trigger_free_gb(),
            target_free_gb: Self::default_target_free_gb(),
            min_age_secs: Self::default_min_age_secs(),
            stale_target_days: 0,
        }
    }
}

impl SoldrConfig {
    // Cold path (config load happens once); boxing the error is not worth it.
    #[allow(clippy::result_large_err)]
    pub fn load(path: &Path) -> Result<Self, SoldrConfigLoadError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(source) => {
                return Err(SoldrConfigLoadError::Read {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        toml::from_str(&text).map_err(|source| SoldrConfigLoadError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[derive(Debug, Error)]
pub enum SoldrConfigLoadError {
    #[error("failed to read soldr config {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse soldr config {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

/// Resolve `$CARGO_HOME` if set and non-empty, otherwise `~/.cargo`.
/// Returns `None` when neither resolves cleanly.
pub fn resolve_cargo_home() -> Option<PathBuf> {
    if let Some(path) = non_empty_env_path(std::env::var_os(CARGO_HOME_ENV_VAR).as_deref()) {
        return Some(path);
    }
    home_dir().ok().map(|home| home.join(".cargo"))
}

/// Resolve `$RUSTUP_HOME` if set and non-empty, otherwise `~/.rustup`.
/// Returns `None` when neither resolves cleanly.
pub fn resolve_rustup_home() -> Option<PathBuf> {
    if let Some(path) = non_empty_env_path(std::env::var_os(RUSTUP_HOME_ENV_VAR).as_deref()) {
        return Some(path);
    }
    home_dir().ok().map(|home| home.join(".rustup"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paths() {
        let paths = SoldrPaths::from_root_env_value(None).unwrap();
        // soldr#1597 Phase 1: the test binary is always a dev build, so
        // this resolves to the dev-default root, not the official one.
        assert!(paths.root.ends_with(default_home_dir_name()));
        assert!(paths.bin.ends_with("bin"));
        assert!(paths.cache.ends_with("cache"));
    }

    #[test]
    fn pinned_bin_survives_soldr_cache_dir_override() {
        // Issue #426: SOLDR_CACHE_DIR overrides the install root (and bin/
        // and cache/ along with it), but the pin must keep living at the
        // home-anchored ~/.soldr/bin/ so a pin registered against the
        // default cache dir stays visible to builds that re-root the cache.
        //
        // We exercise `from_env_root_and_home` directly so the test
        // doesn't have to mutate HOME / USERPROFILE — that would race with
        // every other case in this binary that reads them.
        let home_root = PathBuf::from("/synthetic-home/.soldr");
        let env_root = PathBuf::from("/synthetic-cache-dir");
        let paths =
            SoldrPaths::from_env_root_and_home(Some(env_root.clone()), Ok(home_root.clone()))
                .unwrap();

        assert_eq!(paths.root, env_root, "root follows SOLDR_CACHE_DIR");
        assert_eq!(
            paths.bin,
            env_root.join("bin"),
            "bin/ follows SOLDR_CACHE_DIR (managed binaries stay per-cache-dir)"
        );
        assert_eq!(
            paths.pinned_bin,
            home_root.join("bin"),
            "pinned_bin stays at $HOME/.soldr/bin/ regardless of SOLDR_CACHE_DIR"
        );
        assert_ne!(
            paths.bin, paths.pinned_bin,
            "the whole point of the fix: bin and pinned_bin diverge under env override"
        );
    }

    #[test]
    fn every_build_defaults_to_the_soldr_root() {
        // Dev and release builds share `~/.soldr`; daemon generations are
        // isolated by image hash, not by a second root.
        assert_eq!(default_home_dir_name(), ".soldr");
    }

    #[test]
    fn pinned_bin_equals_bin_when_no_cache_dir_override() {
        // No SOLDR_CACHE_DIR → root falls back to home → bin == pinned_bin.
        // No behavior change in the dominant case.
        let home_root = PathBuf::from("/synthetic-home/.soldr");
        let paths = SoldrPaths::from_env_root_and_home(None, Ok(home_root.clone())).unwrap();
        assert_eq!(paths.root, home_root);
        assert_eq!(paths.bin, home_root.join("bin"));
        assert_eq!(paths.pinned_bin, paths.bin);
    }

    #[test]
    fn pinned_bin_falls_back_to_env_root_when_no_home_available() {
        // Headless sandbox case: SOLDR_CACHE_DIR set but no $HOME /
        // USERPROFILE. pinned_bin must NOT panic — it falls back to the
        // env-rooted bin/ (loses cross-SOLDR_CACHE_DIR survival, but
        // matches today's behavior so nothing regresses).
        let env_root = PathBuf::from("/sandbox/.soldr");
        let paths =
            SoldrPaths::from_env_root_and_home(Some(env_root.clone()), Err(SoldrError::NoHomeDir))
                .unwrap();
        assert_eq!(paths.pinned_bin, env_root.join("bin"));
        assert_eq!(paths.pinned_bin, paths.bin);
    }

    #[test]
    fn pinned_bin_collapses_into_bin_with_synthetic_root() {
        // `SoldrPaths::with_root` is the test-construction path used by
        // integration tests. The pin dir must live inside the synthetic
        // root so test workspaces can register pins without escaping to
        // the host's home dir.
        let root = PathBuf::from("/tmp/synthetic-soldr-root");
        let paths = SoldrPaths::with_root(root.clone());
        assert_eq!(paths.bin, root.join("bin"));
        assert_eq!(paths.pinned_bin, paths.bin);
    }

    #[test]
    fn auto_gc_config_default_round_trip() {
        let cfg: SoldrConfig = toml::from_str("[auto_gc]\n").unwrap();
        assert_eq!(cfg.auto_gc, AutoGcConfig::default());
        assert!(cfg.auto_gc.enabled);
        assert_eq!(cfg.auto_gc.trigger_free_gb, 20);
        assert_eq!(cfg.auto_gc.target_free_gb, 30);
        assert_eq!(cfg.auto_gc.min_age_secs, 3600);
    }

    #[test]
    fn install_config_defaults_and_custom_values_parse() {
        // soldr#2310: [install] defaults to 2-day TTL / 5 GiB soft cap.
        let empty: SoldrConfig = toml::from_str("").unwrap();
        assert_eq!(empty.install, InstallConfig::default());
        assert_eq!(empty.install.source_ttl_days, 2);
        assert_eq!(empty.install.source_max_gb, 5);

        let custom: SoldrConfig =
            toml::from_str("[install]\nsource_ttl_days = 7\nsource_max_gb = 20\n").unwrap();
        assert_eq!(custom.install.source_ttl_days, 7);
        assert_eq!(custom.install.source_max_gb, 20);

        // Unknown keys are rejected (deny_unknown_fields).
        assert!(toml::from_str::<SoldrConfig>("[install]\nbogus = 1\n").is_err());
    }

    #[test]
    fn auto_gc_config_custom_values_parse() {
        let toml_text = r#"
[auto_gc]
enabled = false
trigger_free_gb = 10
target_free_gb = 50
min_age_secs = 7200
"#;
        let cfg: SoldrConfig = toml::from_str(toml_text).unwrap();
        assert!(!cfg.auto_gc.enabled);
        assert_eq!(cfg.auto_gc.trigger_free_gb, 10);
        assert_eq!(cfg.auto_gc.target_free_gb, 50);
        assert_eq!(cfg.auto_gc.min_age_secs, 7200);
    }

    #[test]
    fn missing_auto_gc_section_uses_defaults() {
        let cfg: SoldrConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.auto_gc, AutoGcConfig::default());
    }

    #[test]
    fn pins_section_parses_tool_versions() {
        let toml_text = r#"
[pins]
zccache = "1.12.5"
crgx    = "0.1.0"
"#;
        let cfg: SoldrConfig = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.pins.pin_for("zccache"), Some("1.12.5"));
        assert_eq!(cfg.pins.pin_for("crgx"), Some("0.1.0"));
        assert_eq!(cfg.pins.pin_for("unpinned-tool"), None);
    }

    #[test]
    fn pins_empty_pin_string_is_treated_as_unpinned() {
        let toml_text = r#"
[pins]
zccache = ""
"#;
        let cfg: SoldrConfig = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.pins.pin_for("zccache"), None);
    }

    #[test]
    fn missing_pins_section_uses_defaults() {
        let cfg: SoldrConfig = toml::from_str("").unwrap();
        assert!(cfg.pins.is_empty());
        assert_eq!(cfg.pins.pin_for("zccache"), None);
    }

    #[test]
    fn soldr_root_override_uses_env_path() {
        let root = soldr_root_from_env_var(Some(OsStr::new("C:\\temp\\soldr-cache-root")))
            .unwrap()
            .unwrap();
        assert_eq!(root, PathBuf::from("C:\\temp\\soldr-cache-root"));
    }

    #[test]
    fn soldr_root_override_ignores_empty_env() {
        assert!(soldr_root_from_env_var(Some(OsStr::new(""))).is_none());
    }

    // ---------------------------------------------------------------------
    // Per-version layout helpers (zackees/soldr#743 Phase A).
    //
    // Tests covering the just-added accessors. Per-test timeouts come from
    // cargo-nextest (`.config/nextest.toml`), not from anything declared here.
    // ---------------------------------------------------------------------

    #[test]
    fn managed_shim_version_matches_cargo_pkg_version() {
        // The const exists so callers can refer to it without duplicating
        // the version string. If anyone changes its source away from
        // `env!("CARGO_PKG_VERSION")`, this guard fires.
        assert_eq!(
            MANAGED_SHIM_VERSION,
            env!("CARGO_PKG_VERSION"),
            "MANAGED_SHIM_VERSION must equal the workspace package version"
        );
    }

    #[test]
    fn versioned_root_appends_v_prefix_and_pkg_version() {
        let root = PathBuf::from("/tmp/synthetic-versioned-root");
        let paths = SoldrPaths::with_root(root.clone());
        let v_root = paths.versioned_root();

        let expected_leaf = format!("v{}", MANAGED_SHIM_VERSION);
        assert_eq!(
            v_root,
            root.join(&expected_leaf),
            "versioned_root must be <root>/v<MANAGED_SHIM_VERSION>"
        );
        assert!(
            v_root.starts_with(&root),
            "versioned_root must live under root: {} not under {}",
            v_root.display(),
            root.display()
        );
        assert_eq!(
            v_root.file_name().and_then(|s| s.to_str()),
            Some(expected_leaf.as_str()),
            "versioned_root leaf must be `v<MANAGED_SHIM_VERSION>`"
        );
    }

    #[test]
    fn versioned_shims_dir_is_under_versioned_root() {
        let root = PathBuf::from("/tmp/synthetic-versioned-shims");
        let paths = SoldrPaths::with_root(root.clone());

        let v_root = paths.versioned_root();
        let shims = paths.versioned_shims_dir();

        assert_eq!(
            shims.parent(),
            Some(v_root.as_path()),
            "versioned_shims_dir parent must be versioned_root"
        );
        assert_eq!(
            shims.file_name().and_then(|s| s.to_str()),
            Some("shims"),
            "versioned_shims_dir leaf must be `shims`"
        );
        assert!(
            shims.starts_with(&v_root),
            "versioned_shims_dir must live under versioned_root"
        );
    }

    #[test]
    fn config_load_distinguishes_missing_and_invalid() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing.toml");
        assert!(SoldrConfig::load(&missing).unwrap().auto_gc.enabled);

        let malformed = temp.path().join("malformed.toml");
        std::fs::write(&malformed, "[auto_gc\nenabled = true").unwrap();
        assert!(matches!(
            SoldrConfig::load(&malformed),
            Err(SoldrConfigLoadError::Parse { .. })
        ));

        let unreadable = temp.path().join("directory");
        std::fs::create_dir(&unreadable).unwrap();
        assert!(matches!(
            SoldrConfig::load(&unreadable),
            Err(SoldrConfigLoadError::Read { .. })
        ));
    }

    #[test]
    fn config_load_rejects_unknown_destructive_fields() {
        let temp = tempfile::tempdir().unwrap();
        for (name, text) in [
            (
                "gc.toml",
                "[gc]\nallowlist_roots = [\"/tmp\"]\nunknown = true\n",
            ),
            ("auto.toml", "[auto_gc]\nenabled = true\nunknown = true\n"),
            ("cook.toml", "[cook]\nauto_hydrate = true\nunknown = true\n"),
        ] {
            let path = temp.path().join(name);
            std::fs::write(&path, text).unwrap();
            assert!(
                SoldrConfig::load(&path).is_err(),
                "{name} should reject unknown fields"
            );
        }
    }
}
