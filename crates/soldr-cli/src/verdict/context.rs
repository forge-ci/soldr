//! One checkout's key-derivation context: the git snapshot, the workspace's
//! `[workspace.metadata.soldr.verdict]` configuration and the effective
//! adapters. Shared by the front-door gate and `soldr verdict`.

use std::path::Path;

use super::key::{self, Derivation, KeyRequest, Snapshot};
use super::profiles::{self, Match};

/// Everything needed to derive keys for one checkout.
pub struct Context {
    pub snapshot: Snapshot,
    pub config: profiles::WorkspaceConfig,
    pub profiles: Vec<profiles::Profile>,
}

impl Context {
    pub fn discover(cwd: &Path) -> Result<Self, String> {
        let snapshot = Snapshot::discover(cwd)?;
        let config = profiles::workspace_config(&super::workspace_root(cwd, &snapshot.root));
        let profiles = profiles::effective(&config);
        Ok(Self {
            snapshot,
            config,
            profiles,
        })
    }

    /// Derive the key for `args` run in `cwd`.
    pub fn derive(&self, args: &[String], cwd: &Path) -> Result<Derivation, String> {
        if self.config.enabled == Some(false) {
            return Err("disabled by [workspace.metadata.soldr.verdict] enabled = false".into());
        }
        let profile = match profiles::match_invocation(&self.profiles, args) {
            Match::Check(profile) => profile,
            Match::NotPure(_, reason) => return Err(reason),
            Match::Unknown => return Err("no adapter for this subcommand".into()),
        };
        key::derive(
            &KeyRequest {
                profile,
                config: &self.config,
                args,
                cwd,
            },
            &self.snapshot,
        )
    }
}
