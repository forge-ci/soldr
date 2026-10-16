//! `soldr cargo run` trampoline (issue #342).
//!
//! Intercepts `soldr cargo run` before the normal cargo front door spawns
//! cargo. If a sidecar at
//! `<target-dir>/<target?>/<profile>/.soldr-trampoline/<bin>.toml` proves
//! that the binary + every recorded source file is content-identical to
//! the recorded build, the trampoline `exec`s the binary directly. Any
//! uncertainty falls through to real cargo. After a successful cargo run
//! on the fall-through path, the sidecar is refreshed by walking the
//! `.d` dep-info file cargo wrote.
//!
//! **Oracle: content hash, with mtime+size as a fast skip-hint.**
//!
//! The earlier slice (#344) used mtime+size as the authoritative
//! freshness signal — same model cargo uses. Several real-world
//! scenarios produce mtime orderings that lie about whether the binary
//! is up-to-date: tarballs that normalize all mtimes to a fixed epoch
//! (Docker, reproducible builds, distro packagers); clock skew across
//! machines; filesystem granularity (NTFS 2-second, FAT, network
//! filesystems); build systems that `touch` outputs post-build;
//! restored older sources with mtime preserved; cache restore that
//! rewrites binary mtime but leaves source mtimes fresh. All of these
//! produce **false hits** if mtime is authoritative.
//!
//! New algorithm (issue #342):
//!
//! 1. **Binary fast-skip**: if on-disk `mtime` AND `size` both match
//!    the sidecar, trust the cached `binary_hash` without re-reading.
//! 2. **Binary slow-check**: if either diverges, compute the binary's
//!    content hash; compare to `sidecar.binary_hash`. Match → record
//!    the new mtime+size into the sidecar so the next invocation hits
//!    the fast-skip path. Mismatch → fall through.
//! 3. **Source fast-skip**: same pattern per recorded source file.
//! 4. **Source slow-check**: same pattern — content hash is the
//!    authority, mtime is only a skip-hint.
//!
//! Mtime spoofing (`touch -d 'old date'`), tar-with-`--mtime=epoch`
//! restores, and same-second edits all stay correct because content
//! hash is never load-bearing-on-mtime.

use crate::core::SoldrError;
use crate::resolve_toolchain_binary;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

/// `--no-trampoline` opt-out flag (soldr-private). Stripped before
/// forwarding the arg list to cargo.
pub(crate) const NO_TRAMPOLINE_FLAG: &str = "--no-trampoline";

/// Env-var opt-out (`SOLDR_NO_TRAMPOLINE=1`).
pub(crate) const NO_TRAMPOLINE_ENV_VAR: &str = "SOLDR_NO_TRAMPOLINE";

/// Verbose tracing of fall-through reasons (`SOLDR_TRAMPOLINE_LOG=1`).
pub(crate) const TRAMPOLINE_LOG_ENV_VAR: &str = "SOLDR_TRAMPOLINE_LOG";

/// Outcome of `try_run_trampoline`.
pub(crate) enum TrampolineDecision {
    /// The trampoline executed the binary; soldr should exit with this code.
    Executed(i32),
    /// Fall through to real cargo. After cargo succeeds, the caller should
    /// invoke `refresh_sidecar_after_cargo` so the next invocation can hit
    /// the fast path.
    FellThrough(Box<FellThroughPlan>),
}

/// Information the caller needs to refresh the sidecar after a successful
/// cargo run. None of the fields are required for the fall-through itself —
/// they only feed the post-build sidecar refresh.
pub(crate) struct FellThroughPlan {
    pub(crate) parsed: Option<ParsedRunArgs>,
    /// The cleaned arg vector to forward to cargo (with `--no-trampoline`
    /// stripped).
    pub(crate) cleaned_args: Vec<String>,
    /// Bin path inferred from manifest+args; we'll stat this and its `.d`
    /// after cargo succeeds.
    pub(crate) binary_path: Option<PathBuf>,
    pub(crate) sidecar_path: Option<PathBuf>,
    pub(crate) dep_info_path: Option<PathBuf>,
}

/// Try the trampoline fast path. If conditions aren't right, returns
/// `FellThrough` with everything the caller needs to spawn cargo and then
/// refresh the sidecar.
pub(crate) fn try_run_trampoline(args: &[String]) -> Result<TrampolineDecision, SoldrError> {
    // First, strip `--no-trampoline` from the arg list regardless of what
    // happens; cargo doesn't understand it.
    let (cleaned_args, saw_no_trampoline_flag) = strip_no_trampoline_flag(args);

    // Build the fall-through plan up front. Even when opt-outs are set we
    // still want to refresh the sidecar after a successful build so the
    // *next* invocation (without the opt-out) hits the fast path.
    let parsed = parse_run_args(&cleaned_args);
    let plan = fell_through_plan(parsed.clone(), cleaned_args.clone());

    match try_fast_path(&cleaned_args, parsed, saw_no_trampoline_flag) {
        FastPathOutcome::Hit(binary, bin_args) => {
            log_hit(&binary);
            let code = exec_binary(&binary, &bin_args)?;
            Ok(TrampolineDecision::Executed(code))
        }
        FastPathOutcome::FallThrough(reason) => {
            log_fall_through(&reason);
            Ok(TrampolineDecision::FellThrough(Box::new(plan)))
        }
        FastPathOutcome::StaleSource(path) => {
            eprintln!(
                "soldr warning: {} changed contents after its mtime moved behind the built binary; Cargo may run a stale artifact. Touch the source or run `soldr cargo clean -p <package>` before retrying.",
                path.display()
            );
            Ok(TrampolineDecision::FellThrough(Box::new(plan)))
        }
    }
}

enum FastPathOutcome {
    Hit(PathBuf, Vec<String>),
    FallThrough(String),
    /// A recorded source changed even though its timestamp is older than the
    /// built binary. Cargo's mtime-only freshness check can accept that binary,
    /// so this must be a visible warning rather than an ordinary fall-through.
    StaleSource(PathBuf),
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn try_fast_path(
    cleaned_args: &[String],
    parsed: Option<ParsedRunArgs>,
    saw_no_trampoline_flag: bool,
) -> FastPathOutcome {
    if saw_no_trampoline_flag {
        return FastPathOutcome::FallThrough("opt-out: --no-trampoline flag".into());
    }
    if trampoline_env_disabled() {
        return FastPathOutcome::FallThrough(format!("opt-out: {NO_TRAMPOLINE_ENV_VAR} is set"));
    }
    let Some(parsed) = parsed else {
        return FastPathOutcome::FallThrough("cannot model these cargo args".into());
    };
    let bin = match resolve_bin_name(&parsed) {
        Ok(Some(name)) => name,
        Ok(None) => {
            return FastPathOutcome::FallThrough("could not determine unambiguous --bin".into())
        }
        Err(err) => return FastPathOutcome::FallThrough(format!("manifest read failed: {err}")),
    };
    let layout = compute_layout(&parsed, &bin);
    let binary = layout.binary_path;
    let sidecar = layout.sidecar_path;
    let sidecar_text = match fs::read_to_string(&sidecar) {
        Ok(text) => text,
        Err(err) => {
            return FastPathOutcome::FallThrough(format!(
                "sidecar missing: {} ({err})",
                sidecar.display()
            ))
        }
    };
    let sidecar_data: Sidecar = match toml::from_str(&sidecar_text) {
        Ok(data) => data,
        Err(err) => {
            return FastPathOutcome::FallThrough(format!(
                "sidecar parse failed: {} ({err})",
                sidecar.display()
            ))
        }
    };
    let bin_meta = match fs::metadata(&binary) {
        Ok(meta) => meta,
        Err(err) => {
            return FastPathOutcome::FallThrough(format!(
                "binary missing: {} ({err})",
                binary.display()
            ))
        }
    };
    let bin_mtime = mtime_nanos(&bin_meta);
    let bin_size = size_as_i64(&bin_meta);

    // Args fingerprint comes first so a feature/profile flip on a
    // restored binary falls through before we pay any content-hash
    // I/O.
    let fingerprint = match compute_fingerprint(&parsed) {
        Ok(fp) => fp,
        Err(err) => {
            return FastPathOutcome::FallThrough(format!("fingerprint compute failed: {err}"))
        }
    };
    if fingerprint != sidecar_data.cargo_args_fingerprint {
        return FastPathOutcome::FallThrough(format!(
            "fingerprint mismatch (sidecar={}, current={})",
            sidecar_data.cargo_args_fingerprint, fingerprint
        ));
    }

    // Binary oracle: content hash, with mtime+size as fast skip-hint.
    // An empty `binary_hash` (sidecar predates issue #342) forces the
    // slow check so the next build upgrades the schema.
    let mut refreshed_binary_mtime_size: Option<(i64, i64)> = None;
    let bin_fast_skip = !sidecar_data.binary_hash.is_empty()
        && bin_mtime == Some(sidecar_data.binary_mtime_nanos)
        && bin_size == Some(sidecar_data.binary_size_bytes);
    if !bin_fast_skip {
        let actual_hash = match compute_file_hash(&binary) {
            Ok(h) => h,
            Err(err) => {
                return FastPathOutcome::FallThrough(format!(
                    "binary hash failed: {} ({err})",
                    binary.display()
                ));
            }
        };
        if sidecar_data.binary_hash.is_empty() || actual_hash != sidecar_data.binary_hash {
            return FastPathOutcome::FallThrough(format!(
                "binary {} content hash mismatch (sidecar={}, on-disk={})",
                binary.display(),
                sidecar_data.binary_hash,
                actual_hash,
            ));
        }
        if let (Some(m), Some(s)) = (bin_mtime, bin_size) {
            refreshed_binary_mtime_size = Some((m, s));
        }
    }

    // Source oracle: same fast-skip-then-slow-check pattern. Self-heal
    // entries with content match but diverged mtime/size so the next
    // invocation hits the fast path. Empty hash (legacy sidecar) is
    // treated like a divergence — forces hash, then upgrade-on-success.
    let mut refreshed_sources: Vec<RefreshedSource> = Vec::new();
    for (idx, entry) in sidecar_data.source_files.iter().enumerate() {
        let meta = match fs::metadata(&entry.path) {
            Ok(m) => m,
            Err(err) => {
                return FastPathOutcome::FallThrough(format!(
                    "source missing: {} ({err})",
                    entry.path
                ));
            }
        };
        let mtime = mtime_nanos(&meta);
        let size = size_as_i64(&meta);
        let fast_skip = !entry.content_hash.is_empty()
            && mtime == Some(entry.mtime_nanos)
            && size == Some(entry.size_bytes);
        if fast_skip {
            continue;
        }
        let path = Path::new(&entry.path);
        let actual_hash = match compute_file_hash(path) {
            Ok(h) => h,
            Err(err) => {
                return FastPathOutcome::FallThrough(format!(
                    "source hash failed: {} ({err})",
                    entry.path
                ));
            }
        };
        if entry.content_hash.is_empty() || actual_hash != entry.content_hash {
            if !entry.content_hash.is_empty()
                && actual_hash != entry.content_hash
                && mtime
                    .zip(bin_mtime)
                    .is_some_and(|(source, binary)| source < binary)
            {
                return FastPathOutcome::StaleSource(path.to_path_buf());
            }
            return FastPathOutcome::FallThrough(format!(
                "source {} content mismatch (sidecar={}, on-disk={})",
                entry.path, entry.content_hash, actual_hash,
            ));
        }
        if let (Some(m), Some(s)) = (mtime, size) {
            refreshed_sources.push(RefreshedSource {
                idx,
                mtime_nanos: m,
                size_bytes: s,
            });
        }
    }

    // Self-heal: rewrite the sidecar in place when the content oracle
    // accepted entries whose mtime/size drifted (tar restore, clock
    // skew, touch). Best-effort — a failure here is a perf regression
    // for the *next* invocation, never a correctness issue.
    if refreshed_binary_mtime_size.is_some() || !refreshed_sources.is_empty() {
        let _ = self_heal_sidecar(
            &sidecar,
            &sidecar_data,
            refreshed_binary_mtime_size,
            &refreshed_sources,
        );
    }

    FastPathOutcome::Hit(binary, trailing_user_args(cleaned_args))
}

/// One source-entry whose mtime+size needs to be refreshed in the
/// sidecar after a successful content-hash match. Indexed into the
/// existing `Sidecar.source_files` so we don't rewalk the file list.
struct RefreshedSource {
    idx: usize,
    mtime_nanos: i64,
    size_bytes: i64,
}

/// Write `sidecar_path` with the refreshed mtime/size values applied
/// to the in-memory sidecar copy. Content hashes are unchanged — we
/// already verified content equality. Best-effort; any I/O error is
/// logged and ignored so the trampoline still hits the fast path
/// (the next invocation will simply re-hash before exec).
fn self_heal_sidecar(
    sidecar_path: &Path,
    data: &Sidecar,
    bin_refresh: Option<(i64, i64)>,
    src_refresh: &[RefreshedSource],
) -> std::io::Result<()> {
    let mut updated = data.clone();
    if let Some((m, s)) = bin_refresh {
        updated.binary_mtime_nanos = m;
        updated.binary_size_bytes = s;
    }
    for refresh in src_refresh {
        if let Some(entry) = updated.source_files.get_mut(refresh.idx) {
            entry.mtime_nanos = refresh.mtime_nanos;
            entry.size_bytes = refresh.size_bytes;
        }
    }
    write_sidecar_atomic(sidecar_path, &updated)
}

/// After a successful `cargo run`, walk the dep-info file cargo wrote and
/// refresh the sidecar so the next invocation can hit the fast path.
pub(crate) fn refresh_sidecar_after_cargo(plan: &FellThroughPlan) {
    match build_and_write_sidecar(plan) {
        Ok(path) => log_event(&format!("wrote sidecar {}", path.display())),
        Err(reason) => log_fall_through(&format!("post-build: {reason}")),
    }
}

fn build_and_write_sidecar(plan: &FellThroughPlan) -> Result<PathBuf, String> {
    let parsed = plan.parsed.as_ref().ok_or("no parsed args")?;
    let binary = plan.binary_path.as_ref().ok_or("no binary path")?;
    let sidecar = plan.sidecar_path.as_ref().ok_or("no sidecar path")?;
    let dep_info = plan.dep_info_path.as_ref().ok_or("no dep-info path")?;

    let bin_meta = fs::metadata(binary)
        .map_err(|err| format!("binary stat failed: {} ({err})", binary.display()))?;
    let bin_mtime = mtime_nanos(&bin_meta).ok_or_else(|| "binary mtime unavailable".to_string())?;
    let bin_size = size_as_i64(&bin_meta).ok_or_else(|| "binary size unavailable".to_string())?;
    let bin_hash = compute_file_hash(binary)
        .map_err(|err| format!("binary hash failed: {} ({err})", binary.display()))?;

    let dep_text = fs::read_to_string(dep_info)
        .map_err(|err| format!("dep-info missing: {} ({err})", dep_info.display()))?;
    let sources = parse_dep_info_for_output(&dep_text, binary)
        .ok_or_else(|| format!("dep-info has no stanza for {}", binary.display()))?;
    let fingerprint =
        compute_fingerprint(parsed).map_err(|err| format!("fingerprint compute failed: {err}"))?;

    let mut entries: Vec<SidecarSource> = Vec::with_capacity(sources.len());
    for src in sources {
        let meta = fs::metadata(&src)
            .map_err(|err| format!("source stat failed: {} ({err})", src.display()))?;
        let mtime = mtime_nanos(&meta)
            .ok_or_else(|| format!("source mtime unavailable for {}", src.display()))?;
        let size = size_as_i64(&meta)
            .ok_or_else(|| format!("source size unavailable for {}", src.display()))?;
        let content_hash = compute_file_hash(&src)
            .map_err(|err| format!("source hash failed: {} ({err})", src.display()))?;
        entries.push(SidecarSource {
            path: src.to_string_lossy().to_string(),
            mtime_nanos: mtime,
            size_bytes: size,
            content_hash,
        });
    }

    let sidecar_data = Sidecar {
        binary_path: binary.to_string_lossy().to_string(),
        binary_mtime_nanos: bin_mtime,
        binary_size_bytes: bin_size,
        binary_hash: bin_hash,
        cargo_args_fingerprint: fingerprint,
        source_files: entries,
    };
    write_sidecar_atomic(sidecar, &sidecar_data)
        .map_err(|err| format!("sidecar write failed: {} ({err})", sidecar.display()))?;
    Ok(sidecar.clone())
}

// ---------------------------------------------------------------------------
// Arg parsing
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedRunArgs {
    pub(crate) toolchain: Option<String>,
    pub(crate) bin: Option<String>,
    pub(crate) release: bool,
    pub(crate) profile: Option<String>,
    pub(crate) manifest_path: Option<PathBuf>,
    pub(crate) target: Option<String>,
    pub(crate) features: Vec<String>,
    pub(crate) all_features: bool,
    pub(crate) no_default_features: bool,
    pub(crate) target_dir: Option<PathBuf>,
    pub(crate) trailing: Vec<String>,
}

/// Strip `--no-trampoline` (soldr-private; not understood by cargo). Returns
/// the cleaned arg vec and whether the flag was present.
pub(crate) fn strip_no_trampoline_flag(args: &[String]) -> (Vec<String>, bool) {
    let mut out = Vec::with_capacity(args.len());
    let mut saw = false;
    let mut past_separator = false;
    for arg in args {
        if past_separator {
            out.push(arg.clone());
            continue;
        }
        if arg == "--" {
            past_separator = true;
            out.push(arg.clone());
            continue;
        }
        if arg == NO_TRAMPOLINE_FLAG {
            saw = true;
            continue;
        }
        out.push(arg.clone());
    }
    (out, saw)
}

/// Parse the slice of args after `cargo` (i.e. including the `run`
/// positional). Returns `None` if we encounter an arg we don't model (e.g.
/// `--example`, custom `--target-dir`, an unknown flag that takes a
/// value).
pub(crate) fn parse_run_args(args: &[String]) -> Option<ParsedRunArgs> {
    parse_cargo_args(args, &["run", "r"])
}

/// Shared parser for the trampoline. Accepts only invocations whose first
/// positional matches one of `accepted_subs`. The verb itself is consumed
/// but not recorded — callers already know which they asked for.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) fn parse_cargo_args(args: &[String], accepted_subs: &[&str]) -> Option<ParsedRunArgs> {
    let mut toolchain: Option<String> = None;
    let mut bin: Option<String> = None;
    let mut release = false;
    let mut profile: Option<String> = None;
    let mut manifest_path: Option<PathBuf> = None;
    let mut target: Option<String> = None;
    let mut features: Vec<String> = Vec::new();
    let mut all_features = false;
    let mut no_default_features = false;
    let mut target_dir: Option<PathBuf> = None;

    let mut iter = args.iter();
    // Look for the verb, possibly after `+toolchain` and global flags.
    let mut saw_verb = false;
    while let Some(arg) = iter.next() {
        if arg == "--" {
            // Not yet at the verb — nothing here for us.
            return None;
        }
        if let Some(rest) = arg.strip_prefix('+') {
            if rest.is_empty() {
                return None;
            }
            toolchain = Some(rest.to_string());
            continue;
        }
        if global_flag_takes_value(arg) {
            // Skip the flag and its value (a few global flags like `--config`,
            // `--manifest-path` etc. can appear *before* the subcommand).
            // We don't model arbitrary global flags — bail.
            if arg == "--manifest-path" {
                manifest_path = iter.next().map(PathBuf::from);
                continue;
            }
            if let Some(value) = arg.strip_prefix("--manifest-path=") {
                manifest_path = Some(PathBuf::from(value));
                continue;
            }
            return None;
        }
        if arg.starts_with('-') {
            // A non-value-taking global flag we don't model.
            return None;
        }
        if accepted_subs.contains(&arg.as_str()) {
            saw_verb = true;
            break;
        }
        // Some other subcommand.
        return None;
    }
    if !saw_verb {
        return None;
    }

    while let Some(arg) = iter.next() {
        if arg == "--" {
            // The rest are the binary's argv. Capture them so we can replay.
            let trailing: Vec<String> = iter.cloned().collect();
            return Some(ParsedRunArgs {
                toolchain,
                bin,
                release,
                profile,
                manifest_path,
                target,
                features,
                all_features,
                no_default_features,
                target_dir,
                trailing,
            });
        }
        match arg.as_str() {
            "--bin" => {
                let value = iter.next()?;
                if bin.is_some() {
                    return None;
                }
                bin = Some(value.clone());
            }
            "--release" => release = true,
            "--profile" => {
                profile = Some(iter.next()?.clone());
            }
            "--manifest-path" => {
                manifest_path = Some(PathBuf::from(iter.next()?));
            }
            "--target" => {
                target = Some(iter.next()?.clone());
            }
            "--features" | "-F" => {
                features.extend(split_features(iter.next()?));
            }
            "--all-features" => all_features = true,
            "--no-default-features" => no_default_features = true,
            "--target-dir" => {
                target_dir = Some(PathBuf::from(iter.next()?));
            }
            "--example" => return None,
            "-q" | "--quiet" | "-v" | "--verbose" | "--locked" | "--frozen" | "--offline" => {}
            other => {
                if let Some(value) = other.strip_prefix("--bin=") {
                    if bin.is_some() {
                        return None;
                    }
                    bin = Some(value.to_string());
                } else if let Some(value) = other.strip_prefix("--profile=") {
                    profile = Some(value.to_string());
                } else if let Some(value) = other.strip_prefix("--manifest-path=") {
                    manifest_path = Some(PathBuf::from(value));
                } else if let Some(value) = other.strip_prefix("--target=") {
                    target = Some(value.to_string());
                } else if let Some(value) = other.strip_prefix("--features=") {
                    features.extend(split_features(value));
                } else if let Some(value) = other.strip_prefix("-F=") {
                    features.extend(split_features(value));
                } else if let Some(value) = other.strip_prefix("--target-dir=") {
                    target_dir = Some(PathBuf::from(value));
                } else if other.starts_with("--example") {
                    return None;
                } else if other.starts_with("-vv") || other.starts_with("--color") {
                    // Skip multi-v / `--color=...` — these don't affect the
                    // trampoline decision.
                } else {
                    return None;
                }
            }
        }
    }

    Some(ParsedRunArgs {
        toolchain,
        bin,
        release,
        profile,
        manifest_path,
        target,
        features,
        all_features,
        no_default_features,
        target_dir,
        trailing: Vec::new(),
    })
}

fn split_features(value: &str) -> Vec<String> {
    value
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn global_flag_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-C" | "-Z"
            | "-j"
            | "--color"
            | "--config"
            | "--jobs"
            | "--manifest-path"
            | "--message-format"
            | "--target-dir"
    ) || arg.starts_with("-C=")
        || arg.starts_with("-Z=")
        || arg.starts_with("-j=")
        || arg.starts_with("--color=")
        || arg.starts_with("--config=")
        || arg.starts_with("--jobs=")
        || arg.starts_with("--manifest-path=")
        || arg.starts_with("--message-format=")
        || arg.starts_with("--target-dir=")
}

fn trailing_user_args(args: &[String]) -> Vec<String> {
    let mut iter = args.iter().skip_while(|a| a.as_str() != "--");
    if iter.next().is_some() {
        iter.cloned().collect()
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Layout / binary / sidecar path resolution (extracted to trampoline_layout.rs)
// ---------------------------------------------------------------------------

#[path = "trampoline_layout.rs"]
mod layout;
pub(crate) use layout::{
    compute_layout, effective_target_triple, find_nearest_manifest, resolve_bin_name,
    resolve_target_dir, Layout,
};

// ---------------------------------------------------------------------------
// Sidecar (de)serialization
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Sidecar {
    pub(crate) binary_path: String,
    pub(crate) binary_mtime_nanos: i64,
    pub(crate) binary_size_bytes: i64,
    /// Content hash of the binary at sidecar-write time. Empty string
    /// (`#[serde(default)]`) when the sidecar was written before
    /// issue #342 added the field; the verifier treats an empty hash
    /// as "fall through and rewrite this sidecar" so the next build
    /// upgrades the entry.
    #[serde(default)]
    pub(crate) binary_hash: String,
    pub(crate) cargo_args_fingerprint: String,
    #[serde(default, rename = "source_files")]
    pub(crate) source_files: Vec<SidecarSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SidecarSource {
    pub(crate) path: String,
    pub(crate) mtime_nanos: i64,
    pub(crate) size_bytes: i64,
    /// Content hash of the source file at sidecar-write time. Empty
    /// when the sidecar was written before issue #342; same upgrade
    /// strategy as [`Sidecar::binary_hash`].
    #[serde(default)]
    pub(crate) content_hash: String,
}

pub(crate) fn write_sidecar_atomic(path: &Path, data: &Sidecar) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string(data)
        .map_err(|err| std::io::Error::other(format!("toml serialize: {err}")))?;
    let mut tmp_name: OsString = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all().ok();
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Fingerprint
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct FingerprintInputs<'a> {
    profile: &'a str,
    target: Option<&'a str>,
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
    manifest_path: String,
    rustflags: String,
    rustc_identity: String,
    /// Digest of every `.cargo/config.toml` (+ legacy `.cargo/config`)
    /// cargo would discover walking from the manifest dir up to the
    /// filesystem root, plus `$CARGO_HOME/config.toml`. Added for #346:
    /// the trampoline previously only hashed `RUSTFLAGS` from the env,
    /// so a config-file edit silently fast-pathed the stale binary.
    cargo_config_digest: String,
}

pub(crate) fn compute_fingerprint(parsed: &ParsedRunArgs) -> Result<String, SoldrError> {
    let profile = if parsed.release {
        "release"
    } else if let Some(p) = parsed.profile.as_deref() {
        if p == "dev" {
            "debug"
        } else {
            p
        }
    } else {
        "debug"
    };

    let manifest_path = parsed
        .manifest_path
        .clone()
        .or_else(find_nearest_manifest)
        .ok_or_else(|| SoldrError::Other("Cargo.toml not found for fingerprint".into()))?;
    let canonical = fs::canonicalize(&manifest_path).unwrap_or(manifest_path);
    let manifest_dir = canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let manifest_string = canonical.to_string_lossy().to_string();

    let mut features: Vec<String> = parsed.features.clone();
    features.sort();
    features.dedup();

    let rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
    let rustc_identity = rustc_identity_cached()?;
    let cargo_config_digest = cargo_config_digest(&manifest_dir);

    let effective_target = effective_target_triple(parsed);
    let inputs = FingerprintInputs {
        profile,
        target: effective_target.as_deref(),
        features,
        all_features: parsed.all_features,
        no_default_features: parsed.no_default_features,
        manifest_path: manifest_string,
        rustflags,
        rustc_identity,
        cargo_config_digest,
    };

    let bytes = serde_json::to_vec(&inputs)
        .map_err(|err| SoldrError::Other(format!("fingerprint serialize: {err}")))?;
    let hash = zccache::hash::hash_bytes(&bytes);
    Ok(format!("blake3:{}", hash.to_hex()))
}

static RUSTC_IDENTITY_CACHE: OnceLock<String> = OnceLock::new();

fn rustc_identity_cached() -> Result<String, SoldrError> {
    if let Some(cached) = RUSTC_IDENTITY_CACHE.get() {
        return Ok(cached.clone());
    }
    let rustc = resolve_toolchain_binary("rustc")?;
    let mut stderr = std::io::stderr();
    let text = rustc_identity_with_sinks(
        &rustc,
        crate::core::tool_output::ToolSinks::process(&mut stderr),
    )?;
    let _ = RUSTC_IDENTITY_CACHE.set(text.clone());
    Ok(text)
}

/// `rustc -vV`, with the failure carrying rustc's stderr (soldr#3383).
pub(crate) fn rustc_identity_with_sinks(
    rustc: &std::path::Path,
    sinks: crate::core::tool_output::ToolSinks<'_>,
) -> Result<String, SoldrError> {
    let mut command = std::process::Command::new(rustc);
    command.arg("-vV");
    let output = crate::core::tool_output::run_small_tool_with_sinks(
        &mut command,
        "rustc -vV",
        None,
        sinks,
    )?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Dep-info parsing — see [`dep_info`] sibling module.
// ---------------------------------------------------------------------------

#[path = "trampoline_dep_info.rs"]
mod dep_info;
pub(crate) use dep_info::parse_dep_info_for_output;

// ---------------------------------------------------------------------------
// `.cargo/config.toml` content digest (issue #346) — see [`config`] sibling
// module.
// ---------------------------------------------------------------------------

#[path = "trampoline_config.rs"]
mod config;
pub(crate) use config::{cargo_config_digest, discover_cargo_config_files};

// ---------------------------------------------------------------------------
// Exec / stat / logging helpers
// ---------------------------------------------------------------------------

/// Run `binary` as the current process: Unix execs (replacing this image),
/// Windows spawns and waits. Only the failure path returns here on Unix.
fn exec_binary(binary: &Path, args: &[String]) -> Result<i32, SoldrError> {
    let mut command = std::process::Command::new(binary);
    command.args(args);
    match crate::platform::process::spawn::exec_or_status(&mut command) {
        Ok(status) => Ok(status.code().unwrap_or(1)),
        Err(err) => {
            let detail = match fs::metadata(binary) {
                Ok(meta) => format!(
                    " (regular {}, readonly {}, len {})",
                    meta.is_file(),
                    meta.permissions().readonly(),
                    meta.len()
                ),
                Err(meta_err) => format!(" (metadata unavailable: {meta_err})"),
            };
            Err(SoldrError::Other(format!(
                "failed to exec {}: {err}{detail}",
                binary.display()
            )))
        }
    }
}

pub(crate) fn mtime_nanos(meta: &fs::Metadata) -> Option<i64> {
    let mtime = meta.modified().ok()?;
    let dur = mtime.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    i64::try_from(dur.as_nanos()).ok()
}

pub(crate) fn size_as_i64(meta: &fs::Metadata) -> Option<i64> {
    i64::try_from(meta.len()).ok()
}

/// Tag prefix for hashes stored in the sidecar. Lets us swap the
/// hash algorithm later without misinterpreting old digests.
const HASH_PREFIX: &str = "blake3:";

/// Streaming blake3 hash of `path`. Returns `"blake3:<hex>"` on
/// success. Buffer size matches blake3's preferred 64 KiB chunk so
/// hashing a 10 MB binary takes ~5–10 ms on a warm filesystem.
pub(crate) fn compute_file_hash(path: &Path) -> std::io::Result<String> {
    zccache::hash::hash_file(path).map(|hash| format!("{HASH_PREFIX}{}", hash.to_hex()))
}

/// Pure form of the trampoline env-flag check: delegates to the shared
/// parser (`crate::core::flag_value`) so spellings match every other flag.
pub(crate) fn trampoline_flag_value(value: Option<&str>) -> bool {
    value.is_some_and(crate::core::flag_value)
}

pub(crate) fn trampoline_env_disabled() -> bool {
    trampoline_flag_value(std::env::var(NO_TRAMPOLINE_ENV_VAR).ok().as_deref())
}

pub(crate) fn trampoline_log_enabled() -> bool {
    trampoline_flag_value(std::env::var(TRAMPOLINE_LOG_ENV_VAR).ok().as_deref())
}

pub(crate) fn log_fall_through(reason: &str) {
    if trampoline_log_enabled() {
        eprintln!("soldr trampoline: fall-through: {reason}");
    }
}

pub(crate) fn log_event(message: &str) {
    if trampoline_log_enabled() {
        eprintln!("soldr trampoline: {message}");
    }
}

fn log_hit(binary: &Path) {
    if trampoline_log_enabled() {
        eprintln!("soldr trampoline: hit, exec {}", binary.display());
    }
}

fn fell_through_plan(parsed: Option<ParsedRunArgs>, cleaned_args: Vec<String>) -> FellThroughPlan {
    let (binary_path, sidecar_path, dep_info_path) = match parsed.as_ref() {
        Some(p) => match resolve_bin_name(p) {
            Ok(Some(name)) => {
                let layout = compute_layout(p, &name);
                (
                    Some(layout.binary_path),
                    Some(layout.sidecar_path),
                    Some(layout.dep_info_path),
                )
            }
            _ => (None, None, None),
        },
        None => (None, None, None),
    };
    FellThroughPlan {
        parsed,
        cleaned_args,
        binary_path,
        sidecar_path,
        dep_info_path,
    }
}

#[cfg(test)]
#[path = "trampoline_tests.rs"]
mod tests;
