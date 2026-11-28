//! Persistent affinity and accounting for the trusted remote runner.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::manifest::Manifest;
use super::wire::Start;

const GIB: u64 = 1024 * 1024 * 1024;
const MINIMUM_FREE: u64 = 5 * GIB;
const BYTES_PER_AUTO_SLOT: u64 = 32 * GIB;

pub(super) struct Lease {
    db: Connection,
    session: String,
    pub(super) slot: i64,
    pub(super) workspace: PathBuf,
}

impl Lease {
    /// The caller holds the runner's OS build lock for this lease's lifetime.
    pub(super) fn acquire(
        root: &Path,
        id: &str,
        manifest: &Manifest,
        limit: Option<u32>,
    ) -> io::Result<Self> {
        let (limit, reserve) = match limit {
            Some(limit) => (limit, MINIMUM_FREE),
            None => {
                let total = kernal_api::platform::resources::total_space(root)?;
                (auto_slot_limit(total), disk_reserve(total))
            }
        };
        Self::acquire_with_space_check(root, id, manifest, limit, || reserve_space(root, reserve))
    }

    fn acquire_with_space_check(
        root: &Path,
        id: &str,
        manifest: &Manifest,
        limit: u32,
        check_space: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<Self> {
        let mut db = Connection::open(root.join("slots.sqlite3")).map_err(io::Error::other)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS slots (
               id INTEGER PRIMARY KEY, user_id TEXT NOT NULL, repository TEXT NOT NULL,
               generation TEXT NOT NULL, last_use INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS builds (
               session TEXT PRIMARY KEY, slot_id INTEGER NOT NULL, user_id TEXT NOT NULL,
               repository TEXT NOT NULL, started INTEGER NOT NULL, finished INTEGER,
               exit_code INTEGER
             );",
        )
        .map_err(io::Error::other)?;
        let user = manifest.user();
        let repo = manifest.repository();
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(io::Error::other)?;
        // An unfinished row can only belong to an interrupted runner. Leave its
        // generation on disk so an orphaned child cannot write into a reclaimed slot.
        tx.execute_batch(
            "DELETE FROM slots WHERE generation IN (
               SELECT slots.generation FROM slots JOIN builds ON builds.slot_id = slots.id
               WHERE builds.finished IS NULL
             );
             UPDATE builds SET finished = unixepoch() WHERE finished IS NULL;",
        )
        .map_err(io::Error::other)?;
        tx.execute("DELETE FROM slots WHERE id > ?1", [limit])
            .map_err(io::Error::other)?;
        let affinity: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, generation FROM slots WHERE user_id = ?1 AND repository = ?2",
                params![user, repo],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(io::Error::other)?;
        let (slot, generation) = if let Some(affinity) = affinity {
            affinity
        } else {
            check_space()?;
            (
                choose_slot(&tx, limit)?,
                create_generation(&tx, root, manifest)?,
            )
        };
        tx.execute(
            "INSERT INTO slots VALUES (?1, ?2, ?3, ?4,
               (SELECT COALESCE(MAX(last_use), 0) + 1 FROM slots))
             ON CONFLICT (id) DO UPDATE SET user_id = excluded.user_id,
               repository = excluded.repository, generation = excluded.generation,
               last_use = excluded.last_use",
            params![slot, user, repo, generation],
        )
        .map_err(io::Error::other)?;
        tx.execute(
            "INSERT INTO builds VALUES (?1, ?2, ?3, ?4, unixepoch(), NULL, NULL)",
            params![id, slot, user, repo],
        )
        .map_err(io::Error::other)?;
        tx.commit().map_err(io::Error::other)?;
        let lease = Self {
            db,
            session: id.to_owned(),
            slot,
            workspace: if generation.contains('/') {
                root.join(generation)
            } else {
                root.join("slots").join(generation)
            },
        };
        fs::create_dir_all(&lease.workspace)?;
        Ok(lease)
    }

    pub(super) fn finish(&self, exit_code: Option<i32>) -> io::Result<()> {
        let tx = self.db.unchecked_transaction().map_err(io::Error::other)?;
        let updated = tx
            .execute(
                "UPDATE builds SET finished = unixepoch(), exit_code = ?1
                 WHERE session = ?2 AND finished IS NULL",
                params![exit_code, self.session],
            )
            .map_err(io::Error::other)?;
        if updated != 0 && exit_code.is_none() {
            tx.execute("DELETE FROM slots WHERE id = ?1", [self.slot])
                .map_err(io::Error::other)?;
        }
        tx.commit().map_err(io::Error::other)
    }
}

fn create_generation(db: &Connection, root: &Path, manifest: &Manifest) -> io::Result<String> {
    let prefix = format!("github/repo/{}", manifest.repository_slug());
    fs::create_dir_all(root.join(&prefix))?;
    let branch = manifest.branch_component();
    let commit = manifest.commit();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let seconds = i64::try_from(now.as_secs()).map_err(io::Error::other)?;
    let date: String = db
        .query_row(
            "SELECT strftime('%Y%m%dT%H%M%S', ?1, 'unixepoch')",
            [seconds],
            |row| row.get(0),
        )
        .map_err(io::Error::other)?;
    for fraction in now.subsec_nanos()..1_000_000_000 {
        let generation = format!("{prefix}/{date}.{fraction:09}Z_{branch}_{commit}");
        match fs::create_dir(root.join(&generation)) {
            Ok(()) => return Ok(generation),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other(
        "remote slot allocation timestamp is exhausted",
    ))
}

fn reserve_space(root: &Path, reserve: u64) -> io::Result<()> {
    // Reflinks make the seed cheap, but each compile can dirty private blocks.
    let available = kernal_api::platform::resources::available_space(root)?;
    if available < reserve {
        return Err(io::Error::other(format!(
            "remote slot allocation needs {reserve} bytes of free disk headroom; {available} available"
        )));
    }
    Ok(())
}

fn disk_reserve(total: u64) -> u64 {
    (total / 20).max(MINIMUM_FREE)
}

fn auto_slot_limit(total: u64) -> u32 {
    (total / BYTES_PER_AUTO_SLOT).clamp(1, 1024) as u32
}

fn choose_slot(tx: &rusqlite::Transaction<'_>, limit: u32) -> io::Result<i64> {
    let vacant = tx
        .query_row(
            "WITH RECURSIVE numbers(id) AS (
               SELECT 1 UNION ALL SELECT id+1 FROM numbers WHERE id < ?1
             )
             SELECT id FROM numbers WHERE id NOT IN (SELECT id FROM slots) LIMIT 1",
            [limit],
            |row| row.get(0),
        )
        .optional()
        .map_err(io::Error::other)?;
    match vacant {
        Some(slot) => Ok(slot),
        None => tx
            .query_row(
                "SELECT id FROM slots ORDER BY last_use, id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .map_err(io::Error::other),
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Err(error) = self.finish(None) {
            eprintln!(
                "soldr remote: failed to close slot session {}: {error}",
                self.session
            );
        }
    }
}

type FileStamp = (u64, SystemTime, Option<SystemTime>);

fn output_files(source: &Path) -> io::Result<BTreeMap<PathBuf, FileStamp>> {
    let mut files = BTreeMap::new();
    for entry in jwalk::WalkDir::new(source).min_depth(1).skip_hidden(false) {
        let entry = entry.map_err(io::Error::other)?;
        if entry.file_type().is_file() {
            let path = entry.path();
            let metadata = fs::metadata(&path)?;
            let relative = path.strip_prefix(source).map_err(io::Error::other)?;
            files.insert(
                relative.to_owned(),
                (
                    metadata.len(),
                    metadata.modified()?,
                    metadata.created().ok(),
                ),
            );
        }
    }
    Ok(files)
}

pub(super) struct OutputChanges {
    source: PathBuf,
    marker: PathBuf,
    before: BTreeMap<PathBuf, FileStamp>,
    previous: BTreeMap<PathBuf, FileStamp>,
}

impl OutputChanges {
    pub(super) fn capture(workspace: &Path, request: &Start) -> io::Result<Self> {
        let inputs = serde_json::to_vec(&(&request.commit, request.action, &request.args))?;
        let invocation = super::snapshot::digest(&inputs);
        let source = workspace.join("output");
        let marker = workspace.join("published-outputs").join(invocation);
        let previous = match fs::read(&marker) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error),
        };
        let before = output_files(&source)?;
        Ok(Self {
            source,
            marker,
            before,
            previous,
        })
    }

    pub(super) fn publish(self, output: &Path, succeeded: bool) -> io::Result<()> {
        let mut published = BTreeMap::new();
        if succeeded {
            for (relative, stamp) in output_files(&self.source)? {
                let changed = self.before.get(&relative) != Some(&stamp);
                let replay = self.previous.get(&relative) == Some(&stamp);
                if changed || replay {
                    let source = self.source.join(&relative);
                    let target = output.join(&relative);
                    fs::create_dir_all(super::server::parent(&target)?)?;
                    reflink(&source, &target)?;
                    fs::set_permissions(&target, fs::metadata(&source)?.permissions())?;
                    published.insert(relative, stamp);
                }
            }
        }
        let parent = super::server::parent(&self.marker)?;
        fs::create_dir_all(parent)?;
        let staging = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(staging.as_file(), &published)?;
        staging.persist(self.marker)?;
        Ok(())
    }
}

pub(super) fn reflink(source: &Path, destination: &Path) -> io::Result<()> {
    crate::platform::fs::reflink::reflink_file(source, destination).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "remote slots require a reflink-capable runner volume: {}: {error}",
                destination.display()
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_build::manifest;

    fn claim(root: &Path, session: &str, user: u32, limit: u32) -> Lease {
        let commit = "a".repeat(40);
        let bytes = serde_json::to_vec(&serde_json::json!({
            "github": {"url": "https://github.com/forge-ci/soldr"},
            "user_id": user,
            "parent-commit-hash": null,
            "parent-branch": "release/stable",
            "last-commit-hash": commit,
        }))
        .unwrap();
        let manifest = manifest::decode(Some(&bytes), &commit).unwrap().unwrap();
        Lease::acquire_with_space_check(root, session, &manifest, limit, || Ok(())).unwrap()
    }

    #[test]
    fn returning_user_reuses_generation_and_evicted_user_gets_clean_slot() {
        let root = tempfile::tempdir().unwrap();
        let original = claim(root.path(), "first", 1234, 4);
        let workspace = original.workspace.clone();
        assert_eq!(
            workspace.parent().unwrap(),
            root.path().join("github/repo/forge-ci/soldr")
        );
        let name = workspace.file_name().unwrap().to_str().unwrap();
        let (date, suffix) = name.split_once('_').unwrap();
        assert_eq!(date.len(), 26);
        assert!(date.ends_with('Z'));
        assert_eq!(suffix, format!("release%2Fstable_{}", "a".repeat(40)));
        let slot = original.slot;
        fs::write(workspace.join("marker"), "previous user data").unwrap();
        original.finish(Some(0)).unwrap();
        drop(original);

        let reused = claim(root.path(), "second", 1234, 4);
        assert_eq!(reused.workspace, workspace);
        reused.finish(Some(0)).unwrap();
        drop(reused);

        for user in 0..3 {
            claim(root.path(), &user.to_string(), user, 4)
                .finish(Some(0))
                .unwrap();
        }
        let reassigned = claim(root.path(), "new", 4321, 4);
        assert_eq!(reassigned.slot, slot);
        assert_ne!(reassigned.workspace, workspace);
        assert!(!reassigned.workspace.join("marker").exists());
    }

    #[test]
    fn disk_reserve_scales_with_volume_capacity() {
        assert_eq!(disk_reserve(64 * GIB), 5 * GIB);
        assert_eq!(disk_reserve(2 * 1024 * GIB), 2 * 1024 * GIB / 20);
        assert_eq!(auto_slot_limit(8 * GIB), 1);
        assert_eq!(auto_slot_limit(2 * 1024 * GIB), 64);
        assert_eq!(auto_slot_limit(u64::MAX), 1024);
    }

    #[test]
    fn auto_policy_allocates_past_the_old_fixed_pool_size() {
        let root = tempfile::tempdir().unwrap();
        let limit = auto_slot_limit(256 * GIB);
        for user in 1..=6 {
            let lease = claim(root.path(), &user.to_string(), user, limit);
            assert_eq!(lease.slot, i64::from(user));
            lease.finish(Some(0)).unwrap();
        }
    }
}
