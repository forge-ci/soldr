//! Facade-owned independent resource placement for launched processes.
//!
//! `running-process` is a private backend like Tokio or any other
//! implementation crate: these types mirror its placement contract so callers
//! never name, depend on, or version-lock to a backend type. Conversions stay
//! private to this module, and [`SpawnHandle`] keeps the substrate's live
//! control handle in a private field.
//!
//! [`SpawnMode::Inherited`] is the default and preserves ordinary direct
//! spawning inside the caller's cgroup or Job Object. [`SpawnMode::Independent`]
//! is explicit: it requires a verified native scheduler or a pre-existing
//! broker already outside the caller's boundary. It never silently degrades to
//! inherited placement. Independent placement remains subject to user,
//! container, and machine-wide limits; it is neither privilege elevation nor a
//! container escape.
//!
//! For a persistent build daemon, provide an absolute program/cwd/logging
//! [`LaunchSpec`], select `SpawnMode::Independent` with an
//! [`IndependentBackend`], then retain [`SpawnHandle`] for reuse-safe stop or
//! wait operations. [`SpawnLifetime::Detached`] controls only what dropping that
//! handle does; it is distinct from resource placement. Readiness,
//! cancellation, authority, and unsupported-platform failures propagate as the
//! `std::io::Error` returned by [`spawn_with_options`].

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Which resource boundary owns a newly launched process.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SpawnMode {
    /// Preserve direct spawning and inherited cgroup/Job Object placement.
    #[default]
    Inherited,
    /// Require verified placement outside the requesting worker's boundary.
    /// Enclosing user, container and machine limits still apply.
    Independent,
}

/// Handle lifetime is not resource placement or owner-death binding.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SpawnLifetime {
    /// Stop when the returned handle is dropped normally. This is not an
    /// OS-level guarantee for abrupt termination of the handle's owner.
    #[default]
    KillOnDrop,
    /// Dropping the handle leaves a successfully committed process running.
    Detached,
}

/// Explicit independent-launch authority. No implicit fallback is permitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndependentBackend {
    /// Use a same-user native scheduler with this installed launcher binary.
    /// Availability and actual placement must be verified at launch time.
    NativeScheduler { launcher: PathBuf },
    /// Connect to an already-running broker outside the worker boundary.
    /// The launching process must never create this broker on demand.
    ExternalBroker { endpoint: String },
}

/// Spawn policy. Defaults do not require any external authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnOptions {
    pub mode: SpawnMode,
    pub lifetime: SpawnLifetime,
    /// Required for Independent, absent for Inherited. Conflicting selection
    /// is an error rather than silently ignoring an explicit backend choice.
    pub backend: Option<IndependentBackend>,
    /// Combined scheduling and application-readiness budget: nonzero and at
    /// most 30 seconds.
    pub timeout: Duration,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            mode: SpawnMode::Inherited,
            lifetime: SpawnLifetime::KillOnDrop,
            backend: None,
            timeout: Duration::from_secs(30),
        }
    }
}

/// Explicit target payload. No inherited environment or caller-owned pipe is
/// admitted: `environment` is the complete selected environment, and logging
/// accepts regular files only.
#[derive(Clone)]
pub struct LaunchSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: OsString,
    pub environment: Vec<(OsString, OsString)>,
    pub stdout: Option<OsString>,
    pub stderr: Option<OsString>,
    pub readiness: Readiness,
}

/// Application readiness is distinct from successful exec. A file marker must
/// be absent before launch and contain exactly the caller-selected bytes.
/// The caller owns the marker's path and eventual removal.
#[derive(Clone, Default)]
pub enum Readiness {
    #[default]
    ProcessStarted,
    File {
        path: OsString,
        value: Vec<u8>,
    },
}

/// Exit observation is not necessarily a parent-child exit status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpawnExit {
    /// Available for a directly spawned child; scheduler-owned exit observation
    /// deliberately does not invent a numeric exit status.
    pub code: Option<i32>,
}

/// A reuse-safe process control handle. The reported mode is actual enforcement.
///
/// On Unix, a directly spawned child's terminal status remains waitable until
/// this handle is dropped, preserving its process-group identity for control.
/// The application must leave child reaping to this handle: do not reap its PID
/// with an external waiter or install automatic SIGCHLD reaping while it is held.
/// Dropping the handle applies the selected [`SpawnLifetime`].
pub struct SpawnHandle {
    inner: running_process::SpawnHandle,
}

impl SpawnHandle {
    pub fn id(&self) -> u32 {
        self.inner.id()
    }

    pub fn actual_mode(&self) -> SpawnMode {
        facade_mode(self.inner.actual_mode())
    }

    pub fn is_alive(&mut self) -> io::Result<bool> {
        self.inner.is_alive()
    }

    pub fn stop(&mut self, timeout: Duration) -> io::Result<()> {
        self.inner.stop(timeout)
    }

    pub fn wait(&mut self, timeout: Duration, cancelled: &AtomicBool) -> io::Result<SpawnExit> {
        self.inner
            .wait(timeout, cancelled)
            .map(|exit| SpawnExit { code: exit.code })
    }
}

/// Spawn with explicit resource placement, lifetime, payload and deadline.
/// This operation never silently falls back or lazily starts a broker.
pub fn spawn_with_options(
    spec: &LaunchSpec,
    options: &SpawnOptions,
    cancelled: &AtomicBool,
) -> io::Result<SpawnHandle> {
    running_process::spawn_with_options(&backend_spec(spec), &backend_options(options), cancelled)
        .map(|inner| SpawnHandle { inner })
}

fn backend_mode(mode: SpawnMode) -> running_process::SpawnMode {
    match mode {
        SpawnMode::Inherited => running_process::SpawnMode::Inherited,
        SpawnMode::Independent => running_process::SpawnMode::Independent,
    }
}

fn facade_mode(mode: running_process::SpawnMode) -> SpawnMode {
    match mode {
        running_process::SpawnMode::Inherited => SpawnMode::Inherited,
        running_process::SpawnMode::Independent => SpawnMode::Independent,
    }
}

fn backend_options(options: &SpawnOptions) -> running_process::SpawnOptions {
    running_process::SpawnOptions {
        mode: backend_mode(options.mode),
        lifetime: match options.lifetime {
            SpawnLifetime::KillOnDrop => running_process::SpawnLifetime::KillOnDrop,
            SpawnLifetime::Detached => running_process::SpawnLifetime::Detached,
        },
        backend: options.backend.as_ref().map(|backend| match backend {
            IndependentBackend::NativeScheduler { launcher } => {
                running_process::IndependentBackend::NativeScheduler {
                    launcher: launcher.clone(),
                }
            }
            IndependentBackend::ExternalBroker { endpoint } => {
                running_process::IndependentBackend::ExternalBroker {
                    endpoint: endpoint.clone(),
                }
            }
        }),
        timeout: options.timeout,
    }
}

fn backend_spec(spec: &LaunchSpec) -> running_process::independent_spawn::LaunchSpec {
    running_process::independent_spawn::LaunchSpec {
        program: spec.program.clone(),
        args: spec.args.clone(),
        cwd: spec.cwd.clone(),
        environment: spec.environment.clone(),
        stdout: spec.stdout.clone(),
        stderr: spec.stderr.clone(),
        readiness: match &spec.readiness {
            Readiness::ProcessStarted => {
                running_process::independent_spawn::Readiness::ProcessStarted
            }
            Readiness::File { path, value } => {
                running_process::independent_spawn::Readiness::File {
                    path: path.clone(),
                    value: value.clone(),
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_convert_losslessly_in_both_directions() {
        for mode in [SpawnMode::Inherited, SpawnMode::Independent] {
            assert_eq!(facade_mode(backend_mode(mode)), mode);
        }
    }

    #[test]
    fn default_options_convert_to_the_substrate_defaults() {
        assert_eq!(
            backend_options(&SpawnOptions::default()),
            running_process::SpawnOptions::default()
        );
    }

    #[test]
    fn explicit_options_convert_field_for_field() {
        let options = SpawnOptions {
            mode: SpawnMode::Independent,
            lifetime: SpawnLifetime::Detached,
            backend: Some(IndependentBackend::ExternalBroker {
                endpoint: "broker".to_owned(),
            }),
            timeout: Duration::from_secs(7),
        };
        assert_eq!(
            backend_options(&options),
            running_process::SpawnOptions {
                mode: running_process::SpawnMode::Independent,
                lifetime: running_process::SpawnLifetime::Detached,
                backend: Some(running_process::IndependentBackend::ExternalBroker {
                    endpoint: "broker".to_owned(),
                }),
                timeout: Duration::from_secs(7),
            }
        );
    }
}
