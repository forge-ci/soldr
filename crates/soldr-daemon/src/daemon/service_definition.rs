//! `running-process` **v2** service-definition support for soldr-daemon.
//!
//! soldr adopts the running-process v2 broker (soldr#1495). This module
//! writes the soldr-daemon `.servicedef.v2` file the v2 broker reads to
//! discover the daemon, its `per_version_binary_dir` allow-list root,
//! and its version policy (`min_version` + `version_allow_list`). The v2
//! adoption is mandatory: there is no direct daemon-acquisition lane, opt-in
//! switch, or client-owned placement path.

use crate::daemon::backend_handle_adoption::{
    broker_route_identity, SOLDR_DAEMON_IMAGE_HASH_LABEL, SOLDR_DAEMON_SERVICE_VERSION,
};
use running_process::broker::protocol_v2::{
    service_definition_dir_v2, service_definition_path_v2, BrokerIsolation, ServiceDefinition,
    ServiceDefinitionBuilder,
};
use std::io;
use std::path::{Path, PathBuf};

pub const SOLDR_ROOT_SERVICE_LABEL: &str = "soldr-root";
pub const SOLDR_DAEMON_ENV_LABEL_PREFIX: &str = "soldr-env:";

/// Stable, user-scoped storage owned by the singleton broker rather than by
/// any daemon route.  Daemon images are content/version addressed below this
/// root and can therefore be shared by distinct `SOLDR_CACHE_DIR` partitions;
/// daemon state and sockets remain rooted in each route's [`SoldrPaths`].
pub fn broker_owned_paths() -> crate::core::SoldrPaths {
    let services = service_definition_dir_v2();
    let owner = services.parent().unwrap_or(&services);
    crate::core::SoldrPaths::with_root(owner.join("soldr-broker"))
}

/// Work intentionally left out of the v2 adoption slice, tracked under
/// soldr#1495 Workstream B: the broker-owned `UpgradeDaemon` graceful
/// handoff cannot land until running-process ships the singleton-mode
/// Phase 4b machinery (the v2 adopt path is a stub today).
pub const SOLDR_DAEMON_SERVICE_DEF_DEFERRED: &[&str] = &[
    "broker-owned UpgradeDaemon singleton handoff (upstream Phase 4b)",
    "broker-mediated backend routing once adopt returns a real endpoint",
];

#[derive(Debug, Clone)]
pub struct InstalledServiceDefinition {
    pub path: PathBuf,
    pub definition: ServiceDefinition,
}

pub(crate) fn sibling_daemon_binary(current: &Path) -> PathBuf {
    let stem =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            "soldr-daemon.exe"
        } else {
            "soldr-daemon"
        };
    current
        .parent()
        .map(|p| p.join(stem))
        .unwrap_or_else(|| PathBuf::from(stem))
}

pub(crate) fn default_daemon_binary() -> io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    Ok(sibling_daemon_binary(&current))
}

/// Build the soldr-daemon v2 `SHARED_BROKER` service definition. The
/// `per_version_binary_dir` allow-list root is the directory holding the
/// (canonicalized) daemon binary — matching soldr's version-rooted
/// runtime relocation tree — and the version policy pins exactly this
/// build's `CARGO_PKG_VERSION` so the broker refuses any other version's
/// Hello.
pub(crate) fn soldr_daemon_service_definition(
    daemon_binary: &Path,
) -> io::Result<ServiceDefinition> {
    let paths = crate::core::SoldrPaths::new().map_err(|err| io::Error::other(err.to_string()))?;
    soldr_daemon_service_definition_for_paths(&paths, daemon_binary)
}

pub(crate) fn soldr_daemon_service_definition_for_paths(
    paths: &crate::core::SoldrPaths,
    daemon_binary: &Path,
) -> io::Result<ServiceDefinition> {
    let binary = std::fs::canonicalize(daemon_binary)?;
    let binary_dir = binary.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "soldr-daemon binary path has no parent directory",
        )
    })?;

    let route = broker_route_identity(paths, &binary)?;
    let root = paths.root.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Soldr root is not valid Unicode and cannot be routed by the broker",
        )
    })?;
    let service_name = route.service_name.clone();
    let mut definition =
        ServiceDefinitionBuilder::shared_broker(route.service_name, binary.display().to_string())
            .per_version_binary_dir(binary_dir.display().to_string())
            .min_version(SOLDR_DAEMON_SERVICE_VERSION)
            .version_allow_list([SOLDR_DAEMON_SERVICE_VERSION])
            .label(SOLDR_ROOT_SERVICE_LABEL, root)
            .label(SOLDR_DAEMON_IMAGE_HASH_LABEL, route.image_hash)
            .label("vendor", "zackees")
            .label("package", "soldr")
            .label("running-process-tracker", "zackees/soldr#1495")
            .build();
    add_daemon_env_labels(
        &mut definition,
        crate::daemon::lifecycle::forwarded_soldr_env(),
    )?;
    // soldr#3374: the daemon keys its per-generation route claim by this
    // name, and so does every front door reading it. Pin the route's own
    // name rather than whatever the registering process happened to export
    // (often nothing yet, or a different generation's route).
    add_daemon_env_labels(
        &mut definition,
        [(
            crate::daemon::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR.into(),
            service_name.into(),
        )],
    )?;
    // soldr#3882: the daemon adopts only an idle store its own image
    // lineage left behind, never another soldr's (CI's bootstrap store).
    add_daemon_env_labels(
        &mut definition,
        [(
            crate::zccache_embedded::SOLDR_DAEMON_IMAGE_SOURCE_ENV_VAR.into(),
            binary.display().to_string().into(),
        )],
    )?;

    debug_assert_eq!(definition.isolation, BrokerIsolation::SharedBroker as i32);
    Ok(definition)
}

fn add_daemon_env_labels(
    definition: &mut ServiceDefinition,
    env: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> io::Result<()> {
    for (name, value) in env {
        let name = name.into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "daemon environment variable name is not valid Unicode",
            )
        })?;
        let value = value.into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("daemon environment value for {name} is not valid Unicode"),
            )
        })?;
        definition
            .labels
            .insert(format!("{SOLDR_DAEMON_ENV_LABEL_PREFIX}{name}"), value);
    }
    Ok(())
}

pub fn daemon_env_from_service_definition(
    definition: &ServiceDefinition,
) -> io::Result<Vec<(std::ffi::OsString, std::ffi::OsString)>> {
    daemon_env_from_labels(&definition.labels)
}

pub fn daemon_env_from_labels(
    labels: &std::collections::HashMap<String, String>,
) -> io::Result<Vec<(std::ffi::OsString, std::ffi::OsString)>> {
    let mut env = Vec::new();
    for (label, value) in labels {
        let Some(name) = label.strip_prefix(SOLDR_DAEMON_ENV_LABEL_PREFIX) else {
            continue;
        };
        if name.is_empty()
            || !crate::daemon::lifecycle::forwarded_env_name(std::ffi::OsStr::new(name))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid daemon environment label {label:?}"),
            ));
        }
        env.push((name.into(), value.into()));
    }
    Ok(env)
}

fn servicedef_io_error(err: impl std::fmt::Display) -> io::Error {
    io::Error::other(err.to_string())
}

pub fn install_default_service_definition() -> io::Result<InstalledServiceDefinition> {
    install_service_definition(&default_daemon_binary()?)
}

pub fn install_service_definition(daemon_binary: &Path) -> io::Result<InstalledServiceDefinition> {
    install_service_definition_to_dir(service_definition_dir_v2(), daemon_binary)
}

pub fn install_service_definition_to_dir(
    service_root: impl AsRef<Path>,
    daemon_binary: &Path,
) -> io::Result<InstalledServiceDefinition> {
    let paths = crate::core::SoldrPaths::new().map_err(|err| io::Error::other(err.to_string()))?;
    install_service_definition_to_dir_for_paths(service_root, &paths, daemon_binary)
}

pub fn install_service_definition_to_dir_for_paths(
    service_root: impl AsRef<Path>,
    paths: &crate::core::SoldrPaths,
    daemon_binary: &Path,
) -> io::Result<InstalledServiceDefinition> {
    let service_root = service_root.as_ref();
    let definition = soldr_daemon_service_definition_for_paths(paths, daemon_binary)?;
    // Creates the (privately-permissioned) dir, validates the service name,
    // and replaces the `.servicedef.v2` protobuf atomically (soldr#3172).
    let path = write_service_definition_v2_atomic(service_root, &definition)?;
    debug_assert_eq!(
        path,
        service_definition_path_v2(service_root, &definition.service_name)
            .expect("valid service name"),
    );
    Ok(InstalledServiceDefinition { path, definition })
}

/// Write one `.servicedef.v2` file so a concurrent reader never observes it
/// half-written (soldr#3172).
///
/// Every build front door re-registers the daemon, and upstream
/// `write_service_definition_v2` writes with `std::fs::write`, which truncates
/// the live file before refilling it. The broker loads that same file for the
/// other `soldr ci-test` stages running in parallel, and a load landing between
/// the truncate and the write decodes zero bytes as a definition named `""`:
/// `service-definition requested "soldr-daemon-<hash>" but file declares ""`,
/// answered with `retry_after_ms=0`, so the stage fails outright.
///
/// Upstream's directory and name validation are kept. An identical file is
/// left alone, which is the common case for a re-registration; anything else is
/// written to a hidden temp file in the same directory and renamed over the
/// original, so readers see the old bytes or the new ones and never neither.
/// The temp name lacks the `.servicedef.v2` extension, so a scan ignores it.
fn write_service_definition_v2_atomic(
    root: &Path,
    definition: &ServiceDefinition,
) -> io::Result<PathBuf> {
    use prost::Message as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

    running_process::broker::server::ensure_service_definition_dir(root)
        .map_err(servicedef_io_error)?;
    let path =
        service_definition_path_v2(root, &definition.service_name).map_err(servicedef_io_error)?;
    let bytes = definition.encode_to_vec();
    if std::fs::read(&path).is_ok_and(|existing| existing == bytes) {
        // soldr#3251: an unchanged re-registration still renews it. The broker
        // reclaims registrations nobody has renewed within its protection
        // window, so a front door re-registering a daemon right before asking
        // for it must not look abandoned. Best effort: an unrenewable
        // registration can only be reclaimed early, and the next
        // re-registration writes it back.
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(std::time::SystemTime::now()));
        return Ok(path);
    }
    let temp = root.join(format!(
        ".{}.{}-{}.tmp",
        definition.service_name,
        std::process::id(),
        TEMP_NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&temp, &bytes)?;
    if let Err(error) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process::broker::protocol_v2::ServiceDefinitionLoader;
    use tempfile::TempDir;

    fn fake_daemon_binary(root: &Path) -> PathBuf {
        let binary = root.join(
            if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
                "soldr-daemon.exe"
            } else {
                "soldr-daemon"
            },
        );
        std::fs::write(&binary, b"stub").expect("fake daemon binary");
        binary
    }

    #[test]
    fn service_definition_declares_soldr_daemon_shared_broker() {
        let temp = TempDir::new().expect("tempdir");
        let daemon = fake_daemon_binary(temp.path());

        let definition = soldr_daemon_service_definition(&daemon).expect("definition");

        assert!(definition.service_name.starts_with("soldr-daemon-"));
        assert_eq!(definition.isolation, BrokerIsolation::SharedBroker as i32);
        assert_eq!(definition.min_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(definition.version_allow_list, [env!("CARGO_PKG_VERSION")]);
        assert_eq!(
            definition.labels.get("package").map(String::as_str),
            Some("soldr"),
        );
        assert!(definition.labels.contains_key(SOLDR_ROOT_SERVICE_LABEL));
    }

    #[test]
    fn daemon_environment_is_owned_by_each_route_registration() {
        let mut definition = ServiceDefinitionBuilder::shared_broker("svc", "/bin/svc").build();
        add_daemon_env_labels(
            &mut definition,
            [
                ("SOLDR_JOBS".into(), "2".into()),
                ("ZCCACHE_STAGING_DIR".into(), "/tmp/staging".into()),
            ],
        )
        .expect("encode daemon env");

        let decoded = daemon_env_from_service_definition(&definition).expect("decode daemon env");
        assert!(decoded.contains(&("SOLDR_JOBS".into(), "2".into())));
        assert!(decoded.contains(&("ZCCACHE_STAGING_DIR".into(), "/tmp/staging".into())));
    }

    #[test]
    fn daemon_environment_labels_reject_unapproved_names() {
        let mut definition = ServiceDefinitionBuilder::shared_broker("svc", "/bin/svc").build();
        definition.labels.insert(
            format!("{SOLDR_DAEMON_ENV_LABEL_PREFIX}PATH"),
            "/untrusted".into(),
        );
        let err = daemon_env_from_service_definition(&definition)
            .expect_err("PATH must not cross the broker registration boundary");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn install_service_definition_writes_v2_loader_compatible_protobuf() {
        let temp = TempDir::new().expect("tempdir");
        let service_root = temp.path().join("services");
        let daemon = fake_daemon_binary(temp.path());

        let installed = install_service_definition_to_dir(&service_root, &daemon)
            .expect("install service definition");

        // v2 files carry the `.servicedef.v2` extension.
        assert_eq!(
            installed.path,
            service_root.join(format!(
                "{}.servicedef.v2",
                installed.definition.service_name
            ))
        );
        let loaded = ServiceDefinitionLoader::new(&service_root)
            .load(&installed.definition.service_name)
            .expect("load service definition");
        assert_eq!(loaded, installed.definition);
    }

    /// soldr#3251: the broker reclaims registrations nobody has renewed within
    /// its protection window, so re-registering an unchanged definition, which
    /// skips the write, must still renew the registration's time.
    #[test]
    fn an_unchanged_reregistration_renews_the_registration_time() {
        let temp = TempDir::new().expect("tempdir");
        let service_root = temp.path().join("services");
        let binary = fake_daemon_binary(temp.path());
        let definition = ServiceDefinitionBuilder::shared_broker(
            "soldr-daemon-renewal",
            binary.display().to_string(),
        )
        .build();
        let path = write_service_definition_v2_atomic(&service_root, &definition).expect("write");
        let aged = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        filetime::set_file_mtime(&path, aged).expect("age the registration");
        let bytes_before = std::fs::read(&path).expect("read registration");

        let renewed =
            write_service_definition_v2_atomic(&service_root, &definition).expect("re-register");

        assert_eq!(renewed, path);
        assert_eq!(
            std::fs::read(&path).expect("read registration"),
            bytes_before,
            "an unchanged re-registration must not rewrite the definition"
        );
        let modified = filetime::FileTime::from_last_modification_time(
            &std::fs::metadata(&path).expect("registration metadata"),
        );
        assert!(
            modified > aged,
            "an unchanged re-registration must renew the registration time"
        );
    }

    /// soldr#3172: a broker loading the definition while a front door
    /// re-registers it must never see an empty or partial file.
    #[test]
    fn a_concurrent_loader_never_reads_a_half_written_service_definition() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let temp = TempDir::new().expect("tempdir");
        let service_root = temp.path().join("services");
        let binary = fake_daemon_binary(temp.path());
        let service_name = "soldr-daemon-servicedef-race";
        // Two definitions under one name, so every write really rewrites.
        let variant = |tag: &str| {
            let mut definition =
                ServiceDefinitionBuilder::shared_broker(service_name, binary.display().to_string())
                    .build();
            definition.labels.insert("variant".into(), tag.into());
            definition
        };
        let (first, second) = (variant("first"), variant("second"));
        write_service_definition_v2_atomic(&service_root, &first).expect("initial write");

        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = Arc::clone(&stop);
            let service_root = service_root.clone();
            std::thread::spawn(move || {
                let loader = ServiceDefinitionLoader::new(&service_root);
                let (mut loads, mut failures) = (0usize, Vec::new());
                while !stop.load(Ordering::Relaxed) {
                    loads += 1;
                    if let Err(error) = loader.load(service_name) {
                        failures.push(error.to_string());
                    }
                }
                (loads, failures)
            })
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut writes = 0usize;
        while std::time::Instant::now() < deadline {
            let definition = if writes.is_multiple_of(2) {
                &second
            } else {
                &first
            };
            write_service_definition_v2_atomic(&service_root, definition).expect("rewrite");
            writes += 1;
        }
        stop.store(true, Ordering::Relaxed);
        let (loads, failures) = reader.join().expect("reader thread");

        assert!(loads > 0 && writes > 0, "loads={loads} writes={writes}");
        assert!(
            failures.is_empty(),
            "{} of {loads} loads during {writes} rewrites saw a partial file; first: {}",
            failures.len(),
            failures[0]
        );
        // Every temp file was renamed into place: nothing is left beside it.
        let mut entries: Vec<String> = std::fs::read_dir(&service_root)
            .expect("read service root")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        entries.sort();
        assert_eq!(entries, [format!("{service_name}.servicedef.v2")]);
    }

    #[test]
    fn deferred_items_document_upstream_gated_boundary() {
        // The v2 discovery + servicedef wiring is DONE (this slice); what
        // remains deferred is the upstream-gated broker-owned handoff.
        assert!(SOLDR_DAEMON_SERVICE_DEF_DEFERRED
            .iter()
            .any(|item| item.contains("UpgradeDaemon")));
        assert!(!SOLDR_DAEMON_SERVICE_DEF_DEFERRED
            .iter()
            .any(|item| item.contains("connect_to_backend wiring")));
    }
}
