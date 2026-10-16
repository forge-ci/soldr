//! Target-aware PyO3 build planning (soldr#1610 / #1614).
//!
//! Cargo metadata is the source of truth for the active PyO3 version,
//! features, and workspace targets. Policy resolution is pure and separate
//! from process inspection so every soldr entry point can consume the same
//! plan and the complete decision table stays unit-testable.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

pub const COMPATIBILITY_ENV_VAR: &str = "SOLDR_PYO3_COMPATIBILITY";
pub const BUILD_KIND_ENV_VAR: &str = "SOLDR_PYO3_BUILD_KIND";

const CALLER_CONFIG_VARS: &[&str] = &[
    "PYO3_CONFIG_FILE",
    "PYO3_CROSS",
    "PYO3_CROSS_LIB_DIR",
    "PYO3_CROSS_PYTHON_IMPLEMENTATION",
    "PYO3_CROSS_PYTHON_VERSION",
    "PYO3_NO_PYTHON",
    "PYO3_PYTHON",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildShape {
    Absent,
    Extension,
    Embedding,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanMode {
    Native,
    NoPyo3,
    Abi3NoPython,
    ModernWindowsRawDylib,
    ExtensionDefault,
    RequiresExplicitCompatibility,
    CompatibilitySysroot,
    CallerConfigured,
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedPyo3 {
    pub shape: BuildShape,
    pub versions: BTreeSet<String>,
    pub features: BTreeSet<String>,
}

impl DetectedPyo3 {
    pub fn abi3(&self) -> bool {
        self.features.iter().any(|feature| {
            feature == "abi3"
                || feature == "abi3t"
                || feature.starts_with("abi3-py")
                || feature.starts_with("abi3t-py")
        })
    }

    fn single_version(&self) -> Option<&str> {
        (self.versions.len() == 1)
            .then(|| self.versions.iter().next().map(String::as_str))
            .flatten()
    }
}

#[derive(Debug, Clone)]
pub struct PolicyInput {
    pub host: String,
    pub target: String,
    pub detected: Option<DetectedPyo3>,
    pub caller_pyo3: BTreeMap<String, String>,
    pub compatibility_sysroot: bool,
    pub raw_dylib_disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pyo3BuildPlan {
    pub host: String,
    pub target: String,
    pub shape: BuildShape,
    pub pyo3_versions: Vec<String>,
    pub pyo3_features: Vec<String>,
    pub mode: PlanMode,
    pub env: BTreeMap<String, String>,
    pub needs_python_sysroot: bool,
    pub diagnostic: Option<String>,
}

impl Pyo3BuildPlan {
    pub fn apply_to_command(&self, command: &mut Command) {
        for (key, value) in &self.env {
            if std::env::var_os(key).is_none() {
                command.env(key, value);
            }
        }
    }

    pub fn emit_diagnostic(&self) {
        if let Some(message) = &self.diagnostic {
            eprintln!("soldr: PyO3 build plan: {message}");
        }
    }

    pub async fn materialize_compatibility(
        &mut self,
        paths: &crate::core::SoldrPaths,
    ) -> Result<(), crate::core::SoldrError> {
        if !self.needs_python_sysroot {
            return Ok(());
        }
        let sysroot =
            crate::fetch::python_sysroot::ensure_python_sysroot(paths, &self.target).await?;
        self.env
            .extend(compatibility_sysroot_env(&sysroot.root, &sysroot.version));
        Ok(())
    }
}

fn compatibility_sysroot_env(root: &Path, version: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let abi_version = version.split('.').take(2).collect::<Vec<_>>().join(".");
    env.insert("PYO3_CROSS".into(), "1".into());
    env.insert(
        "PYO3_CROSS_LIB_DIR".into(),
        root.join("lib").display().to_string(),
    );
    env.insert("PYO3_CROSS_PYTHON_VERSION".into(), abi_version);
    env.insert("PYO3_CROSS_PYTHON_IMPLEMENTATION".into(), "CPython".into());
    env
}

pub fn resolve_policy(input: PolicyInput) -> Pyo3BuildPlan {
    let detected = input.detected.as_ref();
    let shape = detected.map_or(BuildShape::Absent, |value| value.shape);
    let versions = detected
        .map(|value| value.versions.iter().cloned().collect())
        .unwrap_or_default();
    let features = detected
        .map(|value| value.features.iter().cloned().collect())
        .unwrap_or_default();
    let mut plan = Pyo3BuildPlan {
        host: input.host.clone(),
        target: input.target.clone(),
        shape,
        pyo3_versions: versions,
        pyo3_features: features,
        mode: PlanMode::Unresolved,
        env: BTreeMap::new(),
        needs_python_sysroot: false,
        diagnostic: None,
    };

    if input.host == input.target {
        plan.mode = PlanMode::Native;
        return plan;
    }
    if detected.is_none() || shape == BuildShape::Absent {
        plan.mode = PlanMode::NoPyo3;
        return plan;
    }
    if input
        .caller_pyo3
        .keys()
        .any(|key| CALLER_CONFIG_VARS.contains(&key.as_str()))
    {
        plan.mode = PlanMode::CallerConfigured;
        return plan;
    }

    let detected = detected.expect("checked above");
    if shape == BuildShape::Extension && detected.abi3() {
        plan.mode = PlanMode::Abi3NoPython;
        plan.env
            .insert("PYO3_NO_PYTHON".to_string(), "1".to_string());
        return plan;
    }

    if shape == BuildShape::Extension
        && input.target.contains("windows")
        && detected
            .single_version()
            .is_some_and(|version| version_at_least(version, 0, 29))
        && !input.raw_dylib_disabled
    {
        plan.mode = PlanMode::ModernWindowsRawDylib;
        return plan;
    }

    if input.compatibility_sysroot {
        plan.mode = PlanMode::CompatibilitySysroot;
        plan.needs_python_sysroot = true;
        return plan;
    }

    if shape == BuildShape::Extension
        && (input.target.contains("linux") || input.target.contains("apple-darwin"))
        && detected
            .single_version()
            .is_some_and(|version| version_at_least(version, 0, 29))
    {
        plan.mode = PlanMode::ExtensionDefault;
        plan.diagnostic = Some(
            "cross extension is not proven ABI3; leaving Python configuration to PyO3/maturin"
                .to_string(),
        );
        return plan;
    }

    plan.mode = PlanMode::RequiresExplicitCompatibility;
    plan.diagnostic = Some(format!(
        "cross {:?} build cannot safely use PYO3_NO_PYTHON; provide PYO3_* configuration or set {COMPATIBILITY_ENV_VAR}=sysroot",
        shape
    ));
    plan
}

fn version_at_least(version: &str, required_major: u64, required_minor: u64) -> bool {
    let mut parts = version.split('.');
    let major = parts.next().and_then(|value| value.parse().ok());
    let minor = parts.next().and_then(|value| value.parse().ok());
    matches!((major, minor), (Some(major), Some(minor)) if (major, minor) >= (required_major, required_minor))
}

pub fn resolve_for_invocation(
    workspace_root: &Path,
    args: &[String],
    target_override: Option<&str>,
) -> Pyo3BuildPlan {
    let target = target_override
        .map(normalize_target)
        .unwrap_or_else(|| resolve_build_target(args, workspace_root));
    let host = host_triple().to_string();
    resolve_for_target(
        workspace_root,
        args,
        host,
        target,
        CargoProbeHints::default(),
    )
}

/// What the cargo front door already knows when it asks for a PyO3 plan
/// (soldr#3794).
#[derive(Clone, Copy, Default)]
pub(crate) struct CargoProbeHints<'a> {
    /// The cargo the front door resolved for the effective channel. The
    /// metadata probe runs it instead of re-resolving the ambient cargo,
    /// which can be a different toolchain.
    pub(crate) cargo: Option<&'a Path>,
    /// An up-to-date `Cargo.lock` has no package named exactly `pyo3`.
    pub(crate) lock_proves_no_pyo3: bool,
}

impl CargoProbeHints<'_> {
    /// The probe may be skipped only on lockfile proof, and only when no
    /// argument can redirect which graph Cargo resolves: `--target` and
    /// `--config` (which may carry `build.target`) keep the probe, and so
    /// does `-C`, which the probe does not honour.
    fn may_skip_probe(&self, args: &[String]) -> bool {
        self.lock_proves_no_pyo3 && !args_override_probe_graph(args)
    }
}

fn args_override_probe_graph(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| {
            matches!(arg.as_str(), "--target" | "--config" | "-C")
                || arg.starts_with("--target=")
                || arg.starts_with("--config=")
        })
}

/// Cargo-front-door variant that can skip metadata only when soldr has
/// positively resolved the target Cargo will use. A missing target is not
/// proof of a native build because `.cargo/config.toml` may select a cross
/// target behind soldr's back.
pub(crate) fn resolve_for_cargo_invocation(
    workspace_root: &Path,
    args: &[String],
    known_target: Option<&str>,
    hints: CargoProbeHints<'_>,
) -> Pyo3BuildPlan {
    let host = host_triple().to_string();
    // An explicit --target is Cargo's strongest target selector and must win
    // even if the caller also exported CARGO_BUILD_TARGET. Conversely, an
    // arbitrary --config may contain `build.target`; unless --target is
    // present, retain the metadata path rather than treating a weaker target
    // source as authoritative.
    let explicit_target = explicit_target(args)
        .map(|target| normalize_target(&target))
        .filter(|target| !target.is_empty());
    let config_may_select_target = explicit_target.is_none()
        && args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .any(|arg| arg == "--config" || arg.starts_with("--config="));
    let known_target = explicit_target.or_else(|| {
        (!config_may_select_target)
            .then(|| known_target.map(normalize_target))
            .flatten()
            .filter(|target| !target.is_empty())
    });
    if !host.is_empty() && known_target.as_deref() == Some(host.as_str()) {
        return resolve_policy(PolicyInput {
            host,
            target: known_target.expect("checked above"),
            detected: None,
            caller_pyo3: BTreeMap::new(),
            compatibility_sysroot: false,
            raw_dylib_disabled: false,
        });
    }

    match known_target {
        Some(target) => resolve_for_target(workspace_root, args, host, target, hints),
        None => {
            let target = resolve_build_target(args, workspace_root);
            resolve_for_target(workspace_root, args, host, target, hints)
        }
    }
}

fn resolve_for_target(
    workspace_root: &Path,
    args: &[String],
    host: String,
    target: String,
    hints: CargoProbeHints<'_>,
) -> Pyo3BuildPlan {
    let probe = if hints.may_skip_probe(args) {
        Ok(None)
    } else {
        detect_workspace_pyo3(workspace_root, args, &target, hints.cargo)
    };
    let detected = match probe {
        Ok(value) => value,
        Err(error) => {
            return Pyo3BuildPlan {
                host,
                target,
                shape: BuildShape::Absent,
                pyo3_versions: Vec::new(),
                pyo3_features: Vec::new(),
                mode: PlanMode::Unresolved,
                env: BTreeMap::new(),
                needs_python_sysroot: false,
                diagnostic: Some(format!(
                    "Cargo metadata could not prove PyO3 eligibility ({error}); no PyO3 variables were injected"
                )),
            };
        }
    };
    let compatibility_sysroot = std::env::var(COMPATIBILITY_ENV_VAR)
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "sysroot"
            )
        });
    let raw_dylib_disabled = std::env::var("PYO3_USE_RAW_DYLIB")
        .ok()
        .is_some_and(|value| crate::core::is_off_value(&value));
    let mut detected = detected;
    if let Some(kind) = build_kind_override() {
        if let Some(value) = &mut detected {
            value.shape = kind;
        }
    }
    resolve_policy(PolicyInput {
        host,
        target,
        detected,
        caller_pyo3: caller_pyo3_env(),
        compatibility_sysroot,
        raw_dylib_disabled,
    })
}

fn build_kind_override() -> Option<BuildShape> {
    match std::env::var(BUILD_KIND_ENV_VAR)
        .ok()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "extension" => Some(BuildShape::Extension),
        "embedding" => Some(BuildShape::Embedding),
        _ => None,
    }
}

fn caller_pyo3_env() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(key, _)| key.starts_with("PYO3_"))
        .collect()
}

pub fn resolve_build_target(args: &[String], workspace_root: &Path) -> String {
    choose_build_target(
        args,
        std::env::var("CARGO_BUILD_TARGET").ok().as_deref(),
        project_maturin_target(workspace_root).as_deref(),
        host_triple(),
    )
}

fn choose_build_target(
    args: &[String],
    env_target: Option<&str>,
    project_target: Option<&str>,
    host: &str,
) -> String {
    explicit_target(args)
        .or_else(|| {
            env_target
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            project_target
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
        .map(|target| normalize_target(&target))
        .unwrap_or_else(|| host.to_string())
}

fn explicit_target(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--target" {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--target=") {
            return Some(value.to_string());
        }
    }
    None
}

pub fn normalize_explicit_target_args(args: &[String]) -> Vec<String> {
    let mut normalized = args.to_vec();
    let mut index = 0;
    while index < normalized.len() {
        if normalized[index] == "--target" {
            if let Some(value) = normalized.get_mut(index + 1) {
                *value = normalize_target(value);
            }
            index += 1;
        } else if let Some(value) = normalized[index].strip_prefix("--target=") {
            normalized[index] = format!("--target={}", normalize_target(value));
        }
        index += 1;
    }
    normalized
}

pub fn maturin_args_are_build(args: &[String]) -> bool {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "-V" | "--version"))
    {
        return false;
    }

    match args.first().map(String::as_str) {
        Some("build" | "develop") => true,
        Some("pep517") => matches!(
            args.get(1).map(String::as_str),
            Some("build-wheel" | "write-dist-info")
        ),
        _ => false,
    }
}

fn project_maturin_target(start: &Path) -> Option<String> {
    let mut current = start.canonicalize().ok()?;
    loop {
        let pyproject = current.join("pyproject.toml");
        if let Ok(text) = std::fs::read_to_string(&pyproject) {
            if let Ok(document) = text.parse::<toml::Value>() {
                if let Some(target) = document
                    .get("tool")
                    .and_then(|value| value.get("maturin"))
                    .and_then(|value| value.get("target"))
                    .and_then(toml::Value::as_str)
                {
                    return Some(target.to_string());
                }
            }
        }
        if !current.pop() {
            break;
        }
    }
    None
}

fn normalize_target(target: &str) -> String {
    crate::target_alias::resolve_soldr_target(target)
        .map(|resolved| resolved.rust_triple)
        .unwrap_or_else(|_| target.trim().to_string())
}

pub fn host_triple() -> &'static str {
    // The platform crate owns the compile-time host triple (the target
    // this binary was built for); an unsupported host yields "".
    crate::platform::host::facts::triple()
}

pub fn workspace_uses_pyo3(cwd: &Path) -> bool {
    detect_workspace_pyo3(cwd, &[], host_triple(), None)
        .ok()
        .flatten()
        .is_some()
}

pub fn cross_env_for_target(workspace_root: &Path, target: &str) -> BTreeMap<String, String> {
    resolve_for_invocation(workspace_root, &[], Some(target)).env
}

fn detect_workspace_pyo3(
    workspace_root: &Path,
    args: &[String],
    target: &str,
    cargo: Option<&Path>,
) -> Result<Option<DetectedPyo3>, String> {
    let cargo = match cargo {
        Some(path) => path.to_path_buf(),
        None => {
            crate::binaries::resolve_toolchain_binary("cargo").map_err(|error| error.to_string())?
        }
    };
    let mut command = Command::new(&cargo);
    command.args(["metadata", "--format-version", "1"]);
    command.current_dir(workspace_root);
    // A direct (non-rustup-proxy) toolchain cargo resolves `rustc` from
    // PATH, and a soldr-managed environment often has no bare rustc there —
    // the probe then dies with `could not execute process \`rustc -vV\``
    // and `soldr wheel` refuses an abi3-provable workspace (soldr#2576).
    // Pin RUSTC to the resolved cargo's sibling, exactly as the maturin
    // dispatch does for the same failure class; an explicit caller RUSTC
    // always wins.
    if std::env::var_os("RUSTC").is_none() {
        if let Some(sibling) = cargo
            .parent()
            .map(|dir| dir.join(crate::platform::executable::name::native("rustc")))
            .filter(|path| path.is_file())
        {
            command.env("RUSTC", sibling);
        }
    }
    // Cargo metadata may invoke rustc through a rustup proxy. In managed CI
    // the pinned toolchain lives in a private rustup home and is not the
    // user's default, so carry the explicit channel into this probe just as
    // the eventual cargo child does. Without it rustup reports that no
    // default toolchain is configured and PyO3 detection becomes lossy.
    crate::toolchain_dir_name::carry_toolchain_into_probe(&mut command, workspace_root);
    command.env_remove("RUSTC_WRAPPER");
    command.env_remove("RUSTC_WORKSPACE_WRAPPER");
    if !target.is_empty() {
        command.args(["--filter-platform", target]);
    }
    append_metadata_feature_args(&mut command, args);
    if let Some(path) = manifest_path_arg(args) {
        command.args(["--manifest-path", &path]);
    }
    // Metadata is advisory for PyO3 planning and must never wedge a target
    // build. Capture both pipes while the child runs so a large workspace
    // cannot fill an OS pipe and deadlock the bounded probe.
    let output = crate::core::command_output_with_timeout(&mut command, "cargo metadata probe")
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    detect_from_metadata_json(&output.stdout, args)
}

fn append_metadata_feature_args(command: &mut Command, args: &[String]) {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if matches!(arg.as_str(), "--all-features" | "--no-default-features") {
            command.arg(arg);
        } else if arg == "--features" {
            if let Some(value) = args.get(index + 1) {
                command.args([arg, value]);
                index += 1;
            }
        } else if arg.starts_with("--features=") {
            command.arg(arg);
        }
        index += 1;
    }
}

fn manifest_path_arg(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--manifest-path" {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--manifest-path=") {
            return Some(value.to_string());
        }
    }
    None
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<String>,
    #[serde(default)]
    workspace_default_members: Vec<String>,
    resolve: Option<CargoResolve>,
}

#[derive(Deserialize)]
struct CargoPackage {
    id: String,
    name: String,
    version: String,
    targets: Vec<CargoTarget>,
}

#[derive(Deserialize)]
struct CargoTarget {
    kind: Vec<String>,
    crate_types: Vec<String>,
}

#[derive(Deserialize)]
struct CargoResolve {
    nodes: Vec<CargoNode>,
}

#[derive(Deserialize)]
struct CargoNode {
    id: String,
    #[serde(default)]
    deps: Vec<CargoDep>,
    #[serde(default)]
    features: Vec<String>,
}

#[derive(Deserialize)]
struct CargoDep {
    pkg: String,
}

fn detect_from_metadata_json(
    bytes: &[u8],
    args: &[String],
) -> Result<Option<DetectedPyo3>, String> {
    let metadata: CargoMetadata =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let Some(resolve) = &metadata.resolve else {
        return Ok(None);
    };
    let packages: HashMap<_, _> = metadata
        .packages
        .iter()
        .map(|package| (package.id.as_str(), package))
        .collect();
    let pyo3_ids: HashSet<_> = metadata
        .packages
        .iter()
        .filter(|package| package.name == "pyo3")
        .map(|package| package.id.as_str())
        .collect();
    if pyo3_ids.is_empty() {
        return Ok(None);
    }
    let nodes: HashMap<_, _> = resolve
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let selected_members = selected_workspace_members(&metadata, args, &packages);
    let reachable_pyo3 = reachable_pyo3_ids(&selected_members, &nodes, &pyo3_ids);
    if reachable_pyo3.is_empty() {
        return Ok(None);
    }
    let mut extension = false;
    let mut embedding = false;
    for member in selected_members.iter().copied() {
        if !reaches_any(member, &nodes, &reachable_pyo3) {
            continue;
        }
        if let Some(package) = packages.get(member) {
            for target in &package.targets {
                extension |= target.crate_types.iter().any(|kind| kind == "cdylib");
                embedding |= target.kind.iter().any(|kind| kind == "bin");
            }
        }
    }
    let mut versions = BTreeSet::new();
    let mut features = BTreeSet::new();
    for id in &reachable_pyo3 {
        if let Some(package) = packages.get(id) {
            versions.insert(package.version.clone());
        }
        if let Some(node) = nodes.get(id) {
            features.extend(node.features.iter().cloned());
        }
    }
    extension |= features.contains("extension-module");
    embedding |= features.contains("auto-initialize");
    let shape = match (extension, embedding) {
        (true, false) => BuildShape::Extension,
        (false, true) => BuildShape::Embedding,
        (true, true) => BuildShape::Ambiguous,
        (false, false) => BuildShape::Embedding,
    };
    Ok(Some(DetectedPyo3 {
        shape,
        versions,
        features,
    }))
}

fn selected_workspace_members<'a>(
    metadata: &'a CargoMetadata,
    args: &[String],
    packages: &HashMap<&str, &'a CargoPackage>,
) -> Vec<&'a str> {
    let requested = package_args(args);
    let use_all = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--workspace" | "--all"));
    let candidates = if use_all || metadata.workspace_default_members.is_empty() {
        &metadata.workspace_members
    } else {
        &metadata.workspace_default_members
    };
    candidates
        .iter()
        .filter(|id| {
            requested.is_empty()
                || packages.get(id.as_str()).is_some_and(|package| {
                    requested
                        .iter()
                        .any(|spec| package_spec_matches(package, spec))
                })
        })
        .map(String::as_str)
        .collect()
}

fn package_args(args: &[String]) -> Vec<String> {
    let mut packages = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if matches!(arg.as_str(), "-p" | "--package") {
            if let Some(value) = iter.next() {
                packages.push(value.clone());
            }
        } else if let Some(value) = arg.strip_prefix("--package=") {
            packages.push(value.to_string());
        }
    }
    packages
}

fn package_spec_matches(package: &CargoPackage, spec: &str) -> bool {
    package.name == spec
        || spec
            .split_once('@')
            .is_some_and(|(name, version)| name == package.name && version == package.version)
}

fn reachable_pyo3_ids<'a>(
    starts: &[&'a str],
    nodes: &HashMap<&'a str, &'a CargoNode>,
    pyo3_ids: &HashSet<&'a str>,
) -> HashSet<&'a str> {
    let mut pending = starts.to_vec();
    let mut seen = HashSet::new();
    let mut reachable = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if pyo3_ids.contains(id) {
            reachable.insert(id);
        }
        if let Some(node) = nodes.get(id) {
            pending.extend(node.deps.iter().map(|dep| dep.pkg.as_str()));
        }
    }
    reachable
}

fn reaches_any(start: &str, nodes: &HashMap<&str, &CargoNode>, targets: &HashSet<&str>) -> bool {
    let mut pending = vec![start];
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if targets.contains(id) {
            return true;
        }
        if let Some(node) = nodes.get(id) {
            pending.extend(node.deps.iter().map(|dep| dep.pkg.as_str()));
        }
    }
    false
}

#[cfg(test)]
impl PolicyInput {
    fn test(shape: BuildShape, abi3: bool, host: &str, target: &str) -> Self {
        let detected = (shape != BuildShape::Absent).then(|| DetectedPyo3 {
            shape,
            versions: BTreeSet::from(["0.29.0".to_string()]),
            features: if abi3 {
                BTreeSet::from(["abi3-py310".to_string()])
            } else {
                BTreeSet::new()
            },
        });
        Self {
            host: host.to_string(),
            target: target.to_string(),
            detected,
            caller_pyo3: BTreeMap::new(),
            compatibility_sysroot: false,
            raw_dylib_disabled: false,
        }
    }
}

#[cfg(test)]
#[path = "pyo3_detect_tests.rs"]
mod tests;
