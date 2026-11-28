use serde::Deserialize;
use url::Url;

use super::catalogue_json::reject_duplicate_json_keys;
use super::catalogue_lookup::{
    CATALOGUE_CAPABILITY, MANIFEST_FETCH_TIMEOUT, MAX_CATALOGUE_ASSET_BYTES, MAX_CATALOGUE_PARTS,
    MAX_CATALOGUE_PART_BYTES, MAX_CATALOGUE_URL_BYTES,
};
use crate::core::SoldrError;

/// One row in the published asset index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub owner: String,
    pub repo: String,
    pub tag: String,
    pub asset: String,
    pub transport: AssetTransport,
    pub sha256: String,
    pub size_bytes: u64,
    pub min_client_version: Option<u32>,
    /// Immutable assets-branch path supplied by v2 publishers.  Legacy v1
    /// rows have no path; consumers that require multipart use this stable
    /// identity rather than a direct URL.
    pub source_path: Option<String>,
}

impl ManifestEntry {
    pub fn direct_url(&self) -> Option<&str> {
        self.transport.direct_url()
    }

    /// Return the first URL the transport will request for progress output.
    pub fn display_url(&self) -> &str {
        self.direct_url()
            .or_else(|| match &self.transport {
                AssetTransport::Direct { .. } => None,
                AssetTransport::Multipart { parts } => parts
                    .first()
                    .and_then(|part| part.urls.first())
                    .map(String::as_str),
            })
            .unwrap_or("multipart catalogue asset")
    }

    /// Match the pre-v2 repository URL used by pinned callers to the same
    /// logical catalogue row after its transport is rewritten to multipart.
    pub fn matches_legacy_url(&self, expected: &str) -> bool {
        self.transport.direct_url() == Some(expected)
            || self
                .source_path
                .as_ref()
                .is_some_and(|path| expected.ends_with(&format!("/assets/{path}")))
    }
}

/// Transport details deliberately kept separate from an asset's logical hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetTransport {
    Direct { urls: Vec<String> },
    Multipart { parts: Vec<Part> },
}

impl AssetTransport {
    pub fn direct_url(&self) -> Option<&str> {
        match self {
            Self::Direct { urls } => urls.first().map(String::as_str),
            Self::Multipart { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub number: u32,
    pub size_bytes: u64,
    pub sha256: String,
    pub urls: Vec<String>,
}

#[derive(Deserialize)]
struct WireV1Catalogue {
    #[serde(default)]
    entries: Vec<WireV1Entry>,
}

#[derive(Deserialize)]
struct WireV1Entry {
    owner: String,
    repo: String,
    tag: String,
    asset: String,
    url: String,
    sha256: String,
}

/// V2 has a deliberately closed wire contract.  Additions require a new
/// schema version so old clients never reinterpret a transport field.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireV2Catalogue {
    schema_version: u32,
    generation: String,
    publication_state: PublicationStateBinding,
    #[serde(default)]
    generated_at: Option<String>,
    #[serde(default)]
    origin: Option<String>,
    pub(super) entries: Vec<WireV2Entry>,
}

/// The v2 root is bound to the immutable publication generation.  The
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationStateBinding {
    generation: String,
    url: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireV2Entry {
    pub(super) owner: String,
    pub(super) repo: String,
    pub(super) tag: String,
    pub(super) asset: String,
    pub(super) size_bytes: u64,
    pub(super) sha256: String,
    pub(super) urls: Option<Vec<String>>,
    pub(super) parts: Option<Vec<WirePart>>,
    #[serde(default)]
    pub(super) min_client_version: Option<u32>,
    #[serde(default)]
    pub(super) source_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PublicationState {
    schema_version: u32,
    generation: String,
    source: SourceGitObject,
    active: PublicationSlot,
    previous: PublicationSlot,
    catalogue_sha256: String,
    assets_by_sha256: std::collections::BTreeMap<String, PublishedAsset>,
    logical_assets: std::collections::BTreeMap<String, LogicalAsset>,
    partitioner_default: DefaultPartitioner,
    published_at: u64,
    retained_generations: Vec<RetainedGeneration>,
    parts_by_sha256: std::collections::BTreeMap<String, PublishedPartIndex>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GitObject {
    commit: String,
    tree: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceGitObject {
    commit: String,
    tree: String,
    branch: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationSlot {
    slot: String,
    commit: String,
    tree: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedAsset {
    size_bytes: u64,
    partitioner: Partitioner,
    parts: Vec<PublishedPart>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Partitioner {
    version: u32,
    target_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultPartitioner {
    version: u32,
    target_bytes: u64,
    max_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedGeneration {
    generation: String,
    published_at: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedPart {
    number: u32,
    sha256: String,
    size_bytes: u64,
    path: String,
    git_blob: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedPartIndex {
    size_bytes: u64,
    git_blob: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogicalAsset {
    source_path: String,
    asset: String,
    source_oid_sha256: String,
    source_size_bytes: u64,
    metadata_fingerprint: String,
    provenance: serde_json::Map<String, serde_json::Value>,
}

fn write_canonical_json(value: &serde_json::Value, out: &mut Vec<u8>) -> Option<()> {
    match value {
        serde_json::Value::Null => out.extend_from_slice(b"null"),
        serde_json::Value::Bool(value) => {
            out.extend_from_slice(if *value { b"true" } else { b"false" })
        }
        serde_json::Value::Number(value) => out.extend_from_slice(value.to_string().as_bytes()),
        serde_json::Value::String(value) => out.extend_from_slice(&serde_json::to_vec(value).ok()?),
        serde_json::Value::Array(values) => {
            out.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                write_canonical_json(value, out)?;
            }
            out.push(b']');
        }
        serde_json::Value::Object(values) => {
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort_unstable();
            out.push(b'{');
            for (index, key) in keys.into_iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                out.extend_from_slice(&serde_json::to_vec(key).ok()?);
                out.push(b':');
                write_canonical_json(values.get(key)?, out)?;
            }
            out.push(b'}');
        }
    }
    Some(())
}

pub(super) fn canonical_catalogue_sha256(body: &str) -> Option<String> {
    reject_duplicate_json_keys(body).ok()?;
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut canonical = Vec::new();
    write_canonical_json(&value, &mut canonical)?;
    Some(super::trust::sha256_of(&canonical))
}

pub(crate) async fn bind_v2_publication_state(body: &str) -> Result<(), SoldrError> {
    reject_duplicate_json_keys(body).map_err(SoldrError::Other)?;
    let catalogue: WireV2Catalogue = serde_json::from_str(body).map_err(|error| {
        SoldrError::Other(format!("canonical v2 catalogue did not parse: {error}"))
    })?;
    let digest = canonical_catalogue_sha256(body).ok_or_else(|| {
        SoldrError::Other("canonical v2 catalogue could not be canonicalized".into())
    })?;
    let state_url = &catalogue.publication_state.url;
    let safe_state_url = super::stream_download::safe_asset_url(state_url);
    let client =
        super::stream_download::control_http_client("the soldr-toolchain publication state")?;
    let response = super::stream_download::send_control_request_with_timeout(
        super::stream_download::get_request(&client, state_url),
        state_url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await?;
    if !(200..300).contains(&response.status()) {
        return Err(SoldrError::Network(format!(
            "publication-state fetch {safe_state_url} returned HTTP {}",
            response.status()
        )));
    }
    let state_body =
        super::stream_download::read_control_text(response, state_url, MANIFEST_FETCH_TIMEOUT)
            .await?;
    validate_publication_state_body(&state_body, &catalogue.generation, &digest)?;
    let state = parse_publication_state(&state_body)?;
    if publication_entries_match_state(&catalogue.entries, &state) {
        Ok(())
    } else {
        Err(SoldrError::Other(
            "publication state does not bind every catalogue source asset".into(),
        ))
    }
}

pub(super) fn publication_state_matches(
    state: &PublicationState,
    generation: &str,
    digest: &str,
) -> bool {
    state.schema_version == 1
        && state.generation == generation
        && valid_sha256(&state.catalogue_sha256)
        && state.catalogue_sha256 == digest
        && valid_git_object(&state.source.commit)
        && valid_git_object(&state.source.tree)
        && valid_git_object(&state.active.commit)
        && valid_git_object(&state.active.tree)
        && valid_git_object(&state.previous.commit)
        && valid_git_object(&state.previous.tree)
        && state.active.slot != state.previous.slot
        && matches!(state.active.slot.as_str(), "public-a" | "public-b")
        && matches!(state.previous.slot.as_str(), "public-a" | "public-b")
        && state.source.branch == "assets"
        && valid_default_partitioner(&state.partitioner_default)
        && state.published_at > 0
        && !state.retained_generations.is_empty()
        && state
            .retained_generations
            .iter()
            .all(|g| valid_generation(&g.generation) && g.published_at > 0)
        && state
            .retained_generations
            .iter()
            .any(|g| g.generation == state.generation)
        && state
            .retained_generations
            .iter()
            .map(|g| &g.generation)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == state.retained_generations.len()
        && state
            .assets_by_sha256
            .iter()
            .all(|(sha256, asset)| valid_published_asset(sha256, asset))
        && state.logical_assets.iter().all(|(key, logical)| {
            !key.is_empty()
                && valid_logical_asset(logical)
                && state
                    .assets_by_sha256
                    .get(&logical.source_oid_sha256)
                    .is_some_and(|asset| asset.size_bytes == logical.source_size_bytes)
        })
        && state
            .logical_assets
            .values()
            .map(|logical| logical.source_oid_sha256.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            == state
                .assets_by_sha256
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>()
        && valid_part_index(&state.assets_by_sha256, &state.parts_by_sha256)
}

pub(super) fn publication_entries_match_state(
    entries: &[WireV2Entry],
    state: &PublicationState,
) -> bool {
    let mut source_paths = std::collections::BTreeSet::new();
    let mut logical_keys = std::collections::BTreeSet::new();
    for entry in entries {
        match (&entry.urls, &entry.parts, &entry.source_path) {
            (Some(_), None, None) => continue,
            (None, Some(_), Some(source_path)) => {
                if !source_paths.insert(source_path.as_str()) {
                    return false;
                }
                let key = format!(
                    "{}\0{}\0{}\0{}",
                    entry.owner, entry.repo, entry.tag, entry.asset
                );
                let Some(logical) = state.logical_assets.get(&key) else {
                    return false;
                };
                let provenance = &logical.provenance;
                if logical.source_path != *source_path
                    || !publication_asset_identity_matches(entry, logical, source_path)
                    || !publication_part_urls_match(entry, state)
                    || logical.source_oid_sha256 != entry.sha256
                    || logical.source_size_bytes != entry.size_bytes
                    || provenance.len() != 4
                    || provenance.get("owner").and_then(serde_json::Value::as_str)
                        != Some(entry.owner.as_str())
                    || provenance.get("repo").and_then(serde_json::Value::as_str)
                        != Some(entry.repo.as_str())
                    || provenance.get("tag").and_then(serde_json::Value::as_str)
                        != Some(entry.tag.as_str())
                    || provenance.get("asset").and_then(serde_json::Value::as_str)
                        != Some(logical.asset.as_str())
                {
                    return false;
                }
                logical_keys.insert(key);
            }
            _ => return false,
        }
    }
    logical_keys.len() == state.logical_assets.len()
}

fn publication_part_urls_match(entry: &WireV2Entry, state: &PublicationState) -> bool {
    let Some(wire_parts) = entry.parts.as_ref() else {
        return false;
    };
    let Some(published) = state.assets_by_sha256.get(&entry.sha256) else {
        return false;
    };
    wire_parts.len() == published.parts.len()
        && wire_parts.iter().zip(&published.parts).all(|(wire, part)| {
            let expected_url = format!(
                "https://raw.githubusercontent.com/zackees/soldr-toolchain/{}/{}",
                state.active.slot, part.path
            );
            wire.number == part.number
                && wire.sha256 == part.sha256
                && wire.size_bytes == part.size_bytes
                && wire.urls.as_slice() == [expected_url]
        })
}

fn publication_asset_identity_matches(
    entry: &WireV2Entry,
    logical: &LogicalAsset,
    source_path: &str,
) -> bool {
    entry.asset == logical.asset
        || (entry.asset == source_path
            && source_path
                .rsplit('/')
                .next()
                .is_some_and(|filename| filename == logical.asset))
}

fn valid_partitioner(partitioner: &Partitioner) -> bool {
    partitioner.version == 1
        && partitioner.target_bytes > 0
        && partitioner.target_bytes <= MAX_CATALOGUE_PART_BYTES
}

fn valid_default_partitioner(partitioner: &DefaultPartitioner) -> bool {
    partitioner.version == 1
        && partitioner.max_bytes == MAX_CATALOGUE_PART_BYTES
        && partitioner.target_bytes > 0
        && partitioner.target_bytes <= partitioner.max_bytes
}

fn valid_part_index(
    assets: &std::collections::BTreeMap<String, PublishedAsset>,
    index: &std::collections::BTreeMap<String, PublishedPartIndex>,
) -> bool {
    let mut expected = std::collections::BTreeMap::new();
    for asset in assets.values() {
        for part in &asset.parts {
            let row = (part.size_bytes, part.git_blob.as_str());
            match expected.insert(part.sha256.as_str(), row) {
                Some(previous) if previous != row => return false,
                _ => {}
            }
        }
    }
    expected.len() == index.len()
        && expected.into_iter().all(|(sha, (size, blob))| {
            index
                .get(sha)
                .is_some_and(|found| found.size_bytes == size && found.git_blob == blob)
        })
}

fn valid_git_object(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_published_asset(sha256: &str, asset: &PublishedAsset) -> bool {
    if !valid_sha256(sha256)
        || asset.size_bytes == 0
        || asset.size_bytes > MAX_CATALOGUE_ASSET_BYTES
        || !valid_partitioner(&asset.partitioner)
        || asset.parts.is_empty()
        || asset.parts.len() > MAX_CATALOGUE_PARTS
    {
        return false;
    }
    let mut total = 0_u64;
    for (offset, part) in asset.parts.iter().enumerate() {
        let expected = (offset + 1) as u32;
        if part.number != expected
            || !valid_sha256(&part.sha256)
            || part.size_bytes == 0
            || part.size_bytes > MAX_CATALOGUE_PART_BYTES
            || (offset + 1 < asset.parts.len() && part.size_bytes != asset.partitioner.target_bytes)
            || (offset + 1 == asset.parts.len() && part.size_bytes > asset.partitioner.target_bytes)
            || part.path != format!("sha256/{sha256}/{expected:04}-{}.part", part.sha256)
            || !valid_git_object(&part.git_blob)
        {
            return false;
        }
        let Some(next) = total.checked_add(part.size_bytes) else {
            return false;
        };
        total = next;
    }
    total == asset.size_bytes
}

fn valid_logical_asset(logical: &LogicalAsset) -> bool {
    valid_source_path(&logical.source_path)
        && !logical.asset.is_empty()
        && valid_sha256(&logical.metadata_fingerprint)
        && valid_sha256(&logical.source_oid_sha256)
        && logical.source_size_bytes > 0
        && logical.source_size_bytes <= MAX_CATALOGUE_ASSET_BYTES
        && !logical.provenance.is_empty()
        && logical.provenance.keys().all(|key| !key.is_empty())
}

fn valid_source_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
}

pub(super) fn parse_publication_state(body: &str) -> Result<PublicationState, SoldrError> {
    reject_duplicate_json_keys(body).map_err(SoldrError::Other)?;
    serde_json::from_str(body)
        .map_err(|error| SoldrError::Other(format!("publication state did not parse: {error}")))
}

pub(super) fn validate_publication_state_body(
    body: &str,
    generation: &str,
    digest: &str,
) -> Result<(), SoldrError> {
    let state = parse_publication_state(body)?;
    if publication_state_matches(&state, generation, digest) {
        Ok(())
    } else {
        Err(SoldrError::Other(
            "publication state does not bind this catalogue generation".into(),
        ))
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WirePart {
    pub(super) number: u32,
    pub(super) size_bytes: u64,
    pub(super) sha256: String,
    pub(super) urls: Vec<String>,
}

/// True when a published release requires a newer transport-capable client.
pub fn supports_min_client_version(required: Option<u32>) -> bool {
    required.unwrap_or(0) <= CATALOGUE_CAPABILITY
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn validate_url(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_CATALOGUE_URL_BYTES {
        return Err("invalid URL length".into());
    }
    let parsed = Url::parse(value).map_err(|_| "URL is not absolute".to_string())?;
    let loopback_http = parsed.scheme() == "http"
        && parsed
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|address| address.is_loopback());
    if (parsed.scheme() != "https" && !loopback_http)
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err("URL must be credential-free absolute HTTPS (or loopback HTTP)".into());
    }
    Ok(())
}
pub(super) fn valid_generation(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn validate_publication_state_url(value: &str, generation: &str) -> Result<(), String> {
    validate_url(value)?;
    let parsed = Url::parse(value).map_err(|_| "URL is not absolute".to_string())?;
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("publication-state URL must not have query or fragment".into());
    }
    let expected = format!("/generations/{generation}/publish-state.v1.json");
    if !parsed.path().ends_with(&expected)
        || parsed
            .path()
            .split('/')
            .any(|part| part == "." || part == "..")
    {
        return Err("publication-state URL is not generation-qualified".into());
    }
    Ok(())
}
fn validate_urls(
    urls: &[String],
    all_urls: &mut std::collections::BTreeSet<String>,
) -> Result<(), String> {
    if urls.is_empty() {
        return Err("transport has no URLs".into());
    }
    for url in urls {
        validate_url(url)?;
        if !all_urls.insert(url.clone()) {
            return Err("duplicate URL".into());
        }
    }
    Ok(())
}

fn validate_v2_direct_urls(
    urls: &[String],
    all_urls: &mut std::collections::BTreeSet<String>,
    part_urls: &std::collections::BTreeMap<String, (String, u64)>,
) -> Result<(), String> {
    validate_urls(urls, all_urls)?;
    if urls.iter().any(|url| part_urls.contains_key(url)) {
        return Err("direct URL duplicates a multipart URL".into());
    }
    Ok(())
}

fn validate_part_urls(
    urls: &[String],
    sha256: &str,
    size_bytes: u64,
    all_urls: &std::collections::BTreeSet<String>,
    part_urls: &mut std::collections::BTreeMap<String, (String, u64)>,
) -> Result<(), String> {
    if urls.is_empty() {
        return Err("transport has no URLs".into());
    }
    let mut local = std::collections::BTreeSet::new();
    for url in urls {
        validate_url(url)?;
        if !local.insert(url) || all_urls.contains(url) {
            return Err("duplicate URL".into());
        }
        let identity = (sha256.to_string(), size_bytes);
        if part_urls
            .get(url)
            .is_some_and(|previous| previous != &identity)
        {
            return Err("URL reused for a different multipart identity".into());
        }
        part_urls.insert(url.clone(), identity);
    }
    Ok(())
}

fn validate_unique_logical_rows(entries: &[ManifestEntry]) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for entry in entries {
        if !seen.insert((&entry.owner, &entry.repo, &entry.tag, &entry.asset)) {
            return Err("duplicate logical catalogue row".into());
        }
    }
    Ok(())
}

fn entry_from_v1_wire(entry: WireV1Entry) -> Result<ManifestEntry, String> {
    if !valid_sha256(&entry.sha256) {
        return Err("asset SHA-256 must be lowercase 64-hex".into());
    }
    validate_url(&entry.url)?;
    let urls = vec![entry.url];
    Ok(ManifestEntry {
        owner: entry.owner,
        repo: entry.repo,
        tag: entry.tag,
        asset: entry.asset,
        transport: AssetTransport::Direct { urls },
        sha256: entry.sha256,
        size_bytes: 0,
        min_client_version: None,
        source_path: None,
    })
}

pub(super) fn entry_from_v2_wire(
    entry: WireV2Entry,
    all_urls: &mut std::collections::BTreeSet<String>,
    part_urls: &mut std::collections::BTreeMap<String, (String, u64)>,
) -> Result<ManifestEntry, String> {
    if [&entry.owner, &entry.repo, &entry.tag, &entry.asset]
        .iter()
        .any(|value| value.is_empty())
    {
        return Err("catalogue identity fields must be nonempty".into());
    }
    if !valid_sha256(&entry.sha256) {
        return Err("asset SHA-256 must be lowercase 64-hex".into());
    }
    if entry.size_bytes == 0 || entry.size_bytes > MAX_CATALOGUE_ASSET_BYTES {
        return Err("invalid asset size_bytes".into());
    }
    if entry
        .min_client_version
        .is_some_and(|version| version != CATALOGUE_CAPABILITY)
    {
        return Err("min_client_version must equal this client capability".into());
    }
    let has_source_path = entry.source_path.is_some();
    let transport = match (entry.urls, entry.parts) {
        (Some(urls), None) => {
            if has_source_path {
                return Err("direct assets must not declare source_path".into());
            }
            validate_v2_direct_urls(&urls, all_urls, part_urls)?;
            AssetTransport::Direct { urls }
        }
        (None, Some(wire_parts)) => {
            if !has_source_path {
                return Err("multipart assets require source_path".into());
            }
            if entry.min_client_version != Some(CATALOGUE_CAPABILITY) {
                return Err("multipart assets require this client capability".into());
            }
            if wire_parts.is_empty() || wire_parts.len() > MAX_CATALOGUE_PARTS {
                return Err("too many parts".into());
            }
            let mut total = 0u64;
            let mut parts = Vec::with_capacity(wire_parts.len());
            let mut identities = std::collections::BTreeSet::new();
            for (offset, part) in wire_parts.into_iter().enumerate() {
                if part.number != (offset + 1) as u32
                    || part.size_bytes == 0
                    || part.size_bytes > MAX_CATALOGUE_PART_BYTES
                    || !valid_sha256(&part.sha256)
                {
                    return Err("invalid or non-contiguous part".into());
                }
                validate_part_urls(
                    &part.urls,
                    &part.sha256,
                    part.size_bytes,
                    all_urls,
                    part_urls,
                )?;
                if !identities.insert((part.number, part.sha256.clone())) {
                    return Err("duplicate part".into());
                }
                total = total
                    .checked_add(part.size_bytes)
                    .ok_or("part size overflow")?;
                parts.push(Part {
                    number: part.number,
                    size_bytes: part.size_bytes,
                    sha256: part.sha256,
                    urls: part.urls,
                });
            }
            if total != entry.size_bytes {
                return Err("part sizes do not equal asset size".into());
            }
            AssetTransport::Multipart { parts }
        }
        _ => return Err("asset must contain exactly one transport field".into()),
    };
    if entry
        .source_path
        .as_deref()
        .is_some_and(|path| !valid_source_path(path))
    {
        return Err("source_path must be a safe relative path".into());
    }
    Ok(ManifestEntry {
        owner: entry.owner,
        repo: entry.repo,
        tag: entry.tag,
        asset: entry.asset,
        transport,
        sha256: entry.sha256,
        size_bytes: entry.size_bytes,
        min_client_version: entry.min_client_version,
        source_path: entry.source_path,
    })
}

/// Parsed shape of the published asset index. Kept deliberately flat —
/// a `Vec` scan is fine at the call rate (one lookup per `fetch_tool`
/// call) and lets us drop a `serde_json::from_str` straight onto the
/// downloaded body with no post-processing.
#[derive(Debug, Clone, Default)]
pub struct ManifestIndex {
    pub entries: Vec<ManifestEntry>,
    /// A syntactically present v2 document was unsafe. Callers must not turn
    /// this into a legacy/live-API fallback.
    pub fail_closed: bool,
    pub(crate) source: CatalogueSource,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CatalogueSource {
    #[default]
    LegacyV1,
    CanonicalV2,
}

pub(crate) fn fail_closed_v2_index() -> ManifestIndex {
    ManifestIndex {
        entries: vec![],
        fail_closed: true,
        source: CatalogueSource::CanonicalV2,
    }
}

pub(crate) fn authoritative_v2_index(
    parsed: Option<ManifestIndex>,
    state_bound: bool,
) -> ManifestIndex {
    match (parsed, state_bound) {
        (Some(index), true) => index,
        _ => fail_closed_v2_index(),
    }
}

impl ManifestIndex {
    /// Empty manifest. Used as the graceful-degrade fallback when the
    /// network fetch, disable env var, or JSON parse fails.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse a JSON body into a [`ManifestIndex`]. Returns `None` (not
    /// an error) on parse failure so the caller can degrade silently to
    /// an empty index — a malformed remote manifest must never wedge a
    /// build.
    pub fn from_json(body: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(body).ok()?;
        if value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            == Some(2)
        {
            Self::from_v2_json(body)
        } else {
            Self::from_v1_json(body)
        }
    }

    pub(crate) fn from_v1_json(body: &str) -> Option<Self> {
        reject_duplicate_json_keys(body).ok()?;
        let wire: WireV1Catalogue = serde_json::from_str(body).ok()?;
        let mut entries = Vec::with_capacity(wire.entries.len());
        for entry in wire.entries {
            entries.push(entry_from_v1_wire(entry).ok()?);
        }
        Some(Self {
            entries,
            fail_closed: false,
            source: CatalogueSource::LegacyV1,
        })
    }

    /// Parse only the canonical v2 wire contract.  Unlike `from_json`, this
    /// never accepts an absent, v1, or unknown schema version.
    pub(crate) fn from_v2_json(body: &str) -> Option<Self> {
        reject_duplicate_json_keys(body).ok()?;
        let wire: WireV2Catalogue = serde_json::from_str(body).ok()?;
        if wire.schema_version != 2
            || !valid_generation(&wire.generation)
            || wire.publication_state.generation != wire.generation
        {
            return None;
        }
        validate_publication_state_url(&wire.publication_state.url, &wire.generation).ok()?;
        if wire
            .origin
            .as_deref()
            .is_some_and(|origin| validate_url(origin).is_err())
        {
            return None;
        }
        let mut all_urls = std::collections::BTreeSet::new();
        all_urls.insert(wire.publication_state.url);
        let mut part_urls = std::collections::BTreeMap::new();
        let mut entries = Vec::with_capacity(wire.entries.len());
        for entry in wire.entries {
            entries.push(entry_from_v2_wire(entry, &mut all_urls, &mut part_urls).ok()?);
        }
        validate_unique_logical_rows(&entries).ok()?;
        Some(Self {
            entries,
            fail_closed: false,
            source: CatalogueSource::CanonicalV2,
        })
    }

    /// Look up the manifest entry for `(owner, repo, tag, asset)`.
    /// Tag matching is exact — the caller is expected to supply the
    /// tag string in the same form the manifest publishes (e.g.
    /// `1.12.9`, not `v1.12.9`, when that's how the upstream release
    /// names it). When the manifest is empty (graceful-degrade case)
    /// every lookup returns `None`.
    pub fn lookup(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
        asset: &str,
    ) -> Option<&ManifestEntry> {
        self.entries
            .iter()
            .find(|e| e.owner == owner && e.repo == repo && e.tag == tag && e.asset == asset)
    }

    /// Look up by `(owner, repo, tag)` only, ignoring the asset filter.
    /// Useful when the caller already knows the platform-matched asset
    /// name and just wants to round-trip the URL + sha256 for that one
    /// release. The first matching entry wins.
    pub fn lookup_release(&self, owner: &str, repo: &str, tag: &str) -> Vec<&ManifestEntry> {
        self.entries
            .iter()
            .filter(|e| e.owner == owner && e.repo == repo && e.tag == tag)
            .collect()
    }

    /// Look up an exact asset filename regardless of its catalogue owner.
    ///
    /// soldr-toolchain republishes some cross-platform tools under its own
    /// `assets` identity when the upstream release does not cover every Soldr
    /// target. Those rows retain an exact, versioned filename, so filename is
    /// the stable lookup key for that explicitly opted-in path.
    pub fn lookup_asset(&self, asset: &str) -> Vec<&ManifestEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.asset == asset)
            .collect()
    }
}
