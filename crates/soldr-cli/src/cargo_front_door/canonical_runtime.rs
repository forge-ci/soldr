//! Profile enforcement before verdict replay, tool bootstrap or compilation.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::cargo_metadata_canonical::{
    profile_from, CanonicalEnforce, CanonicalPackages, CanonicalProfile, CanonicalUnify,
};
use crate::core::{
    command_output_with_timeout, read_rust_toolchain_manifest_from_ancestors, SoldrError,
    TargetTriple,
};

use super::canonical::{self, CompilingVerb, Drift, Selection};

pub(super) struct CanonicalRun {
    pub args: Vec<String>,
    pub unify: bool,
    pub refused: bool,
}

pub(super) fn prepare(args: &[String], cache_enabled: bool) -> Result<CanonicalRun, SoldrError> {
    let mut run = CanonicalRun {
        args: args.to_vec(),
        unify: false,
        refused: false,
    };
    // Leave the misplaced private flag to the front door's existing diagnostic.
    if super::cargo_args_use_reserved_no_cache(args) {
        return Ok(run);
    }
    let (stripped, explicit_toolchain) = super::subcommand::strip_cargo_toolchain_directive(args);
    let Some(verb) = CompilingVerb::from_args(&stripped) else {
        return Ok(run);
    };
    let start = invocation_directory(&stripped)?;
    let Some((mut profile, manifest)) = profile_from(&start)? else {
        return Ok(run);
    };
    let channel = build_channel(explicit_toolchain.as_deref(), &std::env::current_dir()?)?;
    run.unify = unifies(profile.unify, channel.as_deref());
    // The comparator sees the effective mode, never an unresolved Auto.
    profile.unify = if run.unify {
        CanonicalUnify::Workspace
    } else {
        CanonicalUnify::Off
    };
    // Uncached investigations bypass diagnostics, but the resolver mode remains
    // uniform across the repo's compiling invocations (design section 4.5).
    let enforce =
        if !cache_enabled || std::env::var_os("RUSTC_WRAPPER").is_some_and(|s| s.is_empty()) {
            CanonicalEnforce::Off
        } else {
            enforcement(
                profile.enforce,
                std::env::var("SOLDR_CANONICAL").ok().as_deref(),
            )?
        };
    if enforce != CanonicalEnforce::Off {
        let members = members(&manifest, explicit_toolchain.as_deref(), &stripped)?;
        let selection = Selection::from_args(verb, &stripped, &members);
        let drift = selection.drift(&profile);
        if !drift.is_empty() {
            run.refused = enforce == CanonicalEnforce::Refuse;
            let host = TargetTriple::host()?.triple();
            let message = diagnostic(&profile, &selection, verb, &drift, &host, run.refused);
            if run.refused {
                eprintln!(
                    "{}",
                    crate::color_choice::paint_stderr(&message, "\x1b[31m")
                );
            } else {
                eprintln!("{message}");
            }
        }
    }
    if run.unify && !run.refused {
        let index = super::first_cargo_subcommand_index(&run.args).unwrap_or(0);
        run.args.insert(index, "-Zfeature-unification".into());
    }
    Ok(run)
}

fn enforcement(
    profile: CanonicalEnforce,
    value: Option<&str>,
) -> Result<CanonicalEnforce, SoldrError> {
    match value {
        None => Ok(profile),
        Some("off") => Ok(CanonicalEnforce::Off),
        Some("warn") => Ok(CanonicalEnforce::Warn),
        Some("refuse") => Ok(CanonicalEnforce::Refuse),
        Some(value) => Err(SoldrError::Other(format!(
            "SOLDR_CANONICAL: unknown value {value:?}; expected off, warn, refuse"
        ))),
    }
}

fn invocation_directory(args: &[String]) -> Result<PathBuf, SoldrError> {
    let mut iter = args.iter().take_while(|arg| arg.as_str() != "--");
    while let Some(arg) = iter.next() {
        let value = if arg == "--manifest-path" {
            iter.next().map(String::as_str)
        } else {
            arg.strip_prefix("--manifest-path=")
        };
        if let Some(value) = value {
            let path = std::env::current_dir()?.join(value);
            return Ok(path.parent().unwrap_or(Path::new(".")).to_path_buf());
        }
    }
    Ok(std::env::current_dir()?)
}

pub(crate) fn build_channel(
    explicit: Option<&str>,
    cwd: &Path,
) -> Result<Option<String>, SoldrError> {
    if let Some(channel) = explicit {
        return Ok(Some(channel.to_owned()));
    }
    if let Ok(channel) = std::env::var("RUSTUP_TOOLCHAIN") {
        if !channel.is_empty() {
            return Ok(Some(channel));
        }
    }
    Ok(read_rust_toolchain_manifest_from_ancestors(cwd)?.channel)
}

pub(crate) fn unifies(unify: CanonicalUnify, channel: Option<&str>) -> bool {
    match unify {
        CanonicalUnify::Workspace => true,
        CanonicalUnify::Off => false,
        CanonicalUnify::Auto => channel.is_some_and(|channel| channel.contains("nightly")),
    }
}

type MemberCache = BTreeMap<(PathBuf, PathBuf, Option<String>, Vec<String>), Vec<String>>;

fn members(
    manifest: &Path,
    channel: Option<&str>,
    args: &[String],
) -> Result<Vec<String>, SoldrError> {
    static CACHE: OnceLock<Mutex<MemberCache>> = OnceLock::new();
    let cargo = crate::resolve_toolchain_binary_for_channel("cargo", channel)?;
    let globals = metadata_globals(args);
    let key = (
        manifest.to_path_buf(),
        cargo.clone(),
        channel.map(str::to_owned),
        globals.clone(),
    );
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|e| SoldrError::Other(format!("canonical metadata cache: {e}")))?;
    if let Some(members) = cache.get(&key) {
        return Ok(members.clone());
    }
    let mut command = std::process::Command::new(&cargo);
    command
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(manifest)
        .args(globals);
    crate::binaries::apply_resolved_toolchain_homes(&mut command, &cargo);
    crate::toolchain_dir_name::carry_toolchain_into_probe(
        &mut command,
        manifest.parent().unwrap_or(Path::new(".")),
    );
    if let Some(channel) = channel {
        command.env("RUSTUP_TOOLCHAIN", channel);
    }
    // Metadata is observational: never inherit a build wrapper into the probe.
    command
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    let output = command_output_with_timeout(&mut command, "canonical cargo workspace metadata")?;
    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "canonical cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    #[derive(serde::Deserialize)]
    struct Metadata {
        packages: Vec<Package>,
    }
    #[derive(serde::Deserialize)]
    struct Package {
        name: String,
    }
    let metadata: Metadata = serde_json::from_slice(&output.stdout)
        .map_err(|e| SoldrError::Other(format!("canonical cargo metadata: {e}")))?;
    let members = crate::cargo_metadata_canonical::sorted(
        metadata.packages.into_iter().map(|p| p.name).collect(),
    );
    cache.insert(key, members.clone());
    Ok(members)
}

fn metadata_globals(args: &[String]) -> Vec<String> {
    let mut globals = Vec::new();
    let mut iter = args.iter().take_while(|s| s.as_str() != "--");
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--locked" | "--offline" | "--frozen" => globals.push(arg.clone()),
            "--config" => {
                globals.push(arg.clone());
                if let Some(value) = iter.next() {
                    globals.push(value.clone());
                }
            }
            _ if arg.starts_with("--config=") => globals.push(arg.clone()),
            _ => {}
        }
    }
    globals
}

fn diagnostic(
    profile: &CanonicalProfile,
    selection: &Selection,
    verb: CompilingVerb,
    drift: &[Drift],
    host: &str,
    refused: bool,
) -> String {
    let header = if refused {
        "error: off-profile cargo invocation refused"
    } else {
        "warning: off-profile cargo invocation"
    };
    let differs = drift
        .iter()
        .map(|d| format!("{} ({} vs {})", d.field, d.found, d.canonical))
        .collect::<Vec<_>>()
        .join(", ");
    let canonical = canonical::command(profile, verb, host);
    let suggestion = match (&selection.packages, verb.tests()) {
        (CanonicalPackages::Explicit(packages), true) if packages.len() == 1 => format!(
            "{} -E 'package({})'",
            canonical::command(profile, CompilingVerb::NextestRun, host),
            packages[0]
        ),
        _ => "keep the canonical selection and filter what runs".into(),
    };
    let mut message = format!(
        concat!(
            "soldr: {} (soldr#4195)\n",
            "  this run resolves dependency features differently from the repo's canonical\n",
            "  profile ([workspace.metadata.soldr.cargo] in Cargo.toml), so dependency\n",
            "  crates built by the canonical lint/test runs will NOT be reused.\n",
            "    differs in: {}\n",
            "  canonical:  {}\n",
            "  to narrow what runs without changing what compiles, keep the canonical\n",
            "  selection and filter tests instead:\n",
            "              {}\n",
            "  silence: SOLDR_CANONICAL=off, or declare this selection under\n",
            "           [workspace.metadata.soldr.cargo.alternates.<name>]"
        ),
        header, differs, canonical, suggestion
    );
    if drift.iter().any(|d| d.field == "clippy args") {
        message.push_str("\n  move lint levels to [workspace.lints.clippy] in Cargo.toml");
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_unification_only_auto_enables_on_nightly() {
        for channel in [None, Some("stable"), Some("1.98.1"), Some("beta")] {
            assert!(!unifies(CanonicalUnify::Auto, channel));
            assert!(!unifies(CanonicalUnify::Off, channel));
            assert!(unifies(CanonicalUnify::Workspace, channel));
        }
        assert!(unifies(CanonicalUnify::Auto, Some("nightly-2026-08-15")));
    }

    #[test]
    fn canonical_environment_override_is_strict() {
        assert_eq!(
            enforcement(CanonicalEnforce::Refuse, Some("off")).unwrap(),
            CanonicalEnforce::Off
        );
        assert_eq!(
            enforcement(CanonicalEnforce::Off, Some("warn")).unwrap(),
            CanonicalEnforce::Warn
        );
        assert_eq!(
            enforcement(CanonicalEnforce::Warn, Some("refuse")).unwrap(),
            CanonicalEnforce::Refuse
        );
        assert!(enforcement(CanonicalEnforce::Warn, Some("typo"))
            .unwrap_err()
            .to_string()
            .contains("off, warn, refuse"));
    }
}

#[cfg(test)]
#[test]
fn canonical_diagnostic_preserves_the_design_text_and_indentation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace.metadata.soldr.cargo]\nfeatures = 'all'\nunify = 'off'",
    )
    .unwrap();
    let (profile, _) = profile_from(dir.path()).unwrap().unwrap();
    let args = ["test", "-p", "a", "--target=host"].map(str::to_owned);
    let selection = Selection::from_args(CompilingVerb::Test, &args, &["a".into(), "b".into()]);
    let drift = selection.drift(&profile);
    let expected = concat!(
        "soldr: warning: off-profile cargo invocation (soldr#4195)\n",
        "  this run resolves dependency features differently from the repo's canonical\n",
        "  profile ([workspace.metadata.soldr.cargo] in Cargo.toml), so dependency\n",
        "  crates built by the canonical lint/test runs will NOT be reused.\n",
        "    differs in: packages (-p a vs --workspace), features (none vs all)\n",
        "  canonical:  soldr cargo test --workspace --all-features --all-targets --target host\n",
        "  to narrow what runs without changing what compiles, keep the canonical\n",
        "  selection and filter tests instead:\n",
        "              soldr cargo nextest run --workspace --all-features --all-targets --target host -E 'package(a)'\n",
        "  silence: SOLDR_CANONICAL=off, or declare this selection under\n",
        "           [workspace.metadata.soldr.cargo.alternates.<name>]"
    );
    assert_eq!(
        diagnostic(
            &profile,
            &selection,
            CompilingVerb::Test,
            &drift,
            "host",
            false
        ),
        expected
    );
    assert_eq!(
        diagnostic(
            &profile,
            &selection,
            CompilingVerb::Test,
            &drift,
            "host",
            true
        ),
        expected.replacen(
            "warning: off-profile cargo invocation",
            "error: off-profile cargo invocation refused",
            1
        )
    );
    let args = [
        "clippy",
        "--workspace",
        "--all-features",
        "--all-targets",
        "--target=host",
        "--",
        "-A",
        "clippy::all",
    ]
    .map(str::to_owned);
    let selection = Selection::from_args(CompilingVerb::Clippy, &args, &["a".into(), "b".into()]);
    let message = diagnostic(
        &profile,
        &selection,
        CompilingVerb::Clippy,
        &selection.drift(&profile),
        "host",
        false,
    );
    assert!(message.contains("clippy args (-A clippy::all vs -D warnings)"));
    assert!(message.ends_with("move lint levels to [workspace.lints.clippy] in Cargo.toml"));
}
