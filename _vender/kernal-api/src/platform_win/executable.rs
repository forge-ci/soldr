//! win executable naming and image-relative discovery.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// File-name extension the host requires on a runnable image, if any.
pub const EXECUTABLE_EXTENSION: Option<&str> = Some("exe");

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

/// Spell `bare` as this host names an executable, keeping native UTF-16.
///
/// Unlike [`file_name`], a name that already carries any extension is
/// returned unchanged, so a caller never receives `tool.exe.exe` or
/// `tool.cmd.exe`.
pub fn file_name_os(bare: &OsStr) -> OsString {
    if Path::new(bare).extension().is_some() {
        bare.to_os_string()
    } else {
        let mut name = bare.to_os_string();
        name.push(".exe");
        name
    }
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
/// because a caller resolving `"clang.dll"` from a package listing must not
/// get `"clang.dll.dll"` back.
pub fn native_library_name(stem: &OsStr) -> OsString {
    let mut name = stem.to_os_string();
    if Path::new(stem).extension().is_none() {
        name.push(".dll");
    }
    name
}

/// Finds a runnable host image in an explicit ordered directory list.
///
/// Search order and `PATHEXT` are search concerns, not naming ones -- see
/// [`file_name`] for the naming half. A `name` that already carries an
/// extension is looked up verbatim; one that does not is tried against every
/// `PATHEXT` suffix in order, falling back to the customary
/// `.COM;.EXE;.BAT;.CMD` when the environment does not set `PATHEXT` at all.
pub fn find_in_paths(name: &OsStr, directories: &[PathBuf]) -> Option<PathBuf> {
    let path = Path::new(name);
    let suffixes: Vec<OsString> = if path.extension().is_some() {
        vec![OsString::new()]
    } else {
        std::env::var_os("PATHEXT")
            .map(|value| {
                value
                    .to_string_lossy()
                    .split(';')
                    .filter(|suffix| !suffix.is_empty())
                    .map(OsString::from)
                    .collect()
            })
            .filter(|suffixes: &Vec<OsString>| !suffixes.is_empty())
            .unwrap_or_else(|| [".COM", ".EXE", ".BAT", ".CMD"].map(OsString::from).to_vec())
    };

    directories.iter().find_map(|directory| {
        suffixes.iter().find_map(|suffix| {
            let mut candidate_name = name.to_os_string();
            candidate_name.push(suffix);
            let candidate = directory.join(candidate_name);
            candidate.is_file().then_some(candidate)
        })
    })
}

/// Whether a host executable's file stem matches an expected program name.
///
/// This host's file names are case-preserving but case-insensitive, so the
/// comparison ignores ASCII case.
pub fn stem_matches(path: &OsStr, expected: &str) -> bool {
    Path::new(path)
        .file_stem()
        .and_then(OsStr::to_str)
        .is_some_and(|stem| stem.eq_ignore_ascii_case(expected))
}

/// Relocates a running image so a successor can be written to its path.
///
/// This host keeps a running executable's file open for the process's
/// lifetime, which blocks an in-place overwrite even though a *rename* is
/// still allowed. So the running image is renamed aside to a
/// `<stem>.exe.old.<nonce>` sibling -- freeing `image`'s original path for a
/// fresh binary -- and a copy of the retired file is written back to that
/// path so it keeps resolving to *something* runnable in the meantime. The
/// `.old.<nonce>` sibling is left on disk; cleaning it up (immediately, or on
/// a later run once nothing still has it open) is the caller's job.
pub fn unlock_for_replacement(image: &Path) -> std::io::Result<bool> {
    let nonce = std::process::id()
        ^ std::time::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .subsec_nanos();
    let retired = image.with_extension(format!("exe.old.{nonce}"));
    std::fs::rename(image, &retired)?;
    let _ = std::fs::copy(&retired, image);
    Ok(true)
}
