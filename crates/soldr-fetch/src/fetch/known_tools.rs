//! Registry of ecosystem tools with known GitHub Releases and (optional) cargo-subcommand mapping.
//!
//! Some crates have monorepo-style release tags (e.g. `cargo-audit/v0.21.0`) or
//! live in a repository whose name differs from the crate. Falling back to the
//! crates.io → GitHub repository lookup plus `/releases/latest` can pick up the
//! wrong release in those cases. When a tool needs per-release handling, encode
//! it here once and fetch paths can use it directly.

#[derive(Debug, Clone, Copy)]
pub struct ToolSpec {
    /// crates.io crate name and fetch cache key.
    pub crate_name: &'static str,
    /// Name used as `cargo <sub>`. `None` for tools that are not cargo
    /// subcommands (e.g. `cross`, `mdbook`, `sccache`).
    pub cargo_subcommand: Option<&'static str>,
    /// Binary shipped inside the release archive (no OS extension).
    pub binary_name: &'static str,
    /// Optional (owner, repo) override; skips the crates.io lookup when set.
    pub repo: Option<(&'static str, &'static str)>,
    /// Optional release-tag prefix used to filter monorepo releases, e.g.
    /// `"cargo-audit/"` to pick only `cargo-audit/v0.21.0`-style tags.
    pub tag_prefix: Option<&'static str>,
    /// Optional pinned release version (without the leading `v`). When set,
    /// fetches resolve to exactly this version instead of the upstream
    /// `latest` release. Used for tools whose upstream release stream has
    /// drifted away from the platform coverage soldr depends on (e.g.
    /// `cargo-chef` stopped publishing Windows/macOS archives after v0.1.73).
    pub pinned_version: Option<&'static str>,
    /// True when the outer `cargo <sub>` invocation will spawn an inner
    /// cargo build (or otherwise touch `target/` in a way that benefits
    /// from soldr's build-side hooks). Issue #824. Consumed by
    /// `cargo_front_door::subcommand::is_cacheable_cargo_subcommand`,
    /// which in turn gates the cook-hydrate / disk-watchdog / target-
    /// memo hooks.
    ///
    /// Note: this flag NO LONGER controls `RUSTC_WRAPPER=zccache`
    /// injection. The front door always sets `RUSTC_WRAPPER` when
    /// caching is enabled, regardless of the subcommand, so zccache
    /// observes every rustc call — even from build scripts spawned by
    /// `cargo metadata`, third-party plugins not registered here, etc.
    /// zccache's own "non-cacheable" classifier handles the read-only
    /// / non-hashable cases. This flag's role is narrower: "should the
    /// build-side hooks engage?", not "should we cache?".
    ///
    /// Static-analysis tools (`cargo-deny`, `cargo-audit`, `cargo-machete`)
    /// set this to `false` so soldr doesn't run cook hydrate or disk
    /// watchdog probes when there's no build coming. Build/link wrappers
    /// (`cargo-zigbuild`, `cargo-xwin`, `cargo-llvm-cov`,
    /// `cargo-semver-checks`, `cargo-binstall` via its `Compile`
    /// fallback, `cargo-udeps`, `cargo-expand`, `cargo-chef`,
    /// `cargo-nextest`) set this to `true`. `cargo-watch` is `false`
    /// here because its inner subcommand (`watch -x build`) is parsed
    /// out by `cargo_watch_inner_is_cacheable`.
    ///
    /// Tools without a `cargo_subcommand` mapping never consult this
    /// flag — they're top-level dispatches outside the cargo front door.
    pub wraps_inner_cargo_build: bool,
}

pub const KNOWN_TOOLS: &[ToolSpec] = &[
    // Phase 2 — test + security.
    ToolSpec {
        crate_name: "cargo-nextest",
        cargo_subcommand: Some("nextest"),
        binary_name: "cargo-nextest",
        repo: Some(("nextest-rs", "nextest")),
        tag_prefix: Some("cargo-nextest-"),
        pinned_version: Some(CARGO_NEXTEST_PINNED_VERSION),
        wraps_inner_cargo_build: true, // runs `cargo test`
    },
    ToolSpec {
        crate_name: "cargo-deny",
        cargo_subcommand: Some("deny"),
        binary_name: "cargo-deny",
        repo: Some(("EmbarkStudios", "cargo-deny")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: false, // dep-graph linter
    },
    ToolSpec {
        crate_name: "cargo-audit",
        cargo_subcommand: Some("audit"),
        binary_name: "cargo-audit",
        repo: Some(("rustsec", "rustsec")),
        tag_prefix: Some("cargo-audit/"),
        pinned_version: None,
        wraps_inner_cargo_build: false, // Cargo.lock scan against RustSec DB
    },
    ToolSpec {
        crate_name: "cargo-llvm-cov",
        cargo_subcommand: Some("llvm-cov"),
        binary_name: "cargo-llvm-cov",
        repo: Some(("taiki-e", "cargo-llvm-cov")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: true, // runs `cargo test`/`build` + chains RUSTC_WRAPPER
    },
    // Phase 3 — dev ergonomics.
    ToolSpec {
        crate_name: "cargo-udeps",
        cargo_subcommand: Some("udeps"),
        binary_name: "cargo-udeps",
        repo: Some(("est31", "cargo-udeps")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: true, // embeds cargo crate; inherits RUSTC_WRAPPER from parent env
    },
    ToolSpec {
        crate_name: "cargo-semver-checks",
        cargo_subcommand: Some("semver-checks"),
        binary_name: "cargo-semver-checks",
        repo: Some(("obi1kenobi", "cargo-semver-checks")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: true, // runs `cargo doc` for baseline + current
    },
    ToolSpec {
        crate_name: "cargo-expand",
        cargo_subcommand: Some("expand"),
        binary_name: "cargo-expand",
        repo: Some(("dtolnay", "cargo-expand")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: true, // calls `cargo rustc` which engages RUSTC_WRAPPER
    },
    ToolSpec {
        crate_name: "cargo-watch",
        cargo_subcommand: Some("watch"),
        binary_name: "cargo-watch",
        repo: Some(("watchexec", "cargo-watch")),
        tag_prefix: None,
        pinned_version: None,
        // false here: the outer `watch` subcommand isn't itself cacheable;
        // its inner -x <subcommand> is parsed by cargo_watch_inner_is_cacheable.
        wraps_inner_cargo_build: false,
    },
    // `cargo-chef` powers the `soldr cook` content-addressable dep-prebuild
    // (issue #359). Pinned to v0.1.73, then source-built into soldr release
    // archives so setup-soldr does not depend on upstream's incomplete
    // prebuilt matrix (notably no aarch64-apple-darwin asset).
    ToolSpec {
        crate_name: "cargo-chef",
        cargo_subcommand: Some("chef"),
        binary_name: "cargo-chef",
        repo: Some(("LukeMathWalker", "cargo-chef")),
        tag_prefix: None,
        pinned_version: Some(CARGO_CHEF_PINNED_VERSION),
        wraps_inner_cargo_build: true, // runs `cargo build` for the stub project
    },
    // Phase 4 — build + docs. None of these are cargo subcommands — they are
    // top-level tools invoked as `soldr cross ...`, `soldr mdbook ...`, etc.
    ToolSpec {
        crate_name: "cross",
        cargo_subcommand: None,
        binary_name: "cross",
        repo: Some(("cross-rs", "cross")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: false, // top-level dispatch; not a cargo subcommand
    },
    ToolSpec {
        crate_name: "mdbook",
        cargo_subcommand: None,
        binary_name: "mdbook",
        repo: Some(("rust-lang", "mdBook")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: false,
    },
    ToolSpec {
        crate_name: "cbindgen",
        cargo_subcommand: None,
        binary_name: "cbindgen",
        repo: Some(("mozilla", "cbindgen")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: false,
    },
    // Cross-compile front-ends (issue #598 child). Both are explicitly
    // documented in `docs/CROSS_COMPILE.md` as the recommended Rust ↔
    // Windows / Linux cross-compile path, but neither was registered in
    // this table — so the docs promised auto-fetch behavior the binary
    // did not actually deliver. `soldr cargo zigbuild build ...` and
    // `soldr cargo xwin build ...` should now Just Work.
    ToolSpec {
        crate_name: "cargo-zigbuild",
        cargo_subcommand: Some("zigbuild"),
        binary_name: "cargo-zigbuild",
        repo: Some(("rust-cross", "cargo-zigbuild")),
        tag_prefix: None,
        pinned_version: None,
        // Issue #824: this is the lane the bug was filed against.
        // cargo-zigbuild wraps `cargo build` with zig as the linker;
        // without RUSTC_WRAPPER set on the outer cargo, the inner
        // build runs uncached and consumers pay full cold-build cost.
        wraps_inner_cargo_build: true,
    },
    ToolSpec {
        crate_name: "cargo-xwin",
        cargo_subcommand: Some("xwin"),
        binary_name: "cargo-xwin",
        repo: Some(("rust-cross", "cargo-xwin")),
        tag_prefix: None,
        pinned_version: None,
        // Same shape as zigbuild — wraps an inner `cargo build` with a
        // custom linker. Without RUSTC_WRAPPER inherited from the outer
        // cargo, the inner build runs uncached.
        wraps_inner_cargo_build: true,
    },
    // Mindshare cargo subcommands (issue #598 child). Both ship pre-built
    // GitHub Releases assets for the targets soldr cares about.
    // `cargo-binstall` — https://github.com/cargo-bins/cargo-binstall.
    // Tags are plain `vX.Y.Z`. Assets ship in two flavors per target:
    // `cargo-binstall-<triple>.{zip,tgz}` (binary only) and
    // `.full.{zip,tgz}` (binary + minisign signatures). Either form
    // matches the bare-triple lookup the fetch chain performs.
    ToolSpec {
        crate_name: "cargo-binstall",
        cargo_subcommand: Some("binstall"),
        binary_name: "cargo-binstall",
        repo: Some(("cargo-bins", "cargo-binstall")),
        tag_prefix: None,
        pinned_version: None,
        // Default install strategies are `[CrateMetaData, QuickInstall,
        // Compile]`. The Compile fallback shells out to `cargo install`
        // for crates without a matching prebuilt — so RUSTC_WRAPPER on
        // the outer process pays off on that fallback path.
        wraps_inner_cargo_build: true,
    },
    // `cargo-machete` — https://github.com/bnjbvr/cargo-machete.
    // Tags are `vX.Y.Z`; asset names embed the version
    // (`cargo-machete-vX.Y.Z-<triple>.tar.gz`).
    // Audited 2026-10-12 (v0.9.2): x86_64 Linux is musl-only, which the
    // asset matcher accepts for a gnu target; there is no
    // `aarch64-pc-windows-msvc` asset, so that target falls through to the
    // next resolver. Pinned by `github::tests::
    // known_tools_audited_release_assets_select_documented_targets`.
    ToolSpec {
        crate_name: "cargo-machete",
        cargo_subcommand: Some("machete"),
        binary_name: "cargo-machete",
        repo: Some(("bnjbvr", "cargo-machete")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: false, // filesystem + regex scan, no compilation
    },
    // Phase 5 — web/wasm + cache. Top-level tools invoked directly.
    // `cargo-dylint` is the compiler-plugin runner used by `soldr lint`.
    // Dylint resolution is binary-only (soldr#2432): an absent or unusable
    // archive is an actionable error, never an implicit source build.
    ToolSpec {
        crate_name: "cargo-dylint",
        cargo_subcommand: Some("dylint"),
        binary_name: "cargo-dylint",
        repo: Some(("trailofbits", "dylint")),
        tag_prefix: None,
        pinned_version: Some("6.0.3"),
        wraps_inner_cargo_build: true, // Dylint runs cargo check-like builds.
    },
    // Companion linker required by every Dylint lint-library package.
    ToolSpec {
        crate_name: "dylint-link",
        cargo_subcommand: None,
        binary_name: "dylint-link",
        repo: Some(("trailofbits", "dylint")),
        tag_prefix: None,
        pinned_version: Some("6.0.3"),
        wraps_inner_cargo_build: false,
    },
    ToolSpec {
        crate_name: "wasm-pack",
        cargo_subcommand: None,
        binary_name: "wasm-pack",
        // Moved from `rustwasm/wasm-pack` to the `wasm-bindgen` org. The old
        // path still resolves today only because GitHub redirects renamed
        // repositories -- the API reports the new `html_url`, and an asset URL
        // built from the old name follows through to a 200. That redirect is
        // not a guarantee: it lapses if anyone recreates `rustwasm/wasm-pack`,
        // and then `soldr wasm-pack` fails at fetch time on a user's machine.
        repo: Some(("wasm-bindgen", "wasm-pack")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: false,
    },
    ToolSpec {
        crate_name: "trunk",
        cargo_subcommand: None,
        binary_name: "trunk",
        repo: Some(("trunk-rs", "trunk")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: false,
    },
    ToolSpec {
        crate_name: "sccache",
        cargo_subcommand: None,
        binary_name: "sccache",
        repo: Some(("mozilla", "sccache")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: false,
    },
    // `maturin` powers Python+Rust packaging. Two consumers:
    // 1. `soldr maturin <args>` — direct CLI dispatch for users running
    //    `soldr maturin develop` / `soldr maturin build` etc.
    // 2. The PEP 517 build backend in `src/soldr/__init__.py` — when a
    //    downstream pyproject sets `build-backend = "soldr"`, the shim
    //    shells out to `soldr maturin pep517 <hook>` for each PEP 517
    //    entry point. Pinning the version keeps the build backend's
    //    behavior reproducible across machines and CI runs.
    ToolSpec {
        crate_name: "maturin",
        cargo_subcommand: None,
        binary_name: "maturin",
        repo: Some(("zackees", "soldr-maturin")),
        tag_prefix: None,
        pinned_version: Some(super::MANAGED_MATURIN_VERSION),
        wraps_inner_cargo_build: false,
    },
    // Mindshare top-level tools (issue #598 child). Invoked as
    // `soldr bacon`, `soldr just`, `soldr typos`.
    // `bacon` — https://github.com/Canop/bacon. Tags are `vX.Y.Z`.
    // Audited 2026-10-12 (v3.26.0): upstream ships one multi-platform
    // `bacon_<version>.zip` with no target triple in its name, so no asset
    // matches and the fetch falls through to the next resolver.
    // Registering still wins the explicit (owner, repo) override.
    ToolSpec {
        crate_name: "bacon",
        cargo_subcommand: None,
        binary_name: "bacon",
        repo: Some(("Canop", "bacon")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: false,
    },
    // `just` — https://github.com/casey/just. Tags are bare
    // `X.Y.Z` (no `v` prefix). Asset names embed the version
    // (`just-X.Y.Z-<triple>.{tar.gz,zip}`).
    // Audited 2026-10-12 (1.58.0): Linux is musl-only, which the asset
    // matcher accepts for gnu targets, so every documented target has a
    // prebuilt.
    ToolSpec {
        crate_name: "just",
        cargo_subcommand: None,
        binary_name: "just",
        repo: Some(("casey", "just")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: false,
    },
    // `typos` — https://github.com/crate-ci/typos (the `typos-cli`
    // crate). Tags are `vX.Y.Z`; asset names embed the version
    // (`typos-vX.Y.Z-<triple>.{tar.gz,zip}`).
    // Audited 2026-10-12 (v1.51.1): Linux is musl-only, which the asset
    // matcher accepts for gnu targets, and `aarch64-pc-windows-msvc` now
    // ships, so every documented target has a prebuilt.
    ToolSpec {
        crate_name: "typos",
        cargo_subcommand: None,
        binary_name: "typos",
        repo: Some(("crate-ci", "typos")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: false,
    },
    // Self-trampoline: `soldr --as <version>` fetches this entry so an older
    // soldr binary can handle the rest of the invocation.
    ToolSpec {
        crate_name: "soldr",
        cargo_subcommand: None,
        binary_name: "soldr",
        repo: Some(("zackees", "soldr")),
        tag_prefix: None,
        pinned_version: None,
        wraps_inner_cargo_build: false,
    },
    // `crgx` is bundled into the combined soldr release archive
    // (auto-release.yml source-builds it from a pinned crgx tag), so
    // first-use needs no network round trip. The registry entry is the
    // fallback path when the bundle is unavailable (sideloaded soldr
    // binary, custom install, etc.) — it pins to the same upstream
    // version the bundle ships so `soldr crgx ...` is reproducible
    // across both paths. See SOLDR_CRGX_LOCAL_DIR for the runtime
    // override used by the npm shim and setup-soldr action.
    ToolSpec {
        crate_name: "crgx",
        cargo_subcommand: None,
        binary_name: "crgx",
        repo: Some(("yfedoseev", "crgx")),
        tag_prefix: None,
        pinned_version: Some(super::MANAGED_CRGX_VERSION),
        wraps_inner_cargo_build: false,
    },
];

/// Pinned `cargo-chef` release that `soldr cook` resolves by default. See the
/// `cargo-chef` entry in [`KNOWN_TOOLS`] for the rationale.
pub const CARGO_CHEF_PINNED_VERSION: &str = "0.1.73";

/// Exact cargo-nextest release consumed by the cross-build archive pipeline.
/// Keep this synchronized with the cargo-nextest Catalog published by
/// soldr-toolchain so target-run never resolves a moving latest release.
pub const CARGO_NEXTEST_PINNED_VERSION: &str = "0.9.140";

pub fn lookup_by_crate(crate_name: &str) -> Option<&'static ToolSpec> {
    KNOWN_TOOLS.iter().find(|t| t.crate_name == crate_name)
}

pub fn lookup_by_cargo_subcommand(sub: &str) -> Option<&'static ToolSpec> {
    KNOWN_TOOLS.iter().find(|t| t.cargo_subcommand == Some(sub))
}

/// Issue #824: true when the cargo subcommand `sub` belongs to a known
/// managed tool whose outer invocation will spawn an inner cargo build
/// (or otherwise transitively engage rustc) that benefits from
/// `RUSTC_WRAPPER=zccache` propagation.
///
/// Consumed by `cargo_front_door::subcommand::is_cacheable_cargo_subcommand`
/// to decide whether to set up the zccache session for a given
/// `soldr cargo <sub>` invocation. Returns `false` for unknown
/// subcommands (cargo built-ins and external tools soldr doesn't manage)
/// and for known static-analysis tools (`deny`, `audit`, `machete`,
/// `watch`) that don't transitively run rustc.
pub fn wraps_inner_cargo_build(sub: &str) -> bool {
    lookup_by_cargo_subcommand(sub)
        .map(|t| t.wraps_inner_cargo_build)
        .unwrap_or(false)
}

/// Every cargo subcommand soldr knows how to fetch a prebuilt binary
/// for, as a flat list of `&'static str` for use in the fuzzy-match
/// suggestion path (issue #412). Order matches `KNOWN_TOOLS`
/// declaration — which is also the tie-break order
/// `fuzzy_match::suggest_close_match` uses, so the deterministic
/// pick on equal-distance candidates stays predictable.
pub fn known_cargo_subcommands() -> Vec<&'static str> {
    KNOWN_TOOLS
        .iter()
        .filter_map(|t| t.cargo_subcommand)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `repo` override names a repository that still exists under that
    /// exact owner. This is the runtime fetch path: a stale pair does not fail
    /// at build time, it fails on a user's machine when they run the tool.
    ///
    /// Checked against the GitHub API when this was written -- 27 entries, one
    /// stale: `wasm-pack` had moved from `rustwasm` to `wasm-bindgen`. It kept
    /// working only through GitHub's rename redirect, which disappears the
    /// moment anyone recreates a repo at the old path.
    ///
    /// Deliberately offline: asserting the corrected pair rather than querying
    /// GitHub, so the suite stays hermetic. Re-audit with the API when adding
    /// entries; this only pins what was verified.
    #[test]
    fn repo_overrides_name_canonical_repositories() {
        assert_eq!(
            lookup_by_crate("wasm-pack").unwrap().repo,
            Some(("wasm-bindgen", "wasm-pack")),
            "wasm-pack moved orgs; the old path survives only by redirect"
        );

        // Every override must at least be well formed -- an empty half would
        // build a URL that 404s at fetch time.
        for tool in KNOWN_TOOLS {
            if let Some((owner, repo)) = tool.repo {
                assert!(
                    !owner.is_empty() && !repo.is_empty(),
                    "{} has an empty repo override half",
                    tool.crate_name
                );
                assert!(
                    !owner.contains('/') && !repo.contains('/'),
                    "{} splits owner/repo incorrectly: {owner:?}/{repo:?}",
                    tool.crate_name
                );
            }
        }
    }

    #[test]
    fn lookup_by_crate_finds_registered_tools() {
        assert_eq!(
            lookup_by_crate("cargo-nextest").unwrap().cargo_subcommand,
            Some("nextest")
        );
        assert_eq!(
            lookup_by_crate("cargo-llvm-cov").unwrap().binary_name,
            "cargo-llvm-cov"
        );
        assert_eq!(lookup_by_crate("mdbook").unwrap().cargo_subcommand, None);
        assert!(lookup_by_crate("not-a-tool").is_none());
    }

    #[test]
    fn lookup_by_cargo_subcommand_finds_registered_tools() {
        assert_eq!(
            lookup_by_cargo_subcommand("nextest").unwrap().crate_name,
            "cargo-nextest"
        );
        assert_eq!(
            lookup_by_cargo_subcommand("deny").unwrap().crate_name,
            "cargo-deny"
        );
        assert!(lookup_by_cargo_subcommand("build").is_none());
        // Tools with cargo_subcommand: None should never be returned here.
        assert!(lookup_by_cargo_subcommand("mdbook").is_none());
        assert!(lookup_by_cargo_subcommand("cross").is_none());
        assert!(lookup_by_cargo_subcommand("cbindgen").is_none());
    }

    #[test]
    fn cargo_audit_carries_monorepo_tag_prefix() {
        let spec = lookup_by_crate("cargo-audit").unwrap();
        assert_eq!(spec.tag_prefix, Some("cargo-audit/"));
    }

    #[test]
    fn known_tools_are_unique_by_crate_and_subcommand() {
        for (i, a) in KNOWN_TOOLS.iter().enumerate() {
            for b in KNOWN_TOOLS.iter().skip(i + 1) {
                assert_ne!(
                    a.crate_name, b.crate_name,
                    "duplicate crate_name in registry"
                );
                if let (Some(asub), Some(bsub)) = (a.cargo_subcommand, b.cargo_subcommand) {
                    assert_ne!(asub, bsub, "duplicate cargo_subcommand in registry");
                }
            }
        }
    }

    #[test]
    fn cargo_chef_is_registered_and_pinned() {
        // soldr cook (issue #359) is a shim around cargo-chef. Both the
        // registry entry and the pinned-version constant are part of the
        // public contract; if either changes, downstream users see a
        // different cargo-chef.
        let spec = lookup_by_crate("cargo-chef").expect("cargo-chef must be registered");
        assert_eq!(spec.cargo_subcommand, Some("chef"));
        assert_eq!(spec.binary_name, "cargo-chef");
        assert_eq!(spec.repo, Some(("LukeMathWalker", "cargo-chef")));
        assert_eq!(spec.pinned_version, Some(CARGO_CHEF_PINNED_VERSION));
        assert_eq!(CARGO_CHEF_PINNED_VERSION, "0.1.73");
        assert_eq!(
            lookup_by_cargo_subcommand("chef").map(|s| s.crate_name),
            Some("cargo-chef")
        );
    }

    #[test]
    fn cargo_nextest_is_registered_and_pinned() {
        let spec = lookup_by_crate("cargo-nextest").expect("cargo-nextest must be registered");
        assert_eq!(spec.cargo_subcommand, Some("nextest"));
        assert_eq!(spec.binary_name, "cargo-nextest");
        assert_eq!(spec.repo, Some(("nextest-rs", "nextest")));
        assert_eq!(spec.tag_prefix, Some("cargo-nextest-"));
        assert_eq!(spec.pinned_version, Some(CARGO_NEXTEST_PINNED_VERSION));
        assert_eq!(CARGO_NEXTEST_PINNED_VERSION, "0.9.140");
    }

    #[test]
    fn crgx_is_registered_and_pinned_to_managed_version() {
        // soldr bundles crgx into the combined release archive (see
        // auto-release.yml's `Build crgx from pinned source` step).
        // The registry entry MUST pin to `MANAGED_CRGX_VERSION` so the
        // fallback fetch path resolves to the same version the bundle
        // ships. If these drift apart, `soldr crgx` from a sideloaded
        // binary lands on a different version than the bundled one —
        // exactly the cross-source inconsistency the pin is meant to
        // prevent.
        let spec = lookup_by_crate("crgx").expect("crgx must be registered");
        assert_eq!(spec.cargo_subcommand, None);
        assert_eq!(spec.binary_name, "crgx");
        assert_eq!(spec.repo, Some(("yfedoseev", "crgx")));
        assert_eq!(
            spec.pinned_version,
            Some(super::super::MANAGED_CRGX_VERSION)
        );
        assert_eq!(super::super::MANAGED_CRGX_VERSION, "0.1.0");
    }

    #[test]
    fn top_level_tools_are_registered_without_cargo_subcommand() {
        for crate_name in [
            "cross",
            "mdbook",
            "cbindgen",
            "wasm-pack",
            "trunk",
            "sccache",
            "maturin",
            "dylint-link",
        ] {
            let spec = lookup_by_crate(crate_name)
                .unwrap_or_else(|| panic!("missing registry entry for {crate_name}"));
            assert_eq!(spec.cargo_subcommand, None);
            assert!(spec.repo.is_some());
        }
    }

    #[test]
    fn maturin_is_registered_and_pinned_to_managed_version() {
        // The PEP 517 build backend in src/soldr/__init__.py shells out
        // to `soldr maturin pep517 <hook>`. If the registry entry drifts
        // away from MANAGED_MATURIN_VERSION the backend's behavior
        // changes silently across machines; the pin keeps it reproducible.
        let spec = lookup_by_crate("maturin").expect("maturin must be registered");
        assert_eq!(spec.cargo_subcommand, None);
        assert_eq!(spec.binary_name, "maturin");
        assert_eq!(spec.repo, Some(("zackees", "soldr-maturin")));
        assert_eq!(
            spec.pinned_version,
            Some(super::super::MANAGED_MATURIN_VERSION)
        );
    }

    #[test]
    fn cargo_zigbuild_is_registered_for_cross_compile_docs_alignment() {
        // docs/CROSS_COMPILE.md still names cargo-zigbuild for Apple and
        // Linux cross-compile lanes. The promise to auto-fetch it only
        // holds if the registry has the entry.
        let spec = lookup_by_crate("cargo-zigbuild").expect("cargo-zigbuild must be registered");
        assert_eq!(spec.cargo_subcommand, Some("zigbuild"));
        assert_eq!(spec.binary_name, "cargo-zigbuild");
        assert_eq!(spec.repo, Some(("rust-cross", "cargo-zigbuild")));
        assert_eq!(
            lookup_by_cargo_subcommand("zigbuild").map(|s| s.crate_name),
            Some("cargo-zigbuild")
        );
    }

    #[test]
    fn cargo_xwin_is_registered_for_cross_compile_docs_alignment() {
        // docs/CROSS_COMPILE.md names cargo-xwin as the recommended
        // Linux → Windows MSVC cross-compile front-end. Same docs-
        // alignment rationale as cargo-zigbuild above.
        let spec = lookup_by_crate("cargo-xwin").expect("cargo-xwin must be registered");
        assert_eq!(spec.cargo_subcommand, Some("xwin"));
        assert_eq!(spec.binary_name, "cargo-xwin");
        assert_eq!(spec.repo, Some(("rust-cross", "cargo-xwin")));
        assert_eq!(
            lookup_by_cargo_subcommand("xwin").map(|s| s.crate_name),
            Some("cargo-xwin")
        );
    }

    #[test]
    fn mindshare_cargo_subcommands_are_registered() {
        // Issue #598 child PR: `soldr cargo binstall ...` and
        // `soldr cargo machete ...` must resolve to the upstream
        // (owner, repo) overrides without a crates.io round-trip.
        let binstall =
            lookup_by_crate("cargo-binstall").expect("cargo-binstall must be registered");
        assert_eq!(binstall.cargo_subcommand, Some("binstall"));
        assert_eq!(binstall.binary_name, "cargo-binstall");
        assert_eq!(binstall.repo, Some(("cargo-bins", "cargo-binstall")));
        assert_eq!(
            lookup_by_cargo_subcommand("binstall").map(|s| s.crate_name),
            Some("cargo-binstall")
        );

        let machete = lookup_by_crate("cargo-machete").expect("cargo-machete must be registered");
        assert_eq!(machete.cargo_subcommand, Some("machete"));
        assert_eq!(machete.binary_name, "cargo-machete");
        assert_eq!(machete.repo, Some(("bnjbvr", "cargo-machete")));
        assert_eq!(
            lookup_by_cargo_subcommand("machete").map(|s| s.crate_name),
            Some("cargo-machete")
        );
    }

    #[test]
    fn mindshare_top_level_tools_are_registered() {
        // Issue #598 child PR: `soldr bacon`, `soldr just`, and
        // `soldr typos` must resolve to the upstream (owner, repo)
        // overrides. None of them are cargo subcommands.
        for (crate_name, repo, binary_name) in [
            ("bacon", ("Canop", "bacon"), "bacon"),
            ("just", ("casey", "just"), "just"),
            ("typos", ("crate-ci", "typos"), "typos"),
        ] {
            let spec = lookup_by_crate(crate_name)
                .unwrap_or_else(|| panic!("missing registry entry for {crate_name}"));
            assert_eq!(spec.cargo_subcommand, None);
            assert_eq!(spec.binary_name, binary_name);
            assert_eq!(spec.repo, Some(repo));
            // Top-level tools must NOT shadow a cargo subcommand.
            assert!(lookup_by_cargo_subcommand(crate_name).is_none());
        }
    }
}
