#![cfg(feature = "daemon-registration")]

//! Public contract for the frozen v1 daemon-registration facade.

use std::fs;

use kernal_api::daemon_registration::{
    manifest_directory, service_definition_directory, CacheManifest, CacheManifestBuilder,
    CacheRootKind, DaemonRegistrationError, ServiceDefinition, ServiceDefinitionBuilder,
};

#[cfg(windows)]
const ABS_BINARY: &str = "C:\\tools\\zccache.exe";
#[cfg(not(windows))]
const ABS_BINARY: &str = "/usr/local/bin/zccache";

#[cfg(windows)]
const ABS_BINARY_DIR: &str = "C:\\tools";
#[cfg(not(windows))]
const ABS_BINARY_DIR: &str = "/usr/local/bin";

/// Exact frozen v1 manifest bytes from the upstream registration contract.
const FROZEN_MANIFEST_V1: &[u8] = &[
    0x0a, 0x07, b'z', b'c', b'c', b'a', b'c', b'h', b'e', 0x12, 0x05, b'1', b'.', b'2', b'.', b'3',
    0x1a, 0x02, b'v', b'1', 0x20, 0x01, 0x28, 0x02, 0xc2, 0x02, 0x06, b's', b'h', b'a', b'r', b'e',
    b'd', 0xb2, 0x04, 0x06, b'b', b'u', b'n', b'd', b'l', b'e', 0xa0, 0x06, 0x01, 0xaa, 0x06, 0x31,
    b'a', b'p', b'p', b'l', b'i', b'c', b'a', b't', b'i', b'o', b'n', b'/', b'v', b'n', b'd', b'.',
    b'r', b'u', b'n', b'n', b'i', b'n', b'g', b'-', b'p', b'r', b'o', b'c', b'e', b's', b's', b'.',
    b'c', b'a', b'c', b'h', b'e', b'-', b'm', b'a', b'n', b'i', b'f', b'e', b's', b't', b'.', b'v',
    b'1', 0xb2, 0x06, 0x20, 0x01, 0x12, 0x0d, 0x59, 0xff, 0xa9, 0x45, 0xe3, 0xff, 0xa4, 0x6a, 0xaa,
    0xaf, 0xee, 0xc8, 0x6f, 0xef, 0xfe, 0x55, 0xc2, 0x5f, 0x0a, 0x04, 0x0c, 0x9d, 0xe3, 0xdb, 0x67,
    0x4b, 0xe3, 0xa0, 0x51,
];

#[test]
fn reads_the_frozen_v1_manifest_golden_without_protocol_types() {
    let directory = tempfile::tempdir().expect("manifest tempdir");
    let path = directory.path().join("zccache-1.2.3.pb");
    fs::write(&path, FROZEN_MANIFEST_V1).expect("write frozen manifest");

    let manifest = CacheManifest::read(&path).expect("read frozen manifest");
    assert_eq!(manifest.service_name(), "zccache");
    assert_eq!(manifest.service_version(), "1.2.3");
    assert_eq!(manifest.broker_envelope_version(), "v1");
    assert_eq!(manifest.broker_instance(), "shared");
    assert_eq!(manifest.schema_version(), 1);
    assert_eq!(
        manifest.media_type(),
        "application/vnd.running-process.cache-manifest.v1"
    );
    assert_eq!(manifest.created_at_unix_ms(), 1);
    assert_eq!(manifest.last_active_unix_ms(), 2);
    assert!(manifest.has_sha256_seal());
}

#[test]
fn manifest_builder_retains_ordered_roots_metadata_and_seal() {
    let temporary_parent = tempfile::tempdir().expect("registry tempdir");
    let registry = temporary_parent.path().join("manifests");
    let builder = CacheManifestBuilder::new("zccache", "1.11.20")
        .broker_instance("shared")
        .root(CacheRootKind::CacheData, "/var/cache/zccache")
        .root(CacheRootKind::CacheIndex, "/var/cache/zccache/depgraph")
        .root(CacheRootKind::CacheLogs, "/var/cache/zccache/logs")
        .root(CacheRootKind::CacheLocks, "/var/cache/zccache")
        .root(CacheRootKind::CacheTmp, "/var/cache/zccache/tmp");

    let built = builder.clone().build().expect("build sealed manifest");
    assert_eq!(built.service_name(), "zccache");
    assert_eq!(built.service_version(), "1.11.20");
    assert_eq!(built.broker_envelope_version(), "v1");
    assert_eq!(built.schema_version(), 1);
    assert_eq!(
        built.media_type(),
        "application/vnd.running-process.cache-manifest.v1"
    );
    assert!(built.has_host_identity());
    assert!(built.has_sha256_seal());
    assert_eq!(
        built
            .roots()
            .map(|root| (root.kind(), root.path()))
            .collect::<Vec<_>>(),
        vec![
            (CacheRootKind::CacheData, "/var/cache/zccache"),
            (CacheRootKind::CacheIndex, "/var/cache/zccache/depgraph"),
            (CacheRootKind::CacheLogs, "/var/cache/zccache/logs"),
            (CacheRootKind::CacheLocks, "/var/cache/zccache"),
            (CacheRootKind::CacheTmp, "/var/cache/zccache/tmp"),
        ]
    );

    let path = builder.publish_in(&registry).expect("publish manifest");
    assert_eq!(path, registry.join("zccache-1.11.20.pb"));
    assert_eq!(CacheManifest::read(&path).expect("read manifest"), built);

    let mut tampered = fs::read(&path).expect("read published bytes");
    *tampered.last_mut().expect("manifest bytes") ^= 1;
    fs::write(&path, tampered).expect("write tampered bytes");
    assert!(matches!(
        CacheManifest::read(&path),
        Err(DaemonRegistrationError::ManifestIntegrityFailure)
    ));
}

#[test]
fn service_definition_install_keeps_frozen_v1_filename_and_bytes() {
    let temporary_parent = tempfile::tempdir().expect("service definition tempdir");
    let root = temporary_parent.path().join("services");
    let builder = ServiceDefinitionBuilder::shared_broker("zccache", ABS_BINARY)
        .per_version_binary_dir(ABS_BINARY_DIR)
        .min_version("1.10.0")
        .allow_version("1.11.20")
        .label("team", "cache");

    let definition = builder.clone().build().expect("build v1 definition");
    assert_eq!(definition.service_name(), "zccache");
    assert_eq!(definition.binary_path(), ABS_BINARY);
    assert!(definition.is_shared_broker());
    assert_eq!(definition.per_version_binary_dir(), ABS_BINARY_DIR);
    assert_eq!(definition.min_version(), "1.10.0");
    assert_eq!(
        definition.allowed_versions().collect::<Vec<_>>(),
        vec!["1.11.20"]
    );
    assert_eq!(definition.label("team"), Some("cache"));

    let path = builder.install_in(&root).expect("install v1 definition");
    assert_eq!(path, root.join("zccache.servicedef"));
    let frozen_bytes = fs::read(&path).expect("read v1 service definition");
    assert_eq!(frozen_bytes, expected_service_definition_bytes());
    assert_eq!(
        ServiceDefinition::read(&root, "zccache").expect("read v1 definition"),
        definition
    );
}

#[cfg(unix)]
#[test]
fn explicit_registration_directories_are_owner_private() {
    use std::os::unix::fs::PermissionsExt;

    let temporary_parent = tempfile::tempdir().expect("registration tempdir");
    let registry = temporary_parent.path().join("manifests");
    let services = temporary_parent.path().join("services");
    CacheManifestBuilder::new("zccache", "1.11.20")
        .broker_instance("shared")
        .root(CacheRootKind::CacheData, "/var/cache/zccache")
        .publish_in(&registry)
        .expect("publish manifest");
    ServiceDefinitionBuilder::shared_broker("zccache", ABS_BINARY)
        .install_in(&services)
        .expect("install v1 definition");

    for path in [&registry, &services] {
        assert_eq!(
            fs::metadata(path)
                .expect("registration directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "{} must be owner-private",
            path.display()
        );
    }
}

#[cfg(not(windows))]
fn expected_service_definition_bytes() -> Vec<u8> {
    vec![
        0x0a, 0x07, b'z', b'c', b'c', b'a', b'c', b'h', b'e', 0x12, 0x16, b'/', b'u', b's', b'r',
        b'/', b'l', b'o', b'c', b'a', b'l', b'/', b'b', b'i', b'n', b'/', b'z', b'c', b'c', b'a',
        b'c', b'h', b'e', 0x18, 0x01, 0x2a, 0x0e, b'/', b'u', b's', b'r', b'/', b'l', b'o', b'c',
        b'a', b'l', b'/', b'b', b'i', b'n', 0x32, 0x06, b'1', b'.', b'1', b'0', b'.', b'0', 0x3a,
        0x07, b'1', b'.', b'1', b'1', b'.', b'2', b'0', 0x42, 0x0d, 0x0a, 0x04, b't', b'e', b'a',
        b'm', 0x12, 0x05, b'c', b'a', b'c', b'h', b'e',
    ]
}

#[cfg(windows)]
fn expected_service_definition_bytes() -> Vec<u8> {
    vec![
        0x0a, 0x07, b'z', b'c', b'c', b'a', b'c', b'h', b'e', 0x12, 0x14, b'C', b':', b'\\', b't',
        b'o', b'o', b'l', b's', b'\\', b'z', b'c', b'c', b'a', b'c', b'h', b'e', b'.', b'e', b'x',
        b'e', 0x18, 0x01, 0x2a, 0x08, b'C', b':', b'\\', b't', b'o', b'o', b'l', b's', 0x32, 0x06,
        b'1', b'.', b'1', b'0', b'.', b'0', 0x3a, 0x07, b'1', b'.', b'1', b'1', b'.', b'2', b'0',
        0x42, 0x0d, 0x0a, 0x04, b't', b'e', b'a', b'm', 0x12, 0x05, b'c', b'a', b'c', b'h', b'e',
    ]
}

#[test]
fn default_paths_retain_the_v1_product_layout() {
    assert!(manifest_directory().ends_with("running-process/manifests"));
    assert!(service_definition_directory().ends_with("running-process/services"));
}

#[test]
fn semantic_errors_retain_v1_validation_categories() {
    let invalid_name = ServiceDefinitionBuilder::shared_broker("Zccache", ABS_BINARY)
        .build()
        .expect_err("uppercase service name must fail");
    assert!(matches!(
        invalid_name,
        DaemonRegistrationError::InvalidNameOrVersion { .. }
    ));

    let invalid_binary = ServiceDefinitionBuilder::shared_broker("zccache", "relative/zccache")
        .build()
        .expect_err("relative binary path must fail");
    assert!(matches!(
        invalid_binary,
        DaemonRegistrationError::InvalidServicePath {
            field: "binary_path",
            ..
        }
    ));
}
