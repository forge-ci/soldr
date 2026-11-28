//! Owned native temporary directories; cache publication is caller policy.

use std::{
    io,
    path::{Path, PathBuf},
};

/// Maximum UTF-8 prefix length, excluding the 16-character generated suffix.
pub const MAX_TEMP_PREFIX_BYTES: usize = 128;

/// A newly created, exclusively owned directory with best-effort recursive
/// cleanup on drop. Creation uses the existing private temporary-file backend,
/// with at most 65536 collision attempts and a bounded generated name.
///
/// Native creation/cleanup are synchronous and have no wall-clock or
/// cancellation guarantee. Do not drop a large populated directory on a
/// latency-sensitive async thread. Use [`Self::close`] to observe cleanup
/// errors, or [`Self::persist`] to transfer responsibility explicitly.
///
/// The selected parent must be trusted. This is not a filesystem sandbox:
/// external cleaners, renamed/replaced paths and hostile parent-directory
/// mutations can invalidate ownership assumptions. Temporary names are not
/// security tokens. Close child file handles before cleanup on Windows.
#[derive(Debug)]
pub struct TemporaryDirectory {
    inner: tempfile::TempDir,
}

impl TemporaryDirectory {
    /// Create under the operating system's temporary directory.
    ///
    /// # Errors
    /// Reports native creation failures. No existing directory is overwritten.
    pub fn new() -> io::Result<Self> {
        Self::in_directory(&std::env::temp_dir(), "kernal-")
    }

    /// Create directly under an existing caller-selected parent, preserving
    /// same-filesystem staging. Relative parents become absolute at creation.
    /// The prefix is UTF-8, at most 128 bytes, and cannot contain separators,
    /// NUL or Windows filename/drive metacharacters. Empty prefixes are allowed.
    ///
    /// # Errors
    /// Invalid prefixes fail before any filesystem operation. Native errors,
    /// including a missing/non-directory parent, are returned unchanged.
    pub fn in_directory(parent: &Path, prefix: &str) -> io::Result<Self> {
        if prefix.len() > MAX_TEMP_PREFIX_BYTES
            || matches!(prefix, "." | "..")
            || prefix
                .chars()
                .any(|ch| ch.is_control() || "\\/<>:\"|?*".contains(ch))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid temporary-directory prefix",
            ));
        }
        let mut builder = tempfile::Builder::new();
        builder.prefix(prefix).rand_bytes(16);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let inner = builder.tempdir_in(parent)?;
        Ok(Self { inner })
    }

    /// The absolute owned directory path. Do not replace or rename it while
    /// this guard owns cleanup; transfer ownership first when publishing.
    pub fn path(&self) -> &Path {
        self.inner.path()
    }

    /// Transfer the existing path to the caller, disabling automatic cleanup.
    #[must_use]
    pub fn persist(self) -> PathBuf {
        self.inner.keep()
    }

    /// Recursively remove the owned directory, reporting native errors.
    /// On failure, remaining contents are not retried automatically; save
    /// `path().to_path_buf()` first if explicit recovery is needed.
    ///
    /// # Errors
    /// Reports filesystem removal errors, including Windows sharing violations.
    pub fn close(self) -> io::Result<()> {
        self.inner.close()
    }
}

impl AsRef<Path> for TemporaryDirectory {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}
