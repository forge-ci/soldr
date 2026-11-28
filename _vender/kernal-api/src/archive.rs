//! Bounded archive extraction, deterministic archive writers, and in-memory
//! codecs. Callers own staging publication and cleanup.
//!
//! By default the destination must be absent or empty and remain exclusively
//! controlled by the caller throughout extraction; [`ExistingDestination`]
//! opts into merging. Failure may leave partial output.
//!
//! The `archive` feature covers ZIP, tar, gzip and zstd. `archive-xz` and
//! `archive-bzip2` add the [`ArchiveFormat::TarXz`] and
//! [`ArchiveFormat::TarBzip2`] decoders; without them those formats fail with
//! [`std::io::ErrorKind::Unsupported`].
//! ZIP reading supports only the Stored and Deflated methods, the two that
//! [`ZipWriter`] emits. An entry using any other method (bzip2, LZMA, XZ,
//! zstd, PPMd, Deflate64) or AES encryption fails extraction with
//! [`std::io::ErrorKind::Unsupported`]; no feature enables more ZIP codecs.
//! Symbolic targets retain their relative spelling on Unix; Windows converts
//! archive `/` separators to native `\` separators for usable relative links.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

mod stream;
mod tar_format;
mod write;

pub use stream::{
    extract_reader, zstd_decode, zstd_encode, TarCompression, TarEntries, TarEntry, TarEntryKind,
    TarReader,
};
pub use write::{TarWriter, ZipCompression, ZipWriter};

#[cfg(all(test, feature = "archive-auth-test-support"))]
mod zip_reader;

#[cfg(all(test, feature = "archive-auth-test-support"))]
pub(crate) mod authenticated_staging;

#[cfg(test)]
mod owned_source_tests {
    use super::*;

    #[test]
    fn anonymous_seekable_source_extracts_large_entry_without_a_source_path() {
        const LENGTH: u64 = 17 * 1024 * 1024;
        let mut writer = zip::ZipWriter::new(tempfile::tempfile().unwrap());
        writer
            .start_file("payload", zip::write::SimpleFileOptions::default())
            .unwrap();
        let chunk = [0x5a; 64 * 1024];
        for _ in 0..LENGTH / chunk.len() as u64 {
            writer.write_all(&chunk).unwrap();
        }
        let source = writer.finish().unwrap();
        let output = tempfile::tempdir().unwrap();
        extract_file(
            source,
            output.path(),
            ArchiveFormat::Zip,
            ExtractionLimits {
                max_entry_bytes: LENGTH,
                max_output_bytes: LENGTH,
                ..ExtractionLimits::default()
            },
        )
        .unwrap();
        let mut payload = File::open(output.path().join("payload")).unwrap();
        let mut read = [0; 64 * 1024];
        let mut total = 0;
        loop {
            let count = payload.read(&mut read).unwrap();
            if count == 0 {
                break;
            }
            assert!(read[..count].iter().all(|byte| *byte == 0x5a));
            total += count as u64;
        }
        assert_eq!(total, LENGTH);
    }
}

struct MetadataBudget {
    file: File,
    // One non-Clone reader, always accessed through an exclusive borrow.
    // The shared control only disables the ceiling after metadata parsing;
    // it is not a concurrently consumed staging/storage quota.
    remaining: Arc<AtomicU64>,
}

impl Read for MetadataBudget {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let remaining = self.remaining.load(Ordering::Relaxed);
        let allowed = remaining.min(buffer.len() as u64) as usize;
        if allowed == 0 {
            return Err(invalid("ZIP metadata read budget exhausted"));
        }
        let count = self.file.read(&mut buffer[..allowed])?;
        self.remaining
            .store(remaining - count as u64, Ordering::Relaxed);
        Ok(count)
    }
}

impl Seek for MetadataBudget {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

fn copy_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    remaining: &mut u64,
) -> io::Result<()> {
    let mut buffer = [0; 64 * 1024];
    loop {
        let capacity = remaining.saturating_add(1).min(buffer.len() as u64) as usize;
        let count = match reader.read(&mut buffer[..capacity]) {
            Ok(0) => return Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if count as u64 > *remaining {
            return Err(invalid("archive output exceeds byte limit"));
        }
        writer.write_all(&buffer[..count])?;
        *remaining -= count as u64;
    }
}

fn copy_entry_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    remaining: &mut u64,
    max_entry_bytes: u64,
) -> io::Result<()> {
    let before = (*remaining).min(max_entry_bytes);
    let mut entry_remaining = before;
    let result = copy_bounded(reader, writer, &mut entry_remaining);
    *remaining -= before - entry_remaining;
    result
}

#[cfg(test)]
mod entry_budget_tests {
    use super::*;

    #[test]
    fn entry_copy_limits_actual_bytes_even_without_size_metadata() {
        for (bytes, limit, succeeds) in [
            (&b""[..], 0, true),
            (&b"x"[..], 0, false),
            (&b"1234"[..], 4, true),
            (&b"12345"[..], 4, false),
        ] {
            let mut output = Vec::new();
            let mut total = 10;
            let result = copy_entry_bounded(&mut &bytes[..], &mut output, &mut total, limit);
            assert_eq!(result.is_ok(), succeeds);
            assert!(output.len() as u64 <= limit);
            assert_eq!(10 - total, output.len() as u64);
        }
        let mut total = 3;
        let mut output = Vec::new();
        assert!(copy_entry_bounded(&mut &b"1234"[..], &mut output, &mut total, 10).is_err());
        assert!(output.len() <= 3);
    }
}

/// Supported archive encoding.
#[derive(Clone, Copy, Debug)]
pub enum ArchiveFormat {
    /// Standard ZIP, including ZIP64. Self-extracting prefixes are unsupported.
    Zip,
    /// Gzip-compressed tar.
    TarGzip,
    /// Zstandard-compressed tar.
    TarZstd,
    /// XZ-compressed tar. Requires the `archive-xz` feature.
    TarXz,
    /// Bzip2-compressed tar. Requires the `archive-bzip2` feature.
    TarBzip2,
}

/// Treatment of symbolic links whose final target does not exist.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DanglingLinks {
    /// Require every link to resolve to an extracted entry.
    #[default]
    Reject,
    /// Preserve missing final components inside existing extracted directories.
    /// Missing intermediate directories and hardlink targets still fail.
    /// Windows creates missing-target links as file symlinks.
    PreserveMissingLeaf,
    /// Opt-in: preserve any symbolic link whose virtual resolution stays
    /// inside the destination, even when intermediate directories are
    /// missing. Escaping targets and dangling hardlinks still fail.
    PreserveUnresolved,
}

/// Treatment of a destination directory that already has contents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExistingDestination {
    /// Require the destination to be absent or empty.
    #[default]
    RequireEmpty,
    /// Opt-in: extract into an existing directory, replacing colliding
    /// non-directory entries. Pre-existing contents are trusted: the caller
    /// must ensure no pre-existing symlink redirects output. Parent
    /// directories are still checked to resolve inside the destination.
    Merge,
}

/// Resource ceilings and link policy, checked before allocation or output where possible.
#[derive(Clone, Copy, Debug)]
pub struct ExtractionLimits {
    /// Maximum source archive size.
    pub max_input_bytes: u64,
    /// Maximum sum of extracted file bytes.
    pub max_output_bytes: u64,
    /// Maximum uncompressed payload bytes in any one non-metadata entry.
    /// Independent of the aggregate output budget; zero permits empty entries.
    /// Tar extension records remain subject to the metadata budget instead.
    pub max_entry_bytes: u64,
    /// Maximum archive entries.
    pub max_entries: u64,
    /// Maximum central-directory metadata bytes.
    pub max_metadata_bytes: u64,
    /// Maximum entry or link path encoding length.
    pub max_path_bytes: usize,
    /// Maximum component visits while resolving the complete link graph.
    pub max_link_steps: u64,
    /// Whether confined dangling symbolic links may be preserved.
    pub dangling_links: DanglingLinks,
    /// Whether a non-empty destination is accepted.
    pub existing_destination: ExistingDestination,
}

impl ExtractionLimits {
    /// Opt-in: the default policy with every byte and entry-count ceiling
    /// removed (input, output, per-entry, entries). Path, metadata and
    /// link-resolution ceilings stay, as they bound parser work rather than
    /// payload size.
    pub fn without_size_caps() -> Self {
        Self {
            max_input_bytes: u64::MAX,
            max_output_bytes: u64::MAX,
            max_entry_bytes: u64::MAX,
            max_entries: u64::MAX,
            ..Self::default()
        }
    }
}

impl Default for ExtractionLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024 * 1024,
            max_output_bytes: 64 * 1024 * 1024 * 1024,
            max_entry_bytes: 64 * 1024 * 1024 * 1024,
            max_entries: 1_000_000,
            max_metadata_bytes: 64 * 1024 * 1024,
            max_path_bytes: 4096,
            max_link_steps: 1_000_000,
            dangling_links: DanglingLinks::Reject,
            existing_destination: ExistingDestination::RequireEmpty,
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

// Translate a backend ZIP error into a facade io::Error that carries only a
// kind and a message, so no backend type is reachable through the result.
fn zip_error(error: zip::result::ZipError) -> io::Error {
    use zip::result::ZipError;
    let kind = match &error {
        ZipError::Io(inner) => return io::Error::new(inner.kind(), inner.to_string()),
        ZipError::InvalidArchive(_) => io::ErrorKind::InvalidData,
        ZipError::FileNotFound => io::ErrorKind::NotFound,
        ZipError::InvalidPassword => io::ErrorKind::PermissionDenied,
        ZipError::UnsupportedArchive(_) | ZipError::CompressionMethodNotSupported(_) => {
            io::ErrorKind::Unsupported
        }
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error.to_string())
}

fn relative_path(name: &str, limit: usize) -> io::Result<PathBuf> {
    if name.len() > limit || name.contains(['\\', ':', '\0']) {
        return Err(invalid("unsafe or oversized archive path"));
    }
    let path = Path::new(name);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(invalid("archive path escapes destination"));
    }
    let path: PathBuf = path
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .collect();
    if path.as_os_str().is_empty() {
        return Err(invalid("empty archive path"));
    }
    Ok(path)
}

fn prepare_destination(dest: &Path, existing: ExistingDestination) -> io::Result<PathBuf> {
    match fs::symlink_metadata(dest) {
        Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
            return Err(invalid("destination is not a real directory"))
        }
        Ok(_)
            if existing == ExistingDestination::RequireEmpty
                && fs::read_dir(dest)?.next().is_some() =>
        {
            return Err(invalid("destination is not empty"))
        }
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(dest)?,
        Err(error) => return Err(error),
    }
    fs::canonicalize(dest)
}

// Create the parent of one output and open the output exclusively. Merge mode
// replaces a colliding non-directory and rejects a parent resolving outside.
fn create_output(root: &Path, output: &Path, existing: ExistingDestination) -> io::Result<File> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
        if existing == ExistingDestination::Merge && !fs::canonicalize(parent)?.starts_with(root) {
            return Err(invalid("archive output parent escapes destination"));
        }
    }
    if existing == ExistingDestination::Merge {
        remove_colliding(output)?;
    }
    OpenOptions::new().write(true).create_new(true).open(output)
}

fn remove_colliding(output: &Path) -> io::Result<()> {
    match fs::symlink_metadata(output) {
        Ok(meta) if meta.is_dir() => Err(invalid("archive entry collides with a directory")),
        Ok(_) => fs::remove_file(output),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

// Whether a prefix the OS would traverse is acceptable. A missing prefix is
// tolerated only for `PreserveUnresolved` soft links.
fn require_directory(path: &Path, tolerate_missing: bool, message: &str) -> io::Result<()> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(invalid(message)),
        Err(error) if tolerate_missing && error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

// Resolve the virtual link graph before placing any link on disk. In
// particular, `alias/..` must expand alias before interpreting the parent.
struct ArchiveLink {
    target: PathBuf,
    hard: bool,
}

fn resolve_link_path(
    root: &Path,
    path: &Path,
    links: &BTreeMap<PathBuf, ArchiveLink>,
    depth: usize,
    steps: &mut u64,
    tolerate_missing: bool,
) -> io::Result<PathBuf> {
    if depth > 64 {
        return Err(invalid("archive link cycle or excessive depth"));
    }
    let mut resolved = PathBuf::new();
    for component in path.components() {
        *steps = steps
            .checked_sub(1)
            .ok_or_else(|| invalid("archive link work limit exceeded"))?;
        // Every prefix traversed by the OS must exist and be a directory,
        // including the prefix discarded by `..`. Links have not been
        // installed yet; graph expansion below resolves them virtually.
        if matches!(component, Component::Normal(_) | Component::ParentDir) {
            require_directory(
                &root.join(&resolved),
                tolerate_missing,
                "archive link traverses a non-directory",
            )?;
        }
        match component {
            Component::CurDir => (),
            Component::ParentDir => {
                if !resolved.pop() {
                    return Err(invalid("archive link escapes destination"));
                }
            }
            Component::Normal(name) => {
                resolved.push(name);
                if let Some(link) = links.get(&resolved) {
                    let parent = if link.hard {
                        Path::new("")
                    } else {
                        resolved.parent().unwrap_or(Path::new(""))
                    };
                    resolved = resolve_link_path(
                        root,
                        &parent.join(&link.target),
                        links,
                        depth + 1,
                        steps,
                        tolerate_missing,
                    )?;
                }
            }
            _ => return Err(invalid("absolute archive link target")),
        }
    }
    // Components removes terminal separators and `/.`, but the operating
    // system requires those literal targets to name directories.
    let raw = path
        .to_str()
        .ok_or_else(|| invalid("non-UTF-8 link path"))?;
    let requires_directory = raw.ends_with('/')
        || raw.ends_with("/.")
        || (cfg!(windows) && (raw.ends_with('\\') || raw.ends_with("\\.")));
    if requires_directory {
        require_directory(
            &root.join(&resolved),
            tolerate_missing,
            "archive link requires a directory target",
        )?;
    }
    Ok(resolved)
}

#[cfg(any(windows, test))]
fn windows_link_target(target: &Path) -> io::Result<PathBuf> {
    // CreateSymbolicLinkW stores relative targets without translating `/`.
    // NT path resolution then rejects those otherwise-valid archive targets.
    // Replace separators only: do not collapse `..`, `.`, or link chains.
    Ok(PathBuf::from(
        target
            .to_str()
            .ok_or_else(|| invalid("non-UTF-8 link target"))?
            .replace('/', "\\"),
    ))
}

#[cfg(test)]
#[test]
fn windows_link_target_preserves_relative_components_and_trailing_separator() {
    assert_eq!(
        windows_link_target(Path::new("../alias/./tool/"))
            .unwrap()
            .as_os_str(),
        std::ffi::OsStr::new("..\\alias\\.\\tool\\")
    );
}

fn install_links(
    root: &Path,
    links: BTreeMap<PathBuf, ArchiveLink>,
    mut steps: u64,
    dangling_links: DanglingLinks,
    existing: ExistingDestination,
) -> io::Result<()> {
    // Parent creation happens before link creation, so none can redirect it.
    for path in links.keys() {
        if let Some(parent) = root.join(path).parent() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut validated = Vec::with_capacity(links.len());
    for (path, link) in &links {
        let output = root.join(path);
        match fs::symlink_metadata(&output) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
            Ok(_) if existing == ExistingDestination::Merge => remove_colliding(&output)?,
            Ok(_) => return Err(invalid("archive link collides with another entry")),
        }
        let tolerate_missing = !link.hard && dangling_links == DanglingLinks::PreserveUnresolved;
        let resolved = resolve_link_path(root, path, &links, 0, &mut steps, tolerate_missing)?;
        let canonical = match fs::canonicalize(root.join(&resolved)) {
            Ok(path) => path,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && !link.hard
                    && dangling_links != DanglingLinks::Reject =>
            {
                root.join(resolved)
            }
            Err(error) => return Err(error),
        };
        if !canonical.starts_with(root) {
            return Err(invalid("archive link escapes destination"));
        }
        if link.hard && !canonical.is_file() {
            return Err(invalid("hardlink target is not a regular file"));
        }
        let is_dir = canonical.is_dir();
        validated.push((output, link, canonical, is_dir));
    }
    for (output, link, canonical, is_dir) in validated {
        if link.hard {
            fs::hard_link(canonical, output)?;
            continue;
        }
        #[cfg(unix)]
        {
            let _ = is_dir;
            std::os::unix::fs::symlink(&link.target, output)?;
        }
        #[cfg(windows)]
        {
            let target = windows_link_target(&link.target)?;
            if is_dir {
                std::os::windows::fs::symlink_dir(target, output)?;
            } else {
                std::os::windows::fs::symlink_file(target, output)?;
            }
        }
    }
    Ok(())
}

/// Extract an archive under explicit ceilings. Does not promote wrapper dirs.
pub fn extract(
    archive: &Path,
    dest: &Path,
    format: ArchiveFormat,
    limits: ExtractionLimits,
) -> io::Result<()> {
    let file = File::open(archive)?;
    extract_file(file, dest, format, limits)
}

// Consume the already-authorized source without reopening a pathname. Future
// authenticated staging must reach this boundary only after verification.
// File ownership itself is not an authentication verdict.
pub(crate) fn extract_file(
    mut file: File,
    dest: &Path,
    format: ArchiveFormat,
    limits: ExtractionLimits,
) -> io::Result<()> {
    if file.metadata()?.len() > limits.max_input_bytes {
        return Err(invalid("archive input exceeds byte limit"));
    }
    file.rewind()?;
    match format {
        ArchiveFormat::Zip => extract_zip(file, dest, limits),
        _ => tar_format::extract_tar(file, dest, format, limits, None),
    }
}

/// Extract one regular tar member without writing other members. The output
/// must not exist. The entire input is validated, including compression EOF.
pub fn extract_member(
    archive: &Path,
    member: &str,
    dest: &Path,
    format: ArchiveFormat,
    limits: ExtractionLimits,
) -> io::Result<()> {
    let member = relative_path(member, limits.max_path_bytes)?;
    let file = File::open(archive)?;
    if file.metadata()?.len() > limits.max_input_bytes {
        return Err(invalid("archive input exceeds byte limit"));
    }
    if matches!(format, ArchiveFormat::Zip) {
        return Err(invalid("selected ZIP member is unsupported"));
    }
    tar_format::extract_tar(file, dest, format, limits, Some(member))
}

// Check advertised ZIP/ZIP64 metadata before the backend sizes its entry vec.
fn preflight_zip(file: &mut File, limits: ExtractionLimits) -> io::Result<()> {
    let length = file.metadata()?.len();
    let tail_length = length.min(65557) as usize;
    file.seek(SeekFrom::End(-(tail_length as i64)))?;
    let mut tail = vec![0; tail_length];
    file.read_exact(&mut tail)?;
    let offset = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| {
            tail[i..].starts_with(b"PK\x05\x06")
                && i + 22 + u16::from_le_bytes([tail[i + 20], tail[i + 21]]) as usize == tail.len()
        })
        .ok_or_else(|| invalid("missing ZIP end directory"))?;
    let end = &tail[offset..];
    if end[4..8] != [0; 4] {
        return Err(invalid("multi-disk ZIP is unsupported"));
    }
    let mut count = u16::from_le_bytes([end[10], end[11]]) as u64;
    let disk_count = u16::from_le_bytes([end[8], end[9]]) as u64;
    if disk_count != count {
        return Err(invalid("inconsistent ZIP directory counts"));
    }
    let mut metadata_size = u32::from_le_bytes(end[12..16].try_into().unwrap()) as u64;
    let directory_offset = u32::from_le_bytes(end[16..20].try_into().unwrap());
    let position = length - tail_length as u64 + offset as u64;
    let mut locator = [0; 20];
    if position >= 20 {
        file.seek(SeekFrom::Start(position - 20))?;
        file.read_exact(&mut locator)?;
    }
    let has_zip64 = locator.starts_with(b"PK\x06\x07");
    if !has_zip64
        && (count == u16::MAX as u64
            || metadata_size == u32::MAX as u64
            || directory_offset == u32::MAX)
    {
        return Err(invalid("missing ZIP64 locator"));
    }
    if has_zip64 {
        if locator[4..8] != [0; 4] || locator[16..20] != 1_u32.to_le_bytes() {
            return Err(invalid("multi-disk ZIP64 is unsupported"));
        }
        let record = u64::from_le_bytes(locator[8..16].try_into().unwrap());
        if record.checked_add(56).is_none_or(|end| end > position - 20) {
            return Err(invalid("ZIP64 record outside archive"));
        }
        file.seek(SeekFrom::Start(record))?;
        let mut header = [0; 56];
        file.read_exact(&mut header)?;
        if !header.starts_with(b"PK\x06\x06") {
            return Err(invalid("bad ZIP64 directory"));
        }
        let record_size = u64::from_le_bytes(header[4..12].try_into().unwrap());
        if record_size < 44
            || record_size > limits.max_metadata_bytes
            || record
                .checked_add(record_size)
                .and_then(|end| end.checked_add(12))
                != Some(position - 20)
        {
            return Err(invalid("invalid or oversized ZIP64 metadata record"));
        }
        if header[16..24] != [0; 8] || header[24..32] != header[32..40] {
            return Err(invalid("inconsistent ZIP64 disks or counts"));
        }
        count = u64::from_le_bytes(header[32..40].try_into().unwrap());
        metadata_size = u64::from_le_bytes(header[40..48].try_into().unwrap());
    }
    if count > limits.max_entries || metadata_size > limits.max_metadata_bytes {
        return Err(invalid("ZIP metadata exceeds resource limits"));
    }
    file.rewind()
}

fn open_zip(
    mut file: File,
    limits: ExtractionLimits,
) -> io::Result<zip::ZipArchive<MetadataBudget>> {
    if file.metadata()?.len() > limits.max_input_bytes {
        return Err(invalid("archive input exceeds byte limit"));
    }
    preflight_zip(&mut file, limits)?;
    let budget = Arc::new(AtomicU64::new(
        limits.max_metadata_bytes.saturating_add(65557),
    ));
    let reader = MetadataBudget {
        file,
        remaining: Arc::clone(&budget),
    };
    let config = zip::read::Config {
        archive_offset: zip::read::ArchiveOffset::Known(0),
    };
    let archive = zip::ZipArchive::with_config(config, reader).map_err(zip_error)?;
    // Metadata parsing is complete. File data is bounded by compressed file
    // size and the independent decompressed-output counter below.
    budget.store(u64::MAX, Ordering::Relaxed);
    if archive.len() as u64 > limits.max_entries {
        return Err(invalid("too many archive entries"));
    }
    Ok(archive)
}

fn extract_zip(file: File, dest: &Path, limits: ExtractionLimits) -> io::Result<()> {
    let mut archive = open_zip(file, limits)?;
    let root = prepare_destination(dest, limits.existing_destination)?;
    let mut remaining = limits.max_output_bytes;
    let mut link_bytes = limits.max_metadata_bytes;
    let mut links = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(zip_error)?;
        if entry.size() > limits.max_entry_bytes {
            return Err(invalid("ZIP entry exceeds byte limit"));
        }
        let name = entry.name().map_err(zip_error)?;
        let relative = relative_path(&name, limits.max_path_bytes)?;
        let output = root.join(&relative);
        if entry.is_symlink() {
            let mut bytes = Vec::new();
            let mut allowed = (limits.max_path_bytes as u64)
                .min(link_bytes)
                .min(remaining)
                .min(limits.max_entry_bytes);
            let before = allowed;
            copy_bounded(&mut entry, &mut bytes, &mut allowed)?;
            link_bytes -= before - allowed;
            remaining -= before - allowed;
            let target =
                String::from_utf8(bytes).map_err(|_| invalid("non-UTF-8 archive link target"))?;
            if target.is_empty() || target.contains([':', '\0']) {
                return Err(invalid("unsafe archive link target"));
            }
            if links
                .insert(
                    relative,
                    ArchiveLink {
                        target: PathBuf::from(target),
                        hard: false,
                    },
                )
                .is_some()
            {
                return Err(invalid("duplicate archive link"));
            }
            continue;
        }
        if entry.is_dir() {
            fs::create_dir_all(output)?;
            continue;
        }
        if entry.size() > remaining {
            return Err(invalid("archive output exceeds byte limit"));
        }
        let mut target = create_output(&root, &output, limits.existing_destination)?;
        copy_entry_bounded(
            &mut entry,
            &mut target,
            &mut remaining,
            limits.max_entry_bytes,
        )?;
        target.flush()?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&output, fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    install_links(
        &root,
        links,
        limits.max_link_steps,
        limits.dangling_links,
        limits.existing_destination,
    )
}
