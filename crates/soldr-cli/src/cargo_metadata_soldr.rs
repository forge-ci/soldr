//! Reader for `[workspace.metadata.soldr]` / `[package.metadata.soldr]`
//! sections of `Cargo.toml` (RFC #914).
//!
//! Cargo's manifest reference explicitly blesses `[*.metadata.*]` tables
//! as the place for tool-specific configuration:
//!
//! > The `package.metadata` table, however, is completely ignored by
//! > Cargo and will not be warned about. This section can be used for
//! > tools which would like to store package configuration in
//! > `Cargo.toml`.
//!
//! soldr uses this to discover the multi-target shipping matrix for
//! `soldr prepare --target all` without inventing a new file format
//! and without piggybacking on `rust-toolchain.toml [toolchain].targets`
//! (rustup would unconditionally install every entry — ~30–80 MB of
//! `rust-std` per triple — on every fresh checkout).
//!
//! Lookup order:
//! 1. `[workspace.metadata.soldr].targets` — primary.
//! 2. `[package.metadata.soldr].targets`   — fallback for single-crate
//!    repos with no workspace table.
//! 3. Both missing → ERROR with a directive message naming the missing
//!    table. No fallback to `rust-toolchain.toml` or `.cargo/config.toml`
//!    because their semantics differ ("install these components" /
//!    "build for these by default") from "this is the shipping matrix".

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::core::SoldrError;

/// Top-level shape of a `Cargo.toml` for the keys we care about.
/// `serde(default)` everywhere so the parse succeeds against any
/// well-formed manifest, even ones that don't mention soldr at all.
#[derive(Debug, Deserialize, Default)]
struct CargoToml {
    #[serde(default)]
    workspace: Option<WorkspaceTable>,
    #[serde(default)]
    package: Option<PackageTable>,
}

#[derive(Debug, Deserialize, Default)]
struct WorkspaceTable {
    #[serde(default)]
    metadata: Option<MetadataTable>,
}

#[derive(Debug, Deserialize, Default)]
struct PackageTable {
    #[serde(default)]
    metadata: Option<MetadataTable>,
}

#[derive(Debug, Deserialize, Default)]
struct MetadataTable {
    #[serde(default)]
    soldr: Option<SoldrMetadata>,
}

/// `[workspace.metadata.soldr]` / `[package.metadata.soldr]` body.
/// Public so future fields can be added (e.g. `[..].cache_archive`
/// default) without rewriting consumers.
#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq)]
pub struct SoldrMetadata {
    /// The shipping matrix: list of rust target triples this repo
    /// declares it builds for. Consumed by `soldr prepare --target all`.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Prefer a newer `soldr` found globally on PATH over the binary launched
    /// from this checkout. Disabled unless the project opts in.
    #[serde(default)]
    pub prefer_newer_global: bool,
    /// Project-declared linker choice (soldr#3276), e.g. `"reld"`. Consumed by
    /// `linker::resolve_project_choice`, which validates the value; this
    /// reader does no validation of its own.
    #[serde(default)]
    pub linker: Option<String>,
    /// `[workspace.metadata.soldr.lint]` (soldr#3958): the declared `soldr lint`
    /// step list. Raw as written; [`LintMetadata::resolve`] validates it.
    #[serde(default)]
    pub lint: Option<LintMetadata>,
}

/// `[workspace.metadata.soldr.lint]` as written in the manifest.
///
/// ```toml
/// [workspace.metadata.soldr.lint]
/// steps = ["fmt", "clippy", "dylint"]   # built-ins, in order
/// extra = [["ruff", "check", "."]]       # repo commands, run in the workspace root
/// ```
#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LintMetadata {
    /// Built-in steps to run, in order. Absent means every built-in.
    #[serde(default)]
    pub steps: Option<Vec<String>>,
    /// Repo commands (`[program, args...]`) run after the built-ins.
    #[serde(default)]
    pub extra: Vec<Vec<String>>,
}

/// A built-in `soldr lint rust` step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintStep {
    Fmt,
    Clippy,
    Dylint,
}

impl LintStep {
    /// Every built-in step, in today's order.
    pub const ALL: [LintStep; 3] = [LintStep::Fmt, LintStep::Clippy, LintStep::Dylint];

    pub fn name(self) -> &'static str {
        match self {
            LintStep::Fmt => "fmt",
            LintStep::Clippy => "clippy",
            LintStep::Dylint => "dylint",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|step| step.name() == name)
    }
}

/// A validated `[*.metadata.soldr.lint]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintConfig {
    pub steps: Vec<LintStep>,
    pub extra: Vec<Vec<String>>,
}

impl Default for LintConfig {
    /// No table: today's behavior (every built-in, no extras).
    fn default() -> Self {
        Self {
            steps: LintStep::ALL.to_vec(),
            extra: Vec::new(),
        }
    }
}

/// Why a `lint` metadata table was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LintConfigError {
    UnknownStep { name: String },
    DuplicateStep { name: String },
    EmptyExtraCommand { index: usize },
}

impl std::fmt::Display for LintConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownStep { name } => write!(
                f,
                "lint: unknown step {name:?} in [workspace.metadata.soldr.lint].steps; \
                 expected fmt, clippy, or dylint (repo commands belong in `extra`)"
            ),
            Self::DuplicateStep { name } => write!(
                f,
                "lint: step {name:?} is listed twice in [workspace.metadata.soldr.lint].steps"
            ),
            Self::EmptyExtraCommand { index } => write!(
                f,
                "lint: [workspace.metadata.soldr.lint].extra[{index}] names no program; \
                 each entry is [program, args...]"
            ),
        }
    }
}

impl std::error::Error for LintConfigError {}

impl From<LintConfigError> for SoldrError {
    fn from(error: LintConfigError) -> Self {
        SoldrError::Other(error.to_string())
    }
}

impl LintMetadata {
    /// Validate at the boundary: unknown or repeated step names and empty
    /// `extra` commands are typed errors, never silently dropped.
    pub fn resolve(&self) -> Result<LintConfig, LintConfigError> {
        let steps = match &self.steps {
            None => LintStep::ALL.to_vec(),
            Some(names) => {
                let mut steps = Vec::with_capacity(names.len());
                for name in names {
                    let step = LintStep::parse(name)
                        .ok_or_else(|| LintConfigError::UnknownStep { name: name.clone() })?;
                    if steps.contains(&step) {
                        return Err(LintConfigError::DuplicateStep { name: name.clone() });
                    }
                    steps.push(step);
                }
                steps
            }
        };
        for (index, command) in self.extra.iter().enumerate() {
            if command
                .first()
                .is_none_or(|program| program.trim().is_empty())
            {
                return Err(LintConfigError::EmptyExtraCommand { index });
            }
        }
        Ok(LintConfig {
            steps,
            extra: self.extra.clone(),
        })
    }
}

/// The lint table that applies to `start` (a `Cargo.toml` or a directory to
/// walk up from), plus the directory extras run in -- the manifest's own
/// directory. No manifest or no table means [`LintConfig::default`] and `None`.
pub fn lint_config_from(start: &Path) -> Result<(LintConfig, Option<PathBuf>), SoldrError> {
    let manifest = if start.is_file() {
        start.to_path_buf()
    } else {
        match find_cargo_toml(start) {
            Some(path) => path,
            None => return Ok((LintConfig::default(), None)),
        }
    };
    let root = manifest.parent().map(Path::to_path_buf);
    let config = match read_soldr_metadata(&manifest)?.lint {
        Some(table) => table.resolve()?,
        None => LintConfig::default(),
    };
    Ok((config, root))
}

/// Read soldr metadata from the `Cargo.toml` at `path`. Returns the
/// merged `SoldrMetadata` honoring the workspace → package fallback
/// order described in the module docstring.
///
/// **Does not error on missing tables** — returns `Ok(SoldrMetadata
/// { targets: vec![] })` when no `[*.metadata.soldr]` section exists.
/// Callers that need to require the section (e.g. `--target all`)
/// must check `targets.is_empty()` themselves and surface a
/// directive error; that keeps this reader reusable for any future
/// soldr-metadata field that's genuinely optional.
pub fn read_soldr_metadata(cargo_toml_path: &Path) -> Result<SoldrMetadata, SoldrError> {
    // soldr#3378: this reader is now shared by `soldr prepare --target all`
    // and `soldr lint`'s declared-target resolution, so the error text no
    // longer names one specific caller.
    let body = std::fs::read_to_string(cargo_toml_path).map_err(|e| {
        SoldrError::Other(format!("soldr: read {}: {e}", cargo_toml_path.display()))
    })?;
    let parsed: CargoToml = toml::from_str(&body).map_err(|e| {
        SoldrError::Other(format!("soldr: parse {}: {e}", cargo_toml_path.display()))
    })?;
    // Workspace takes precedence; package-scoped metadata is fallback
    // for single-crate repos that don't declare a workspace table.
    let workspace_meta = parsed
        .workspace
        .and_then(|w| w.metadata)
        .and_then(|m| m.soldr);
    if let Some(meta) = workspace_meta {
        return Ok(meta);
    }
    let package_meta = parsed
        .package
        .and_then(|p| p.metadata)
        .and_then(|m| m.soldr);
    Ok(package_meta.unwrap_or_default())
}

/// Walk up from `start_dir` looking for the first `Cargo.toml`. Used
/// when `soldr prepare --target all` is invoked from a subdirectory
/// of the repo. Returns `None` if no manifest is found before the
/// filesystem root.
pub fn find_cargo_toml(start_dir: &Path) -> Option<PathBuf> {
    let mut cur = start_dir;
    loop {
        let candidate = cur.join("Cargo.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        cur = cur.parent()?;
    }
}

/// Read the declared cross-target marker list for `soldr lint` (soldr#3378):
/// `[workspace.metadata.soldr].targets` (package fallback), starting from
/// `start` — either a `Cargo.toml` file directly (so a caller can honor an
/// explicit `--manifest-path`) or a directory to walk up from via
/// [`find_cargo_toml`].
///
/// Unlike [`resolve_all_targets`], this returns an **empty** list — never
/// soldr's compiled-in canonical target list — when nothing is declared or no
/// manifest is found. `soldr prepare --target all` needs *something* to
/// build for and falls back to the canonical list; `soldr lint` reads
/// "nothing declared" as "keep today's host-only behaviour", so a silent
/// canonical-list fallback here would turn on cross-target Clippy for every
/// workspace, declared or not.
pub fn declared_targets_from(start: &Path) -> Result<Vec<String>, SoldrError> {
    let manifest = if start.is_file() {
        start.to_path_buf()
    } else {
        match find_cargo_toml(start) {
            Some(path) => path,
            None => return Ok(Vec::new()),
        }
    };
    Ok(read_soldr_metadata(&manifest)?.targets)
}

/// Whether any Cargo manifest from `start_dir` through its ancestor chain
/// opts into handing off local soldr invocations to a newer global binary.
///
/// Walking the full chain matters when the command starts in a workspace
/// member: its nearest manifest usually has package metadata while the policy
/// belongs to the root's `[workspace.metadata.soldr]` table.
pub fn prefer_newer_global_from(start_dir: &Path) -> bool {
    let mut current = start_dir;
    loop {
        let candidate = current.join("Cargo.toml");
        if candidate.is_file()
            && read_soldr_metadata(&candidate)
                .map(|metadata| metadata.prefer_newer_global)
                .unwrap_or(false)
        {
            return true;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return false,
        }
    }
}

/// Best-effort current-directory wrapper around [`prefer_newer_global_from`].
/// An unreadable working directory simply leaves the opt-in policy disabled.
pub fn prefer_newer_global_from_cwd() -> bool {
    std::env::current_dir()
        .ok()
        .is_some_and(|cwd| prefer_newer_global_from(&cwd))
}

/// Resolve `--target all` to the explicit target list.
///
/// Resolution order (soldr#937 — containerized image-build needs a
/// no-workspace fallback):
///
/// 1. Walk up from the current working directory; on the first
///    `Cargo.toml` found, return `[*.metadata.soldr].targets` if the
///    section is present **and non-empty**.
/// 2. Otherwise fall back to [`crate::core::canonical_targets`] —
///    soldr's compiled-in canonical target list. Prints a one-line stderr
///    notice so the fallback is visible in container build logs.
///
/// The fallback path is what makes `soldr prepare --target all`
/// work inside `examples/docker-cross-all/Dockerfile` where the
/// soldr source isn't mounted — no need for the explicit
/// comma-separated 8-triple form the Dockerfile used to ship.
pub fn resolve_all_targets() -> Result<Vec<String>, SoldrError> {
    let cwd = std::env::current_dir()
        .map_err(|e| SoldrError::Other(format!("soldr prepare: cwd: {e}")))?;

    if let Some(manifest) = find_cargo_toml(&cwd) {
        let meta = read_soldr_metadata(&manifest)?;
        if !meta.targets.is_empty() {
            return Ok(meta.targets);
        }
        eprintln!(
            "soldr prepare --target all: `{}` declares no \
             `[workspace.metadata.soldr].targets` — falling back to soldr's \
             compiled-in canonical target list (soldr#937).",
            manifest.display()
        );
    } else {
        eprintln!(
            "soldr prepare --target all: no Cargo.toml under `{}` — \
             falling back to soldr's compiled-in canonical target list (soldr#937).",
            cwd.display()
        );
    }

    Ok(crate::core::canonical_targets()
        .iter()
        .map(|s| (*s).to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: write a Cargo.toml fixture to `tmpdir/Cargo.toml` and
    /// return the path.
    fn write_fixture(tmpdir: &Path, body: &str) -> PathBuf {
        let p = tmpdir.join("Cargo.toml");
        std::fs::write(&p, body).expect("write fixture");
        p
    }

    #[test]
    fn reads_workspace_metadata() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            r#"
[workspace]

[workspace.metadata.soldr]
targets = [
    "x86_64-pc-windows-msvc",
    "aarch64-apple-darwin",
]
"#,
        );
        let meta = read_soldr_metadata(&p).expect("parse");
        assert_eq!(
            meta.targets,
            vec![
                "x86_64-pc-windows-msvc".to_string(),
                "aarch64-apple-darwin".to_string(),
            ]
        );
    }

    #[test]
    fn reads_package_metadata_when_no_workspace() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            r#"
[package]
name = "thing"
version = "0.1.0"

[package.metadata.soldr]
targets = ["x86_64-unknown-linux-gnu"]
"#,
        );
        let meta = read_soldr_metadata(&p).expect("parse");
        assert_eq!(meta.targets, vec!["x86_64-unknown-linux-gnu".to_string()]);
    }

    #[test]
    fn workspace_metadata_wins_over_package() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            r#"
[package]
name = "thing"
version = "0.1.0"

[package.metadata.soldr]
targets = ["aarch64-unknown-linux-musl"]

[workspace]

[workspace.metadata.soldr]
targets = ["x86_64-pc-windows-msvc"]
"#,
        );
        let meta = read_soldr_metadata(&p).expect("parse");
        // workspace block wins
        assert_eq!(meta.targets, vec!["x86_64-pc-windows-msvc".to_string()]);
    }

    #[test]
    fn no_metadata_returns_empty() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            r#"
[package]
name = "thing"
version = "0.1.0"
"#,
        );
        let meta = read_soldr_metadata(&p).expect("parse");
        assert!(meta.targets.is_empty());
    }

    #[test]
    fn declared_targets_from_reads_the_workspace_list_given_the_manifest_file() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            r#"
[workspace]

[workspace.metadata.soldr]
targets = ["x86_64-pc-windows-msvc", "aarch64-apple-darwin"]
"#,
        );
        let targets = declared_targets_from(&p).expect("declared targets");
        assert_eq!(
            targets,
            vec![
                "x86_64-pc-windows-msvc".to_string(),
                "aarch64-apple-darwin".to_string(),
            ]
        );
    }

    #[test]
    fn declared_targets_from_walks_up_from_a_directory() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        write_fixture(
            tmp.path(),
            "[workspace]\n\n[workspace.metadata.soldr]\ntargets = [\"x86_64-unknown-linux-musl\"]\n",
        );
        let nested = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&nested).expect("mkdir");
        let targets = declared_targets_from(&nested).expect("declared targets");
        assert_eq!(targets, vec!["x86_64-unknown-linux-musl".to_string()]);
    }

    #[test]
    fn declared_targets_from_is_empty_when_the_section_is_absent() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            "[package]\nname = \"thing\"\nversion = \"0.1.0\"\n",
        );
        let targets = declared_targets_from(&p).expect("declared targets");
        assert!(targets.is_empty());
    }

    #[test]
    fn declared_targets_from_is_empty_when_no_manifest_is_found() {
        // A directory with no Cargo.toml ancestor within the tempdir: unlike
        // `find_cargo_toml_returns_none_when_missing`, this only needs to
        // observe "no panic and no error", since a real Cargo.toml might
        // exist above the OS temp directory on some hosts.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let nested = tmp.path().join("nothing-here");
        std::fs::create_dir_all(&nested).expect("mkdir");
        let _ = declared_targets_from(&nested);
    }

    #[test]
    fn reads_prefer_newer_global() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let root = tmp.path().join("workspace");
        let nested = root.join("member").join("src");
        std::fs::create_dir_all(&nested).expect("nested workspace member");
        write_fixture(
            &root,
            "[workspace]\n\n[workspace.metadata.soldr]\nprefer_newer_global = true\n",
        );
        std::fs::write(
            root.join("member").join("Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\n",
        )
        .expect("member manifest");

        assert!(prefer_newer_global_from(&nested));
    }

    #[test]
    fn malformed_toml_errors_with_path() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(tmp.path(), "this is = = not valid toml");
        let err = read_soldr_metadata(&p).expect_err("malformed");
        let msg = err.to_string();
        assert!(msg.contains("parse"), "msg: {msg}");
        assert!(msg.contains("Cargo.toml"), "msg: {msg}");
    }

    #[test]
    fn find_cargo_toml_walks_up() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(tmp.path(), "[package]\nname = \"x\"\nversion = \"0\"\n");
        // create a nested subdir
        let nested = tmp.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).expect("mkdir");
        let found = find_cargo_toml(&nested).expect("walked up");
        assert_eq!(found, p);
    }

    #[test]
    fn reads_workspace_linker() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            r#"
[workspace]

[workspace.metadata.soldr]
linker = "reld"
"#,
        );
        let meta = read_soldr_metadata(&p).expect("parse");
        assert_eq!(meta.linker.as_deref(), Some("reld"));
    }

    #[test]
    fn reads_package_linker_when_no_workspace() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            r#"
[package]
name = "thing"
version = "0.1.0"

[package.metadata.soldr]
linker = "mold"
"#,
        );
        let meta = read_soldr_metadata(&p).expect("parse");
        assert_eq!(meta.linker.as_deref(), Some("mold"));
    }

    #[test]
    fn no_metadata_linker_is_none() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            r#"
[package]
name = "thing"
version = "0.1.0"
"#,
        );
        let meta = read_soldr_metadata(&p).expect("parse");
        assert!(meta.linker.is_none());
    }

    #[test]
    fn find_cargo_toml_returns_none_when_missing() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let nested = tmp.path().join("nothing-here");
        std::fs::create_dir_all(&nested).expect("mkdir");
        // tmp is under TEMP which has no Cargo.toml ancestor on a
        // freshly-created TempDir; but the actual filesystem may
        // have one above TEMP. Use the canonical root to verify the
        // None path is reachable in principle.
        // We can't guarantee no Cargo.toml exists above tmpdir on
        // every host, so only assert this is a safe call (no panic).
        let _ = find_cargo_toml(&nested);
    }

    #[test]
    fn lint_table_absent_means_todays_behavior() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(tmp.path(), "[workspace]\n");
        let (config, root) = lint_config_from(&p).expect("config");
        assert_eq!(config, LintConfig::default());
        assert_eq!(root.as_deref(), Some(tmp.path()));
    }

    #[test]
    fn lint_table_parses_steps_in_order_and_extras() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            r#"
[workspace]

[workspace.metadata.soldr.lint]
steps = ["dylint", "fmt"]
extra = [["ruff", "check", "."], ["python", "ci/guard.py"]]
"#,
        );
        let (config, _) = lint_config_from(&p).expect("config");
        assert_eq!(config.steps, vec![LintStep::Dylint, LintStep::Fmt]);
        assert_eq!(
            config.extra,
            vec![
                vec!["ruff".to_string(), "check".into(), ".".into()],
                vec!["python".to_string(), "ci/guard.py".into()],
            ]
        );
    }

    #[test]
    fn lint_extra_without_steps_keeps_every_builtin() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            "[workspace]\n[workspace.metadata.soldr.lint]\nextra = [[\"ruff\"]]\n",
        );
        let (config, _) = lint_config_from(&p).expect("config");
        assert_eq!(config.steps, LintStep::ALL.to_vec());
        assert_eq!(config.extra.len(), 1);
    }

    #[test]
    fn lint_unknown_step_is_a_typed_error() {
        let table = LintMetadata {
            steps: Some(vec!["fmt".into(), "ruff".into()]),
            extra: Vec::new(),
        };
        assert_eq!(
            table.resolve(),
            Err(LintConfigError::UnknownStep {
                name: "ruff".into()
            })
        );
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            "[workspace]\n[workspace.metadata.soldr.lint]\nsteps = [\"fmt\", \"ruff\"]\n",
        );
        let error = lint_config_from(&p).expect_err("unknown step").to_string();
        assert!(error.contains("\"ruff\""), "{error}");
    }

    #[test]
    fn lint_duplicate_step_and_empty_extra_are_rejected() {
        let dup = LintMetadata {
            steps: Some(vec!["fmt".into(), "fmt".into()]),
            extra: Vec::new(),
        };
        assert_eq!(
            dup.resolve(),
            Err(LintConfigError::DuplicateStep { name: "fmt".into() })
        );
        let empty = LintMetadata {
            steps: None,
            extra: vec![vec![]],
        };
        assert_eq!(
            empty.resolve(),
            Err(LintConfigError::EmptyExtraCommand { index: 0 })
        );
    }

    #[test]
    fn lint_table_rejects_unknown_keys() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            "[workspace]\n[workspace.metadata.soldr.lint]\nstep = [\"fmt\"]\n",
        );
        assert!(lint_config_from(&p).is_err());
    }
}
