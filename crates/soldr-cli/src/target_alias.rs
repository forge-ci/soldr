//! soldr#997 — friendly target aliases + Rust-triple passthrough.
//!
//! The native-chain front door of `soldr build --target …` accepts
//! either a soldr alias (`win-x64`, `mac-arm64`, `linux-x64-musl`, …)
//! or a real Rust target triple (`x86_64-pc-windows-msvc`, …).
//! Both routes converge on the resolved Rust triple plus the target OS
//! catalogue assets and linker environment needed before spawning cargo.
//! Workspace-dependent policies, including PyO3 target Python compatibility,
//! are resolved separately after target alias resolution.
//!
//! See [`resolve_soldr_target`] for the routing entry point.
//!
//! ## Grammar
//!
//! `<os>-<arch>[-<libc-or-abi>][-glibc_<major>_<minor>]`. Case-insensitive,
//! hyphen-separated. The optional glibc suffix (soldr#3806) is only legal on
//! GNU/Linux aliases and selects the same catalogue sysroot floor as the
//! `<triple>.<major>.<minor>` spelling (soldr#2139): `linux-x64-glibc_2_17`
//! resolves exactly like `x86_64-unknown-linux-gnu.2.17`. `glibc2.17` and
//! `glibc_2.17` are accepted too.
//! `arch` is always the short form: `x64` (= `x86_64` / `amd64`) and
//! `arm64` (= `aarch64`). 32-bit triples don't ship in soldr.
//!
//! Canonical aliases — the documented form — are listed in the
//! [`CANONICAL_ALIASES`] table below. Synonyms (`darwin-arm64`,
//! `apple-silicon`, `musl-x64`, …) all resolve silently through
//! [`SYNONYMS`].
//!
//! ## CLAUDE.md policy
//!
//! Bare cargo built-in verbs (`build`, `test`, `check`, `run`,
//! `bench`, `doc`, `fmt`, `clippy`) become soldr-native **only when
//! `--target` is present**. Without it they remain shorthand for
//! `cargo <verb>`. The explicit escape hatch is `soldr cargo <verb>`
//! which is always pure passthrough regardless of `--target`. See
//! the documentation in `crate::main` for where the routing decision
//! is made.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Canonical alias → Rust target triple table. 9 entries; matches
/// `crate::core::canonical_targets::CANONICAL_TARGETS` 1:1.
pub const CANONICAL_ALIASES: &[(&str, &str)] = &[
    ("win-x64", "x86_64-pc-windows-msvc"),
    ("win-x64-gnu", "x86_64-pc-windows-gnu"),
    ("win-arm64", "aarch64-pc-windows-msvc"),
    ("mac-x64", "x86_64-apple-darwin"),
    ("mac-arm64", "aarch64-apple-darwin"),
    ("linux-x64", "x86_64-unknown-linux-gnu"),
    ("linux-arm64", "aarch64-unknown-linux-gnu"),
    ("linux-x64-musl", "x86_64-unknown-linux-musl"),
    ("linux-arm64-musl", "aarch64-unknown-linux-musl"),
];

/// Replace soldr target aliases in cargo-style arguments with Rust triples.
///
/// The blessed build path prepares its toolchain from the resolved target and
/// then forwards the same argument vector to cargo. Normalizing that vector
/// keeps documented aliases from leaking through to cargo as unknown targets.
/// Unknown values are deliberately left alone so cargo can still accept custom
/// target specifications.
pub fn normalize_target_aliases_in_args(args: &mut [String]) {
    let mut index = 0;
    while index < args.len() {
        if let Some(input) = args[index].strip_prefix("--target=") {
            if let Some(triple) = target_for_known_alias(input) {
                args[index] = format!("--target={triple}");
            }
        } else if args[index] == "--target" && index + 1 < args.len() {
            if let Some(triple) = target_for_known_alias(&args[index + 1]) {
                args[index + 1] = triple.to_string();
            }
            index += 1;
        }
        index += 1;
    }
}

/// `args` with any `--target <triple>.<glibc>` reduced to the bare triple.
///
/// soldr#2139: the `.<glibc>` suffix is a soldr-level spelling selecting a
/// catalogue-backed GNU/Linux sysroot ABI floor. rustc has never heard of it,
/// so it must not reach a cargo child. This is applied at each place a cargo process is actually
/// spawned rather than once in the caller, because the blessed path spawns
/// two -- the build itself and `cargo fetch` -- and a rule enforced at the
/// boundary cannot be forgotten by a future third.
///
/// Borrows when there is nothing to strip, which is the overwhelmingly common
/// case, so the ordinary build path allocates nothing.
pub fn args_without_glibc_floor(args: &[String]) -> std::borrow::Cow<'_, [String]> {
    let needs_strip = args.iter().enumerate().any(|(index, arg)| {
        arg.strip_prefix("--target=")
            .map(|value| split_glibc_floor(value).is_some())
            .unwrap_or_else(|| {
                arg == "--target"
                    && args
                        .get(index + 1)
                        .is_some_and(|next| split_glibc_floor(next).is_some())
            })
    });
    if !needs_strip {
        return std::borrow::Cow::Borrowed(args);
    }
    let mut stripped = args.to_vec();
    strip_glibc_floor_in_args(&mut stripped);
    std::borrow::Cow::Owned(stripped)
}

/// In-place form of [`args_without_glibc_floor`].
pub fn strip_glibc_floor_in_args(args: &mut [String]) {
    let mut index = 0;
    while index < args.len() {
        if let Some(input) = args[index].strip_prefix("--target=") {
            if let Some((base, _)) = split_glibc_floor(input) {
                args[index] = format!("--target={base}");
            }
        } else if args[index] == "--target" && index + 1 < args.len() {
            if let Some((base, _)) = split_glibc_floor(&args[index + 1]) {
                args[index + 1] = base.to_string();
            }
            index += 1;
        }
        index += 1;
    }
}

fn target_for_known_alias(input: &str) -> Option<String> {
    // soldr#3806: an alias with a glibc suffix becomes the
    // `<triple>.<major>.<minor>` spelling the build path already floors and
    // strips (soldr#2139). An invalid suffix is left alone here and rejected
    // by the resolver/guard with a precise error.
    if let Ok(Some(versioned)) = alias_with_glibc_floor_as_triple(input) {
        return Some(versioned);
    }
    let lower = input.trim().to_ascii_lowercase();
    canonical_lookup(&lower)
        .or_else(|| synonym_lookup(&lower).and_then(canonical_lookup))
        .map(str::to_string)
}

/// Split `linux-x64-glibc_2_17` into (`linux-x64`, `2.17`).
///
/// Accepts `-glibc_<maj>_<min>`, `-glibc<maj>.<min>`, `-glibc_<maj>.<min>`
/// and `-glibc<maj>_<min>`. Returns `None` when there is no glibc suffix.
fn split_alias_glibc_suffix(lower: &str) -> Option<(&str, String)> {
    let (alias, suffix) = lower.rsplit_once("-glibc")?;
    let version = suffix.strip_prefix('_').unwrap_or(suffix);
    let (major, minor) = version.split_once(['_', '.'])?;
    let numeric = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit());
    (numeric(major) && numeric(minor)).then(|| (alias, format!("{major}.{minor}")))
}

/// soldr#3806: rewrite a glibc-suffixed alias to `<triple>.<major>.<minor>`.
///
/// `Ok(None)` when `input` carries no alias glibc suffix. An `Err` when the
/// alias part is not a GNU/Linux alias (`mac-x64-glibc_2_17`,
/// `linux-x64-musl-glibc_2_17`) or the floor is not catalogue-backed.
fn alias_with_glibc_floor_as_triple(input: &str) -> Result<Option<String>, AliasError> {
    let raw = input.trim();
    let lower = raw.to_ascii_lowercase();
    let Some((alias, floor)) = split_alias_glibc_suffix(&lower) else {
        return Ok(None);
    };
    let triple =
        canonical_lookup(alias).or_else(|| synonym_lookup(alias).and_then(canonical_lookup));
    match triple {
        Some(triple) if triple.ends_with("-linux-gnu") => {
            let versioned = format!("{triple}.{floor}");
            reject_glibc_versioned(&versioned).map_err(|_| AliasError::GlibcVersioned {
                input: raw.to_string(),
                base: triple.to_string(),
                version: floor.clone(),
            })?;
            Ok(Some(versioned))
        }
        Some(triple) => Err(AliasError::GlibcVersioned {
            input: raw.to_string(),
            base: triple.to_string(),
            version: floor,
        }),
        None => Ok(None),
    }
}

/// Synonym → canonical alias table. Every entry's value MUST appear
/// in [`CANONICAL_ALIASES`]'s key column. Tests enforce this.
const SYNONYMS: &[(&str, &str)] = &[
    // Windows
    ("win-x86_64", "win-x64"),
    ("win-amd64", "win-x64"),
    ("windows-x64", "win-x64"),
    ("windows-x86_64", "win-x64"),
    ("windows-amd64", "win-x64"),
    ("windows-x64-gnu", "win-x64-gnu"),
    ("windows-x86_64-gnu", "win-x64-gnu"),
    ("win-aarch64", "win-arm64"),
    ("windows-arm64", "win-arm64"),
    ("windows-aarch64", "win-arm64"),
    // soldr#3806: Windows has no supported ARM32 target, so the short
    // spelling is unambiguous.
    ("win-arm", "win-arm64"),
    ("windows-arm", "win-arm64"),
    // macOS
    ("mac-x86_64", "mac-x64"),
    ("mac-amd64", "mac-x64"),
    ("macos-x64", "mac-x64"),
    ("macos-x86_64", "mac-x64"),
    ("darwin-x64", "mac-x64"),
    ("darwin-x86_64", "mac-x64"),
    ("mac-aarch64", "mac-arm64"),
    ("macos-arm64", "mac-arm64"),
    ("macos-aarch64", "mac-arm64"),
    ("darwin-arm64", "mac-arm64"),
    ("darwin-aarch64", "mac-arm64"),
    ("apple-silicon", "mac-arm64"),
    // soldr#3806: there has never been an ARM32 Mac.
    ("mac-arm", "mac-arm64"),
    ("macos-arm", "mac-arm64"),
    ("darwin-arm", "mac-arm64"),
    // Linux glibc
    ("linux-x86_64", "linux-x64"),
    ("linux-amd64", "linux-x64"),
    ("linux-aarch64", "linux-arm64"),
    // Linux musl
    ("linux-x86_64-musl", "linux-x64-musl"),
    ("linux-amd64-musl", "linux-x64-musl"),
    ("musl-x64", "linux-x64-musl"),
    ("musl-x86_64", "linux-x64-musl"),
    ("linux-aarch64-musl", "linux-arm64-musl"),
    ("musl-arm64", "linux-arm64-musl"),
    ("musl-aarch64", "linux-arm64-musl"),
];

/// Special aliases that don't map directly to a fixed triple.
const SPECIAL_ALIASES: &[&str] = &["native", "host", "all"];

/// Result of resolving a target string. Carries both the resolved
/// Rust triple and the input form, so error messages can echo what
/// the user typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// What the user typed verbatim.
    pub input: String,
    /// Resolved Rust target triple, e.g. `x86_64-pc-windows-msvc`.
    pub rust_triple: String,
    /// True iff `input` was a soldr alias (canonical or synonym).
    /// False iff `input` was already a Rust triple.
    pub via_alias: bool,
    /// Requested glibc floor from a `<triple>.<major>.<minor>` input
    /// (soldr#2139). `rust_triple` is always the bare, rustc-legal triple, so
    /// this is the only place the floor survives resolution.
    pub glibc_floor: Option<String>,
}

/// Errors raised when resolution fails.
///
/// soldr#3390: `Display` deliberately carries NO command verb. `resolve_soldr_target`
/// is shared by every `--target`-accepting surface (`build`, `prepare`, `env`,
/// `cc`/`c++`, `wheel`, `lint`), and a hard-coded `soldr build --target` prefix
/// baked into the message was wrong for every surface except `build` itself.
/// Render an `AliasError` for display via [`AliasError::render`] or
/// [`AliasError::into_soldr_error`], naming the [`TargetSurface`] the caller
/// actually is. An unrendered `to_string()` is incomplete-but-true rather than
/// naming a command the user never ran.
#[derive(Debug, thiserror::Error)]
pub enum AliasError {
    #[error("`{input}`: not a known alias or Rust triple. Did you mean `{suggestion}`?")]
    Unknown { input: String, suggestion: String },
    #[error(
        "`{input}`: ambiguous — could mean ARM32 \
         (not supported) or ARM64. Use `{disambiguated}` explicitly."
    )]
    Ambiguous {
        input: String,
        disambiguated: String,
    },
    #[error(
        "`{input}`: 32-bit targets are not in soldr's \
         supported set. Did you mean `{suggestion}`?"
    )]
    Thirty2Bit { input: String, suggestion: String },
    #[error(
        "`{input}`: a glibc floor cannot be honoured for \
         `{base}`. soldr has no catalogue-backed GNU/Linux sysroot for that \
         target, so accepting the `.{version}` suffix would drop the requested \
         ABI floor. The current catalogue baseline is 2.17 for x86_64-unknown-linux-gnu \
         and aarch64-unknown-linux-gnu; use that floor or cargo-zigbuild directly. \
         Tracked in soldr#1060 / soldr#2139."
    )]
    GlibcVersioned {
        input: String,
        base: String,
        version: String,
    },
    #[error(
        "`all` is only accepted as the whole value of `soldr prepare --target all`; \
         name the targets explicitly."
    )]
    AllNotBuildable,
}

/// The command whose `--target` the user typed.
///
/// Every [`AliasError`] is rendered against exactly one of these (soldr#3390)
/// — there is no default surface, which is what keeps `AliasError`'s bare
/// `Display` verb-less: a caller that forgets to pick one gets an incomplete
/// message rather than a wrong command name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSurface {
    Build,
    Prepare,
    Env,
    Cc,
    Cxx,
    Wheel,
    Lint,
    Ld,
    Link,
}

impl TargetSurface {
    /// Every surface, for table-driven tests. Order matches the enum.
    pub const ALL: &'static [TargetSurface] = &[
        TargetSurface::Build,
        TargetSurface::Prepare,
        TargetSurface::Env,
        TargetSurface::Cc,
        TargetSurface::Cxx,
        TargetSurface::Wheel,
        TargetSurface::Lint,
        TargetSurface::Ld,
        TargetSurface::Link,
    ];

    /// The verb as it appears on the command line, e.g. `soldr {verb} --target`.
    pub fn verb(self) -> &'static str {
        match self {
            TargetSurface::Build => "build",
            TargetSurface::Prepare => "prepare",
            TargetSurface::Env => "env",
            TargetSurface::Cc => "cc",
            TargetSurface::Cxx => "c++",
            TargetSurface::Wheel => "wheel",
            TargetSurface::Lint => "lint",
            TargetSurface::Ld => "ld",
            TargetSurface::Link => "link",
        }
    }
}

impl AliasError {
    /// Canonical shape: `soldr <verb> --target <reason>`. No extra prefix —
    /// `main` already prepends `soldr: ` when it prints the top-level error.
    pub fn render(&self, surface: TargetSurface) -> String {
        format!("soldr {} --target {self}", surface.verb())
    }

    /// [`AliasError::render`], wrapped as the [`crate::core::SoldrError`]
    /// every call site actually needs to return.
    pub fn into_soldr_error(self, surface: TargetSurface) -> crate::core::SoldrError {
        crate::core::SoldrError::Other(self.render(surface))
    }
}

/// One-shot resolver — primary entry point. Pass whatever the user
/// typed; gets back a [`ResolvedTarget`] or an [`AliasError`] with
/// a suggested correction.
///
/// Resolution order:
/// 1. **Lowercase + trim** the input.
/// 2. **Canonical alias** — exact match against [`CANONICAL_ALIASES`].
/// 3. **Synonym** — lookup in [`SYNONYMS`] → canonical → triple.
/// 4. **Special: `native` / `host`** — current host's triple.
/// 5. **Special: `all`** — explicit error (caller must expand).
/// 6. **Rust triple passthrough** — accepted if `looks_like_rust_triple`.
/// 7. **Reject** with [`AliasError::Unknown`] including a
///    Jaro-Winkler best-match suggestion (reuses the `strsim` dep
///    already on the workspace).
pub fn resolve_soldr_target(input: &str) -> Result<ResolvedTarget, AliasError> {
    let raw = input.trim();
    let lower = raw.to_ascii_lowercase();

    // Ambiguity + 32-bit gates fire BEFORE alias lookup so users get
    // the targeted error rather than "unknown" with a fuzzy match.
    if let Some(disambiguated) = check_ambiguous(&lower) {
        return Err(AliasError::Ambiguous {
            input: raw.to_string(),
            disambiguated: disambiguated.to_string(),
        });
    }
    if let Some(sug) = check_thirty2_bit(&lower) {
        return Err(AliasError::Thirty2Bit {
            input: raw.to_string(),
            suggestion: sug.to_string(),
        });
    }
    // soldr#2139. cargo-zigbuild spells an old-glibc target as
    // `<triple>.<major>.<minor>`. Without this gate the suffixed form reaches the
    // sysroot table and fails as "no sqlite sysroot recipe for target
    // x86_64-unknown-linux-gnu.2.17", which reads like a hole in that table
    // rather than an unsupported request -- and invites "just strip the suffix",
    // which would ship a binary whose glibc floor is not the one that was asked
    // for. Say what soldr cannot do, and name the tool that can.
    reject_glibc_versioned(raw)?;

    // soldr#3806: `linux-x64-glibc_2_17` is the alias spelling of
    // `x86_64-unknown-linux-gnu.2.17`. Rewrite it to that triple form and
    // resolve again, so both spellings share one floor gate and one result.
    if let Some(versioned) = alias_with_glibc_floor_as_triple(raw)? {
        let mut resolved = resolve_soldr_target(&versioned)?;
        resolved.input = raw.to_string();
        resolved.via_alias = true;
        return Ok(resolved);
    }

    // soldr#2139: a supported floor survived the gate above. Resolve the base
    // triple through the ordinary path and carry the floor beside it --
    // `rust_triple` stays rustc-legal, so no caller can pass the suffixed
    // spelling to a tool that has never heard of it.
    if let Some((base, floor)) = split_glibc_floor(raw) {
        let mut resolved = resolve_soldr_target(base)?;
        resolved.input = raw.to_string();
        resolved.glibc_floor = Some(floor.to_string());
        return Ok(resolved);
    }

    // Special aliases
    if lower == "native" || lower == "host" {
        return Ok(ResolvedTarget {
            input: raw.to_string(),
            rust_triple: host_triple().to_string(),
            via_alias: true,
            glibc_floor: None,
        });
    }
    if lower == "all" {
        return Err(AliasError::AllNotBuildable);
    }

    // Canonical alias
    if let Some(triple) = canonical_lookup(&lower) {
        return Ok(ResolvedTarget {
            input: raw.to_string(),
            rust_triple: triple.to_string(),
            via_alias: true,
            glibc_floor: None,
        });
    }

    // Synonym → canonical → triple
    if let Some(canonical) = synonym_lookup(&lower) {
        let triple = canonical_lookup(canonical)
            .expect("synonyms table value MUST appear in CANONICAL_ALIASES — test enforced");
        return Ok(ResolvedTarget {
            input: raw.to_string(),
            rust_triple: triple.to_string(),
            via_alias: true,
            glibc_floor: None,
        });
    }

    // Rust triple passthrough — let it through if it looks like one
    if looks_like_rust_triple(&lower) {
        return Ok(ResolvedTarget {
            input: raw.to_string(),
            rust_triple: raw.to_string(),
            via_alias: false,
            glibc_floor: None,
        });
    }

    // Reject with a suggestion
    Err(AliasError::Unknown {
        input: raw.to_string(),
        suggestion: best_match_suggestion(&lower).to_string(),
    })
}

// ---- internal helpers -----------------------------------------

fn canonical_map() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| CANONICAL_ALIASES.iter().copied().collect())
}

fn synonym_map() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| SYNONYMS.iter().copied().collect())
}

fn canonical_lookup(input: &str) -> Option<&'static str> {
    canonical_map().get(input).copied()
}

fn synonym_lookup(input: &str) -> Option<&'static str> {
    synonym_map().get(input).copied()
}

/// True iff `input` superficially looks like a Rust target triple.
/// Loose check — 2+ hyphens, only ASCII alnum + `_` + `-` characters.
/// Rust's actual triple grammar is more relaxed than this, but
/// anything that passes the check is forwarded to cargo, which will
/// produce the real validation error if the triple is genuinely
/// unknown.
fn looks_like_rust_triple(input: &str) -> bool {
    if input.matches('-').count() < 2 {
        return false;
    }
    input
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Detect inputs that are ambiguous between 32-bit and 64-bit ARM.
/// Returns the suggested unambiguous alias.
fn check_ambiguous(input: &str) -> Option<&'static str> {
    match input {
        // armv7 Linux is a real, common target, so `linux-arm` stays
        // ambiguous; `win-arm` / `mac-arm` are synonyms (soldr#3806).
        "linux-arm" => Some("linux-arm64"),
        _ => None,
    }
}

/// Base triples whose glibc floor soldr can actually honour.
///
/// The catalogue-backed GNU Linux toolchain owns this capability. Keeping the
/// decision with the bundle map prevents target resolution from accepting a
/// floor that lifecycle preparation cannot enforce.
pub fn glibc_floor_is_supported(base: &str, floor: &str) -> bool {
    crate::fetch::gnu_linux_toolchain::supports_glibc_floor(base, floor)
}

/// Split `x86_64-unknown-linux-gnu.2.17` into its base triple and the
/// requested glibc floor.
///
/// Anchored on `-linux-gnu` rather than matching any trailing dotted number:
/// musl has no glibc notion, and the other platforms version their SDK
/// differently, so a dot after those is a typo and belongs in the
/// "did you mean" path instead.
///
/// Returns slices of `raw`, so the caller keeps the original casing.
pub fn split_glibc_floor(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.trim();
    let (base, version) = trimmed.split_once('.')?;
    if !base.to_ascii_lowercase().ends_with("-linux-gnu") {
        return None;
    }
    // Digits and dots only, starting with a digit -- a version, not any suffix.
    let looks_like_version = version.starts_with(|c: char| c.is_ascii_digit())
        && version.chars().all(|c| c.is_ascii_digit() || c == '.');
    looks_like_version.then_some((base, version))
}

/// Detect 32-bit-named inputs and suggest the 64-bit replacement.
/// Soldr deliberately doesn't ship 32-bit triples.
///
/// Reject `<triple>.<major>.<minor>` when the floor cannot be honoured.
///
/// Callable independently of full alias resolution, because the two surfaces
/// resolve targets differently: `soldr prepare` goes through
/// [`resolve_soldr_target`], while `soldr build` only runs
/// [`normalize_target_aliases_in_args`], which rewrites *known* aliases and
/// passes anything else through untouched. Routing `build` through the full
/// resolver instead would make it reject every target not in the alias table,
/// including legitimate custom triples -- so the narrow guard is the one that
/// can be shared safely.
///
/// soldr#2139: without this, `soldr build --target x86_64-unknown-linux-gnu.2.17`
/// reported "no sqlite sysroot recipe for target …" for each library in turn
/// and then *continued*, which reads as a hole in the sysroot table rather
/// than an unsupported request.
pub fn reject_glibc_versioned(raw: &str) -> Result<(), AliasError> {
    match split_glibc_floor(raw) {
        // soldr#2139: catalogue-backed GNU Linux sysroots honour this floor.
        // Everywhere else the original refusal stands.
        Some((base, floor)) if glibc_floor_is_supported(base, floor) => Ok(()),
        Some((base, version)) => Err(AliasError::GlibcVersioned {
            input: raw.to_string(),
            base: base.to_string(),
            version: version.to_string(),
        }),
        None => Ok(()),
    }
}

fn check_thirty2_bit(input: &str) -> Option<&'static str> {
    match input {
        "win-x86" | "windows-x86" | "win-i686" | "win-i386" => Some("win-x64"),
        "mac-x86" | "macos-x86" | "darwin-x86" | "mac-i686" => Some("mac-x64"),
        "linux-x86" | "linux-i686" | "linux-i386" => Some("linux-x64"),
        _ => None,
    }
}

/// Best Jaro-Winkler match among all canonical aliases + Rust triples
/// we recognize. Used for the "did you mean" suggestion in
/// [`AliasError::Unknown`].
fn best_match_suggestion(input: &str) -> &'static str {
    let mut best: (f64, &'static str) = (-1.0, "win-x64");
    for (alias, _) in CANONICAL_ALIASES {
        let score = strsim::jaro_winkler(input, alias);
        if score > best.0 {
            best = (score, alias);
        }
    }
    for &alias in SPECIAL_ALIASES {
        let score = strsim::jaro_winkler(input, alias);
        if score > best.0 {
            best = (score, alias);
        }
    }
    best.1
}

/// Host triple at runtime — used by `native`/`host` aliases.
fn host_triple() -> &'static str {
    // soldr ships a host-triple constant baked at build time via
    // env! in `core::target_triple` for the actual targeting logic.
    // For the alias resolver we just want the canonical form.
    // Runtime facts-based resolution covers the supported host set;
    // anything else falls back to a sensible default + lets cargo
    // error. The musl cases use the runtime libc probe (glibc/musl).
    use crate::platform::host::facts::{HostArch, HostLibc, HostOs};
    let info = crate::platform::host::facts::info();
    match (info.os, info.arch, info.libc) {
        (HostOs::Windows, HostArch::X86_64, _) => "x86_64-pc-windows-msvc",
        (HostOs::Windows, HostArch::Aarch64, _) => "aarch64-pc-windows-msvc",
        (HostOs::MacOs, HostArch::X86_64, _) => "x86_64-apple-darwin",
        (HostOs::MacOs, HostArch::Aarch64, _) => "aarch64-apple-darwin",
        (HostOs::Linux, HostArch::X86_64, HostLibc::Musl) => "x86_64-unknown-linux-musl",
        (HostOs::Linux, HostArch::Aarch64, HostLibc::Musl) => "aarch64-unknown-linux-musl",
        (HostOs::Linux, HostArch::X86_64, _) => "x86_64-unknown-linux-gnu",
        (HostOs::Linux, HostArch::Aarch64, _) => "aarch64-unknown-linux-gnu",
        _ => "x86_64-unknown-linux-gnu",
    }
}

#[cfg(test)]
#[path = "target_alias_tests.rs"]
mod tests;
