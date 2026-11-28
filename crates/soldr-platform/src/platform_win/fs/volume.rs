//! Windows volume identity and free-space probes.

use std::io;
use std::path::Path;

/// Volume identity for `path`: the uppercase drive letter (`C`, `D`), or
/// `None` when no drive letter can be derived.
pub fn identity(path: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = canonical.to_string_lossy().to_string();
    // Strip UNC prefix \\?\ if present.
    let trimmed = s.trim_start_matches(r"\\?\");
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Some((bytes[0] as char).to_ascii_uppercase().to_string());
    }
    None
}

/// Free space on the volume containing `path`.
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    kernal_api::platform::resources::available_space(path)
}
