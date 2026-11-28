//! macOS per-user directory placement for product runtime artifacts.

use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

#[cfg(test)]
#[path = "fs_walk_link_tests.rs"]
mod walk_link_tests;

#[cfg(test)]
#[path = "fs_private_acl_tests.rs"]
mod private_acl_tests;

#[path = "fs_private_acl.rs"]
mod private_acl;
pub use private_acl::ensure_no_extended_acl;

/// Directory for `product`'s ephemeral runtime artifacts (sockets, pid files).
///
/// macOS ships no `XDG_RUNTIME_DIR`, but honours it when a caller's
/// environment sets one. The fallback qualifies `/tmp` with the caller's uid,
/// because `/tmp` is shared and two accounts must not land on one directory.
pub fn user_runtime_dir(product: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join(product);
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/{product}-{uid}"))
}

/// Directory for `product`'s persistent state (databases that outlive a boot).
pub fn user_state_dir(product: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(dir).join(product)
    } else if let Some(home) = dirs::home_dir() {
        home.join(".local/state").join(product)
    } else {
        PathBuf::from(format!("/tmp/{product}-state"))
    }
}

/// Root under which `product` keeps per-run scratch data.
///
/// macOS gives each user a `Library/Caches` for exactly this class of data,
/// which is where it belongs rather than beside the sockets.
pub fn user_run_data_root(product: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Library/Caches")
        .join(product)
}

/// Stable identity of an open file on this host.
///
/// Two paths that resolve to the same bytes on disk report the same identity,
/// which is what lets a caller notice that the file it opened has since been
/// replaced. The two fields are whatever this host uses to say that: a device
/// and inode, a volume serial and file index, or an equivalent pair. Callers
/// compare them; they do not interpret them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    /// Device, volume, or platform-equivalent file namespace.
    pub device: u64,
    /// Inode, file index, or platform-equivalent file number.
    pub file: u64,
}

/// Identity of an already-open file.
pub fn file_identity(file: &File) -> io::Result<Option<FileIdentity>> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file.metadata()?;
    Ok(Some(FileIdentity {
        device: metadata.dev(),
        file: metadata.ino(),
    }))
}

/// Identity of the file a path currently names.
pub fn path_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = path.metadata()?;
    Ok(Some(FileIdentity {
        device: metadata.dev(),
        file: metadata.ino(),
    }))
}

/// Open `path` for use as an advisory lock file, creating it if absent.
///
/// The mode matters as much as the open: a lock file another account can
/// rewrite is not a lock. Unix answers that with owner-only permissions.
pub fn open_lock_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Never truncate: an existing lock file may be held right now,
        // and its contents are not ours to clear.
        .truncate(false)
        .mode(0o600)
        .open(path)
}

/// Take an exclusive advisory lock without waiting.
///
/// Returns immediately when another holder has it; the caller decides whether
/// that is a conflict worth retrying, via [`is_lock_conflict`].
pub fn try_lock_exclusive(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd as _;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Take an exclusive advisory lock, waiting until it is available.
pub fn lock_exclusive(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd as _;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Take a shared advisory lock, waiting until it is available.
pub fn lock_shared(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd as _;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Take a shared advisory lock without waiting.
///
/// Returns immediately when an exclusive holder has it; the caller decides
/// whether that is a conflict worth retrying, via [`is_lock_conflict`].
pub fn try_lock_shared(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd as _;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Release a lock taken by [`try_lock_exclusive`], [`try_lock_shared`],
/// [`lock_exclusive`], or [`lock_shared`].
pub fn unlock(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd as _;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Set the modification time of the file at `path`, without disturbing its
/// access time.
///
/// This calls `utimensat` directly on the path rather than opening a file
/// handle: the permission this needs is ownership of the file (or an
/// equivalent privilege), not a write-mode open, so this succeeds even on a
/// file this process has marked read-only -- the state a content-addressed
/// cache commonly leaves an entry in. `UTIME_OMIT` for the access-time slot
/// is what leaves it untouched. macOS has supported `utimensat` and
/// `UTIME_OMIT` since 10.13; this crate does not support older releases.
pub fn set_file_mtime(
    path: &Path,
    seconds_since_unix_epoch: i64,
    nanoseconds: u32,
) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    // On our supported 64-bit targets `time_t` already is `i64`; the
    // cast stays for portability to a target where it is narrower.
    #[allow(clippy::unnecessary_cast)]
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: seconds_since_unix_epoch as libc::time_t,
            tv_nsec: nanoseconds as _,
        },
    ];
    // SAFETY: `c_path` is a NUL-terminated path alive for the call, and
    // `times` is a valid two-element array of the expected type.
    let result = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether `error` means "someone else holds it", rather than a real failure.
///
/// Hosts spell this differently and callers must not have to know which; the
/// distinction decides whether waiting is worthwhile.
pub fn is_lock_conflict(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::EWOULDBLOCK) || error.raw_os_error() == Some(libc::EAGAIN)
}

/// Encode `path` as the bytes this host uses to spell it.
///
/// Faithful, not canonical: this is the encoding a path is carried in so the
/// other end can reconstruct exactly the path that was named. It is
/// deliberately not `ipc::endpoint_scope_bytes`, which folds away differences
/// a host considers meaningless in order to hash two spellings to one identity.
/// Round-tripping through the pair here must return the original path;
/// round-tripping through that one need not.
pub fn encode_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;

    path.as_os_str().as_bytes().to_vec()
}

/// Reconstruct a path from [`encode_path_bytes`] output produced on this host.
pub fn decode_path_bytes(bytes: &[u8]) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;

    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
}

/// Directory for `product`'s shared application data.
///
/// Distinct from [`user_state_dir`]: state is this machine's private
/// bookkeeping, while this is the data a user expects to follow their account.
/// macOS keeps it under `Application Support`.
pub fn user_data_dir(product: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Library")
        .join("Application Support")
        .join(product)
}

/// The directory this host keeps a product's *configuration* in.
///
/// The same location as [`user_data_dir`] here. macOS does not separate the
/// two, and inventing a split would put files where no macOS user or tool
/// looks for them.
pub fn user_config_dir(product: &str) -> PathBuf {
    user_data_dir(product)
}

/// Move `tmp` onto `target`, replacing it, without a window where neither is
/// readable.
pub fn replace_file(tmp: &Path, target: &Path) -> io::Result<()> {
    std::fs::rename(tmp, target)
}

/// Make a directory entry created by [`replace_file`] durable.
///
/// A rename is only as durable as the directory recording it, which Unix does
/// not flush with the file.
pub fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

/// Open an append-only file without truncating the bytes already in it.
///
/// A POSIX open claims no exclusion, so nothing here has to ask for sharing;
/// the Windows tree does, which is why this capability is spelled the same on
/// every host but implemented per tree.
pub fn open_shared_append(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// Tighten a directory that group or others can write, reporting what it did.
///
/// `Ok(false)` means it was already private, `Ok(true)` that it was tightened,
/// and `Err` that it is still exposed afterwards -- the three outcomes the
/// Windows tree reports from its DACL, expressed here in mode bits.
///
/// A sticky directory is left alone. The sticky bit is this host's marker for
/// a *shared* root -- `/tmp` and `/var/tmp` are `1777` by design -- so
/// tightening one would be wrong for every other process on the machine, and
/// as root it would succeed.
pub fn ensure_dir_private(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = std::fs::metadata(path)?;
    let full_mode = metadata.permissions().mode();
    const STICKY: u32 = 0o1000;
    if full_mode & STICKY != 0 {
        return Ok(false);
    }
    if full_mode & 0o022 == 0 {
        return Ok(false);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    // Read back rather than trusting the write: a filesystem that does not
    // carry modes can report success and keep the old permissions.
    let after = std::fs::metadata(path)?;
    if after.permissions().mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is writable by others and could not be tightened",
                path.display()
            ),
        ));
    }
    Ok(true)
}

/// Create `path` and any missing parents owner-only from the moment they exist.
///
/// The mode goes to `mkdir(2)` itself, so no directory is ever briefly visible
/// with a mode inherited from its parent -- the window the Windows tree has to
/// close by passing a descriptor to `CreateDirectoryW`.
pub fn create_dir_all_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Create a new file that only its owner can read, failing if it exists.
///
/// `create_new` is part of the contract, not a convenience: a private file
/// opened over one that already exists inherits whatever that one allowed.
/// Unix sets the mode at creation, so there is no window where the file exists
/// with broader permissions.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Secure, bounded private-file read for the facade.
pub fn read_private_regular_file_bounded(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

    let parent = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "private file path has no parent"))?;
    let parent_metadata = parent.metadata()?;
    // SAFETY: `geteuid` has no preconditions and only reads this process's credentials.
    if !parent_metadata.is_dir() || parent_metadata.uid() != unsafe { libc::geteuid() } || parent_metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "private file parent is not current-user private"));
    }
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "private input is not a regular file"));
    }
    // SAFETY: `geteuid` has no preconditions and only reads this process's credentials.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "private input is not current-user private"));
    }
    let bound_plus_one = max_bytes.checked_add(1).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "private file limit overflows"))?;
    if metadata.len() > max_bytes as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "private input exceeds limit"));
    }
    let mut bytes = Vec::with_capacity(bound_plus_one);
    (&mut &file)
        .take(bound_plus_one as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "private input exceeds limit"));
    }
    if path_identity(path)? != file_identity(&file)? {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "private input path changed while it was read"));
    }
    Ok(bytes)
}

/// Bounded ordinary regular-file observation for the public context facade.
pub fn read_context_regular_file_bounded(
    path: &Path,
    max_bytes: usize,
) -> io::Result<crate::platform::fs::ContextFileObservation> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    let bound_plus_one = max_bytes.checked_add(1).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "context file limit overflows"))?;
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "context input final component is a symbolic link"));
    }
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let before = file.metadata()?;
    if !before.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "context input is not a regular file"));
    }
    let identity = file_identity(&file)?.ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "context file identity is unavailable"))?;
    let before_modified = before.modified()?;
    if before.len() > max_bytes as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "context input exceeds limit"));
    }
    let mut bytes = Vec::with_capacity(bound_plus_one);
    (&mut &file).take(bound_plus_one as u64).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "context input exceeds limit"));
    }
    let after = file.metadata()?;
    let after_identity = file_identity(&file)?.ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "context file identity is unavailable"))?;
    if after_identity != identity || after.len() != before.len() || after.modified()? != before_modified || bytes.len() as u64 != after.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "context input changed while it was read"));
    }
    let path_metadata = std::fs::symlink_metadata(path)?;
    if !path_metadata.is_file()
        || (FileIdentity {
            device: path_metadata.dev(),
            file: path_metadata.ino(),
        }) != identity
    {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "context input path changed while it was read"));
    }
    Ok(crate::platform::fs::ContextFileObservation { bytes, metadata: crate::platform::fs::context_regular_file_metadata(&after, identity)? })
}

#[cfg(test)]
mod private_directory_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    /// A group- or other-writable directory is tightened to owner-only.
    ///
    /// This is the mode-bit half of the contract, asserted where the mode bits
    /// are the implementation: the facade test next to `ensure_dir_private`
    /// can only state the host-neutral property.
    #[test]
    fn an_exposed_directory_is_tightened_to_owner_only() {
        let root = tempfile::tempdir().expect("temp root");
        let path = root.path().join("exposed");
        std::fs::create_dir(&path).expect("create");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).expect("expose");

        assert!(ensure_dir_private(&path).expect("tighten"), "it was exposed");
        assert_eq!(mode_of(&path), 0o700);
    }

    /// An already-private directory is reported as such and left untouched.
    #[test]
    fn an_owner_only_directory_is_left_alone() {
        let root = tempfile::tempdir().expect("temp root");
        let path = root.path().join("private");
        std::fs::create_dir(&path).expect("create");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("tighten");

        assert!(!ensure_dir_private(&path).expect("inspect"));
        assert_eq!(mode_of(&path), 0o700);
    }

    /// A sticky directory is a shared root and is never tightened.
    ///
    /// `/tmp` is `1777` by design. Tightening one would break every other
    /// process on the machine, and as root it would succeed -- so the sticky
    /// bit is checked before the write, not after it fails.
    #[test]
    fn a_sticky_shared_root_is_not_tightened() {
        let root = tempfile::tempdir().expect("temp root");
        let path = root.path().join("shared");
        std::fs::create_dir(&path).expect("create");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o1777)).expect("sticky");

        assert!(!ensure_dir_private(&path).expect("inspect sticky"));
        assert_eq!(mode_of(&path), 0o777, "the shared root keeps its mode");
    }

    /// Every directory the create path makes is owner-only, parents included.
    ///
    /// The mode goes to `mkdir(2)`, so this also asserts there is no window
    /// where a parent exists with an inherited mode.
    #[test]
    fn created_directories_and_their_parents_are_owner_only() {
        let root = tempfile::tempdir().expect("temp root");
        let outer = root.path().join("outer");
        let inner = outer.join("inner");

        create_dir_all_private(&inner).expect("create");

        assert_eq!(mode_of(&inner), 0o700);
        assert_eq!(mode_of(&outer), 0o700);
    }
}
