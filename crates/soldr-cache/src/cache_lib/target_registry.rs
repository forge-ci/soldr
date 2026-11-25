//! SQLite-backed registry of observed Cargo `target/` directories.
//!
//! Implements the data plane for issue #234: every `RUSTC_WRAPPER`
//! invocation upserts the resolved workspace `target/` path with the
//! current unix timestamp. The `soldr gc` command later scans the
//! registry to find stale candidates for reclamation.
//!
//! The store lives in `~/.soldr/state.sqlite3` alongside other soldr state.

use crate::cache_lib::state_store::{open_state_db, open_state_db_best_effort, StateDbHandle};
use rusqlite::{params, OptionalExtension};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

/// Default file name for the soldr state database under `~/.soldr/`.
pub const DATA_DB_FILE: &str = super::state_db::STATE_DB_FILE;

/// Default staleness threshold (10 days) for `soldr gc` and the startup warning.
pub const DEFAULT_STALE_AGE_SECONDS: u64 = 10 * 24 * 60 * 60;

/// Default size threshold (256 MiB) for `soldr gc` and the startup warning.
pub const DEFAULT_STALE_SIZE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum RegistryError {
    /// Rendered with the SQLite extended code (soldr#3290), so a
    /// `database is locked` says whether it was an instant WAL snapshot
    /// conflict (517) or a genuine timed-out wait (5).
    #[error("sqlite error: {}", crate::cache_lib::state_store::describe_sqlite_error(.0))]
    Sqlite(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("system clock before unix epoch: {0}")]
    Clock(String),
}

/// One row from the `targets` table augmented with the current
/// observation time so callers can compute the age cheaply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRow {
    pub path: PathBuf,
    pub last_used: i64,
}

/// SQLite registry backed by `~/.soldr/state.sqlite3` (or a caller path).
pub struct TargetRegistry {
    db: StateDbHandle,
}

impl TargetRegistry {
    /// Open or create the database at the given path. Parent dirs are
    /// created automatically. WAL mode means this handle coexists with
    /// concurrent readers and one writer across processes — the redb-era
    /// in-process open mutex and exclusive-file-lock choreography (#608)
    /// are gone.
    pub fn open(path: &Path) -> Result<Self, RegistryError> {
        Ok(Self {
            db: open_state_db(path)?,
        })
    }

    /// Open for a latency-critical, losable write (issue #1814).
    ///
    /// Same as [`TargetRegistry::open`] but a contended write waits tens of
    /// milliseconds instead of the full busy budget. The wrapper's per-rustc
    /// `target/` touch uses it because the row is GC bookkeeping that the
    /// next invocation re-touches — stalling a compile to write it is
    /// strictly worse than skipping it.
    pub fn open_best_effort(path: &Path) -> Result<Self, RegistryError> {
        Ok(Self {
            db: open_state_db_best_effort(path)?,
        })
    }

    /// Open an in-memory database (tests, or callers that avoid disk).
    pub fn open_in_memory() -> Result<Self, RegistryError> {
        Ok(Self {
            db: crate::cache_lib::state_store::open_state_db_in_memory()?,
        })
    }

    /// Insert or update the row for `path` with the supplied unix timestamp.
    pub fn upsert_with_time(&self, path: &Path, unix_seconds: i64) -> Result<(), RegistryError> {
        let path_str = path_to_string(path);
        self.db.execute(
            "INSERT INTO target_registry_targets(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![path_str, unix_seconds],
        )?;
        Ok(())
    }

    /// Convenience wrapper that stamps the row with the current time.
    pub fn upsert(&self, path: &Path) -> Result<(), RegistryError> {
        self.upsert_with_time(path, current_unix_seconds()?)
    }

    /// Return all tracked rows, ordered by `last_used` ascending.
    pub fn list(&self) -> Result<Vec<TargetRow>, RegistryError> {
        let mut statement = self.db.prepare(
            "SELECT key, value FROM target_registry_targets ORDER BY value ASC, key ASC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(TargetRow {
                    path: PathBuf::from(row.get::<_, String>(0)?),
                    last_used: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Look up a single tracked row by path, if present.
    pub fn get(&self, path: &Path) -> Result<Option<TargetRow>, RegistryError> {
        let path_str = path_to_string(path);
        let last_used: Option<i64> = self
            .db
            .query_row(
                "SELECT value FROM target_registry_targets WHERE key = ?1",
                params![path_str],
                |row| row.get(0),
            )
            .optional()?;
        Ok(last_used.map(|last_used| TargetRow {
            path: PathBuf::from(path_str),
            last_used,
        }))
    }

    /// Remove the row for `path`. No-op if it doesn't exist.
    pub fn remove(&self, path: &Path) -> Result<bool, RegistryError> {
        let path_str = path_to_string(path);
        let removed = self.db.execute(
            "DELETE FROM target_registry_targets WHERE key = ?1",
            params![path_str],
        )?;
        Ok(removed > 0)
    }

    /// Remove rows for every path in `paths` in a single transaction,
    /// skipping missing rows. Returns the number actually removed.
    pub fn remove_many(&self, paths: &[PathBuf]) -> Result<usize, RegistryError> {
        if paths.is_empty() {
            return Ok(0);
        }
        let tx = self.db.unchecked_transaction()?;
        let mut removed = 0usize;
        for path in paths {
            let key = path_to_string(path);
            removed += tx.execute(
                "DELETE FROM target_registry_targets WHERE key = ?1",
                params![key],
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Total number of tracked rows.
    pub fn len(&self) -> Result<usize, RegistryError> {
        let count: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM target_registry_targets", [], |row| {
                    row.get(0)
                })?;
        Ok(count as usize)
    }

    /// Whether the registry has any rows.
    pub fn is_empty(&self) -> Result<bool, RegistryError> {
        Ok(self.len()? == 0)
    }
}

/// Human-readable byte size, kibibyte-based (e.g. `12.3 GB`).
pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

/// Human-readable age in days/hours from a delta of seconds.
pub fn human_age(seconds: i64) -> String {
    if seconds < 0 {
        return "future".to_string();
    }
    let secs = seconds as u64;
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    if days > 0 {
        format!("{days}d{hours}h")
    } else {
        let minutes = (secs % 3_600) / 60;
        format!("{hours}h{minutes}m")
    }
}

/// Bytes that deleting `path` would free, never crossing symlinks. A hardlinked
/// file counts once, and only when all its names are inside `path`
/// (soldr#3439); reflink sharing and Windows hardlinks are not detected, so
/// there the figure is an upper bound. Errors on individual entries are
/// swallowed -- partial sizes are still useful for the GC heuristic.
pub fn directory_size(path: &Path) -> u64 {
    crate::cache_lib::dir_footprint::measure(path).0
}

/// [`directory_size`] plus the walk's file count, as `(total_bytes, file_count)`.
pub fn directory_size_and_files(path: &Path) -> (u64, u64) {
    crate::cache_lib::dir_footprint::measure(path)
}

/// Resolve the workspace `target/` dir from an arbitrary path that
/// sits *inside* a `target/` tree. Walks up until it finds a path
/// component named `target`. Returns `None` if no such ancestor is
/// found cheaply — we never walk the whole filesystem.
pub fn resolve_target_dir_from_descendant(start: &Path) -> Option<PathBuf> {
    // Return the FIRST match, i.e. the nearest enclosing `target/`.
    //
    // This walk runs inner -> outer, and previously kept overwriting the
    // result on every match, so the value it returned was the OUTERMOST
    // `target/` ancestor. For a nested layout like
    // `.../target/repo/target/debug/deps/foo` that resolved to the outer
    // `.../target`, and GC would then register — and be willing to delete —
    // an enclosing repository tree instead of the workspace target
    // directory it was asked about (#1671).
    let mut current = start;
    while let Some(parent) = current.parent() {
        if parent.file_name().map(|n| n == "target").unwrap_or(false) {
            return Some(parent.to_path_buf());
        }
        current = parent;
    }
    None
}

/// Top-level markers cargo lays down inside a `target/` directory. Any one
/// is enough — which ones exist depends on what has been run.
///
/// Mirrors `soldr-cli`'s `gc::target_walker::CARGO_TARGET_MARKERS`. Maven
/// and other JVM tooling also use a `target/` directory, but ship
/// `target/classes/` or `target/maven-archiver/` rather than these, so the
/// check stays Rust-specific.
const CARGO_TARGET_MARKERS: &[&str] = &[
    "debug",
    "release",
    "doc",
    ".rustc_info.json",
    "CACHEDIR.TAG",
];

/// Whether `dir` looks like a cargo-produced `target/` tree.
///
/// Name-based resolution alone is not enough to hand a path to destructive
/// registry work: any directory called `target` on the way up matches, and
/// #1671 showed the walk could select an enclosing repository. Requiring a
/// cargo marker means a directory merely *named* `target` is not treated as
/// one. Returns `false` when the directory does not exist or cannot be
/// inspected — callers skip rather than guess.
pub fn looks_like_cargo_target(dir: &Path) -> bool {
    CARGO_TARGET_MARKERS.iter().any(|m| dir.join(m).exists())
}

/// Resolve the canonical workspace target directory from a
/// rustc-wrapper argv slice. Returns `None` if the path can't be
/// derived cheaply -- callers MUST silently skip in that case rather
/// than fail the build.
///
/// Strategy (see [`resolve_workspace_target_dir_with`]):
/// 1. Honor `CARGO_TARGET_DIR` if set; a relative value is resolved
///    against the current directory (soldr#3645).
/// 2. Otherwise walk up from rustc's `--out-dir`: nearest ancestor named
///    `target`, else the nearest ancestor carrying cargo's root-only
///    markers (`CACHEDIR.TAG` / `.rustc_info.json`), else the nearest
///    ancestor that merely looks like a cargo target.
pub fn resolve_workspace_target_dir(rustc_args: &[String]) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok();
    resolve_workspace_target_dir_with(
        rustc_args,
        std::env::var_os("CARGO_TARGET_DIR").as_deref(),
        cwd.as_deref(),
    )
}

/// Env-free core of [`resolve_workspace_target_dir`].
pub fn resolve_workspace_target_dir_with(
    rustc_args: &[String],
    env_target_dir: Option<&std::ffi::OsStr>,
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(env_dir) = env_target_dir.filter(|d| !d.is_empty()) {
        let path = PathBuf::from(env_dir);
        if path.is_absolute() {
            return Some(canonicalize_or_self(&path));
        }
        if let Some(cwd) = cwd {
            return Some(canonicalize_or_self(&cwd.join(path)));
        }
    }

    let out_dir = extract_flag_value(rustc_args, "--out-dir")?;
    let mut out_path = PathBuf::from(out_dir);
    if out_path.is_relative() {
        if let Some(cwd) = cwd {
            out_path = cwd.join(out_path);
        }
    }
    if let Some(target) = resolve_target_dir_from_descendant(&out_path) {
        return Some(canonicalize_or_self(&target));
    }
    let root_marked = out_path
        .ancestors()
        .skip(1)
        .find(|a| a.join("CACHEDIR.TAG").exists() || a.join(".rustc_info.json").exists());
    if let Some(a) = root_marked {
        return Some(canonicalize_or_self(a));
    }
    let a = out_path
        .ancestors()
        .skip(1)
        .find(|a| looks_like_cargo_target(a))?;
    Some(canonicalize_or_self(a))
}

fn extract_flag_value(args: &[String], flag: &str) -> Option<String> {
    let mut iter = args.iter();
    let prefix = format!("{flag}=");
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

fn canonicalize_or_self(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(_) => path.to_path_buf(),
    }
}

/// Registry key for `path`.
///
/// soldr#2700: normalize away the Windows verbatim prefix (`\\?\`) before
/// the key is formed. `std::fs::canonicalize` returns verbatim paths on
/// Windows while the cargo front door records the plain form, so the same
/// `target/` dir landed in the registry twice — once as `C:\...`, once as
/// `\\?\C:\...`. Two rows for one directory double-count its size in the
/// GC report and let a delete target a path form the other row still
/// claims is live.
///
/// Stripping is purely lexical and only applies to the drive-letter form
/// (`\\?\C:\...`). UNC verbatim paths (`\\?\UNC\server\share`) are left
/// alone: `\\?\UNC\` does not round-trip to `\\server\share` by prefix
/// removal alone, so rewriting them would produce a path that does not
/// resolve.
fn path_to_string(path: &Path) -> String {
    let raw = path.to_string_lossy().to_string();
    strip_verbatim_prefix(&raw).to_string()
}

/// Lexically remove a `\\?\` prefix from a drive-letter path. Returns the
/// input unchanged when the prefix is absent or the remainder is not a
/// `<letter>:` path (notably the `UNC\` form).
///
/// `pub(crate)`: also used by `cache_lib::save`'s mtime-replay path
/// (soldr#3289) to normalize a workspace root before it is joined
/// against manifest-relative POSIX paths.
pub(crate) fn strip_verbatim_prefix(raw: &str) -> &str {
    let Some(rest) = raw.strip_prefix(r"\\?\") else {
        return raw;
    };
    let mut chars = rest.chars();
    let is_drive_path = matches!(
        (chars.next(), chars.next()),
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic()
    );
    if is_drive_path {
        rest
    } else {
        raw
    }
}

/// Current unix timestamp in seconds.
pub fn current_unix_seconds() -> Result<i64, RegistryError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|e| RegistryError::Clock(e.to_string()))
}

// ---------------------------------------------------------------------------
// Safety guards (from docs/TARGET_GC_PROPOSAL.md)
// ---------------------------------------------------------------------------

/// Decision returned by [`evaluate_safety_guards`] for a single
/// candidate `target/` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardOutcome {
    /// Safe to consider for GC.
    Eligible,
    /// Skipped because of an active build lock or recent activity.
    Skipped(String),
}

/// Apply the proposal's three safety guards to a candidate `target/`
/// directory:
///
/// (a) `Cargo.lock` mtime within the staleness window — workspace was
///     built recently so the dir is likely live.
/// (b) `target/.cargo-lock` exists — cargo is currently holding the
///     build lock.
/// (c) `dev_root` allowlist — the `target/` path must sit under one
///     of the configured roots (defaults to `~/dev`).
pub fn evaluate_safety_guards(
    target_dir: &Path,
    workspace_root: &Path,
    dev_roots: &[PathBuf],
    stale_age_seconds: u64,
    now_unix_seconds: i64,
) -> GuardOutcome {
    let metadata = super::gc::evaluate_target_metadata_guards(
        target_dir,
        workspace_root,
        dev_roots,
        stale_age_seconds,
        now_unix_seconds,
    );
    if !matches!(metadata, GuardOutcome::Eligible) {
        return metadata;
    }

    // A registry row can outlive a target directory and be replaced by a
    // non-directory entry. Let the purge phase report that malformed row as a
    // deletion failure instead of silently hiding it behind a lock-probe I/O
    // error.
    if !target_dir.is_dir() {
        return GuardOutcome::Eligible;
    }

    match super::cargo_lock::probe(target_dir) {
        Ok(super::cargo_lock::CargoLockProbe::Idle(_guard)) => {}
        Ok(super::cargo_lock::CargoLockProbe::Active(lock)) => {
            return GuardOutcome::Skipped(format!(
                "active cargo build lock present at {}",
                lock.display()
            ));
        }
        Err(error) => {
            return GuardOutcome::Skipped(format!("cargo build lock probe failed closed: {error}"));
        }
    }

    GuardOutcome::Eligible
}

/// Whether `path` lies under any of the supplied `dev_roots` after
/// canonicalization. An empty `dev_roots` slice rejects everything.
pub fn is_under_any_root(path: &Path, dev_roots: &[PathBuf]) -> bool {
    if dev_roots.is_empty() {
        return false;
    }
    let canonical = canonicalize_or_self(path);
    for root in dev_roots {
        let root_canonical = canonicalize_or_self(root);
        if canonical.starts_with(&root_canonical) {
            return true;
        }
    }
    false
}

/// Walk up from a `target/` dir to its workspace root. The workspace
/// root is the parent directory of `target/` (the one containing
/// `Cargo.toml`).
pub fn workspace_root_for_target(target_dir: &Path) -> PathBuf {
    target_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| target_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt;
    use tempfile::tempdir;

    fn fixed_now() -> i64 {
        1_700_000_000
    }

    /// Create a directory symlink, or return false when the platform/session
    /// cannot make one. Windows needs Developer Mode or elevation, so the
    /// symlink tests self-skip rather than failing on an unprivileged box.
    fn try_symlink_dir(src: &std::path::Path, dst: &std::path::Path) -> bool {
        crate::platform::fs::links::create(&src.to_string_lossy(), dst, true).is_ok()
    }

    #[test]
    fn directory_size_does_not_follow_a_symlink_cycle() {
        // Before #1662 the per-entry check used `DirEntry::metadata`, which
        // follows the link, so `is_symlink()` never fired and this recursed
        // until the stack blew.
        let root = tempdir().expect("tempdir");
        let inner = root.path().join("inner");
        std::fs::create_dir_all(&inner).expect("mkdir");
        std::fs::write(inner.join("real.bin"), b"0123456789").expect("write");

        if !try_symlink_dir(root.path(), &inner.join("loop")) {
            eprintln!("skipping: cannot create directory symlinks here");
            return;
        }

        // The assertion is that this terminates at all; the size must also
        // count only the one real file, not the cycle's repeats.
        assert_eq!(directory_size(root.path()), 10);
    }

    #[test]
    fn directory_size_ignores_a_symlink_pointing_outside_the_tree() {
        let root = tempdir().expect("tempdir");
        let outside = tempdir().expect("tempdir");
        std::fs::write(outside.path().join("huge.bin"), vec![0u8; 4096]).expect("write");
        std::fs::write(root.path().join("small.bin"), b"abc").expect("write");

        if !try_symlink_dir(outside.path(), &root.path().join("escape")) {
            eprintln!("skipping: cannot create directory symlinks here");
            return;
        }

        // Only `small.bin`. Counting the linked-in tree would make an
        // unrelated directory look like it belonged to this target.
        assert_eq!(directory_size(root.path()), 3);
    }

    #[test]
    fn directory_size_and_files_is_symlink_safe() {
        let root = tempdir().expect("tempdir");
        let inner = root.path().join("inner");
        std::fs::create_dir_all(&inner).expect("mkdir");
        std::fs::write(inner.join("a.bin"), b"12345").expect("write");

        if !try_symlink_dir(root.path(), &inner.join("loop")) {
            eprintln!("skipping: cannot create directory symlinks here");
            return;
        }

        let (bytes, files) = directory_size_and_files(root.path());
        assert_eq!((bytes, files), (5, 1));
    }

    #[test]
    fn upsert_is_idempotent_and_updates_timestamp() {
        let registry = TargetRegistry::open_in_memory().unwrap();
        let path = PathBuf::from("/tmp/repo/target");
        registry.upsert_with_time(&path, 100).unwrap();
        registry.upsert_with_time(&path, 200).unwrap();
        registry.upsert_with_time(&path, 200).unwrap();

        let row = registry.get(&path).unwrap().unwrap();
        assert_eq!(row.last_used, 200);
        assert_eq!(registry.len().unwrap(), 1);
    }

    #[test]
    fn list_returns_all_rows_sorted() {
        let registry = TargetRegistry::open_in_memory().unwrap();
        registry
            .upsert_with_time(Path::new("/a/target"), 300)
            .unwrap();
        registry
            .upsert_with_time(Path::new("/b/target"), 100)
            .unwrap();
        registry
            .upsert_with_time(Path::new("/c/target"), 200)
            .unwrap();

        let rows = registry.list().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].path, PathBuf::from("/b/target"));
        assert_eq!(rows[1].path, PathBuf::from("/c/target"));
        assert_eq!(rows[2].path, PathBuf::from("/a/target"));
    }

    #[test]
    fn remove_deletes_row() {
        let registry = TargetRegistry::open_in_memory().unwrap();
        let path = PathBuf::from("/tmp/repo/target");
        registry.upsert_with_time(&path, 100).unwrap();
        assert!(registry.remove(&path).unwrap());
        assert!(!registry.remove(&path).unwrap());
        assert_eq!(registry.len().unwrap(), 0);
    }

    #[test]
    fn remove_many_batches_deletes_and_counts_hits() {
        let registry = TargetRegistry::open_in_memory().unwrap();
        let a = PathBuf::from("/tmp/a/target");
        let b = PathBuf::from("/tmp/b/target");
        let c = PathBuf::from("/tmp/c/target");
        let ghost = PathBuf::from("/tmp/ghost/target");
        registry.upsert_with_time(&a, 10).unwrap();
        registry.upsert_with_time(&b, 20).unwrap();
        registry.upsert_with_time(&c, 30).unwrap();

        let removed = registry
            .remove_many(&[a.clone(), ghost.clone(), b.clone()])
            .unwrap();
        assert_eq!(removed, 2);
        assert!(registry.get(&a).unwrap().is_none());
        assert!(registry.get(&b).unwrap().is_none());
        assert!(registry.get(&c).unwrap().is_some());
        assert!(registry.get(&ghost).unwrap().is_none());
    }

    #[test]
    fn remove_many_on_empty_input_is_a_noop() {
        let registry = TargetRegistry::open_in_memory().unwrap();
        let path = PathBuf::from("/tmp/keep/target");
        registry.upsert_with_time(&path, 5).unwrap();
        assert_eq!(registry.remove_many(&[]).unwrap(), 0);
        assert!(registry.get(&path).unwrap().is_some());
    }

    #[test]
    fn open_persists_to_disk() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("state.sqlite3");
        {
            let registry = TargetRegistry::open(&db_path).unwrap();
            registry
                .upsert_with_time(Path::new("/tmp/persisted/target"), 555)
                .unwrap();
        }
        let registry = TargetRegistry::open(&db_path).unwrap();
        let row = registry
            .get(Path::new("/tmp/persisted/target"))
            .unwrap()
            .unwrap();
        assert_eq!(row.last_used, 555);
    }

    #[test]
    fn open_in_memory_uses_isolated_redb_backend() {
        let first = TargetRegistry::open_in_memory().unwrap();
        let second = TargetRegistry::open_in_memory().unwrap();
        first
            .upsert_with_time(Path::new("/tmp/first/target"), 10)
            .unwrap();

        assert_eq!(first.len().unwrap(), 1);
        assert!(second.is_empty().unwrap());
    }

    #[test]
    fn human_size_formats_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(2u64 * 1024 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn human_age_formats_days_and_hours() {
        assert_eq!(human_age(0), "0h0m");
        assert_eq!(human_age(3600), "1h0m");
        assert_eq!(human_age(86_400), "1d0h");
        assert_eq!(human_age(86_400 * 3 + 3600 * 4), "3d4h");
    }

    #[test]
    fn directory_size_sums_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("b.bin"), vec![0u8; 1024]).unwrap();
        let nested = dir.path().join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("c.bin"), vec![0u8; 256]).unwrap();
        assert_eq!(directory_size(dir.path()), 5 + 1024 + 256);
    }

    #[test]
    fn resolve_target_dir_picks_the_nearest_of_nested_targets() {
        // #1671: the walk runs inner -> outer and used to keep overwriting,
        // so it returned the OUTERMOST match. GC would then treat the
        // enclosing repository as the target directory.
        let path = PathBuf::from("/home/user/target/repo/target/debug/deps/foo-abcdef");
        let target = resolve_target_dir_from_descendant(&path).unwrap();
        assert_eq!(target, PathBuf::from("/home/user/target/repo/target"));
    }

    #[test]
    fn resolve_target_dir_handles_three_levels_of_nesting() {
        let path = PathBuf::from("/t/target/a/target/b/target/debug/deps/x");
        let target = resolve_target_dir_from_descendant(&path).unwrap();
        assert_eq!(target, PathBuf::from("/t/target/a/target/b/target"));
    }

    #[test]
    fn with_relative_env_resolves_against_cwd() {
        let tmp = tempdir().unwrap();
        let deps = tmp.path().join("out/debug/deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(tmp.path().join("out/CACHEDIR.TAG"), "x").unwrap();
        let want = std::fs::canonicalize(tmp.path().join("out")).unwrap();
        let args = vec!["--out-dir".to_string(), deps.to_string_lossy().to_string()];
        let got = resolve_workspace_target_dir_with(
            &args,
            Some(std::ffi::OsStr::new("out")),
            Some(tmp.path()),
        );
        assert_eq!(got, Some(want.clone()));
        let rel = vec!["--out-dir".to_string(), "out/debug/deps".to_string()];
        let got = resolve_workspace_target_dir_with(&rel, None, Some(tmp.path()));
        assert_eq!(got, Some(want));
    }

    #[test]
    fn with_no_env_finds_marked_non_target_name() {
        let tmp = tempdir().unwrap();
        let deps = tmp.path().join("build/debug/deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(tmp.path().join("build/CACHEDIR.TAG"), "x").unwrap();
        let want = std::fs::canonicalize(tmp.path().join("build")).unwrap();
        let args = vec!["--out-dir".to_string(), deps.to_string_lossy().to_string()];
        assert_eq!(
            resolve_workspace_target_dir_with(&args, None, None),
            Some(want)
        );
    }

    #[test]
    fn with_no_markers_and_no_target_is_none() {
        let tmp = tempdir().unwrap();
        let d = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&d).unwrap();
        let args = vec!["--out-dir".to_string(), d.to_string_lossy().to_string()];
        assert_eq!(resolve_workspace_target_dir_with(&args, None, None), None);
    }

    #[test]
    fn with_target_named_dir_and_absolute_env() {
        let tmp = tempdir().unwrap();
        let deps = tmp.path().join("target/debug/deps");
        std::fs::create_dir_all(&deps).unwrap();
        let args = vec!["--out-dir".to_string(), deps.to_string_lossy().to_string()];
        let want = std::fs::canonicalize(tmp.path().join("target")).unwrap();
        assert_eq!(
            resolve_workspace_target_dir_with(&args, None, None),
            Some(want)
        );
        let abs = tmp.path().join("target");
        let env = abs.as_os_str();
        let other = tempdir().unwrap();
        let got = resolve_workspace_target_dir_with(&args, Some(env), Some(other.path()));
        assert_eq!(got, Some(std::fs::canonicalize(&abs).unwrap()));
    }

    #[test]
    fn looks_like_cargo_target_requires_a_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let impostor = dir.path().join("target");
        std::fs::create_dir_all(&impostor).expect("mkdir");
        assert!(
            !looks_like_cargo_target(&impostor),
            "a bare directory named `target` is not a cargo target"
        );

        std::fs::create_dir_all(impostor.join("debug")).expect("mkdir");
        assert!(
            looks_like_cargo_target(&impostor),
            "a profile directory is a cargo marker"
        );
    }

    #[test]
    fn resolve_target_dir_walks_up_to_target_boundary() {
        let path = PathBuf::from("/home/user/repo/target/debug/deps/foo-abcdef");
        let target = resolve_target_dir_from_descendant(&path).unwrap();
        assert_eq!(target, PathBuf::from("/home/user/repo/target"));
    }

    #[test]
    fn resolve_target_dir_returns_none_when_no_target_ancestor() {
        let path = PathBuf::from("/home/user/some/other/dir");
        assert!(resolve_target_dir_from_descendant(&path).is_none());
    }

    #[test]
    fn extract_flag_value_handles_space_and_equals_forms() {
        let args: Vec<String> = vec![
            "--crate-name".into(),
            "foo".into(),
            "--out-dir".into(),
            "/tmp/repo/target/debug/deps".into(),
        ];
        assert_eq!(
            extract_flag_value(&args, "--out-dir"),
            Some("/tmp/repo/target/debug/deps".into())
        );

        let args2: Vec<String> = vec!["--out-dir=/tmp/repo/target/debug/deps".into()];
        assert_eq!(
            extract_flag_value(&args2, "--out-dir"),
            Some("/tmp/repo/target/debug/deps".into())
        );
    }

    #[test]
    fn allowlist_guard_short_circuits_outside_dev_roots() {
        let dir = tempdir().unwrap();
        let target_dir = dir.path().join("repo").join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        let workspace = target_dir.parent().unwrap().to_path_buf();
        // No dev_roots configured => everything is rejected.
        let outcome = evaluate_safety_guards(
            &target_dir,
            &workspace,
            &[],
            DEFAULT_STALE_AGE_SECONDS,
            fixed_now(),
        );
        assert!(matches!(outcome, GuardOutcome::Skipped(_)));
    }

    #[test]
    fn allowlist_guard_passes_when_target_is_under_root() {
        let dir = tempdir().unwrap();
        let target_dir = dir.path().join("repo").join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        let workspace = target_dir.parent().unwrap().to_path_buf();

        let outcome = evaluate_safety_guards(
            &target_dir,
            &workspace,
            &[dir.path().to_path_buf()],
            DEFAULT_STALE_AGE_SECONDS,
            fixed_now(),
        );
        assert_eq!(outcome, GuardOutcome::Eligible);
    }

    #[test]
    fn cargo_build_lock_skips_candidate() {
        let dir = tempdir().unwrap();
        let target_dir = dir.path().join("repo").join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        let workspace = target_dir.parent().unwrap().to_path_buf();
        let cargo_lock = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(target_dir.join(".cargo-lock"))
            .unwrap();
        cargo_lock.try_lock_exclusive().unwrap();

        let outcome = evaluate_safety_guards(
            &target_dir,
            &workspace,
            &[dir.path().to_path_buf()],
            DEFAULT_STALE_AGE_SECONDS,
            fixed_now(),
        );
        match outcome {
            GuardOutcome::Skipped(reason) => assert!(reason.contains("cargo build lock")),
            _ => panic!("expected skip"),
        }
    }

    #[test]
    fn fresh_cargo_lock_skips_candidate() {
        let dir = tempdir().unwrap();
        let target_dir = dir.path().join("repo").join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        let workspace = target_dir.parent().unwrap().to_path_buf();
        // Cargo.lock that is mtime "now" — newer than the threshold.
        std::fs::write(workspace.join("Cargo.lock"), b"").unwrap();

        let outcome = evaluate_safety_guards(
            &target_dir,
            &workspace,
            &[dir.path().to_path_buf()],
            DEFAULT_STALE_AGE_SECONDS,
            current_unix_seconds().unwrap(),
        );
        match outcome {
            GuardOutcome::Skipped(reason) => assert!(reason.contains("Cargo.lock modified")),
            _ => panic!("expected skip"),
        }
    }

    #[test]
    fn workspace_root_is_target_parent() {
        let target = PathBuf::from("/home/me/repo/target");
        assert_eq!(
            workspace_root_for_target(&target),
            PathBuf::from("/home/me/repo")
        );
    }

    #[test]
    fn verbatim_and_plain_windows_paths_collapse_to_one_row() {
        // soldr#2700: `std::fs::canonicalize` hands back `\\?\C:\...` on
        // Windows while the cargo front door records `C:\...`. Before
        // normalization the registry held both, double-counting the
        // directory's size and letting a delete of one form leave the
        // other claiming the target is still live.
        let registry = TargetRegistry::open_in_memory().unwrap();
        registry
            .upsert_with_time(Path::new(r"C:\Users\dev\proj\target"), 100)
            .unwrap();
        registry
            .upsert_with_time(Path::new(r"\\?\C:\Users\dev\proj\target"), 200)
            .unwrap();

        let rows = registry.list().unwrap();
        assert_eq!(rows.len(), 1, "expected one row, got {rows:?}");
        assert_eq!(rows[0].path, PathBuf::from(r"C:\Users\dev\proj\target"));
        assert_eq!(rows[0].last_used, 200, "later touch should win");
    }

    #[test]
    fn verbatim_path_is_readable_and_removable_through_the_plain_key() {
        // The normalization has to apply to every accessor, not just
        // upsert -- otherwise a row written one way is unreachable the
        // other way and `dropped_missing` can never clear it.
        let registry = TargetRegistry::open_in_memory().unwrap();
        registry
            .upsert_with_time(Path::new(r"\\?\C:\Users\dev\proj\target"), 55)
            .unwrap();

        let found = registry
            .get(Path::new(r"C:\Users\dev\proj\target"))
            .unwrap()
            .expect("row should be reachable via the plain form");
        assert_eq!(found.last_used, 55);
        assert!(registry
            .remove(Path::new(r"\\?\C:\Users\dev\proj\target"))
            .unwrap());
        assert!(registry.list().unwrap().is_empty());
    }

    #[test]
    fn strip_verbatim_prefix_leaves_unc_and_plain_paths_alone() {
        // `\\?\UNC\server\share` does not become `\\server\share` by
        // dropping the prefix, so rewriting it would yield a path that
        // resolves to nothing. Leave it exactly as it came in.
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\server\share\target"),
            r"\\?\UNC\server\share\target"
        );
        assert_eq!(
            strip_verbatim_prefix(r"C:\plain\target"),
            r"C:\plain\target"
        );
        assert_eq!(
            strip_verbatim_prefix("/unix/style/target"),
            "/unix/style/target"
        );
        assert_eq!(strip_verbatim_prefix(r"\\?\D:\x"), r"D:\x");
    }
}
