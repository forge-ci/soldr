//! Helpers for materializing multicall shim names from the main `soldr`
//! binary.
//!
//! Issue #1302 removes tiny sidecar shim executables from release
//! archives. Installers now create hardlinks to `soldr` under the desired
//! argv[0] name, falling back to byte-for-byte copies when hardlinks are
//! unavailable.

use crate::core::SoldrError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub(crate) const LINK_MODE_HARDLINK: &str = "hardlink";
pub(crate) const LINK_MODE_COPY: &str = "copy";
pub(crate) const LINK_MODE_HARDLINK_OR_COPY: &str = "hardlink-or-copy";
/// Reported for position-dependent sources (#1908), which get a
/// `#!/bin/sh` trampoline rather than a hardlink. Distinct from the others
/// so the slower path is visible in diagnostics instead of silently
/// masquerading as a hardlink.
pub(crate) const LINK_MODE_TRAMPOLINE: &str = "trampoline";
const MATERIALIZATION_MEMO_VERSION: u32 = 1;

/// Stable file identity for the materialization memo. Owned by the
/// platform crate: Unix fills the dev/ino/ctime members, Windows the
/// volume-serial/file-index/creation-time members, so this type is
/// neutral and serializable.
type FileStamp = crate::platform::fs::identity::FileIdentity;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MaterializationMemo {
    version: u32,
    source_path: String,
    source: FileStamp,
    target: FileStamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MaterializeResult {
    pub created: bool,
    pub link_mode: &'static str,
}

/// Resolve the source `soldr` binary that should be used for installed
/// multicall shims. When self-relocation is active, prefer the original
/// executable path so long-lived shims do not point at a GC-managed runtime
/// copy.
pub(crate) fn soldr_binary_source() -> Result<PathBuf, SoldrError> {
    if let Some(original) = std::env::var_os(crate::self_relocate::ORIGINAL_EXE_ENV_VAR) {
        let path = PathBuf::from(original);
        if path.is_file() {
            return Ok(path);
        }
    }
    std::env::current_exe().map_err(SoldrError::from)
}

/// Refuse to copy `source` to a location where it cannot load its libraries
/// (soldr#3403). A binary that names its bundled libraries relative to itself
/// (`$ORIGIN` / `@loader_path`) aborts in the loader when copied elsewhere,
/// before `main` and before any logging. Shims dodge that with a trampoline;
/// the broker's stable image is a byte-for-byte copy, so it must not silently
/// become a binary that cannot start.
/// [`soldr_core::self_relocate::exe_has_loader_path_reference`], remembered per
/// image content (soldr#3831).
///
/// The check parses the whole binary, which for a debug build is a 150 MB read
/// on every fresh soldr root -- the dominant cost of a cold broker route on CI.
/// The verdict depends only on the bytes, so it is keyed on the image's blake3,
/// which the machine-scoped image-hash memo already serves without a read. Any
/// memo failure falls back to the direct check.
pub(crate) fn has_loader_path_reference(source: &Path) -> bool {
    let dir = crate::daemon::image_hash::machine_scoped_cache_dir(&std::env::temp_dir());
    let Ok(digest) = crate::daemon::image_hash::cached_blake3_hex(&dir, source) else {
        return soldr_core::self_relocate::exe_has_loader_path_reference(source);
    };
    let entry = dir.join(format!("{digest}.loader-path"));
    match std::fs::read(&entry).as_deref() {
        Ok(b"1") => return true,
        Ok(b"0") => return false,
        _ => {}
    }
    let verdict = soldr_core::self_relocate::exe_has_loader_path_reference(source);
    let temporary = entry.with_extension(format!("loader-path.{}", std::process::id()));
    if std::fs::write(&temporary, if verdict { b"1" } else { b"0" }).is_ok()
        && std::fs::rename(&temporary, &entry).is_err()
    {
        let _ = std::fs::remove_file(&temporary);
    }
    verdict
}

pub(crate) fn require_relocatable(source: &Path, destination_role: &str) -> Result<(), String> {
    if has_loader_path_reference(source) {
        return Err(format!(
            "{} names its shared libraries relative to its own location ($ORIGIN or \
             @loader_path), so a copy staged as {destination_role} could not load them. This \
             happens when a source build was repaired with bundled libraries (for example \
             `pip install .` on a host with a system liblzma). Build with the libraries linked \
             statically, or run the installed release binary (soldr#3403).",
            source.display()
        ));
    }
    Ok(())
}

/// Point `target` at `source` with a `#!/bin/sh` trampoline instead of a
/// hardlink, for sources that only run from their own directory (#1908).
///
/// The trampoline forwards `"$@"` untouched and passes its own `$0` in the
/// environment, which [`crate::multicall::apply_shim_argv0_override`] restores
/// to argv[0]. That is what makes it equivalent to the hardlinked alias — the
/// earlier `exec <soldr> <stem> "$@"` form was not, and shifted every argument
/// right by one (soldr#1934). Because the identity no longer appears in the
/// script, the body does not depend on the target at all.
///
/// Compares against [`crate::shim_dir::trampoline_shim_body`] rather than
/// byte-comparing to `source`: comparing a small script to a Mach-O always
/// differs, which would republish the shim on every invocation and defeat
/// the #1831 memo fast path.
fn materialize_trampoline(source: &Path, target: &Path) -> Result<MaterializeResult, SoldrError> {
    let want = crate::shim_dir::trampoline_shim_body(source);
    if let Ok(existing) = std::fs::read_to_string(target) {
        if existing == want {
            return Ok(MaterializeResult {
                created: false,
                link_mode: LINK_MODE_TRAMPOLINE,
            });
        }
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(SoldrError::Io)?;
    }
    // Replacing a running shim in place can hit ETXTBSY, and a partially
    // written trampoline is worse than none: write beside it and rename,
    // which is atomic and leaves no window where the shim is truncated.
    let tmp = tmp_path_for(target);
    let _ = std::fs::remove_file(&tmp);
    crate::shim_dir::write_trampoline_shim(&tmp, source)?;
    std::fs::rename(&tmp, target).map_err(SoldrError::Io)?;
    Ok(MaterializeResult {
        created: true,
        link_mode: LINK_MODE_TRAMPOLINE,
    })
}

/// Install `target` as a hardlink to `source`, falling back to a copy.
/// Returns `created=false` when the existing target already has identical
/// bytes.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) fn materialize_executable(
    source: &Path,
    target: &Path,
) -> Result<MaterializeResult, SoldrError> {
    // #1908: a position-dependent binary cannot be hardlinked or copied
    // anywhere -- it names its libraries relative to its own location, so
    // dyld aborts the copy at exec, before main and before any logging.
    //
    // The guard lives *here*, in the one function every shim writer calls,
    // rather than at each call site. #1856 fixed the two writers known at
    // the time and #1908 was the writers it missed -- including one that
    // overwrote a correct trampoline with a hardlink, so the same path
    // could look right and then break. An opt-in guard makes every new
    // writer a fresh chance to reintroduce the bug; this makes the safe
    // behaviour the default; the verdict is memoized per image content.
    if has_loader_path_reference(source) {
        return materialize_trampoline(source, target);
    }
    // Regression for #1831: these shims are large copies on installations
    // where the Soldr binary and cache live on different volumes. Rehashing
    // both files for every cargo invocation made a no-op build reread
    // hundreds of MiB before Cargo could start.
    if materialization_memo_matches(source, target) {
        return Ok(MaterializeResult {
            created: false,
            link_mode: LINK_MODE_HARDLINK_OR_COPY,
        });
    }
    let verified_snapshot = materialization_memo(source, target);
    if executable_matches(target, source)? {
        if let Some(verified) = verified_snapshot.as_ref() {
            write_materialization_memo_if_unchanged(
                source,
                target,
                &verified.source,
                Some(&verified.target),
            );
        }
        return Ok(MaterializeResult {
            created: false,
            link_mode: LINK_MODE_HARDLINK_OR_COPY,
        });
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(SoldrError::Io)?;
    }

    let tmp = tmp_path_for(target);
    let _ = std::fs::remove_file(&tmp);
    let link_mode = match std::fs::hard_link(source, &tmp) {
        Ok(()) => LINK_MODE_HARDLINK,
        Err(_) => {
            std::fs::copy(source, &tmp).map_err(SoldrError::Io)?;
            LINK_MODE_COPY
        }
    };

    let source_meta = std::fs::metadata(source).map_err(SoldrError::Io)?;
    // The platform applies publish permissions: fixed 0o755 where exec
    // bits exist, the source's permissions where they don't.
    crate::platform::fs::permissions::make_executable_from(&tmp, &source_meta.permissions())
        .map_err(SoldrError::Io)?;

    if link_mode == LINK_MODE_COPY {
        if let Ok(mtime) = source_meta.modified() {
            std::fs::File::options()
                .write(true)
                .open(&tmp)
                .and_then(|f| f.set_modified(mtime))
                .map_err(SoldrError::Io)?;
        }
    }

    // Concurrent first-use materialization can put several writers here.
    // Publish first so identical winners are normally observed without being
    // removed. For stale targets on Windows (where rename does not replace),
    // remove and retry; bounded rechecks tolerate another writer publishing or
    // removing an identical target between any two filesystem operations.
    const PUBLISH_ATTEMPTS: usize = 20;
    let mut last_error = None;
    for attempt in 0..PUBLISH_ATTEMPTS {
        match std::fs::rename(&tmp, target) {
            Ok(()) => {
                // Re-verify after publication before memoizing. This is
                // intentionally a cold-path hash for cross-volume copies:
                // it closes the source/target mutation window, while every
                // subsequent unchanged call takes the metadata-only memo.
                let verified_snapshot = materialization_memo(source, target);
                if executable_matches(target, source).unwrap_or(false) {
                    if let Some(verified) = verified_snapshot.as_ref() {
                        write_materialization_memo_if_unchanged(
                            source,
                            target,
                            &verified.source,
                            Some(&verified.target),
                        );
                    }
                }
                return Ok(MaterializeResult {
                    created: true,
                    link_mode,
                });
            }
            Err(err) => last_error = Some(err),
        }

        let verified_snapshot = materialization_memo(source, target);
        if executable_matches(target, source).unwrap_or(false) {
            let _ = std::fs::remove_file(&tmp);
            if let Some(verified) = verified_snapshot.as_ref() {
                write_materialization_memo_if_unchanged(
                    source,
                    target,
                    &verified.source,
                    Some(&verified.target),
                );
            }
            return Ok(MaterializeResult {
                created: false,
                link_mode: LINK_MODE_HARDLINK_OR_COPY,
            });
        }

        match swap_stale_target(&tmp, target) {
            Ok(()) => {
                let verified_snapshot = materialization_memo(source, target);
                if executable_matches(target, source).unwrap_or(false) {
                    if let Some(verified) = verified_snapshot.as_ref() {
                        write_materialization_memo_if_unchanged(
                            source,
                            target,
                            &verified.source,
                            Some(&verified.target),
                        );
                    }
                }
                return Ok(MaterializeResult {
                    created: true,
                    link_mode,
                });
            }
            Err(err) => {
                if err.kind() != std::io::ErrorKind::NotFound {
                    last_error = Some(err);
                }
            }
        }
        if attempt + 1 < PUBLISH_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    let verified_snapshot = materialization_memo(source, target);
    if executable_matches(target, source).unwrap_or(false) {
        let _ = std::fs::remove_file(&tmp);
        if let Some(verified) = verified_snapshot.as_ref() {
            write_materialization_memo_if_unchanged(
                source,
                target,
                &verified.source,
                Some(&verified.target),
            );
        }
        return Ok(MaterializeResult {
            created: false,
            link_mode: LINK_MODE_HARDLINK_OR_COPY,
        });
    }
    let _ = std::fs::remove_file(&tmp);
    Err(SoldrError::Io(last_error.unwrap_or_else(|| {
        std::io::Error::other(format!(
            "failed to publish executable shim {}",
            target.display()
        ))
    })))
}

/// Swap `tmp` into `target`, displacing a stale shim that cannot simply be
/// overwritten.
///
/// The previous behaviour deleted the target and let the next loop iteration
/// retry the rename, which left the shim path absent in between. A concurrent
/// build spawning `<shims>/rustc.exe` inside that window fails with a bare
/// "cannot find the path specified" and no compiler diagnostics, so
/// reinstalling `soldr` could break every build running at the time.
///
/// Renaming the stale file aside and immediately publishing narrows the gap to
/// two back-to-back renames, and works in the case that made `remove_file`
/// fail outright: Windows refuses to delete or overwrite a running executable
/// image but does allow renaming it. If publication still fails the stale file
/// is put back, so a failed attempt never leaves the shim missing.
///
/// The displaced file is deleted best-effort — it stays on disk while some
/// process still maps it, and is swept by a later materialization.
fn swap_stale_target(tmp: &Path, target: &Path) -> std::io::Result<()> {
    let aside = stale_path_for(target);
    let _ = std::fs::remove_file(&aside);
    match std::fs::rename(target, &aside) {
        Ok(()) => match std::fs::rename(tmp, target) {
            Ok(()) => {
                let _ = std::fs::remove_file(&aside);
                sweep_stale_siblings(target);
                Ok(())
            }
            Err(err) => {
                // Put the stale shim back rather than leaving a hole.
                let _ = std::fs::rename(&aside, target);
                Err(err)
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => std::fs::rename(tmp, target),
        // The target could not be renamed aside. Fall back to the historical
        // delete so a same-volume stale file still gets replaced rather than
        // failing publication outright.
        Err(_) => {
            std::fs::remove_file(target)?;
            std::fs::rename(tmp, target)
        }
    }
}

/// Best-effort cleanup of displaced shims left behind by earlier publishes in
/// *this* process whose images were still mapped at the time.
///
/// Scoped to our own PID on purpose: another process may be mid-swap and still
/// need to rename its own displaced file back over the target, so deleting it
/// here would turn its recoverable failure into a missing shim.
fn sweep_stale_siblings(target: &Path) {
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return;
    };
    let prefix = {
        let mut p = name.to_os_string();
        p.push(STALE_INFIX);
        p.push(format!("{}-", std::process::id()));
        p.to_string_lossy().into_owned()
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

const STALE_INFIX: &str = ".stale.";

fn stale_path_for(target: &Path) -> PathBuf {
    static STALE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let sequence = STALE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut path = target.as_os_str().to_os_string();
    path.push(STALE_INFIX);
    path.push(format!("{pid}-{nanos}-{sequence}"));
    PathBuf::from(path)
}

fn tmp_path_for(target: &Path) -> PathBuf {
    // Threads in one process can observe the same clock tick, especially on
    // macOS. A colliding fallback copy can otherwise truncate another
    // thread's hardlinked temp file (and therefore the source executable).
    static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let sequence = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let suffix = format!("tmp.{pid}-{nanos}-{sequence}");
    let mut path = target.as_os_str().to_os_string();
    path.push(".");
    path.push(suffix);
    PathBuf::from(path)
}

fn materialization_memo_path(target: &Path) -> PathBuf {
    let mut path = target.as_os_str().to_os_string();
    path.push(".materialized-v1.json");
    PathBuf::from(path)
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    crate::platform::fs::identity::file_identity(path)
}

fn materialization_memo(source: &Path, target: &Path) -> Option<MaterializationMemo> {
    Some(MaterializationMemo {
        version: MATERIALIZATION_MEMO_VERSION,
        source_path: source.as_os_str().to_string_lossy().into_owned(),
        source: file_stamp(source)?,
        target: file_stamp(target)?,
    })
}

fn materialization_memo_matches(source: &Path, target: &Path) -> bool {
    let path = materialization_memo_path(target);
    let Ok(raw) = std::fs::read(path) else {
        return false;
    };
    let Ok(saved) = serde_json::from_slice::<MaterializationMemo>(&raw) else {
        return false;
    };
    materialization_memo(source, target).is_some_and(|current| current == saved)
}

fn write_materialization_memo_if_unchanged(
    source: &Path,
    target: &Path,
    expected_source: &FileStamp,
    expected_target: Option<&FileStamp>,
) -> bool {
    let Some(memo) = materialization_memo(source, target) else {
        return false;
    };
    if &memo.source != expected_source
        || expected_target.is_some_and(|expected| &memo.target != expected)
    {
        return false;
    }
    let Ok(raw) = serde_json::to_vec(&memo) else {
        return false;
    };
    // This is a performance memo, not authoritative state. A partial or
    // racing write cannot produce a false match: parse failure falls back to
    // the content hash, while a complete memo is validated against both
    // files' current metadata before use.
    std::fs::write(materialization_memo_path(target), raw).is_ok()
}

pub(crate) fn executable_matches(target: &Path, source: &Path) -> Result<bool, SoldrError> {
    let Ok(target_meta) = std::fs::metadata(target) else {
        return Ok(false);
    };
    let Ok(source_meta) = std::fs::metadata(source) else {
        return Ok(false);
    };
    if target_meta.len() != source_meta.len() {
        return Ok(false);
    }
    if crate::platform::fs::identity::same_file(target, source) {
        return Ok(true);
    }
    Ok(blake3_file(target)? == blake3_file(source)?)
}

fn blake3_file(path: &Path) -> Result<[u8; 32], SoldrError> {
    zccache::hash::hash_file(path)
        .map(|hash| *hash.as_bytes())
        .map_err(SoldrError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_source(tmp: &tempfile::TempDir, bytes: &[u8]) -> PathBuf {
        let source = tmp.path().join("soldr");
        std::fs::write(&source, bytes).unwrap();
        source
    }

    // A minimal 64-bit LE Mach-O whose load-command region names
    // @loader_path -- i.e. a binary that only runs from its own directory.
    fn position_dependent_source(tmp: &tempfile::TempDir) -> PathBuf {
        let payload = b"\x0c\x00\x00\x00@loader_path/../soldr.dylibs/liblzma.dylib\x00";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xfeed_facfu32.to_le_bytes()); // magic
        bytes.extend_from_slice(&0u32.to_le_bytes()); // cputype
        bytes.extend_from_slice(&0u32.to_le_bytes()); // cpusubtype
        bytes.extend_from_slice(&2u32.to_le_bytes()); // filetype
        bytes.extend_from_slice(&1u32.to_le_bytes()); // ncmds
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // sizeofcmds
        bytes.extend_from_slice(&0u32.to_le_bytes()); // flags
        bytes.extend_from_slice(&0u32.to_le_bytes()); // reserved
        bytes.extend_from_slice(payload);
        let source = tmp.path().join("soldr");
        std::fs::write(&source, bytes).unwrap();
        source
    }

    // A minimal 64-bit LE ELF whose DT_RUNPATH is `$ORIGIN/../soldr.libs` --
    // what maturin's repair writes for a bundled liblzma (soldr#3403).
    fn origin_runpath_elf_source(tmp: &tempfile::TempDir) -> PathBuf {
        let rpath = b"$ORIGIN/../soldr.libs\0";
        let (dyn_offset, dyn_size) = (64u64 + 2 * 56, 3 * 16u64);
        let strtab_offset = dyn_offset + dyn_size;
        let total = strtab_offset + rpath.len() as u64;
        let base = 0x40_0000u64;
        let mut bytes = b"\x7fELF\x02\x01\x01".to_vec();
        bytes.resize(16, 0);
        for (value, width) in [(3u64, 2), (62, 2), (1, 4), (0, 8), (64, 8), (0, 8), (0, 4)] {
            bytes.extend_from_slice(&value.to_le_bytes()[..width]);
        }
        for (value, width) in [(64u64, 2), (56, 2), (2, 2), (0, 6)] {
            bytes.extend_from_slice(&value.to_le_bytes()[..width]);
        }
        for (kind, offset, size) in [(1u32, 0u64, total), (2, dyn_offset, dyn_size)] {
            bytes.extend_from_slice(&kind.to_le_bytes());
            bytes.extend_from_slice(&5u32.to_le_bytes());
            for field in [offset, base + offset, base + offset, size, size, 8] {
                bytes.extend_from_slice(&field.to_le_bytes());
            }
        }
        for (tag, value) in [(5u64, base + strtab_offset), (29, 0), (0, 0)] {
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(rpath);
        let source = tmp.path().join("soldr");
        std::fs::write(&source, bytes).unwrap();
        source
    }

    #[test]
    fn a_position_dependent_source_is_refused_for_the_broker_image() {
        let tmp = tempfile::tempdir().unwrap();
        let elf = origin_runpath_elf_source(&tmp);
        let error = require_relocatable(&elf, "the stable broker image")
            .expect_err("an $ORIGIN rpath cannot be copied to the broker path");
        assert!(
            error.contains("$ORIGIN") && error.contains("broker image"),
            "{error}"
        );

        let plain = tmp.path().join("plain");
        std::fs::write(&plain, b"#!/bin/sh\n").unwrap();
        assert!(require_relocatable(&plain, "the stable broker image").is_ok());
    }

    /// soldr#3831: the verdict is remembered per image content, so a fresh
    /// root does not reparse the whole binary.
    #[test]
    fn loader_path_verdict_is_memoized_by_content() {
        let tmp = tempfile::tempdir().unwrap();
        let elf = origin_runpath_elf_source(&tmp);
        let dir = crate::daemon::image_hash::machine_scoped_cache_dir(&std::env::temp_dir());
        let digest = crate::daemon::image_hash::cached_blake3_hex(&dir, &elf).unwrap();
        let entry = dir.join(format!("{digest}.loader-path"));
        let _ = std::fs::remove_file(&entry);

        assert!(has_loader_path_reference(&elf));
        assert_eq!(std::fs::read(&entry).unwrap(), b"1");

        // A recorded verdict is served without reparsing: flip it and the
        // answer follows the memo, proving the read was skipped.
        std::fs::write(&entry, b"0").unwrap();
        assert!(!has_loader_path_reference(&elf));
        let _ = std::fs::remove_file(&entry);
    }

    #[test]
    fn elf_with_an_origin_runpath_gets_a_trampoline() {
        let tmp = tempfile::tempdir().unwrap();
        let source = origin_runpath_elf_source(&tmp);
        let target = tmp.path().join("shims").join("rustc");

        let result = materialize_executable(&source, &target).unwrap();
        assert_eq!(result.link_mode, LINK_MODE_TRAMPOLINE);
        assert!(std::fs::read_to_string(&target)
            .unwrap()
            .starts_with("#!/bin/sh"));
    }

    // #1908: the guard belongs to materialize_executable, so every writer
    // inherits it. Before this, rustc_wrapper_shim_binary and friends
    // hardlinked the Mach-O straight into the shim dir and dyld aborted it
    // -- and one of them overwrote a correct trampoline written moments
    // earlier by `soldr shims`, so the same path could look right and then
    // break.
    #[test]
    fn position_dependent_source_gets_a_trampoline() {
        let tmp = tempfile::tempdir().unwrap();
        let source = position_dependent_source(&tmp);
        let target = tmp.path().join("shims").join("rustc");

        let result = materialize_executable(&source, &target).unwrap();
        assert!(result.created);
        assert_eq!(result.link_mode, LINK_MODE_TRAMPOLINE);

        let body = std::fs::read_to_string(&target).unwrap();
        assert!(
            body.starts_with("#!/bin/sh"),
            "expected a trampoline: {body}"
        );
        // soldr#1934: this assertion used to demand the opposite -- the tool
        // name inserted ahead of `"$@"` -- and that is precisely what broke
        // every wheel install in 0.8.26. The wrapper contract is positional on
        // argv[1], so nothing may come between the shim and its arguments.
        assert!(
            !body.contains(" rustc \"$@\""),
            "the tool name must not be pushed into argv[1]: {body}"
        );
        assert!(
            body.trim_end().ends_with(" \"$@\""),
            "arguments must be forwarded verbatim: {body}"
        );
        // The identity travels in the environment instead, as the shim's $0.
        assert!(
            body.contains(crate::multicall::SHIM_ARGV0_ENV),
            "trampoline must carry its argv[0] identity: {body}"
        );
        assert!(
            body.contains(&source.to_string_lossy().to_string()),
            "trampoline must exec the source in place: {body}"
        );
    }

    // Idempotency matters here specifically: comparing a small script to a
    // Mach-O always differs, so a naive byte comparison would republish the
    // shim on every cargo invocation and undo the #1831 memo fast path.
    #[test]
    fn trampoline_materialization_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let source = position_dependent_source(&tmp);
        let target = tmp.path().join("shims").join("rustc");

        assert!(materialize_executable(&source, &target).unwrap().created);
        let second = materialize_executable(&source, &target).unwrap();
        assert!(
            !second.created,
            "an unchanged trampoline must not be rewritten"
        );
        assert_eq!(second.link_mode, LINK_MODE_TRAMPOLINE);
    }

    // The reverse direction of the same bug: a stale hardlinked Mach-O
    // sitting where a trampoline belongs must be replaced, not kept.
    #[test]
    fn trampoline_replaces_a_stale_hardlinked_shim() {
        let tmp = tempfile::tempdir().unwrap();
        let source = position_dependent_source(&tmp);
        let target = tmp.path().join("shims").join("rustc");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, std::fs::read(&source).unwrap()).unwrap();

        let result = materialize_executable(&source, &target).unwrap();
        assert!(result.created, "stale Mach-O shim must be replaced");
        assert!(std::fs::read_to_string(&target)
            .unwrap()
            .starts_with("#!/bin/sh"));
    }

    // Ordinary binaries must keep the fast path untouched -- the startup
    // latency work in #1831/#1834 depends on it.
    #[test]
    fn ordinary_source_still_takes_the_hardlink_path() {
        let tmp = tempfile::tempdir().unwrap();
        let source = fake_source(&tmp, b"an ordinary binary mentioning nothing special");
        let target = tmp.path().join("shims").join("cargo");
        let result = materialize_executable(&source, &target).unwrap();
        assert!(result.created);
        assert_ne!(result.link_mode, LINK_MODE_TRAMPOLINE);
    }

    #[test]
    fn materialize_executable_creates_matching_target() {
        let tmp = tempfile::tempdir().unwrap();
        let source = fake_source(&tmp, b"fake-soldr-v1");
        let target = tmp.path().join("cargo");
        let result = materialize_executable(&source, &target).unwrap();
        assert!(result.created);
        assert!([LINK_MODE_HARDLINK, LINK_MODE_COPY].contains(&result.link_mode));
        assert_eq!(std::fs::read(&target).unwrap(), b"fake-soldr-v1");
    }

    #[test]
    fn materialize_executable_is_idempotent_for_matching_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let source = fake_source(&tmp, b"fake-soldr-v1");
        let target = tmp.path().join("rustc");
        assert!(materialize_executable(&source, &target).unwrap().created);
        let second = materialize_executable(&source, &target).unwrap();
        assert!(!second.created);
    }

    #[test]
    fn unchanged_copy_uses_validated_materialization_memo() {
        let tmp = tempfile::tempdir().unwrap();
        let source = fake_source(&tmp, b"fake-soldr-v1");
        let target = tmp.path().join("rustc");
        materialize_executable(&source, &target).unwrap();
        // Force the cross-volume fallback shape: the normal tempdir path
        // creates a hardlink, whose contents naturally change with source.
        std::fs::remove_file(&target).unwrap();
        std::fs::copy(&source, &target).unwrap();
        let copied = materialization_memo(&source, &target).unwrap();
        assert!(write_materialization_memo_if_unchanged(
            &source,
            &target,
            &copied.source,
            Some(&copied.target),
        ));

        assert!(materialization_memo_matches(&source, &target));
        let second = materialize_executable(&source, &target).unwrap();
        assert!(!second.created);

        let original_modified = std::fs::metadata(&source).unwrap().modified().unwrap();
        std::fs::remove_file(&source).unwrap();
        // A different length makes the FileStamp invalidation deterministic
        // even when two writes share one coarse filesystem timestamp.
        std::fs::write(&source, b"fake-soldr-v2-longer").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(original_modified)
            .unwrap();
        assert!(!materialization_memo_matches(&source, &target));
        let replaced = materialize_executable(&source, &target).unwrap();
        assert!(replaced.created);
        assert_eq!(std::fs::read(target).unwrap(), b"fake-soldr-v2-longer");
    }

    #[test]
    fn memo_rejects_source_change_after_content_verification() {
        let tmp = tempfile::tempdir().unwrap();
        let source = fake_source(&tmp, b"fake-soldr-v1");
        let target = tmp.path().join("rustc");
        std::fs::copy(&source, &target).unwrap();
        let verified = materialization_memo(&source, &target).unwrap();
        assert!(executable_matches(&target, &source).unwrap());

        std::fs::remove_file(&source).unwrap();
        // Make the replacement a different length so the test proves a real
        // FileStamp change even on mounts whose timestamp granularity cannot
        // distinguish two immediate writes. Same-size content replacement is
        // covered by `unchanged_copy_uses_validated_materialization_memo`,
        // which restores the original mtime before the content comparison.
        std::fs::write(&source, b"fake-soldr-v2-longer").unwrap();
        assert!(!write_materialization_memo_if_unchanged(
            &source,
            &target,
            &verified.source,
            Some(&verified.target),
        ));
        assert!(!materialization_memo_path(&target).exists());
    }

    #[test]
    fn materialize_executable_accepts_an_identical_concurrent_winner() {
        let tmp = tempfile::tempdir().unwrap();
        let source = fake_source(&tmp, b"fake-soldr-v1");
        let target = tmp.path().join("rustc");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let writers = (0..16)
            .map(|_| {
                let source = source.clone();
                let target = target.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    materialize_executable(&source, &target)
                })
            })
            .collect::<Vec<_>>();

        for writer in writers {
            writer
                .join()
                .expect("materialization writer panicked")
                .expect("concurrent materialization failed");
        }
        assert_eq!(std::fs::read(target).unwrap(), b"fake-soldr-v1");
    }

    #[test]
    fn materialize_executable_replaces_different_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let source = fake_source(&tmp, b"fake-soldr-v1");
        let target = tmp.path().join("rustfmt");
        materialize_executable(&source, &target).unwrap();
        std::fs::remove_file(&source).unwrap();
        std::fs::write(&source, b"fake-soldr-v2").unwrap();
        let replaced = materialize_executable(&source, &target).unwrap();
        assert!(replaced.created);
        assert_eq!(std::fs::read(&target).unwrap(), b"fake-soldr-v2");
    }

    // Regression guard for the concurrent-install failure: reinstalling soldr
    // while a build was running could not replace a shim whose image was
    // mapped by a live `rustc` wrapper. Windows refuses to delete or overwrite
    // a running executable, so the old publish path (`remove_file` + retry)
    // burned all 20 attempts and returned an error, while a cargo process that
    // spawned `<shims>/rustc` in the meantime saw a bare "cannot find the path
    // specified" with no compiler diagnostics. Renaming the running image
    // aside succeeds where deleting it cannot.
    //
    // This test fails against the pre-fix implementation.
    #[test]
    fn replaces_a_shim_whose_image_is_currently_running() {
        if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
            // Only Windows refuses to delete or overwrite a running
            // image; the test fixtures are Windows-only.
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("running-shim.exe");
        // A real executable is required: the point is a mapped image, which no
        // amount of fake bytes reproduces.
        let system_exe = std::path::Path::new(r"C:\Windows\System32\ping.exe");
        std::fs::copy(system_exe, &target).unwrap();

        let mut child = std::process::Command::new(&target)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn shim under test");

        // Sanity: the image really is locked against deletion right now.
        assert!(
            std::fs::remove_file(&target).is_err(),
            "precondition failed: a running image should not be deletable"
        );

        let source = fake_source(&tmp, b"fake-soldr-next-version");
        let result = materialize_executable(&source, &target);

        let _ = child.kill();
        let _ = child.wait();

        let replaced = result.expect("replacing a running shim must succeed");
        assert!(replaced.created);
        assert_eq!(std::fs::read(&target).unwrap(), b"fake-soldr-next-version");
    }

    #[test]
    fn replacing_a_shim_keeps_the_path_readable_for_spawners() {
        let tmp = tempfile::tempdir().unwrap();
        let source = fake_source(&tmp, b"fake-soldr-v1");
        let target = tmp.path().join("rustc");
        materialize_executable(&source, &target).unwrap();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let missing = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let observer = {
            let target = target.clone();
            let stop = std::sync::Arc::clone(&stop);
            let missing = std::sync::Arc::clone(&missing);
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    // A spawner only ever needs *some* shim at this path.
                    if std::fs::metadata(&target).is_err() {
                        missing.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            })
        };

        for generation in 2..40u8 {
            let bytes = format!("fake-soldr-v{generation}");
            std::fs::remove_file(&source).unwrap();
            std::fs::write(&source, bytes.as_bytes()).unwrap();
            materialize_executable(&source, &target).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        observer.join().unwrap();

        assert_eq!(
            missing.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "shim path disappeared while it was being replaced"
        );
    }

    #[test]
    fn swap_stale_target_restores_the_shim_when_publish_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("rustc");
        std::fs::write(&target, b"stale-shim").unwrap();
        // A tmp path that does not exist makes the publishing rename fail.
        let missing_tmp = tmp.path().join("never-created");

        let err = swap_stale_target(&missing_tmp, &target).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"stale-shim",
            "a failed publish must leave the previous shim in place"
        );
    }

    #[test]
    fn stale_paths_are_unique_and_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("rustc");
        let mut paths = std::collections::HashSet::new();
        for _ in 0..256 {
            assert!(paths.insert(stale_path_for(&target)));
        }
        for path in &paths {
            std::fs::write(path, b"leftover").unwrap();
        }
        std::fs::write(&target, b"current").unwrap();
        sweep_stale_siblings(&target);
        for path in &paths {
            assert!(!path.exists(), "stale sibling was not swept: {path:?}");
        }
        assert!(target.exists(), "sweep must not remove the live shim");
    }

    #[test]
    fn temporary_paths_are_unique_within_one_process() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("rustc");
        let mut paths = std::collections::HashSet::new();
        for _ in 0..1_024 {
            assert!(paths.insert(tmp_path_for(&target)));
        }
    }
}
