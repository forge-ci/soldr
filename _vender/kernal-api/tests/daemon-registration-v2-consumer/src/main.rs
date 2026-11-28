//! Compile-only consumer proving the v2 facade needs no backend API.

use kernal_api::daemon_registration_v2::{
    service_definition_directory, service_definition_path, ServiceDefinitionBuilder,
};

#[cfg(windows)]
const ABS_BINARY: &str = "C:\\tools\\zccache.exe";
#[cfg(not(windows))]
const ABS_BINARY: &str = "/usr/local/bin/zccache";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let definition = ServiceDefinitionBuilder::shared_broker("consumer", ABS_BINARY)
        .per_version_binary_dir("/var/cache/consumer")
        .min_version("1.2.3")
        .allow_version("1.2.3")
        .label("consumer", "fixture")
        .build();
    let _ = (
        service_definition_directory(),
        service_definition_path("/tmp", definition.service_name())?,
        definition.binary_path(),
        definition.is_shared_broker(),
        definition.allowed_versions().count(),
        definition.label("consumer"),
    );
    Ok(())
}
