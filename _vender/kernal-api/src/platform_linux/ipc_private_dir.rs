use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use crate::platform::ipc::OwnerPrivateDirectoryOutcome;

pub fn ensure_owner_private_directory(path: &Path) -> io::Result<OwnerPrivateDirectoryOutcome> {
    fs::create_dir_all(path)?;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions)?;
    if owner_private_directory(path)? {
        Ok(OwnerPrivateDirectoryOutcome::Hardened)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "private-directory permissions were not applied to {}",
                path.display()
            ),
        ))
    }
}

pub fn owner_private_directory(path: &Path) -> io::Result<bool> {
    Ok(fs::metadata(path)?.permissions().mode() & 0o077 == 0)
}
