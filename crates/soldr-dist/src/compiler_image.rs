//! Canonical multicall alias for the exact running image, independent of daemon PATH.
use crate::Result;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

fn digest(path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0u8; 65536];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub(crate) fn materialize(root: &Path, image: &Path) -> Result<PathBuf> {
    let hash = digest(image)?;
    let dir = root.join("runtime").join("compiler").join(&hash);
    std::fs::create_dir_all(&dir)?;
    let dir = std::fs::canonicalize(dir)?;
    let alias = dir.join(format!("soldr{}", std::env::consts::EXE_SUFFIX));
    if alias.is_file() {
        if digest(&alias)? != hash {
            return Err("dist compiler image alias failed integrity check".into());
        }
        return Ok(alias);
    }
    let staging = tempfile::tempdir_in(&dir)?;
    let temp = staging.path().join("image");
    if std::fs::hard_link(image, &temp).is_err() {
        std::fs::copy(image, &temp)?;
    }
    if digest(&temp)? != hash {
        return Err("dist compiler image changed while materializing".into());
    }
    if let Err(error) = std::fs::rename(&temp, &alias) {
        if !alias.is_file() || digest(&alias)? != hash {
            return Err(error.into());
        }
    }
    Ok(alias)
}

#[test]
fn canonical_alias_preserves_image_and_stays_inside_the_dist_root() {
    let temp = tempfile::tempdir().unwrap();
    let image = temp.path().join("soldr-daemon");
    std::fs::write(&image, "test image").unwrap();
    let root = temp.path().join("dist");
    let alias = materialize(&root, &image).unwrap();
    assert!(alias.starts_with(std::fs::canonicalize(&root).unwrap()));
    assert_eq!(alias.file_stem().unwrap(), "soldr");
    assert_eq!(std::fs::read(&alias).unwrap(), b"test image");
    assert_eq!(materialize(&root, &image).unwrap(), alias);
}

#[cfg(unix)]
#[test]
fn relative_state_root_still_returns_an_absolute_compiler_program() {
    let temp = tempfile::tempdir().unwrap();
    let image = temp.path().join("soldr-daemon");
    std::fs::write(&image, "relative-root image").unwrap();
    let cwd = std::env::current_dir().unwrap();
    let mut relative = PathBuf::new();
    for _ in cwd.components().skip(1) {
        relative.push("..");
    }
    relative.push(temp.path().strip_prefix("/").unwrap());
    relative.push("dist");
    assert!(!relative.is_absolute());
    let alias = materialize(&relative, &image).unwrap();
    assert!(alias.is_absolute());
    assert_eq!(std::fs::read(alias).unwrap(), b"relative-root image");
}
