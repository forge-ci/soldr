//! Content-addressed memo for the daemon identity's legacy SHA-256 (soldr#3736).
//!
//! `running-process`'s default `DaemonProcess::current_process` reads the
//! daemon executable twice: once with BLAKE3 for `exe_hash`, and once with
//! SHA-256 for `legacy_exe_sha256`, the digest that pre-BLAKE3 stable brokers
//! still read from the probe wire. soldr keeps sending that digest, so the
//! second read cannot simply be dropped.
//!
//! It can be avoided, though. Measured in the bosn tests lane, the SHA-256 pass
//! was ~280 ms of the ~375 ms `daemon_identity` + later phases of every cold
//! start, even with `sha2` optimized, while BLAKE3 over the same ~147 MB image
//! takes ~8 ms. Every broker route stages its own *copy* of the daemon image
//! (a distinct inode), so the inode-keyed memo in [`super::image_hash`] can
//! never hit here. Content can: the BLAKE3 digest is computed anyway, and
//! `blake3 -> sha256` is a pure function of the bytes. Keying the memo by the
//! BLAKE3 digest makes it correct by construction, independent of path,
//! inode, size or mtime -- a hit is only possible for identical content.
//!
//! Any memo problem (missing, unreadable, malformed, unwritable) degrades to
//! computing the digest; the memo only ever skips work.

use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::backend_lifecycle::identity::DaemonIdentityHashPolicy;
use running_process::broker::backend_lifecycle::identity::IdentityError;
use running_process::broker::protocol::Endpoint;
use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Build the current daemon identity, reusing a memoized legacy SHA-256 when
/// this exact executable content has been hashed before on this machine.
///
/// Produces the same identity as `DaemonProcess::current_process` (the
/// `LegacyCompatible` policy), field for field.
pub(crate) fn current_process_memoized(
    ipc_endpoint: Endpoint,
    idle_timeout_secs: Option<u32>,
    memo_dir: &Path,
) -> Result<DaemonProcess, IdentityError> {
    let mut daemon = DaemonProcess::current_process_with_hash_policy(
        ipc_endpoint,
        idle_timeout_secs,
        DaemonIdentityHashPolicy::Blake3Only,
    )?;
    let exe_path = daemon.exe_path.clone();
    daemon.legacy_exe_sha256 =
        memoized_legacy_sha256(memo_dir, &daemon.exe_hash, || sha256_file(&exe_path))
            .map_err(IdentityError::Io)?;
    Ok(daemon)
}

/// Return the SHA-256 for the content whose BLAKE3 digest is `blake3`, from
/// the memo when present, otherwise from `compute` (then stored best-effort).
pub(crate) fn memoized_legacy_sha256(
    memo_dir: &Path,
    blake3: &[u8; 32],
    compute: impl FnOnce() -> io::Result<[u8; 32]>,
) -> io::Result<[u8; 32]> {
    let entry = memo_entry_path(memo_dir, blake3);
    if let Some(digest) = read_entry(&entry) {
        return Ok(digest);
    }
    let digest = compute()?;
    // Best-effort store; a memo write failure must never fail the identity.
    let _ = write_entry(memo_dir, &entry, &digest);
    Ok(digest)
}

fn memo_entry_path(memo_dir: &Path, blake3: &[u8; 32]) -> PathBuf {
    memo_dir.join(format!("{}.legacy-sha256", hex(blake3)))
}

fn read_entry(entry: &Path) -> Option<[u8; 32]> {
    let text = std::fs::read_to_string(entry).ok()?;
    parse_hex32(text.trim())
}

fn write_entry(memo_dir: &Path, entry: &Path, digest: &[u8; 32]) -> io::Result<()> {
    std::fs::create_dir_all(memo_dir)?;
    // Temp + rename so a concurrent reader never sees a partial entry.
    let tmp = memo_dir.join(format!(
        ".{}.{}.tmp",
        entry
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&tmp, hex(digest))?;
    std::fs::rename(&tmp, entry).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn sha256_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.finalize().into())
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_hex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
#[path = "legacy_sha256_memo_tests.rs"]
mod tests;
