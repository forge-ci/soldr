//! One checkout's key-derivation context: the git snapshot, the workspace's
//! `[workspace.metadata.soldr.verdict]` configuration and the effective
//! adapters. Shared by the front-door gate and `soldr verdict`.

use std::collections::BTreeMap;
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

    /// The adapter claiming `args`, if it is a pure check.
    pub fn profile_for(&self, args: &[String]) -> Option<&profiles::Profile> {
        match profiles::match_invocation(&self.profiles, args) {
            Match::Check(profile) => Some(profile),
            _ => None,
        }
    }

    /// Per-member keys for a workspace-wide `args` (soldr#3897), from
    /// `cargo metadata`. `Err` means: use the workspace key alone.
    pub fn derive_crates(
        &self,
        args: &[String],
        cwd: &Path,
    ) -> Result<std::collections::BTreeMap<String, String>, String> {
        let profile = self
            .profile_for(args)
            .ok_or_else(|| "not a pure check".to_string())?;
        if !profile.per_package || !super::crates::selects_workspace(args) {
            return Err("not a narrowable workspace run".into());
        }
        let cargo = crate::binaries::resolve_toolchain_binary("cargo")
            .map_err(|err| format!("cannot resolve cargo: {err}"))?;
        let mut command = std::process::Command::new(cargo);
        command
            .args([
                "metadata",
                "--no-deps",
                "--format-version",
                "1",
                "--offline",
            ])
            .current_dir(cwd);
        let output = crate::core::command_output_with_timeout_duration(
            &mut command,
            "verdict cargo metadata",
            std::time::Duration::from_secs(60),
        )
        .map_err(|err| err.to_string())?;
        if !output.status.success() {
            return Err("cargo metadata failed".into());
        }
        let members = super::crates::parse_metadata(
            &String::from_utf8_lossy(&output.stdout),
            &self.snapshot.root,
        )?;
        key::derive_crates(
            &KeyRequest {
                profile,
                config: &self.config,
                args,
                cwd,
                env: None,
            },
            &self.snapshot,
            &members,
        )
    }

    /// Derive the key for `args` run in `cwd`.
    pub fn derive(&self, args: &[String], cwd: &Path) -> Result<Derivation, String> {
        self.derive_with_env(args, cwd, None)
    }

    /// [`Self::derive`] under a recorded environment (`None`: this
    /// process's), so a re-key matches the step that ran the check.
    pub fn derive_with_env(
        &self,
        args: &[String],
        cwd: &Path,
        env: Option<&BTreeMap<String, String>>,
    ) -> Result<Derivation, String> {
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
                env,
            },
            &self.snapshot,
        )
    }
}
