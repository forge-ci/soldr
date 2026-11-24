//! A case-insensitive clang VFS overlay over the xwin SDK include trees
//! (soldr#3415).
//!
//! The xwin bundle stores headers under one casing (mostly lowercase:
//! `basetsd.h`), while Windows code includes them in whatever casing it was
//! written in (`BaseTsd.h`, `WINDOWS.H`). On a case-sensitive Linux filesystem
//! `#include <BaseTsd.h>` fails. Case-variant aliases (see
//! [`super::xwin_cache::ensure_xwin_case_aliases`]) cover the casings the SDK
//! itself and a documented list use, but cannot cover every spelling. A VFS
//! overlay with `case-sensitive: false` can: clang resolves any path under an
//! overlay root without regard to case, and paths it does not list fall through
//! to the real file system.
//!
//! The overlay is generated from the include trees, so it carries one entry per
//! header (about 7,500 for the current SDK, 840 KiB). Measured with clang-cl 21
//! on the real SDK it adds roughly 20-30 ms to each compile, because clang parses
//! it on every invocation; that is the price of resolving any spelling. It is
//! content-addressed by name, so the flag on the compiler command line (and
//! therefore any compile cache key) only changes when the header set does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kernal_api::hash::Sha256Hasher;

use crate::core::SoldrError;

const OVERLAY_FORMAT_VERSION: &str = "v1";

/// The overlay's JSON text (JSON is valid YAML, which is what clang reads) for
/// every header under `include_roots`, in a stable order.
fn overlay_text(base: &Path, include_roots: &[PathBuf]) -> Result<String, SoldrError> {
    // directory -> [(file name, full path)], sorted for determinism.
    let mut directories: BTreeMap<PathBuf, Vec<(String, PathBuf)>> = BTreeMap::new();
    for root in include_roots {
        collect(root, &mut directories)?;
    }
    let roots: Vec<serde_json::Value> = directories
        .into_iter()
        .map(|(directory, mut files)| {
            files.sort();
            let contents: Vec<serde_json::Value> = files
                .into_iter()
                .map(|(name, path)| {
                    // Relative to the overlay file's directory (see
                    // `overlay-relative` below): roughly halves the file, and
                    // clang re-parses it on every compile.
                    let target = path.strip_prefix(base).unwrap_or(&path);
                    serde_json::json!({
                        "name": name,
                        "type": "file",
                        "external-contents": target.to_string_lossy(),
                    })
                })
                .collect();
            serde_json::json!({
                "name": directory.to_string_lossy(),
                "type": "directory",
                "contents": contents,
            })
        })
        .collect();
    let document = serde_json::json!({
        "version": 0,
        "case-sensitive": false,
        "overlay-relative": true,
        "roots": roots,
    });
    serde_json::to_string(&document)
        .map_err(|error| SoldrError::Other(format!("serialize SDK case overlay: {error}")))
}

fn collect(
    directory: &Path,
    out: &mut BTreeMap<PathBuf, Vec<(String, PathBuf)>>,
) -> Result<(), SoldrError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        // A missing include tree (catalogue drift) is simply not overlaid.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(SoldrError::Other(format!(
                "read SDK include dir {}: {error}",
                directory.display()
            )))
        }
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect(&path, out)?;
        } else if file_type.is_file() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue; // a non-UTF-8 name cannot appear in an #include
            };
            out.entry(directory.to_path_buf())
                .or_default()
                .push((name, path));
        }
    }
    Ok(())
}

/// Write (if absent) and return the case-insensitive overlay for
/// `include_roots`, stored beside the SDK as
/// `<xwin_dir>/soldr-case-overlay-v1-<digest>.yaml`. `None` when there is
/// nothing to overlay.
pub fn ensure_case_overlay(
    xwin_dir: &Path,
    include_roots: &[PathBuf],
) -> Result<Option<PathBuf>, SoldrError> {
    let text = overlay_text(xwin_dir, include_roots)?;
    if !text.contains("\"external-contents\"") {
        return Ok(None);
    }
    let digest = format!("{:x}", Sha256Hasher::digest(text.as_bytes()));
    let path = xwin_dir.join(format!(
        "soldr-case-overlay-{OVERLAY_FORMAT_VERSION}-{}.yaml",
        &digest[..20]
    ));
    if path.is_file() {
        return Ok(Some(path));
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, &text)?;
    if let Err(error) = std::fs::rename(&temporary, &path) {
        let _ = std::fs::remove_file(&temporary);
        // A concurrent writer of identical content is fine.
        if !path.is_file() {
            return Err(SoldrError::Io(error));
        }
    }
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sdk() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("sdk").join("include").join("shared");
        std::fs::create_dir_all(shared.join("sub")).unwrap();
        std::fs::write(shared.join("basetsd.h"), b"#pragma once\n").unwrap();
        std::fs::write(shared.join("sub").join("Nested.h"), b"#pragma once\n").unwrap();
        dir
    }

    fn roots(dir: &Path) -> Vec<PathBuf> {
        vec![
            dir.join("sdk").join("include").join("shared"),
            dir.join("sdk").join("include").join("missing"),
        ]
    }

    #[test]
    fn the_overlay_is_case_insensitive_and_lists_every_header() {
        let dir = sdk();
        let path = ensure_case_overlay(dir.path(), &roots(dir.path()))
            .unwrap()
            .expect("an overlay");
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(parsed["case-sensitive"], serde_json::Value::Bool(false));
        assert_eq!(parsed["version"], 0);
        assert_eq!(parsed["overlay-relative"], serde_json::Value::Bool(true));
        let listed: Vec<String> = parsed["roots"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|root| root["contents"].as_array().unwrap().iter())
            .map(|file| file["name"].as_str().unwrap().to_string())
            .collect();
        let targets: Vec<String> = parsed["roots"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|root| root["contents"].as_array().unwrap().iter())
            .map(|file| file["external-contents"].as_str().unwrap().to_string())
            .collect();
        assert!(
            targets
                .iter()
                .all(|target| !Path::new(target).is_absolute()),
            "targets under the SDK dir are relative to it: {targets:?}"
        );
        assert!(listed.contains(&"basetsd.h".to_string()), "{listed:?}");
        assert!(listed.contains(&"Nested.h".to_string()), "{listed:?}");
    }

    #[test]
    fn the_overlay_is_content_addressed_stable_and_not_rewritten() {
        let dir = sdk();
        let first = ensure_case_overlay(dir.path(), &roots(dir.path()))
            .unwrap()
            .unwrap();
        let modified = std::fs::metadata(&first).unwrap().modified().unwrap();
        let second = ensure_case_overlay(dir.path(), &roots(dir.path()))
            .unwrap()
            .unwrap();
        assert_eq!(first, second, "same headers, same file");
        assert_eq!(
            std::fs::metadata(&second).unwrap().modified().unwrap(),
            modified
        );

        let extra = dir
            .path()
            .join("sdk")
            .join("include")
            .join("shared")
            .join("New.h");
        std::fs::write(extra, b"#pragma once\n").unwrap();
        let third = ensure_case_overlay(dir.path(), &roots(dir.path()))
            .unwrap()
            .unwrap();
        assert_ne!(first, third, "a changed header set is a different overlay");
    }

    #[test]
    fn nothing_to_overlay_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        let none = ensure_case_overlay(dir.path(), &[dir.path().join("absent")]).unwrap();
        assert!(none.is_none());
    }

    /// Where a `clang-cl` can be found: `SOLDR_LLVM_DIR`, else the search path.
    fn find_clang_cl() -> Option<PathBuf> {
        let name = format!("clang-cl{}", std::env::consts::EXE_SUFFIX);
        let from_dir = std::env::var_os("SOLDR_LLVM_DIR")
            .map(|dir| PathBuf::from(dir).join(&name))
            .filter(|path| path.is_file());
        from_dir.or_else(|| {
            let search = std::env::var_os("PATH")?;
            std::env::split_paths(&search)
                .map(|dir| dir.join(&name))
                .find(|path| path.is_file())
        })
    }

    /// The point of the overlay: a real `clang-cl` resolves a spelling that is
    /// not on disk. Skips where no `clang-cl` is available (the CI lane does
    /// not put one on the search path); a genuinely missing header must still
    /// fail either way.
    #[test]
    fn a_real_clang_cl_resolves_any_include_casing_through_the_generated_overlay() {
        let Some(clang_cl) = find_clang_cl() else {
            return;
        };
        let dir = sdk();
        let shared = dir.path().join("sdk").join("include").join("shared");
        let overlay = ensure_case_overlay(dir.path(), &roots(dir.path()))
            .unwrap()
            .unwrap();
        std::fs::write(dir.path().join("mixed.c"), "#include <BASETSD.h>\n").unwrap();
        std::fs::write(dir.path().join("absent.c"), "#include <Nope.h>\n").unwrap();
        let compile = |source: &str, with_overlay: bool| {
            let mut command = std::process::Command::new(&clang_cl);
            command
                .args(["--target=x86_64-pc-windows-msvc", "-fsyntax-only"])
                .arg(format!("/imsvc{}", shared.display()));
            if with_overlay {
                command.arg("-vfsoverlay").arg(&overlay);
            }
            command.arg(dir.path().join(source)).output().unwrap()
        };
        assert!(
            !compile("mixed.c", false).status.success(),
            "control: no overlay, no header"
        );
        assert!(
            compile("mixed.c", true).status.success(),
            "the overlay resolves BASETSD.h"
        );
        assert!(
            !compile("absent.c", true).status.success(),
            "a missing header still fails"
        );
    }
}
