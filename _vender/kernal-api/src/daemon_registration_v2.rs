//! Frozen v2 service-definition semantics for application-owned rollout policy.
//!
//! The private substrate retains the established `.servicedef.v2` layout,
//! validation, owner-private directory behavior, and deliberately non-atomic
//! write. This facade deliberately owns no manifest, loader, negotiation,
//! endpoint, or runtime policy.

use std::path::{Path, PathBuf};

use running_process::daemon_registration_v2 as backend;

/// Failure while validating or persisting a frozen v2 service definition.
#[derive(Debug, thiserror::Error)]
pub enum DaemonRegistrationV2Error {
    /// A filesystem operation failed.
    #[error("daemon registration v2 I/O failed: {0}")]
    Io(#[source] std::io::Error),
    /// A service name violates frozen registration validation.
    #[error("invalid frozen v2 service name: {detail}")]
    InvalidName {
        /// Stable diagnostic supplied by the underlying validator.
        detail: String,
    },
    /// The service-definition directory was not private to its current user.
    #[error("v2 service-definition directory is not owner-private: {}", path.display())]
    InsecureDirectory {
        /// Directory rejected by the persistence layer.
        path: PathBuf,
    },
    /// A definition failed a frozen semantic validation not otherwise exposed.
    #[error("invalid frozen v2 service definition: {detail}")]
    InvalidDefinition {
        /// Stable diagnostic supplied by the underlying validator.
        detail: String,
    },
}

fn service_error(error: backend::ServiceDefinitionError) -> DaemonRegistrationV2Error {
    match error {
        backend::ServiceDefinitionError::Io(error) => DaemonRegistrationV2Error::Io(error),
        backend::ServiceDefinitionError::InvalidName(error) => {
            DaemonRegistrationV2Error::InvalidName {
                detail: error.to_string(),
            }
        }
        backend::ServiceDefinitionError::InsecureDirectory(path) => {
            DaemonRegistrationV2Error::InsecureDirectory { path }
        }
        other => DaemonRegistrationV2Error::InvalidDefinition {
            detail: other.to_string(),
        },
    }
}

/// Return the platform- or environment-selected v2 service-definition directory.
///
/// The directory remains the established `running-process/services` root; v2
/// uses a distinct file suffix so it can coexist with frozen v1 records.
#[must_use]
pub fn service_definition_directory() -> PathBuf {
    backend::service_definition_dir_v2()
}

/// Compute the v2 path for one service definition beneath `root`.
///
/// # Errors
///
/// Returns [`DaemonRegistrationV2Error::InvalidName`] when `service_name`
/// does not satisfy the frozen service-name policy.
pub fn service_definition_path(
    root: impl AsRef<Path>,
    service_name: impl AsRef<str>,
) -> Result<PathBuf, DaemonRegistrationV2Error> {
    backend::service_definition_path_v2(root.as_ref(), service_name.as_ref()).map_err(service_error)
}

/// Write a v2 service definition into an explicit owner-private directory.
///
/// The frozen v2 writer uses one direct non-atomic write. Callers needing a
/// different durability policy must own it above this compatibility surface.
pub fn write_service_definition(
    root: impl AsRef<Path>,
    definition: &ServiceDefinition,
) -> Result<PathBuf, DaemonRegistrationV2Error> {
    backend::write_service_definition_v2(root.as_ref(), &definition.inner).map_err(service_error)
}

/// Builder for the shared-broker frozen v2 service definition used by current consumers.
#[derive(Clone, Debug)]
pub struct ServiceDefinitionBuilder {
    service_name: String,
    binary_path: String,
    per_version_binary_dir: Option<String>,
    min_version: Option<String>,
    allowed_versions: Vec<String>,
    labels: Vec<(String, String)>,
}

impl ServiceDefinitionBuilder {
    /// Begin a shared-broker definition for an application-selected binary.
    #[must_use]
    pub fn shared_broker(service_name: impl Into<String>, binary_path: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            binary_path: binary_path.into(),
            per_version_binary_dir: None,
            min_version: None,
            allowed_versions: Vec::new(),
            labels: Vec::new(),
        }
    }

    /// Set the per-version binary directory retained in the v2 definition.
    #[must_use]
    pub fn per_version_binary_dir(mut self, directory: impl Into<String>) -> Self {
        self.per_version_binary_dir = Some(directory.into());
        self
    }

    /// Set the minimum compatible service version.
    #[must_use]
    pub fn min_version(mut self, version: impl Into<String>) -> Self {
        self.min_version = Some(version.into());
        self
    }

    /// Append one allowed service version, retaining caller order.
    #[must_use]
    pub fn allow_version(mut self, version: impl Into<String>) -> Self {
        self.allowed_versions.push(version.into());
        self
    }

    /// Attach one application-selected label without canonicalizing map order.
    #[must_use]
    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.push((key.into(), value.into()));
        self
    }

    /// Finalize a facade-owned v2 definition without persisting it.
    #[must_use]
    pub fn build(self) -> ServiceDefinition {
        let mut inner =
            backend::ServiceDefinitionBuilder::shared_broker(self.service_name, self.binary_path);
        if let Some(directory) = self.per_version_binary_dir {
            inner = inner.per_version_binary_dir(directory);
        }
        if let Some(version) = self.min_version {
            inner = inner.min_version(version);
        }
        inner = inner.version_allow_list(self.allowed_versions);
        for (key, value) in self.labels {
            inner = inner.label(key, value);
        }
        ServiceDefinition {
            inner: inner.build(),
        }
    }

    /// Write the definition into the default owner-private v2 directory.
    pub fn install(self) -> Result<PathBuf, DaemonRegistrationV2Error> {
        let root = service_definition_directory();
        self.install_in(root)
    }

    /// Write the definition into an explicit owner-private v2 directory.
    pub fn install_in(self, root: impl AsRef<Path>) -> Result<PathBuf, DaemonRegistrationV2Error> {
        write_service_definition(root, &self.build())
    }
}

/// A frozen v2 service definition with its generated record kept private.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceDefinition {
    inner: backend::ServiceDefinition,
}

impl ServiceDefinition {
    /// Service name retained in the v2 definition.
    #[must_use]
    pub fn service_name(&self) -> &str {
        &self.inner.service_name
    }

    /// Binary path string retained in the v2 definition.
    #[must_use]
    pub fn binary_path(&self) -> &str {
        &self.inner.binary_path
    }

    /// Whether this definition selects the shared-broker isolation mode.
    #[must_use]
    pub fn is_shared_broker(&self) -> bool {
        self.inner.isolation == backend::BrokerIsolation::SharedBroker as i32
    }

    /// Per-version binary directory retained in the v2 definition.
    #[must_use]
    pub fn per_version_binary_dir(&self) -> &str {
        &self.inner.per_version_binary_dir
    }

    /// Minimum compatible version retained in the v2 definition.
    #[must_use]
    pub fn min_version(&self) -> &str {
        &self.inner.min_version
    }

    /// Allowed versions in the caller-supplied order.
    pub fn allowed_versions(&self) -> impl ExactSizeIterator<Item = &str> {
        self.inner.version_allow_list.iter().map(String::as_str)
    }

    /// Return one application-selected label, if present.
    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.inner.labels.get(key).map(String::as_str)
    }

    /// Iterate the generated label map without imposing a canonical order.
    pub fn labels(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.inner
            .labels
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn facade_and_backend_keep_same_object_write_bytes_in_one_process() {
        let definition = ServiceDefinitionBuilder::shared_broker("service", "/bin/service")
            .per_version_binary_dir("/bin")
            .min_version("1.2.3")
            .allow_version("1.2.3")
            .allow_version("1.2.4")
            .label("vendor", "zackees")
            .label("package", "fixture")
            .build();
        let facade_root = tempdir().expect("facade root");
        let backend_root = tempdir().expect("backend root");

        let facade_path =
            write_service_definition(facade_root.path(), &definition).expect("facade write");
        let backend_path =
            backend::write_service_definition_v2(backend_root.path(), &definition.inner)
                .expect("backend write");

        assert_eq!(
            std::fs::read(facade_path).expect("facade bytes"),
            std::fs::read(backend_path).expect("backend bytes"),
            "the comparison intentionally uses the same object in one process; labels remain a HashMap and are not a canonical byte promise"
        );
    }
}
