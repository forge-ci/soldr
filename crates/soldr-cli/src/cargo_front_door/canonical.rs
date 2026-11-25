//! Syntactic Cargo selection comparison. No resolution or subprocesses.
use crate::cargo_metadata_canonical::{
    sorted, CanonicalFeatures, CanonicalPackages, CanonicalProfile, CanonicalTarget, CanonicalUnify,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompilingVerb {
    Check,
    Clippy,
    Test,
    Build,
    NextestRun,
    NextestArchive,
}

impl CompilingVerb {
    pub(crate) fn from_args(args: &[String]) -> Option<Self> {
        let index = super::first_cargo_subcommand_index(args)?;
        let verb = args[index].as_str();
        match verb {
            "check" => Some(Self::Check),
            "clippy" => Some(Self::Clippy),
            "test" => Some(Self::Test),
            "build" => Some(Self::Build),
            "nextest" => match super::first_nextest_verb(args, index)? {
                "run" => Some(Self::NextestRun),
                "archive" => Some(Self::NextestArchive),
                _ => None,
            },
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Clippy => "clippy",
            Self::Test => "test",
            Self::Build => "build",
            Self::NextestRun => "nextest run",
            Self::NextestArchive => "nextest archive",
        }
    }

    /// Verbs that compile test binaries. `nextest archive` builds every test
    /// binary the archive ships, so it belongs here with the two runners.
    pub(crate) fn tests(self) -> bool {
        matches!(self, Self::Test | Self::NextestRun | Self::NextestArchive)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Selection {
    pub features: CanonicalFeatures,
    pub default_features: bool,
    pub packages: CanonicalPackages,
    pub exclude: Vec<String>,
    pub dev_targets: bool,
    pub target_explicit: bool,
    pub clippy_args: Vec<String>,
    verb: CompilingVerb,
    doc_tests: bool,
    members: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Drift {
    pub field: &'static str,
    pub found: String,
    pub canonical: String,
}

impl Selection {
    pub(crate) fn from_args(verb: CompilingVerb, args: &[String], members: &[String]) -> Self {
        Self::parse(
            verb,
            args,
            members,
            std::env::var_os("CARGO_BUILD_TARGET").is_some(),
        )
    }

    fn parse(verb: CompilingVerb, args: &[String], members: &[String], build_target: bool) -> Self {
        let members = sorted(members.to_vec());
        let mut selection = Self {
            features: CanonicalFeatures::Explicit(Vec::new()),
            default_features: true,
            packages: CanonicalPackages::Explicit(Vec::new()),
            exclude: Vec::new(),
            dev_targets: false,
            target_explicit: build_target,
            clippy_args: Vec::new(),
            verb,
            doc_tests: false,
            members,
        };
        let mut features = Vec::new();
        let mut packages = Vec::new();
        let mut target_selector = false;
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if arg == "--" {
                if verb == CompilingVerb::Clippy {
                    selection.clippy_args = iter.cloned().collect();
                }
                break;
            }
            let (key, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
            match key {
                "--all-features" => selection.features = CanonicalFeatures::All,
                "--no-default-features" => selection.default_features = false,
                "--workspace" | "--all" => selection.packages = CanonicalPackages::Workspace,
                "--all-targets" | "--tests" | "--benches" | "--examples" => {
                    selection.dev_targets = true;
                    target_selector = true;
                }
                "--lib" | "--bins" => target_selector = true,
                "--doc" if verb == CompilingVerb::Test => {
                    selection.doc_tests = true;
                    target_selector = true;
                }
                "--features" | "-F" | "--package" | "-p" | "--exclude" | "--target" | "--bin"
                | "--test" | "--bench" | "--example" => {
                    let value = inline.or_else(|| iter.next().map(String::as_str));
                    selection.take_value(
                        key,
                        value,
                        &mut features,
                        &mut packages,
                        &mut target_selector,
                    );
                }
                _ if arg.starts_with("-p") && arg.len() > 2 => packages.push(arg[2..].to_owned()),
                _ if arg.starts_with("-F") && arg.len() > 2 => {
                    split_features(&arg[2..], &mut features)
                }
                // Consume values that might otherwise look like selection flags.
                "--manifest-path"
                | "--target-dir"
                | "--config"
                | "--profile"
                | "--jobs"
                | "-j"
                | "--message-format"
                | "--color"
                | "--exclude-from-test"
                | "-E"
                | "--filter-expr"
                    if inline.is_none() =>
                {
                    iter.next();
                }
                _ => {}
            }
        }
        if selection.features != CanonicalFeatures::All {
            selection.features = CanonicalFeatures::Explicit(sorted(features));
        }
        let packages = sorted(packages);
        if !packages.is_empty() {
            selection.packages = if packages == selection.members {
                CanonicalPackages::Workspace
            } else {
                CanonicalPackages::Explicit(packages)
            };
        } else if selection.members.len() == 1 {
            selection.packages = CanonicalPackages::Workspace;
        }
        selection.exclude = sorted(selection.exclude);
        // Breadth, not dev-dependency activation: a test verb with no target
        // selector builds every test target, exactly like `--all-targets`
        // does for the other verbs. (Dev-dependencies are on for *any* test
        // verb, so they cannot distinguish these two shapes -- but they are
        // the reason the difference costs a rebuild.)
        selection.dev_targets |= verb.tests() && !target_selector;
        selection
    }

    fn take_value(
        &mut self,
        key: &str,
        value: Option<&str>,
        features: &mut Vec<String>,
        packages: &mut Vec<String>,
        selector: &mut bool,
    ) {
        match key {
            "--features" | "-F" => {
                if let Some(value) = value {
                    split_features(value, features);
                }
            }
            "--package" | "-p" => {
                if let Some(value) = value {
                    packages.push(value.to_owned());
                }
            }
            "--exclude" => {
                if let Some(value) = value {
                    self.exclude.push(value.to_owned());
                }
            }
            "--target" => self.target_explicit = true,
            // A single-target selector (`--bin`, `--test`, `--bench`,
            // `--example`) narrows the run, so it never widens `dev_targets`:
            // that field means "the dev target kinds are selected", i.e.
            // `--all-targets` or a test verb with no narrowing at all. It is
            // the same axis `soldr cargo` prints as `--all-targets`.
            _ => *selector = true,
        }
    }

    pub(crate) fn drift(&self, profile: &CanonicalProfile) -> Vec<Drift> {
        let drift = self.compare(profile);
        if drift.is_empty() {
            return drift;
        }
        for alternate in &profile.alternates {
            let mut candidate = profile.clone();
            if let Some(features) = &alternate.features {
                candidate.features = features.clone();
            }
            if let Some(packages) = &alternate.packages {
                candidate.packages = packages.clone();
                if matches!(packages, CanonicalPackages::Explicit(_)) {
                    candidate.exclude.clear();
                }
            }
            if let Some(dev_targets) = alternate.dev_targets {
                candidate.dev_targets = dev_targets;
            }
            if let Some(args) = &alternate.clippy_args {
                candidate.clippy_args = args.clone();
            }
            if self.compare(&candidate).is_empty() {
                return Vec::new();
            }
        }
        drift
    }

    fn compare(&self, profile: &CanonicalProfile) -> Vec<Drift> {
        let mut drift = Vec::new();
        if profile.unify != CanonicalUnify::Workspace
            && self.package_set(&self.packages, &self.exclude)
                != self.package_set(&profile.packages, &profile.exclude)
        {
            drift.push(Drift {
                field: "packages",
                found: package_flags(&self.packages, &self.exclude),
                canonical: package_flags(&profile.packages, &profile.exclude),
            });
        }
        // Keep the diagnostic order stable: packages, features, defaults, dev targets, target, Clippy.
        let mut compare = |field, found: String, canonical: String| {
            if found != canonical {
                drift.push(Drift {
                    field,
                    found,
                    canonical,
                });
            }
        };
        compare(
            "features",
            feature_description(&self.features),
            feature_description(&profile.features),
        );
        compare(
            "default-features",
            self.default_features.to_string(),
            profile.default_features.to_string(),
        );
        // Doctests form their own rustdoc family and Cargo forbids combining
        // --doc with target-kind flags. Keep the remaining axes canonical.
        if !self.doc_tests {
            compare(
                "dev-targets",
                self.dev_targets.to_string(),
                profile.dev_targets.to_string(),
            );
        }
        compare(
            "target",
            target_description(self.target_explicit).into(),
            target_description(profile.target == CanonicalTarget::ExplicitHost).into(),
        );
        if self.verb == CompilingVerb::Clippy {
            compare(
                "clippy args",
                self.clippy_args.join(" "),
                profile.clippy_args.join(" "),
            );
        }
        drift
    }

    fn package_set(&self, packages: &CanonicalPackages, exclude: &[String]) -> Vec<String> {
        let selected = match packages {
            CanonicalPackages::Workspace => &self.members,
            CanonicalPackages::Explicit(packages) => packages,
        };
        sorted(
            selected
                .iter()
                .filter(|name| !exclude.contains(name))
                .cloned()
                .collect(),
        )
    }
}

fn split_features(value: &str, features: &mut Vec<String>) {
    features.extend(
        value
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    );
}

fn target_description(explicit: bool) -> &'static str {
    if explicit {
        "explicit-host"
    } else {
        "implicit"
    }
}

fn feature_description(features: &CanonicalFeatures) -> String {
    match features {
        CanonicalFeatures::All => "all".into(),
        CanonicalFeatures::Explicit(features) if features.is_empty() => "none".into(),
        CanonicalFeatures::Explicit(features) => sorted(features.clone()).join(","),
    }
}

pub(super) fn package_flags(packages: &CanonicalPackages, exclude: &[String]) -> String {
    let mut flags = match packages {
        CanonicalPackages::Workspace => vec!["--workspace".into()],
        CanonicalPackages::Explicit(packages) if packages.is_empty() => {
            vec!["default packages".into()]
        }
        CanonicalPackages::Explicit(packages) => sorted(packages.clone())
            .iter()
            .map(|p| format!("-p {p}"))
            .collect(),
    };
    flags.extend(exclude.iter().map(|p| format!("--exclude {p}")));
    flags.join(" ")
}

/// Cargo argv shared by entrypoints and the diagnostic renderer. Keep values
/// intact: a Clippy argument can contain whitespace.
pub(crate) fn args(profile: &CanonicalProfile, verb: CompilingVerb, host: &str) -> Vec<String> {
    let mut parts: Vec<String> = verb.name().split_whitespace().map(str::to_owned).collect();
    match &profile.packages {
        CanonicalPackages::Workspace => parts.push("--workspace".into()),
        CanonicalPackages::Explicit(packages) => {
            for package in sorted(packages.clone()) {
                parts.extend(["-p".into(), package]);
            }
        }
    }
    for package in &profile.exclude {
        parts.extend(["--exclude".into(), package.clone()]);
    }
    match &profile.features {
        CanonicalFeatures::All => parts.push("--all-features".into()),
        CanonicalFeatures::Explicit(features) if !features.is_empty() => {
            parts.extend(["--features".into(), features.join(",")]);
        }
        CanonicalFeatures::Explicit(_) => {}
    }
    if !profile.default_features {
        parts.push("--no-default-features".into());
    }
    if profile.dev_targets {
        parts.push("--all-targets".into());
    } else if verb.tests() {
        parts.extend(["--lib".into(), "--bins".into()]);
    }
    if profile.target == CanonicalTarget::ExplicitHost {
        parts.extend(["--target".into(), host.into()]);
    }
    if verb == CompilingVerb::Clippy && !profile.clippy_args.is_empty() {
        parts.push("--".into());
        parts.extend(profile.clippy_args.clone());
    }
    parts
}

/// Explicit caller scope remains visible to normal front-door enforcement.
pub(crate) fn scoped_args(
    profile: &CanonicalProfile,
    verb: CompilingVerb,
    host: &str,
    scope: &[String],
) -> Vec<String> {
    let mut args = args(profile, verb, host);
    let has = |flags: &[&str]| {
        scope.iter().any(|arg| {
            let key = arg.split('=').next().unwrap_or(arg);
            flags.contains(&key)
                || (flags.contains(&"-p") && arg.starts_with("-p") && arg.len() > 2)
                || (flags.contains(&"-F") && arg.starts_with("-F") && arg.len() > 2)
        })
    };
    let packages = has(&["--workspace", "--all", "--package", "-p"]);
    let features = has(&["--all-features", "--features", "-F"]);
    let target = has(&["--target"]);
    let defaults = has(&["--no-default-features"]);
    let breadth = has(&[
        "--all-targets",
        "--lib",
        "--bins",
        "--bin",
        "--tests",
        "--test",
        "--benches",
        "--bench",
        "--examples",
        "--example",
    ]);
    let mut skip_value = false;
    let mut tail = false;
    args.retain(|arg| {
        if skip_value {
            skip_value = false;
            return false;
        }
        tail |= arg == "--";
        if tail {
            return true;
        }
        if (packages && matches!(arg.as_str(), "-p" | "--exclude"))
            || (features && arg == "--features")
            || (target && arg == "--target")
        {
            skip_value = true;
            return false;
        }
        !((defaults && arg == "--no-default-features")
            || (packages && arg == "--workspace")
            || (features && arg == "--all-features")
            || (breadth && matches!(arg.as_str(), "--all-targets" | "--lib" | "--bins")))
    });
    let tail = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    args.splice(tail..tail, scope.iter().cloned());
    args
}

pub(crate) fn command(profile: &CanonicalProfile, verb: CompilingVerb, host: &str) -> String {
    let mut parts: Vec<String> = vec!["soldr".into(), "cargo".into()];
    parts.extend(args(profile, verb, host).iter().map(|arg| {
        if arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-/.,:=+".contains(c))
            && !arg.is_empty()
        {
            arg.clone()
        } else {
            crate::linker_shim::shell_single_quote(arg)
        }
    }));
    parts.join(" ")
}

#[cfg(test)]
#[path = "canonical_tests.rs"]
mod tests;
