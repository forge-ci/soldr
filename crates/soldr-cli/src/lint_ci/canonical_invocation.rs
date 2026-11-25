//! Canonical Cargo invocation policy (soldr#4195).
use std::path::Path;

use crate::cargo_front_door::{
    canonical::{self, CompilingVerb, Selection},
    canonical_runtime,
};
use crate::cargo_metadata_canonical::{
    profile_from, CanonicalEnforce, CanonicalPackages, CanonicalProfile, CanonicalUnify,
};
use crate::core::{SoldrError, TargetTriple};

use super::model::{Finding, Severity};
use super::registry::CiRule;
use super::scan::ScannedFile;

const RULE: &str = "canonical-cargo-invocation";

pub(super) struct CanonicalInvocation {
    context: Result<Option<Context>, SoldrError>,
}

struct Context {
    profile: CanonicalProfile,
    members: Vec<String>,
    host: String,
    channel: Option<String>,
}

impl CanonicalInvocation {
    pub(super) fn new(root: Option<&Path>) -> Self {
        Self {
            context: root.map(Context::load).unwrap_or(Ok(None)),
        }
    }
}

impl Context {
    fn load(root: &Path) -> Result<Option<Self>, SoldrError> {
        let Some((profile, manifest)) = profile_from(root)? else {
            return Ok(None);
        };
        let root = manifest.parent().unwrap_or(root).to_path_buf();
        Ok(Some(Self {
            profile,
            members: crate::ci_test::workspace_member_names(&root),
            channel: canonical_runtime::build_channel(None, &root)?,
            host: TargetTriple::host()?.triple(),
        }))
    }

    fn check(&self, file: &ScannedFile, line: u32, mut args: Vec<String>) -> Option<Finding> {
        // The scanner enters only Soldr's Cargo front door (including its
        // shorthand); a bare Cargo invocation outside Soldr is another policy.
        let cargo = args.first().is_some_and(|arg| arg == "cargo");
        if cargo {
            args.remove(0);
        }
        let channel = args
            .first()
            .and_then(|arg| arg.strip_prefix('+'))
            .map(str::to_owned);
        if channel.is_some() {
            args.remove(0);
        }
        let verb = CompilingVerb::from_args(&args)?;
        if !cargo && verb == CompilingVerb::Build {
            return None; // `soldr build` is the blessed native surface.
        }
        // Computed argv cannot be decided statically. The runtime check is the
        // authority; do not turn an unknown selection into a false finding.
        if args
            .iter()
            .any(|arg| arg.contains('$') || arg.contains('{') || arg.starts_with('*'))
        {
            return None;
        }
        let mut profile = self.profile.clone();
        let channel = channel.as_deref().or(self.channel.as_deref());
        profile.unify = if canonical_runtime::unifies(profile.unify, channel) {
            CanonicalUnify::Workspace
        } else {
            CanonicalUnify::Off
        };
        let selection = Selection::from_args(verb, &args, &self.members);
        let drift = selection.drift(&profile);
        if drift.is_empty() {
            return None;
        }
        // The profile's `enforce` is one policy with two surfaces: the front
        // door (warn/refuse at run time) and this rule. `off` means the repo
        // has not opted in yet, so it must not fail `soldr lint ci` either.
        let severity = match profile.enforce {
            CanonicalEnforce::Off => return None,
            CanonicalEnforce::Warn => Severity::Warning,
            CanonicalEnforce::Refuse => Severity::Error,
        };
        let package: &str = match &selection.packages {
            CanonicalPackages::Explicit(packages) if packages.len() == 1 => &packages[0],
            _ => "x",
        };
        Some(Finding {
            rule: RULE.into(),
            severity,
            file: file.rel_path.clone(),
            line,
            tool: format!("soldr {}{}", if cargo { "cargo " } else { "" }, verb.name()),
            target: drift
                .iter()
                .map(|drift| drift.field)
                .collect::<Vec<_>>()
                .join(", "),
            recommendation: format!(
                "use `{}`; to narrow execution, use `{} -E 'package({package})'`",
                canonical::command(&profile, verb, &self.host),
                canonical::command(&profile, CompilingVerb::NextestRun, &self.host),
            ),
        })
    }
}

impl CiRule for CanonicalInvocation {
    fn id(&self) -> &'static str {
        RULE
    }

    fn check(&self, files: &[ScannedFile]) -> Vec<Finding> {
        let context = match &self.context {
            Ok(Some(context)) => context,
            Ok(None) => return Vec::new(),
            Err(error) => {
                return vec![Finding {
                    rule: RULE.into(),
                    severity: Severity::Error,
                    file: "Cargo.toml".into(),
                    line: 1,
                    tool: "canonical cargo profile".into(),
                    target: "configuration".into(),
                    recommendation: format!("fix the canonical cargo profile: {error}"),
                }]
            }
        };
        let mut findings = Vec::new();
        eprintln!(
            "DBG files={} lines={}",
            files.len(),
            files.iter().map(|f| f.lines.len()).sum::<usize>()
        );
        for file in files {
            for line in &file.lines {
                if line.is_comment || !line.is_command {
                    continue;
                }
                let inv = super::canonical_tokens::invocations(&line.original);
                if !inv.is_empty() {
                    eprintln!("DBG {}:{} -> {:?}", file.rel_path, line.line, inv);
                }
                for args in inv {
                    if let Some(finding) = context.check(file, line.line, args) {
                        findings.push(finding);
                    }
                }
            }
        }
        findings
    }
}

#[cfg(test)]
#[path = "canonical_invocation_tests.rs"]
mod tests;
