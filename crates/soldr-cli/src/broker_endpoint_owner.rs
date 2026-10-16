//! The broker's claim on its stable socket path (soldr#3815).
//!
//! The broker is meant to be a singleton, but a filesystem socket is only a
//! name: a broker that unlinked the path on its way out, or a successor that
//! recovered it as stale, leaves an older broker alive and listening on a file
//! nobody can open again. That orphan still runs its route reaper over the
//! routes it once served. Its requesters exit, its table empties, and it reaps
//! the shared daemon the *current* broker's clients are compiling on -- the
//! "reaped route ...; every process that asked for it has exited" kills seen
//! while dozens of compiles were in flight. Three brokers were found listening
//! on one path on one host, one of them 8.5 days old.
//!
//! So a broker records which file it bound (device, inode, change time). It
//! retires the path only while that file is still its own, and it stops
//! reaping and stands down the moment the path belongs to someone else.

/// The file a broker bound at its stable endpoint.
#[derive(Debug)]
pub(crate) struct EndpointClaim {
    endpoint: String,
    identity: Option<(u64, u64, i128)>,
}

impl EndpointClaim {
    /// Record the file currently at `endpoint`; call right after binding it.
    pub(crate) fn capture(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            identity: crate::platform::ipc::broker::endpoint_identity(endpoint),
        }
    }

    /// [`Self::capture`], shared between the accept loop and the reaper.
    pub(crate) fn shared(endpoint: &str) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::capture(endpoint))
    }

    /// True while the path still names the file this broker bound.
    pub(crate) fn still_owned(&self) -> bool {
        self.identity.is_some()
            && crate::platform::ipc::broker::endpoint_identity(&self.endpoint) == self.identity
    }

    /// Unlink the path, but never a successor's socket.
    pub(crate) fn retire(&self) {
        crate::platform::ipc::broker::retire_endpoint_if_identity(&self.endpoint, self.identity);
    }

    /// The loud line an orphaned broker prints before it stands down.
    pub(crate) fn orphaned_message(&self) -> String {
        format!(
            "soldr broker: ERROR: {} no longer names this broker's socket (another broker \
             rebound it); this broker is orphaned and unreachable. Standing down without \
             reaping any route, so it cannot kill daemons other sessions are using (soldr#3815).",
            self.endpoint
        )
    }
}

#[cfg(test)]
#[path = "broker_endpoint_owner_tests.rs"]
mod tests;
