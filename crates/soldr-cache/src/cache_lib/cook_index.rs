//! Cross-repo shared `soldr cook` artifact index (issue #576, meta
//! #579). This module owns the `cook_index_v2` table inside the shared
//! `~/.soldr/state.sqlite3` and gives the daemon a per-request-open
//! surface to look up, record, touch, and stat cook artifacts. Heavy
//! I/O (tar+zstd packing) lives in the PR 2 `soldr cook` worker;
//! this module never reads or writes the artifact bytes themselves —
//! it only tracks where they are and how big they are.
//!
//! Key tuple (prost-encoded blob, used as the table key):
//!
//! ```text
//! (recipe_hash: [u8; 32], target_triple, profile, channel, rustc_version)
//! ```
//!
//! Cross-target sharing is structurally impossible by including
//! triple/profile/channel/rustc in the key — there is no code path
//! that can relax this without changing the schema.
//!
//! ## Storage (post-#603)
//!
//! `cook_index_v2` is the only live table. Keys are bare prost-encoded
//! `WireCookKey` bytes; values are `[0x01][prost-encoded WireCookEntry]`
//! tagged-byte rows — the row encoding survived the redb→SQLite engine
//! swap verbatim (the legacy redb *file* is deleted whole on first
//! SQLite open; orphaned cook artifacts under `~/.soldr/cache/cook/`
//! re-index on the next `soldr cook`).
//!
//! A breaking schema change MUST land as `cook_index_v3` rather than
//! mutating v2 in place. Additive protobuf tags are forward-compatible;
//! their zero defaults deliberately mean "observation unavailable" to the
//! cook cost gate.

use crate::cache_lib::state_store::{open_state_db, StateDbHandle};
use crate::cache_lib::target_registry::RegistryError;
use crate::core::wire::{prost_tagged_bytes, proto, WireDecodeError, REDB_TAG_PROST};
use prost::Message;
use rusqlite::{params, OptionalExtension};
use std::path::Path;

/// Lookup key for the cook artifact index. Cross-target sharing is
/// forbidden by including triple/profile/channel/rustc in the key —
/// two builds that differ on any of these resolve to different rows
/// regardless of recipe_hash.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CookKey {
    pub recipe_hash: [u8; 32],
    pub target_triple: String,
    pub profile: String,
    pub channel: String,
    pub rustc_version: String,
}

/// Stored value for a cook artifact entry. PR 2 writes these via
/// `CookRecord`; PR 3 reads them via `CookLookup` + `CookTouch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookEntry {
    pub sha256: [u8; 32],
    pub size_bytes: u64,
    pub created_unix_ms: u64,
    pub last_used_unix_ms: u64,
    /// Best-effort, normalized git remote URL. None when the source
    /// workspace had no `.git/` at cook time or `git config
    /// remote.origin.url` was unset.
    pub origin_url_normalized: Option<String>,
    /// Human-readable cook command summary stored for diagnostics
    /// (e.g. shown by `soldr cache stats`).
    pub cook_cmd_summary: String,
    /// Best-effort local branch name recorded at cook time. Used only
    /// to rank same-origin fallback hydration candidates.
    pub branch_name: Option<String>,
    /// Wall time of the compile that produced this artifact. A zero value is
    /// an old row and deliberately cannot pass the restore cost gate.
    pub compile_duration_ms: u64,
    /// Observed archive-save wall time for this exact cook key.
    pub save_elapsed_ms: u64,
}

fn prost_decode_err(e: prost::DecodeError) -> RegistryError {
    RegistryError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn wire_err(e: WireDecodeError) -> RegistryError {
    RegistryError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Open the shared state store. The `cook_index_v2` table (with every
/// other table in the store) is created by the open itself, so there is
/// no per-module init and no legacy-table migration: a legacy redb file
/// is deleted whole by `state_store` on first open.
fn open_db(path: &Path) -> Result<StateDbHandle, RegistryError> {
    Ok(open_state_db(path)?)
}

/// Idempotent: open the state store, which ensures `cook_index_v2`
/// exists. Kept as the daemon-startup entry point.
pub fn ensure_initialized(db_path: &Path) -> Result<(), RegistryError> {
    let _db = open_db(db_path)?;
    Ok(())
}

// =========================================================================
// Encoding helpers
// =========================================================================

fn cook_key_to_wire(key: &CookKey) -> proto::WireCookKey {
    proto::WireCookKey {
        recipe_hash: key.recipe_hash.to_vec(),
        target_triple: key.target_triple.clone(),
        profile: key.profile.clone(),
        channel: key.channel.clone(),
        rustc_version: key.rustc_version.clone(),
    }
}

fn cook_key_from_wire(wire: proto::WireCookKey) -> Result<CookKey, WireDecodeError> {
    if wire.recipe_hash.len() != 32 {
        return Err(WireDecodeError::InvalidShaLength(wire.recipe_hash.len()));
    }
    let mut recipe_hash = [0u8; 32];
    recipe_hash.copy_from_slice(&wire.recipe_hash);
    Ok(CookKey {
        recipe_hash,
        target_triple: wire.target_triple,
        profile: wire.profile,
        channel: wire.channel,
        rustc_version: wire.rustc_version,
    })
}

fn cook_entry_to_wire(entry: &CookEntry) -> proto::WireCookEntry {
    proto::WireCookEntry {
        sha256: entry.sha256.to_vec(),
        size_bytes: entry.size_bytes,
        created_unix_ms: entry.created_unix_ms,
        last_used_unix_ms: entry.last_used_unix_ms,
        origin_url_normalized: entry.origin_url_normalized.clone(),
        cook_cmd_summary: entry.cook_cmd_summary.clone(),
        branch_name: entry.branch_name.clone(),
        compile_duration_ms: entry.compile_duration_ms,
        save_elapsed_ms: entry.save_elapsed_ms,
    }
}

fn cook_entry_from_wire(wire: proto::WireCookEntry) -> Result<CookEntry, WireDecodeError> {
    if wire.sha256.len() != 32 {
        return Err(WireDecodeError::InvalidShaLength(wire.sha256.len()));
    }
    let mut sha256 = [0u8; 32];
    sha256.copy_from_slice(&wire.sha256);
    Ok(CookEntry {
        sha256,
        size_bytes: wire.size_bytes,
        created_unix_ms: wire.created_unix_ms,
        last_used_unix_ms: wire.last_used_unix_ms,
        origin_url_normalized: wire.origin_url_normalized,
        cook_cmd_summary: wire.cook_cmd_summary,
        branch_name: wire.branch_name,
        compile_duration_ms: wire.compile_duration_ms,
        save_elapsed_ms: wire.save_elapsed_ms,
    })
}

fn encode_key(key: &CookKey) -> Vec<u8> {
    let wire = cook_key_to_wire(key);
    let mut out = Vec::with_capacity(wire.encoded_len());
    wire.encode(&mut out).expect("Vec write is infallible");
    out
}

fn decode_key(bytes: &[u8]) -> Result<CookKey, RegistryError> {
    let wire = proto::WireCookKey::decode(bytes).map_err(prost_decode_err)?;
    cook_key_from_wire(wire).map_err(wire_err)
}

/// Strict prost-only decode of a `cook_index_v2` value. Values that
/// don't carry the `0x01` tag (impossible under v2 by construction)
/// surface an `InvalidData` error.
fn decode_entry(bytes: &[u8]) -> Result<CookEntry, RegistryError> {
    let rest = match bytes.split_first() {
        Some((&REDB_TAG_PROST, rest)) => rest,
        _ => {
            return Err(RegistryError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "cook_index_v2 row missing prost tag byte",
            )))
        }
    };
    let wire = proto::WireCookEntry::decode(rest).map_err(prost_decode_err)?;
    cook_entry_from_wire(wire).map_err(wire_err)
}

// =========================================================================
// Public surface
// =========================================================================

/// One raw `(key, value)` pair from `cook_index_v2`.
type RawRow = (Vec<u8>, Vec<u8>);

/// Every `(key, value)` row in `cook_index_v2`, raw. The shared scan
/// loop for the fallback/drift/eviction passes below.
fn raw_rows(db: &StateDbHandle) -> Result<Vec<RawRow>, RegistryError> {
    let mut statement = db.prepare("SELECT key, value FROM cook_index_v2")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Upsert a cook artifact entry.
pub fn upsert(db_path: &Path, key: &CookKey, entry: &CookEntry) -> Result<(), RegistryError> {
    let db = open_db(db_path)?;
    let key_bytes = encode_key(key);
    let value_bytes = prost_tagged_bytes(&cook_entry_to_wire(entry));
    db.execute(
        "INSERT INTO cook_index_v2(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key_bytes, value_bytes],
    )?;
    Ok(())
}

/// Look up a single entry by the full key tuple.
pub fn lookup(db_path: &Path, key: &CookKey) -> Result<Option<CookEntry>, RegistryError> {
    let db = open_db(db_path)?;
    let key_bytes = encode_key(key);
    let row: Option<Vec<u8>> = db
        .query_row(
            "SELECT value FROM cook_index_v2 WHERE key = ?1",
            params![key_bytes],
            |row| row.get(0),
        )
        .optional()?;
    let Some(bytes) = row else {
        return Ok(None);
    };
    Ok(Some(decode_entry(&bytes)?))
}

/// Best-effort same-origin fallback used when the exact recipe hash
/// misses. Returns the highest-ranked compatible row and the key it
/// was stored under.
pub fn lookup_origin_fallback(
    db_path: &Path,
    miss_key: &CookKey,
    origin_url_normalized: Option<&str>,
    branch_lineage: &[String],
) -> Result<Option<(CookKey, CookEntry)>, RegistryError> {
    let Some(origin) = origin_url_normalized else {
        return Ok(None);
    };
    let db = open_db(db_path)?;
    let mut best: Option<(usize, u64, CookKey, CookEntry)> = None;
    for (k_bytes, v_bytes) in raw_rows(&db)? {
        let Ok(stored_key) = decode_key(&k_bytes) else {
            continue;
        };
        if !key_matches_origin_matrix(&stored_key, miss_key) {
            continue;
        }
        let Ok(stored_entry) = decode_entry(&v_bytes) else {
            continue;
        };
        if stored_entry.origin_url_normalized.as_deref() != Some(origin) {
            continue;
        }
        let Some(rank) = fallback_branch_rank(stored_entry.branch_name.as_deref(), branch_lineage)
        else {
            continue;
        };
        let used = stored_entry.last_used_unix_ms;
        let replace = match &best {
            None => true,
            Some((best_rank, best_used, _, _)) => {
                rank < *best_rank || (rank == *best_rank && used > *best_used)
            }
        };
        if replace {
            best = Some((rank, used, stored_key, stored_entry));
        }
    }
    Ok(best.map(|(_, _, key, entry)| (key, entry)))
}

fn fallback_branch_rank(branch_name: Option<&str>, branch_lineage: &[String]) -> Option<usize> {
    if branch_lineage.is_empty() {
        return Some(0);
    }
    match branch_name {
        Some(branch) => branch_lineage
            .iter()
            .position(|candidate| candidate == branch),
        None => Some(usize::MAX),
    }
}

/// Drift diagnostic: given a (missing) lookup key, scan v2 for other
/// rows whose `(origin_url_normalized, target_triple, profile,
/// channel, rustc_version)` match — but whose `recipe_hash` differs.
/// Returns up to `limit` previous recipe hashes, newest-first.
pub fn drift_recipe_hashes(
    db_path: &Path,
    miss_key: &CookKey,
    origin_url_normalized: Option<&str>,
    limit: usize,
) -> Result<Vec<[u8; 32]>, RegistryError> {
    let Some(origin) = origin_url_normalized else {
        return Ok(Vec::new());
    };
    let db = open_db(db_path)?;
    let mut out: Vec<([u8; 32], u64)> = Vec::new();
    for (k_bytes, v_bytes) in raw_rows(&db)? {
        let Ok(stored_key) = decode_key(&k_bytes) else {
            continue;
        };
        if !key_matches_origin_matrix(&stored_key, miss_key) {
            continue;
        }
        let Ok(stored_entry) = decode_entry(&v_bytes) else {
            continue;
        };
        if stored_entry.origin_url_normalized.as_deref() != Some(origin) {
            continue;
        }
        out.push((stored_key.recipe_hash, stored_entry.last_used_unix_ms));
    }
    out.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    out.truncate(limit);
    Ok(out.into_iter().map(|(h, _)| h).collect())
}

fn key_matches_origin_matrix(stored: &CookKey, miss: &CookKey) -> bool {
    if stored.recipe_hash == miss.recipe_hash {
        return false;
    }
    stored.target_triple == miss.target_triple
        && stored.profile == miss.profile
        && stored.channel == miss.channel
        && stored.rustc_version == miss.rustc_version
}

/// Bump the `last_used_unix_ms` field for the entry whose `sha256`
/// matches.
pub fn touch(db_path: &Path, sha256: &[u8; 32], now_unix_ms: u64) -> Result<bool, RegistryError> {
    let db = open_db(db_path)?;
    let mut updates: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (k_bytes, v_bytes) in raw_rows(&db)? {
        let Ok(mut value) = decode_entry(&v_bytes) else {
            continue;
        };
        if &value.sha256 == sha256 {
            value.last_used_unix_ms = now_unix_ms;
            let new_v = prost_tagged_bytes(&cook_entry_to_wire(&value));
            updates.push((k_bytes, new_v));
        }
    }
    if updates.is_empty() {
        return Ok(false);
    }
    let tx = db.unchecked_transaction()?;
    for (k, v) in &updates {
        tx.execute(
            "UPDATE cook_index_v2 SET value = ?2 WHERE key = ?1",
            params![k, v],
        )?;
    }
    tx.commit()?;
    Ok(true)
}

/// Collect every `(CookKey, CookEntry)` row in `cook_index_v2` for
/// ranking / filtering by an eviction pass. Undecodable rows are
/// silently skipped — eviction is best-effort and we never want a
/// single corrupt row to wedge the pass.
pub fn iter_entries(db_path: &Path) -> Result<Vec<(CookKey, CookEntry)>, RegistryError> {
    let db = open_db(db_path)?;
    let mut out: Vec<(CookKey, CookEntry)> = Vec::new();
    for (k_bytes, v_bytes) in raw_rows(&db)? {
        let Ok(key) = decode_key(&k_bytes) else {
            continue;
        };
        let Ok(value) = decode_entry(&v_bytes) else {
            continue;
        };
        out.push((key, value));
    }
    Ok(out)
}

/// Delete every `cook_index_v2` row whose decoded entry carries the
/// supplied `sha256`. Returns `true` when at least one row was
/// removed. Does NOT touch the on-disk `<sha256>.tar.zst` artifact —
/// the eviction pass is responsible for unlinking that file
/// separately.
pub fn evict(db_path: &Path, sha256: &[u8; 32]) -> Result<bool, RegistryError> {
    let db = open_db(db_path)?;
    let mut victims: Vec<Vec<u8>> = Vec::new();
    for (k_bytes, v_bytes) in raw_rows(&db)? {
        let Ok(value) = decode_entry(&v_bytes) else {
            continue;
        };
        if &value.sha256 == sha256 {
            victims.push(k_bytes);
        }
    }
    if victims.is_empty() {
        return Ok(false);
    }
    let tx = db.unchecked_transaction()?;
    for k in &victims {
        tx.execute("DELETE FROM cook_index_v2 WHERE key = ?1", params![k])?;
    }
    tx.commit()?;
    Ok(true)
}

/// Aggregate statistics surfaced via `Status` and `soldr daemon
/// status`: returns `(entry_count, total_size_bytes)`.
pub fn stats(db_path: &Path) -> Result<(u64, u64), RegistryError> {
    let db = open_db(db_path)?;
    let mut count: u64 = 0;
    let mut total: u64 = 0;
    for (_, v_bytes) in raw_rows(&db)? {
        if let Ok(decoded) = decode_entry(&v_bytes) {
            count = count.saturating_add(1);
            total = total.saturating_add(decoded.size_bytes);
        }
    }
    Ok((count, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_key(recipe_byte: u8) -> CookKey {
        CookKey {
            recipe_hash: [recipe_byte; 32],
            target_triple: "x86_64-unknown-linux-gnu".into(),
            profile: "release".into(),
            channel: "1.94.1".into(),
            rustc_version: "rustc 1.94.1 (abcdef0)".into(),
        }
    }

    fn sample_entry(sha_byte: u8, size: u64, origin: Option<&str>) -> CookEntry {
        CookEntry {
            sha256: [sha_byte; 32],
            size_bytes: size,
            created_unix_ms: 1_700_000_000_000,
            last_used_unix_ms: 1_700_000_000_000,
            origin_url_normalized: origin.map(str::to_owned),
            cook_cmd_summary: "cook --release".into(),
            branch_name: None,
            compile_duration_ms: 60_000,
            save_elapsed_ms: 5_000,
        }
    }

    #[test]
    fn round_trip_upsert_lookup() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        let key = sample_key(1);
        let entry = sample_entry(0xAA, 1024, Some("https://github.com/zackees/soldr"));
        upsert(&path, &key, &entry).expect("upsert");
        let got = lookup(&path, &key).expect("lookup");
        assert_eq!(got, Some(entry));
    }

    #[test]
    fn lookup_returns_none_when_missing() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        let key = sample_key(2);
        assert_eq!(lookup(&path, &key).expect("lookup"), None);
    }

    #[test]
    fn per_target_safety_isolates_rows() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        let mut a = sample_key(3);
        a.target_triple = "x86_64-unknown-linux-gnu".into();
        let mut b = sample_key(3);
        b.target_triple = "aarch64-apple-darwin".into();
        upsert(&path, &a, &sample_entry(0xAA, 100, None)).expect("upsert a");
        upsert(&path, &b, &sample_entry(0xBB, 200, None)).expect("upsert b");
        let got_a = lookup(&path, &a).expect("lookup a").expect("hit a");
        let got_b = lookup(&path, &b).expect("lookup b").expect("hit b");
        assert_eq!(got_a.sha256, [0xAA; 32]);
        assert_eq!(got_b.sha256, [0xBB; 32]);
        assert_eq!(got_a.size_bytes, 100);
        assert_eq!(got_b.size_bytes, 200);
    }

    #[test]
    fn per_profile_safety_isolates_rows() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        let mut release = sample_key(4);
        release.profile = "release".into();
        let mut debug = sample_key(4);
        debug.profile = "dev".into();
        upsert(&path, &release, &sample_entry(0xCC, 500, None)).expect("upsert rel");
        upsert(&path, &debug, &sample_entry(0xDD, 600, None)).expect("upsert dev");
        assert_ne!(
            lookup(&path, &release).expect("a").expect("hit").sha256,
            lookup(&path, &debug).expect("b").expect("hit").sha256
        );
    }

    #[test]
    fn drift_collects_other_recipe_hashes_with_same_origin_and_target() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        let origin = "https://github.com/zackees/soldr";
        let key_a = sample_key(5);
        let key_b = sample_key(6);
        let mut entry_a = sample_entry(0xA1, 10, Some(origin));
        entry_a.last_used_unix_ms = 100;
        let mut entry_b = sample_entry(0xB2, 20, Some(origin));
        entry_b.last_used_unix_ms = 200;
        upsert(&path, &key_a, &entry_a).expect("upsert a");
        upsert(&path, &key_b, &entry_b).expect("upsert b");
        let miss = sample_key(7);
        let drift = drift_recipe_hashes(&path, &miss, Some(origin), 10).expect("drift");
        assert_eq!(drift.len(), 2);
        assert_eq!(drift[0], [6u8; 32]);
        assert_eq!(drift[1], [5u8; 32]);
    }

    #[test]
    fn drift_skips_other_targets_and_other_origins() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        let origin = "https://github.com/zackees/soldr";
        let mut same_origin_diff_triple = sample_key(10);
        same_origin_diff_triple.target_triple = "aarch64-apple-darwin".into();
        let same_origin_diff_triple_entry = sample_entry(0x11, 1, Some(origin));
        let mut diff_origin = sample_key(11);
        diff_origin.target_triple = "x86_64-unknown-linux-gnu".into();
        let diff_origin_entry = sample_entry(0x22, 1, Some("https://github.com/other/repo"));
        upsert(
            &path,
            &same_origin_diff_triple,
            &same_origin_diff_triple_entry,
        )
        .expect("upsert sodt");
        upsert(&path, &diff_origin, &diff_origin_entry).expect("upsert do");
        let mut miss = sample_key(12);
        miss.target_triple = "x86_64-unknown-linux-gnu".into();
        let drift = drift_recipe_hashes(&path, &miss, Some(origin), 10).expect("drift");
        assert!(drift.is_empty());
    }

    #[test]
    fn fallback_prefers_branch_lineage_before_newer_unrelated_branch() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        let origin = "https://github.com/zackees/soldr";

        let main_key = sample_key(60);
        let other_key = sample_key(61);

        let mut main_entry = sample_entry(0x60, 10, Some(origin));
        main_entry.branch_name = Some("main".into());
        main_entry.last_used_unix_ms = 100;
        let mut other_entry = sample_entry(0x61, 20, Some(origin));
        other_entry.branch_name = Some("other".into());
        other_entry.last_used_unix_ms = 200;

        upsert(&path, &main_key, &main_entry).expect("upsert main");
        upsert(&path, &other_key, &other_entry).expect("upsert other");

        let miss = sample_key(62);
        let lineage = vec!["feature/cook".to_string(), "main".to_string()];
        let (key, entry) = lookup_origin_fallback(&path, &miss, Some(origin), &lineage)
            .expect("fallback")
            .expect("hit");
        assert_eq!(key.recipe_hash, [60u8; 32]);
        assert_eq!(entry.sha256, [0x60; 32]);
    }

    #[test]
    fn fallback_keeps_legacy_unbranched_entries_when_lineage_is_known() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        let origin = "https://github.com/zackees/soldr";

        let old_key = sample_key(70);
        let new_key = sample_key(71);
        let mut old_entry = sample_entry(0x70, 10, Some(origin));
        old_entry.last_used_unix_ms = 100;
        let mut new_entry = sample_entry(0x71, 20, Some(origin));
        new_entry.branch_name = Some("other".into());
        new_entry.last_used_unix_ms = 200;

        upsert(&path, &old_key, &old_entry).expect("upsert old");
        upsert(&path, &new_key, &new_entry).expect("upsert new");

        let miss = sample_key(72);
        let lineage = vec!["feature/cook".to_string(), "main".to_string()];
        let (key, entry) = lookup_origin_fallback(&path, &miss, Some(origin), &lineage)
            .expect("fallback")
            .expect("hit");
        assert_eq!(key.recipe_hash, [70u8; 32]);
        assert_eq!(entry.sha256, [0x70; 32]);
    }

    #[test]
    fn fallback_uses_newest_same_origin_when_lineage_is_empty() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        let origin = "https://github.com/zackees/soldr";

        let old_key = sample_key(80);
        let new_key = sample_key(81);
        let mut old_entry = sample_entry(0x80, 10, Some(origin));
        old_entry.last_used_unix_ms = 100;
        let mut new_entry = sample_entry(0x81, 20, Some(origin));
        new_entry.branch_name = Some("other".into());
        new_entry.last_used_unix_ms = 200;

        upsert(&path, &old_key, &old_entry).expect("upsert old");
        upsert(&path, &new_key, &new_entry).expect("upsert new");

        let miss = sample_key(82);
        let (key, entry) = lookup_origin_fallback(&path, &miss, Some(origin), &[])
            .expect("fallback")
            .expect("hit");
        assert_eq!(key.recipe_hash, [81u8; 32]);
        assert_eq!(entry.sha256, [0x81; 32]);
    }

    #[test]
    fn drift_returns_empty_without_origin_hint() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        upsert(
            &path,
            &sample_key(20),
            &sample_entry(0xEE, 1, Some("https://x")),
        )
        .expect("upsert");
        let miss = sample_key(21);
        assert!(drift_recipe_hashes(&path, &miss, None, 10)
            .expect("drift")
            .is_empty());
    }

    #[test]
    fn touch_bumps_last_used_ms() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        let key = sample_key(30);
        let mut entry = sample_entry(0xFF, 1, None);
        entry.last_used_unix_ms = 1;
        upsert(&path, &key, &entry).expect("upsert");
        assert!(touch(&path, &[0xFF; 32], 9_999).expect("touch"));
        let got = lookup(&path, &key).expect("lookup").expect("hit");
        assert_eq!(got.last_used_unix_ms, 9_999);
    }

    #[test]
    fn touch_returns_false_when_sha_unknown() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        assert!(!touch(&path, &[0x00; 32], 1).expect("touch"));
    }

    #[test]
    fn stats_sums_entries_and_sizes() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        upsert(&path, &sample_key(40), &sample_entry(0x40, 1_024, None)).expect("a");
        upsert(&path, &sample_key(41), &sample_entry(0x41, 2_048, None)).expect("b");
        upsert(&path, &sample_key(42), &sample_entry(0x42, 4_096, None)).expect("c");
        let (count, total) = stats(&path).expect("stats");
        assert_eq!(count, 3);
        assert_eq!(total, 1_024 + 2_048 + 4_096);
    }

    #[test]
    fn ensure_initialized_is_idempotent() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        ensure_initialized(&path).expect("init 1");
        ensure_initialized(&path).expect("init 2");
        let (count, total) = stats(&path).expect("stats");
        assert_eq!((count, total), (0, 0));
    }

    // Tag-byte hygiene: every value blob starts with 0x01.
    #[test]
    fn value_blobs_carry_the_prost_tag() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.sqlite3");
        upsert(&path, &sample_key(50), &sample_entry(0x50, 1, None)).expect("upsert");
        let db = open_db(&path).expect("open");
        for (_, value) in raw_rows(&db).expect("rows") {
            assert_eq!(value[0], REDB_TAG_PROST);
        }
    }

    // A legacy redb store beside the SQLite file is deleted whole on
    // first open — the modern replacement of the old per-table v1 sweep.
    #[test]
    fn legacy_redb_store_is_deleted_on_first_open() {
        let dir = TempDir::new().expect("tempdir");
        let legacy = dir.path().join("state.redb");
        std::fs::write(&legacy, b"pre-sqlite bytes").expect("seed legacy");
        let path = dir.path().join("state.sqlite3");
        ensure_initialized(&path).expect("ensure");
        assert!(
            !legacy.exists(),
            "legacy redb store must be dropped by the first sqlite open"
        );
    }
}
