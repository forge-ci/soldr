//! Verdict key derivation (soldr#3894 §3).
//!
//! A key is a BLAKE3 digest, truncated to 128 bits, over everything that can
//! change a check's result: the workspace's tracked files (by git blob id,
//! so nothing is read for a clean tree), uncommitted edits (by content), the
//! toolchain and tool identity, the relevant environment, the normalized
//! argv and the working directory. The rule is to over-include: anything
//! the derivation cannot account for is a reason not to skip, never a
//! reason to guess.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

use super::profiles::{Profile, WorkspaceConfig};

/// Files that can change any Rust check wherever they sit in the repo.
const BASE_INPUTS: &[&str] = &[
    "**/*.rs",
    "**/Cargo.toml",
    "**/Cargo.lock",
    "**/.cargo/config",
    "**/.cargo/config.toml",
    "**/rust-toolchain",
    "**/rust-toolchain.toml",
];

/// Environment that can change any Rust check. A trailing `*` is a prefix.
const BASE_ENV: &[&str] = &[
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "RUSTUP_TOOLCHAIN",
    "RUSTC_BOOTSTRAP",
    "CARGO_BUILD_*",
    "CARGO_TARGET_*",
    "CARGO_PROFILE_*",
    "CARGO_UNSTABLE_*",
];

/// Prefix-matched variables that only move output or scheduling.
const ENV_NOISE: &[&str] = &[
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_JOBS",
    "CARGO_BUILD_TARGET_DIR",
    "CARGO_BUILD_INCREMENTAL",
];

/// A derived key and the inputs that produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derivation {
    /// `soldr-verdict-v1-<tool>-<scope>-<digest>`.
    pub key: String,
    pub tool: String,
    pub scope: String,
    pub toolchain: String,
    pub cwd: String,
    pub normalized_args: Vec<String>,
    pub files: usize,
    pub dirty: usize,
    /// Labelled component digests, for `soldr verdict explain`.
    pub components: BTreeMap<String, String>,
}

/// The git view of the checkout a key is derived from.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub root: PathBuf,
    /// Tracked path → blob id.
    pub tracked: BTreeMap<String, String>,
    /// Paths whose working-tree state differs from the index.
    pub dirty: BTreeMap<String, Dirty>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dirty {
    Modified,
    Untracked,
    Deleted,
}

/// Every probe the derivation runs is a short host query; a minute is far
/// beyond any healthy answer, and a probe that times out disables the skip.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

fn probe(
    command: &mut std::process::Command,
    context: &str,
) -> Result<std::process::Output, String> {
    crate::core::command_output_with_timeout_duration(command, context, PROBE_TIMEOUT)
        .map_err(|err| format!("{context} failed: {err}"))
}

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0");
    let output = probe(&mut command, "verdict git probe")?;
    if !output.status.success() {
        return Err(format!("`git {}` failed", args.join(" ")));
    }
    Ok(output.stdout)
}

impl Snapshot {
    /// Read the index and working-tree status of the repo containing `cwd`.
    pub fn discover(cwd: &Path) -> Result<Self, String> {
        let top = git(cwd, &["rev-parse", "--show-toplevel"])
            .map_err(|_| "not inside a git checkout".to_string())?;
        let root = PathBuf::from(String::from_utf8_lossy(&top).trim());
        let index = git(&root, &["ls-files", "-s", "-z"])?;
        let status = git(
            &root,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--no-renames",
            ],
        )?;
        Ok(Self {
            tracked: parse_ls_files(&index),
            dirty: parse_status(&status),
            root,
        })
    }
}

/// `git ls-files -s -z`: `<mode> <oid> <stage>\t<path>\0`.
pub(crate) fn parse_ls_files(raw: &[u8]) -> BTreeMap<String, String> {
    raw.split(|b| *b == 0)
        .filter_map(|entry| {
            let entry = std::str::from_utf8(entry).ok()?;
            let (meta, path) = entry.split_once('\t')?;
            let oid = meta.split(' ').nth(1)?;
            Some((path.to_string(), oid.to_string()))
        })
        .collect()
}

/// `git status --porcelain=v1 -z --no-renames`: `XY <path>\0`.
pub(crate) fn parse_status(raw: &[u8]) -> BTreeMap<String, Dirty> {
    raw.split(|b| *b == 0)
        .filter_map(|entry| {
            let entry = std::str::from_utf8(entry).ok()?;
            if entry.len() < 4 {
                return None;
            }
            let (xy, path) = entry.split_at(3);
            let state = match xy.trim_end() {
                "??" => Dirty::Untracked,
                code if code.contains('D') => Dirty::Deleted,
                _ => Dirty::Modified,
            };
            Some((path.to_string(), state))
        })
        .collect()
}

fn globset(patterns: impl IntoIterator<Item = impl AsRef<str>>) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let pattern = pattern.as_ref();
        builder.add(Glob::new(pattern).map_err(|err| format!("bad glob `{pattern}`: {err}"))?);
    }
    builder.build().map_err(|err| err.to_string())
}

/// Which repo paths feed a key.
struct Selection {
    package_roots: Vec<String>,
    include: GlobSet,
    exclude: GlobSet,
}

impl Selection {
    fn new(
        snapshot: &Snapshot,
        profile: &Profile,
        config: &WorkspaceConfig,
    ) -> Result<Self, String> {
        let include = globset(
            BASE_INPUTS
                .iter()
                .copied()
                .chain(profile.inputs.iter().map(String::as_str)),
        )?;
        Ok(Self {
            package_roots: package_roots(snapshot),
            include,
            exclude: globset(&config.exclude)?,
        })
    }

    fn selects(&self, path: &str) -> bool {
        if self.exclude.is_match(path) {
            return false;
        }
        self.include.is_match(path) || self.package_roots.iter().any(|root| under(path, root))
    }
}

fn under(path: &str, root: &str) -> bool {
    root.is_empty()
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Directories of every `Cargo.toml` that declares a `[package]`. Every
/// file under a package root is an input: build scripts, `include_str!`
/// targets and doc includes all live there.
fn package_roots(snapshot: &Snapshot) -> Vec<String> {
    let mut roots: Vec<String> = manifest_paths(snapshot)
        .filter(|path| {
            std::fs::read_to_string(snapshot.root.join(path))
                .ok()
                .and_then(|text| text.parse::<toml::Table>().ok())
                .is_some_and(|table| table.contains_key("package"))
        })
        .map(|path| path.rsplit_once('/').map_or("", |(dir, _)| dir).to_string())
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

fn manifest_paths(snapshot: &Snapshot) -> impl Iterator<Item = &String> {
    snapshot
        .tracked
        .keys()
        .chain(snapshot.dirty.keys())
        .filter(|path| path.as_str() == "Cargo.toml" || path.ends_with("/Cargo.toml"))
}

/// A `path = "..."` dependency or patch that leaves the repo makes the key
/// blind to that code, so it disables the skip.
fn escaping_path_dependency(snapshot: &Snapshot) -> Option<String> {
    let configs = snapshot
        .tracked
        .keys()
        .filter(|path| path.ends_with(".cargo/config") || path.ends_with(".cargo/config.toml"));
    for path in manifest_paths(snapshot).chain(configs) {
        let Ok(text) = std::fs::read_to_string(snapshot.root.join(path)) else {
            continue;
        };
        let Ok(table) = text.parse::<toml::Table>() else {
            return Some(format!("`{path}` does not parse"));
        };
        let dir = Path::new(path).parent().unwrap_or(Path::new(""));
        // `.cargo/config` paths resolve against the directory holding `.cargo`.
        let base = if dir.ends_with(".cargo") {
            dir.parent().unwrap_or(Path::new(""))
        } else {
            dir
        };
        let mut escapes = None;
        walk_paths(&toml::Value::Table(table), false, &mut |value| {
            if escapes.is_none() && leaves_repo(&base.join(value)) {
                escapes = Some(format!(
                    "`{path}` has path dependency `{value}` outside the repo"
                ));
            }
        });
        if escapes.is_some() {
            return escapes;
        }
    }
    None
}

fn walk_paths(value: &toml::Value, in_deps: bool, found: &mut dyn FnMut(&str)) {
    let toml::Value::Table(table) = value else {
        return;
    };
    for (key, child) in table {
        let deps = in_deps || key.ends_with("dependencies") || key == "patch" || key == "replace";
        if deps && key == "path" {
            if let toml::Value::String(text) = child {
                found(text);
            }
        }
        walk_paths(child, deps, found);
    }
}

/// Lexically normalize a repo-relative path and report whether it climbs
/// above the root (or is absolute).
fn leaves_repo(path: &Path) -> bool {
    let mut depth: i64 = 0;
    for component in path.components() {
        match component {
            Component::ParentDir => depth -= 1,
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return true,
        }
        if depth < 0 {
            return true;
        }
    }
    false
}

/// Drop flags that change only presentation or scheduling. Everything after
/// `--` is the tool's own argv and is kept verbatim.
pub fn normalize_args(args: &[String]) -> Vec<String> {
    const WITH_VALUE: &[&str] = &["--color", "-j", "--jobs", "--target-dir"];
    const FLAGS: &[&str] = &["-q", "--quiet", "-v", "-vv", "--verbose", "--timings"];
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            out.push(arg.clone());
            out.extend(iter.cloned());
            break;
        }
        if WITH_VALUE.contains(&arg.as_str()) {
            iter.next();
            continue;
        }
        let noisy_eq = WITH_VALUE.iter().any(|flag| {
            arg.strip_prefix(flag)
                .is_some_and(|rest| rest.starts_with('='))
        });
        let short_jobs =
            arg.len() > 2 && arg.starts_with("-j") && arg[2..].bytes().all(|b| b.is_ascii_digit());
        if noisy_eq || short_jobs || FLAGS.contains(&arg.as_str()) || arg.starts_with("--timings=")
        {
            continue;
        }
        out.push(arg.clone());
    }
    out
}

fn env_selected(name: &str, patterns: &[&str]) -> bool {
    patterns
        .iter()
        .any(|pattern| match pattern.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == *pattern,
        })
}

fn env_component(profile: &Profile) -> Vec<(String, String)> {
    let patterns: Vec<&str> = BASE_ENV
        .iter()
        .copied()
        .chain(profile.env.iter().map(String::as_str))
        .collect();
    let mut vars: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.to_string_lossy().into_owned())))
        .filter(|(k, _)| env_selected(k, &patterns) && !ENV_NOISE.contains(&k.as_str()))
        .collect();
    vars.sort();
    vars
}

/// `nightly-2026-05-28`, `1.89.0`, `1.89.0-x86_64-unknown-linux-gnu`.
pub(crate) fn is_exact_channel(channel: &str) -> bool {
    let version = channel.split('-').next().unwrap_or_default();
    if crate::system_toolchain_pin::is_exact_version_channel(version) {
        return true;
    }
    let dated = |rest: &str| {
        let date: Vec<&str> = rest.splitn(4, '-').take(3).collect();
        date.len() == 3
            && date[0].len() == 4
            && date
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    ["nightly-", "beta-"]
        .iter()
        .any(|prefix| channel.strip_prefix(prefix).is_some_and(dated))
}

fn toolchain_file_channel(root: &Path, cwd: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        for name in ["rust-toolchain.toml", "rust-toolchain"] {
            if let Ok(text) = std::fs::read_to_string(current.join(name)) {
                let parsed = text.parse::<toml::Table>().ok().and_then(|t| {
                    t.get("toolchain")?
                        .get("channel")?
                        .as_str()
                        .map(str::to_string)
                });
                return parsed.or_else(|| Some(text.trim().to_string()));
            }
        }
        if current == root {
            break;
        }
        dir = current.parent();
    }
    None
}

/// Toolchain identity: an exact pin is identity enough (it names one
/// immutable release); anything floating is resolved by asking rustc.
fn toolchain_identity(root: &Path, cwd: &Path, args: &[String]) -> Result<String, String> {
    let directive = args
        .iter()
        .take_while(|a| a.as_str() != "--")
        .find_map(|a| a.strip_prefix('+').map(str::to_string));
    let channel = directive
        .or_else(|| {
            std::env::var("RUSTUP_TOOLCHAIN")
                .ok()
                .filter(|v| !v.is_empty())
        })
        .or_else(|| toolchain_file_channel(root, cwd));
    if let Some(channel) = channel.as_deref().filter(|c| is_exact_channel(c)) {
        return Ok(format!("pin:{channel}"));
    }
    let rustc = crate::binaries::resolve_toolchain_binary("rustc")
        .map_err(|err| format!("cannot resolve rustc for a floating toolchain: {err}"))?;
    let mut command = std::process::Command::new(rustc);
    command.arg("-vV").current_dir(cwd);
    let output = probe(&mut command, "verdict rustc -vV")?;
    if !output.status.success() {
        return Err("rustc -vV failed".into());
    }
    Ok(format!(
        "rustc:{}",
        String::from_utf8_lossy(&output.stdout).trim()
    ))
}

/// Extra identity probes declared by the adapter.
fn identity_components(profile: &Profile, cwd: &Path) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for probe in &profile.identity {
        if probe == "toolchain" {
            continue;
        }
        if probe == "soldr" {
            out.push(("soldr".into(), env!("CARGO_PKG_VERSION").into()));
            continue;
        }
        out.push((probe.clone(), run_probe(probe, cwd)?));
    }
    Ok(out)
}

/// Run an identity probe. A probe starting with `cargo` goes through this
/// soldr's own front door so it resolves the same tool the check will.
fn run_probe(probe_line: &str, cwd: &Path) -> Result<String, String> {
    let words: Vec<&str> = probe_line.split_whitespace().collect();
    let (program, rest) = words
        .split_first()
        .ok_or_else(|| "empty identity probe".to_string())?;
    let mut command = if *program == "cargo" {
        let exe = std::env::current_exe().map_err(|err| err.to_string())?;
        let mut command = std::process::Command::new(exe);
        command.arg("cargo");
        command
    } else {
        std::process::Command::new(program)
    };
    command
        .args(rest)
        .current_dir(cwd)
        .env(super::MODE_ENV, "off")
        .stdin(std::process::Stdio::null());
    let output = probe(&mut command, "verdict identity probe")?;
    if !output.status.success() {
        return Err(format!("identity probe `{probe_line}` failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// User-level cargo configuration also feeds every build.
fn cargo_home_config() -> String {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| crate::core::home_dir().ok().map(|h| h.join(".cargo")));
    let Some(home) = home else {
        return "none".into();
    };
    let mut hasher = blake3::Hasher::new();
    for name in ["config", "config.toml"] {
        if let Ok(bytes) = std::fs::read(home.join(name)) {
            hasher.update(name.as_bytes());
            hasher.update(&bytes);
        }
    }
    hasher.finalize().to_hex()[..32].to_string()
}

fn cwd_relative(root: &Path, cwd: &Path) -> Result<String, String> {
    let rel = cwd
        .strip_prefix(root)
        .map_err(|_| "working directory is outside the checkout".to_string())?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Ok(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}

/// Everything one derivation needs.
pub struct KeyRequest<'a> {
    pub profile: &'a Profile,
    pub config: &'a WorkspaceConfig,
    pub args: &'a [String],
    pub cwd: &'a Path,
}

/// One selected input: repo-relative path and its fingerprint (the blob id
/// for a clean tracked file, a content hash for an uncommitted one).
type Entry = (String, String);

fn input_entries(
    snapshot: &Snapshot,
    selection: &Selection,
) -> Result<(Vec<Entry>, usize), String> {
    let mut entries = Vec::new();
    for (path, oid) in &snapshot.tracked {
        if selection.selects(path) && !snapshot.dirty.contains_key(path) {
            entries.push((path.clone(), oid.clone()));
        }
    }
    let mut dirty_count = 0usize;
    for (path, state) in &snapshot.dirty {
        if !selection.selects(path) {
            continue;
        }
        let fingerprint = match state {
            Dirty::Deleted => "deleted".to_string(),
            Dirty::Modified | Dirty::Untracked => {
                let full = snapshot.root.join(path);
                if full.is_dir() {
                    return Err(format!("untracked directory `{path}` (nested repo?)"));
                }
                let bytes = std::fs::read(&full)
                    .map_err(|err| format!("cannot read uncommitted `{path}`: {err}"))?;
                format!("dirty:{}", blake3::hash(&bytes).to_hex())
            }
        };
        entries.push((path.clone(), fingerprint));
        dirty_count += 1;
    }
    entries.sort();
    Ok((entries, dirty_count))
}

fn short(hash: blake3::Hash) -> String {
    hash.to_hex()[..32].to_string()
}

fn digest_entries<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> String {
    let mut hasher = blake3::Hasher::new();
    for (path, fingerprint) in entries {
        hasher
            .update(path.as_bytes())
            .update(b"\0")
            .update(fingerprint.as_bytes())
            .update(b"\n");
    }
    short(hasher.finalize())
}

/// Everything but the files: toolchain, host, config, cwd, argv, env,
/// identity probes and the adapter row.
struct Common {
    components: BTreeMap<String, String>,
    toolchain: String,
    normalized_args: Vec<String>,
    cwd: String,
    entries: Vec<Entry>,
    dirty: usize,
}

fn common(
    request: &KeyRequest<'_>,
    snapshot: &Snapshot,
    args: &[String],
) -> Result<Common, String> {
    if let Some(reason) = escaping_path_dependency(snapshot) {
        return Err(reason);
    }
    let cwd = request
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| request.cwd.to_path_buf());
    let root = snapshot
        .root
        .canonicalize()
        .unwrap_or_else(|_| snapshot.root.clone());
    let selection = Selection::new(snapshot, request.profile, request.config)?;
    let (entries, dirty) = input_entries(snapshot, &selection)?;
    let toolchain = toolchain_identity(&root, &cwd, request.args)?;
    let normalized_args = normalize_args(args);
    let cwd_rel = cwd_relative(&root, &cwd)?;
    let mut components = BTreeMap::new();
    components.insert("toolchain".into(), toolchain.clone());
    components.insert("host".into(), crate::pyo3_detect::host_triple().to_string());
    components.insert("cargo-home-config".into(), cargo_home_config());
    components.insert("cwd".into(), cwd_rel.clone());
    components.insert(
        "args".into(),
        serde_json::to_string(&normalized_args).unwrap_or_default(),
    );
    for (name, value) in env_component(request.profile) {
        components.insert(format!("env:{name}"), value);
    }
    for (name, value) in identity_components(request.profile, &cwd)? {
        components.insert(format!("identity:{name}"), value);
    }
    let profile_json = serde_json::to_string(request.profile).unwrap_or_default();
    components.insert(
        "profile".into(),
        short(blake3::hash(profile_json.as_bytes())),
    );
    Ok(Common {
        components,
        toolchain,
        normalized_args,
        cwd: cwd_rel,
        entries,
        dirty,
    })
}

fn key_id(tool: &str, scope: &str, digest: &str) -> String {
    format!(
        "soldr-verdict-v{}-{tool}-{scope}-{digest}",
        super::record::SCHEMA
    )
}

/// Derive the key, or the reason this invocation must run.
pub fn derive(request: &KeyRequest<'_>, snapshot: &Snapshot) -> Result<Derivation, String> {
    let mut common = common(request, snapshot, request.args)?;
    common
        .components
        .insert("files".into(), digest_entries(&common.entries));
    let scope = "ws".to_string();
    let digest = digest_components(&request.profile.name, &scope, &common.components);
    Ok(Derivation {
        key: key_id(&request.profile.name, &scope, &digest),
        tool: request.profile.name.clone(),
        scope,
        toolchain: common.toolchain,
        cwd: common.cwd,
        normalized_args: common.normalized_args,
        files: common.entries.len() - common.dirty,
        dirty: common.dirty,
        components: common.components,
    })
}

/// Per-member keys for a workspace-wide invocation (soldr#3897). The
/// workspace-selecting flags are dropped from the argv component so a later
/// `-p name` run of the same check derives the same key for `name`.
pub fn derive_crates(
    request: &KeyRequest<'_>,
    snapshot: &Snapshot,
    members: &[super::crates::Member],
) -> Result<BTreeMap<String, String>, String> {
    let args: Vec<String> = request
        .args
        .iter()
        .take_while(|a| a.as_str() != "--")
        .filter(|a| !matches!(a.as_str(), "--workspace" | "--all"))
        .chain(request.args.iter().skip_while(|a| a.as_str() != "--"))
        .cloned()
        .collect();
    let common = common(request, snapshot, &args)?;
    let mut owned: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    let mut shared = Vec::new();
    for entry in &common.entries {
        match super::crates::owner(members, &entry.0) {
            Some(member) => owned.entry(member.name.clone()).or_default().push(entry),
            None => shared.push(entry),
        }
    }
    let mut base = common.components;
    base.insert("shared-files".into(), digest_entries(shared));
    let own: BTreeMap<String, String> = members
        .iter()
        .map(|m| {
            let entries = owned.get(&m.name).map(Vec::as_slice).unwrap_or(&[]);
            (m.name.clone(), digest_entries(entries.iter().copied()))
        })
        .collect();
    let tool = request.profile.name.clone();
    let digests = super::crates::merkle(members, &own, |name, own_digest, dep_keys| {
        let mut components = base.clone();
        components.insert("own-files".into(), own_digest.to_string());
        components.insert("deps".into(), dep_keys.join(","));
        digest_components(&tool, &format!("c-{name}"), &components)
    })?;
    Ok(digests
        .into_iter()
        .map(|(name, digest)| {
            let id = key_id(&tool, &format!("c-{name}"), &digest);
            (name, id)
        })
        .collect())
}

pub(crate) fn digest_components(
    tool: &str,
    scope: &str,
    components: &BTreeMap<String, String>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(
        format!(
            "schema\0{}\ntool\0{tool}\nscope\0{scope}\n",
            super::record::SCHEMA
        )
        .as_bytes(),
    );
    for (label, value) in components {
        hasher
            .update(label.as_bytes())
            .update(b"\0")
            .update(value.as_bytes())
            .update(b"\n");
    }
    hasher.finalize().to_hex()[..32].to_string()
}
