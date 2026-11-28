//! Verification and control of a recorded daemon process.
//!
//! The substrate's PID verifier lives behind its broker client feature, which
//! would drag argument parsing, configuration, and broker IPC into every
//! direct daemon.  This module owns the same checks over this crate's own
//! host facade instead: host boot, process liveness, executable path, and the
//! executable's BLAKE3 digest.  A successful verification returns a
//! [`VerifiedDaemon`] that keeps the live process reference it was verified
//! through, so later liveness questions and a forced termination address that
//! instance rather than whatever process later reuses its PID.

use std::io;
use std::path::PathBuf;

use crate::platform::process::{
    ProcessIdentity, ProcessIdentityAction, ProcessIdentityActionError, ProcessIdentityCapture,
    ProcessIdentityUnavailable, ProcessInspectError, ProcessInspectErrorKind, ProcessLiveness,
};

use super::DaemonIdentity;

/// A daemon process whose recorded identity was verified against the host.
///
/// The value retains the host's live reference to the verified process (a
/// pidfd, a kqueue subscription, or an open process handle), so
/// [`Self::is_alive`] keeps describing that instance.  Values produced by
/// [`DaemonIdentity::verify_for_control`] also retain the process's creation
/// generation, which is what makes [`Self::force_kill`] refuse a recycled PID.
#[derive(Debug)]
pub struct VerifiedDaemon {
    liveness: ProcessLiveness,
    control: Option<ProcessIdentity>,
}

impl VerifiedDaemon {
    /// Operating-system process identifier of the verified daemon.
    pub fn pid(&self) -> u32 {
        self.liveness.pid()
    }

    /// Whether the verified daemon instance is still running.
    pub fn is_alive(&self) -> bool {
        self.liveness.is_alive()
    }

    /// Whether the verified daemon instance has exited, or why the host could
    /// not tell.
    ///
    /// [`Self::is_alive`] is a convenience predicate that reads any failed
    /// observation as "not alive". Cleanup that follows [`Self::force_kill`]
    /// -- removing a lock file, retiring an endpoint -- needs the stronger
    /// answer: `Ok(true)` only when the retained process reference observed
    /// the exit, `Ok(false)` while it is still running, and `Err` when the
    /// observation itself failed, which says nothing about the daemon.
    ///
    /// The question is asked through the reference retained at verification
    /// (a pidfd, a kqueue exit subscription, or an open process handle), never
    /// by reopening the PID, so a successor that reuses the PID is not
    /// mistaken for the daemon.
    ///
    /// # Errors
    ///
    /// Returns the host failure when the exit could not be observed.
    pub fn has_exited(&self) -> io::Result<bool> {
        self.liveness.has_exited()
    }

    /// Forcibly terminate exactly the verified daemon instance.
    ///
    /// Only values produced by [`DaemonIdentity::verify_for_control`] retain
    /// the creation generation this requires; others return
    /// [`DaemonVerifyError::ControlNotRetained`].  A daemon that has already
    /// exited reports [`ProcessIdentityAction::AlreadyExited`] and no
    /// replacement process is touched.
    pub fn force_kill(&self) -> Result<ProcessIdentityAction, DaemonVerifyError> {
        let pid = self.pid();
        let identity = self
            .control
            .ok_or(DaemonVerifyError::ControlNotRetained { pid })?;
        crate::platform::process::force_kill(identity).map_err(|error| match error {
            ProcessIdentityActionError::StaleIdentity => DaemonVerifyError::StaleProcess { pid },
            ProcessIdentityActionError::Unavailable(reason) => {
                DaemonVerifyError::ControlUnavailable { pid, reason }
            }
            ProcessIdentityActionError::Host(source) => DaemonVerifyError::Handle { pid, source },
        })
    }
}

/// Why a recorded daemon identity does not describe a live process.
#[derive(Debug, thiserror::Error)]
pub enum DaemonVerifyError {
    /// PID zero or a value outside the host's PID range is never valid.
    #[error("invalid daemon pid: {0}")]
    InvalidPid(u32),
    /// No process currently has the recorded PID.
    #[error("process not found: {pid}")]
    NotFound {
        /// Recorded process identifier.
        pid: u32,
    },
    /// The identity was recorded during another host boot.
    #[error("daemon boot id mismatch: expected {expected}, current {actual}")]
    BootIdMismatch {
        /// Boot identifier recorded with the daemon identity.
        expected: String,
        /// Current host boot identifier, or `unavailable`.
        actual: String,
    },
    /// The running process's executable path could not be read.
    #[error("failed to resolve executable path for pid {pid}: {source}")]
    ExecutablePath {
        /// Recorded process identifier.
        pid: u32,
        /// Host failure.
        source: io::Error,
    },
    /// The running process was started from a different executable path.
    #[error(
        "daemon executable path mismatch for pid {pid}: expected {expected:?}, actual {actual:?}"
    )]
    ExecutablePathMismatch {
        /// Recorded process identifier.
        pid: u32,
        /// Executable path recorded with the daemon identity.
        expected: PathBuf,
        /// Executable path reported by the host.
        actual: PathBuf,
    },
    /// The running process's executable could not be hashed.
    #[error("failed to hash executable for pid {pid} at {path:?}: {source}")]
    ExecutableHash {
        /// Recorded process identifier.
        pid: u32,
        /// Executable path that was hashed.
        path: PathBuf,
        /// Read or hashing failure.
        source: io::Error,
    },
    /// The executable's BLAKE3 digest differs from the recorded digest.
    #[error("daemon executable blake3 hash mismatch for pid {pid}")]
    ExecutableHashMismatch {
        /// Recorded process identifier.
        pid: u32,
    },
    /// A host process-reference operation failed.
    #[error("process handle operation failed for pid {pid}: {source}")]
    Handle {
        /// Recorded process identifier.
        pid: u32,
        /// Host failure.
        source: io::Error,
    },
    /// The PID now belongs to a different process instance.
    #[error("daemon pid {pid} was reused by a different process")]
    StaleProcess {
        /// Recorded process identifier.
        pid: u32,
    },
    /// The host could not capture the generation required for safe control.
    #[error("daemon pid {pid} cannot be controlled safely: {reason:?}")]
    ControlUnavailable {
        /// Recorded process identifier.
        pid: u32,
        /// Why the host could not name the process instance.
        reason: ProcessIdentityUnavailable,
    },
    /// Termination was requested from a liveness-only verification.
    #[error("daemon pid {pid} was verified for liveness only, not for control")]
    ControlNotRetained {
        /// Recorded process identifier.
        pid: u32,
    },
}

impl DaemonIdentity {
    /// Verify that this identity still describes a live process on this host.
    ///
    /// Checks, in order: the recorded PID is valid, the recorded boot matches
    /// the current host boot, the process is running, its executable path is
    /// the recorded one, and that executable's BLAKE3 digest is the recorded
    /// digest.  No endpoint connection is made.
    ///
    /// **Blocking.** The executable is read and hashed; run this on a
    /// blocking worker from asynchronous code.
    pub fn verify_live(&self) -> Result<VerifiedDaemon, DaemonVerifyError> {
        self.verify(false)
    }

    /// [`Self::verify_live`], additionally retaining the process generation
    /// that [`VerifiedDaemon::force_kill`] needs to terminate exactly this
    /// instance.
    ///
    /// Fails with [`DaemonVerifyError::ControlUnavailable`] where the host
    /// cannot name the process instance safely.
    pub fn verify_for_control(&self) -> Result<VerifiedDaemon, DaemonVerifyError> {
        self.verify(true)
    }

    fn verify(&self, for_control: bool) -> Result<VerifiedDaemon, DaemonVerifyError> {
        let pid = self.pid();
        if pid == 0 {
            return Err(DaemonVerifyError::InvalidPid(pid));
        }
        verify_boot(self.boot_id())?;

        let liveness = ProcessLiveness::open(pid).map_err(|error| inspect_error(pid, error))?;
        let actual_path = crate::process_executable_path(pid)
            .map_err(|source| DaemonVerifyError::ExecutablePath { pid, source })?;
        if !crate::platform::process::same_executable_path(&actual_path, self.executable_path()) {
            return Err(DaemonVerifyError::ExecutablePathMismatch {
                pid,
                expected: self.executable_path().to_path_buf(),
                actual: actual_path,
            });
        }
        let digest = crate::hash::blake3_file(
            &actual_path,
            crate::hash::Blake3ReadOptions::new().memory_map(true),
        )
        .map_err(|error| DaemonVerifyError::ExecutableHash {
            pid,
            path: actual_path.clone(),
            source: io::Error::other(error),
        })?;
        if digest.as_bytes() != self.blake3_digest() {
            return Err(DaemonVerifyError::ExecutableHashMismatch { pid });
        }

        // Capture the generation before the final liveness check: while the
        // retained reference still reports the verified process alive, its
        // PID cannot have been handed to a successor.
        let control = if for_control {
            Some(capture_generation(pid)?)
        } else {
            None
        };
        if !liveness.is_alive() {
            return Err(DaemonVerifyError::NotFound { pid });
        }
        Ok(VerifiedDaemon { liveness, control })
    }
}

/// Compare a recorded boot identifier with the current host boot.
///
/// An empty recorded value predates boot tracking and is accepted.  A host
/// that cannot name its boot fails closed for every recorded value, because
/// accepting it would let an identity from a prior boot authorize a PID that
/// has since been reissued.
fn verify_boot(expected: &str) -> Result<(), DaemonVerifyError> {
    if expected.is_empty() {
        return Ok(());
    }
    match crate::platform::host::boot_id() {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(DaemonVerifyError::BootIdMismatch {
            expected: expected.to_owned(),
            actual,
        }),
        None => Err(DaemonVerifyError::BootIdMismatch {
            expected: expected.to_owned(),
            actual: "unavailable".to_owned(),
        }),
    }
}

fn capture_generation(pid: u32) -> Result<ProcessIdentity, DaemonVerifyError> {
    match crate::platform::process::capture_identity(pid) {
        ProcessIdentityCapture::Found(identity) => Ok(identity),
        ProcessIdentityCapture::Exited => Err(DaemonVerifyError::NotFound { pid }),
        ProcessIdentityCapture::Unavailable(reason) => {
            Err(DaemonVerifyError::ControlUnavailable { pid, reason })
        }
        ProcessIdentityCapture::Error(source) => Err(DaemonVerifyError::Handle { pid, source }),
    }
}

fn inspect_error(pid: u32, error: ProcessInspectError) -> DaemonVerifyError {
    match error.kind {
        ProcessInspectErrorKind::InvalidPid => DaemonVerifyError::InvalidPid(pid),
        ProcessInspectErrorKind::NotFound => DaemonVerifyError::NotFound { pid },
        ProcessInspectErrorKind::Unsupported | ProcessInspectErrorKind::Host => {
            DaemonVerifyError::Handle {
                pid,
                source: error.source,
            }
        }
    }
}
