//! Async client for the crash-isolated symbolization worker.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::AsyncWriteExt as _;

use super::wire::{RawCapture, SymbolReport};

/// Optional explicit path to the `kernal-symbolize` worker executable.
pub const SYMBOLIZER_WORKER_ENV: &str = "KERNAL_API_SYMBOLIZER";

/// Locate the worker from [`SYMBOLIZER_WORKER_ENV`] or beside this process.
///
/// Applications distribute the worker beside their executable, or set the
/// environment variable to a release asset installed elsewhere. A Cargo
/// library dependency intentionally does not smuggle parser code into the
/// long-lived application process.
pub fn default_worker_path() -> std::io::Result<PathBuf> {
    if let Some(path) = std::env::var_os(SYMBOLIZER_WORKER_ENV) {
        if !path.is_empty() {
            return Ok(path.into());
        }
    }
    let mut path = std::env::current_exe()?;
    path.set_file_name(if cfg!(windows) {
        "kernal-symbolize.exe"
    } else {
        "kernal-symbolize"
    });
    Ok(path)
}

/// A configured off-process symbolizer.
#[derive(Clone, Debug)]
pub struct SymbolizerWorker {
    executable: PathBuf,
}

impl SymbolizerWorker {
    /// Use an explicit worker executable.
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    /// Resolve the worker using [`default_worker_path`].
    pub fn discover() -> Result<Self, WorkerError> {
        Ok(Self::new(default_worker_path()?))
    }

    /// Worker executable used by this client.
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Symbolize one capture in a fresh, crash-isolated subprocess.
    pub async fn symbolize(&self, capture: &RawCapture) -> Result<SymbolReport, WorkerError> {
        let input = capture.encode_wire();
        let mut child = tokio::process::Command::new(&self.executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| WorkerError::Spawn {
                executable: self.executable.clone(),
                source,
            })?;

        let mut stdin = child.stdin.take().ok_or(WorkerError::MissingStdin)?;
        stdin.write_all(&input).await?;
        drop(stdin);

        let output = child.wait_with_output().await?;
        if !output.status.success() {
            return Err(WorkerError::Failed {
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(SymbolReport::decode_wire(&output.stdout)?)
    }
}

/// Failure to launch, communicate with, or decode the symbolizer worker.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// The default worker location could not be determined.
    #[error("cannot determine the symbolizer worker path: {0}")]
    Io(#[from] std::io::Error),
    /// The worker executable could not be started.
    #[error("cannot start symbolizer worker {executable}: {source}", executable = executable.display())]
    Spawn {
        /// Requested executable.
        executable: PathBuf,
        /// Operating-system failure.
        source: std::io::Error,
    },
    /// Tokio did not expose the piped standard input requested by the client.
    #[error("symbolizer worker started without piped stdin")]
    MissingStdin,
    /// The report did not match the stable protobuf schema.
    #[error("symbolizer wire payload is invalid: {0}")]
    Wire(#[from] super::wire::WireError),
    /// The isolated parser failed; the caller process remains healthy.
    #[error("symbolizer worker failed with status {status:?}: {stderr}")]
    Failed {
        /// Platform exit code, when one exists.
        status: Option<i32>,
        /// Bounded worker diagnostic.
        stderr: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_worker_has_the_platform_executable_suffix() {
        let worker = default_worker_path().unwrap();
        let expected = if cfg!(windows) {
            "kernal-symbolize.exe"
        } else {
            "kernal-symbolize"
        };
        assert_eq!(worker.file_name().unwrap(), expected);
    }
}
