//! Private compiler-output staging configuration for the embedded zccache.
//!
//! Durable artifacts stay under the Soldr cache. On Windows, ephemeral
//! compiler outputs use a short, cache-specific temp path so legacy toolchain
//! processes do not cross `MAX_PATH`. An explicit `ZCCACHE_STAGING_DIR`
//! remains authoritative on every platform.

use std::path::{Path, PathBuf};

use zccache::core::NormalizedPath;
use zccache::embedded::{DiskCacheLimits, MaintenanceOwnership, ZccacheStartOptions};
use zccache::hash::StreamHasher;

pub(crate) fn options(cache_root: &Path, disk_limits: DiskCacheLimits) -> ZccacheStartOptions {
    ZccacheStartOptions {
        disk_limits,
        maintenance_ownership: MaintenanceOwnership::Host,
        staging_root: resolve(cache_root),
    }
}

fn resolve(cache_root: &Path) -> Option<NormalizedPath> {
    if let Some(configured) = zccache::core::config::staging_dir_override() {
        return Some(configured);
    }

    platform_default(cache_root)
}

fn platform_default(cache_root: &Path) -> Option<NormalizedPath> {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        return None;
    }
    let root = windows_private_staging_root(cache_root, &std::env::temp_dir());
    tracing::debug!(
        path = %root.display(),
        "using short Windows compiler-staging root"
    );
    Some(root.into())
}

fn windows_private_staging_root(cache_root: &Path, temp: &Path) -> PathBuf {
    let mut hasher = StreamHasher::new();
    hasher.update(cache_root.to_string_lossy().as_bytes());
    let cache_id = hasher.finalize().to_hex()[..16].to_string();
    temp.join("sz").join(cache_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_staging_root_is_short_and_cache_specific() {
        let temp = Path::new(r"C:\Users\runneradmin\AppData\Local\Temp");
        let first = windows_private_staging_root(
            Path::new(r"C:\deep\cache\root\that\must\not\be\repeated\inside\the\linker\path"),
            temp,
        );
        let second = windows_private_staging_root(Path::new(r"D:\other-cache"), temp);

        assert!(first.starts_with(temp));
        assert_ne!(first, second, "isolated caches need isolated staging roots");
        assert!(
            first.as_os_str().len() <= temp.as_os_str().len() + 20,
            "cache-root depth leaked into compiler staging: {}",
            first.display()
        );
    }
}
