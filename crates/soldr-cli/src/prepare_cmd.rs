//! `soldr prepare --target <triple>` — uniform cross-compile toolchain bootstrap.
//!
//! Single CLI surface for every cross-compile target: same invocation
//! shape, only `--target` varies. Internally dispatches based on the
//! target triple:
//!
//! - `*-pc-windows-msvc` -> prepare the blessed MSVC toolchain stack,
//!   including LLVM, xwin SDK cache, and target-scoped compiler/linker
//!   env for `soldr build`, deferred `soldr cook`, and cargo-xwin
//!   consumers that honor `XWIN_CACHE_DIR`.
//! - `x86_64-pc-windows-gnu` on Windows x64 -> ensure managed
//!   MinGW-w64 GCC, prepend it to PATH, export target-scoped
//!   Cargo/cc-rs env, and materialize GNU-shaped syslib rows when
//!   present. Other hosts fail visibly instead of falling back to
//!   cargo-zigbuild for this target.
//! - `*-apple-darwin` → ensure the target-shaped Apple SDK and print
//!   `SDKROOT=<path>` so the caller can plumb it into `$GITHUB_ENV`.
//!   `soldr build --target` is the blessed Darwin cross-build path.
//! - All targets: `rustup target add <triple>`.
//!
//! Designed to collapse the per-step ad-hoc downloads in
//! `cross-compile-all-targets.yml` into a single "Preparing Cross
//! Compile Toolchain" step.
//!
//! Output goes to stdout (human-readable). When `--github-env` is set,
//! also appends `KEY=VALUE` lines (e.g. `SDKROOT=/opt/...`) to that
//! file so a GitHub-Actions runner can pick them up in $GITHUB_ENV.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::core::{InstallerWatchdogConfig, SoldrError, SoldrPaths};
use crate::fetch::xwin_cache::ensure_xwin_case_aliases;

pub const RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR: &str = "SOLDR_RUSTUP_TARGET_ADD_TIMEOUT_SECS";

pub(crate) use crate::prepare_github_env::{append_env, apply_blessed_prep_env};

/// Parse the `--target` argument into a list of triples.
///
/// Accepts three shapes — see `cli_args::Commands::Prepare::target` for
/// the user-facing documentation. The literal `all` is a sentinel that
/// callers must expand via `cargo_metadata_soldr::resolve_all_targets`;
/// the dispatch returns `Err(SentinelAll)` so callers branch explicitly.
///
/// Comma-separated entries are trimmed and empty tokens dropped; an
/// empty effective list errors instead of silently producing a zero-
/// target run.
pub fn parse_target_arg(target: &str) -> Result<ParsedTargetArg, SoldrError> {
    if target == "all" {
        return Ok(ParsedTargetArg::All);
    }
    if target.contains(',') {
        let parsed: Vec<String> = target
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if parsed.is_empty() {
            return Err(SoldrError::Other(
                "soldr prepare --target: comma-separated list was empty".to_string(),
            ));
        }
        return Ok(ParsedTargetArg::Explicit(parsed));
    }
    Ok(ParsedTargetArg::Explicit(vec![target.to_string()]))
}

/// Result of parsing the `--target` CLI argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedTargetArg {
    /// `--target all` — caller must resolve from `Cargo.toml`.
    All,
    /// One or more explicit triples (single or comma-separated form).
    Explicit(Vec<String>),
}

/// Top-level entry point for `soldr prepare --target <triple>`.
pub async fn run(
    target: String,
    github_env: Option<PathBuf>,
    save: Option<PathBuf>,
    restore: Option<PathBuf>,
) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let github_env_path = github_env.as_deref();

    // Classification is shared by dispatch and restore-state auditing.
    let attrs = classify_target(&target)?;

    eprintln!("soldr prepare: target={target}");
    if let Some(reld) = ensure_selected_reld(&paths).await? {
        eprintln!("soldr prepare: reld at {}", reld.display());
    }

    // Restore failures are non-fatal; normal preparation fills any gaps.
    // Always report which cached pieces survived.
    if let Some(archive) = restore.as_deref() {
        match restore_prepare_state(archive, &paths) {
            Ok(()) => eprintln!("soldr prepare: restored state from {}", archive.display()),
            Err(e) => eprintln!(
                "soldr prepare: warning: restore from {} failed: {e}; will re-download as needed",
                archive.display()
            ),
        }
        let report = expected_state_paths(&attrs, &paths)?;
        emit_restore_report(&report);
    }

    // Preparation fetches each target's independent assets concurrently.
    match attrs.os {
        TargetOs::Windows => match attrs.abi {
            Some(TargetAbi::Msvc) => {
                eprintln!("soldr prepare: dispatch=blessed-msvc");
                let prep = crate::target_lifecycle::prepare_target(&paths, &target, &[]).await?;
                if let Some((_, cache_dir)) = prep
                    .env
                    .iter()
                    .find(|(key, _)| key == crate::fetch::xwin_cache::XWIN_CACHE_DIR_ENV_VAR)
                {
                    eprintln!("soldr prepare: xwin cache at {cache_dir}");
                }
                apply_blessed_prep_env(github_env_path, &prep, &attrs.triple)?;
            }
            Some(TargetAbi::Gnu) => {
                eprintln!("soldr prepare: dispatch=mingw-w64-gcc+syslibs");
                let prep = crate::target_lifecycle::prepare_target(&paths, &target, &[]).await?;
                if let Some((_, root)) =
                    prep.env.iter().find(|(key, _)| key == "MINGW_W64_GCC_ROOT")
                {
                    eprintln!("soldr prepare: MinGW-w64 GCC at {root}");
                }
                apply_blessed_prep_env(github_env_path, &prep, &attrs.triple)?;
            }
            _ => unreachable!("classify_target rejects Windows without a supported ABI"),
        },
        TargetOs::Darwin => {
            // Darwin prepare must export the same target-scoped clang,
            // SDK, linker, LLVM, and cmake/ninja env as `soldr build`.
            // Deferred cook runs before the final build step in CI, so
            // `SDKROOT` alone still lets cc-rs/ring probe `/usr/bin/cc`
            // and fall back to the host Linux linker. Export exactly
            // the environment used by the blessed build path.
            eprintln!("soldr prepare: dispatch=blessed-darwin");
            let prep = crate::target_lifecycle::prepare_target(&paths, &target, &[]).await?;
            if let Some(sdk) = prep.sdkroot.as_ref() {
                eprintln!("soldr prepare: Apple SDK at {}", sdk.display());
                println!("SDKROOT={}", sdk.display());
            }
            apply_blessed_prep_env(github_env_path, &prep, &attrs.triple)?;
        }
        TargetOs::Linux => {
            eprintln!("soldr prepare: dispatch=blessed-linux");
            let prep = crate::target_lifecycle::prepare_target(&paths, &target, &[]).await?;
            apply_blessed_prep_env(github_env_path, &prep, &attrs.triple)?;
        }
    }

    // `--save`: capture the prepared state into a tar.zst that callers
    // can plug into `actions/cache@v4`'s save step. Subsequent CI runs
    // pass the same path to `--restore` and skip the live downloads.
    if let Some(archive) = save.as_deref() {
        save_prepare_state(archive, &paths, &target)?;
        eprintln!("soldr prepare: saved state to {}", archive.display());
    }

    eprintln!("soldr prepare: done");
    Ok(())
}

/// Fetch a pinned `reld` when the caller's project (env, config, or the
/// project's own `.cargo/config.toml`) selects it. `reld` is a host tool —
/// it links whatever's being built, so it is resolved against the host
/// triple regardless of `--target`.
///
/// Called exactly once per `soldr prepare` invocation (outside any
/// per-target loop, see `run()` ~L92) — `--target all` fans `run()` out
/// per triple at the `soldr_main_dispatch` call site, but the fetch itself
/// is idempotent (an already-downloaded `reld` short-circuits before any
/// network call), so repeated invocations across targets do not re-fetch.
async fn ensure_selected_reld(paths: &SoldrPaths) -> Result<Option<PathBuf>, SoldrError> {
    let host_triple = crate::core::TargetTriple::host()?.triple();
    let selection =
        crate::linker::resolve_project_choice_from_cwd(Some(&host_triple), paths, false)?;
    ensure_selected_reld_with(&selection, || crate::fetch::ensure_reld(paths)).await
}

/// Testable core of [`ensure_selected_reld`]: the fetch is injected so unit
/// tests can assert it is invoked exactly once (or zero times) without
/// touching the network.
async fn ensure_selected_reld_with<Fut>(
    selection: &crate::linker::ProjectLinkerSelection,
    fetch: impl FnOnce() -> Fut,
) -> Result<Option<PathBuf>, SoldrError>
where
    Fut: std::future::Future<Output = Result<PathBuf, SoldrError>>,
{
    if !selection.needs_reld() {
        return Ok(None);
    }
    match fetch().await {
        Ok(path) => Ok(Some(path)),
        // An explicit `reld` selection that fails to fetch is a hard error —
        // the caller asked for reld by name. An automatic/fast selection
        // that merely tried reld opportunistically falls back silently, the
        // same way `resolve_for_target`'s `Fast` arm does.
        Err(error) if selection.is_explicit() => Err(crate::linker::reld_fetch_error(error)),
        Err(_) => Ok(None),
    }
}

/// One row in the per-target post-restore validation report.
/// `present` is true when the expected path exists on disk; the
/// `path` field is the location consumers can grep for in logs.
#[derive(Debug, Clone)]
pub(crate) struct RestoreEntry {
    pub(crate) label: String,
    pub(crate) path: PathBuf,
    pub(crate) present: bool,
}

/// CPU architecture tag in a Rust target triple. Restricted to the
/// arches soldr's cross-compile bootstrap supports; anything else
/// causes `classify_target` to ERROR rather than falling through
/// to a no-op dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetArch {
    X86_64,
    Aarch64,
}

/// OS family in a Rust target triple. Limited to the cross-compile
/// destinations soldr supports today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOs {
    Windows,
    Darwin,
    Linux,
}

/// ABI suffix in a Rust target triple. `None` for darwin (no ABI
/// suffix in apple triples). `Msvc`/`Gnu`/`Musl` for the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAbi {
    Msvc,
    Gnu,
    Musl,
}

/// Result of classifying a Rust target triple into the soldr
/// bootstrap dispatch attributes. Replaces the ad-hoc
/// `target.ends_with("...")` checks with a single source-of-truth
/// classifier — every consumer (the dispatch in `run()`, the
/// restore-audit's `expected_state_paths`, future per-target cache
/// namespacing) reads off the same struct.
///
/// `classify_target` ERRORs on unknown triples rather than silently
/// returning an empty attribute set, so a typo in CI YAML surfaces
/// loudly instead of pretending the prepare succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetAttrs {
    /// The canonical triple as supplied.
    pub triple: String,
    pub arch: TargetArch,
    pub os: TargetOs,
    pub abi: Option<TargetAbi>,
    /// Needs Zig on PATH for a retained legacy preparation path. True for
    /// Darwin and musl; GNU Linux uses its catalogue-backed toolchain.
    pub needs_zig: bool,
    /// Needs the blessed MSVC CRT + Windows SDK cache under
    /// `~/.soldr/sdk/<triple>/xwin/<version>/`. True for
    /// `*-pc-windows-msvc`.
    pub needs_xwin_cache: bool,
    /// Needs the soldr-managed LLVM toolchain (clang / lld-link /
    /// llvm-lib). True for `*-pc-windows-msvc` (cargo-xwin uses it).
    pub needs_llvm_toolchain: bool,
    /// Needs the soldr-managed MinGW-w64 GCC toolchain on supported
    /// hosts. True for `x86_64-pc-windows-gnu`.
    pub needs_mingw_w64_gcc: bool,
    /// Needs the vendored Apple SDK (IOKit/CoreFoundation/...).
    /// True for `*-apple-darwin`.
    pub needs_apple_sdk: bool,
}

/// Registry of known arch tokens. The classifier scores user input
/// against every entry and picks the best match — so adding a new
/// alias (e.g. `("x86_AMD", TargetArch::X86_64)`) automatically
/// works without changing the matching logic. Order doesn't matter;
/// scoring is symmetric.
pub const KNOWN_ARCHES: &[(&str, TargetArch)] = &[
    ("x86_64", TargetArch::X86_64),
    ("aarch64", TargetArch::Aarch64),
];

/// Registry of known OS tokens. See `KNOWN_ARCHES`.
pub const KNOWN_OSES: &[(&str, TargetOs)] = &[
    ("windows", TargetOs::Windows),
    ("darwin", TargetOs::Darwin),
    ("linux", TargetOs::Linux),
];

/// Registry of known ABI tokens. See `KNOWN_ARCHES`.
pub const KNOWN_ABIS: &[(&str, TargetAbi)] = &[
    ("msvc", TargetAbi::Msvc),
    ("gnu", TargetAbi::Gnu),
    ("musl", TargetAbi::Musl),
];

/// Minimum fuzzy score (0.0–1.0) for a token to be accepted as a
/// match. Exact matches score 1.0; case-insensitive exact 0.99; so
/// any threshold below 0.99 still admits casing variation. Below
/// 0.95 admits one-character extensions like `x86_64h` (Haswell+)
/// silently routing to `x86_64` — confirmed against the rustc
/// target list. The current value rejects every observed close-but-
/// wrong arch in the corpus while still accepting case variation
/// and any registered alias as an exact match.
///
/// Why not "exact match only"? The registry-of-aliases design (see
/// `best_match`) explicitly wants a future `("x86_AMD",
/// TargetArch::X86_64)` entry to work without re-coding the match
/// arm. Fuzzy scoring with a high threshold gives us that property
/// AND case-insensitivity in one knob.
pub const FUZZY_MATCH_THRESHOLD: f64 = 0.95;

/// Score `input` against `candidate` for fuzzy triple-component
/// matching. Returns `1.0` for an exact match (case-insensitive
/// 0.99), otherwise a blend of common-prefix ratio (50%) and
/// Jaro-Winkler (50%) — prefix weighting matters because arch/os/abi
/// tokens are short, so a shared "x86_" prefix carries more signal
/// than character-level edit distance alone.
///
/// Example: `fuzzy_score("x86_AMD", "x86")` and `fuzzy_score(
/// "x86_AMD", "x86_AMD")` both return non-zero, but the exact match
/// scores higher (1.0 vs ~0.65). The user-driven design constraint
/// is: when multiple registry entries match an input, the best one
/// wins; the brittle `match input { "x86_64" => ... }` it replaces
/// returned `Err` for every input that wasn't an exact key.
pub fn fuzzy_score(input: &str, candidate: &str) -> f64 {
    if input == candidate {
        return 1.0;
    }
    let i = input.to_ascii_lowercase();
    let c = candidate.to_ascii_lowercase();
    if i == c {
        return 0.99;
    }
    let common = i.bytes().zip(c.bytes()).take_while(|(a, b)| a == b).count();
    let max_len = i.len().max(c.len()).max(1);
    let prefix_score = common as f64 / max_len as f64;
    let jw = strsim::jaro_winkler(&i, &c);
    0.5 * prefix_score + 0.5 * jw
}

/// Find the best-matching registry entry for `input`. Returns the
/// matched value plus the runner-up name + score (useful for error
/// messages). Errors when the best score is below
/// `FUZZY_MATCH_THRESHOLD` — the registry doesn't have a candidate
/// close enough to commit to.
pub fn best_match<T: Copy>(
    input: &str,
    registry: &[(&str, T)],
    kind: &str,
) -> Result<T, SoldrError> {
    let mut best: Option<(&str, T, f64)> = None;
    for (name, val) in registry {
        let s = fuzzy_score(input, name);
        let take = match best {
            None => true,
            Some((_, _, prev)) => s > prev,
        };
        if take {
            best = Some((name, *val, s));
        }
    }
    let (name, val, score) = best
        .ok_or_else(|| SoldrError::Other(format!("soldr prepare: empty {kind} registry — bug")))?;
    if score < FUZZY_MATCH_THRESHOLD {
        let supported: Vec<&str> = registry.iter().map(|(n, _)| *n).collect();
        return Err(SoldrError::Other(format!(
            "soldr prepare: {kind} `{input}` did not match any known {kind} \
             (closest: `{name}` score={score:.3}; supported: {})",
            supported.join(", ")
        )));
    }
    Ok(val)
}

/// Classify a Rust target triple into a `TargetAttrs`. Pattern is
/// the standard `<arch>-<vendor>-<os>[-<abi>]` triple structure
/// LLVM uses; `vendor` is ignored (always `pc`, `apple`, or
/// `unknown` for the targets we care about). Each component is
/// fuzzy-matched against its registry via `best_match`, so adding
/// a new alias to e.g. `KNOWN_ARCHES` automatically extends the
/// classifier without touching this function. Triples that don't
/// score above `FUZZY_MATCH_THRESHOLD` against any registry entry
/// surface as a hard error naming the closest candidate.
pub fn classify_target(triple: &str) -> Result<TargetAttrs, SoldrError> {
    // Tokenize on `-`. Accept `<arch>-<vendor>-<os>` (3 parts, darwin)
    // and `<arch>-<vendor>-<os>-<abi>` (4 parts, windows/linux).
    let parts: Vec<&str> = triple.split('-').collect();
    if parts.len() < 3 || parts.len() > 4 {
        return Err(SoldrError::Other(format!(
            "soldr prepare: unrecognized target triple shape: `{triple}` \
             (expected `<arch>-<vendor>-<os>[-<abi>]`)"
        )));
    }
    let arch = best_match(parts[0], KNOWN_ARCHES, "arch")?;
    let vendor = parts[1];
    let os = best_match(parts[2], KNOWN_OSES, "os")?;
    // Vendor must match the canonical (os, vendor) pair soldr ships
    // against. Without this check `x86_64-uwp-windows-msvc`,
    // `x86_64-win7-windows-msvc`, `x86_64-unikraft-linux-musl`, and
    // `x86_64-pc-linux-gnu` all get wrongly accepted because the
    // classifier only looked at arch/os/abi. See the rustc target
    // list — UWP, Windows 7, Unikraft, etc. are real targets we
    // don't support.
    let expected_vendor = match os {
        TargetOs::Windows => "pc",
        TargetOs::Darwin => "apple",
        TargetOs::Linux => "unknown",
    };
    if !vendor.eq_ignore_ascii_case(expected_vendor) {
        return Err(SoldrError::Other(format!(
            "soldr prepare: triple `{triple}` has vendor `{vendor}`; \
             expected `{expected_vendor}` for the {os:?} family"
        )));
    }
    let abi = match parts.get(3) {
        Some(s) => Some(best_match(s, KNOWN_ABIS, "abi")?),
        None => None,
    };
    // Shape constraint: darwin has no ABI suffix, windows/linux require
    // one. Reject mismatches here — fuzzy matching can't catch
    // cross-component invariants.
    match (os, abi) {
        (TargetOs::Darwin, Some(_)) => {
            return Err(SoldrError::Other(format!(
                "soldr prepare: `*-apple-darwin` triples take no ABI suffix; got `{triple}`"
            )));
        }
        (TargetOs::Windows, None) | (TargetOs::Linux, None) => {
            return Err(SoldrError::Other(format!(
                "soldr prepare: `{}` triples require an ABI suffix; got `{triple}`",
                match os {
                    TargetOs::Windows => "windows",
                    TargetOs::Linux => "linux",
                    TargetOs::Darwin => unreachable!(),
                }
            )));
        }
        // Disallow windows-musl and linux-msvc. Windows GNU is
        // supported for x86_64 only today; other Windows GNU-family
        // triples (aarch64 gnullvm, i686 gnu, etc.) remain explicit
        // follow-ups rather than silently accepting a no-op.
        (TargetOs::Windows, Some(TargetAbi::Musl)) | (TargetOs::Linux, Some(TargetAbi::Msvc)) => {
            return Err(SoldrError::Other(format!(
                "soldr prepare: unsupported os/abi combination in `{triple}` \
                 (supported: windows-msvc, x86_64 windows-gnu, darwin, linux-gnu, linux-musl)"
            )));
        }
        (TargetOs::Windows, Some(TargetAbi::Gnu)) if arch != TargetArch::X86_64 => {
            return Err(SoldrError::Other(format!(
                "soldr prepare: unsupported Windows GNU target `{triple}` \
                 (first-class MinGW provisioning currently supports only x86_64-pc-windows-gnu)"
            )));
        }
        _ => {}
    }
    Ok(TargetAttrs {
        triple: triple.to_string(),
        arch,
        os,
        abi,
        needs_zig: matches!(os, TargetOs::Darwin),
        needs_xwin_cache: matches!((os, abi), (TargetOs::Windows, Some(TargetAbi::Msvc))),
        needs_llvm_toolchain: matches!((os, abi), (TargetOs::Windows, Some(TargetAbi::Msvc))),
        needs_mingw_w64_gcc: matches!(
            (os, abi, arch),
            (TargetOs::Windows, Some(TargetAbi::Gnu), TargetArch::X86_64)
        ),
        needs_apple_sdk: matches!(os, TargetOs::Darwin),
    })
}

/// List the on-disk paths that `prepare --target <triple>` is
/// expected to populate. Used after `--restore` to surface a
/// present/missing summary; the dispatch below downloads anything
/// missing so the report is purely informational.
///
/// Paths are version-pinned where possible (e.g. LLVM 21.1.5 and the
/// managed GNU/musl compiler bundles) so a stale archive that's missing the current pin is
/// reported as "missing" even if an older version exists on disk.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) fn expected_state_paths(
    attrs: &TargetAttrs,
    paths: &SoldrPaths,
) -> Result<Vec<RestoreEntry>, SoldrError> {
    let mut entries = Vec::new();
    if attrs.needs_zig {
        let zig_dir = paths
            .bin
            .join(format!("zig-{}", crate::fetch::MANAGED_ZIG_VERSION));
        let present = zig_dir.join(".complete").is_file() || zig_dir.is_dir();
        entries.push(RestoreEntry {
            label: format!("zig {}", crate::fetch::MANAGED_ZIG_VERSION),
            path: zig_dir,
            present,
        });
    }
    if matches!(
        (attrs.os, attrs.abi),
        (TargetOs::Linux, Some(TargetAbi::Gnu))
    ) {
        let Some(target) =
            crate::fetch::gnu_linux_toolchain::GnuLinuxToolchainTarget::for_triple(&attrs.triple)
        else {
            return Err(SoldrError::UnsupportedPlatform(format!(
                "no catalogue-backed GNU/Linux toolchain is available for `{}`",
                attrs.triple
            )));
        };
        let bundle = paths
            .bin
            .join("syslib")
            .join("gnu-linux-toolchain")
            .join(crate::fetch::gnu_linux_toolchain::GNU_LINUX_TOOLCHAIN_VERSION)
            .join(target.slug());
        let package = bundle.join("package");
        entries.push(RestoreEntry {
            label: format!(
                "GNU/Linux toolchain {} ({})",
                crate::fetch::gnu_linux_toolchain::GNU_LINUX_TOOLCHAIN_VERSION,
                target.slug()
            ),
            present: bundle.join(".complete").is_file()
                && package
                    .join("bin")
                    .join(format!("{}-gcc", target.compiler_prefix()))
                    .is_file(),
            path: package,
        });
    }
    if matches!(
        (attrs.os, attrs.abi),
        (TargetOs::Linux, Some(TargetAbi::Musl))
    ) {
        let Some(target) =
            crate::fetch::musl_linux_toolchain::MuslLinuxToolchainTarget::for_triple(&attrs.triple)
        else {
            return Err(SoldrError::UnsupportedPlatform(format!(
                "no catalogue-backed musl/Linux toolchain is available for `{}`",
                attrs.triple
            )));
        };
        let bundle = paths
            .bin
            .join("syslib")
            .join("musl-linux-toolchain")
            .join(crate::fetch::musl_linux_toolchain::MUSL_LINUX_TOOLCHAIN_VERSION)
            .join(target.slug());
        let package = bundle.join("package");
        entries.push(RestoreEntry {
            label: format!(
                "musl/Linux toolchain {} ({})",
                crate::fetch::musl_linux_toolchain::MUSL_LINUX_TOOLCHAIN_VERSION,
                target.slug()
            ),
            present: bundle.join(".complete").is_file()
                && package
                    .join("bin")
                    .join(format!("{}-gcc", target.compiler_prefix()))
                    .is_file()
                && package
                    .join(target.compiler_prefix())
                    .join("lib/crt1.o")
                    .is_file(),
            path: package,
        });
    }
    if attrs.needs_llvm_toolchain {
        let llvm_dir = crate::fetch::managed_llvm_install_dir(paths);
        entries.push(RestoreEntry {
            label: format!("LLVM {}", crate::fetch::MANAGED_LLVM_VERSION),
            present: llvm_dir.is_dir(),
            path: llvm_dir,
        });
    }
    if attrs.needs_xwin_cache {
        let xwin_root = blessed_xwin_cache_root(paths, &attrs.triple);
        let xwin = xwin_root.join("xwin");
        entries.push(RestoreEntry {
            label: "xwin MSVC CRT + Windows SDK".to_string(),
            present: xwin_root.join(".complete").is_file()
                && xwin.join("crt").join("include").is_dir()
                && xwin.join("sdk").join("include").is_dir(),
            path: xwin_root,
        });
    }
    if attrs.needs_mingw_w64_gcc
        && crate::fetch::mingw_w64_gcc::current_host_supports_mingw_w64_gcc()
    {
        let mingw = paths
            .bin
            .join("syslib")
            .join(crate::fetch::mingw_w64_gcc::MINGW_W64_GCC_TOOL)
            .join(crate::fetch::mingw_w64_gcc::MANAGED_MINGW_W64_GCC_VERSION)
            .join(crate::fetch::mingw_w64_gcc::MINGW_W64_GCC_SLUG);
        let package = mingw.join("package");
        entries.push(RestoreEntry {
            label: format!(
                "MinGW-w64 GCC {}",
                crate::fetch::mingw_w64_gcc::MANAGED_MINGW_W64_GCC_VERSION
            ),
            // soldr#2336 gap #3: verify tools + sysroot inputs, not just gcc.
            present: crate::fetch::mingw_w64_gcc::managed_restore_present(&mingw, &package),
            path: package,
        });
    }
    if attrs.needs_mingw_w64_gcc
        && crate::fetch::mingw_w64_gcc::current_host_supports_mingw_w64_cross()
    {
        let cross = paths
            .bin
            .join("syslib")
            .join(crate::fetch::mingw_w64_gcc::MINGW_W64_CROSS_TOOL)
            .join(crate::fetch::mingw_w64_gcc::MINGW_W64_CROSS_VERSION)
            .join(crate::fetch::mingw_w64_gcc::MINGW_W64_CROSS_HOST_SLUG);
        let package = cross.join("package");
        entries.push(RestoreEntry {
            label: format!(
                "MinGW-w64 cross gcc {}",
                crate::fetch::mingw_w64_gcc::MINGW_W64_CROSS_VERSION
            ),
            present: cross.join(".complete").is_file()
                && crate::fetch::mingw_w64_gcc::cross_tool_path(&package, "gcc").is_file()
                && crate::fetch::mingw_w64_gcc::cross_tool_path(&package, "dlltool").is_file(),
            path: package,
        });
    }
    // reld selection is independent of `attrs` (it's a host tool, not a
    // per-target one) but the resolution is synchronous and file-only — no
    // network call — so it's cheap to resolve once here, per the same
    // per-target/sync contract every other entry in this function follows.
    if reld_selected_for_restore_report(paths)? {
        let reld_dir = paths
            .bin
            .join(format!("reld-{}", crate::fetch::MANAGED_RELD_VERSION));
        let reld_bin = reld_dir.join(format!("reld{}", std::env::consts::EXE_SUFFIX));
        entries.push(RestoreEntry {
            label: format!("reld {}", crate::fetch::MANAGED_RELD_VERSION),
            present: reld_bin.is_file(),
            path: reld_dir,
        });
    }
    if attrs.needs_apple_sdk {
        let selection = crate::fetch::apple_sdk::resolve_apple_sdk_selection(Some(&attrs.triple));
        let sdk = crate::fetch::apple_sdk::install_dir_for_selection(paths, &selection);
        let sdk_dir = crate::fetch::apple_sdk::sdk_dir_for_selection(paths, &selection);
        entries.push(RestoreEntry {
            label: format!(
                "Apple SDK {}/{}",
                selection.version,
                selection.shape.catalogue_slug()
            ),
            present: sdk.join(".complete").is_file() && sdk_dir.is_dir(),
            path: sdk,
        });
    }
    Ok(entries)
}

/// Whether the active project selects `reld`, resolved without any network
/// call — same inputs `ensure_selected_reld` uses, minus the fetch.
fn reld_selected_for_restore_report(paths: &SoldrPaths) -> Result<bool, SoldrError> {
    let host_triple = crate::core::TargetTriple::host()?.triple();
    let selection =
        crate::linker::resolve_project_choice_from_cwd(Some(&host_triple), paths, false)?;
    Ok(selection.needs_reld())
}

pub(crate) fn blessed_xwin_cache_root(paths: &SoldrPaths, target: &str) -> PathBuf {
    paths
        .root
        .join("sdk")
        .join(target)
        .join("xwin")
        .join(crate::fetch::xwin_cache::MANAGED_XWIN_CACHE_VERSION)
}

fn emit_restore_report(entries: &[RestoreEntry]) {
    if entries.is_empty() {
        eprintln!("soldr prepare: restore audit: target has no expected paths to check");
        return;
    }
    let present = entries.iter().filter(|e| e.present).count();
    let total = entries.len();
    eprintln!("soldr prepare: restore audit: {present}/{total} expected entries present");
    for entry in entries {
        let mark = if entry.present { "✓" } else { "✗" };
        eprintln!(
            "  {mark} {label}  ({path})",
            mark = mark,
            label = entry.label,
            path = entry.path.display()
        );
    }
    if present < total {
        eprintln!(
            "soldr prepare: restore audit: {} missing entr{} will be downloaded by dispatch",
            total - present,
            if total - present == 1 { "y" } else { "ies" }
        );
    }
}

/// Worker count for the zstd encoder. `std::thread::available_parallelism`
/// is the most portable read of host parallelism; saturate to a sane
/// upper bound so we don't spawn a hundred zstd workers on big runners.
fn num_cpus_for_zstd() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8) as u32)
        .unwrap_or(4)
}

/// Glob-style listing of soldr-managed dirs that `prepare` populates.
/// Captured by `--save`, restored by `--restore`. Paths are relative
/// to the user's HOME dir so the archive is portable across runners
/// that share the same host triple.
///
/// We pack ENTIRE versioned subdirs (e.g. `~/.soldr/bin/zig-0.13.0/`)
/// rather than the parent `~/.soldr/bin/` so the archive doesn't
/// accidentally pull in zccache binaries or anything unrelated.
pub(crate) fn prepare_state_roots(paths: &SoldrPaths) -> Result<Vec<PathBuf>, SoldrError> {
    let mut roots = Vec::new();
    // ~/.soldr/bin/{zig-<ver>,llvm-<ver>,apple-sdk/<ver>,syslib/{mingw-w64-gcc,mingw-w64-cross,gnu-linux-toolchain,musl-linux-toolchain}}
    if let Ok(entries) = std::fs::read_dir(&paths.bin) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let n = name.to_string_lossy();
            if n.starts_with("zig-")
                || n.starts_with("llvm-")
                || n.starts_with("reld-")
                || n == "apple-sdk"
            {
                roots.push(entry.path());
            }
        }
    }
    let mingw_root = paths
        .bin
        .join("syslib")
        .join(crate::fetch::mingw_w64_gcc::MINGW_W64_GCC_TOOL);
    if mingw_root.is_dir() {
        roots.push(mingw_root);
    }
    // Linux-hosted mingw cross toolchain (soldr#2336): `prepare` on a Linux
    // host fetches `mingw-w64-cross`, not the Windows-host `mingw-w64-gcc`
    // bundle, so it must be captured here too or the save emits an empty
    // archive (soldr#3257).
    let mingw_cross_root = paths
        .bin
        .join("syslib")
        .join(crate::fetch::mingw_w64_gcc::MINGW_W64_CROSS_TOOL);
    if mingw_cross_root.is_dir() {
        roots.push(mingw_cross_root);
    }
    let gnu_linux_root = paths.bin.join("syslib").join("gnu-linux-toolchain");
    if gnu_linux_root.is_dir() {
        roots.push(gnu_linux_root);
    }
    let musl_linux_root = paths.bin.join("syslib").join("musl-linux-toolchain");
    if musl_linux_root.is_dir() {
        roots.push(musl_linux_root);
    }
    // Blessed target SDK caches (`~/.soldr/sdk/<triple>/xwin/<version>/...`).
    let sdk_root = paths.root.join("sdk");
    if sdk_root.is_dir() {
        roots.push(sdk_root);
    }
    Ok(roots)
}

/// Pack the prepare-managed dirs into a tar.zst at `archive`. Paths
/// inside the tar are relative to the selected Soldr root so restore
/// can replay them under a different `SOLDR_CACHE_DIR`.
pub(crate) fn save_prepare_state(
    archive: &Path,
    paths: &SoldrPaths,
    target: &str,
) -> Result<(), SoldrError> {
    let roots = prepare_state_roots(paths)?;
    // A GNU prepare archive must be self-contained without also inheriting a
    // previous target's Zig/Apple/MSVC state from the shared Soldr root.
    let syslib_root = paths.bin.join("syslib");
    let roots: Vec<_> = if target.ends_with("-unknown-linux-gnu") {
        // The compiler bundle refers to companion syslib packages such as
        // zlib-ng and CMake through its generated env. Archive the syslib
        // parent as one portable unit, while still excluding sibling Zig.
        [syslib_root, paths.root.join("sdk")]
            .into_iter()
            .filter(|root| root.exists())
            .collect()
    } else {
        roots
    };
    if roots.is_empty() {
        eprintln!("soldr prepare: nothing to save (no zig/llvm/reld/apple-sdk/mingw/gnu-linux/musl-linux/xwin dirs found)");
        return Ok(());
    }

    if let Some(parent) = archive.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(archive)?;
    // Level 12 + multi-thread: balances compression ratio and wall-clock.
    // -19 (the manifest-branch durable archive setting) is ~10x slower
    // single-threaded; -3 is fastest but bloats the archive. Multi-thread
    // (`-T0` equivalent) lets level 12 finish in a fraction of -19's
    // wall time while keeping the archive small enough to fit GHA's
    // 10 GiB per-repo cache budget comfortably.
    let mut encoder = zstd::stream::write::Encoder::new(file, 12)
        .map_err(|e| SoldrError::Archive(format!("zstd encoder init: {e}")))?;
    let _ = encoder.multithread(num_cpus_for_zstd());
    let encoder = encoder.auto_finish();
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    for root in &roots {
        let rel = match root.strip_prefix(&paths.root) {
            Ok(r) => r,
            Err(_) => {
                eprintln!(
                    "soldr prepare: warning: {} is outside Soldr root ({}); skipping",
                    root.display(),
                    paths.root.display()
                );
                continue;
            }
        };
        eprintln!("soldr prepare: saving {}", rel.display());
        // tar append can fail on Windows NTFS reparse points (junctions)
        // that cargo-xwin creates locally; on Linux CI runners these
        // are POSIX symlinks and tar handles them natively. Log + skip
        // failures rather than aborting the whole save — partial
        // archives still help the next restore.
        if let Err(e) = builder.append_dir_all(rel, root) {
            eprintln!(
                "soldr prepare: warning: tar append {} failed: {e}; skipping",
                rel.display()
            );
        }
    }
    builder
        .finish()
        .map_err(|e| SoldrError::Archive(format!("tar finish: {e}")))?;
    Ok(())
}

/// Extract a previously-saved tar.zst back onto disk. New entries are
/// resolved relative to the selected Soldr root; legacy HOME-relative
/// entries remain compatible. Existing files are overwritten
/// — the caller (`--restore`) treats partial / outdated archives as
/// best-effort: anything still missing after restore is re-downloaded
/// by the normal dispatch.
pub(crate) fn restore_prepare_state(archive: &Path, paths: &SoldrPaths) -> Result<(), SoldrError> {
    // Archives written before #2236 were relative to HOME and began at
    // `.soldr/`; retain that layout on restore. New archives are relative to
    // the selected Soldr root so `SOLDR_CACHE_DIR` can relocate them.
    let legacy_home_relative = {
        let file = std::fs::File::open(archive)
            .map_err(|e| SoldrError::Other(format!("open {}: {e}", archive.display())))?;
        let zst = zstd::stream::read::Decoder::new(file)
            .map_err(|e| SoldrError::Archive(format!("zstd decoder: {e}")))?;
        let mut probe = tar::Archive::new(zst);
        let mut legacy = false;
        for entry in probe
            .entries()
            .map_err(|e| SoldrError::Archive(format!("tar entries: {e}")))?
        {
            let entry = entry.map_err(|e| SoldrError::Archive(format!("tar entry: {e}")))?;
            let entry_path = entry
                .path()
                .map_err(|e| SoldrError::Archive(format!("tar entry path: {e}")))?;
            if entry_path == Path::new(".soldr") || entry_path.starts_with(".soldr/") {
                legacy = true;
                break;
            }
        }
        legacy
    };
    let destination = if legacy_home_relative {
        crate::core::home_dir()?
    } else {
        paths.root.clone()
    };
    let file = std::fs::File::open(archive)
        .map_err(|e| SoldrError::Other(format!("open {}: {e}", archive.display())))?;
    let zst = zstd::stream::read::Decoder::new(file)
        .map_err(|e| SoldrError::Archive(format!("zstd decoder: {e}")))?;
    let mut tarball = tar::Archive::new(zst);
    std::fs::create_dir_all(&destination)?;
    tarball
        .unpack(&destination)
        .map_err(|e| SoldrError::Archive(format!("tar.zst unpack: {e}")))?;
    Ok(())
}

/// Run `rustup target add <triple>` for the active toolchain.
/// Idempotent — already-installed targets are a no-op.
pub(crate) fn rustup_add_target(triple: &str) -> Result<(), SoldrError> {
    // soldr#2612: the host's own std ships with the toolchain, so adding
    // the host triple as a target is a no-op at best — and on a musl host
    // (Alpine) it is a hard failure: rustup errors with "Missing manifest
    // in toolchain '<channel>-x86_64-unknown-linux-musl'" for the
    // musl-hosted distribution. Found by the soldr#2297 hermetic Alpine
    // proof, where host-native `soldr build --target linux-x64-musl`
    // could not get past this call.
    if triple == crate::pyo3_detect::host_triple() {
        return Ok(());
    }
    let paths = SoldrPaths::new()?;
    let rustup = crate::binaries::rustup_binary();
    let mut command = std::process::Command::new(rustup);
    command.args(["target", "add", triple]);
    let channel = pinned_toolchain_channel()?;
    if let Some(channel) = channel.as_deref() {
        command.args(["--toolchain", channel]);
    }
    command.env(
        crate::core::CARGO_HOME_ENV_VAR,
        std::env::var_os(crate::core::CARGO_HOME_ENV_VAR)
            .unwrap_or_else(|| crate::fetch::managed_cargo_home(&paths).into_os_string()),
    );
    let caller_home =
        std::env::var_os(crate::core::RUSTUP_HOME_ENV_VAR).filter(|home| !home.is_empty());
    let caller_selected_home = caller_home.is_some();
    let rustup_home = caller_home
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::fetch::managed_rustup_home(&paths));
    command.env(crate::core::RUSTUP_HOME_ENV_VAR, &rustup_home);
    crate::core::suppress_windows_console_window(&mut command);
    let status = run_rustup_target_add(&mut command, triple)?;
    if !status.success() {
        return Err(SoldrError::Other(format!(
            "rustup target add {triple} exited with {}",
            status
        )));
    }
    // soldr#3793: "up to date" is rustup's claim; check the files.
    let channel = channel.or_else(|| {
        std::env::var(crate::toolchain::RUSTUP_TOOLCHAIN_ENV_VAR)
            .ok()
            .filter(|value| !value.is_empty())
    });
    if let Some(channel) = channel {
        crate::toolchain_std_libs::verify_added_target_std(
            &rustup_home,
            &channel,
            triple,
            caller_selected_home,
            &mut |triple| crate::toolchain_std_libs::reinstall_rust_std(&channel, triple),
        )?;
    }
    Ok(())
}

fn run_rustup_target_add(
    command: &mut std::process::Command,
    triple: &str,
) -> Result<std::process::ExitStatus, SoldrError> {
    crate::core::forbid_toolchain_install_tripwire(&format!("rustup target add {triple}"))?;
    crate::exit_guard::run_child_command(
        command,
        &format!("rustup target add {triple}"),
        "target-install",
        InstallerWatchdogConfig::from_env(RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR),
    )
}

fn pinned_toolchain_channel() -> Result<Option<String>, SoldrError> {
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    // soldr#3633: nearest pin in cwd or ancestors, like cargo/rustup.
    Ok(crate::core::read_rust_toolchain_manifest_from_ancestors(&workspace_root)?.channel)
}

#[cfg(test)]
#[path = "prepare_cmd_tests.rs"]
mod tests;
