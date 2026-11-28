//! Linux volume identity and free-space probes.

use std::io;
use std::path::Path;

/// Volume identity for `path`: the device id from `stat()`.
pub fn identity(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let meta = std::fs::metadata(&canonical).ok()?;
    Some(meta.dev().to_string())
}

/// Free space on the volume containing `path`.
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    kernal_api::platform::resources::available_space(path)
}
