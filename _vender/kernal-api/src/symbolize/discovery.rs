//! Deterministic, identity-gated symbol discovery (#638).
//!
//! The order is part of the contract:
//!
//! 1. an extracted embedded section supplied with the capture;
//! 2. a manifest adjacent to the native module;
//! 3. paths or a manifest declared during registration;
//! 4. native symbols adjacent to the module;
//! 5. a build-id cache;
//! 6. configured local symbol stores.
//!
//! A path or matching filename is never sufficient. Every candidate must
//! carry the module's expected identity (when it came from a manifest) and its
//! own bytes must independently verify as the same build before it can win.
//! All parsing stays in this short-lived worker process.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use prost::Message as _;

use kernal_api::symbolize::wire::{DiscoveryConfig, DiscoverySource, ModuleRef};

/// Environment variable containing build-id cache roots.
pub const BUILD_ID_CACHE_ENV: &str = "KERNAL_API_BUILD_ID_CACHE";
/// Admin-only, comma-separated HTTP(S) symbol-server base URLs.
pub const SYMBOL_SERVERS_ENV: &str = "KERNAL_API_SYMBOL_SERVERS";
/// Platform path-list of additional local symbol-store roots.
pub const SYMBOL_PATH_ENV: &str = "KERNAL_API_SYMBOL_PATH";
const MANIFEST_SCHEMA: &str = "kernal-api-symbol-manifest/v1";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_MANIFEST_MODULES: usize = 1024;
const MAX_ARTIFACTS_PER_MODULE: usize = 32;
pub(crate) const MAX_SYMBOL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SERVER_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SERVER_AGGREGATE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SYMBOL_SERVERS: usize = 8;
const MAX_CAPTURE_IDENTITY_BYTES: u64 = 512 * 1024 * 1024;

/// A process-level symbol manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolManifest {
    /// Stable schema identifier.
    pub schema: String,
    /// Modules described by this manifest.
    pub modules: Vec<ManifestModule>,
}

/// One exact-build entry in a [`SymbolManifest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestModule {
    /// Optional display-only module name. It is never used for selection.
    pub name: Option<String>,
    /// Exact typed build identity used to select this entry.
    pub identity: SymbolIdentity,
    /// Candidate artifacts for this exact build.
    pub artifacts: Vec<ManifestArtifact>,
}

/// Typed module/symbol identity.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum SymbolIdentity {
    /// PE CodeView / PDB GUID plus age.
    PePdb {
        /// RFC-4122 byte-order GUID as 32 hex digits.
        guid: String,
        /// PDB age.
        age: u32,
    },
    /// GNU ELF build-id.
    ElfBuildId {
        /// Lowercase hexadecimal build-id.
        hex: String,
    },
    /// Mach-O `LC_UUID`.
    MachoUuid {
        /// Lowercase hexadecimal UUID.
        hex: String,
    },
}

impl SymbolIdentity {
    fn parse(value: &str) -> Option<Self> {
        if let Some(value) = value.strip_prefix("pdb:") {
            let (guid, age) = value.rsplit_once('-')?;
            return Self::PePdb {
                guid: guid.to_owned(),
                age: age.parse().ok()?,
            }
            .normalized();
        }
        if let Some(hex) = value.strip_prefix("elf:") {
            return Self::ElfBuildId { hex: hex.into() }.normalized();
        }
        value
            .strip_prefix("macho:")
            .and_then(|hex| Self::MachoUuid { hex: hex.into() }.normalized())
    }

    fn normalized(&self) -> Option<Self> {
        match self {
            Self::PePdb { guid, age } => Some(Self::PePdb {
                guid: normalized_hex(guid, Some(32), true)?,
                age: *age,
            }),
            Self::ElfBuildId { hex } => Some(Self::ElfBuildId {
                hex: normalized_hex(hex, None, false)?,
            }),
            Self::MachoUuid { hex } => Some(Self::MachoUuid {
                hex: normalized_hex(hex, Some(32), true)?,
            }),
        }
    }
}

fn normalized_hex(value: &str, exact_len: Option<usize>, allow_hyphens: bool) -> Option<String> {
    let compact = if allow_hyphens {
        value.replace('-', "")
    } else {
        value.to_owned()
    };
    if compact.is_empty()
        || compact.len() % 2 != 0
        || exact_len.is_some_and(|length| compact.len() != length)
        || !compact.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(compact.to_ascii_lowercase())
}

/// One symbol artifact in a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestArtifact {
    /// Parser format.
    pub format: SymbolArtifactFormat,
    /// Where the artifact resides.
    pub storage: ArtifactStorage,
    /// Optional SHA-256 of the artifact bytes.
    pub sha256: Option<String>,
    /// Optional exact byte length.
    pub expected_size: Option<u64>,
}

/// Symbol artifact parser format.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum SymbolArtifactFormat {
    /// Microsoft PDB.
    Pdb,
    /// ELF/DWARF object or split-debug file.
    ElfDwarf,
    /// Mach-O dSYM object.
    MachoDsym,
}

/// Manifest storage declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactStorage {
    /// A path relative to the manifest directory.
    RelativePath {
        /// Traversal-free relative path.
        path: PathBuf,
    },
}

#[cfg(test)]
fn encode_manifest(manifest: &SymbolManifest) -> Vec<u8> {
    manifest_proto::Manifest::from(manifest).encode_to_vec()
}

fn decode_manifest(bytes: &[u8]) -> Option<SymbolManifest> {
    manifest_proto::Manifest::decode(bytes)
        .ok()?
        .try_into()
        .ok()
}

mod manifest_proto {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Manifest {
        #[prost(string, tag = "1")]
        pub schema: String,
        #[prost(message, repeated, tag = "2")]
        pub modules: Vec<Module>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Module {
        #[prost(string, optional, tag = "1")]
        pub name: Option<String>,
        #[prost(message, optional, tag = "2")]
        pub identity: Option<Identity>,
        #[prost(message, repeated, tag = "3")]
        pub artifacts: Vec<Artifact>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Identity {
        #[prost(oneof = "identity::Kind", tags = "1, 2, 3")]
        pub kind: Option<identity::Kind>,
    }

    pub mod identity {
        #[derive(Clone, PartialEq, prost::Oneof)]
        pub enum Kind {
            #[prost(message, tag = "1")]
            PePdb(super::PePdb),
            #[prost(string, tag = "2")]
            ElfBuildId(String),
            #[prost(string, tag = "3")]
            MachoUuid(String),
        }
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct PePdb {
        #[prost(string, tag = "1")]
        pub guid: String,
        #[prost(uint32, tag = "2")]
        pub age: u32,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Artifact {
        #[prost(int32, tag = "1")]
        pub format: i32,
        #[prost(string, tag = "2")]
        pub relative_path: String,
        #[prost(string, optional, tag = "3")]
        pub sha256: Option<String>,
        #[prost(uint64, optional, tag = "4")]
        pub expected_size: Option<u64>,
    }
}

impl From<&SymbolManifest> for manifest_proto::Manifest {
    fn from(value: &SymbolManifest) -> Self {
        Self {
            schema: value.schema.clone(),
            modules: value.modules.iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<manifest_proto::Manifest> for SymbolManifest {
    type Error = ();

    fn try_from(value: manifest_proto::Manifest) -> Result<Self, Self::Error> {
        Ok(Self {
            schema: value.schema,
            modules: value
                .modules
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl From<&ManifestModule> for manifest_proto::Module {
    fn from(value: &ManifestModule) -> Self {
        Self {
            name: value.name.clone(),
            identity: Some((&value.identity).into()),
            artifacts: value.artifacts.iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<manifest_proto::Module> for ManifestModule {
    type Error = ();

    fn try_from(value: manifest_proto::Module) -> Result<Self, Self::Error> {
        Ok(Self {
            name: value.name,
            identity: value.identity.ok_or(())?.try_into()?,
            artifacts: value
                .artifacts
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl From<&SymbolIdentity> for manifest_proto::Identity {
    fn from(value: &SymbolIdentity) -> Self {
        use manifest_proto::identity::Kind;
        let kind = match value {
            SymbolIdentity::PePdb { guid, age } => Kind::PePdb(manifest_proto::PePdb {
                guid: guid.clone(),
                age: *age,
            }),
            SymbolIdentity::ElfBuildId { hex } => Kind::ElfBuildId(hex.clone()),
            SymbolIdentity::MachoUuid { hex } => Kind::MachoUuid(hex.clone()),
        };
        Self { kind: Some(kind) }
    }
}

impl TryFrom<manifest_proto::Identity> for SymbolIdentity {
    type Error = ();

    fn try_from(value: manifest_proto::Identity) -> Result<Self, Self::Error> {
        use manifest_proto::identity::Kind;
        match value.kind.ok_or(())? {
            Kind::PePdb(value) => Ok(Self::PePdb {
                guid: value.guid,
                age: value.age,
            }),
            Kind::ElfBuildId(hex) => Ok(Self::ElfBuildId { hex }),
            Kind::MachoUuid(hex) => Ok(Self::MachoUuid { hex }),
        }
    }
}

impl From<&ManifestArtifact> for manifest_proto::Artifact {
    fn from(value: &ManifestArtifact) -> Self {
        let ArtifactStorage::RelativePath { path } = &value.storage;
        Self {
            format: match value.format {
                SymbolArtifactFormat::Pdb => 0,
                SymbolArtifactFormat::ElfDwarf => 1,
                SymbolArtifactFormat::MachoDsym => 2,
            },
            relative_path: path.to_string_lossy().into_owned(),
            sha256: value.sha256.clone(),
            expected_size: value.expected_size,
        }
    }
}

impl TryFrom<manifest_proto::Artifact> for ManifestArtifact {
    type Error = ();

    fn try_from(value: manifest_proto::Artifact) -> Result<Self, Self::Error> {
        let format = match value.format {
            0 => SymbolArtifactFormat::Pdb,
            1 => SymbolArtifactFormat::ElfDwarf,
            2 => SymbolArtifactFormat::MachoDsym,
            _ => return Err(()),
        };
        Ok(Self {
            format,
            storage: ArtifactStorage::RelativePath {
                path: PathBuf::from(value.relative_path),
            },
            sha256: value.sha256,
            expected_size: value.expected_size,
        })
    }
}

/// A verified symbol source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSymbolSource {
    /// Discovery tier that won.
    pub source: DiscoverySource,
    /// Verified symbol-file path.
    pub path: PathBuf,
}

/// Outcome of walking every discovery tier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// The first exact-identity candidate.
    Found(ResolvedSymbolSource),
    /// No candidate existed.
    NotFound,
    /// Candidates existed, but every one failed identity verification.
    Mismatched {
        /// Number of rejected candidates.
        rejected: usize,
    },
}

/// Outcome of sequential, bounded configured-server discovery.
pub enum ServerResolve<T> {
    /// First response whose bytes verified and parsed.
    Found {
        /// Source URL for reporting.
        url: String,
        /// Caller-produced parsed value.
        value: T,
        /// The verified download, kept on disk (#818).
        ///
        /// Symbol *names* are parsed during verification and need nothing
        /// further. Line programs are read later, in a pre-pass over the
        /// addresses a capture actually contains, and that needs the file to
        /// still exist. Dropping this removes it, so it is held for as long as
        /// the symbols it describes.
        ///
        /// The worker is one-shot and handles a single capture, so the
        /// lifetime is bounded by the process — no cache, no eviction, no
        /// cleanup path to get wrong.
        retained: tempfile::TempPath,
    },
    /// No origin yielded a candidate response.
    NotFound,
    /// Candidate responses arrived but did not verify and parse.
    Mismatched {
        /// Number of rejected response bodies.
        rejected: usize,
    },
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Candidate {
    source: DiscoverySource,
    path: PathBuf,
    claimed_identity: Option<SymbolIdentity>,
    format: Option<SymbolArtifactFormat>,
    sha256: Option<String>,
    expected_size: Option<u64>,
}

/// Discover a symbol file using the documented precedence.
///
/// `symbol_file_name` is the native sidecar name (for example `app.pdb`).
/// `verify` must compute identity from the candidate's own bytes and return
/// true only for the expected build. Keeping this callback format-neutral lets
/// the walker cover PDB, ELF, and Mach-O without moving parsers into the daemon.
pub fn resolve_symbols(
    module: &ModuleRef,
    config: &DiscoveryConfig,
    expected_debug_id: &str,
    expected_format: SymbolArtifactFormat,
    symbol_file_name: &Path,
    configured_stores: &[PathBuf],
    mut verify: impl FnMut(&Path) -> bool,
) -> ResolveOutcome {
    let Some(expected_identity) = SymbolIdentity::parse(expected_debug_id) else {
        return ResolveOutcome::NotFound;
    };
    let candidates = candidates(
        module,
        config,
        expected_debug_id,
        symbol_file_name,
        configured_stores,
    );
    let mut seen = HashSet::new();
    let mut rejected = 0usize;

    for candidate in candidates {
        if !seen.insert(candidate.clone()) || !candidate.path.is_file() {
            continue;
        }
        let claim_matches = candidate
            .claimed_identity
            .as_ref()
            .is_none_or(|claimed| claimed.normalized().as_ref() == Some(&expected_identity));
        let format_matches = candidate
            .format
            .is_none_or(|format| format == expected_format);
        let snapshot = (claim_matches && format_matches)
            .then(|| snapshot_candidate(&candidate))
            .flatten();
        if let Some(snapshot) = snapshot.filter(|snapshot| verify(snapshot.path())) {
            // Keep the private snapshot alive until verification and parsing
            // have both completed. Reports still name the declared source.
            drop(snapshot);
            return ResolveOutcome::Found(ResolvedSymbolSource {
                source: candidate.source,
                path: candidate.path,
            });
        }
        rejected += 1;
    }

    if rejected == 0 {
        ResolveOutcome::NotFound
    } else {
        ResolveOutcome::Mismatched { rejected }
    }
}

fn candidates(
    module: &ModuleRef,
    config: &DiscoveryConfig,
    expected_debug_id: &str,
    symbol_file_name: &Path,
    configured_stores: &[PathBuf],
) -> Vec<Candidate> {
    let mut out = Vec::new();

    if let Some(path) = module.embedded_symbol_path.as_deref() {
        out.push(Candidate {
            source: DiscoverySource::Embedded,
            path: PathBuf::from(path),
            claimed_identity: None,
            format: None,
            sha256: None,
            expected_size: None,
        });
    }

    if let Some(image) = module.path_hint.as_deref().map(Path::new) {
        for manifest in adjacent_manifest_paths(image) {
            extend_manifest(
                &mut out,
                &manifest,
                module,
                DiscoverySource::AdjacentManifest,
            );
        }
    }

    if let Some(manifest) = config.registered_manifest.as_deref().map(Path::new) {
        extend_manifest(&mut out, manifest, module, DiscoverySource::Registration);
    }
    for path in &config.registered_symbol_paths {
        out.push(Candidate {
            source: DiscoverySource::Registration,
            path: candidate_from_declared_path(Path::new(path), symbol_file_name),
            claimed_identity: None,
            format: None,
            sha256: None,
            expected_size: None,
        });
    }

    if let Some(image) = module.path_hint.as_deref().map(Path::new) {
        if let Some(parent) = image.parent() {
            out.push(Candidate {
                source: DiscoverySource::AdjacentNative,
                path: parent.join(symbol_file_name),
                claimed_identity: None,
                format: None,
                sha256: None,
                expected_size: None,
            });
        }
    }

    for root in cache_roots() {
        out.push(Candidate {
            source: DiscoverySource::BuildIdCache,
            path: cache_candidate(&root, expected_debug_id, symbol_file_name),
            claimed_identity: None,
            format: None,
            sha256: None,
            expected_size: None,
        });
    }

    for root in configured_stores {
        out.push(Candidate {
            source: DiscoverySource::ConfiguredStore,
            path: root.join(symbol_file_name),
            claimed_identity: None,
            format: None,
            sha256: None,
            expected_size: None,
        });
    }

    out
}

fn adjacent_manifest_paths(image: &Path) -> [PathBuf; 2] {
    let appended = PathBuf::from(format!("{}.kernal-symbols.pb", image.to_string_lossy()));
    let replaced = image.with_extension("kernal-symbols.pb");
    [appended, replaced]
}

fn cache_candidate(root: &Path, identity: &str, symbol_file_name: &Path) -> PathBuf {
    if let Some(build_id) = identity.strip_prefix("elf:") {
        let split = build_id.len().min(2);
        return root
            .join(".build-id")
            .join(&build_id[..split])
            .join(format!("{}.debug", &build_id[split..]));
    }
    if let Some(debug_id) = identity.strip_prefix("pdb:") {
        let key = pdb_guid_age_key(debug_id).unwrap_or_else(|| debug_id.replace('-', ""));
        return root.join(symbol_file_name).join(key).join(symbol_file_name);
    }
    if let Some(uuid) = identity.strip_prefix("macho:") {
        return root.join(uuid).join(symbol_file_name);
    }
    root.join(identity).join(symbol_file_name)
}

fn pdb_guid_age_key(debug_id: &str) -> Option<String> {
    let (guid, age) = debug_id.rsplit_once('-')?;
    let age: u32 = age.parse().ok()?;
    Some(format!(
        "{}{age:x}",
        guid.replace('-', "").to_ascii_uppercase()
    ))
}

fn cache_roots() -> Vec<PathBuf> {
    let mut roots = parse_path_list(std::env::var_os(BUILD_ID_CACHE_ENV));
    if roots.is_empty() {
        roots.push(default_cache_root());
    }
    roots
}

fn default_cache_root() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map_or_else(
            || PathBuf::from(r"C:\ProgramData\kernal-api\symbol-cache"),
            |base| PathBuf::from(base).join("kernal-api").join("symbol-cache"),
        )
    }
    #[cfg(unix)]
    {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local").join("state"))
            })
            .map_or_else(
                || PathBuf::from("/tmp/kernal-api-state/symbol-cache"),
                |base| base.join("kernal-api").join("symbol-cache"),
            )
    }
}

fn extend_manifest(
    out: &mut Vec<Candidate>,
    manifest_path: &Path,
    _module: &ModuleRef,
    source: DiscoverySource,
) {
    use std::io::Read as _;

    let Ok(file) = std::fs::File::open(manifest_path) else {
        return;
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_MANIFEST_BYTES
    {
        return;
    }
    let Some(manifest) = decode_manifest(&bytes) else {
        return;
    };
    if manifest.schema != MANIFEST_SCHEMA || manifest.modules.len() > MAX_MANIFEST_MODULES {
        return;
    }
    let Ok(parent) = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .canonicalize()
    else {
        return;
    };
    for entry in manifest.modules {
        if entry.artifacts.len() > MAX_ARTIFACTS_PER_MODULE {
            continue;
        }
        for artifact in entry.artifacts {
            let ArtifactStorage::RelativePath { path } = artifact.storage;
            if path.is_absolute()
                || path.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                continue;
            }
            let joined = parent.join(path);
            let Ok(resolved) = joined.canonicalize() else {
                continue;
            };
            if !resolved.starts_with(&parent) {
                continue;
            }
            out.push(Candidate {
                source,
                path: resolved,
                claimed_identity: Some(entry.identity.clone()),
                format: Some(artifact.format),
                sha256: artifact.sha256,
                expected_size: artifact.expected_size,
            });
        }
    }
}

fn candidate_from_declared_path(path: &Path, symbol_file_name: &Path) -> PathBuf {
    if path.is_dir() {
        path.join(symbol_file_name)
    } else {
        path.to_path_buf()
    }
}

fn parse_path_list(raw: Option<std::ffi::OsString>) -> Vec<PathBuf> {
    raw.map_or_else(Vec::new, |value| {
        std::env::split_paths(&value)
            .filter(|path| !path.as_os_str().is_empty())
            .collect()
    })
}

fn snapshot_candidate(candidate: &Candidate) -> Option<tempfile::NamedTempFile> {
    use sha2::{Digest as _, Sha256};
    use std::io::{Read as _, Write as _};

    let mut source = std::fs::File::open(&candidate.path).ok()?;
    let mut snapshot = tempfile::NamedTempFile::new().ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = source.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        total = total.checked_add(read as u64)?;
        if total > MAX_SYMBOL_BYTES {
            return None;
        }
        hasher.update(&buffer[..read]);
        snapshot.write_all(&buffer[..read]).ok()?;
    }
    if candidate
        .expected_size
        .is_some_and(|expected| total != expected)
    {
        return None;
    }
    let actual = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if candidate
        .sha256
        .as_deref()
        .is_some_and(|expected| !actual.eq_ignore_ascii_case(expected))
    {
        return None;
    }
    snapshot.flush().ok()?;
    Some(snapshot)
}

/// Verify that the on-disk image still has the SHA-256 captured while the
/// module was loaded.
///
/// Symbol format identities are extracted only after this passes. If the path
/// was replaced between capture and worker execution, symbolization degrades
/// rather than deriving an identity from a different build.
pub fn captured_image_still_matches(module: &ModuleRef) -> bool {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;

    let Some(expected) = module
        .code_id
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
    else {
        return true;
    };
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }
    let Some(path) = module.path_hint.as_deref() else {
        return false;
    };
    if std::fs::metadata(path)
        .map(|metadata| metadata.len() > MAX_CAPTURE_IDENTITY_BYTES)
        .unwrap_or(true)
    {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let Ok(read) = file.read(&mut buffer) else {
            return false;
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    actual.eq_ignore_ascii_case(expected)
}

/// Resolve opt-in server candidates sequentially using typed platform keys.
///
/// The environment is inherited from the daemon process; target-provided
/// capture JSON cannot add a server. Redirects are disabled so an approved
/// HTTPS origin cannot silently bounce to another scheme or host. Each
/// response is bounded before it reaches a parser, and callers must still
/// verify the downloaded bytes' exact module identity.
pub fn resolve_configured_server<T>(
    identity: &str,
    symbol_file_name: &Path,
    verify: impl FnMut(&Path) -> Option<T>,
) -> ServerResolve<T> {
    let servers = std::env::var(SYMBOL_SERVERS_ENV)
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    resolve_from_servers(&servers, identity, symbol_file_name, verify)
}

fn resolve_from_servers<T>(
    servers: &[String],
    identity: &str,
    symbol_file_name: &Path,
    mut verify: impl FnMut(&Path) -> Option<T>,
) -> ServerResolve<T> {
    use std::io::{Read as _, Write as _};
    use std::time::Duration;

    let key = server_key(identity, symbol_file_name);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout_read(Duration::from_secs(10))
        .timeout_write(Duration::from_secs(2))
        .redirects(0)
        .build();
    let mut aggregate_bytes = 0u64;
    let mut rejected = 0usize;

    for base in servers.iter().take(MAX_SYMBOL_SERVERS) {
        let remaining = MAX_SERVER_AGGREGATE_BYTES.saturating_sub(aggregate_bytes);
        if remaining == 0 {
            break;
        }
        let response_limit = MAX_SERVER_RESPONSE_BYTES.min(remaining);
        let Ok(mut url) = url::Url::parse(base) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            continue;
        }
        let Ok(mut segments) = url.path_segments_mut() else {
            continue;
        };
        segments.pop_if_empty();
        for segment in &key {
            segments.push(segment);
        }
        drop(segments);

        let Ok(response) = agent.get(url.as_str()).call() else {
            continue;
        };
        if !(200..300).contains(&response.status()) {
            continue;
        }
        let mut reader = response.into_reader().take(response_limit + 1);
        let Ok(mut file) = tempfile::NamedTempFile::new() else {
            continue;
        };
        let Ok(copied) = std::io::copy(&mut reader, &mut file) else {
            continue;
        };
        aggregate_bytes = aggregate_bytes.saturating_add(copied);
        if copied > response_limit {
            rejected += 1;
            if aggregate_bytes >= MAX_SERVER_AGGREGATE_BYTES {
                break;
            }
            continue;
        }
        if file.flush().is_err() {
            continue;
        }
        if let Some(value) = verify(file.path()) {
            // `into_temp_path` transfers deletion to the returned handle, so
            // the file survives this function instead of being unlinked when
            // `file` drops.
            return ServerResolve::Found {
                url: url.into(),
                value,
                retained: file.into_temp_path(),
            };
        }
        rejected += 1;
    }
    if rejected == 0 {
        ServerResolve::NotFound
    } else {
        ServerResolve::Mismatched { rejected }
    }
}

fn server_key(identity: &str, symbol_file_name: &Path) -> Vec<String> {
    let name = symbol_file_name.to_string_lossy().into_owned();
    if let Some(build_id) = identity.strip_prefix("elf:") {
        return vec!["buildid".into(), build_id.into(), "debuginfo".into()];
    }
    if let Some(debug_id) = identity.strip_prefix("pdb:") {
        let key = pdb_guid_age_key(debug_id).unwrap_or_else(|| debug_id.replace('-', ""));
        return vec![name.clone(), key, name];
    }
    if let Some(uuid) = identity.strip_prefix("macho:") {
        let mut key = vec![uuid.into()];
        key.extend(
            symbol_file_name
                .components()
                .filter_map(|component| match component {
                    std::path::Component::Normal(segment) => {
                        Some(segment.to_string_lossy().into_owned())
                    }
                    _ => None,
                }),
        );
        return key;
    }
    vec![identity.into(), name]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use tempfile::TempDir;

    fn module(image: &Path) -> ModuleRef {
        ModuleRef {
            name: "app.exe".into(),
            path_hint: Some(image.to_string_lossy().into_owned()),
            debug_id: Some("BUILD-1".into()),
            ..Default::default()
        }
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"symbols").unwrap();
    }

    fn write_manifest_with(
        path: &Path,
        symbol_path: &str,
        debug_id: &str,
        format: SymbolArtifactFormat,
        sha256: Option<String>,
        expected_size: Option<u64>,
    ) {
        let manifest = SymbolManifest {
            schema: MANIFEST_SCHEMA.into(),
            modules: vec![ManifestModule {
                name: Some("app.exe".into()),
                identity: SymbolIdentity::parse(debug_id).unwrap(),
                artifacts: vec![ManifestArtifact {
                    format,
                    storage: ArtifactStorage::RelativePath {
                        path: symbol_path.into(),
                    },
                    sha256,
                    expected_size,
                }],
            }],
        };
        std::fs::write(path, encode_manifest(&manifest)).unwrap();
    }

    fn write_manifest(path: &Path, symbol_path: &str, debug_id: &str) {
        write_manifest_with(
            path,
            symbol_path,
            debug_id,
            SymbolArtifactFormat::ElfDwarf,
            None,
            None,
        );
    }

    fn serve_once(
        status: &str,
        body: Vec<u8>,
        extra_headers: &str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let extra_headers = extra_headers.to_string();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]).into_owned();
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
            request
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn embedded_precedes_adjacent_manifest_and_native() {
        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        touch(&image);
        let embedded = temp.path().join("embedded.pdb");
        let manifest_symbols = temp.path().join("manifest.pdb");
        let adjacent = temp.path().join("app.pdb");
        for path in [&embedded, &manifest_symbols, &adjacent] {
            touch(path);
        }
        write_manifest(
            &PathBuf::from(format!("{}.kernal-symbols.pb", image.display())),
            "manifest.pdb",
            "elf:abcd",
        );
        let mut input = module(&image);
        input.embedded_symbol_path = Some(embedded.to_string_lossy().into_owned());

        let outcome = resolve_symbols(
            &input,
            &DiscoveryConfig::default(),
            "elf:abcd",
            SymbolArtifactFormat::ElfDwarf,
            Path::new("app.pdb"),
            &[],
            |_| true,
        );
        assert_eq!(
            outcome,
            ResolveOutcome::Found(ResolvedSymbolSource {
                source: DiscoverySource::Embedded,
                path: embedded,
            })
        );
    }

    #[test]
    fn adjacent_manifest_precedes_registration_and_adjacent_native() {
        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        touch(&image);
        let manifest_symbols = temp.path().join("manifest.pdb");
        let registered = temp.path().join("registered.pdb");
        let adjacent = temp.path().join("app.pdb");
        for path in [&manifest_symbols, &registered, &adjacent] {
            touch(path);
        }
        write_manifest(
            &PathBuf::from(format!("{}.kernal-symbols.pb", image.display())),
            "manifest.pdb",
            "elf:abcd",
        );
        let input = module(&image);
        let config = DiscoveryConfig {
            registered_symbol_paths: vec![registered.to_string_lossy().into_owned()],
            ..Default::default()
        };

        let outcome = resolve_symbols(
            &input,
            &config,
            "elf:abcd",
            SymbolArtifactFormat::ElfDwarf,
            Path::new("app.pdb"),
            &[],
            |_| true,
        );
        assert!(matches!(
            outcome,
            ResolveOutcome::Found(ResolvedSymbolSource {
                source: DiscoverySource::AdjacentManifest,
                ..
            })
        ));
    }

    #[test]
    fn mismatched_manifest_identity_is_refused_not_symbolized() {
        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        touch(&image);
        let symbols = temp.path().join("wrong.pdb");
        touch(&symbols);
        write_manifest(
            &PathBuf::from(format!("{}.kernal-symbols.pb", image.display())),
            "wrong.pdb",
            "elf:dcba",
        );
        let mut verifier_called = false;
        let outcome = resolve_symbols(
            &module(&image),
            &DiscoveryConfig::default(),
            "elf:abcd",
            SymbolArtifactFormat::ElfDwarf,
            Path::new("app.pdb"),
            &[],
            |_| {
                verifier_called = true;
                true
            },
        );
        assert_eq!(outcome, ResolveOutcome::Mismatched { rejected: 1 });
        assert!(
            !verifier_called,
            "a false manifest claim must be rejected before parsing its candidate"
        );
    }

    #[test]
    fn manifest_guid_spelling_is_normalized_before_exact_selection() {
        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        let symbols = temp.path().join("app.pdb");
        touch(&image);
        touch(&symbols);
        write_manifest_with(
            &PathBuf::from(format!("{}.kernal-symbols.pb", image.display())),
            "app.pdb",
            "pdb:AABBCCDD-EEFF-0011-2233-445566778899-10",
            SymbolArtifactFormat::Pdb,
            None,
            None,
        );
        assert!(matches!(
            resolve_symbols(
                &module(&image),
                &DiscoveryConfig::default(),
                "pdb:aabbccddeeff00112233445566778899-10",
                SymbolArtifactFormat::Pdb,
                Path::new("app.pdb"),
                &[],
                |_| true
            ),
            ResolveOutcome::Found(_)
        ));
    }

    #[test]
    fn mismatched_manifest_format_is_refused_before_parsing() {
        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        let symbols = temp.path().join("wrong-format.pdb");
        touch(&image);
        touch(&symbols);
        write_manifest_with(
            &PathBuf::from(format!("{}.kernal-symbols.pb", image.display())),
            "wrong-format.pdb",
            "elf:abcd",
            SymbolArtifactFormat::Pdb,
            None,
            None,
        );

        let mut verifier_called = false;
        assert_eq!(
            resolve_symbols(
                &module(&image),
                &DiscoveryConfig::default(),
                "elf:abcd",
                SymbolArtifactFormat::ElfDwarf,
                Path::new("app.debug"),
                &[],
                |_| {
                    verifier_called = true;
                    true
                }
            ),
            ResolveOutcome::Mismatched { rejected: 1 }
        );
        assert!(!verifier_called);
    }

    #[test]
    fn manifest_integrity_mismatch_is_refused_before_parsing() {
        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        let symbols = temp.path().join("symbols.debug");
        touch(&image);
        touch(&symbols);
        write_manifest_with(
            &PathBuf::from(format!("{}.kernal-symbols.pb", image.display())),
            "symbols.debug",
            "elf:abcd",
            SymbolArtifactFormat::ElfDwarf,
            Some("00".repeat(32)),
            Some(7),
        );

        let mut verifier_called = false;
        assert_eq!(
            resolve_symbols(
                &module(&image),
                &DiscoveryConfig::default(),
                "elf:abcd",
                SymbolArtifactFormat::ElfDwarf,
                Path::new("app.debug"),
                &[],
                |_| {
                    verifier_called = true;
                    true
                }
            ),
            ResolveOutcome::Mismatched { rejected: 1 }
        );
        assert!(!verifier_called);
    }

    #[test]
    fn a_bad_earlier_candidate_does_not_hide_a_later_exact_build() {
        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        touch(&image);
        let wrong = temp.path().join("wrong.pdb");
        let good = temp.path().join("good.pdb");
        std::fs::write(&wrong, b"wrong build").unwrap();
        std::fs::write(&good, b"exact build").unwrap();
        let input = module(&image);
        let config = DiscoveryConfig {
            registered_symbol_paths: vec![
                wrong.to_string_lossy().into_owned(),
                good.to_string_lossy().into_owned(),
            ],
            ..Default::default()
        };

        let outcome = resolve_symbols(
            &input,
            &config,
            "elf:abcd",
            SymbolArtifactFormat::ElfDwarf,
            Path::new("app.pdb"),
            &[],
            |path| std::fs::read(path).is_ok_and(|bytes| bytes == b"exact build"),
        );
        assert_eq!(
            outcome,
            ResolveOutcome::Found(ResolvedSymbolSource {
                source: DiscoverySource::Registration,
                path: good,
            })
        );
    }

    #[test]
    fn verification_uses_a_private_snapshot_but_reports_the_declared_path() {
        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        let symbols = temp.path().join("app.debug");
        touch(&image);
        std::fs::write(&symbols, b"immutable symbol bytes").unwrap();
        let config = DiscoveryConfig {
            registered_symbol_paths: vec![symbols.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let mut verified_path = None;

        let outcome = resolve_symbols(
            &module(&image),
            &config,
            "elf:abcd",
            SymbolArtifactFormat::ElfDwarf,
            Path::new("unused.debug"),
            &[],
            |path| {
                verified_path = Some(path.to_path_buf());
                std::fs::read(path).is_ok_and(|bytes| bytes == b"immutable symbol bytes")
            },
        );

        assert_eq!(
            outcome,
            ResolveOutcome::Found(ResolvedSymbolSource {
                source: DiscoverySource::Registration,
                path: symbols.clone(),
            })
        );
        assert_ne!(verified_path.as_deref(), Some(symbols.as_path()));
        assert!(
            verified_path.is_some_and(|path| !path.exists()),
            "the worker-private snapshot should be removed after parsing"
        );
    }

    #[test]
    fn a_bad_manifest_declaration_does_not_dedupe_a_good_registration_path() {
        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        let symbols = temp.path().join("shared.debug");
        touch(&image);
        touch(&symbols);
        write_manifest(
            &PathBuf::from(format!("{}.kernal-symbols.pb", image.display())),
            "shared.debug",
            "elf:dcba",
        );
        let config = DiscoveryConfig {
            registered_symbol_paths: vec![symbols.to_string_lossy().into_owned()],
            ..Default::default()
        };
        assert_eq!(
            resolve_symbols(
                &module(&image),
                &config,
                "elf:abcd",
                SymbolArtifactFormat::ElfDwarf,
                Path::new("app.debug"),
                &[],
                |_| true
            ),
            ResolveOutcome::Found(ResolvedSymbolSource {
                source: DiscoverySource::Registration,
                path: symbols,
            })
        );
    }

    #[test]
    fn no_symbols_is_distinct_from_identity_rejection() {
        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        touch(&image);
        assert_eq!(
            resolve_symbols(
                &module(&image),
                &DiscoveryConfig::default(),
                "elf:abcd",
                SymbolArtifactFormat::ElfDwarf,
                Path::new("app.pdb"),
                &[],
                |_| false
            ),
            ResolveOutcome::NotFound
        );
    }

    #[test]
    fn standard_cache_layouts_are_identity_keyed() {
        assert_eq!(
            cache_candidate(Path::new("/cache"), "elf:abcdef", Path::new("app.debug")),
            PathBuf::from("/cache/.build-id/ab/cdef.debug")
        );
        assert_eq!(
            cache_candidate(Path::new("/cache"), "pdb:aabb-10", Path::new("app.pdb")),
            PathBuf::from("/cache/app.pdb/AABBa/app.pdb")
        );
        assert_eq!(
            server_key("pdb:aabb-17", Path::new("app.pdb")),
            vec!["app.pdb", "AABB11", "app.pdb"]
        );
        assert!(default_cache_root().ends_with("kernal-api/symbol-cache"));
    }

    #[test]
    fn macho_server_key_preserves_the_dsym_path_segments() {
        assert_eq!(
            server_key(
                "macho:abcdef",
                Path::new("app.dSYM/Contents/Resources/DWARF/app")
            ),
            vec![
                "abcdef",
                "app.dSYM",
                "Contents",
                "Resources",
                "DWARF",
                "app"
            ]
        );
    }

    #[test]
    fn manifest_relative_paths_cannot_escape_the_manifest_directory() {
        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        touch(&image);
        let outside = temp.path().parent().unwrap().join("outside.pdb");
        touch(&outside);
        write_manifest(
            &PathBuf::from(format!("{}.kernal-symbols.pb", image.display())),
            "../outside.pdb",
            "elf:abcd",
        );
        let mut verifier_called = false;
        assert_eq!(
            resolve_symbols(
                &module(&image),
                &DiscoveryConfig::default(),
                "elf:abcd",
                SymbolArtifactFormat::ElfDwarf,
                Path::new("app.pdb"),
                &[],
                |_| {
                    verifier_called = true;
                    true
                }
            ),
            ResolveOutcome::NotFound
        );
        assert!(!verifier_called);
    }

    #[cfg(unix)]
    #[test]
    fn manifest_symlinks_cannot_escape_the_manifest_directory() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside.debug");
        let image = root.join("app");
        touch(&image);
        touch(&outside);
        symlink(&outside, root.join("escape.debug")).unwrap();
        write_manifest(
            &PathBuf::from(format!("{}.kernal-symbols.pb", image.display())),
            "escape.debug",
            "elf:abcd",
        );
        assert_eq!(
            resolve_symbols(
                &module(&image),
                &DiscoveryConfig::default(),
                "elf:abcd",
                SymbolArtifactFormat::ElfDwarf,
                Path::new("app.debug"),
                &[],
                |_| true
            ),
            ResolveOutcome::NotFound
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_relative_manifest_paths_are_refused() {
        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        touch(&image);
        write_manifest(
            &PathBuf::from(format!("{}.kernal-symbols.pb", image.display())),
            r"C:escape.debug",
            "elf:abcd",
        );
        assert_eq!(
            resolve_symbols(
                &module(&image),
                &DiscoveryConfig::default(),
                "elf:abcd",
                SymbolArtifactFormat::ElfDwarf,
                Path::new("app.debug"),
                &[],
                |_| true
            ),
            ResolveOutcome::NotFound
        );
    }

    #[test]
    fn configured_http_server_uses_the_typed_elf_route_and_bounds_to_a_temp_file() {
        let (base, server) = serve_once("200 OK", b"symbols".to_vec(), "");
        let outcome = resolve_from_servers(
            &[base, "http://127.0.0.1:1/must-not-be-contacted".into()],
            "elf:abcdef",
            Path::new("ignored.debug"),
            |path| std::fs::read(path).ok(),
        );
        let ServerResolve::Found { value, .. } = outcome else {
            panic!("expected a verified server response");
        };
        assert_eq!(value, b"symbols");
        let request = server.join().unwrap();
        assert!(
            request.starts_with("GET /buildid/abcdef/debuginfo HTTP/1.1"),
            "{request}"
        );
    }

    #[test]
    fn a_verified_server_download_survives_the_call_that_fetched_it() {
        // The whole point of retaining it (#818): line programs are read in a
        // later pre-pass, long after `resolve_from_servers` has returned. An
        // earlier revision dropped the `NamedTempFile` on return, so the file
        // was already gone and server-sourced modules silently resolved to
        // names only.
        let (base, server) = serve_once("200 OK", b"symbols".to_vec(), "");
        let outcome =
            resolve_from_servers(&[base], "elf:abcdef", Path::new("ignored.debug"), |path| {
                std::fs::read(path).ok()
            });
        let ServerResolve::Found { retained, .. } = outcome else {
            panic!("expected a verified server response");
        };
        let path = retained.to_path_buf();
        assert!(
            path.is_file(),
            "the verified download must still exist at {}",
            path.display()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"symbols");

        // And it is still temporary: dropping the handle removes it, so a
        // long-lived process would not accumulate symbol files.
        drop(retained);
        assert!(!path.exists(), "dropping the handle must remove the file");
        let _ = server.join();
    }

    #[test]
    fn configured_pdb_server_uses_hex_guid_age() {
        let (base, server) = serve_once("200 OK", b"pdb".to_vec(), "");
        assert!(matches!(
            resolve_from_servers(&[base], "pdb:aabbccdd-10", Path::new("app.pdb"), |_| Some(
                ()
            )),
            ServerResolve::Found { .. }
        ));
        let request = server.join().unwrap();
        assert!(
            request.starts_with("GET /app.pdb/AABBCCDDa/app.pdb HTTP/1.1"),
            "{request}"
        );
    }

    #[test]
    fn configured_server_redirects_are_not_followed() {
        let (base, server) = serve_once(
            "302 Found",
            Vec::new(),
            "Location: http://127.0.0.1:1/unapproved\r\n",
        );
        assert!(matches!(
            resolve_from_servers(&[base], "elf:abcdef", Path::new("ignored.debug"), |_| Some(
                ()
            )),
            ServerResolve::NotFound
        ));
        let _ = server.join().unwrap();
    }

    #[test]
    fn capture_identity_rejects_a_replaced_module_image() {
        use sha2::{Digest as _, Sha256};

        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        std::fs::write(&image, b"loaded build").unwrap();
        let digest = Sha256::digest(b"loaded build");
        let mut input = module(&image);
        input.code_id = Some(format!(
            "sha256:{}",
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        assert!(captured_image_still_matches(&input));

        std::fs::write(&image, b"replacement build").unwrap();
        assert!(!captured_image_still_matches(&input));
    }

    #[test]
    fn unusable_capture_identity_fails_closed() {
        let temp = TempDir::new().unwrap();
        let image = temp.path().join("app.exe");
        touch(&image);
        let mut input = module(&image);
        input.code_id = Some("sha256:unavailable".into());
        assert!(!captured_image_still_matches(&input));
    }

    #[test]
    fn configured_servers_are_off_when_no_admin_origins_are_supplied() {
        assert!(matches!(
            resolve_from_servers(&[], "elf:abcdef", Path::new("ignored.debug"), |_| Some(())),
            ServerResolve::NotFound
        ));
    }
}
