#![cfg(feature = "daemon-registration-v2")]

//! Public contract for the frozen v2 daemon-registration facade.

use std::fs;

use kernal_api::daemon_registration_v2::{
    service_definition_directory, service_definition_path, write_service_definition,
    DaemonRegistrationV2Error, ServiceDefinitionBuilder,
};

#[cfg(windows)]
const ABS_BINARY: &str = "C:\\tools\\zccache.exe";
#[cfg(not(windows))]
const ABS_BINARY: &str = "/usr/local/bin/zccache";

#[cfg(windows)]
const ABS_BINARY_DIR: &str = "C:\\tools";
#[cfg(not(windows))]
const ABS_BINARY_DIR: &str = "/usr/local/bin";

#[test]
fn shared_v2_definition_preserves_semantics_path_and_version_order() {
    let temporary_parent = tempfile::tempdir().expect("service definition tempdir");
    let root = temporary_parent.path().join("services");
    let builder = ServiceDefinitionBuilder::shared_broker("zccache", ABS_BINARY)
        .per_version_binary_dir(ABS_BINARY_DIR)
        .min_version("1.10.0")
        .allow_version("1.11.20")
        .allow_version("1.11.21")
        .label("team", "cache")
        .label("vendor", "zackees");

    let definition = builder.clone().build();
    assert_eq!(definition.service_name(), "zccache");
    assert_eq!(definition.binary_path(), ABS_BINARY);
    assert!(definition.is_shared_broker());
    assert_eq!(definition.per_version_binary_dir(), ABS_BINARY_DIR);
    assert_eq!(definition.min_version(), "1.10.0");
    assert_eq!(
        definition.allowed_versions().collect::<Vec<_>>(),
        ["1.11.20", "1.11.21"]
    );
    assert_eq!(definition.label("team"), Some("cache"));
    assert_eq!(definition.label("vendor"), Some("zackees"));
    assert_eq!(definition.labels().count(), 2);

    let path = builder.install_in(&root).expect("install v2 definition");
    assert_eq!(path, root.join("zccache.servicedef.v2"));
    assert_eq!(
        service_definition_path(&root, "zccache").expect("v2 path"),
        path
    );
    assert!(fs::metadata(path)
        .expect("persisted v2 definition")
        .is_file());
}

#[test]
fn explicit_writer_keeps_the_same_v2_path_and_record_semantics() {
    let temporary_parent = tempfile::tempdir().expect("service definition tempdir");
    let root = temporary_parent.path().join("services");
    let definition = ServiceDefinitionBuilder::shared_broker("zccache", ABS_BINARY)
        .per_version_binary_dir(ABS_BINARY_DIR)
        .min_version("1.11.20")
        .allow_version("1.11.20")
        .label("consumer", "zccache")
        .build();

    let path = write_service_definition(&root, &definition).expect("write v2 definition");
    assert_eq!(path, root.join("zccache.servicedef.v2"));
    assert!(path.exists());
    assert_eq!(definition.label("consumer"), Some("zccache"));
}

#[test]
fn invalid_service_name_does_not_create_a_v2_record() {
    let temporary_parent = tempfile::tempdir().expect("service definition tempdir");
    let root = temporary_parent.path().join("services");
    let definition = ServiceDefinitionBuilder::shared_broker("Zccache", ABS_BINARY).build();

    assert!(matches!(
        write_service_definition(&root, &definition),
        Err(DaemonRegistrationV2Error::InvalidName { .. })
    ));
    assert!(!root.join("Zccache.servicedef.v2").exists());
}

#[cfg(unix)]
#[test]
fn v2_service_definition_directory_is_owner_private() {
    use std::os::unix::fs::PermissionsExt;

    let temporary_parent = tempfile::tempdir().expect("service definition tempdir");
    let root = temporary_parent.path().join("services");
    ServiceDefinitionBuilder::shared_broker("zccache", ABS_BINARY)
        .install_in(&root)
        .expect("install v2 definition");

    assert_eq!(
        fs::metadata(&root)
            .expect("service root metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700,
        "v2 service-definition root must be owner-private"
    );
}

#[test]
fn default_directory_retains_the_established_product_layout() {
    assert!(service_definition_directory().ends_with("running-process/services"));
}
