//! Crash-safe guard around cargo-chef's IN-PLACE skeleton reconstruction
//! (zackees/soldr#3518).
//!
//! `soldr cook` and `soldr dylint cook` let `cargo chef cook` rewrite the
//! real checkout (crate roots stubbed to empty files, manifests normalized)
//! and put the originals back from an in-memory [`ProjectSourceSnapshot`]
//! afterwards. A SIGKILL/OOM/reaped CI step between the two left the real
//! sources truncated, and the next cook re-snapshotted the broken tree, which
//! made the damage permanent.
//!
//! [`CookSourceGuard::begin`] therefore writes the snapshot to a durable
//! on-disk journal (temp file, fsync, atomic rename, fsync dir) BEFORE any
//! mutation, and holds an exclusive lock on a sibling lock file for the
//! whole mutate/restore window. The journal is deleted only after the
//! originals are back on disk. Because the kernel drops the lock when its
//! owner dies, a journal whose lock can be taken belongs to a dead cook:
//! [`recover_stale_cook_journals`] (run by every cook entry point and by
//! the cargo front door before it does anything) restores it first, so a
//! broken tree is never built or re-snapshotted. Whatever is on disk at
//! recovery time that differs from the journal is copied to a
//! `recovered-*` backup directory before being replaced, so a recovery can
//! never lose an edit made after the crash either.

use crate::cook_source_snapshot::{
    restore_project_source, snapshot_project_source, walk_project_source, ProjectSourceSnapshot,
    JOURNAL_DIR_NAME,
};
use crate::core::SoldrError;
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const MAGIC: &[u8] = b"SOLDR-COOK-SOURCE-JOURNAL v1\n";
const TRAILER: &[u8] = b"\nEND\n";
const JOURNAL_EXT: &str = "journal";
const GIT_JOURNAL_DIR: &str = "soldr-cook-journal";

/// Owns the journal + lock for one in-place cook. Call [`Self::restore`] on
/// every normal path; `Drop` restores best-effort if a caller unwinds or
/// returns early, and a killed process is recovered by the next invocation.
pub(crate) struct CookSourceGuard {
    root: PathBuf,
    snapshot: ProjectSourceSnapshot,
    journal: PathBuf,
    lock: Option<File>,
}

impl CookSourceGuard {
    /// Recover any stale journal for `root`, snapshot its sources, and make
    /// the snapshot durable before returning. The caller may mutate the
    /// tree only after this returns `Ok`.
    pub(crate) fn begin(root: &Path) -> Result<Self, SoldrError> {
        let root = canonical(root);
        let dir = journal_dir_for(&root);
        std::fs::create_dir_all(&dir).map_err(|e| io_err("create journal dir", &dir, e))?;
        let (journal, lock_path) = journal_paths(&dir, &root);
        let lock = open_lock(&lock_path)?;
        crate::platform::wait::lock_exclusive_announced(
            &lock,
            crate::platform::wait::LONG_LOCK,
            "the cook source journal",
        )
        .map_err(|e| io_err("lock cook journal", &lock_path, e))?;
        // We hold the lock, so any journal here was left by a dead cook.
        if journal.exists() {
            recover_journal(&journal)?;
        }
        let snapshot = snapshot_project_source(&root)?;
        write_journal(&journal, &root, &snapshot)?;
        Ok(Self {
            root,
            snapshot,
            journal,
            lock: Some(lock),
        })
    }

    /// Put the originals back, then retire the journal and release the lock.
    pub(crate) fn restore(mut self) -> Result<(), SoldrError> {
        self.finish()
    }

    fn finish(&mut self) -> Result<(), SoldrError> {
        let Some(lock) = self.lock.take() else {
            return Ok(());
        };
        // On failure the journal stays (and the lock drops with `lock`), so
        // the next invocation retries the recovery.
        restore_project_source(&self.root, &self.snapshot)?;
        remove_journal(&self.journal)?;
        let _ = FileExt::unlock(&lock);
        Ok(())
    }

    /// Test hook: simulate the process being killed between mutation and
    /// restore (the journal stays on disk, the kernel would drop the lock).
    #[cfg(test)]
    pub(crate) fn abandon_for_test(mut self) {
        self.lock = None;
    }
}

impl Drop for CookSourceGuard {
    fn drop(&mut self) {
        if self.lock.is_some() {
            if let Err(error) = self.finish() {
                eprintln!("soldr cook: failed to restore workspace sources: {error}");
            }
        }
    }
}

/// Restore every stale cook journal visible from `start` (its ancestors'
/// `.soldr-cook-journal/` dirs and the enclosing git dir's journal dir).
/// Journals whose lock is held belong to a live cook and are skipped.
/// Returns how many journals were recovered. Errors (for example a corrupt
/// journal) must stop the caller: the tree may still be a skeleton.
pub(crate) fn recover_stale_cook_journals(start: &Path) -> Result<usize, SoldrError> {
    let mut recovered = 0;
    let mut current = Some(start);
    while let Some(dir) = current {
        let local = dir.join(JOURNAL_DIR_NAME);
        if local.is_dir() {
            recovered += recover_dir(&local)?;
        }
        if let Some(git_dir) = git_dir_at(dir) {
            let shared = git_dir.join(GIT_JOURNAL_DIR);
            if shared.is_dir() {
                recovered += recover_dir(&shared)?;
            }
            break;
        }
        current = dir.parent();
    }
    Ok(recovered)
}

fn recover_dir(dir: &Path) -> Result<usize, SoldrError> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(0);
    };
    let mut recovered = 0;
    for entry in entries.flatten() {
        let journal = entry.path();
        if journal.extension().and_then(|e| e.to_str()) != Some(JOURNAL_EXT) {
            continue;
        }
        let lock_path = journal.with_extension("lock");
        let lock = open_lock(&lock_path)?;
        if lock.try_lock_exclusive().is_err() {
            continue; // a live cook owns it
        }
        if journal.exists() {
            recover_journal(&journal)?;
            recovered += 1;
        }
        let _ = FileExt::unlock(&lock);
    }
    Ok(recovered)
}

/// Restore a dead cook's journal. Caller holds the journal's lock.
fn recover_journal(journal: &Path) -> Result<(), SoldrError> {
    let bytes = std::fs::read(journal).map_err(|e| io_err("read cook journal", journal, e))?;
    let (root, snapshot) = decode(&bytes).map_err(|why| {
        SoldrError::Other(format!(
            "soldr cook: refusing to continue: the source journal {} left by an interrupted cook is unreadable ({why}). \
The workspace may still contain cargo-chef skeleton files (empty crate roots, `0.0.1` manifests). \
Recover them with `git status` / `git checkout -- <paths>` (or from your editor/backups), then delete {} and retry. See zackees/soldr#3518.",
            journal.display(),
            journal.display()
        ))
    })?;
    let backup = backup_divergent_files(journal, &root, &snapshot)?;
    restore_project_source(&root, &snapshot)?;
    remove_journal(journal)?;
    eprintln!(
        "soldr cook: recovered {} source files under {} from a cook that was interrupted mid-skeleton (zackees/soldr#3518).",
        snapshot.len(),
        root.display()
    );
    if let Some(backup) = backup {
        eprintln!(
            "soldr cook: the replaced on-disk contents were saved under {}; delete it once you have checked nothing there is yours.",
            backup.display()
        );
    }
    Ok(())
}

/// Copy every file recovery is about to overwrite or delete into a fresh
/// `recovered-*` directory next to the journal. Returns it if non-empty.
fn backup_divergent_files(
    journal: &Path,
    root: &Path,
    snapshot: &ProjectSourceSnapshot,
) -> Result<Option<PathBuf>, SoldrError> {
    let wanted: std::collections::HashMap<&Path, &[u8]> = snapshot
        .files
        .iter()
        .map(|(rel, bytes)| (rel.as_path(), bytes.as_slice()))
        .collect();
    let mut divergent: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut mark = |abs: &Path, rel: PathBuf| {
        let same = wanted
            .get(rel.as_path())
            .is_some_and(|bytes| std::fs::read(abs).is_ok_and(|current| current == *bytes));
        if !same {
            divergent.push((abs.to_path_buf(), rel));
        }
    };
    walk_project_source(root, root, &mut mark)
        .map_err(|e| io_err("scan workspace for recovery", root, e))?;
    if divergent.is_empty() {
        return Ok(None);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let stem = journal
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = journal.with_file_name(format!("recovered-{stem}-{stamp}"));
    for (abs, rel) in divergent {
        let dest = dir.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err("create backup dir", parent, e))?;
        }
        std::fs::copy(&abs, &dest).map_err(|e| io_err("back up", &abs, e))?;
    }
    Ok(Some(dir))
}

/// Where `root`'s journal lives: the enclosing git dir (outside the work
/// tree, per worktree) or, for a non-git workspace, `root/.soldr-cook-journal`.
pub(crate) fn journal_dir_for(root: &Path) -> PathBuf {
    let mut current = Some(root);
    while let Some(dir) = current {
        if let Some(git_dir) = git_dir_at(dir) {
            return git_dir.join(GIT_JOURNAL_DIR);
        }
        current = dir.parent();
    }
    root.join(JOURNAL_DIR_NAME)
}

fn git_dir_at(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let git_dir = PathBuf::from(contents.trim().strip_prefix("gitdir:")?.trim());
    Some(if git_dir.is_absolute() {
        git_dir
    } else {
        dir.join(git_dir)
    })
}

fn journal_paths(dir: &Path, root: &Path) -> (PathBuf, PathBuf) {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let name: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    (
        dir.join(format!("{name}.{JOURNAL_EXT}")),
        dir.join(format!("{name}.lock")),
    )
}

fn canonical(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn open_lock(path: &Path) -> Result<File, SoldrError> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| io_err("open cook journal lock", path, e))
}

fn write_journal(
    journal: &Path,
    root: &Path,
    snapshot: &ProjectSourceSnapshot,
) -> Result<(), SoldrError> {
    let bytes = encode(root, snapshot)?;
    let tmp = journal.with_extension("journal.tmp");
    {
        let mut file = File::create(&tmp).map_err(|e| io_err("create cook journal", &tmp, e))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| io_err("write cook journal", &tmp, e))?;
    }
    std::fs::rename(&tmp, journal).map_err(|e| io_err("commit cook journal", journal, e))?;
    sync_parent(journal);
    Ok(())
}

fn remove_journal(journal: &Path) -> Result<(), SoldrError> {
    match std::fs::remove_file(journal) {
        Ok(()) => {
            sync_parent(journal);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("retire cook journal", journal, e)),
    }
}

/// Best-effort directory fsync so the rename/unlink is durable. Opening a
/// directory fails harmlessly where the OS does not support it.
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

fn io_err(what: &str, path: &Path, error: std::io::Error) -> SoldrError {
    SoldrError::Other(format!(
        "soldr cook: failed to {what} {}: {error}",
        path.display()
    ))
}

fn path_str(path: &Path) -> Result<&str, SoldrError> {
    path.to_str().ok_or_else(|| {
        SoldrError::Other(format!(
            "soldr cook: refusing to cook in place: non-UTF-8 path {} cannot be journaled (zackees/soldr#3518)",
            path.display()
        ))
    })
}

fn put(out: &mut Vec<u8>, chunk: &[u8]) {
    out.extend_from_slice(&(chunk.len() as u64).to_le_bytes());
    out.extend_from_slice(chunk);
}

/// `MAGIC | root | count | (rel, bytes)* | sha256(everything before) | TRAILER`.
pub(crate) fn encode(root: &Path, snapshot: &ProjectSourceSnapshot) -> Result<Vec<u8>, SoldrError> {
    let mut out = MAGIC.to_vec();
    put(&mut out, path_str(root)?.as_bytes());
    out.extend_from_slice(&(snapshot.files.len() as u64).to_le_bytes());
    for (rel, bytes) in &snapshot.files {
        put(&mut out, path_str(rel)?.as_bytes());
        put(&mut out, bytes);
    }
    let digest = Sha256::digest(&out);
    out.extend_from_slice(&digest);
    out.extend_from_slice(TRAILER);
    Ok(out)
}

pub(crate) fn decode(bytes: &[u8]) -> Result<(PathBuf, ProjectSourceSnapshot), &'static str> {
    let body_len = bytes
        .len()
        .checked_sub(32 + TRAILER.len())
        .ok_or("truncated")?;
    if !bytes.ends_with(TRAILER) || !bytes.starts_with(MAGIC) {
        return Err("bad header or trailer");
    }
    let (body, digest) = bytes.split_at(body_len);
    if Sha256::digest(body).as_slice() != &digest[..32] {
        return Err("checksum mismatch");
    }
    decode_body(&body[MAGIC.len()..])
}

fn utf8_path(bytes: &[u8]) -> Result<PathBuf, &'static str> {
    std::str::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|_| "non-UTF-8 path")
}

fn decode_body(mut cursor: &[u8]) -> Result<(PathBuf, ProjectSourceSnapshot), &'static str> {
    fn chunk<'a>(cursor: &mut &'a [u8]) -> Result<&'a [u8], &'static str> {
        let len = u64_at(cursor)?;
        if cursor.len() < len {
            return Err("truncated");
        }
        let (head, tail) = cursor.split_at(len);
        *cursor = tail;
        Ok(head)
    }
    fn u64_at(cursor: &mut &[u8]) -> Result<usize, &'static str> {
        if cursor.len() < 8 {
            return Err("truncated");
        }
        let (head, tail) = cursor.split_at(8);
        *cursor = tail;
        let value = u64::from_le_bytes(head.try_into().map_err(|_| "truncated")?);
        usize::try_from(value).map_err(|_| "oversized length")
    }
    let root = utf8_path(chunk(&mut cursor)?)?;
    let count = u64_at(&mut cursor)?;
    let mut files = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        let rel = utf8_path(chunk(&mut cursor)?)?;
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("path escapes the workspace");
        }
        files.push((rel, chunk(&mut cursor)?.to_vec()));
    }
    if !cursor.is_empty() {
        return Err("trailing bytes");
    }
    Ok((root, ProjectSourceSnapshot { files }))
}

#[cfg(test)]
#[path = "cook_source_journal_tests.rs"]
mod tests;
