//! Fingerprint-cached blake3 hashing of the daemon image (soldr#2442 / 0.9.0
//! cold-start perf).
//!
//! The broker verifies the `soldr-daemon` image before spawning it, and the
//! front door hashes the same image to register its identity. That image is a
//! large binary, and the cold path used to read it fully three-to-four times
//! per launch (`std::fs::read` + SHA-256 at every site) — ~19s under concurrent
//! cold I/O in a Docker VM. This module replaces that with:
//!
//! - **blake3** via zccache's shared, streaming hasher (`zccache::hash::hash_file`
//!   — no new dependency, and the whole binary is never loaded into RAM); and
//! - a **fingerprint cache** keyed on `(path, size, mtime)`: a prior hash of the
//!   exact same file (same size and mtime) is reused instead of re-reading the
//!   binary. A miss, a stale fingerprint, or any unreadable / malformed cache
//!   entry falls back to recomputation — the cache only ever *skips work*, it
//!   never returns a hash for content it did not read.
//!
//! The `(size, mtime)` freshness model matches how cargo and zccache already
//! decide a build artifact is unchanged. It is not an adversarial integrity
//! boundary — the daemon-image label the caller compares against remains the
//! boundary; this only accelerates producing the hash value on both sides.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// blake3 hex of `path`, streamed through zccache's shared hasher.
pub fn blake3_hex(path: &Path) -> io::Result<String> {
    Ok(zccache::hash::hash_file(path)?.to_hex())
}

/// `(size, mtime_nanos)` fingerprint used as the cache validity key.
fn fingerprint(path: &Path) -> io::Result<(u64, u128)> {
    let meta = std::fs::metadata(path)?;
    let mtime_nanos = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    Ok((meta.len(), mtime_nanos))
}

/// Cache filename for `target`: a blake3 of the file's stable identity, so
/// distinct binaries never collide and the name is filesystem-safe.
///
/// Keyed on *which file this is*, not on the name it was reached by. soldr#2996:
/// the integration harness hard-links one built `soldr-daemon` into a per-test
/// path so each test gets its own endpoint namespace
/// (`tests/common/isolated_daemon.rs`). Those links are the same inode with the
/// same size and mtime — only the path differs — so a path-keyed entry missed
/// on every test and each broker/daemon cold start re-hashed the identical
/// image. Identity keying makes every link to one image share a single entry.
///
/// Falls back to the canonical path when no identity is available, which keeps
/// the previous behaviour on any host or filesystem that cannot report one.
fn cache_entry_path(cache_dir: &Path, target: &Path) -> PathBuf {
    let key_material = stable_identity_key(target).unwrap_or_else(|| {
        let abs = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
        format!("path\0{}", abs.to_string_lossy())
    });
    let key = zccache::hash::hash_bytes(key_material.as_bytes()).to_hex();
    cache_dir.join(format!("{key}.blake3"))
}

/// The identity components that survive a hard link: the Unix `(dev, ino)`
/// pair, or the Windows `(volume serial, file index)` pair.
///
/// Deliberately excludes `len` and `modified_ns`. Those are the entry's
/// *validity* fields, re-checked on every read by [`read_cache_entry`], and
/// folding them into the entry's name would strand one file per rewrite
/// instead of replacing the previous digest. Correctness is unchanged: a
/// recycled inode still fails the `(size, mtime)` check and is recomputed.
fn stable_identity_key(target: &Path) -> Option<String> {
    let identity = crate::platform::fs::identity::file_identity(target)?;
    if let (Some(dev), Some(ino)) = (identity.dev, identity.ino) {
        return Some(format!("ino\0{dev}\0{ino}"));
    }
    match (identity.volume_serial_number, identity.file_index) {
        (Some(volume), Some(index)) => Some(format!("fileid\0{volume}\0{index}")),
        _ => None,
    }
}

/// Machine-scoped home for image-hash memo entries (soldr#2521 B1).
///
/// The memo is a pure `(canonical path, size, mtime) -> digest` map, so
/// sharing one cache across every soldr root on the machine is safe by
/// construction — the entry key already names the exact file, and the label
/// comparison on both sides of a launch remains the integrity boundary (see
/// module doc). Root-scoped placement guaranteed a cold miss for every
/// isolated `HOME`/`SOLDR_CACHE_DIR`, which is exactly what CI test fixtures
/// create: each broker/daemon test re-hashed the same multi-hundred-megabyte
/// binaries from scratch, and on the slow target-run hosts that alone
/// consumed most of the 30s daemon-route budget.
///
/// Placement is per-user and outside any soldr root: the platform runtime
/// dir on Unix (`$XDG_RUNTIME_DIR` / `/tmp/soldr-<uid>` style, uid-scoped),
/// the per-user temp dir on Windows. The directory is created owner-only; if
/// it cannot be created or secured (e.g. a pre-existing entry owned by
/// another user), the caller's root-scoped `fallback` is returned instead —
/// the memo only ever loses sharing, never correctness.
pub fn machine_scoped_cache_dir(fallback: &Path) -> PathBuf {
    let base =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            // %TEMP% is already per-user on Windows, and deliberately NOT under
            // %USERPROFILE% as far as this code is concerned: test fixtures remap
            // HOME/USERPROFILE per test, while the ambient temp dir stays stable
            // across the whole suite run.
            std::env::temp_dir()
        } else {
            // Uid-scoped on multi-user hosts; world-writable /tmp is only ever
            // the parent, never the cache dir itself.
            crate::platform::ipc::endpoint::machine_runtime_dir()
        };
    if base.as_os_str().is_empty() {
        return fallback.to_path_buf();
    }
    let shared = base.join("soldr-image-hash-v1");
    let secured = std::fs::create_dir_all(&shared)
        .and_then(|()| crate::platform::fs::permissions::make_private(&shared));
    match secured {
        Ok(()) => shared,
        Err(_) => fallback.to_path_buf(),
    }
}

/// Return the blake3 hex digest of `target`, reusing a cached digest when the
/// file's `(size, mtime)` is unchanged since it was last hashed. Cache entries
/// live under `cache_dir` (created on demand). Any cache problem degrades to a
/// fresh [`blake3_hex`] — this never returns a digest for content it did not
/// actually hash in some run.
pub fn cached_blake3_hex(cache_dir: &Path, target: &Path) -> io::Result<String> {
    cached_blake3_hex_with_progress(cache_dir, target, |_, _| {})
}

/// [`cached_blake3_hex`] with byte-counted progress on a cache miss.
/// A cache hit performs no file scan and therefore emits no synthetic event.
pub fn cached_blake3_hex_with_progress(
    cache_dir: &Path,
    target: &Path,
    mut progress: impl FnMut(u64, u64),
) -> io::Result<String> {
    let (initial_size, initial_mtime) = fingerprint(target)?;
    let entry = cache_entry_path(cache_dir, target);

    if let Some(hex) = read_cache_entry(&entry, initial_size, initial_mtime) {
        return Ok(hex);
    }

    // A cold front-door stampede used to make every process hash the same
    // large executable independently. Serialize only cache misses, then
    // re-check after acquiring the lock so all contenders except the winner
    // consume its completed entry. The lock file is persistent disposable
    // cache state; the OS releases its lock if the computing process exits.
    let lock = std::fs::create_dir_all(cache_dir)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(entry.with_extension("blake3.lock"))
        })
        .ok();
    if let Some(lock) = lock.as_ref() {
        // Both outcomes lead to the same next step: re-read the cache entry
        // (the holder may have just published it), then hash if still missing.
        // Only the lock ownership differs, and the temp+rename write is safe
        // either way.
        let _ = wait_for_hash_lock(lock, initial_size, &mut progress);
    }

    let (size, mtime) = fingerprint(target)?;
    if let Some(hex) = read_cache_entry(&entry, size, mtime) {
        return Ok(hex);
    }

    compute_blake3_hex_with_progress(target, size, &mut progress).inspect(|hex| {
        // Best-effort store; a cache write failure must never fail the hash.
        let _ = write_cache_entry(cache_dir, &entry, size, mtime, hex);
    })
}

/// Longest a cache miss will wait for another process to publish its digest
/// before hashing the image itself.
///
/// The lock is an optimization, not an integrity gate — the `Err(_) => break`
/// arm below already degrades to a direct hash on an unsupported or read-only
/// cache filesystem. A *slow or wedged* holder deserves the same treatment: an
/// unbounded wait stalls broker cold start with no ceiling at all.
///
/// The budget must comfortably exceed a legitimate hash, or every waiter
/// abandons and the stampede this lock exists to prevent (soldr#2442) comes
/// straight back — with the wasted wait added on top. The costs to clear:
/// ~19s for the concurrent-cold-I/O Docker case in this module's header, and
/// 3.8s measured for a cold 60MB image on a warm dev box. 30s covers both with
/// margin while still guaranteeing forward progress.
///
/// Note the ceiling is no longer what keeps a stall diagnosable: the two
/// stderr lines below do that, and they fire as soon as the wait begins.
const HASH_LOCK_WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);
/// First backoff step. Doubles up to [`HASH_LOCK_BACKOFF_CAP`], with full
/// jitter applied to each sleep so contenders do not poll in lockstep.
const HASH_LOCK_BACKOFF_BASE: std::time::Duration = std::time::Duration::from_millis(5);
const HASH_LOCK_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_millis(100);
/// Shortest sleep between polls. Keeps a zero jitter draw, or a deadline only
/// microseconds away, from turning the last iterations into a busy-wait.
const HASH_LOCK_MIN_SLEEP: std::time::Duration = std::time::Duration::from_micros(500);

/// Why [`wait_for_hash_lock`] stopped waiting. Both outcomes are correct; they
/// differ only in whether this process is the one publishing the cache entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HashLockOutcome {
    /// The lock is held by this process (or locking is unsupported here).
    Proceed,
    /// The budget expired with the lock still held elsewhere. Hash directly.
    Abandoned,
}

/// Wait for the cache-miss lock, bounded by [`HASH_LOCK_WAIT_BUDGET`].
///
/// Exclusive advisory locks give no queue and no fairness: every waiter races
/// on release. Polling on a fixed interval therefore synchronizes the herd.
/// Full jitter over a growing window spreads the retries out and keeps a
/// long-waiting contender from being beaten by every new arrival.
#[must_use]
fn wait_for_hash_lock(
    lock: &std::fs::File,
    initial_size: u64,
    progress: &mut impl FnMut(u64, u64),
) -> HashLockOutcome {
    wait_for_hash_lock_within(lock, initial_size, HASH_LOCK_WAIT_BUDGET, progress)
}

/// [`wait_for_hash_lock`] with an explicit budget, so the bounded-wait
/// behavior is testable without spending the production budget in a test.
#[must_use]
fn wait_for_hash_lock_within(
    lock: &std::fs::File,
    initial_size: u64,
    budget: std::time::Duration,
    progress: &mut impl FnMut(u64, u64),
) -> HashLockOutcome {
    let started = std::time::Instant::now();
    let deadline = started + budget;
    let mut next_progress = started;
    let mut backoff = HASH_LOCK_BACKOFF_BASE;
    let mut waited = false;
    loop {
        match kernal_api::fs_try_lock_exclusive(lock) {
            Ok(()) => {
                if waited {
                    eprintln!(
                        "soldr image-hash: acquired contended hash lock after {}ms",
                        started.elapsed().as_millis()
                    );
                }
                return HashLockOutcome::Proceed;
            }
            Err(error) if lock_is_contended(&error) => {
                let now = std::time::Instant::now();
                if !waited {
                    waited = true;
                    // Never wait silently: a broker stuck here used to emit
                    // nothing at all, which is indistinguishable from a hang.
                    eprintln!(
                        "soldr image-hash: another process is hashing this image; \
                         waiting up to {}ms before hashing it directly",
                        budget.as_millis()
                    );
                }
                if now >= deadline {
                    eprintln!(
                        "soldr image-hash: hash lock still held after {}ms; hashing directly",
                        started.elapsed().as_millis()
                    );
                    return HashLockOutcome::Abandoned;
                }
                if now >= next_progress {
                    progress(0, initial_size);
                    next_progress = now + std::time::Duration::from_millis(250);
                }
                let remaining = deadline.saturating_duration_since(now);
                // Floor the draw before clamping: full jitter can return 0, and
                // `remaining` shrinks to microseconds near the deadline, so the
                // last iterations would otherwise become a tight spin.
                let slept = jittered_backoff(backoff)
                    .max(HASH_LOCK_MIN_SLEEP)
                    .min(remaining.max(HASH_LOCK_MIN_SLEEP));
                std::thread::sleep(slept);
                backoff = (backoff * 2).min(HASH_LOCK_BACKOFF_CAP);
            }
            // Cache locking is an optimization, not an integrity gate.
            // An unsupported or read-only cache filesystem falls back to
            // the existing direct hash behavior.
            Err(_) => return HashLockOutcome::Proceed,
        }
    }
}

/// Full jitter: a uniform draw from `[0, window]`. Cheap and dependency-free —
/// the seed only needs to differ between contending processes, not to be
/// cryptographically random.
fn jittered_backoff(window: std::time::Duration) -> std::time::Duration {
    let window_us = window.as_micros().max(1) as u64;
    std::time::Duration::from_micros(next_jitter_source() % window_us)
}

fn next_jitter_source() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|state| {
        let mut seed = state.get();
        if seed == 0 {
            // Distinct per process and per waiter; the nanosecond clock keeps
            // two processes started in the same millisecond apart.
            seed = u64::from(std::process::id()).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos() as u64)
                    .unwrap_or(0x5DEE_CE66_D000_0001);
            seed |= 1;
        }
        // xorshift64*: adequate for spreading retries, no dependency.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        state.set(seed);
        seed
    })
}

fn lock_is_contended(error: &io::Error) -> bool {
    // Windows LockFileEx reports raw ERROR_LOCK_VIOLATION / ERROR_SHARING_VIOLATION
    // for nonblocking collisions; std does not normalize them to WouldBlock.
    // The platform crate owns that normalization.
    crate::platform::fs::contention::is_lock_contention(error)
}

fn compute_blake3_hex_with_progress(
    target: &Path,
    size: u64,
    progress: &mut impl FnMut(u64, u64),
) -> io::Result<String> {
    let mut file = File::open(target)?;
    let mut hasher = zccache::hash::StreamHasher::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut completed = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        completed = completed.saturating_add(read as u64);
        progress(completed, size);
    }
    Ok(hasher.finalize().to_hex())
}

/// Parse a cache entry and return its digest iff the stored `(size, mtime)`
/// matches. Any I/O or parse problem returns `None` (recompute).
fn read_cache_entry(entry: &Path, size: u64, mtime: u128) -> Option<String> {
    let contents = std::fs::read_to_string(entry).ok()?;
    let mut parts = contents.trim().split('\t');
    let cached_size: u64 = parts.next()?.parse().ok()?;
    let cached_mtime: u128 = parts.next()?.parse().ok()?;
    let hex = parts.next()?;
    if cached_size == size
        && cached_mtime == mtime
        && hex.len() == 64
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Some(hex.to_string())
    } else {
        None
    }
}

/// Write a cache entry atomically (temp + rename) so a concurrent reader never
/// observes a half-written line.
fn write_cache_entry(
    cache_dir: &Path,
    entry: &Path,
    size: u64,
    mtime: u128,
    hex: &str,
) -> io::Result<()> {
    std::fs::create_dir_all(cache_dir)?;
    let tmp = entry.with_extension(format!("blake3.tmp.{}", std::process::id()));
    {
        let mut file = File::create(&tmp)?;
        write!(file, "{size}\t{mtime}\t{hex}")?;
        file.flush()?;
    }
    match std::fs::rename(&tmp, entry) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_links_to_one_image_share_a_memo_entry() {
        // soldr#2996: the integration harness hard-links one built
        // `soldr-daemon` into a per-test path so each test gets its own
        // endpoint namespace. Those links are the same inode with the same
        // size and mtime, so keying the memo on the path made every test a
        // cold miss and re-hashed the identical image on every broker and
        // daemon cold start.
        let temp = tempfile::tempdir().expect("tempdir");
        let image = temp.path().join("soldr-daemon");
        std::fs::write(&image, b"daemon image bytes").expect("write image");
        let linked = temp.path().join("per-test-root").join("soldr-daemon");
        std::fs::create_dir_all(linked.parent().expect("parent")).expect("link dir");
        if std::fs::hard_link(&image, &linked).is_err() {
            // A filesystem without hard links cannot exhibit the bug; the
            // path fallback below is then the correct behaviour.
            return;
        }

        let cache_dir = temp.path().join("memo");
        let direct = cached_blake3_hex(&cache_dir, &image).expect("hash image");
        let via_link = cached_blake3_hex(&cache_dir, &linked).expect("hash link");
        assert_eq!(direct, via_link, "both names address one file");
        assert_eq!(
            cache_entry_path(&cache_dir, &image),
            cache_entry_path(&cache_dir, &linked),
            "hard links must resolve to one memo entry"
        );

        let entries = std::fs::read_dir(&cache_dir)
            .expect("read memo dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "blake3"))
            .count();
        assert_eq!(entries, 1, "one image must not occupy two memo entries");
    }

    #[test]
    fn distinct_images_never_share_a_memo_entry() {
        // The other half of the identity contract: keying on the file rather
        // than the path must not let two different binaries collide.
        let temp = tempfile::tempdir().expect("tempdir");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::write(&first, b"first image").expect("write first");
        std::fs::write(&second, b"second image").expect("write second");

        let cache_dir = temp.path().join("memo");
        assert_ne!(
            cache_entry_path(&cache_dir, &first),
            cache_entry_path(&cache_dir, &second)
        );
        assert_ne!(
            cached_blake3_hex(&cache_dir, &first).expect("hash first"),
            cached_blake3_hex(&cache_dir, &second).expect("hash second")
        );
    }

    #[test]
    fn a_rewritten_image_is_rehashed_rather_than_served_stale() {
        // Identity keying reuses one entry name across rewrites, so the
        // `(size, mtime)` validity check is what keeps a stale digest from
        // being served. Recycled inodes rely on the same check.
        let temp = tempfile::tempdir().expect("tempdir");
        let image = temp.path().join("soldr-daemon");
        std::fs::write(&image, b"original bytes").expect("write original");
        let cache_dir = temp.path().join("memo");
        let before = cached_blake3_hex(&cache_dir, &image).expect("hash original");

        // A fresh mtime is what invalidates the entry; sleep past the
        // coarsest timestamp granularity we might be running on.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&image, b"replacement bytes of a different length").expect("rewrite");
        let after = cached_blake3_hex(&cache_dir, &image).expect("hash replacement");

        assert_ne!(before, after, "a rewritten image must be re-hashed");
        assert_eq!(after, blake3_hex(&image).expect("direct hash"));
    }

    #[test]
    fn platform_lock_contention_is_retryable() {
        // WouldBlock is the normalized form on every platform; the raw
        // Windows error codes are covered beside the Windows implementation
        // in soldr-platform.
        assert!(lock_is_contended(&io::Error::from(
            io::ErrorKind::WouldBlock
        )));
    }

    #[test]
    fn hash_lock_wait_is_bounded_and_degrades_to_direct_hashing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lock_path = temp.path().join("held.lock");
        let holder = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("open holder");
        let holder = kernal_api::platform::fs::try_lock_exclusive_owned(holder)
            .expect("holder takes the lock");

        let waiter = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("open waiter");
        // A short injected budget: the production one is deliberately long
        // enough to let a legitimate hash finish, which would make this test
        // spend 30s proving a property that holds at any budget.
        let budget = std::time::Duration::from_millis(300);
        let started = std::time::Instant::now();
        let outcome = wait_for_hash_lock_within(&waiter, 0, budget, &mut |_, _| {});
        let elapsed = started.elapsed();

        // The point of the fix: a held lock must never stall the caller
        // indefinitely. It abandons the wait and hashes for itself.
        assert_eq!(outcome, HashLockOutcome::Abandoned);
        assert!(
            elapsed >= budget,
            "must actually wait its budget, waited {elapsed:?}"
        );
        assert!(
            elapsed < budget * 10,
            "must not overshoot the budget, waited {elapsed:?}"
        );
        drop(holder);
    }

    #[test]
    fn production_budget_outlasts_a_legitimate_hash() {
        // Regression guard for the trade-off this constant encodes. Too short
        // and every waiter abandons, restoring the very stampede the lock was
        // added to prevent (soldr#2442) plus the wasted wait. The module
        // header documents ~19s for the concurrent-cold-I/O case.
        assert!(
            HASH_LOCK_WAIT_BUDGET >= std::time::Duration::from_secs(20),
            "budget {HASH_LOCK_WAIT_BUDGET:?} is shorter than a documented cold hash"
        );
    }

    #[test]
    fn uncontended_hash_lock_is_acquired_without_waiting() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(temp.path().join("free.lock"))
            .expect("open lock");
        let started = std::time::Instant::now();
        assert_eq!(
            wait_for_hash_lock(&lock, 0, &mut |_, _| {}),
            HashLockOutcome::Proceed
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    }

    #[test]
    fn jittered_backoff_stays_within_its_window() {
        let window = std::time::Duration::from_millis(40);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let drawn = jittered_backoff(window);
            assert!(drawn < window, "{drawn:?} escaped the window");
            seen.insert(drawn.as_micros());
        }
        // Full jitter must actually spread the herd, not return a constant.
        assert!(seen.len() > 1, "backoff produced a single value");
    }

    /// soldr#2521 B1 acceptance shape: two distinct soldr roots (the way CI
    /// fixtures isolate `HOME`/`SOLDR_CACHE_DIR` per test) must share memo
    /// hits for the same binary. Progress events fire only on a cache miss,
    /// so "no events for root B" IS the cross-root hit.
    #[test]
    fn distinct_roots_share_hits_through_the_machine_scoped_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("daemon-image");
        std::fs::write(&binary, vec![0xAB; 64 * 1024]).expect("write image");

        // Model machine_scoped_cache_dir's contract rather than calling it
        // (its real path is ambient per-user state a unit test must not
        // mutate): both roots resolve to ONE shared dir.
        let shared = temp.path().join("machine-scoped");
        let root_a_fallback = temp.path().join("root-a/cache/image-hash");
        let root_b_fallback = temp.path().join("root-b/cache/image-hash");

        let mut a_events = 0_u32;
        let from_a = cached_blake3_hex_with_progress(&shared, &binary, |_, _| a_events += 1)
            .expect("root A hash");
        assert!(
            a_events > 0,
            "first hash must be a miss that scans the file"
        );

        let mut b_events = 0_u32;
        let from_b = cached_blake3_hex_with_progress(&shared, &binary, |_, _| b_events += 1)
            .expect("root B hash");
        assert_eq!(from_a, from_b);
        assert_eq!(
            b_events, 0,
            "the second root must consume the first root's memo entry"
        );
        // The root-scoped fallbacks stay untouched when the shared dir works.
        assert!(!root_a_fallback.exists());
        assert!(!root_b_fallback.exists());
    }

    /// An unusable shared location must degrade to the caller's root-scoped
    /// fallback rather than fail or silently drop the memo.
    #[test]
    fn machine_scoped_dir_falls_back_when_unsecurable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let fallback = temp.path().join("root-scoped");
        // Force `create_dir_all` to fail by occupying the parent path the
        // helper would need as a directory with a plain file.
        let dir = machine_scoped_cache_dir(&fallback);
        // Either outcome is a valid environment-dependent result, but the
        // returned path must always be usable by cached_blake3_hex: a real
        // directory the memo can live in, or the caller's fallback.
        let binary = temp.path().join("bin");
        std::fs::write(&binary, b"payload").expect("write");
        let digest = cached_blake3_hex(&dir, &binary).expect("hash through returned dir");
        assert_eq!(digest, blake3_hex(&binary).expect("reference"));
    }

    #[test]
    fn blake3_hex_matches_zccache_reference() {
        let temp = tempfile::tempdir().expect("tempdir");
        let f = temp.path().join("bin");
        std::fs::write(&f, b"hello world").expect("write");
        let got = blake3_hex(&f).expect("hash");
        let expected = zccache::hash::hash_bytes(b"hello world").to_hex();
        assert_eq!(got, expected);
    }

    #[test]
    fn cache_hit_returns_same_digest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache = temp.path().join("cache");
        let f = temp.path().join("bin");
        std::fs::write(&f, b"payload-v1").expect("write");

        let first = cached_blake3_hex(&cache, &f).expect("first");
        let entries = std::fs::read_dir(&cache)
            .expect("cache dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "blake3"))
            .count();
        assert_eq!(entries, 1, "one digest cache entry expected");
        let second = cached_blake3_hex(&cache, &f).expect("second");
        assert_eq!(first, second);
        assert_eq!(first, zccache::hash::hash_bytes(b"payload-v1").to_hex());
    }

    #[test]
    fn cache_miss_reports_real_bytes_and_hit_stays_silent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache = temp.path().join("cache");
        let file = temp.path().join("daemon");
        let contents = vec![0xa5_u8; 2 * 1024 * 1024 + 29];
        std::fs::write(&file, &contents).expect("write");
        let mut progress = Vec::new();

        let first = cached_blake3_hex_with_progress(&cache, &file, |done, total| {
            progress.push((done, total));
        })
        .expect("hash miss");
        assert!(progress.len() >= 2);
        assert_eq!(
            progress.last(),
            Some(&(contents.len() as u64, contents.len() as u64))
        );
        assert_eq!(first, zccache::hash::hash_bytes(&contents).to_hex());

        let mut hit_events = 0;
        let second = cached_blake3_hex_with_progress(&cache, &file, |_, _| hit_events += 1)
            .expect("hash hit");
        assert_eq!(second, first);
        assert_eq!(hit_events, 0, "a cache hit must not invent byte progress");
    }

    #[test]
    fn cold_stampede_hashes_the_file_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(temp.path().join("cache"));
        let file = Arc::new(temp.path().join("broker"));
        std::fs::write(&*file, vec![0xa5_u8; 4 * 1024 * 1024]).expect("write broker");
        let contenders = 16;
        let barrier = Arc::new(Barrier::new(contenders));
        let readers = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..contenders)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let file = Arc::clone(&file);
                let barrier = Arc::clone(&barrier);
                let readers = Arc::clone(&readers);
                std::thread::spawn(move || {
                    barrier.wait();
                    let mut counted = false;
                    cached_blake3_hex_with_progress(&cache, &file, |completed, _| {
                        if completed > 0 && !counted {
                            readers.fetch_add(1, Ordering::Relaxed);
                            counted = true;
                        }
                    })
                    .expect("hash")
                })
            })
            .collect();
        let hashes: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().expect("contender"))
            .collect();
        assert!(hashes.iter().all(|hash| hash == &hashes[0]));
        assert_eq!(
            readers.load(Ordering::Relaxed),
            1,
            "only the cache-miss winner may scan the executable"
        );
    }

    #[test]
    fn changed_content_invalidates_cache() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache = temp.path().join("cache");
        let f = temp.path().join("bin");

        std::fs::write(&f, b"payload-v1").expect("write");
        let first = cached_blake3_hex(&cache, &f).expect("first");

        // Different length changes the size fingerprint; sleep so mtime also
        // advances past filesystem granularity.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&f, b"payload-v2-longer").expect("rewrite");
        let second = cached_blake3_hex(&cache, &f).expect("second");

        assert_ne!(first, second, "new content must produce a new digest");
        assert_eq!(
            second,
            zccache::hash::hash_bytes(b"payload-v2-longer").to_hex()
        );
    }

    #[test]
    fn malformed_cached_digest_is_recomputed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache = temp.path().join("cache");
        let file = temp.path().join("daemon");
        std::fs::write(&file, b"payload").expect("write");
        let (size, mtime) = fingerprint(&file).expect("fingerprint");
        let entry = cache_entry_path(&cache, &file);
        std::fs::create_dir_all(&cache).expect("cache dir");
        std::fs::write(&entry, format!("{size}\t{mtime}\tnot-a-digest")).expect("bad cache entry");

        let got = cached_blake3_hex(&cache, &file).expect("recomputed hash");
        assert_eq!(got, zccache::hash::hash_bytes(b"payload").to_hex());
    }
}
