//! Streaming tar reading, reader-based extraction, and in-memory zstd.
//!
//! Backend archive and codec types stay private: entries are exposed through
//! facade-owned [`TarEntry`] values that implement [`Read`].

use super::{invalid, ArchiveFormat, ExtractionLimits};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

// Largest zstd window accepted by every decoder here (128 MiB), matching the
// bounded extractor. Frames written with `--long=31` are rejected.
const ZSTD_WINDOW_LOG_MAX: u32 = 27;

/// Compression wrapping a tar stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TarCompression {
    /// Uncompressed tar.
    None,
    /// Gzip, including concatenated members.
    Gzip,
    /// Zstandard.
    Zstd,
    /// XZ, including concatenated streams. Requires the `archive-xz` feature.
    Xz,
    /// Bzip2, including concatenated streams. Requires `archive-bzip2`.
    Bzip2,
}

impl TarCompression {
    pub(super) fn for_format(format: ArchiveFormat) -> Option<Self> {
        match format {
            ArchiveFormat::Zip => None,
            ArchiveFormat::TarGzip => Some(Self::Gzip),
            ArchiveFormat::TarZstd => Some(Self::Zstd),
            ArchiveFormat::TarXz => Some(Self::Xz),
            ArchiveFormat::TarBzip2 => Some(Self::Bzip2),
        }
    }
}

#[cfg(not(all(feature = "archive-xz", feature = "archive-bzip2")))]
fn unsupported(feature: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("kernal-api was built without the `{feature}` feature"),
    )
}

pub(super) fn decoder<'a>(
    source: impl Read + 'a,
    compression: TarCompression,
) -> io::Result<Box<dyn Read + 'a>> {
    Ok(match compression {
        TarCompression::None => Box::new(source),
        TarCompression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(source)),
        TarCompression::Zstd => {
            let mut decoder = zstd::stream::read::Decoder::new(source)?;
            decoder.window_log_max(ZSTD_WINDOW_LOG_MAX)?;
            Box::new(decoder)
        }
        TarCompression::Xz => xz_decoder(source)?,
        TarCompression::Bzip2 => bzip2_decoder(source)?,
    })
}

#[cfg(feature = "archive-xz")]
fn xz_decoder<'a>(source: impl Read + 'a) -> io::Result<Box<dyn Read + 'a>> {
    // 256 MiB covers `xz -9e` (64 MiB dictionary) with headroom while still
    // refusing a header that demands unbounded decoder memory.
    const MEMORY_LIMIT_KIB: u32 = 256 * 1024;
    Ok(Box::new(lzma_rust2::XzReader::new_mem_limit(
        source,
        true,
        MEMORY_LIMIT_KIB,
    )))
}

#[cfg(not(feature = "archive-xz"))]
fn xz_decoder<'a>(_source: impl Read + 'a) -> io::Result<Box<dyn Read + 'a>> {
    Err(unsupported("archive-xz"))
}

#[cfg(feature = "archive-bzip2")]
fn bzip2_decoder<'a>(source: impl Read + 'a) -> io::Result<Box<dyn Read + 'a>> {
    Ok(Box::new(bzip2::read::MultiBzDecoder::new(source)))
}

#[cfg(not(feature = "archive-bzip2"))]
fn bzip2_decoder<'a>(_source: impl Read + 'a) -> io::Result<Box<dyn Read + 'a>> {
    Err(unsupported("archive-bzip2"))
}

/// Compress a byte buffer as one zstd frame. The output is a pure function
/// of `data` and `level` (single-threaded, no timestamps).
pub fn zstd_encode(data: &[u8], level: i32) -> io::Result<Vec<u8>> {
    zstd::stream::encode_all(data, level)
}

/// Decompress zstd frames, failing once output would exceed
/// `max_output_bytes`. Pass `u64::MAX` to opt out of the ceiling.
pub fn zstd_decode(data: &[u8], max_output_bytes: u64) -> io::Result<Vec<u8>> {
    let mut decoder = zstd::stream::read::Decoder::new(data)?;
    decoder.window_log_max(ZSTD_WINDOW_LOG_MAX)?;
    let mut output = Vec::new();
    decoder
        .take(max_output_bytes.saturating_add(1))
        .read_to_end(&mut output)?;
    if output.len() as u64 > max_output_bytes {
        return Err(invalid("zstd output exceeds byte limit"));
    }
    Ok(output)
}

// Enforce `max_input_bytes` on a source with no length to inspect up front.
struct InputBudget<R> {
    source: R,
    remaining: u64,
}

impl<R: Read> Read for InputBudget<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let allowed = self.remaining.saturating_add(1).min(buffer.len() as u64) as usize;
        let count = self.source.read(&mut buffer[..allowed])?;
        if count as u64 > self.remaining {
            return Err(invalid("archive input exceeds byte limit"));
        }
        self.remaining -= count as u64;
        Ok(count)
    }
}

/// Extract a tar-family archive from any reader (a byte slice, a pipe, a
/// network body) under the same policy as [`super::extract`]. ZIP needs a
/// seekable file; use [`super::extract`] for it.
pub fn extract_reader(
    source: impl Read,
    dest: &Path,
    format: ArchiveFormat,
    limits: ExtractionLimits,
) -> io::Result<()> {
    if matches!(format, ArchiveFormat::Zip) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ZIP extraction needs a seekable file; use archive::extract",
        ));
    }
    let source = InputBudget {
        source,
        remaining: limits.max_input_bytes,
    };
    super::tar_format::extract_tar(source, dest, format, limits, None)
}

/// Streaming reader over a tar archive in any [`Read`] source.
///
/// Unlike extraction, this applies no path policy: entry paths and link
/// targets are reported as stored, and the caller decides what to trust.
pub struct TarReader<'a> {
    archive: tar::Archive<Box<dyn Read + 'a>>,
}

impl<'a> TarReader<'a> {
    /// Read a tar stream wrapped in `compression`.
    pub fn new(source: impl Read + 'a, compression: TarCompression) -> io::Result<Self> {
        Ok(Self {
            archive: tar::Archive::new(decoder(source, compression)?),
        })
    }

    /// Read an archive held in memory.
    pub fn from_slice(bytes: &'a [u8], compression: TarCompression) -> io::Result<Self> {
        Self::new(bytes, compression)
    }

    /// Iterate entries in archive order. Each entry must be consumed before
    /// the next is requested; unread payload is skipped.
    pub fn entries(&mut self) -> io::Result<TarEntries<'_, 'a>> {
        Ok(TarEntries {
            inner: self.archive.entries()?,
        })
    }
}

/// Iterator over [`TarReader`] entries.
pub struct TarEntries<'r, 'a> {
    inner: tar::Entries<'r, Box<dyn Read + 'a>>,
}

impl<'r, 'a> Iterator for TarEntries<'r, 'a> {
    type Item = io::Result<TarEntry<'r, 'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner
            .next()
            .map(|entry| entry.map(|inner| TarEntry { inner }))
    }
}

/// Kind of a tar entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TarEntryKind {
    /// Regular file (including contiguous files).
    File,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
    /// Hard link.
    HardLink,
    /// Device, FIFO, or another kind not modelled here.
    Other,
}

/// One tar entry; reading it yields the entry payload.
pub struct TarEntry<'r, 'a> {
    inner: tar::Entry<'r, Box<dyn Read + 'a>>,
}

impl TarEntry<'_, '_> {
    /// Path as stored (GNU long names and PAX paths applied).
    pub fn path(&self) -> io::Result<PathBuf> {
        Ok(self.inner.path()?.into_owned())
    }

    /// Entry kind.
    pub fn kind(&self) -> TarEntryKind {
        let kind = self.inner.header().entry_type();
        if kind.is_file() || kind.is_contiguous() {
            TarEntryKind::File
        } else if kind.is_dir() {
            TarEntryKind::Directory
        } else if kind.is_symlink() {
            TarEntryKind::Symlink
        } else if kind.is_hard_link() {
            TarEntryKind::HardLink
        } else {
            TarEntryKind::Other
        }
    }

    /// Payload size in bytes.
    pub fn size(&self) -> u64 {
        self.inner.size()
    }

    /// Permission bits as stored.
    pub fn mode(&self) -> io::Result<u32> {
        self.inner.header().mode()
    }

    /// Link target of a symbolic or hard link, as stored.
    pub fn link_target(&self) -> io::Result<Option<PathBuf>> {
        Ok(self.inner.link_name()?.map(|target| target.into_owned()))
    }
}

impl Read for TarEntry<'_, '_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buffer)
    }
}
