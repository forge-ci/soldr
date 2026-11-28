//! Compile-only consumer proving the registration facade needs no backend API.

use kernal_api::daemon_registration::{
    CacheManifestBuilder, CacheRootKind, ServiceDefinitionBuilder,
};

#[cfg(windows)]
const ABS_BINARY: &str = "C:\\tools\\zccache.exe";
#[cfg(not(windows))]
const ABS_BINARY: &str = "/usr/local/bin/zccache";

#[cfg(windows)]
const ABS_BINARY_DIR: &str = "C:\\tools";
#[cfg(not(windows))]
const ABS_BINARY_DIR: &str = "/usr/local/bin";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = CacheManifestBuilder::new("consumer", "1.2.3")
        .broker_instance("shared")
        .root(CacheRootKind::CacheData, "/var/cache/consumer")
        .build()?;
    let definition = ServiceDefinitionBuilder::shared_broker("consumer", ABS_BINARY)
        .per_version_binary_dir(ABS_BINARY_DIR)
        .min_version("1.2.3")
        .allow_version("1.2.3")
        .label("consumer", "fixture")
        .build()?;
    let _ = (
        manifest.service_name(),
        manifest.roots().count(),
        definition.binary_path(),
        definition.is_shared_broker(),
    );
    Ok(())
}
