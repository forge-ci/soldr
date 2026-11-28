//! Prepare a target directory for an unmediated no-cache build.
//!
//! zccache's fastest non-reflink delivery tier is a protected hardlink: the
//! cache blob and requested target path share an inode and are read-only.
//! Mediated compiles copy-detach that output before rustc writes it. A
//! no-cache recovery deliberately removes the wrapper and daemon, so this
//! module performs the same ownership transition locally.
//!
//! Never clear read-only on a shared file. On Unix, unlinking a read-only file
//! is controlled by its parent directory and leaves the cache inode untouched.
//! On Windows, `FileDispositionInfoEx` with
//! `FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE` removes only the opened
//! directory entry without changing the shared file attributes.

use crate::core::{command_output_with_timeout, suppress_windows_console_window, SoldrError};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static DETACH_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const DETACH_TEMP_PREFIX: &str = ".soldr-no-cache-detach-";

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct NoCacheDetachReport {
    pub(super) target_dir: PathBuf,
    pub(super) detached_shared: usize,
    pub(super) made_writable: usize,
    scanned_files: usize,
}

#[derive(Deserialize)]
struct TargetDirectoryMetadata {
    target_directory: PathBuf,
}

pub(super) fn prepare_target_for_unmediated_build(
    tool: &Path,
    args: &[String],
    child_command: &std::process::Command,
) -> Result<NoCacheDetachReport, SoldrError> {
    let target_dir = resolve_target_directory(tool, args, child_command)?;
    detach_target_tree(&target_dir)
}

/// Resolve the target root in the build tool's own order. Explicit CLI/env
/// overrides are cheap and authoritative; otherwise metadata is the only
/// reliable way to honor workspace discovery plus `.cargo/config.toml`
/// `build.target-dir` settings.
fn resolve_target_directory(
    tool: &Path,
    args: &[String],
    child_command: &std::process::Command,
) -> Result<PathBuf, SoldrError> {
    let cargo_target_dir = std::env::var_os("CARGO_TARGET_DIR");
    let test_cargo_bin = std::env::var_os(crate::TEST_CARGO_BIN_ENV_VAR);
    resolve_target_directory_with_env(
        tool,
        args,
        child_command,
        cargo_target_dir.as_deref(),
        test_cargo_bin.as_deref(),
        super::resolve_target_dir_for_hooks,
    )
}

fn resolve_target_directory_with_env(
    tool: &Path,
    args: &[String],
    child_command: &std::process::Command,
    cargo_target_dir: Option<&OsStr>,
    test_cargo_bin: Option<&OsStr>,
    test_default_target: impl FnOnce(&[String]) -> Option<PathBuf>,
) -> Result<PathBuf, SoldrError> {
    if let Some(target_dir) = super::disk::cargo_arg_value(args, "--target-dir") {
        return Ok(super::disk::absolutize_path(PathBuf::from(target_dir)));
    }
    if let Some(target_dir) = cargo_target_dir.filter(|value| !value.is_empty()) {
        return Ok(super::disk::absolutize_path(PathBuf::from(target_dir)));
    }

    // Test Cargo binaries are intentionally tiny argv recorders and need not
    // implement metadata. Use the same inexpensive selected/default target
    // resolution as target-GC hooks only for this explicit seam. Real Cargo
    // always takes the metadata path below, preserving workspace/config rules.
    if test_cargo_bin.is_some() {
        return test_default_target(args).ok_or_else(|| {
            SoldrError::Other(format!(
                "no-cache preflight could not select a target directory for {}",
                crate::TEST_CARGO_BIN_ENV_VAR,
            ))
        });
    }

    let mut probe = std::process::Command::new(tool);
    probe.args(["metadata", "--format-version", "1", "--no-deps"]);
    probe.args(cargo_metadata_passthrough_args(args));
    crate::binaries::apply_resolved_toolchain_homes(&mut probe, tool);
    suppress_windows_console_window(&mut probe);
    probe.env_remove("MAKEFLAGS");
    probe.env_remove("CARGO_MAKEFLAGS");

    // Match the already-prepared child environment. This carries the selected
    // toolchain, SDK variables, and explicit removals without forwarding the
    // build subcommand itself to the metadata probe.
    for (key, value) in child_command.get_envs() {
        if let Some(value) = value {
            probe.env(key, value);
        } else {
            probe.env_remove(key);
        }
    }

    let output = command_output_with_timeout(&mut probe, "metadata probe for no-cache preflight")?;
    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "metadata failed while preparing the no-cache target directory: {}",
            crate::build_cache_session::command_stderr(&output),
        )));
    }
    let metadata: TargetDirectoryMetadata =
        serde_json::from_slice(&output.stdout).map_err(|error| {
            SoldrError::Other(format!(
                "failed to parse metadata for the no-cache target directory: {error}"
            ))
        })?;
    Ok(metadata.target_directory)
}

fn detach_target_tree(target_dir: &Path) -> Result<NoCacheDetachReport, SoldrError> {
    detach_target_tree_with_hook(target_dir, |_| {})
}

struct OpenDirectory {
    dir: Dir,
    relative_path: PathBuf,
    display_path: PathBuf,
}

fn open_target_root(target_dir: &Path) -> Result<Option<OpenDirectory>, SoldrError> {
    let canonical = match std::fs::canonicalize(target_dir) {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(detach_error(
                target_dir,
                "resolve target directory (including symlink/junction roots)",
                error,
            ))
        }
    };
    let dir = Dir::open_ambient_dir(&canonical, ambient_authority())
        .map_err(|error| detach_error(&canonical, "open target directory capability", error))?;
    let metadata = dir
        .dir_metadata()
        .map_err(|error| detach_error(&canonical, "inspect opened target directory", error))?;
    if !metadata.is_dir() {
        return Err(SoldrError::Other(format!(
            "no-cache preflight target is not a directory: {}",
            canonical.display(),
        )));
    }
    Ok(Some(OpenDirectory {
        dir,
        relative_path: PathBuf::new(),
        display_path: canonical,
    }))
}

fn detach_target_tree_with_hook(
    target_dir: &Path,
    mut after_directory_open: impl FnMut(&Path),
) -> Result<NoCacheDetachReport, SoldrError> {
    let Some(root) = open_target_root(target_dir)? else {
        return Ok(NoCacheDetachReport {
            target_dir: target_dir.to_path_buf(),
            ..NoCacheDetachReport::default()
        });
    };
    let mut report = NoCacheDetachReport {
        target_dir: root.display_path.clone(),
        ..NoCacheDetachReport::default()
    };

    // Retain every acquired Cargo lock until the complete ownership
    // transition is done. Releasing each successful probe immediately would
    // leave a window for another Cargo process to start during the scan.
    let _build_lock_guards = acquire_build_locks(&root)?;
    let mut pending = vec![root];
    while let Some(directory) = pending.pop() {
        for name in snapshot_directory(&directory, "read target directory")? {
            if is_detach_temp(&name) {
                continue;
            }
            let display_path = directory.display_path.join(&name);
            let metadata = match directory.dir.symlink_metadata(&name) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(detach_error(&display_path, "inspect target entry", error))
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                let Some(child) = open_child_directory(&directory, &name)? else {
                    continue;
                };
                after_directory_open(&child.display_path);
                pending.push(child);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            report.scanned_files += 1;
            match prepare_file(&directory, &name)? {
                PreparedFile::Unchanged => {}
                PreparedFile::DetachedShared => report.detached_shared += 1,
                PreparedFile::MadeWritable => report.made_writable += 1,
            }
        }
    }
    Ok(report)
}

fn open_child_directory(
    parent: &OpenDirectory,
    name: &OsStr,
) -> Result<Option<OpenDirectory>, SoldrError> {
    let display_path = parent.display_path.join(name);
    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    let file = match parent.dir.open_with(name, &options) {
        Ok(file) => file.into_std(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_error) if entry_is_now_symlink(&parent.dir, Path::new(name)) => return Ok(None),
        Err(error) => {
            return Err(detach_error(
                &display_path,
                "open target subdirectory",
                error,
            ))
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| detach_error(&display_path, "inspect target subdirectory", error))?;
    if !metadata.is_dir() || is_reparse_or_symlink(&metadata) {
        return Ok(None);
    }
    let dir = Dir::from_std_file(file);
    Ok(Some(OpenDirectory {
        dir,
        relative_path: parent.relative_path.join(name),
        display_path,
    }))
}

fn snapshot_directory(
    directory: &OpenDirectory,
    operation: &str,
) -> Result<Vec<OsString>, SoldrError> {
    let entries = match directory.dir.entries() {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(detach_error(&directory.display_path, operation, error)),
    };
    let mut names = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => names.push(entry.file_name()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(detach_error(
                    &directory.display_path,
                    "read target entry",
                    error,
                ))
            }
        }
    }
    names.sort_unstable();
    Ok(names)
}

fn is_detach_temp(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with(DETACH_TEMP_PREFIX))
}

/// Discover locks first, then acquire them in a stable order and retain the
/// file guards through the full detach. Refusing is safer than racing an
/// active compiler while replacing its outputs.
fn acquire_build_locks(
    root: &OpenDirectory,
) -> Result<Vec<kernal_api::platform::fs::OwnedFileLock>, SoldrError> {
    let mut pending = vec![OpenDirectory {
        dir: root
            .dir
            .try_clone()
            .map_err(|error| detach_error(&root.display_path, "clone target capability", error))?,
        relative_path: PathBuf::new(),
        display_path: root.display_path.clone(),
    }];
    let mut lock_paths = Vec::new();
    while let Some(directory) = pending.pop() {
        for name in snapshot_directory(&directory, "inspect build locks")? {
            if is_detach_temp(&name) {
                continue;
            }
            let display_path = directory.display_path.join(&name);
            let metadata = match directory.dir.symlink_metadata(&name) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(detach_error(
                        &display_path,
                        "inspect build lock entry",
                        error,
                    ))
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                if let Some(child) = open_child_directory(&directory, &name)? {
                    pending.push(child);
                }
            } else if metadata.is_file() && name == std::ffi::OsStr::new(".cargo-lock") {
                lock_paths.push(directory.relative_path.join(name));
            }
        }
    }
    lock_paths.sort_unstable();

    let mut guards = Vec::with_capacity(lock_paths.len());
    for path in lock_paths {
        let display_path = root.display_path.join(&path);
        let file = match open_lock_no_follow(&root.dir, &path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_error) if entry_is_now_symlink(&root.dir, &path) => continue,
            Err(error) => return Err(detach_error(&display_path, "open build lock", error)),
        };
        match kernal_api::platform::fs::try_lock_exclusive_owned(file) {
            Ok(guard) => guards.push(guard),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                return Err(active_lock_error(&root.display_path, &display_path));
            }
            Err(error) => return Err(detach_error(&display_path, "acquire build lock", error)),
        }
    }
    Ok(guards)
}

fn active_lock_error(target_dir: &Path, lock_path: &Path) -> SoldrError {
    SoldrError::Other(format!(
        "no-cache preflight refused to modify {} while build lock {} is active",
        target_dir.display(),
        lock_path.display(),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedFile {
    Unchanged,
    DetachedShared,
    MadeWritable,
}

fn prepare_file(parent: &OpenDirectory, name: &OsStr) -> Result<PreparedFile, SoldrError> {
    prepare_file_with_final_rename(parent, name, |directory, temp_name, final_name| {
        directory.rename(temp_name, directory, final_name)
    })
}

/// Detach an explicit set of declared compiler outputs (issue #1817).
///
/// The whole-tree [`prepare_target_for_unmediated_build`] preflight only runs
/// when the finalized plan has no managed zccache session. A build that starts
/// *with* a session and loses the daemon mid-run skips it, then launches a
/// direct compiler against outputs that are still protected read-only
/// hardlinks — which rustc reports as `<file> is not writeable`.
///
/// This is the output-scoped counterpart. It deliberately does **not** walk the
/// target tree or take the Cargo build lock: the late transition happens inside
/// a running Cargo build that already owns that lock, and scanning the whole
/// target once per compiler process would be quadratic.
///
/// Paths are grouped by parent so each directory capability is opened once for
/// a whole output family rather than once per file. Missing paths are fine —
/// a first compile has nothing to detach.
pub(crate) fn detach_declared_outputs(paths: &[PathBuf]) -> Result<DetachSummary, SoldrError> {
    let mut summary = DetachSummary::default();
    let mut by_parent: Vec<(PathBuf, Vec<OsString>)> = Vec::new();
    for path in paths {
        let Some(parent) = path.parent() else {
            continue;
        };
        let Some(name) = path.file_name() else {
            continue;
        };
        match by_parent.iter_mut().find(|(dir, _)| dir == parent) {
            Some((_, names)) => names.push(name.to_os_string()),
            None => by_parent.push((parent.to_path_buf(), vec![name.to_os_string()])),
        }
    }
    for (parent, names) in by_parent {
        // A not-yet-created out-dir means nothing has been materialized into
        // it, so there is nothing protected to detach.
        let Some(directory) = open_target_root(&parent)? else {
            continue;
        };
        for name in names {
            match prepare_file(&directory, &name)? {
                PreparedFile::Unchanged => {}
                PreparedFile::DetachedShared => summary.detached_shared += 1,
                PreparedFile::MadeWritable => summary.made_writable += 1,
            }
        }
    }
    Ok(summary)
}

/// Outcome of [`detach_declared_outputs`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DetachSummary {
    /// Outputs that shared an inode with a cache blob and were copy-detached.
    pub(crate) detached_shared: usize,
    /// Outputs that were already private but read-only.
    pub(crate) made_writable: usize,
}

impl DetachSummary {
    /// True when at least one output actually needed an ownership transition.
    pub(crate) fn changed_anything(self) -> bool {
        self.detached_shared > 0 || self.made_writable > 0
    }
}

/// Make one target file safe to replace without mutating a shared cache blob.
///
/// The fallback-output migration uses this before publishing filtered
/// diagnostics. In particular, the Windows path needs the same
/// `FileDispositionInfoEx` handling as the full no-cache preflight when the
/// target entry is a protected read-only hardlink.
pub(super) fn prepare_path_for_replacement(path: &Path) -> Result<(), SoldrError> {
    let parent = path.parent().ok_or_else(|| {
        SoldrError::Other(format!(
            "cannot prepare target file without a parent: {}",
            path.display()
        ))
    })?;
    let name = path.file_name().ok_or_else(|| {
        SoldrError::Other(format!(
            "cannot prepare target file without a name: {}",
            path.display()
        ))
    })?;
    let directory = open_target_root(parent)?.ok_or_else(|| {
        SoldrError::Other(format!(
            "target file parent disappeared while preparing {}",
            path.display()
        ))
    })?;
    let _ = prepare_file(&directory, name)?;
    Ok(())
}

fn prepare_file_with_final_rename(
    parent: &OpenDirectory,
    name: &OsStr,
    final_rename: impl FnOnce(&Dir, &OsStr, &OsStr) -> std::io::Result<()>,
) -> Result<PreparedFile, SoldrError> {
    let display_path = parent.display_path.join(name);
    let mut source = match open_source_for_detach(&parent.dir, Path::new(name)) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PreparedFile::Unchanged)
        }
        Err(_error) if entry_is_now_symlink(&parent.dir, Path::new(name)) => {
            return Ok(PreparedFile::Unchanged)
        }
        Err(error) => return Err(detach_error(&display_path, "open target file", error)),
    };
    let metadata = source
        .metadata()
        .map_err(|error| detach_error(&display_path, "inspect target file", error))?;
    if !metadata.is_file() || is_reparse_or_symlink(&metadata) {
        return Ok(PreparedFile::Unchanged);
    }
    let links = hard_link_count(&source, &metadata)
        .map_err(|error| detach_error(&display_path, "read target hard-link count", error))?;
    if links <= 1 {
        if metadata.permissions().readonly() {
            make_file_writable(&source, &metadata).map_err(|error| {
                detach_error(&display_path, "make private target file writable", error)
            })?;
            return Ok(PreparedFile::MadeWritable);
        }
        return Ok(PreparedFile::Unchanged);
    }

    let (temp_name, mut temp) = create_private_temp(&parent.dir, name)
        .map_err(|error| detach_error(&display_path, "create detach temporary file", error))?;
    let prepare_result = (|| {
        std::io::copy(&mut source, &mut temp)?;
        temp.flush()?;
        set_private_permissions(&temp, &metadata)?;
        // Preserve the shared output's mtime on its private copy, mtime only
        // as `zccache::core::mtime` does (#3462). Deliberately through the
        // open handle rather than `stamp_mtime`, which takes a path: this
        // module resolves every entry relative to a held directory
        // capability, and re-opening the temp by path would reintroduce the
        // symlink race that design exists to close.
        filetime::set_file_handle_times(
            &temp,
            None,
            Some(filetime::FileTime::from_last_modification_time(&metadata)),
        )?;
        temp.sync_all()?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(error) = prepare_result {
        drop(temp);
        let _ = parent.dir.remove_file(&temp_name);
        return Err(detach_error(
            &display_path,
            "detach shared target file",
            error,
        ));
    }

    if let Err(error) = remove_shared_alias(&parent.dir, name, source) {
        drop(temp);
        let _ = parent.dir.remove_file(&temp_name);
        return Err(detach_error(
            &display_path,
            "detach shared target file",
            error,
        ));
    }

    // The original directory entry no longer exists. From this point onward,
    // the temporary is the sole private copy and must never be cleaned up on
    // error. The capability-relative rename is atomic when it succeeds; when
    // it fails, retain the copy under its exact reported recovery path.
    drop(temp);
    if let Err(error) = final_rename(&parent.dir, &temp_name, name) {
        let preserved_path = parent.display_path.join(&temp_name);
        return Err(SoldrError::Other(format!(
            "no-cache preflight could not finalize detached target file {}: {error}; the shared alias was removed and the private copy was preserved at {}",
            display_path.display(),
            preserved_path.display(),
        )));
    }
    Ok(PreparedFile::DetachedShared)
}

fn create_private_temp(parent: &Dir, name: &OsStr) -> std::io::Result<(OsString, File)> {
    let name = name.to_string_lossy();
    let mut last_error = None;
    for _ in 0..32 {
        let counter = DETACH_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_name = OsString::from(format!(
            "{DETACH_TEMP_PREFIX}{}-{counter}-{name}",
            std::process::id(),
        ));
        let mut options = CapOpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        match parent.open_with(&temp_name, &options) {
            Ok(file) => return Ok((temp_name, file.into_std())),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "exhausted no-cache detach temporary names",
        )
    }))
}

fn detach_error(path: &Path, operation: &str, error: std::io::Error) -> SoldrError {
    SoldrError::Other(format!(
        "no-cache preflight could not {operation} {}: {error}",
        path.display(),
    ))
}

fn entry_is_now_symlink(parent: &Dir, name: &Path) -> bool {
    parent
        .symlink_metadata(name)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn open_cap_file_no_follow(parent: &Dir, name: &Path, write: bool) -> std::io::Result<File> {
    let mut options = CapOpenOptions::new();
    options.read(true).write(write).follow(FollowSymlinks::No);
    parent.open_with(name, &options).map(|file| file.into_std())
}

fn open_source_for_detach(parent: &Dir, name: &Path) -> std::io::Result<File> {
    let initial = open_cap_file_no_follow(parent, name, false)?;
    // The platform crate upgrades the already-resolved handle (Windows
    // ReOpenFile with delete access + share-delete; a no-op elsewhere)
    // rather than resolving an ambient path again, preserving the
    // beneath-root guarantee.
    crate::platform::fs::replace::open_for_retire(initial)
}

fn open_lock_no_follow(parent: &Dir, name: &Path) -> std::io::Result<File> {
    let file = open_cap_file_no_follow(parent, name, true)?;
    if is_reparse_or_symlink(&file.metadata()?) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "build lock is a reparse point",
        ));
    }
    Ok(file)
}

fn is_reparse_or_symlink(metadata: &std::fs::Metadata) -> bool {
    crate::platform::fs::links::is_link_or_reparse(metadata)
}

fn hard_link_count(source: &File, _metadata: &std::fs::Metadata) -> std::io::Result<u64> {
    crate::platform::fs::links::hard_link_count(source)
}

fn set_private_permissions(file: &File, source: &std::fs::Metadata) -> std::io::Result<()> {
    crate::platform::fs::permissions::make_writable_like(file, &source.permissions())
}

fn make_file_writable(file: &File, metadata: &std::fs::Metadata) -> std::io::Result<()> {
    set_private_permissions(file, metadata)
}

fn remove_shared_alias(parent: &Dir, name: &OsStr, source: File) -> std::io::Result<()> {
    // The platform crate retires the file through its own handle on
    // Windows (FileDispositionInfoEx POSIX-delete — the only mechanism
    // that removes a still-mapped image); elsewhere it drops the handle
    // and runs this capability-safe plain remove.
    crate::platform::fs::replace::retire_open_file(source, || parent.remove_file(name))
}

#[cfg(test)]
mod tests;

// soldr#2996: relocated from the deleted `rust_plan` module, whose target
// cache was removed. This detach probe is the only remaining consumer.
fn cargo_metadata_passthrough_args(args: &[String]) -> Vec<std::ffi::OsString> {
    let mut values = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        match arg.as_str() {
            "--locked" | "--offline" | "--frozen" | "--all-features" | "--no-default-features" => {
                values.push(arg.as_str().into())
            }
            "--manifest-path" | "--config" | "--features" | "--filter-platform" => {
                if let Some(value) = iter.next() {
                    values.push(arg.as_str().into());
                    values.push(value.as_str().into());
                }
            }
            _ => {
                for flag in [
                    "--manifest-path=",
                    "--config=",
                    "--features=",
                    "--filter-platform=",
                ] {
                    if arg.starts_with(flag) {
                        values.push(arg.as_str().into());
                    }
                }
            }
        }
    }
    values
}
