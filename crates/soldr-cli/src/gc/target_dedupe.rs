//! Content-addressed dedupe of workspace-independent `target/` outputs
//! across worktrees (soldr#4033, follow-up to soldr#4031).
//!
//! zccache already links cached rlib/rmeta into each worktree when the
//! probed pair supports it, but two classes of output still get one full
//! copy per worktree:
//!
//! 1. build-script outputs (`target/<profile>/build/<pkg>-<hash>/out/**`),
//!    which are not rustc invocations and so never pass through the
//!    wrapper; and
//! 2. dependency artifacts delivered by plain COPY (ext4 without reflink,
//!    or cache and target on different volumes).
//!
//! # Why an offline dedupe pass, not build-script caching
//!
//! Caching build scripts as a unit is unsound in general: a build script
//! may read any file or env var, probe the host (pkg-config, compilers),
//! and its `rerun-if-*` directives are advisory and often absent. Keying a
//! cache on "crate id + script hash + declared inputs" would replay stale
//! output whenever an undeclared input changed. Deduplicating the bytes a
//! build script *already produced* has none of that risk: cargo still
//! decides when to rerun it, and we only collapse files that are already
//! byte-identical (blake3) into one stored copy.
//!
//! # Safety rules
//!
//! * Only immutable artifacts are touched: `deps/*.rlib` / `deps/*.rmeta`
//!   (rustc writes these to a temp file and renames, never in place) and
//!   files under `build/*/out/`.
//! * Build-script `out/` files are only ever *reflinked* (copy-on-write).
//!   Build scripts commonly rewrite `OUT_DIR` files in place with
//!   `fs::write`, which on a hardlink would corrupt every other worktree
//!   (zccache#1791). A reflink gives each path its own inode, so an in-place
//!   write only un-shares that one file.
//! * `deps/` artifacts may be hardlinked to the store when no reflink is
//!   available. The shared inode is made read-only first, so any in-place
//!   writer fails loudly instead of silently corrupting siblings.
//! * A target with a live cargo build is skipped; for the rest, every
//!   `.cargo-lock` is held for the whole pass so no build can start
//!   mid-rewrite. Files younger than `min_age_secs`, smaller than
//!   `min_size`, or changed while being hashed are left alone.
//! * Hardlinks never cross volumes: the store is only used for files on
//!   the same device as the store.
//!
//! Needs device/inode/link-count identity from `soldr_platform`; where the
//! host has none (Windows) the pass reports `unsupported`.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// Default lower bound on file size worth deduplicating.
pub(crate) const DEDUPE_MIN_SIZE_BYTES: u64 = 4 * 1024;

/// Reflinked blobs older than this are pruned from the store. Reflinks are
/// independent copies, so pruning only drops the dedupe source; the next
/// pass reseeds it from a surviving target.
pub(crate) const COW_BLOB_RETENTION_SECS: u64 = 30 * 24 * 60 * 60;

/// How a target-relative path may be shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactClass {
    /// `deps/*.rlib|rmeta`: hardlink or reflink.
    Deps,
    /// `build/<pkg>/out/**`: reflink only.
    BuildOut,
}

/// Classify a path relative to the `target/` root. `None` means the file
/// is not an immutable workspace-independent artifact and is never touched.
pub(crate) fn classify_target_path(rel: &Path) -> Option<ArtifactClass> {
    let comps: Vec<&str> = rel
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    let name = *comps.last()?;
    if name.starts_with('.') || name.ends_with(".soldr-dedupe") {
        return None;
    }
    // target/<profile>/deps/<f> or target/<triple>/<profile>/deps/<f>
    if (comps.len() == 3 || comps.len() == 4) && comps[comps.len() - 2] == "deps" {
        let ext = Path::new(name).extension().and_then(|e| e.to_str());
        return matches!(ext, Some("rlib") | Some("rmeta")).then_some(ArtifactClass::Deps);
    }
    // target/<profile>/build/<pkg>/out/** (optionally under <triple>/)
    for i in 1..=2 {
        if comps.get(i) == Some(&"build") && comps.get(i + 2) == Some(&"out") && comps.len() > i + 3
        {
            return Some(ArtifactClass::BuildOut);
        }
    }
    None
}

#[derive(Debug, Clone)]
pub(crate) struct DedupeOptions {
    /// Store root; blobs live in `<store>/hard/` and `<store>/cow/`.
    pub store: PathBuf,
    pub min_age_secs: u64,
    pub min_size: u64,
    pub dry_run: bool,
}

impl DedupeOptions {
    pub(crate) fn for_paths(paths: &crate::core::SoldrPaths, min_age_secs: u64) -> Self {
        Self {
            store: paths.root.join("target-store"),
            min_age_secs,
            min_size: DEDUPE_MIN_SIZE_BYTES,
            dry_run: false,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct DedupeReport {
    pub supported: bool,
    pub dry_run: bool,
    pub targets_scanned: usize,
    pub targets_skipped_active: usize,
    pub files_examined: u64,
    pub files_hardlinked: u64,
    pub files_reflinked: u64,
    pub files_added_to_store: u64,
    pub files_already_shared: u64,
    pub bytes_saved: u64,
    pub skipped_unsupported: u64,
    pub errors: u64,
    pub blobs_pruned: u64,
}

/// Per-target split of on-disk bytes into those shared with another link
/// (not freed by deleting this target) and those unique to it.
#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct TargetSharing {
    pub total_bytes: u64,
    pub shared_bytes: u64,
    pub unique_bytes: u64,
}

/// Walk `target` and split its regular-file bytes into shared (link count
/// above one) and unique. Reflinked extents are not visible to userspace
/// and count as unique.
pub(crate) fn measure_target_sharing(target: &Path) -> TargetSharing {
    let mut out = TargetSharing::default();
    for (path, _) in walk_files(target) {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        out.total_bytes = out.total_bytes.saturating_add(meta.len());
        if link_count(&meta) > 1 {
            out.shared_bytes = out.shared_bytes.saturating_add(meta.len());
        } else {
            out.unique_bytes = out.unique_bytes.saturating_add(meta.len());
        }
    }
    out
}

/// Every regular file under `root` (symlinks not followed), with its path
/// relative to `root`.
fn walk_files(root: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                pending.push(path);
            } else if ft.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    let rel = rel.to_path_buf();
                    out.push((path, rel));
                }
            }
        }
    }
    out.sort();
    out
}

fn link_count(meta: &std::fs::Metadata) -> u64 {
    identity(meta).map_or(1, |id| id.links)
}

/// Device, inode and link count, or `None` where the host does not expose
/// them (Windows), in which case the dedupe pass is unsupported.
fn identity(meta: &std::fs::Metadata) -> Option<soldr_platform::fs::identity::HardlinkIdentity> {
    soldr_platform::fs::identity::hardlink_identity(meta)
}

/// Deduplicate eligible artifacts across `targets` into the store.
pub(crate) fn dedupe_targets(targets: &[PathBuf], opts: &DedupeOptions) -> DedupeReport {
    let mut report = DedupeReport {
        dry_run: opts.dry_run,
        ..DedupeReport::default()
    };
    imp::run(targets, opts, &mut report);
    report
}

mod imp {
    use super::{
        classify_target_path, identity, walk_files, ArtifactClass, DedupeOptions, DedupeReport,
    };
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    struct Ctx<'a> {
        opts: &'a DedupeOptions,
        store_dev: Option<u64>,
        verified: HashSet<PathBuf>,
        /// Dry run: blobs that would exist after this pass.
        virtual_blobs: HashSet<PathBuf>,
        now: std::time::SystemTime,
    }

    pub(super) fn run(targets: &[PathBuf], opts: &DedupeOptions, report: &mut DedupeReport) {
        // Without device/inode/link-count identity (Windows) neither the
        // already-shared check nor store pruning is sound: do nothing.
        report.supported = std::fs::metadata(std::env::temp_dir())
            .ok()
            .and_then(|m| identity(&m))
            .is_some();
        if !report.supported {
            return;
        }
        if !opts.dry_run {
            for sub in ["hard", "cow"] {
                if std::fs::create_dir_all(opts.store.join(sub)).is_err() {
                    report.errors += 1;
                    return;
                }
            }
        }
        let store_dev = std::fs::metadata(&opts.store)
            .ok()
            .and_then(|m| identity(&m))
            .map(|id| id.dev);
        let mut ctx = Ctx {
            opts,
            store_dev,
            verified: HashSet::new(),
            virtual_blobs: HashSet::new(),
            now: std::time::SystemTime::now(),
        };
        for target in targets {
            if !target.is_dir() {
                continue;
            }
            // Hold every .cargo-lock for the whole target so no build can
            // start while files are being swapped.
            let _guard = match crate::cache_lib::cargo_lock::probe(target) {
                Ok(crate::cache_lib::cargo_lock::CargoLockProbe::Idle(guard)) => guard,
                Ok(crate::cache_lib::cargo_lock::CargoLockProbe::Active(_)) => {
                    report.targets_skipped_active += 1;
                    continue;
                }
                Err(_) => {
                    report.errors += 1;
                    continue;
                }
            };
            report.targets_scanned += 1;
            for (path, rel) in walk_files(target) {
                let Some(class) = classify_target_path(&rel) else {
                    continue;
                };
                report.files_examined += 1;
                if let Err(()) = process_file(&mut ctx, &path, class, report) {
                    report.errors += 1;
                }
            }
        }
        if !opts.dry_run {
            report.blobs_pruned = prune_store(&opts.store, ctx.now);
        }
    }

    fn hash_file(path: &Path) -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        std::io::copy(&mut file, &mut hasher)?;
        Ok(hasher.finalize().to_hex().to_string())
    }

    fn blob_path(store: &Path, kind: &str, hex: &str) -> PathBuf {
        store.join(kind).join(&hex[..2]).join(hex)
    }

    fn tmp_sibling(path: &Path) -> PathBuf {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{}.soldr-dedupe", std::process::id()));
        path.with_file_name(name)
    }

    /// Atomically replace `file` with `make(tmp)`'s result.
    fn swap_in(file: &Path, make: impl FnOnce(&Path) -> std::io::Result<()>) -> bool {
        let tmp = tmp_sibling(file);
        let _ = std::fs::remove_file(&tmp);
        if make(&tmp).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        if std::fs::rename(&tmp, file).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        true
    }

    fn reflink(src: &Path, dst: &Path) -> std::io::Result<()> {
        soldr_platform::fs::reflink::reflink_file(src, dst)
    }

    fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
        match (identity(a), identity(b)) {
            (Some(a), Some(b)) => a.dev == b.dev && a.index == b.index,
            _ => false,
        }
    }

    fn process_file(
        ctx: &mut Ctx<'_>,
        path: &Path,
        class: ArtifactClass,
        report: &mut DedupeReport,
    ) -> Result<(), ()> {
        let meta = std::fs::symlink_metadata(path).map_err(|_| ())?;
        if !meta.is_file() || meta.len() < ctx.opts.min_size {
            return Ok(());
        }
        let mtime = meta.modified().map_err(|_| ())?;
        let age = ctx.now.duration_since(mtime).unwrap_or_default().as_secs();
        if age < ctx.opts.min_age_secs {
            return Ok(());
        }
        let hex = hash_file(path).map_err(|_| ())?;
        // Changed while hashing: leave it for a later pass.
        let after = std::fs::symlink_metadata(path).map_err(|_| ())?;
        if after.len() != meta.len() || after.modified().ok() != Some(mtime) {
            return Ok(());
        }
        let Some(id) = identity(&meta) else {
            report.skipped_unsupported += 1;
            return Ok(());
        };
        let same_dev = ctx.store_dev == Some(id.dev);

        match class {
            ArtifactClass::Deps => {
                let blob = blob_path(&ctx.opts.store, "hard", &hex);
                if let Ok(bmeta) = std::fs::symlink_metadata(&blob) {
                    if same_file(&bmeta, &meta) {
                        report.files_already_shared += 1;
                        return Ok(());
                    }
                }
                if id.links > 1 {
                    // Linked by someone else (cargo uplift, zccache); not ours
                    // to rewrite.
                    report.files_already_shared += 1;
                    return Ok(());
                }
                if !same_dev && !ctx.opts.dry_run {
                    report.skipped_unsupported += 1;
                    return Ok(());
                }
                link_or_store(ctx, path, &meta, &blob, &hex, true, report)
            }
            ArtifactClass::BuildOut => {
                let blob = blob_path(&ctx.opts.store, "cow", &hex);
                link_or_store(ctx, path, &meta, &blob, &hex, false, report)
            }
        }
    }

    /// Point `path` at the stored copy of its content, or seed the store
    /// from it. `allow_hardlink` is false for build-script outputs.
    fn link_or_store(
        ctx: &mut Ctx<'_>,
        path: &Path,
        meta: &std::fs::Metadata,
        blob: &Path,
        hex: &str,
        allow_hardlink: bool,
        report: &mut DedupeReport,
    ) -> Result<(), ()> {
        if ctx.opts.dry_run {
            if blob.exists() || ctx.virtual_blobs.contains(blob) {
                report.bytes_saved = report.bytes_saved.saturating_add(meta.len());
                if allow_hardlink {
                    report.files_hardlinked += 1;
                } else {
                    report.files_reflinked += 1;
                }
            } else {
                ctx.virtual_blobs.insert(blob.to_path_buf());
                report.files_added_to_store += 1;
            }
            return Ok(());
        }

        match std::fs::symlink_metadata(blob) {
            Ok(bmeta) => {
                if bmeta.len() != meta.len() {
                    return Err(());
                }
                if !ctx.verified.contains(blob) {
                    if hash_file(blob).map_err(|_| ())? != hex {
                        // Corrupt blob: drop it; the next pass reseeds.
                        let _ = remove_blob(blob);
                        return Err(());
                    }
                    ctx.verified.insert(blob.to_path_buf());
                }
                if swap_in(path, |tmp| reflink(blob, tmp)) {
                    report.files_reflinked += 1;
                } else if allow_hardlink && swap_in(path, |tmp| std::fs::hard_link(blob, tmp)) {
                    report.files_hardlinked += 1;
                } else {
                    report.skipped_unsupported += 1;
                    return Ok(());
                }
                report.bytes_saved = report.bytes_saved.saturating_add(meta.len());
                Ok(())
            }
            Err(_) => {
                let parent = blob.parent().ok_or(())?;
                std::fs::create_dir_all(parent).map_err(|_| ())?;
                let tmp = tmp_sibling(blob);
                let _ = std::fs::remove_file(&tmp);
                let seeded = if allow_hardlink {
                    // Read-only before sharing the inode: an in-place writer
                    // must fail instead of corrupting other worktrees.
                    let mut perms = meta.permissions();
                    perms.set_readonly(true);
                    std::fs::set_permissions(path, perms).is_ok()
                        && std::fs::hard_link(path, &tmp).is_ok()
                } else {
                    reflink(path, &tmp).is_ok()
                };
                if !seeded {
                    let _ = std::fs::remove_file(&tmp);
                    report.skipped_unsupported += 1;
                    return Ok(());
                }
                if std::fs::rename(&tmp, blob).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(());
                }
                ctx.verified.insert(blob.to_path_buf());
                report.files_added_to_store += 1;
                Ok(())
            }
        }
    }

    fn remove_blob(blob: &Path) -> std::io::Result<()> {
        std::fs::remove_file(blob)
    }

    /// Drop hardlink blobs no worktree references any more (link count 1)
    /// and reflink blobs older than `COW_BLOB_RETENTION_SECS`.
    fn prune_store(store: &Path, now: std::time::SystemTime) -> u64 {
        let mut pruned = 0;
        for (path, _) in walk_files(&store.join("hard")) {
            if std::fs::symlink_metadata(&path)
                .ok()
                .and_then(|m| identity(&m))
                .is_some_and(|id| id.links == 1)
                && remove_blob(&path).is_ok()
            {
                pruned += 1;
            }
        }
        for (path, _) in walk_files(&store.join("cow")) {
            let stale = std::fs::symlink_metadata(&path)
                .and_then(|m| m.modified())
                .map(|t| {
                    now.duration_since(t).unwrap_or_default().as_secs()
                        >= super::COW_BLOB_RETENTION_SECS
                })
                .unwrap_or(false);
            if stale && remove_blob(&path).is_ok() {
                pruned += 1;
            }
        }
        pruned
    }
}

/// `soldr gc dedupe` (soldr#4033): collapse byte-identical immutable
/// artifacts across worktree `target/` dirs into one content-addressed
/// stored copy, then report per-target shared vs. unique bytes.
pub(crate) fn run_gc_dedupe_command(
    targets: Vec<std::path::PathBuf>,
    dry_run: bool,
    json: bool,
) -> Result<(), crate::core::SoldrError> {
    let paths = crate::core::SoldrPaths::new()?;
    let config = paths
        .load_config()
        .map_err(|error| crate::core::SoldrError::Other(error.to_string()))?;
    let targets = if targets.is_empty() {
        super::auto::collect_workspace_targets(&paths).0
    } else {
        targets
    };
    let mut opts = DedupeOptions::for_paths(&paths, config.auto_gc.min_age_secs);
    opts.dry_run = dry_run;
    let report = dedupe_targets(&targets, &opts);
    let sharing: Vec<GcDedupeTargetOutput> = targets
        .iter()
        .filter(|t| t.is_dir())
        .map(|t| {
            let s = measure_target_sharing(t);
            GcDedupeTargetOutput {
                path: t.display().to_string(),
                total_bytes: s.total_bytes,
                shared_bytes: s.shared_bytes,
                unique_bytes: s.unique_bytes,
            }
        })
        .collect();
    if json {
        return crate::cache::print_json(&GcDedupeOutput {
            schema_version: 1,
            report,
            targets: sharing,
        });
    }
    if !report.supported {
        println!("soldr gc dedupe: unsupported on this platform; nothing done.");
        return Ok(());
    }
    let human = crate::cache_lib::target_registry::human_size;
    println!(
        "soldr gc dedupe{}: {} target dir(s) scanned, {} skipped (active build); \
         {} saved ({} hardlinked, {} reflinked, {} newly stored, {} already shared, \
         {} unsupported, {} errors, {} store blobs pruned)",
        if dry_run { " (dry run)" } else { "" },
        report.targets_scanned,
        report.targets_skipped_active,
        human(report.bytes_saved),
        report.files_hardlinked,
        report.files_reflinked,
        report.files_added_to_store,
        report.files_already_shared,
        report.skipped_unsupported,
        report.errors,
        report.blobs_pruned,
    );
    for t in &sharing {
        println!(
            "  {}  total={}  shared={}  unique={}",
            t.path,
            human(t.total_bytes),
            human(t.shared_bytes),
            human(t.unique_bytes)
        );
    }
    Ok(())
}

#[derive(Serialize)]
struct GcDedupeOutput {
    schema_version: u32,
    report: DedupeReport,
    targets: Vec<GcDedupeTargetOutput>,
}

#[derive(Serialize)]
struct GcDedupeTargetOutput {
    path: String,
    total_bytes: u64,
    shared_bytes: u64,
    unique_bytes: u64,
}

#[cfg(test)]
#[path = "target_dedupe_tests.rs"]
mod tests;
