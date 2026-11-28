//! `.app` bundles in the standard Applications folders, launched and opened through `open`.
use crate::app_catalog::{AppEntry, LaunchError, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;

fn roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = [
        "/Applications",
        "/Applications/Utilities",
        "/System/Applications",
        "/System/Applications/Utilities",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(Path::new(&home).join("Applications"));
    }
    roots
}

pub(crate) async fn list_apps() -> Vec<AppEntry> {
    tokio::task::spawn_blocking(|| {
        roots()
            .iter()
            .filter_map(|root| std::fs::read_dir(root).ok())
            .flat_map(|entries| entries.flatten())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "app"))
            .filter_map(|path| {
                Some(AppEntry {
                    id: path.to_string_lossy().into_owned(),
                    name: path.file_stem()?.to_str()?.to_owned(),
                    keywords: Vec::new(),
                    icon: None,
                    target: path,
                })
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

async fn open(args: &[&std::ffi::OsStr]) -> Result<(), LaunchError> {
    let status = tokio::process::Command::new("/usr/bin/open")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|_| LaunchError::Failed)?;
    // `open` exits 1 when the target does not exist or no handler claims it.
    if status.success() { Ok(()) } else { Err(LaunchError::NotFound) }
}

pub(crate) async fn launch(app: &AppEntry) -> Result<(), LaunchError> {
    open(&[app.target.as_os_str()]).await
}

pub(crate) async fn open_uri(uri: &str) -> Result<(), LaunchError> {
    open(&[uri.as_ref()]).await
}

pub(crate) async fn open_uri_with(app: &AppEntry, uri: &str, _options: OpenOptions) -> Result<(), LaunchError> {
    open(&["-a".as_ref(), app.target.as_os_str(), uri.as_ref()]).await
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn the_runner_ships_applications() {
        assert!(!super::list_apps().await.is_empty());
    }
}
