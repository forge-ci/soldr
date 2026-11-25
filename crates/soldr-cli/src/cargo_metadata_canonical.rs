//! The optional canonical Cargo selection (soldr#4195).
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::core::SoldrError;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CanonicalEnforce {
    Off,
    #[default]
    Warn,
    Refuse,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "FeatureValue")]
pub(crate) enum CanonicalFeatures {
    Explicit(Vec<String>),
    All,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum FeatureValue {
    Name(String),
    List(Vec<String>),
}

impl TryFrom<FeatureValue> for CanonicalFeatures {
    type Error = String;

    fn try_from(value: FeatureValue) -> Result<Self, Self::Error> {
        match value {
            FeatureValue::List(features) => Ok(Self::Explicit(sorted(features))),
            FeatureValue::Name(name) if name == "all" => Ok(Self::All),
            FeatureValue::Name(name) => Err(format!(
                "cargo: features: unknown value {name:?}; expected \"all\" or a list"
            )),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "FeatureValue")]
pub(crate) enum CanonicalPackages {
    #[default]
    Workspace,
    Explicit(Vec<String>),
}

impl TryFrom<FeatureValue> for CanonicalPackages {
    type Error = String;

    fn try_from(value: FeatureValue) -> Result<Self, Self::Error> {
        match value {
            FeatureValue::List(packages) => Ok(Self::Explicit(sorted(packages))),
            FeatureValue::Name(name) if name == "workspace" => Ok(Self::Workspace),
            FeatureValue::Name(name) => Err(format!(
                "cargo: packages: unknown value {name:?}; expected \"workspace\" or a list"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CanonicalTarget {
    #[default]
    ExplicitHost,
    Implicit,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CanonicalUnify {
    #[default]
    Auto,
    Workspace,
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CanonicalProfile {
    pub enforce: CanonicalEnforce,
    pub features: CanonicalFeatures,
    pub default_features: bool,
    pub packages: CanonicalPackages,
    pub exclude: Vec<String>,
    pub dev_targets: bool,
    pub target: CanonicalTarget,
    pub clippy_args: Vec<String>,
    pub unify: CanonicalUnify,
    pub alternates: Vec<AlternateProfile>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct AlternateProfile {
    #[serde(skip)]
    pub name: String,
    pub features: Option<CanonicalFeatures>,
    pub packages: Option<CanonicalPackages>,
    pub dev_targets: Option<bool>,
    pub clippy_args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct ProfileTable {
    enforce: CanonicalEnforce,
    features: CanonicalFeatures,
    default_features: bool,
    packages: CanonicalPackages,
    exclude: Option<Vec<String>>,
    dev_targets: bool,
    target: CanonicalTarget,
    clippy_args: Vec<String>,
    unify: CanonicalUnify,
    alternates: BTreeMap<String, AlternateProfile>,
}

impl Default for ProfileTable {
    fn default() -> Self {
        Self {
            enforce: CanonicalEnforce::Warn,
            features: CanonicalFeatures::Explicit(Vec::new()),
            default_features: true,
            packages: CanonicalPackages::Workspace,
            exclude: None,
            dev_targets: true,
            target: CanonicalTarget::ExplicitHost,
            clippy_args: vec!["-D".into(), "warnings".into()],
            unify: CanonicalUnify::Auto,
            alternates: BTreeMap::new(),
        }
    }
}

pub(crate) fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

impl CanonicalProfile {
    fn parse(value: toml::Value) -> Result<Self, SoldrError> {
        let table: ProfileTable = value
            .try_into()
            .map_err(|error| SoldrError::Other(format!("canonical cargo profile: {error}")))?;
        if !table.default_features
            && (table.features == CanonicalFeatures::All
                || table
                    .alternates
                    .values()
                    .any(|alt| alt.features == Some(CanonicalFeatures::All)))
        {
            return Err(SoldrError::Other(
                "cargo: features = \"all\" conflicts with default-features = false".into(),
            ));
        }
        if table.exclude.is_some() && table.packages != CanonicalPackages::Workspace {
            return Err(SoldrError::Other(
                "cargo: exclude requires packages = \"workspace\"".into(),
            ));
        }
        Ok(Self {
            enforce: table.enforce,
            features: table.features,
            default_features: table.default_features,
            packages: table.packages,
            exclude: sorted(table.exclude.unwrap_or_default()),
            dev_targets: table.dev_targets,
            target: table.target,
            clippy_args: table.clippy_args,
            unify: table.unify,
            alternates: table
                .alternates
                .into_iter()
                .map(|(name, mut alternate)| {
                    alternate.name = name;
                    alternate
                })
                .collect(),
        })
    }
}

/// Find the owning workspace even when invoked inside a member. Cargo's
/// explicit `package.workspace` pointer also permits a non-ancestor root.
pub(crate) fn profile_from(
    start: &Path,
) -> Result<Option<(CanonicalProfile, PathBuf)>, SoldrError> {
    let Some(nearest) = crate::cargo_metadata_soldr::find_cargo_toml(start) else {
        return Ok(None);
    };
    let local = read_manifest(&nearest)?;
    let pointed = local
        .get("package")
        .and_then(|p| p.get("workspace"))
        .and_then(toml::Value::as_str);
    if let Some(pointer) = pointed {
        let root = nearest
            .parent()
            .unwrap_or(start)
            .join(pointer)
            .join("Cargo.toml");
        return read_profile(&root, &read_manifest(&root)?);
    }
    for directory in nearest.parent().unwrap_or(start).ancestors() {
        let path = directory.join("Cargo.toml");
        if !path.is_file() {
            continue;
        }
        let manifest = if path == nearest {
            local.clone()
        } else {
            read_manifest(&path)?
        };
        if manifest.get("workspace").is_some() {
            return read_profile(&path, &manifest);
        }
    }
    read_profile(&nearest, &local)
}

fn read_manifest(path: &Path) -> Result<toml::Value, SoldrError> {
    let body = std::fs::read_to_string(path)
        .map_err(|error| SoldrError::Other(format!("{}: {error}", path.display())))?;
    toml::from_str(&body).map_err(|error| SoldrError::Other(format!("{}: {error}", path.display())))
}

fn read_profile(
    path: &Path,
    manifest: &toml::Value,
) -> Result<Option<(CanonicalProfile, PathBuf)>, SoldrError> {
    let metadata = |section: &str| manifest.get(section)?.get("metadata")?.get("soldr");
    let table = metadata("workspace")
        .or_else(|| metadata("package"))
        .and_then(|m| m.get("cargo"));
    table
        .cloned()
        .map(|value| CanonicalProfile::parse(value).map(|profile| (profile, path.to_path_buf())))
        .transpose()
}

#[cfg(test)]
#[path = "cargo_metadata_canonical_tests.rs"]
mod tests;
