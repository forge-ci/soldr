use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn scratch(label: &str) -> io::Result<PathBuf> {
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let root = std::env::current_dir()?
        .join("target/test-fixtures")
        .join(format!("fs-scan-{label}-{}-{time}", std::process::id()));
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

pub(crate) fn paths(root: &Path) -> Result<Vec<PathBuf>, super::ScanError> {
    let mut result = Vec::new();
    super::scan_stream(root, &Default::default(), |entry| {
        result.push(root.join(entry.relative_name));
        true
    })?;
    Ok(result)
}
