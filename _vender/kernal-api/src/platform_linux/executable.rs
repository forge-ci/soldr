//! linux executable naming and image-relative discovery.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// File-name extension the host requires on a runnable image, if any.
pub const EXECUTABLE_EXTENSION: Option<&str> = None;

/// Spell `bare` the way this host names an executable file.
///
/// Callers name the *program*; the host decides whether that program is a file
/// called `bare` or `bare.exe`. Only the file spelling changes here — PATH
/// search order and `PATHEXT` are search concerns, not naming ones.
pub fn file_name(bare: &str) -> String {
    match EXECUTABLE_EXTENSION {
        Some(extension) => format!("{bare}.{extension}"),
        None => bare.to_owned(),
    }
}

/// Spell `bare` as this host names an executable, keeping native bytes.
///
/// Nothing is appended here: this host's runnable images carry no suffix.
pub fn file_name_os(bare: &OsStr) -> OsString {
    bare.to_os_string()
}

/// Path to a sibling program installed beside the running image.
///
/// Returns `None` when the current image cannot be resolved, has no parent
/// directory, or the sibling is not a file — all of which mean the same thing
/// to a caller: this program is not installed next to us, look elsewhere.
pub fn sibling_of_current_image(bare: &str) -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let candidate = current.parent()?.join(file_name(bare));
    candidate.is_file().then_some(candidate)
}

/// Spell `stem` the way this host names a dynamic library file.
///
/// Unlike [`file_name`], whose caller only ever supplies a bare program name,
/// `stem` here may already be a full file name that happens to end in the
/// right suffix -- in which case nothing changes. That idempotence matters
/// because a caller resolving `"libclang.so"` from a package listing must not
/// get `"libclang.so.so"` back.
pub fn native_library_name(stem: &OsStr) -> OsString {
    let mut name = stem.to_os_string();
    if Path::new(stem).extension().is_none() {
        name.push(".so");
    }
    name
}

/// Finds a runnable host image in an explicit ordered directory list.
///
/// Search order and `PATHEXT` are search concerns, not naming ones -- see
/// [`file_name`] for the naming half. This host has no extension search: a
/// candidate either exists under `name` verbatim or it does not.
pub fn find_in_paths(name: &OsStr, directories: &[PathBuf]) -> Option<PathBuf> {
    directories
        .iter()
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// Whether a host executable's file stem matches an expected program name.
///
/// This host's file names are case-sensitive, so the comparison is exact.
pub fn stem_matches(path: &OsStr, expected: &str) -> bool {
    Path::new(path).file_stem() == Some(OsStr::new(expected))
}

/// This host never locks a running image against being overwritten in place,
/// so there is nothing to relocate before replacing it.
pub fn unlock_for_replacement(_image: &Path) -> std::io::Result<bool> {
    Ok(false)
}
