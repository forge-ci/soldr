//! Auto-GC under disk pressure (issue #323).
//!
//! This module owns the pure policy:
//!
//! * group soldr-relevant paths by volume,
//! * decide whether any volume is below the configured trigger,
//! * compute the clamped age floor that every tier must respect,
//! * advance through tiers stopping as soon as the target free-space
//!   threshold is reached.
//!
//! It does **not** perform any disk operations. Disk-free probing, the
//! shell-out to `cargo -Zgc clean gc`, and the soldr target purge live
//! in the CLI layer. That separation keeps this module unit-testable
//! with a mocked disk-free probe and path enumerator.
//!
//! See `docs/API.md` and `gh issue 323 zackees/soldr` for the
//! user-facing brief.
//!
//! ```text
//! Tier 1 — cargo `clean gc` with conservative ages (cargo defaults).
//! Tier 2 — soldr target/ purge (older_than = max(1h, config min_age),
//!          larger_than = 256MiB).
//! Tier 3 — cargo `clean gc` with aggressive ages
//!          (`--max-src-age=7d --max-crate-age=14d --max-git-co-age=7d`).
//! Tier 4 — warn and stop. Anything more aggressive requires explicit
//!          `soldr gc sweep --aggressive`.
//! ```
//!
//! All tier ages are clamped to be ≥ `min_age_secs` before being passed
//! to cargo or the soldr purge so we never accidentally torch a build
//! mid-flight.

use crate::core::AutoGcConfig;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// One GiB in bytes — the unit `AutoGcConfig::trigger_free_gb` and
/// `target_free_gb` are expressed in.
pub const GIB: u64 = 1024 * 1024 * 1024;

/// A path soldr cares about for auto-GC. The `kind` is informational
/// only; callers (e.g. the auto-GC orchestrator) decide which kinds to
/// pass to which tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoGcPath {
    pub kind: AutoGcPathKind,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AutoGcPathKind {
    /// `$CARGO_HOME` — registry/git caches, the `.global-cache` SQLite
    /// file. Touched indirectly by cargo's own GC.
    CargoHome,
    /// `$RUSTUP_HOME` — toolchains and update hashes. Never touched by
    /// auto-GC; included only so its volume contributes to disk-free
    /// computations.
    RustupHome,
    /// `~/.soldr/cache` — soldr's own artifact cache.
    SoldrCache,
    /// A workspace `target/` directory from the soldr registry.
    WorkspaceTarget,
}

/// Trait abstracting disk-free probing so tests can plug in a fixed
/// map of `volume_key -> free_bytes`.
pub trait DiskFreeProbe {
    /// Return free bytes on the volume that backs `path`, or `None`
    /// if the volume can't be resolved (missing path, permission
    /// error, etc.). Implementations should be cheap.
    fn free_bytes(&self, path: &Path) -> Option<u64>;
    /// Capacity of the same volume. Unknown capacity preserves the
    /// absolute GiB thresholds, including for older custom probes.
    fn capacity_bytes(&self, _path: &Path) -> Option<u64> {
        None
    }
}

/// Trait abstracting "what is this path's volume?" so the per-volume
/// grouping is testable without touching the filesystem. Real
/// implementations return e.g. the drive letter on Windows or the
/// `statvfs` `f_fsid` on Unix. Tests typically return the first
/// component of the path.
pub trait VolumeProbe {
    fn volume_key(&self, path: &Path) -> Option<String>;
}

/// A volume that is below the configured trigger threshold and has
/// soldr-owned paths on it. Returned from `plan_auto_gc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumePlan {
    pub volume_key: String,
    pub free_bytes: u64,
    pub trigger_bytes: u64,
    pub target_bytes: u64,
    pub paths: Vec<AutoGcPath>,
}

/// Validate the auto-GC config: `target_free_gb >= trigger_free_gb`.
/// Returns the (possibly-corrected) config and a list of warnings.
pub fn validate_config(config: &AutoGcConfig) -> (AutoGcConfig, Vec<String>) {
    let mut warnings = Vec::new();
    let mut out = config.clone();
    // Do not turn the adaptive default target into a custom absolute
    // value before capacity is known. The effective target is clamped by
    // free_space_thresholds after both policies have been resolved.
    let adaptive_target = out.target_free_gb == AutoGcConfig::default_target_free_gb()
        && out.target_free_percent.is_none();
    if out.target_free_gb < out.trigger_free_gb && !adaptive_target {
        warnings.push(format!(
            "auto_gc.target_free_gb ({}) < trigger_free_gb ({}); clamping target to trigger",
            out.target_free_gb, out.trigger_free_gb
        ));
        out.target_free_gb = out.trigger_free_gb;
    }
    for (name, value) in [
        ("trigger_free_percent", &mut out.trigger_free_percent),
        ("target_free_percent", &mut out.target_free_percent),
    ] {
        if value.is_some_and(|percent| percent > 100) {
            warnings.push(format!("auto_gc.{name} exceeds 100; clamping to 100"));
            *value = Some(100);
        }
    }
    (out, warnings)
}

/// Shared CLI/daemon pressure policy. Keep a proportional reserve on large
/// disks without changing an operator's custom absolute thresholds.
pub fn free_space_thresholds(config: &AutoGcConfig, capacity: Option<u64>) -> (u64, u64) {
    fn threshold(
        gib: u64,
        default_gib: u64,
        percent: Option<u8>,
        default_percent: u8,
        capacity: Option<u64>,
    ) -> u64 {
        let percent = percent
            .unwrap_or(if gib == default_gib {
                default_percent
            } else {
                0
            })
            .min(100);
        let proportional = capacity.map_or(0, |bytes| {
            // Divide first, including the remainder, to avoid overflow at
            // u64::MAX without losing the fractional threshold.
            bytes / 100 * u64::from(percent) + bytes % 100 * u64::from(percent) / 100
        });
        gib.saturating_mul(GIB).max(proportional)
    }
    let trigger = threshold(
        config.trigger_free_gb,
        AutoGcConfig::default_trigger_free_gb(),
        config.trigger_free_percent,
        10,
        capacity,
    );
    let target = threshold(
        config.target_free_gb,
        AutoGcConfig::default_target_free_gb(),
        config.target_free_percent,
        15,
        capacity,
    );
    (trigger, target.max(trigger))
}

/// Clamp every age (in seconds) to be at least `floor_secs`. Used to
/// enforce the `min_age_secs` floor on cargo and soldr purge ages.
pub fn clamp_age_to_floor(age_secs: u64, floor_secs: u64) -> u64 {
    age_secs.max(floor_secs)
}

/// Format a Cargo `clean gc` duration using Cargo's accepted grammar (#2241).
pub fn cargo_gc_duration_arg(seconds: u64) -> String {
    format!("{seconds} seconds")
}

/// Conservative tier-1 ages used when invoking cargo's `clean gc`.
/// Mirrors cargo's own defaults (~1 month for sources, ~3 months for
/// the compressed crate cache). The cargo CLI accepts these as
/// `--max-src-age` / `--max-crate-age` (etc.).
pub struct CargoGcAges {
    pub max_src_age_days: u64,
    pub max_crate_age_days: u64,
    pub max_index_age_days: u64,
    pub max_git_co_age_days: u64,
    pub max_git_db_age_days: u64,
    pub max_download_age_days: u64,
}

pub const TIER1_AGES: CargoGcAges = CargoGcAges {
    max_src_age_days: 30,
    max_crate_age_days: 90,
    max_index_age_days: 90,
    max_git_co_age_days: 30,
    max_git_db_age_days: 180,
    max_download_age_days: 30,
};

pub const TIER3_AGES: CargoGcAges = CargoGcAges {
    max_src_age_days: 7,
    max_crate_age_days: 14,
    max_index_age_days: 30,
    max_git_co_age_days: 7,
    max_git_db_age_days: 30,
    max_download_age_days: 7,
};

impl CargoGcAges {
    /// Convert each age to seconds, clamped to be at least
    /// `floor_secs`. Returned in a stable order matching the field
    /// definition.
    pub fn clamped_seconds(&self, floor_secs: u64) -> CargoGcAgeSeconds {
        let per_day = 86_400u64;
        CargoGcAgeSeconds {
            max_src: clamp_age_to_floor(self.max_src_age_days.saturating_mul(per_day), floor_secs),
            max_crate: clamp_age_to_floor(
                self.max_crate_age_days.saturating_mul(per_day),
                floor_secs,
            ),
            max_index: clamp_age_to_floor(
                self.max_index_age_days.saturating_mul(per_day),
                floor_secs,
            ),
            max_git_co: clamp_age_to_floor(
                self.max_git_co_age_days.saturating_mul(per_day),
                floor_secs,
            ),
            max_git_db: clamp_age_to_floor(
                self.max_git_db_age_days.saturating_mul(per_day),
                floor_secs,
            ),
            max_download: clamp_age_to_floor(
                self.max_download_age_days.saturating_mul(per_day),
                floor_secs,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoGcAgeSeconds {
    pub max_src: u64,
    pub max_crate: u64,
    pub max_index: u64,
    pub max_git_co: u64,
    pub max_git_db: u64,
    pub max_download: u64,
}

/// Identify volumes below the trigger threshold, group the supplied
/// soldr paths by volume, and emit a per-volume plan that the
/// orchestrator can iterate through.
///
/// The returned plans are ordered by ascending free-bytes so callers
/// reclaim the tightest-volume first.
pub fn plan_auto_gc<D: DiskFreeProbe, V: VolumeProbe>(
    config: &AutoGcConfig,
    paths: &[AutoGcPath],
    disk: &D,
    volumes: &V,
) -> Vec<VolumePlan> {
    if !config.enabled {
        return Vec::new();
    }
    // Group paths by volume key.
    let mut by_volume: BTreeMap<String, Vec<AutoGcPath>> = BTreeMap::new();
    for entry in paths {
        let Some(key) = volumes.volume_key(&entry.path) else {
            continue;
        };
        by_volume.entry(key).or_default().push(entry.clone());
    }

    // Probe free space and keep only volumes below the trigger.
    let mut plans: Vec<VolumePlan> = by_volume
        .into_iter()
        .filter_map(|(volume_key, paths)| {
            let probe_path = paths.first()?.path.clone();
            let free_bytes = disk.free_bytes(&probe_path)?;
            let (trigger_bytes, target_bytes) =
                free_space_thresholds(config, disk.capacity_bytes(&probe_path));
            if free_bytes >= trigger_bytes {
                return None;
            }
            Some(VolumePlan {
                volume_key,
                free_bytes,
                trigger_bytes,
                target_bytes,
                paths,
            })
        })
        .collect();
    plans.sort_by_key(|p| p.free_bytes);
    plans
}

/// Which tier should run next for a given volume? Returns `None` when
/// the volume has reached its target free-space, otherwise the tier
/// index (1, 2, 3) the orchestrator should invoke.
pub fn next_tier(current_free_bytes: u64, target_bytes: u64, last_tier_run: u8) -> Option<u8> {
    if current_free_bytes >= target_bytes {
        return None;
    }
    if last_tier_run >= 3 {
        return None;
    }
    Some(last_tier_run + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FixedDisk {
        per_volume: HashMap<String, u64>,
        volume_for: HashMap<PathBuf, String>,
        capacity: Option<u64>,
    }

    impl FixedDisk {
        fn new() -> Self {
            Self {
                per_volume: HashMap::new(),
                volume_for: HashMap::new(),
                capacity: None,
            }
        }

        fn add(&mut self, volume: &str, free_bytes: u64) -> &mut Self {
            self.per_volume.insert(volume.to_string(), free_bytes);
            self
        }

        fn map(&mut self, path: &Path, volume: &str) -> &mut Self {
            self.volume_for
                .insert(path.to_path_buf(), volume.to_string());
            self
        }
    }

    impl DiskFreeProbe for FixedDisk {
        fn free_bytes(&self, path: &Path) -> Option<u64> {
            let key = self.volume_for.get(path)?;
            self.per_volume.get(key).copied()
        }

        fn capacity_bytes(&self, _path: &Path) -> Option<u64> {
            self.capacity
        }
    }

    impl VolumeProbe for FixedDisk {
        fn volume_key(&self, path: &Path) -> Option<String> {
            self.volume_for.get(path).cloned()
        }
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn large_volume_plans_reclamation_before_absolute_floor() {
        let mut probe = FixedDisk::new();
        probe.capacity = Some(2000 * GIB);
        probe.add("C", 74 * GIB).map(&p("C:/cargo"), "C");
        let plans = plan_auto_gc(
            &AutoGcConfig::default(),
            &[AutoGcPath {
                kind: AutoGcPathKind::CargoHome,
                path: p("C:/cargo"),
            }],
            &probe,
            &probe,
        );
        assert_eq!(plans.len(), 1, "74 GiB free on 2 TiB is under pressure");
        assert_eq!(plans[0].trigger_bytes, 200 * GIB);
        assert_eq!(plans[0].target_bytes, 300 * GIB);
        assert_eq!(next_tier(250 * GIB, plans[0].target_bytes, 1), Some(2));
        assert_eq!(next_tier(300 * GIB, plans[0].target_bytes, 1), None);
    }

    #[test]
    fn headroom_preserves_absolute_overrides_and_probe_fallback() {
        let mut config = AutoGcConfig::default();
        assert_eq!(free_space_thresholds(&config, None), (20 * GIB, 30 * GIB));
        assert_eq!(
            free_space_thresholds(&config, Some(100 * GIB)),
            (20 * GIB, 30 * GIB)
        );
        config.trigger_free_gb = 50;
        config.target_free_gb = 80;
        assert_eq!(
            free_space_thresholds(&config, Some(2000 * GIB)),
            (50 * GIB, 80 * GIB)
        );
        config.trigger_free_percent = Some(10);
        config.target_free_percent = Some(15);
        assert_eq!(
            free_space_thresholds(&config, Some(2000 * GIB)),
            (200 * GIB, 300 * GIB)
        );
        config.trigger_free_percent = Some(0);
        config.target_free_percent = Some(0);
        assert_eq!(
            free_space_thresholds(&config, Some(2000 * GIB)),
            (50 * GIB, 80 * GIB)
        );
        config = AutoGcConfig {
            trigger_free_gb: 250,
            ..AutoGcConfig::default()
        };
        config = validate_config(&config).0;
        assert_eq!(
            free_space_thresholds(&config, Some(2000 * GIB)),
            (250 * GIB, 300 * GIB)
        );
        config = AutoGcConfig {
            target_free_gb: 50,
            ..AutoGcConfig::default()
        };
        assert_eq!(
            free_space_thresholds(&config, Some(2000 * GIB)),
            (200 * GIB, 200 * GIB)
        );
    }

    #[test]
    fn headroom_bounds_percentages_target_and_arithmetic() {
        let mut config = AutoGcConfig {
            trigger_free_percent: Some(100),
            target_free_percent: Some(1),
            ..AutoGcConfig::default()
        };
        assert_eq!(
            free_space_thresholds(&config, Some(u64::MAX)),
            (u64::MAX, u64::MAX)
        );
        config.trigger_free_gb = u64::MAX;
        assert_eq!(free_space_thresholds(&config, None), (u64::MAX, u64::MAX));
        config.trigger_free_percent = Some(101);
        let (validated, warnings) = validate_config(&config);
        assert_eq!(validated.trigger_free_percent, Some(100));
        assert!(!warnings.is_empty());
    }

    #[test]
    fn unreadable_free_space_does_not_plan_speculative_eviction() {
        let mut probe = FixedDisk::new();
        probe.capacity = Some(2000 * GIB);
        probe.map(&p("C:/cargo"), "C");
        assert!(plan_auto_gc(
            &AutoGcConfig::default(),
            &[AutoGcPath {
                kind: AutoGcPathKind::CargoHome,
                path: p("C:/cargo")
            }],
            &probe,
            &probe,
        )
        .is_empty());
    }

    #[test]
    fn disabled_config_returns_no_plans() {
        let mut probe = FixedDisk::new();
        probe.add("C", 1).map(&p("C:/cargo"), "C");
        let config = AutoGcConfig {
            enabled: false,
            ..AutoGcConfig::default()
        };
        let plans = plan_auto_gc(
            &config,
            &[AutoGcPath {
                kind: AutoGcPathKind::CargoHome,
                path: p("C:/cargo"),
            }],
            &probe,
            &probe,
        );
        assert!(plans.is_empty());
    }

    #[test]
    fn healthy_volume_is_excluded_even_when_other_below_trigger() {
        // Volume C has 5 GiB free (below 20 GiB trigger);
        // volume D has 100 GiB free (well above trigger).
        let mut probe = FixedDisk::new();
        probe
            .add("C", 5 * GIB)
            .add("D", 100 * GIB)
            .map(&p("C:/cargo"), "C")
            .map(&p("D:/work/repo/target"), "D");
        let config = AutoGcConfig::default();
        let plans = plan_auto_gc(
            &config,
            &[
                AutoGcPath {
                    kind: AutoGcPathKind::CargoHome,
                    path: p("C:/cargo"),
                },
                AutoGcPath {
                    kind: AutoGcPathKind::WorkspaceTarget,
                    path: p("D:/work/repo/target"),
                },
            ],
            &probe,
            &probe,
        );
        assert_eq!(plans.len(), 1, "only the below-trigger volume should plan");
        assert_eq!(plans[0].volume_key, "C");
        assert_eq!(plans[0].paths.len(), 1);
        assert_eq!(plans[0].paths[0].kind, AutoGcPathKind::CargoHome);
    }

    #[test]
    fn paths_are_grouped_by_volume() {
        let mut probe = FixedDisk::new();
        probe
            .add("C", 5 * GIB)
            .map(&p("C:/cargo"), "C")
            .map(&p("C:/work/a/target"), "C")
            .map(&p("C:/work/b/target"), "C");
        let config = AutoGcConfig::default();
        let plans = plan_auto_gc(
            &config,
            &[
                AutoGcPath {
                    kind: AutoGcPathKind::CargoHome,
                    path: p("C:/cargo"),
                },
                AutoGcPath {
                    kind: AutoGcPathKind::WorkspaceTarget,
                    path: p("C:/work/a/target"),
                },
                AutoGcPath {
                    kind: AutoGcPathKind::WorkspaceTarget,
                    path: p("C:/work/b/target"),
                },
            ],
            &probe,
            &probe,
        );
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].paths.len(), 3);
    }

    #[test]
    fn min_age_floor_clamps_aggressive_tiers() {
        // floor 1h = 3600s. Tier-3 aggressive max_src_age = 7d = 604_800s,
        // already > 1h, so it stays. But if floor were larger than 7d
        // (say 30d = 2_592_000s), the floor should win.
        let s = TIER3_AGES.clamped_seconds(3600);
        assert_eq!(s.max_src, 7 * 86_400);
        assert_eq!(s.max_crate, 14 * 86_400);
        let s = TIER3_AGES.clamped_seconds(30 * 86_400);
        assert_eq!(s.max_src, 30 * 86_400);
        assert_eq!(s.max_crate, 30 * 86_400);
    }

    #[test]
    fn cargo_gc_duration_arg_uses_cargo_accepted_syntax() {
        assert_eq!(cargo_gc_duration_arg(604_800), "604800 seconds");
    }

    #[test]
    fn clamp_age_floor_never_lowers_existing_value() {
        assert_eq!(clamp_age_to_floor(100, 50), 100);
        assert_eq!(clamp_age_to_floor(100, 200), 200);
        assert_eq!(clamp_age_to_floor(0, 3600), 3600);
    }

    #[test]
    fn next_tier_stops_when_target_reached() {
        let target = 30 * GIB;
        assert_eq!(next_tier(31 * GIB, target, 0), None);
        assert_eq!(next_tier(31 * GIB, target, 1), None);
    }

    #[test]
    fn next_tier_advances_until_three() {
        let target = 30 * GIB;
        let free = 5 * GIB;
        assert_eq!(next_tier(free, target, 0), Some(1));
        assert_eq!(next_tier(free, target, 1), Some(2));
        assert_eq!(next_tier(free, target, 2), Some(3));
        assert_eq!(next_tier(free, target, 3), None);
    }

    #[test]
    fn validate_config_clamps_target_below_trigger() {
        let config = AutoGcConfig {
            trigger_free_gb: 50,
            target_free_gb: 20,
            ..AutoGcConfig::default()
        };
        let (out, warnings) = validate_config(&config);
        assert_eq!(out.target_free_gb, 50);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn plans_are_sorted_by_ascending_free_bytes() {
        let mut probe = FixedDisk::new();
        probe
            .add("C", 5 * GIB)
            .add("D", GIB)
            .map(&p("C:/cargo"), "C")
            .map(&p("D:/work/target"), "D");
        let config = AutoGcConfig::default();
        let plans = plan_auto_gc(
            &config,
            &[
                AutoGcPath {
                    kind: AutoGcPathKind::CargoHome,
                    path: p("C:/cargo"),
                },
                AutoGcPath {
                    kind: AutoGcPathKind::WorkspaceTarget,
                    path: p("D:/work/target"),
                },
            ],
            &probe,
            &probe,
        );
        assert_eq!(plans.len(), 2);
        // D (1 GiB) comes before C (5 GiB).
        assert_eq!(plans[0].volume_key, "D");
        assert_eq!(plans[1].volume_key, "C");
    }
}
