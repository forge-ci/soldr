use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use crate::remote_build::snapshot;

const PACKAGE_FIELDS: &[&str] = &[
    "CARGO_PKG_AUTHORS",
    "CARGO_PKG_DESCRIPTION",
    "CARGO_PKG_HOMEPAGE",
    "CARGO_PKG_LICENSE",
    "CARGO_PKG_LICENSE_FILE",
    "CARGO_PKG_NAME",
    "CARGO_PKG_README",
    "CARGO_PKG_REPOSITORY",
    "CARGO_PKG_RUST_VERSION",
    "CARGO_PKG_VERSION",
    "CARGO_PKG_VERSION_MAJOR",
    "CARGO_PKG_VERSION_MINOR",
    "CARGO_PKG_VERSION_PATCH",
    "CARGO_PKG_VERSION_PRE",
];
const MANIFEST_FIELDS: &[&str] = &["CARGO_MANIFEST_DIR", "CARGO_MANIFEST_PATH"];

pub(super) fn capture(root: &Path) -> BTreeMap<String, String> {
    let mut fields: BTreeMap<_, _> = PACKAGE_FIELDS
        .iter()
        .filter_map(|&key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
        .collect();
    for &key in MANIFEST_FIELDS {
        if let Ok(path) = std::env::var(key) {
            let relative = Path::new(&path)
                .canonicalize()
                .ok()
                .and_then(|path| path.strip_prefix(root).ok()?.to_str().map(str::to_owned));
            if let Some(relative) = relative {
                fields.insert(key.to_owned(), relative);
            }
        }
    }
    fields
}

pub(super) fn apply(
    command: &mut tokio::process::Command,
    root: &Path,
    fields: BTreeMap<String, String>,
) -> io::Result<()> {
    for (key, value) in fields {
        if PACKAGE_FIELDS.contains(&key.as_str()) {
            command.env(key, value);
        } else if MANIFEST_FIELDS.contains(&key.as_str()) {
            let path = if value.is_empty() {
                root.to_owned()
            } else {
                root.join(snapshot::relative_path(&value)?).canonicalize()?
            };
            if !path.starts_with(root) {
                return Err(io::Error::other(
                    "remote test manifest path escapes the checkout",
                ));
            }
            command.env(key, path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_only_package_metadata_and_local_manifest_paths() -> io::Result<()> {
        let checkout = tempfile::tempdir()?;
        let root = checkout.path().canonicalize()?;
        let package = root.join("packages/leaf");
        std::fs::create_dir_all(&package)?;
        let manifest = package.join("Cargo.toml");
        std::fs::write(&manifest, "")?;

        let mut command = tokio::process::Command::new("test-harness");
        apply(
            &mut command,
            &root,
            BTreeMap::from([
                ("CARGO_MANIFEST_DIR".into(), "packages/leaf".into()),
                (
                    "CARGO_MANIFEST_PATH".into(),
                    "packages/leaf/Cargo.toml".into(),
                ),
                ("CARGO_PKG_NAME".into(), "leaf".into()),
                ("LD_PRELOAD".into(), "/worker/lib.so".into()),
            ]),
        )?;
        let env: BTreeMap<_, _> = command.as_std().get_envs().collect();
        assert_eq!(env.len(), 3);
        assert_eq!(
            env[std::ffi::OsStr::new("CARGO_MANIFEST_DIR")],
            Some(package.as_os_str())
        );
        assert_eq!(
            env[std::ffi::OsStr::new("CARGO_MANIFEST_PATH")],
            Some(manifest.as_os_str())
        );
        assert_eq!(
            env[std::ffi::OsStr::new("CARGO_PKG_NAME")],
            Some(std::ffi::OsStr::new("leaf"))
        );
        assert!(apply(
            &mut command,
            &root,
            BTreeMap::from([("CARGO_MANIFEST_PATH".into(), "../Cargo.toml".into())]),
        )
        .is_err());
        Ok(())
    }
}
