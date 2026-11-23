//! Publication of the prepared Dylint toolchain marker.

use std::path::Path;

use super::{prepared_marker_path, DylintToolchainPlan};
use crate::core::{SoldrError, SoldrPaths};

/// Production entry point for persisting the warm-path marker after a
/// successful cold-path prepare. Best-effort by contract — callers must
/// not fail the run on a write error, only log it.
pub(super) fn write_prepared_marker(
    version: &str,
    plan: &DylintToolchainPlan,
    preserve_existing: bool,
) -> Result<(), SoldrError> {
    let base_dir = SoldrPaths::new()?.root;
    write_prepared_marker_at(&base_dir, version, plan, preserve_existing)
}

pub(super) fn write_prepared_marker_at(
    base_dir: &Path,
    version: &str,
    plan: &DylintToolchainPlan,
    preserve_existing: bool,
) -> Result<(), SoldrError> {
    use std::io::Write as _;

    let path = prepared_marker_path(base_dir, version);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = format!("{}\n", plan.cache_identity());
    if preserve_existing {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => file.write_all(contents.as_bytes())?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    } else {
        std::fs::write(path, contents)?;
    }
    Ok(())
}
