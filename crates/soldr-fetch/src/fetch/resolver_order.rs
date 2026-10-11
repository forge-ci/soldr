//! `SOLDR_RESOLVER_ORDER` parsing (issue #873), split out of `fetch/mod.rs`
//! when the QuickInstall hop was added (soldr#3700).

use crate::core::SoldrError;

/// Env var (issue #873) controlling which resolver hops fire. Comma-
/// separated list of `embed`, `live`, `api`, `binstall`, `quickinstall` (e.g.
/// `SOLDR_RESOLVER_ORDER=live,api` to skip the embedded blob,
/// `SOLDR_RESOLVER_ORDER=api` to skip both manifest hops). Unset or
/// empty → all five hops fire in the default order. Tokens not in the
/// known-set are warned about and ignored; if no token is recognised the
/// value falls back to all hops, so a typo can never disable the
/// sha-pinned manifest hops. The `api` and `quickinstall` hops are gated too.
///
/// `quickinstall` (soldr#3700) runs only after the `api` hop misses (or is
/// excluded) for an exact version; QuickInstall publishes no checksum, so
/// its assets are trust-`unverified` unless `SOLDR_CHECKSUMS_FILE` pins them.
/// `binstall` (soldr#3726) runs just before `quickinstall` under the same
/// conditions and trust posture, from the crate's own binstall `pkg-url`.
///
/// The `api` hop is always last when listed — there is no way to
/// re-order the hops, only to disable them. This keeps the trust
/// posture intact: a hit from the embed/live manifest is sha-pinned;
/// a hit from the api path is not. Promoting the api path above either
/// manifest path would silently downgrade integrity.
pub const RESOLVER_ORDER_ENV_VAR: &str = "SOLDR_RESOLVER_ORDER";

/// Decoded form of [`RESOLVER_ORDER_ENV_VAR`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolverOrder {
    pub try_embed: bool,
    pub try_live: bool,
    /// When false, the unpinned GitHub Releases API hop is skipped; a manifest miss then fails with "no resolver hop permitted".
    pub try_api: bool,
    /// When false, the unpinned QuickInstall fallback hop (soldr#3700) is skipped.
    pub try_quickinstall: bool,
    /// When false, the unpinned `[package.metadata.binstall]` hop (soldr#3726) is skipped.
    pub try_binstall: bool,
}

impl ResolverOrder {
    /// All hops, in the canonical embed → live → api → binstall → quickinstall order.
    pub const fn all() -> Self {
        Self {
            try_embed: true,
            try_live: true,
            try_api: true,
            try_quickinstall: true,
            try_binstall: true,
        }
    }

    /// Parse `SOLDR_RESOLVER_ORDER` from the process environment.
    pub fn from_env() -> Self {
        match std::env::var(RESOLVER_ORDER_ENV_VAR) {
            Ok(raw) => Self::parse(&raw),
            Err(_) => Self::all(),
        }
    }

    /// Parse a comma-separated token list. Unknown tokens are warned about
    /// and ignored; empty input or input with no recognised token falls
    /// back to `Self::all()`.
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Self::all();
        }
        let mut order = Self {
            try_embed: false,
            try_live: false,
            try_api: false,
            try_quickinstall: false,
            try_binstall: false,
        };
        let mut recognised = false;
        for token in trimmed.split(',') {
            let tok = token.trim();
            match tok.to_ascii_lowercase().as_str() {
                "embed" => {
                    recognised = true;
                    order.try_embed = true;
                }
                "live" => {
                    recognised = true;
                    order.try_live = true;
                }
                "api" => {
                    recognised = true;
                    order.try_api = true;
                }
                "quickinstall" => {
                    recognised = true;
                    order.try_quickinstall = true;
                }
                "binstall" => {
                    recognised = true;
                    order.try_binstall = true;
                }
                "" => {}
                _ => eprintln!(
                    "soldr: warning: {RESOLVER_ORDER_ENV_VAR}: ignoring unknown resolver hop `{tok}` (known: embed, live, api, binstall, quickinstall)"
                ),
            }
        }
        if !recognised {
            eprintln!(
                "soldr: warning: {RESOLVER_ORDER_ENV_VAR}={trimmed:?} names no known hop; using all hops (embed,live,api,binstall,quickinstall)"
            );
            return Self::all();
        }
        order
    }
}

/// Refuse the unpinned GitHub Releases API hop when `SOLDR_RESOLVER_ORDER`
/// excludes `api` (issue #3640).
pub(crate) fn ensure_api_hop_permitted(
    order: ResolverOrder,
    cache_name: &str,
) -> Result<(), SoldrError> {
    if order.try_api {
        return Ok(());
    }
    Err(SoldrError::Other(format!(
        "no resolver hop permitted for {cache_name}: the embed/live manifest hops did not resolve it and {RESOLVER_ORDER_ENV_VAR} excludes `api` (the unpinned GitHub Releases API hop)"
    )))
}
