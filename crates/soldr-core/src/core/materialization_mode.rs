//! zccache cache-hit delivery mode (soldr#3407, zackees/zccache#1683,
//! soldr#3440, zackees/zccache#1792).
//!
//! zccache reads `ZCCACHE_MODE` (`AUTO` | `LINK` | `COPY` | `REFLINK` |
//! `REFLINK_OR_LINK_OR_COPY`) per request from the environment the rustc
//! wrapper forwards. soldr's job is only to decide which value that is and
//! put it on the cargo child, so the embedded service sees it on every
//! compile.
//!
//! # Precedence
//!
//! 1. `SOLDR_ZCCACHE_MODE` — soldr's own knob; `--zccache-mode` publishes it.
//! 2. `[zccache] mode` in `config.toml`.
//! 3. The user's own `ZCCACHE_MODE` — honored as-is for an explicit `LINK`,
//!    `COPY`, `REFLINK`, or `REFLINK_OR_LINK_OR_COPY`, and never rewritten.
//! 4. Unset.
//!
//! # `AUTO` and unset: soldr probes once and picks the explicit mode (zccache#1792)
//!
//! As of zccache 1.15.0 (zccache#1792), zccache's own `AUTO` means reflink,
//! else an independent copy, for Rust outputs — it never hardlinks a Rust
//! artifact, closing the read-only-artifact bug in zccache#1791. (C/C++
//! outputs keep the older reflink → hardlink → copy chain under `AUTO`;
//! this soldr change is about the Rust-artifact path soldr itself wraps.)
//! Rather than ask zccache to retry a chain
//! (reflink → hardlink → copy) on every cache hit, soldr probes **once**,
//! before the build starts, whether the specific `(zccache cache dir,
//! cargo target dir)` pair supports a reflink or a hardlink
//! (`soldr_platform::fs::delivery_probe::probe_delivery_capability`),
//! and puts the single, explicit answer — `REFLINK`, `LINK`, or `COPY` — on
//! the cargo child. This applies whenever the resolved mode is `AUTO` —
//! from *any* tier, including the user's own literal `ZCCACHE_MODE=AUTO` —
//! or nothing is configured at all. An explicit `LINK`, `COPY`, `REFLINK`,
//! or `REFLINK_OR_LINK_OR_COPY` is honored unchanged. See
//! [`ResolvedZccacheMode::child_env_value`] and [`child_env_value_for`].
//!
//! `REFLINK_OR_LINK_OR_COPY` ships in zccache 1.15.0 as the new explicit
//! spelling of the old `AUTO` chain; soldr accepts it as a valid explicit
//! user/soldr value and simply forwards it, but never *injects* it itself
//! (it injects the probed, single mode instead). `REFLINK`, `LINK`, and
//! `COPY` already exist in the pinned zccache `=1.14.14`, so probing and
//! injecting one of those three needs no zccache version bump — see
//! soldr#3440 for the compatibility finding.
//!
//! Unlike [`crate::core::jobs`], an unrecognised value is an error rather
//! than a fall-through: silently delivering hardlinks to someone who asked
//! for `COPY` defeats the reason they asked. The embedded service only logs
//! an invalid `ZCCACHE_MODE`, so soldr rejects every tier, including the
//! user's own variable, before the build starts.

use serde::Deserialize;
use std::fmt;

/// soldr's own spelling of the mode knob. `--zccache-mode` publishes it.
pub const SOLDR_ZCCACHE_MODE_ENV_VAR: &str = "SOLDR_ZCCACHE_MODE";

/// The variable zccache itself reads, per request.
pub const ZCCACHE_MODE_ENV_VAR: &str = "ZCCACHE_MODE";

/// How the embedded zccache delivers a cache hit to its output path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZccacheMode {
    /// zccache's own default. As of zccache 1.15.0 this means reflink, else
    /// an independent copy, for Rust outputs — it never hardlinks a Rust
    /// artifact (zccache#1791/#1792); C/C++ outputs keep the older
    /// reflink → hardlink → copy chain. Soldr's resolver never injects
    /// `AUTO` itself: it probes the cache-to-target directory pair once and
    /// injects the concrete [`Self::Reflink`], [`Self::Link`], or
    /// [`Self::Copy`] answer instead (soldr#3440), whether the resolved
    /// mode was `AUTO` or nothing was configured at all.
    Auto,
    /// Hardlink eligible outputs.
    Link,
    /// Always an independent, writable byte copy.
    Copy,
    /// An independent copy-on-write clone; a copy where the volume cannot.
    Reflink,
    /// Reflink, else hardlink (when the output may share an inode), else
    /// copy — the former meaning of `AUTO` before zccache#1792, now its own
    /// explicit spelling. Ships in zccache 1.15.0; soldr accepts and
    /// forwards it unchanged as an explicit user/soldr value but never
    /// injects it itself.
    ReflinkOrLinkOrCopy,
}

impl ZccacheMode {
    pub const ALL: [Self; 5] = [
        Self::Auto,
        Self::Link,
        Self::Copy,
        Self::Reflink,
        Self::ReflinkOrLinkOrCopy,
    ];

    /// The canonical spelling zccache documents and soldr injects.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::Link => "LINK",
            Self::Copy => "COPY",
            Self::Reflink => "REFLINK",
            Self::ReflinkOrLinkOrCopy => "REFLINK_OR_LINK_OR_COPY",
        }
    }

    /// Case- and whitespace-insensitive. Empty means unset (`Ok(None)`);
    /// anything else that is not a mode is [`UnknownZccacheMode`].
    pub fn parse(raw: &str) -> Result<Option<Self>, UnknownZccacheMode> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        Self::ALL
            .into_iter()
            .find(|mode| trimmed.eq_ignore_ascii_case(mode.as_str()))
            .map(Some)
            .ok_or(UnknownZccacheMode)
    }
}

/// A value that names none of the five modes. [`resolve_zccache_mode_from`]
/// attaches the offending tier and value as [`InvalidZccacheMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownZccacheMode;

impl fmt::Display for ZccacheMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `[zccache]` section of `config.toml`.
///
/// ```toml
/// [zccache]
/// mode = "reflink"
/// ```
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ZccacheModeConfig {
    /// Cache-hit delivery mode. `None` falls through to the next tier.
    #[serde(default)]
    pub mode: Option<String>,
    /// Size cap for the embedded zccache artifact store, in gibibytes
    /// (soldr#4028). `None` uses [`DEFAULT_ZCCACHE_MAX_SIZE_GB`]; `0`
    /// restores zccache's dynamic disk-relative default. The
    /// `ZCCACHE_CACHE_SIZE_BYTES` / `ZCCACHE_CACHE_SIZE_PERCENT` env vars
    /// outrank this key.
    #[serde(default)]
    pub max_size_gb: Option<u64>,
}

/// Default embedded zccache artifact-store cap in GiB (soldr#4028). zccache's
/// own default (5% of the volume, clamped to 40-200 GiB) let a single store
/// reach 99 GiB on a large disk; a fixed cap keeps it bounded regardless of
/// volume size while still holding several toolchains' worth of artifacts.
pub const DEFAULT_ZCCACHE_MAX_SIZE_GB: u64 = 25;

impl ZccacheModeConfig {
    /// Configured artifact-store cap in bytes. `None` means "defer to
    /// zccache's dynamic default" (the user set `max_size_gb = 0`).
    pub fn max_size_bytes(&self) -> Option<u64> {
        match self.max_size_gb.unwrap_or(DEFAULT_ZCCACHE_MAX_SIZE_GB) {
            0 => None,
            gb => Some(gb.saturating_mul(1024 * 1024 * 1024)),
        }
    }
}

/// Where a resolved mode came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZccacheModeSource {
    SoldrEnv,
    Config,
    ZccacheEnv,
}

impl ZccacheModeSource {
    pub fn describe(self) -> &'static str {
        match self {
            Self::SoldrEnv => "SOLDR_ZCCACHE_MODE (or --zccache-mode)",
            Self::Config => "config.toml [zccache].mode",
            Self::ZccacheEnv => "ZCCACHE_MODE",
        }
    }

    /// Whether soldr owns this value and must put it on the cargo child.
    /// The user's own `ZCCACHE_MODE` already reaches zccache unaided.
    pub fn soldr_owned(self) -> bool {
        !matches!(self, Self::ZccacheEnv)
    }
}

/// The resolved mode plus where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedZccacheMode {
    pub mode: ZccacheMode,
    pub source: ZccacheModeSource,
}

impl ResolvedZccacheMode {
    /// The value soldr must set as `ZCCACHE_MODE` on the cargo child, if
    /// any. `probe` is called at most once, and only when the resolved mode
    /// is [`ZccacheMode::Auto`]: an explicit `LINK`/`COPY`/`REFLINK`/
    /// `REFLINK_OR_LINK_OR_COPY` never needs to probe the filesystem to
    /// decide. `probe` must return one of [`ZccacheMode::Reflink`],
    /// [`ZccacheMode::Link`], or [`ZccacheMode::Copy`] — the caller (the
    /// cache-to-target delivery capability probe) never has a reason to
    /// answer `Auto` or `ReflinkOrLinkOrCopy`.
    ///
    /// `AUTO` is decided regardless of `self.source`: even the user's own
    /// literal `ZCCACHE_MODE=AUTO` means "let the tool decide", so soldr's
    /// probed answer applies there too (zccache#1792). Any other mode keeps
    /// the original rule: only a soldr-owned source is injected; the user's
    /// own explicit `ZCCACHE_MODE` already reaches zccache unaided.
    pub fn child_env_value(self, probe: impl FnOnce() -> ZccacheMode) -> Option<&'static str> {
        if self.mode == ZccacheMode::Auto {
            return Some(probe().as_str());
        }
        self.source.soldr_owned().then(|| self.mode.as_str())
    }
}

/// [`ResolvedZccacheMode::child_env_value`], extended to the "nothing
/// configured at all" case: unset also means "soldr decides" (zccache#1792).
/// `probe` is called at most once, only when a probe-driven decision is
/// actually needed.
pub fn child_env_value_for(
    resolved: Option<ResolvedZccacheMode>,
    probe: impl FnOnce() -> ZccacheMode,
) -> Option<&'static str> {
    match resolved {
        None => Some(probe().as_str()),
        Some(resolved) => resolved.child_env_value(probe),
    }
}

/// A configured value that is not one of the five modes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidZccacheMode {
    pub source: ZccacheModeSource,
    pub value: String,
}

impl fmt::Display for InvalidZccacheMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = ZccacheMode::ALL
            .iter()
            .map(|mode| mode.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "soldr: invalid {} value {:?}: expected one of {names}",
            self.source.describe(),
            self.value
        )
    }
}

impl std::error::Error for InvalidZccacheMode {}

/// Resolve the mode from an explicit set of inputs. Pure, so the precedence
/// is unit-testable without touching the process environment.
pub fn resolve_zccache_mode_from(
    soldr_env: Option<&str>,
    config: Option<&str>,
    zccache_env: Option<&str>,
) -> Result<Option<ResolvedZccacheMode>, InvalidZccacheMode> {
    let tiers = [
        (soldr_env, ZccacheModeSource::SoldrEnv),
        (config, ZccacheModeSource::Config),
        (zccache_env, ZccacheModeSource::ZccacheEnv),
    ];
    for (value, source) in tiers {
        let Some(value) = value else { continue };
        match ZccacheMode::parse(value) {
            Ok(Some(mode)) => return Ok(Some(ResolvedZccacheMode { mode, source })),
            Ok(None) => continue,
            Err(UnknownZccacheMode) => {
                return Err(InvalidZccacheMode {
                    source,
                    value: value.to_string(),
                })
            }
        }
    }
    Ok(None)
}

/// [`resolve_zccache_mode_from`] against the live environment and the
/// caller's parsed config.
pub fn resolve_zccache_mode(
    config: Option<&str>,
) -> Result<Option<ResolvedZccacheMode>, InvalidZccacheMode> {
    let soldr = std::env::var(SOLDR_ZCCACHE_MODE_ENV_VAR).ok();
    let zccache = std::env::var(ZCCACHE_MODE_ENV_VAR).ok();
    resolve_zccache_mode_from(soldr.as_deref(), config, zccache.as_deref())
}

#[cfg(test)]
#[path = "materialization_mode_tests.rs"]
mod tests;
