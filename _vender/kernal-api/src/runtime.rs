//! Shared async-engine runtime diagnostics.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing_subscriber::prelude::*;

/// Product-neutral configuration for the Tokio Console server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticsConfig {
    bind: String,
    publish_interval: Option<Duration>,
    recording_path: Option<PathBuf>,
}

impl DiagnosticsConfig {
    /// Configure the server address clients connect to.
    pub fn new(bind: impl Into<String>) -> Self {
        Self {
            bind: bind.into(),
            publish_interval: None,
            recording_path: None,
        }
    }

    /// Override the interval between task-statistics publications.
    pub fn with_publish_interval(mut self, interval: Duration) -> Self {
        self.publish_interval = Some(interval);
        self
    }

    /// Record console events to a file as well as serving them live.
    pub fn with_recording_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.recording_path = Some(path.into());
        self
    }

    /// Configured bind address.
    pub fn bind(&self) -> &str {
        &self.bind
    }

    /// Configured publication interval, when overridden.
    pub fn publish_interval(&self) -> Option<Duration> {
        self.publish_interval
    }

    /// Configured event recording path, when present.
    pub fn recording_path(&self) -> Option<&Path> {
        self.recording_path.as_deref()
    }

    /// Install the console layer on the process-global tracing registry.
    ///
    /// `console-subscriber` panics when the binary was not compiled with
    /// `--cfg tokio_unstable`; the panic is converted to an actionable error
    /// so enabling diagnostics can never crash a production daemon.
    pub fn install(&self) -> Result<(), DiagnosticsInstallError> {
        let bind = self
            .bind
            .parse::<SocketAddr>()
            .map_err(|error| DiagnosticsInstallError::InvalidBind(error.to_string()))?;
        let mut builder = console_subscriber::Builder::default()
            .with_default_env()
            .server_addr(bind);
        if let Some(interval) = self.publish_interval {
            builder = builder.publish_interval(interval);
        }
        if let Some(path) = &self.recording_path {
            builder = builder.recording_path(path);
        }
        let layer = std::panic::catch_unwind(|| builder.spawn())
            .map_err(|_| DiagnosticsInstallError::BackendInstrumentationRequired)?;
        tracing_subscriber::registry()
            .with(layer)
            .try_init()
            .map_err(|error| DiagnosticsInstallError::Subscriber(error.to_string()))
    }
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self::new("127.0.0.1:6669")
    }
}

/// Why Tokio Console initialization failed.
#[derive(Debug, thiserror::Error)]
pub enum DiagnosticsInstallError {
    /// The bind address is not a socket address.
    #[error("invalid async diagnostics bind address: {0}")]
    InvalidBind(String),
    /// The executable lacks Tokio's unstable task instrumentation.
    #[error("the current async backend requires RUSTFLAGS=\"--cfg tokio_unstable\" for task instrumentation")]
    BackendInstrumentationRequired,
    /// Another process-global tracing subscriber is already installed.
    #[error("could not install the async diagnostics subscriber: {0}")]
    Subscriber(String),
}
