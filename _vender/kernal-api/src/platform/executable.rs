//! Executable naming, search, image discovery, and materialization primitives.
//!
//! Callers name the *program* they want. Whether that program is a file called
//! `runpm` or `runpm.exe`, and where a sibling install lives relative to the
//! running image, is a host mechanic and is decided here.

pub use crate::{
    executable_file_name as file_name, executable_file_name_os as file_name_os,
    executable_find_in_paths as find_in_paths,
    executable_native_library_name as native_library_name,
    executable_sibling_of_current_image as sibling_of_current_image,
    executable_stem_matches as stem_matches,
    executable_unlock_for_replacement as unlock_for_replacement, EXECUTABLE_EXTENSION,
};

/// Path of the image this process is running.
///
/// Named for what it answers -- which file on disk this process was launched
/// from -- so a caller redeploying or re-executing itself reads that intent
/// from `platform::executable` alongside [`sibling_of_current_image`], which
/// resolves against exactly this path. The result is the host's answer and can
/// name a deleted or replaced file; it is not revalidated here.
pub fn current_image() -> std::io::Result<std::path::PathBuf> {
    std::env::current_exe()
}

/// Finds a runnable host image using the process `PATH`.
///
/// Splits `PATH` into its component directories and delegates to
/// [`find_in_paths`], so Windows' `PATHEXT` suffix search applies exactly as
/// it does for an explicit directory list. An unset `PATH` reports nothing
/// found rather than treating every directory as implicitly on it.
pub fn find_on_path(name: &std::ffi::OsStr) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    let directories: Vec<_> = std::env::split_paths(&path).collect();
    find_in_paths(name, &directories)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// The running image is an existing file, and siblings resolve beside it.
    ///
    /// Asserted against `sibling_of_current_image`, the operation that depends
    /// on this one, rather than against `std::env::current_exe` -- restating
    /// the implementation would test nothing.
    #[test]
    fn the_current_image_is_a_real_file_its_siblings_resolve_against() {
        let image = current_image().expect("a running process has an image");

        assert!(image.is_file(), "{} is a file", image.display());
        let bare = image
            .file_stem()
            .expect("image stem")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            sibling_of_current_image(&bare).as_deref(),
            Some(&*image),
            "siblings resolve against exactly the image this reports"
        );
    }

    /// The host decides the spelling; the caller never does.
    ///
    /// Asserted against `EXECUTABLE_EXTENSION` rather than a hard-coded
    /// `.exe`, so the test states the contract instead of restating one host's
    /// answer -- the shape the caller sites used to have.
    #[test]
    fn file_name_applies_the_host_executable_extension() {
        let named = file_name("running-process-daemon");
        match EXECUTABLE_EXTENSION {
            Some(extension) => {
                assert_eq!(named, format!("running-process-daemon.{extension}"));
                assert!(std::path::Path::new(&named).extension().is_some());
            }
            None => assert_eq!(named, "running-process-daemon"),
        }
    }

    /// The running image is always a sibling of itself, under whatever
    /// spelling this host uses -- which is the only claim that holds on every
    /// host without assuming what else is installed.
    #[test]
    fn the_running_image_is_found_beside_itself() {
        let current = std::env::current_exe().expect("current image");
        let bare = current
            .file_stem()
            .expect("image stem")
            .to_string_lossy()
            .into_owned();

        assert_eq!(sibling_of_current_image(&bare).as_deref(), Some(&*current));
    }

    /// A program that is not installed beside us is reported as absent rather
    /// than as a path that does not exist.
    #[test]
    fn an_absent_sibling_is_none_not_a_missing_path() {
        assert!(sibling_of_current_image("rp-no-such-sibling-program").is_none());
    }

    /// The host always adds some dynamic-library suffix to a bare stem.
    #[test]
    fn native_library_name_carries_a_host_extension() {
        let name = native_library_name(OsStr::new("clang"));
        assert!(std::path::Path::new(&name).extension().is_some());
    }

    /// A stem that already carries an extension is returned unchanged, on
    /// every host -- resolving `"clang.so"` from a package listing must never
    /// grow a second suffix.
    #[test]
    fn native_library_name_is_idempotent_when_already_extended() {
        assert_eq!(
            native_library_name(OsStr::new("clang.so")),
            OsStr::new("clang.so")
        );
    }

    /// An explicit directory list finds the running image beside itself --
    /// the same claim the sibling-image test above makes for
    /// [`sibling_of_current_image`], restated for the lower-level primitive.
    #[test]
    fn find_in_paths_locates_the_current_image_beside_itself() {
        let image = std::env::current_exe().expect("current executable image");
        let directory = image.parent().expect("image directory").to_path_buf();
        let name = image.file_name().expect("image file name");
        assert_eq!(find_in_paths(name, &[directory]), Some(image));
    }

    /// A name absent from every candidate directory is reported as absent.
    #[test]
    fn find_in_paths_reports_absence_for_an_unknown_name() {
        let directory = std::env::temp_dir();
        assert_eq!(
            find_in_paths(OsStr::new("rp-no-such-executable-xyz"), &[directory]),
            None,
        );
    }

    /// `find_on_path` never invents a match; a name nothing on `PATH` owns
    /// stays absent. The positive case lives on [`find_in_paths`] instead --
    /// mutating `PATH` here would race every other test in this process.
    #[test]
    fn find_on_path_reports_absence_for_an_unknown_name() {
        assert!(find_on_path(OsStr::new("rp-no-such-executable-on-path-xyz")).is_none());
    }

    /// The comparison is host-neutral for two spellings that already agree in
    /// case, which is the only claim that holds on every host.
    #[test]
    fn stem_matches_the_expected_program_name() {
        assert!(stem_matches(
            OsStr::new("/usr/bin/running-process"),
            "running-process"
        ));
        assert!(!stem_matches(
            OsStr::new("/usr/bin/other"),
            "running-process"
        ));
    }

    /// Relocating a running image never destroys it: the original path keeps
    /// resolving to a file either way, and any litter this leaves behind
    /// lives inside the scratch directory this test removes afterwards.
    #[test]
    fn unlock_for_replacement_matches_host_locking_semantics() {
        let dir = std::env::temp_dir().join(format!(
            "rp-executable-unlock-{}-{:?}",
            std::process::id(),
            std::thread::current().id(),
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let image = dir.join(file_name("probe"));
        std::fs::write(&image, b"image").expect("write image");

        let relocated = unlock_for_replacement(&image).expect("unlock");
        assert_eq!(relocated, cfg!(target_os = "windows"));
        assert!(image.is_file(), "the original path must still be a file");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
