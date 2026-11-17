//! tar + zstd:9 packer for cross-repo `soldr cook` artifacts (issue
//! #577, meta #579).
//!
//! Layout of `~/.soldr/cache/cook/`:
//!
//! ```text
//! <sha256>.tar.zst              # content-addressed cook artifact
//! .tmp/<rand>.tar.zst           # in-progress write (rename-on-finish)
//! ```
//!
//! This module is **purely a packer**. It is unrelated to the
//! `cache_lib::save` archive transport — `save.rs` keeps its
//! `DEFAULT_ZSTD_LEVEL = 3` for the short-lived save/load round-trip,
//! while cook artifacts default to level 9. Level 19 + `--long=27` packed a
//! real obs-studio closure (266 MB) in 26.1 s for 38 MiB; level 9 takes
//! 1.24 s for 53 MiB, and every cook pays the pack while the extra bytes
//! cost a few seconds of upload at most (soldr#4162). `SOLDR_ZSTD_LEVEL`
//! overrides it; the long window applies from level 10 up.
//!
//! The packer never reads or writes `cook_index_v1` itself — PR 3's
//! pre-flight hydrate is what looks up entries via the daemon, and
//! PR 2's `soldr cook` is what calls `CookRecord` after this packer
//! returns the resolved sha256.

use crate::cache_lib::target_registry::RegistryError;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Default zstd level for cross-repo cook artifacts (`SOLDR_ZSTD_LEVEL`
/// overrides it). Was 19 + the long window: 20x slower to pack for 30%
/// smaller artifacts, and decompression speed barely depends on the
/// level. Do NOT reuse for the save/load transport — that path stays at
/// `crate::cache_lib::save::DEFAULT_ZSTD_LEVEL = 3`.
pub const COOK_ZSTD_LEVEL: i32 = 9;

/// `--long=27` window log enables zstd long-distance matching on the
/// 128 MiB window — large enough that a typical trimmed `target/`
/// tree compresses to a small fraction of its raw size.
pub const COOK_ZSTD_LONG_WINDOW: u32 = 27;

/// The long window (`COOK_ZSTD_LONG_WINDOW`) only pays at slow, high
/// levels; below this a fast level stays fast.
pub const COOK_LONG_WINDOW_MIN_LEVEL: i32 = 10;

/// The cook packer's level: `SOLDR_ZSTD_LEVEL` when set, else
/// [`COOK_ZSTD_LEVEL`].
pub fn cook_zstd_level_from(env: Option<i32>) -> i32 {
    crate::cache_lib::zstd_level::resolve_zstd_level(None, env, COOK_ZSTD_LEVEL)
}

/// Whether a cook archive at `level` uses the long window.
pub fn cook_uses_long_window(level: i32) -> bool {
    level >= COOK_LONG_WINDOW_MIN_LEVEL
}

/// Result of [`pack_cook_archive`]: the on-disk path and digest of
/// the new `<sha256>.tar.zst` artifact, together with the byte size
/// of the compressed file.
#[derive(Debug, Clone)]
pub struct PackedCookArchive {
    /// Absolute path to `<cook_cache_dir>/<sha256_hex>.tar.zst`.
    pub path: PathBuf,
    /// SHA-256 digest of the compressed bytes. Filename is the lower-
    /// case hex form of this; verifying the file == filename is the
    /// integrity check PR 3's pre-flight runs before extraction.
    pub sha256: [u8; 32],
    /// On-disk size of the compressed artifact.
    pub size_bytes: u64,
}

/// Resolve the `~/.soldr/cache/cook/` directory under the supplied
/// `paths`. Created lazily by [`pack_cook_archive`] but exposed
/// separately so PR 3 can find existing artifacts without re-packing.
pub fn cook_cache_dir(paths: &crate::core::SoldrPaths) -> PathBuf {
    paths.cache.join("cook")
}

/// Construct the canonical `<sha256_hex>.tar.zst` path for an artifact
/// under `cook_cache_dir`. Useful for tests + for the daemon's
/// `Response::CookHit { path }` which mirrors this shape.
pub fn artifact_path_for_sha(cook_dir: &Path, sha256: &[u8; 32]) -> PathBuf {
    cook_dir.join(format!("{}.tar.zst", hex_lower(sha256)))
}

/// Pack `source_dir` into `<cook_cache_dir>/<sha256>.tar.zst` using
/// tar + zstd at [`COOK_ZSTD_LEVEL`] with the
/// [`COOK_ZSTD_LONG_WINDOW`] window log. Writes go through
/// `<cook_cache_dir>/.tmp/<rand>.tar.zst` first so a crashed packer
/// never leaves a half-written `<sha256>.tar.zst` on disk.
///
/// `source_dir` is typically the trimmed `target/<profile>/` tree
/// (after `cargo chef cook` + `StripTargetOptions::cook()`). The
/// archive entries are recorded with paths *relative to* the
/// parent of `source_dir` — i.e. they start with the profile name
/// (`release/`, `debug/`, ...) so PR 3 can extract straight into
/// `target/` without an extra rename.
///
/// Errors:
///  * I/O failures creating `.tmp/`, writing the temp file, computing
///    the sha256, or renaming into place.
///  * The source_dir not existing — returned as `RegistryError::Io`.
pub fn pack_cook_archive(
    source_dir: &Path,
    cook_dir: &Path,
) -> Result<PackedCookArchive, RegistryError> {
    let level = cook_zstd_level_from(crate::cache_lib::zstd_level::env_zstd_level());
    pack_cook_archive_at_level(source_dir, cook_dir, level)
}

/// [`pack_cook_archive`] at an explicit zstd `level`; the long window is
/// used only when [`cook_uses_long_window`].
pub fn pack_cook_archive_at_level(
    source_dir: &Path,
    cook_dir: &Path,
    level: i32,
) -> Result<PackedCookArchive, RegistryError> {
    if !source_dir.is_dir() {
        return Err(RegistryError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("cook archive source missing: {}", source_dir.display()),
        )));
    }

    let tmp_dir = cook_dir.join(".tmp");
    std::fs::create_dir_all(&tmp_dir)?;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let temp_path = tmp_dir.join(format!("cook-{pid}-{nanos}.tar.zst.tmp"));

    let temp_file = File::create(&temp_path)?;
    let mut encoder = zstd::Encoder::new(temp_file, level).map_err(io_err)?;
    if cook_uses_long_window(level) {
        encoder.long_distance_matching(true).map_err(io_err)?;
        encoder.window_log(COOK_ZSTD_LONG_WINDOW).map_err(io_err)?;
    }
    // Multi-threaded compression — `zstdmt` feature already enabled
    // in workspace deps. Using `num_cpus` would add a dep we do not
    // want; the OS-determined thread count works well at every level.
    if let Ok(n) = std::thread::available_parallelism() {
        let _ = encoder.multithread(n.get() as u32);
    }

    {
        // tar archive: entries recorded relative to source_dir's parent
        // so the leading path component is the profile name.
        let mut tar_builder = tar::Builder::new(&mut encoder);
        let prefix_name = source_dir
            .file_name()
            .map(|n| n.to_owned())
            .unwrap_or_else(|| "cook".into());
        append_tree_with_precise_mtimes(&mut tar_builder, source_dir, Path::new(&prefix_name))?;
        tar_builder.finish().map_err(io_err)?;
    }
    encoder.finish().map_err(io_err)?;

    // Hash the on-disk file. Going through a fresh `Read` cycle is
    // simpler than chaining a `Sha256` writer through the zstd encoder
    // and matches the integrity model PR 3 uses on hydrate (re-hash
    // the file at the named path and compare to the filename stem).
    let (sha256, size_bytes) = hash_file(&temp_path)?;
    let final_path = artifact_path_for_sha(cook_dir, &sha256);

    // Rename into place. If a row already exists, replace it — the
    // content is identical by construction (same sha256), so a
    // re-cook of the same recipe is idempotent.
    std::fs::rename(&temp_path, &final_path).or_else(|_| {
        // Same-sha file already there: drop the temp.
        let _ = std::fs::remove_file(&temp_path);
        Ok::<(), std::io::Error>(())
    })?;

    Ok(PackedCookArchive {
        path: final_path,
        sha256,
        size_bytes,
    })
}

/// Append a tree with a POSIX pax `mtime` record before every entry.
///
/// A traditional tar header stores mtimes as whole seconds. Cargo fingerprints
/// compare dependency outputs at nanosecond precision, so rounding a cooked
/// target tree can reverse files that were produced a few milliseconds apart
/// and turn a hydrate into a broad rebuild. Pax fractional mtimes retain that
/// ordering while keeping the artifact a normal tar stream.
fn append_tree_with_precise_mtimes<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    source_root: &Path,
    archive_root: &Path,
) -> Result<(), RegistryError> {
    let mut pending = vec![(source_root.to_path_buf(), archive_root.to_path_buf())];
    while let Some((source, archived)) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&source)?;
        if let Ok(modified) = metadata.modified() {
            if let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH) {
                let value = format!(
                    "{}.{:09}",
                    since_epoch.as_secs(),
                    since_epoch.subsec_nanos()
                );
                builder
                    .append_pax_extensions([("mtime", value.as_bytes())])
                    .map_err(io_err)?;
            }
        }
        builder
            .append_path_with_name(&source, &archived)
            .map_err(io_err)?;

        if metadata.is_dir() {
            let mut children = std::fs::read_dir(&source)?.collect::<Result<Vec<_>, _>>()?;
            children.sort_by_key(|entry| entry.file_name());
            for child in children.into_iter().rev() {
                pending.push((child.path(), archived.join(child.file_name())));
            }
        }
    }
    Ok(())
}

fn pax_mtime(entry: &mut tar::Entry<'_, impl Read>) -> Option<zccache::core::mtime::FileTime> {
    let extensions = entry.pax_extensions().ok()??;
    for extension in extensions.flatten() {
        if extension.key().ok()? != "mtime" {
            continue;
        }
        let raw = extension.value().ok()?;
        let (seconds, fraction) = raw.split_once('.').unwrap_or((raw, ""));
        let seconds = seconds.parse::<i64>().ok()?;
        let mut nanos = fraction
            .as_bytes()
            .iter()
            .take(9)
            .try_fold(0u32, |acc, byte| {
                byte.is_ascii_digit()
                    .then_some(acc.saturating_mul(10).saturating_add((byte - b'0') as u32))
            })?;
        for _ in fraction.len().min(9)..9 {
            nanos = nanos.saturating_mul(10);
        }
        return Some(zccache::core::mtime::FileTime::from_unix_time(
            seconds, nanos,
        ));
    }
    None
}

fn hash_file(path: &Path) -> Result<([u8; 32], u64), RegistryError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        total = total.saturating_add(read as u64);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok((out, total))
}

fn io_err(e: std::io::Error) -> RegistryError {
    RegistryError::Io(e)
}

fn hex_lower(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}

/// Pretty-print an abbreviated SHA-256 (12 hex chars) for human-
/// readable status / warning output. Matches the spec in #577 / #579
/// (`sha256=<abbrev12>`).
pub fn sha_abbrev(sha256: &[u8; 32]) -> String {
    hex_lower(sha256).chars().take(12).collect()
}

/// Stable "recipe hash" used as the authoritative key tuple component
/// for cross-repo `soldr cook` indexing (issue #578). Computed from
/// the workspace's content-defining files:
/// * `Cargo.lock`
/// * Every `Cargo.toml` under the workspace root (sorted by relative
///   path, `target/` and `.git/` excluded).
/// * `rust-toolchain.toml` `[toolchain] channel` if declared.
///
/// **Important:** PR 2's `soldr cook` records this hash via
/// `CookRecord`, and PR 3's cargo-front-door pre-flight queries by
/// it. Both sides MUST go through this single function so the
/// recorded key matches the lookup key.
///
/// Returns `None` when `Cargo.lock` is missing — pre-flight callers
/// short-circuit silently in that case.
pub fn compute_recipe_hash_proxy(manifest_dir: &Path) -> Option<[u8; 32]> {
    let lockfile = manifest_dir.join("Cargo.lock");
    let lock_bytes = std::fs::read(&lockfile).ok()?;
    let manifest_pairs = collect_manifest_hashes(manifest_dir).ok()?;

    let mut hasher = Sha256::new();
    hasher.update(b"soldr-cook-recipe-hash-v1\n");
    hasher.update(b"Cargo.lock\n");
    hasher.update((lock_bytes.len() as u64).to_le_bytes());
    hasher.update(&lock_bytes);
    for (rel, hex) in &manifest_pairs {
        hasher.update(rel.as_bytes());
        hasher.update(b"\n");
        hasher.update(hex.as_bytes());
        hasher.update(b"\n");
    }
    let channel = std::fs::read_to_string(manifest_dir.join("rust-toolchain.toml"))
        .ok()
        .and_then(|s| parse_toolchain_channel(&s));
    if let Some(channel) = channel {
        hasher.update(b"channel=");
        hasher.update(channel.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Some(out)
}

/// Walk `dir`, returning `(relative_path, sha256_hex)` for every
/// `Cargo.toml` outside of `target/`, `.git/`, `.soldr/`,
/// `node_modules/`. Sorted by relative path for determinism.
/// Repo-local toolchains, registries, agent worktrees, virtual environments,
/// and generated caches are operational state rather than recipe inputs and
/// are excluded as well.
fn collect_manifest_hashes(dir: &Path) -> std::io::Result<Vec<(String, String)>> {
    let mut protected_roots = std::collections::BTreeSet::new();
    loop {
        let mut out = Vec::new();
        walk_for_manifests(dir, dir, &protected_roots, &mut out)?;
        out.sort_by(|a, b| a.0.cmp(&b.0));

        let discovered = discover_referenced_state_roots(dir, &out);
        let prior_len = protected_roots.len();
        protected_roots.extend(discovered);
        if protected_roots.len() == prior_len {
            return Ok(out);
        }
    }
}

fn is_recipe_state_dir(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    matches!(
        name.as_ref(),
        ".cache"
            | ".claude"
            | ".clud"
            | ".codex"
            | ".extern-repos"
            | ".git"
            | ".loop"
            | ".perf-local"
            | ".pytest_cache"
            | ".rustup"
            | ".soldr"
            | ".venv"
            | "node_modules"
            | "target"
    ) || name == concat!(".car", "go")
}

fn discover_referenced_state_roots(
    root: &Path,
    manifests: &[(String, String)],
) -> std::collections::BTreeSet<String> {
    let mut protected = std::collections::BTreeSet::new();
    for (relative, _) in manifests {
        let manifest_path = root.join(relative);
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(value) = raw.parse::<toml::Value>() else {
            continue;
        };
        let base = manifest_path.parent().unwrap_or(root);
        collect_referenced_state_roots(&value, None, root, base, &mut protected);
    }
    protected
}

fn collect_referenced_state_roots(
    value: &toml::Value,
    key: Option<&str>,
    root: &Path,
    base: &Path,
    protected: &mut std::collections::BTreeSet<String>,
) {
    match value {
        toml::Value::String(path) if key == Some("path") => {
            protect_referenced_state_root(root, base, path, protected);
        }
        toml::Value::Array(values) if key == Some("members") => {
            for value in values {
                if let Some(path) = value.as_str() {
                    protect_referenced_state_root(root, base, path, protected);
                }
            }
        }
        toml::Value::Array(values) => {
            for value in values {
                collect_referenced_state_roots(value, None, root, base, protected);
            }
        }
        toml::Value::Table(table) => {
            for (child_key, value) in table {
                collect_referenced_state_roots(value, Some(child_key), root, base, protected);
            }
        }
        _ => {}
    }
}

fn protect_referenced_state_root(
    root: &Path,
    base: &Path,
    path: &str,
    protected: &mut std::collections::BTreeSet<String>,
) {
    let candidate = lexical_normalize(&base.join(path));
    let root = lexical_normalize(root);
    let Ok(relative) = candidate.strip_prefix(root) else {
        return;
    };
    let Some(first) = relative.components().next() else {
        return;
    };
    let first = first.as_os_str();
    if is_recipe_state_dir(first) {
        protected.insert(first.to_string_lossy().into_owned());
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn walk_for_manifests(
    root: &Path,
    dir: &Path,
    protected_roots: &std::collections::BTreeSet<String>,
    out: &mut Vec<(String, String)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let name = entry.file_name();
            let protected_at_root =
                dir == root && protected_roots.contains(name.to_string_lossy().as_ref());
            if is_recipe_state_dir(&name) && !protected_at_root {
                continue;
            }
            walk_for_manifests(root, &path, protected_roots, out)?;
        } else if file_type.is_file() && entry.file_name() == std::ffi::OsStr::new("Cargo.toml") {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&path)?;
            let mut h = Sha256::new();
            h.update(&bytes);
            out.push((rel, hex_lower_short(&h.finalize().into())));
        }
    }
    Ok(())
}

/// Best-effort parse of `[toolchain] channel = "..."` from
/// `rust-toolchain.toml`. Avoids depending on `toml::from_str` so
/// the hash function stays infallible even when downstream parses
/// would fail (the daemon's view of the workspace is bytewise).
fn parse_toolchain_channel(text: &str) -> Option<String> {
    let mut in_toolchain = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_toolchain = trimmed == "[toolchain]";
            continue;
        }
        if !in_toolchain {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("channel") {
            let rest = rest.trim_start();
            let rest = rest.strip_prefix('=')?;
            let rest = rest.trim();
            let trimmed_value = rest.trim_matches(|c: char| c == '"' || c == '\'');
            if !trimmed_value.is_empty() {
                return Some(trimmed_value.to_string());
            }
        }
    }
    None
}

fn hex_lower_short(bytes: &[u8; 32]) -> String {
    hex_lower(bytes)
}

/// Re-hash the file at `path` and compare against `expected`. Used by
/// PR 3's pre-flight hydrate to integrity-check the artifact before
/// extracting (issue #578). On mismatch the caller is expected to move
/// the file to `<sha>.tar.zst.quarantine` and fall through to a normal
/// cargo build.
pub fn verify_sha256(path: &Path, expected: &[u8; 32]) -> Result<bool, RegistryError> {
    let (actual, _size) = hash_file(path)?;
    Ok(&actual == expected)
}

/// Counts returned by [`extract_skip_existing`] so the caller can
/// surface a deterministic summary.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExtractReport {
    /// Files written by this hydrate (path did not already exist).
    pub files_written: u64,
    /// Files left alone because the path already existed in `target/`.
    /// Hydration is additive — the user's build state is never
    /// overwritten.
    pub files_skipped: u64,
    /// Directory entries materialized when their parent ancestors
    /// were missing.
    pub dirs_created: u64,
}

/// Stream-extract `<sha256>.tar.zst` into `dest_root`, **skipping** any
/// file path that already exists. Hydration is additive: PR 3's
/// pre-flight runs this BEFORE cargo invokes rustc, and we must
/// never overwrite a fresher artifact the user already produced.
///
/// `dest_root` is expected to be the project's `target/` directory.
/// The archive entries (written by [`pack_cook_archive`]) start with
/// the profile name (`release/`, `debug/`, ...), so extracting at the
/// `target/` root puts files at the right paths.
pub fn extract_skip_existing(
    archive_path: &Path,
    dest_root: &Path,
) -> Result<ExtractReport, RegistryError> {
    let file = File::open(archive_path)?;
    let decoder = zstd::Decoder::new(file).map_err(io_err)?;
    let mut archive = tar::Archive::new(decoder);
    let mut report = ExtractReport::default();

    for entry in archive.entries().map_err(io_err)? {
        let mut entry = entry.map_err(io_err)?;
        let path_in_tar = entry.path().map_err(io_err)?.to_path_buf();
        if is_unsafe_tar_path(&path_in_tar) {
            // Defensive: refuse to extract above the destination root
            // even though our own packer never produces these.
            continue;
        }
        let dest = dest_root.join(&path_in_tar);
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            if !dest.is_dir() {
                std::fs::create_dir_all(&dest)?;
                report.dirs_created = report.dirs_created.saturating_add(1);
            }
            continue;
        }
        if !entry_type.is_file() {
            // Skip symlinks, hardlinks, fifos, devices. Cook artifacts
            // only carry regular files + directories.
            continue;
        }
        if dest.exists() {
            report.files_skipped = report.files_skipped.saturating_add(1);
            continue;
        }
        let precise_mtime = pax_mtime(&mut entry);
        if let Some(parent) = dest.parent() {
            if !parent.is_dir() {
                std::fs::create_dir_all(parent)?;
            }
        }
        // Capture the mode before consuming the entry body: after the
        // copy, `entry` is drained and the header borrow is awkward.
        let mode_bits = entry.header().mode().ok();
        // tar's `unpack_in` would overwrite; we manually copy bytes
        // so the skip-existing invariant cannot be violated.
        let mut out = File::create(&dest)?;
        std::io::copy(&mut entry, &mut out)?;
        // #1880: hand-copying the body means nothing ever applies the
        // tar header's Unix mode, so every restored file lands at the
        // process umask default (0o644). Cargo's `build-script-build`
        // binaries then fail execve with EACCES and the build dies on
        // an arbitrary crate -- whichever build script cargo reaches
        // first. This is the same defect #587 fixed for the save/load
        // path (`save.rs` extract_one); the cook extractor predates
        // that fix and never inherited it.
        //
        // The platform crate owns the mode application: Unix restores
        // the archived bits, Windows ignores them (NTFS uses ACLs and
        // the tar header's Unix mode carries no meaning there).
        // Drop the handle first so the chmod applies to a settled
        // file rather than racing our own open descriptor.
        drop(out);
        crate::platform::fs::permissions::restore_mode(&dest, mode_bits)?;
        if let Some(mtime) = precise_mtime {
            zccache::core::mtime::stamp_mtime(&dest, mtime)?;
        }
        report.files_written = report.files_written.saturating_add(1);
    }
    Ok(report)
}

/// Move the corrupted artifact to `<sha>.tar.zst.quarantine` so a
/// subsequent CookLookup-driven re-cook can re-populate the slot
/// without confusing the operator. Best-effort: a rename failure is
/// surfaced but never blocks the cook flow — the on-disk garbage will
/// be GC'd by the auto-gc cook pass.
pub fn quarantine_artifact(path: &Path) -> Result<PathBuf, RegistryError> {
    let mut name = path
        .file_name()
        .map(|n| n.to_owned())
        .unwrap_or_else(|| std::ffi::OsString::from("artifact"));
    name.push(".quarantine");
    let quarantine = path
        .parent()
        .map(|p| p.join(&name))
        .unwrap_or_else(|| PathBuf::from(&name));
    std::fs::rename(path, &quarantine)?;
    Ok(quarantine)
}

/// Reject tar entries whose normalized path either has an absolute
/// root, walks above the destination via `..`, or is empty.
fn is_unsafe_tar_path(path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return true;
    }
    if path.is_absolute() {
        return true;
    }
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => return true,
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return true,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn cook_level_defaults_to_9_and_follows_soldr_zstd_level() {
        // 9 packs a real obs-studio closure in 1.2 s at 53 MiB; 19 with the
        // long window took 26 s for 38 MiB (soldr#4162).
        assert_eq!(cook_zstd_level_from(None), 9);
        assert_eq!(cook_zstd_level_from(Some(1)), 1);
        assert_eq!(cook_zstd_level_from(Some(-3)), -3);
    }

    #[test]
    fn default_cook_archive_skips_the_long_window() {
        assert!(!cook_uses_long_window(cook_zstd_level_from(None)));
    }

    #[test]
    fn config_default_matches_the_cook_level() {
        assert_eq!(crate::core::CookConfig::default_zstd_level(), COOK_ZSTD_LEVEL);
    }

    #[test]
    fn long_window_only_at_high_levels() {
        assert!(cook_uses_long_window(19));
        assert!(cook_uses_long_window(COOK_LONG_WINDOW_MIN_LEVEL));
        assert!(!cook_uses_long_window(COOK_LONG_WINDOW_MIN_LEVEL - 1));
        assert!(!cook_uses_long_window(1));
        assert!(!cook_uses_long_window(-5));
    }

    #[test]
    fn fast_level_archive_round_trips() {
        let dir = TempDir::new().expect("tempdir");
        let source = dir.path().join("debug");
        write_file(&source.join("deps/libfoo.rlib"), &[7u8; 4096]);
        let cook = dir.path().join("cook");
        let fast = pack_cook_archive_at_level(&source, &cook, -1).expect("pack -1");
        let slow =
            pack_cook_archive_at_level(&source, &dir.path().join("cook19"), 19).expect("pack 19");
        assert_ne!(fast.sha256, slow.sha256, "the level must reach the encoder");
        let decoder = zstd::Decoder::new(File::open(&fast.path).expect("open")).expect("decoder");
        let mut archive = tar::Archive::new(decoder);
        let names: Vec<String> = archive
            .entries()
            .expect("entries")
            .map(|e| {
                e.expect("entry")
                    .path()
                    .expect("path")
                    .display()
                    .to_string()
            })
            .collect();
        assert!(
            names.iter().any(|n| n == "debug/deps/libfoo.rlib"),
            "{names:?}"
        );
    }

    fn write_file(p: &Path, bytes: &[u8]) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        let mut f = File::create(p).expect("create");
        f.write_all(bytes).expect("write");
    }

    fn manifest_name() -> &'static str {
        concat!("Car", "go.toml")
    }

    fn lock_name() -> &'static str {
        concat!("Car", "go.lock")
    }

    #[test]
    fn recipe_hash_skips_large_operational_state_trees() {
        let dir = TempDir::new().expect("tempdir");
        write_file(&dir.path().join(lock_name()), b"lock-v1\n");
        write_file(&dir.path().join(manifest_name()), b"[workspace]\n");
        write_file(
            &dir.path().join("crates/app").join(manifest_name()),
            b"[package]\nname='app'\nversion='0.1.0'\n",
        );
        write_file(
            &dir.path().join("vendor/notify").join(manifest_name()),
            b"[package]\nname='notify'\nversion='1.0.0'\n",
        );
        let baseline = compute_recipe_hash_proxy(dir.path()).expect("baseline hash");

        let mut state_dirs = vec![
            ".cache",
            ".claude",
            ".clud",
            ".codex",
            ".extern-repos",
            ".git",
            ".loop",
            ".perf-local",
            ".pytest_cache",
            ".rustup",
            ".soldr",
            ".venv",
            "node_modules",
            "target",
        ];
        state_dirs.push(concat!(".car", "go"));
        for state_dir in state_dirs {
            for index in 0..100 {
                write_file(
                    &dir.path()
                        .join(state_dir)
                        .join(format!("decoy-{index}"))
                        .join(manifest_name()),
                    format!("[package]\nname='decoy-{index}'\n").as_bytes(),
                );
            }
        }

        let after_decoys = compute_recipe_hash_proxy(dir.path()).expect("hash after decoys");
        assert_eq!(
            after_decoys, baseline,
            "operational state changed recipe hash"
        );

        write_file(
            &dir.path().join("crates/app").join(manifest_name()),
            b"[package]\nname='app'\nversion='0.2.0'\n",
        );
        assert_ne!(
            compute_recipe_hash_proxy(dir.path()).expect("changed source hash"),
            baseline,
            "included source manifest mutation must change the recipe hash"
        );
    }

    #[test]
    fn referenced_manifests_override_operational_tree_exclusions() {
        let dir = TempDir::new().expect("tempdir");
        write_file(&dir.path().join(lock_name()), b"lock-v1\n");
        write_file(
            &dir.path().join(manifest_name()),
            b"[workspace]\nmembers=['crates/app']\n",
        );
        write_file(
            &dir.path().join("crates/app").join(manifest_name()),
            b"[package]\nname='app'\nversion='0.1.0'\n\
              [dependencies]\nhidden={path='../../.extern-repos/member'}\n",
        );
        let member = dir
            .path()
            .join(".extern-repos/member")
            .join(manifest_name());
        let path_dependency = dir
            .path()
            .join(".perf-local/path-dep")
            .join(manifest_name());
        write_file(
            &member,
            b"[package]\nname='member'\nversion='0.1.0'\n\
              [dependencies]\ntransitive={path='../../.perf-local/path-dep'}\n",
        );
        write_file(
            &path_dependency,
            b"[package]\nname='path-dep'\nversion='0.1.0'\n",
        );
        let baseline = compute_recipe_hash_proxy(dir.path()).expect("baseline hash");

        write_file(
            &member,
            b"[package]\nname='member'\nversion='0.2.0'\n\
              [dependencies]\ntransitive={path='../../.perf-local/path-dep'}\n",
        );
        let member_changed = compute_recipe_hash_proxy(dir.path()).expect("member hash");
        assert_ne!(
            member_changed, baseline,
            "workspace member must affect hash"
        );

        write_file(
            &member,
            b"[package]\nname='member'\nversion='0.1.0'\n\
              [dependencies]\ntransitive={path='../../.perf-local/path-dep'}\n",
        );
        write_file(
            &path_dependency,
            b"[package]\nname='path-dep'\nversion='0.2.0'\n",
        );
        assert_ne!(
            compute_recipe_hash_proxy(dir.path()).expect("path dependency hash"),
            baseline,
            "path dependency must affect hash"
        );
    }

    #[test]
    fn pack_writes_content_addressed_artifact() {
        let dir = TempDir::new().expect("tempdir");
        let source = dir.path().join("release");
        write_file(&source.join("deps").join("libfoo-abc.rlib"), b"hello\n");
        write_file(&source.join("deps").join("libbar-def.rmeta"), b"world\n");

        let cook_dir = dir.path().join("cook");
        let packed = pack_cook_archive(&source, &cook_dir).expect("pack");

        // Filename matches the sha256 hex.
        let expected = artifact_path_for_sha(&cook_dir, &packed.sha256);
        assert_eq!(packed.path, expected);
        assert!(packed.path.is_file());
        assert!(packed.size_bytes > 0);

        // Sanity: tmp dir is empty after success.
        let tmp = cook_dir.join(".tmp");
        let leftover_count = std::fs::read_dir(&tmp)
            .map(|it| it.flatten().count())
            .unwrap_or(0);
        assert_eq!(leftover_count, 0);
    }

    #[test]
    fn pack_is_idempotent_on_same_content() {
        let dir = TempDir::new().expect("tempdir");
        let source = dir.path().join("release");
        write_file(&source.join("deps").join("libfoo-abc.rlib"), b"hello\n");

        let cook_dir = dir.path().join("cook");
        let a = pack_cook_archive(&source, &cook_dir).expect("a");
        let b = pack_cook_archive(&source, &cook_dir).expect("b");

        // Identical input → identical sha → identical artifact path.
        // (Note: tar entries carry per-file mtimes; zstd output may
        // differ. We assert path stability via the daemon-side hash,
        // not byte stability across runs.)
        assert_eq!(a.path.file_name(), b.path.file_name());
        assert!(a.path.is_file());
    }

    #[test]
    fn pack_errors_on_missing_source_dir() {
        let dir = TempDir::new().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        let cook_dir = dir.path().join("cook");
        let err = pack_cook_archive(&missing, &cook_dir).expect_err("must error");
        match err {
            RegistryError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected Io NotFound, got {other:?}"),
        }
    }

    #[test]
    fn sha_abbrev_is_twelve_lowercase_hex_chars() {
        let bytes = [0xAB; 32];
        let abbrev = sha_abbrev(&bytes);
        assert_eq!(abbrev.len(), 12);
        assert!(abbrev
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn artifact_path_matches_sha_filename() {
        let cook = Path::new("/cook");
        let sha = [0x12u8; 32];
        let path = artifact_path_for_sha(cook, &sha);
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.ends_with(".tar.zst"));
        assert!(name.starts_with(&hex_lower(&sha)));
    }

    #[test]
    fn verify_sha256_matches_packed_artifact() {
        let dir = TempDir::new().expect("tempdir");
        let source = dir.path().join("release");
        write_file(&source.join("deps").join("libfoo-abc.rlib"), b"hello\n");
        let cook = dir.path().join("cook");
        let packed = pack_cook_archive(&source, &cook).expect("pack");
        assert!(verify_sha256(&packed.path, &packed.sha256).expect("verify"));
        // Flip a byte → mismatch.
        let mut wrong = packed.sha256;
        wrong[0] ^= 0xFF;
        assert!(!verify_sha256(&packed.path, &wrong).expect("verify"));
    }

    #[test]
    fn extract_round_trips_packed_bytes() {
        let dir = TempDir::new().expect("tempdir");
        let source = dir.path().join("release");
        write_file(&source.join("deps").join("libfoo-abc.rlib"), b"foo\n");
        write_file(&source.join("deps").join("libbar-def.rmeta"), b"bar\n");
        let cook = dir.path().join("cook");
        let packed = pack_cook_archive(&source, &cook).expect("pack");

        let dest = dir.path().join("target");
        std::fs::create_dir_all(&dest).unwrap();
        let report = extract_skip_existing(&packed.path, &dest).expect("extract");
        assert!(report.files_written >= 2, "expected files written");
        assert_eq!(report.files_skipped, 0);

        let restored_foo = std::fs::read(dest.join("release").join("deps").join("libfoo-abc.rlib"))
            .expect("read foo");
        assert_eq!(restored_foo, b"foo\n");
    }

    #[test]
    fn extract_preserves_fractional_mtime_for_cargo_fingerprints() {
        let dir = TempDir::new().expect("tempdir");
        let source = dir.path().join("release");
        let artifact = source.join("deps").join("libfoo-abc.rlib");
        write_file(&artifact, b"foo\n");
        let expected = filetime::FileTime::from_unix_time(1_700_000_000, 123_456_700);
        filetime::set_file_mtime(&artifact, expected).expect("set source mtime");

        let cook = dir.path().join("cook");
        let packed = pack_cook_archive(&source, &cook).expect("pack");
        let dest = dir.path().join("target");
        extract_skip_existing(&packed.path, &dest).expect("extract");

        let restored = filetime::FileTime::from_last_modification_time(
            &std::fs::metadata(dest.join("release/deps/libfoo-abc.rlib"))
                .expect("restored metadata"),
        );
        assert_eq!(
            restored, expected,
            "tar hydration must not round to seconds"
        );
    }

    // #1880: cargo's `build-script-build` binaries must come back
    // executable. The extractor hand-copies bytes into a fresh
    // `File::create` (to honor skip-existing), which lands at the
    // umask default and silently drops `+x` unless the tar header's
    // mode is applied explicitly.
    //
    // The consequence in the field is badly disguised: cargo reports
    // `could not execute process ... (never executed)` /
    // `Permission denied (os error 13)` naming whichever build script
    // it happened to reach first, so the failure looks like a flaky
    // problem with an unrelated third-party crate.
    //
    // The regression test for this lives in
    // `tests/cook_archive_executable_bit.rs` (Unix-only, since mode
    // bits only exist there).
    #[test]
    fn extract_skips_existing_files() {
        let dir = TempDir::new().expect("tempdir");
        let source = dir.path().join("release");
        write_file(&source.join("deps").join("libfoo-abc.rlib"), b"FRESH\n");
        let cook = dir.path().join("cook");
        let packed = pack_cook_archive(&source, &cook).expect("pack");

        // Pre-populate the destination with user content that must NOT
        // be overwritten.
        let dest = dir.path().join("target");
        let user_path = dest.join("release").join("deps").join("libfoo-abc.rlib");
        write_file(&user_path, b"USER-OWNS-THIS\n");

        let report = extract_skip_existing(&packed.path, &dest).expect("extract");
        assert!(report.files_skipped >= 1, "expected skip count");
        let on_disk = std::fs::read(&user_path).expect("read");
        assert_eq!(
            on_disk, b"USER-OWNS-THIS\n",
            "must not overwrite user state"
        );
    }

    #[test]
    fn quarantine_renames_artifact_in_place() {
        let dir = TempDir::new().expect("tempdir");
        let cook = dir.path().join("cook");
        std::fs::create_dir_all(&cook).unwrap();
        let art = cook.join("deadbeef.tar.zst");
        write_file(&art, b"corrupt bytes");
        let quarantined = quarantine_artifact(&art).expect("quarantine");
        assert!(!art.exists());
        assert!(quarantined.exists());
        assert!(quarantined
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".quarantine"));
    }

    #[test]
    fn is_unsafe_tar_path_rejects_traversal() {
        assert!(is_unsafe_tar_path(Path::new("../etc/passwd")));
        assert!(is_unsafe_tar_path(Path::new("/etc/passwd")));
        assert!(is_unsafe_tar_path(Path::new("")));
        assert!(!is_unsafe_tar_path(Path::new("release/deps/libfoo.rlib")));
    }
}
