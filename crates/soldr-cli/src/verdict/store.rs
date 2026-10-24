//! Local verdict store (soldr#3894 §5).
//!
//! ```text
//! <soldr root>/verdicts/        (or $SOLDR_VERDICT_DIR)
//!   records/<key>.json          verdicts this machine may replay
//!   spool/<key>.json            verdicts recorded this session, for an
//!                               uploader (setup-soldr's post step) to publish
//!   seen.jsonl                  every gated invocation, hit or miss, so an
//!                               uploader can predict the next run's keys
//! ```
//!
//! The store never talks to a network. Remote tiers restore records into
//! `records/` before checks run and publish `spool/` after; trust policy
//! (who may publish) belongs to them.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::record::Record;

/// Records kept by `gc`, newest first.
pub const DEFAULT_MAX_RECORDS: usize = 512;

/// `seen.jsonl` restarts beyond this size.
pub const MAX_SEEN_BYTES: u64 = 1024 * 1024;

pub const VERDICT_DIR_ENV: &str = "SOLDR_VERDICT_DIR";

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

/// One gated invocation, appended to `seen.jsonl`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    /// The adapter, or empty for an ungated front-door run (a build, a
    /// cook, a check that could not be keyed): work a remote tier must not
    /// predict away, because it needs the toolchain and build caches.
    pub tool: String,
    /// The original (un-normalized) cargo argv, so a re-key can replay the
    /// exact invocation.
    pub args: Vec<String>,
    pub cwd: String,
    pub key: String,
    pub hit: bool,
}

impl Store {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `$SOLDR_VERDICT_DIR`, else `<soldr root>/verdicts`.
    pub fn default_location() -> Option<Self> {
        if let Some(dir) = std::env::var_os(VERDICT_DIR_ENV).filter(|v| !v.is_empty()) {
            return Some(Self::at(dir));
        }
        crate::core::SoldrPaths::new()
            .ok()
            .map(|paths| Self::at(paths.root.join("verdicts")))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn records_dir(&self) -> PathBuf {
        self.root.join("records")
    }

    pub fn spool_dir(&self) -> PathBuf {
        self.root.join("spool")
    }

    pub fn seen_path(&self) -> PathBuf {
        self.root.join("seen.jsonl")
    }

    fn record_path(&self, key: &str) -> PathBuf {
        self.records_dir().join(format!("{key}.json"))
    }

    /// A valid record for `key`, if one is stored.
    pub fn lookup(&self, key: &str) -> Option<Record> {
        let text = std::fs::read_to_string(self.record_path(key)).ok()?;
        Record::parse_for(&text, key)
    }

    /// Store a record and spool it for upload. Atomic per file.
    pub fn put(&self, record: &Record) -> std::io::Result<()> {
        let text = serde_json::to_string(record)?;
        write_atomic(&self.record_path(&record.key), text.as_bytes())?;
        write_atomic(
            &self.spool_dir().join(format!("{}.json", record.key)),
            text.as_bytes(),
        )
    }

    /// Append one line to `seen.jsonl`. The log is a per-job prediction
    /// input, not history: past [`MAX_SEEN_BYTES`] it starts over.
    pub fn note_seen(&self, seen: &Seen) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.seen_path();
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_SEEN_BYTES) {
            std::fs::remove_file(&path)?;
        }
        let mut line = serde_json::to_string(seen)?;
        line.push('\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(line.as_bytes())
    }

    /// Every stored record, newest first.
    pub fn list(&self) -> Vec<(PathBuf, Record)> {
        let Ok(entries) = std::fs::read_dir(self.records_dir()) else {
            return Vec::new();
        };
        let mut found: Vec<(std::time::SystemTime, PathBuf, Record)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let key = path.file_stem()?.to_str()?.to_string();
                let text = std::fs::read_to_string(&path).ok()?;
                let record = Record::parse_for(&text, &key)?;
                let mtime = entry.metadata().ok()?.modified().ok()?;
                Some((mtime, path, record))
            })
            .collect();
        found.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        found.into_iter().map(|(_, p, r)| (p, r)).collect()
    }

    /// Keep the newest `max` records; drop the rest and any unreadable
    /// record file. Returns the number removed.
    pub fn gc(&self, max: usize) -> usize {
        let keep: std::collections::HashSet<PathBuf> =
            self.list().into_iter().take(max).map(|(p, _)| p).collect();
        let Ok(entries) = std::fs::read_dir(self.records_dir()) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !keep.contains(&path) && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("record path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.persist(path).map_err(|err| err.error)?;
    Ok(())
}
