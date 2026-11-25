use running_process::broker::protocol::Endpoint;
use std::path::{Path, PathBuf};

use crate::platform::host::facts::HostOs;
use crate::platform::ipc::endpoint::{
    windows_pipe_from_executable_with_suffix, WINDOWS_PIPE_PREFIX,
};

/// Why the physical endpoint differs from the canonical logical socket path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrokerEndpointFallback {
    /// The Unix logical path, including its trailing NUL, exceeds `sun_path`.
    UnixSunPathOverflow,
    /// The home-anchored broker directory is on a filesystem that cannot
    /// safely host this singleton socket.
    UnixNonBindableFilesystem,
    /// The injective Windows leaf would exceed the complete pipe-name limit.
    WindowsPipeOverflow,
}

/// One authoritative broker resolution shared by bind, dial, admin, and
/// resurrection code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedBrokerEndpoint {
    /// Exact home-anchored executable staged and launched by front doors.
    pub executable_path: PathBuf,
    /// Canonical logical socket identity derived from `executable_path`.
    pub logical_socket_path: String,
    /// Complete platform endpoint used for bind and connect.
    pub bind_endpoint: String,
    /// Bare Windows pipe leaf for APIs that reject `\\.\pipe\`; absent on Unix.
    pub windows_pipe_leaf: Option<String>,
    /// The pre-fallback percent-encoded Windows leaf, when it overflowed.
    pub oversized_windows_pipe_leaf: Option<String>,
    /// Deterministic bind-location fallback, if selected.
    pub fallback: Option<BrokerEndpointFallback>,
    /// Machine-local fenced-resurrection lease database.
    pub lease_database_path: PathBuf,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct DoctorBrokerEndpoint {
    pub(crate) executable_path: Option<String>,
    pub(crate) logical_socket_path: Option<String>,
    pub(crate) bind_endpoint: Option<String>,
    pub(crate) fallback: Option<String>,
    pub(crate) resolution_error: Option<String>,
}

pub(crate) fn doctor_endpoint() -> DoctorBrokerEndpoint {
    match ResolvedBrokerEndpoint::resolve() {
        Ok(endpoint) => DoctorBrokerEndpoint {
            executable_path: Some(endpoint.executable_path.display().to_string()),
            logical_socket_path: Some(endpoint.logical_socket_path),
            bind_endpoint: Some(endpoint.bind_endpoint),
            fallback: endpoint.fallback.map(|value| format!("{value:?}")),
            resolution_error: None,
        },
        Err(error) => DoctorBrokerEndpoint {
            executable_path: None,
            logical_socket_path: None,
            bind_endpoint: None,
            fallback: None,
            resolution_error: Some(error.to_string()),
        },
    }
}

pub(crate) fn print_doctor_endpoint() {
    let endpoint = doctor_endpoint();
    println!("\nbroker endpoint:");
    if let Some(error) = endpoint.resolution_error {
        println!("  resolution error: {error}");
        return;
    }
    println!(
        "  executable: {}",
        endpoint.executable_path.as_deref().unwrap_or("unavailable")
    );
    println!(
        "  logical:    {}",
        endpoint
            .logical_socket_path
            .as_deref()
            .unwrap_or("unavailable")
    );
    println!(
        "  bind:       {}",
        endpoint.bind_endpoint.as_deref().unwrap_or("unavailable")
    );
    if let Some(fallback) = endpoint.fallback {
        println!("  fallback:   {fallback}");
    }
}

impl ResolvedBrokerEndpoint {
    /// Resolve the running user's one Soldr broker installation.
    pub fn resolve() -> Result<Self, BrokerIdentityError> {
        let home = crate::core::user_home_dir()
            .map_err(|error| BrokerIdentityError::Home(error.to_string()))?;
        if !home.is_absolute() {
            return Err(BrokerIdentityError::RelativeHome(home));
        }

        // The facade HostOs selects the branch; the concrete naming
        // primitives live in the platform crate.
        if crate::platform::host::facts::os() == HostOs::Windows {
            let executable = authoritative_broker_executable(&home, "soldr-broker.exe");
            let local_app_data = std::env::var_os("LOCALAPPDATA")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData").join("Local"));
            resolve_windows_for_executable(&executable, &local_app_data)
        } else {
            let executable = authoritative_broker_executable(&home, "soldr-broker");
            let runtime = crate::platform::ipc::endpoint::machine_runtime_dir();
            resolve_unix_for_executable(
                &executable,
                &runtime,
                None,
                crate::platform::ipc::endpoint::sun_path_capacity(),
            )
        }
    }

    /// Create every broker-owned directory with owner-only permissions.
    /// Resolution itself remains read-only so compiler-wrapper dials do not
    /// mutate filesystem state.
    pub fn create_owner_only_directories(&self) -> Result<(), BrokerIdentityError> {
        let mut directories = Vec::new();
        if let Some(parent) = self.executable_path.parent() {
            directories.push(parent.to_path_buf());
        }
        if crate::platform::host::facts::os() != HostOs::Windows {
            if let Some(parent) = Path::new(&self.bind_endpoint).parent() {
                directories.push(parent.to_path_buf());
            }
        }
        if let Some(parent) = self.lease_database_path.parent() {
            directories.push(parent.to_path_buf());
        }

        directories.sort();
        directories.dedup();
        for directory in directories {
            std::fs::create_dir_all(&directory).map_err(|source| {
                BrokerIdentityError::CreateDirectory {
                    path: directory.clone(),
                    source,
                }
            })?;
            // soldr#2477: the staged broker executable and the resurrection
            // lease are integrity boundaries, so the privacy policy must
            // hold on every platform. `make_private` is a no-op on Windows
            // (NTFS ACLs, not mode bits), which left these directories on
            // the ambient inherited DACL. running-process's secure_dir owns
            // the tested cross-platform policy: 0700 on Unix, a protected
            // owner+SYSTEM DACL on Windows.
            running_process::broker::secure_dir::ensure_private_dir(&directory).map_err(
                |source| BrokerIdentityError::SecureDirectory {
                    path: directory.clone(),
                    source,
                },
            )?;
        }
        Ok(())
    }

    /// Render the fallback decision with every value needed to diagnose it.
    pub fn fallback_diagnostic(&self) -> Option<String> {
        let fallback = self.fallback?;
        let mut message = format!(
            "broker endpoint fallback={fallback:?} executable={} logical={} bind={}",
            self.executable_path.display(),
            self.logical_socket_path,
            self.bind_endpoint
        );
        if let Some(oversized) = &self.oversized_windows_pipe_leaf {
            message.push_str(&format!(" oversized_leaf={oversized}"));
        }
        Some(message)
    }
}

/// The installed image is always derived from the expanded user home. The
/// detached-spawn path preserves the resolver's small environment input set,
/// so a broker launched from any other binary path cannot create an alternate
/// public endpoint merely because its executable leaf happens to match.
#[doc(hidden)]
pub fn authoritative_broker_executable(home: &Path, installed_leaf: &str) -> PathBuf {
    canonicalize_existing_ancestor(&home.join(".soldr").join("broker").join(installed_leaf))
}

/// Canonicalize the deepest existing ancestor while preserving the not-yet-
/// created broker suffix. This makes a symlinked home spelling and its real
/// path converge before either the executable or socket exists.
fn canonicalize_existing_ancestor(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        let Some(leaf) = existing.file_name() else {
            return path.to_path_buf();
        };
        suffix.push(leaf.to_os_string());
        let Some(parent) = existing.parent() else {
            return path.to_path_buf();
        };
        existing = parent;
    }
    let Ok(mut canonical) = std::fs::canonicalize(existing) else {
        return path.to_path_buf();
    };
    for component in suffix.into_iter().rev() {
        canonical.push(component);
    }
    canonical
}

/// Endpoint derivation failures.
#[derive(Debug, thiserror::Error)]
pub enum BrokerIdentityError {
    /// No authoritative user home could be resolved.
    #[error("could not resolve the user home for the broker installation: {0}")]
    Home(String),
    /// A relative home would make endpoint identity depend on cwd.
    #[error("broker home must be absolute, got {0:?}")]
    RelativeHome(PathBuf),
    /// The authoritative executable path is not an absolute Windows path.
    #[error("Windows broker executable path must be absolute: {0}")]
    RelativeWindowsExecutable(String),
    /// A lexical parent component is never safe in canonical identity.
    #[error("Windows broker executable path contains unresolved '..': {0}")]
    WindowsParentComponent(String),
    /// The executable leaf does not end in `.exe`.
    #[error("Windows broker executable path must end in .exe: {0}")]
    MissingWindowsExeExtension(String),
    /// The Windows path prefix was malformed or unsupported.
    #[error("unsupported Windows broker executable path: {0}")]
    UnsupportedWindowsPath(String),
    /// An owner-only broker directory could not be created.
    #[error("could not create broker directory {path:?}: {source}")]
    CreateDirectory {
        /// Directory that failed.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// Broker directory permissions could not be restricted.
    #[error("could not set owner-only permissions on {path:?}: {source}")]
    SecureDirectory {
        /// Directory that failed.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A path-derived endpoint could not be represented by running-process.
    #[error("could not construct path-derived endpoint: {0}")]
    Endpoint(String),
}

/// Pure Unix resolver seam. `non_bindable_override` and `sun_path_capacity`
/// let integration tests prove both deterministic fallback branches without
/// a real NFS mount or a second platform.
#[doc(hidden)]
pub fn resolve_unix_for_home(
    home: &Path,
    machine_runtime_dir: &Path,
    non_bindable_override: Option<bool>,
    sun_path_capacity: usize,
) -> Result<ResolvedBrokerEndpoint, BrokerIdentityError> {
    let executable = home.join(".soldr").join("broker").join("soldr-broker");
    resolve_unix_for_executable(
        &executable,
        machine_runtime_dir,
        non_bindable_override,
        sun_path_capacity,
    )
}

#[doc(hidden)]
pub fn resolve_unix_for_executable(
    executable: &Path,
    machine_runtime_dir: &Path,
    non_bindable_override: Option<bool>,
    sun_path_capacity: usize,
) -> Result<ResolvedBrokerEndpoint, BrokerIdentityError> {
    let executable = executable.to_path_buf();
    let logical = executable.with_file_name("soldr-broker.sock");
    let path_key = identity_key(&unix_path_bytes(&logical));
    let lease_file = format!("broker-lease-{path_key}.sqlite3");
    let fallback_leaf = format!("soldr-broker-{path_key}.sock");
    let fallback_candidates = [
        machine_runtime_dir
            .join("soldr")
            .join("broker")
            .join(&fallback_leaf),
        PathBuf::from(format!("/tmp/soldr-{}", crate::platform::host::user::uid()))
            .join("broker")
            .join(&fallback_leaf),
    ];
    let fallback_bind = fallback_candidates
        .into_iter()
        .find(|path| crate::platform::ipc::endpoint::socket_path_fits(path, sun_path_capacity))
        .ok_or_else(|| {
            BrokerIdentityError::Endpoint(format!(
                "no Unix socket path can represent broker executable {}",
                executable.display()
            ))
        })?;
    let local_broker_dir = fallback_bind
        .parent()
        .expect("broker fallback has a parent")
        .to_path_buf();
    let logical_fits =
        crate::platform::ipc::endpoint::socket_path_fits(&logical, sun_path_capacity);
    let non_bindable = non_bindable_override.unwrap_or_else(|| {
        crate::platform::ipc::endpoint::path_is_on_non_bindable_filesystem(&logical)
    });

    let (bind, fallback) = if !logical_fits {
        (
            fallback_bind,
            Some(BrokerEndpointFallback::UnixSunPathOverflow),
        )
    } else if non_bindable {
        (
            fallback_bind,
            Some(BrokerEndpointFallback::UnixNonBindableFilesystem),
        )
    } else {
        (logical.clone(), None)
    };

    Ok(ResolvedBrokerEndpoint {
        executable_path: executable,
        logical_socket_path: logical.to_string_lossy().into_owned(),
        bind_endpoint: bind.to_string_lossy().into_owned(),
        windows_pipe_leaf: None,
        oversized_windows_pipe_leaf: None,
        fallback,
        lease_database_path: local_broker_dir.join(lease_file),
    })
}

/// The raw identity bytes for a socket path (the platform owns the
/// representation; on Unix these are the exact `OsStr` bytes).
fn unix_path_bytes(path: &Path) -> Vec<u8> {
    crate::platform::ipc::endpoint::socket_path_bytes(path)
}

fn resolve_windows_for_executable(
    executable: &Path,
    local_app_data: &Path,
) -> Result<ResolvedBrokerEndpoint, BrokerIdentityError> {
    let pipe = windows_pipe_from_executable_with_suffix(
        &executable.to_string_lossy(),
        ".sock",
        "soldr-broker",
    )
    .map_err(broker_windows_error)?;
    let lease_file = format!(
        "broker-lease-{}.sqlite3",
        identity_key(pipe.logical_socket_path.as_bytes())
    );
    Ok(ResolvedBrokerEndpoint {
        executable_path: executable.to_path_buf(),
        logical_socket_path: pipe.logical_socket_path,
        bind_endpoint: format!("{WINDOWS_PIPE_PREFIX}{}", pipe.pipe_leaf),
        windows_pipe_leaf: Some(pipe.pipe_leaf),
        oversized_windows_pipe_leaf: pipe.oversized_leaf,
        fallback: pipe
            .overflowed
            .then_some(BrokerEndpointFallback::WindowsPipeOverflow),
        lease_database_path: local_app_data.join("soldr").join("broker").join(lease_file),
    })
}

/// Map the platform's neutral windows-path derivation errors back onto
/// the broker-identity error vocabulary.
#[doc(hidden)]
pub fn broker_windows_error(message: String) -> BrokerIdentityError {
    if message.contains("must be absolute") {
        BrokerIdentityError::RelativeWindowsExecutable(message)
    } else if message.contains("'..'") {
        BrokerIdentityError::WindowsParentComponent(message)
    } else if message.contains(".exe") {
        BrokerIdentityError::MissingWindowsExeExtension(message)
    } else {
        BrokerIdentityError::UnsupportedWindowsPath(message)
    }
}

#[doc(hidden)]
pub fn identity_key(identity: &[u8]) -> String {
    let digest = kernal_api::hash::blake3_bytes(identity);
    digest.to_hex()[..16].to_string()
}

/// Canonicalize and encode a Windows broker executable path without relying
/// on host-platform `Path` parsing, so the complete Windows matrix runs under
/// the Linux development harness too.
#[doc(hidden)]
pub fn windows_broker_pipe_from_executable(
    executable: &str,
) -> Result<crate::platform::ipc::endpoint::WindowsPipeName, BrokerIdentityError> {
    windows_pipe_from_executable_with_suffix(executable, ".sock", "soldr-broker")
        .map_err(broker_windows_error)
}

/// Derive a private daemon SESSION endpoint solely from the canonical path of
/// the executable the broker will launch. No SID, separately queried user
/// identity, allocator index, or random token participates in the name.
pub fn daemon_session_endpoint_from_executable(
    executable: &Path,
) -> Result<Endpoint, BrokerIdentityError> {
    let executable = std::fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf());
    if !executable.is_absolute() {
        return Err(BrokerIdentityError::Endpoint(format!(
            "daemon executable path must be absolute: {}",
            executable.display()
        )));
    }
    let namespace_id = executable.to_string_lossy().into_owned();
    if crate::platform::host::facts::os() == HostOs::Windows {
        let pipe = windows_pipe_from_executable_with_suffix(
            &executable.to_string_lossy(),
            ".session.sock",
            "soldr-daemon-session",
        )
        .map_err(broker_windows_error)?;
        Endpoint::windows_pipe(namespace_id, pipe.pipe_leaf)
            .map_err(|error| BrokerIdentityError::Endpoint(error.to_string()))
    } else {
        let executable_leaf = executable
            .file_name()
            .and_then(|leaf| leaf.to_str())
            .ok_or_else(|| {
                BrokerIdentityError::Endpoint(format!(
                    "daemon executable has no UTF-8 file name: {}",
                    executable.display()
                ))
            })?;
        let capacity = crate::platform::ipc::endpoint::sun_path_capacity();
        let logical = executable.with_file_name(format!("{executable_leaf}.session.sock"));
        let bind = if crate::platform::ipc::endpoint::socket_path_fits(&logical, capacity) {
            logical
        } else {
            let broker = ResolvedBrokerEndpoint::resolve()?;
            let broker_parent = Path::new(&broker.bind_endpoint)
                .parent()
                .unwrap_or_else(|| Path::new("/tmp"));
            let key = identity_key(executable.to_string_lossy().as_bytes());
            let leaf = format!("d-{key}.session.sock");
            let candidates = [
                broker_parent.join(&leaf),
                crate::platform::ipc::endpoint::machine_runtime_dir()
                    .join("soldr")
                    .join("daemon")
                    .join(&leaf),
                PathBuf::from(format!("/tmp/soldr-{}", crate::platform::host::user::uid()))
                    .join("daemon")
                    .join(&leaf),
            ];
            candidates
                .into_iter()
                .find(|path| crate::platform::ipc::endpoint::socket_path_fits(path, capacity))
                .ok_or_else(|| {
                    BrokerIdentityError::Endpoint(format!(
                        "no Unix socket path can represent daemon executable {}",
                        executable.display()
                    ))
                })?
        };
        Endpoint::unix_socket(namespace_id, bind.to_string_lossy().into_owned())
            .map_err(|error| BrokerIdentityError::Endpoint(error.to_string()))
    }
}

#[cfg(test)]
mod secure_directory_tests {
    use super::*;

    /// soldr#2477: broker-owned directories must satisfy running-process's
    /// cross-platform privacy predicate — 0700 on Unix, the protected
    /// owner+SYSTEM DACL on Windows. Pre-creating the directories models the
    /// unsafe state: they exist with ambient inherited permissions, and
    /// `create_owner_only_directories` must repair them, not accept them.
    /// No cfg-gating: the same assertions run on every host.
    #[test]
    fn preexisting_broker_directories_are_repaired_to_private() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let exe_dir = tmp.path().join("broker-install");
        let lease_dir = tmp.path().join("broker-lease");
        let sock_dir = tmp.path().join("broker-sock");
        std::fs::create_dir_all(&exe_dir).expect("exe dir");
        std::fs::create_dir_all(&lease_dir).expect("lease dir");
        std::fs::create_dir_all(&sock_dir).expect("sock dir");

        let endpoint = ResolvedBrokerEndpoint {
            executable_path: exe_dir.join("soldr-broker.exe"),
            logical_socket_path: "test-logical".to_string(),
            bind_endpoint: sock_dir.join("broker.sock").display().to_string(),
            windows_pipe_leaf: None,
            oversized_windows_pipe_leaf: None,
            fallback: None,
            lease_database_path: lease_dir.join("lease.sqlite3"),
        };
        endpoint
            .create_owner_only_directories()
            .expect("secure broker directories");

        for directory in [&exe_dir, &lease_dir] {
            assert!(
                running_process::broker::secure_dir::private_dir_permissions_are_private(directory)
                    .expect("read permissions"),
                "directory must satisfy the private policy: {}",
                directory.display()
            );
        }
    }
}
