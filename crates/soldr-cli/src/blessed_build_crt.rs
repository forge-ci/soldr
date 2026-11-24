// Which C runtime the caller asked the MSVC cross-link to use (soldr#2794).
//
// `soldr build --target *-pc-windows-msvc` injects the UCRT/VCRuntime
// **import** libraries so clang-cl's default `/MD` mode resolves its
// `__declspec(dllimport)` references. That is right until the caller also
// passes `-C target-feature=+crt-static`, at which point rustc emits
// `/defaultlib:libcmt` and the link line carries *both* CRTs:
//
// ```text
// lld-link: error: duplicate symbol: __vcrt_InitializeCriticalSectionEx
//   >>> libvcruntime.lib(winapi_downlevel.obj)   (static)
//   >>> vcruntime.lib(VCRUNTIME140.dll)          (dynamic)
// ```
//
// soldr was hard-coding a *policy* (dynamic CRT) into what is otherwise
// toolchain *plumbing* (linker flavor + library search paths). This module
// reads the policy back off the caller instead. Upstream `cargo-xwin` made
// the same change in 0.20.0 (rust-cross/cargo-xwin#166); soldr needs it for
// `vcruntime` as well as `ucrt`, because soldr injects a `vcruntime` default
// that cargo-xwin does not.
//
// ## Everything here is pure
//
// Detection takes its inputs as arguments rather than reading the process
// environment, so the tests never mutate a shared variable. That is not
// incidental tidiness: `tests/guards/env_lock_lint.rs` requires every mutated
// variable to sit under a single barrier, and `RUSTFLAGS` is already mutated
// under two others. A pure core sidesteps the question — only
// [`requested_crt_linkage`] touches the environment, and it does nothing but
// gather strings and delegate.

/// Which CRT the MSVC link line should pull in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrtLinkage {
    /// Import libraries — `ucrt.lib` / `vcruntime.lib`. soldr's default, and
    /// what clang-cl's `/MD` mode needs.
    Dynamic,
    /// Static archives — `libucrt.lib` / `libvcruntime.lib`. Selected when the
    /// caller has already said `+crt-static`.
    Static,
}

/// The `target-feature` spelling that selects each linkage.
const STATIC_FEATURE: &str = "+crt-static";
const DYNAMIC_FEATURE: &str = "-crt-static";

/// Decide the linkage from one flag string.
///
/// Returns `None` when the string says nothing about `crt-static`, which is
/// what lets [`crt_linkage_from_merge`] fall back to the dynamic default and
/// [`declared_config_linkage`] skip to the next config table, rather than
/// either treating silence as a decision.
///
/// Within a single string the **last** mention wins, matching how rustc
/// accumulates `-C target-feature` — `+crt-static ... -crt-static` really does
/// end up dynamic, and reporting it as static would produce exactly the
/// mismatched link line this module exists to prevent.
fn crt_linkage_in(flags: &str) -> Option<CrtLinkage> {
    let last_static = flags.rfind(STATIC_FEATURE);
    let last_dynamic = flags.rfind(DYNAMIC_FEATURE);
    match (last_static, last_dynamic) {
        (None, None) => None,
        (Some(_), None) => Some(CrtLinkage::Static),
        (None, Some(_)) => Some(CrtLinkage::Dynamic),
        (Some(s), Some(d)) => Some(if s > d {
            CrtLinkage::Static
        } else {
            CrtLinkage::Dynamic
        }),
    }
}

/// Decide the linkage from flag sources listed in **soldr's own append order**,
/// lowest precedence first.
///
/// soldr#2830: this used to model Cargo's rule -- four mutually exclusive
/// sources, first one set wins -- which is not the rule that applies here.
/// soldr never lets Cargo do that selection. It merges every source into a
/// single value and exports the encoded form
/// (`target_lifecycle::merge_encoded_rustflags`), appending
/// `CARGO_ENCODED_RUSTFLAGS`, then soldr's own flags, then
/// `CARGO_TARGET_<T>_RUSTFLAGS`, then `RUSTFLAGS`. rustc then resolves the
/// concatenation with its own last-wins rule, so `RUSTFLAGS` is the strongest
/// source and ambient `CARGO_ENCODED_RUSTFLAGS` the weakest -- the exact
/// reverse of what the old order assumed.
///
/// Joining the sources and deferring to [`crt_linkage_in`] reproduces that by
/// construction, which is why this deletes the parallel precedence model rather
/// than correcting it: there is now one place where "last mention wins" lives.
pub(crate) fn crt_linkage_from_merge<I, S>(sources: I) -> CrtLinkage
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let merged = sources
        .into_iter()
        .map(|source| source.as_ref().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    crt_linkage_in(&merged).unwrap_or(CrtLinkage::Dynamic)
}

/// The linkage a `.cargo/config.toml` asks for, under Cargo's *config*
/// specificity: `target.<triple>` before `build`, first mention winning.
///
/// This is only ever used to warn. Config rustflags do not reach rustc on this
/// path at all (see [`warn_if_config_crt_is_inert`]), so this must not feed the
/// decision -- but knowing a preference was expressed is what lets soldr say so
/// instead of ignoring it in silence.
fn declared_config_linkage(sources: &[String]) -> Option<CrtLinkage> {
    sources.iter().find_map(|source| crt_linkage_in(source))
}

/// `rustflags` entries from a `.cargo/config.toml` under `root`, most specific
/// first: `target.<triple>.rustflags` then `build.rustflags`.
///
/// Both string and array spellings are accepted. A missing, unreadable, or
/// malformed config yields nothing at all — this feeds a link-flag choice with
/// a working default, so it must never be the thing that fails a build.
pub(crate) fn cargo_config_rustflags(root: &std::path::Path, target_triple: &str) -> Vec<String> {
    let mut found = Vec::new();
    for relative in [".cargo/config.toml", ".cargo/config"] {
        let Ok(contents) = std::fs::read_to_string(root.join(relative)) else {
            continue;
        };
        let Ok(value) = toml::from_str::<toml::Value>(&contents) else {
            continue;
        };
        let target_flags = value
            .get("target")
            .and_then(|targets| targets.get(target_triple))
            .and_then(|entry| entry.get("rustflags"))
            .and_then(flatten_rustflags);
        let build_flags = value
            .get("build")
            .and_then(|build| build.get("rustflags"))
            .and_then(flatten_rustflags);
        found.extend(target_flags);
        found.extend(build_flags);
        if !found.is_empty() {
            break;
        }
    }
    found
}

/// `"-C target-feature=+crt-static"` or `["-C", "target-feature=+crt-static"]`
/// alike, flattened to one whitespace-joined string.
fn flatten_rustflags(value: &toml::Value) -> Option<String> {
    let text = match value {
        toml::Value::String(value) => value.clone(),
        toml::Value::Array(values) => values
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

/// The linkage the caller has asked for, reading the environment in the
/// order soldr merges it (soldr#2830).
///
/// ## Why this is read here and not at `soldr prepare` time
///
/// `soldr prepare --github-env <FILE>` writes a *static snapshot*. In the
/// zccache action that motivated soldr#2794, `setup-soldr` runs some ninety
/// lines before the step that exports `+crt-static`, so a snapshot-time read
/// would look before the flag exists and silently never fire. This is called
/// while the flags are being constructed for the dispatch, which is the last
/// moment before they are used and the first at which the caller's own
/// `RUSTFLAGS` is guaranteed visible.
///
/// A caller who sets the flag *after* `soldr prepare` still gets the dynamic
/// default. That case is not fixable from here — nothing can read a variable
/// that does not exist yet. A `.cargo/config.toml` is on disk before either
/// step runs, but it cannot stand in for that, because Cargo does not read it
/// on this path at all; see [`warn_if_config_crt_is_inert`].
pub(crate) fn requested_crt_linkage(target_triple: &str) -> CrtLinkage {
    let target_key = format!(
        "CARGO_TARGET_{}_RUSTFLAGS",
        target_triple.to_uppercase().replace('-', "_")
    );
    // soldr's own append order, weakest first. See `crt_linkage_from_merge`.
    let linkage = crt_linkage_from_merge([
        std::env::var("CARGO_ENCODED_RUSTFLAGS")
            .unwrap_or_default()
            .replace('\u{1f}', " "),
        std::env::var(&target_key).unwrap_or_default(),
        std::env::var("RUSTFLAGS").unwrap_or_default(),
    ]);
    warn_if_config_crt_is_inert(target_triple);
    linkage
}

/// Say so when a `.cargo/config.toml` CRT preference cannot take effect.
///
/// soldr#2798 read this file into the decision, on the reasoning that a config
/// is on disk before either step runs. It is -- but Cargo never reads it here.
/// soldr exports `CARGO_ENCODED_RUSTFLAGS`, Cargo's highest-precedence
/// rustflags source, and Cargo's sources are mutually exclusive, so config
/// rustflags are suppressed outright; soldr does not fold them into the merge
/// either. Measured with a probe crate: with the encoded variable set, a
/// `build.rustflags` entry reaches rustc zero times.
///
/// Honouring it anyway is what this fix removes, because it desynchronizes the
/// link line in the direction soldr#2794 exists to prevent -- soldr emits the
/// static archives while rustc still emits `/defaultlib:msvcrt`. But ignoring
/// it in silence is its own failure: the binary comes out dynamically linked
/// after the project asked for static, and nothing says why. So it is ignored
/// loudly, with the remedy named.
fn warn_if_config_crt_is_inert(target_triple: &str) {
    let Ok(current) = std::env::current_dir() else {
        return;
    };
    let root = project_root_for_crt(&current);
    let declared = cargo_config_rustflags(&root, target_triple);
    let Some(linkage) = declared_config_linkage(&declared) else {
        return;
    };
    let asked = match linkage {
        CrtLinkage::Static => "+crt-static",
        CrtLinkage::Dynamic => "-crt-static",
    };
    eprintln!(
        "soldr build: {} asks for {asked}, but Cargo ignores config rustflags here",
        root.join(".cargo/config.toml").display()
    );
    eprintln!(
        "soldr build: soldr exports CARGO_ENCODED_RUSTFLAGS, which outranks them; set RUSTFLAGS=\"-C target-feature={asked}\" instead (soldr#2830)"
    );
}

/// Nearest ancestor holding a `Cargo.toml`, falling back to `start`.
fn project_root_for_crt(start: &std::path::Path) -> std::path::PathBuf {
    for directory in start.ancestors() {
        if directory.join("Cargo.toml").is_file() {
            return directory.to_path_buf();
        }
    }
    start.to_path_buf()
}
