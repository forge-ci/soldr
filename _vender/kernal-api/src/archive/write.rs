//! Deterministic tar (plain or zstd) and ZIP writers.
//!
//! Identical inputs produce byte-identical archives: directory walks are
//! sorted by name, timestamps and ownership are fixed, and permission bits are
//! normalized to `0o755` (directories and executables) or `0o644`.

use super::invalid;
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::Path;

const DIR_MODE: u32 = 0o755;

enum TarSink<W: Write> {
    Plain(W),
    Zstd(zstd::stream::write::Encoder<'static, W>),
}

impl<W: Write> Write for TarSink<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(writer) => writer.write(buffer),
            Self::Zstd(writer) => writer.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(writer) => writer.flush(),
            Self::Zstd(writer) => writer.flush(),
        }
    }
}

/// Deterministic tar writer, optionally zstd-compressed.
///
/// Equivalent to a tar builder in deterministic header mode: fixed mtime,
/// zero uid/gid, normalized modes. Symbolic links are stored as links.
pub struct TarWriter<W: Write> {
    builder: tar::Builder<TarSink<W>>,
}

impl<W: Write> TarWriter<W> {
    /// Write an uncompressed tar stream.
    pub fn plain(writer: W) -> Self {
        Self::with_sink(TarSink::Plain(writer))
    }

    /// Write a zstd-compressed tar stream at `level` (single-threaded, so the
    /// compressed bytes are deterministic too).
    pub fn zstd(writer: W, level: i32) -> io::Result<Self> {
        Ok(Self::with_sink(TarSink::Zstd(
            zstd::stream::write::Encoder::new(writer, level)?,
        )))
    }

    fn with_sink(sink: TarSink<W>) -> Self {
        let mut builder = tar::Builder::new(sink);
        builder.mode(tar::HeaderMode::Deterministic);
        builder.follow_symlinks(false);
        Self { builder }
    }

    fn header(kind: tar::EntryType, size: u64, mode: u32) -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(size);
        header.set_mode(mode);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header
    }

    /// Append an in-memory regular file with explicit permission bits.
    pub fn append_bytes(&mut self, archive_path: &str, data: &[u8], mode: u32) -> io::Result<()> {
        let mut header = Self::header(tar::EntryType::Regular, data.len() as u64, mode);
        self.builder.append_data(&mut header, archive_path, data)
    }

    /// Append an empty directory entry.
    pub fn append_dir(&mut self, archive_path: &str) -> io::Result<()> {
        let mut header = Self::header(tar::EntryType::Directory, 0, DIR_MODE);
        self.builder
            .append_data(&mut header, archive_path, io::empty())
    }

    /// Append a symbolic link to `target`, stored verbatim.
    pub fn append_symlink(&mut self, archive_path: &str, target: &str) -> io::Result<()> {
        let mut header = Self::header(tar::EntryType::Symlink, 0, 0o777);
        self.builder.append_link(&mut header, archive_path, target)
    }

    /// Append one filesystem entry (file, directory, or symlink, not
    /// followed) under `archive_path`. Directories are not recursed.
    pub fn append_path(&mut self, archive_path: &str, source: &Path) -> io::Result<()> {
        self.builder.append_path_with_name(source, archive_path)
    }

    /// Append `source` recursively under `archive_prefix` (empty for the
    /// archive root), visiting each directory in byte-wise name order.
    pub fn append_dir_all(&mut self, archive_prefix: &str, source: &Path) -> io::Result<()> {
        walk_sorted(source, archive_prefix, &mut |name, path, kind| match kind {
            WalkKind::Dir => self.append_dir(name),
            _ => self.append_path(name, path),
        })
    }

    /// Write the end-of-archive marker, finish compression, and return the
    /// inner writer.
    pub fn finish(self) -> io::Result<W> {
        match self.builder.into_inner()? {
            TarSink::Plain(writer) => Ok(writer),
            TarSink::Zstd(encoder) => encoder.finish(),
        }
    }
}

/// Compression method for [`ZipWriter`] entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ZipCompression {
    /// No compression.
    Stored,
    /// Deflate (the ZIP default).
    #[default]
    Deflated,
}

/// Deterministic ZIP writer: fixed 1980-01-01 timestamps and normalized
/// Unix permission bits.
pub struct ZipWriter<W: Write + Seek> {
    inner: zip::ZipWriter<W>,
    compression: ZipCompression,
}

impl<W: Write + Seek> ZipWriter<W> {
    /// Start a ZIP archive at the writer's current position.
    pub fn new(writer: W, compression: ZipCompression) -> Self {
        Self {
            inner: zip::ZipWriter::new(writer),
            compression,
        }
    }

    fn options(&self, mode: u32) -> zip::write::SimpleFileOptions {
        let method = match self.compression {
            ZipCompression::Stored => zip::CompressionMethod::Stored,
            ZipCompression::Deflated => zip::CompressionMethod::Deflated,
        };
        zip::write::SimpleFileOptions::default()
            .compression_method(method)
            .last_modified_time(zip::DateTime::default())
            .unix_permissions(mode)
            .large_file(false)
    }

    /// Append an in-memory regular file with explicit permission bits.
    pub fn append_bytes(&mut self, archive_path: &str, data: &[u8], mode: u32) -> io::Result<()> {
        self.start(archive_path, mode)?;
        self.inner.write_all(data)
    }

    fn start(&mut self, archive_path: &str, mode: u32) -> io::Result<()> {
        let options = self.options(mode);
        self.inner
            .start_file(archive_path, options)
            .map_err(super::zip_error)
    }

    /// Append a regular file read from disk. Its mode is normalized to
    /// `0o755` when executable on the host, otherwise `0o644`.
    pub fn append_file(&mut self, archive_path: &str, source: &Path) -> io::Result<()> {
        let mut file = File::open(source)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(invalid("ZIP source is not a regular file"));
        }
        // The tar backend owns host-specific executable detection; reuse its
        // deterministic normalization rather than inspecting OS permissions.
        let mut probe = tar::Header::new_gnu();
        probe.set_metadata_in_mode(&meta, tar::HeaderMode::Deterministic);
        self.start(archive_path, probe.mode()?)?;
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                return Ok(());
            }
            self.inner.write_all(&buffer[..count])?;
        }
    }

    /// Append a directory entry.
    pub fn append_dir(&mut self, archive_path: &str) -> io::Result<()> {
        let options = self.options(DIR_MODE);
        self.inner
            .add_directory(archive_path, options)
            .map_err(super::zip_error)
    }

    /// Append a symbolic link to `target`, stored verbatim.
    pub fn append_symlink(&mut self, archive_path: &str, target: &str) -> io::Result<()> {
        let options = self.options(0o777);
        self.inner
            .add_symlink(archive_path, target, options)
            .map_err(super::zip_error)
    }

    /// Append `source` recursively under `archive_prefix` (empty for the
    /// archive root), visiting each directory in byte-wise name order.
    /// Symbolic links are stored as links, never followed.
    pub fn append_dir_all(&mut self, archive_prefix: &str, source: &Path) -> io::Result<()> {
        walk_sorted(source, archive_prefix, &mut |name, path, kind| match kind {
            WalkKind::Dir => self.append_dir(name),
            WalkKind::File => self.append_file(name, path),
            WalkKind::Symlink => {
                let target = fs::read_link(path)?;
                let target = target
                    .to_str()
                    .ok_or_else(|| invalid("non-UTF-8 symlink target"))?
                    .replace('\\', "/");
                self.append_symlink(name, &target)
            }
        })
    }

    /// Write the central directory and return the inner writer.
    pub fn finish(self) -> io::Result<W> {
        self.inner.finish().map_err(super::zip_error)
    }
}

#[derive(Clone, Copy)]
enum WalkKind {
    Dir,
    File,
    Symlink,
}

// Depth-first walk of `root`'s contents in sorted order, reporting archive
// names joined with `/` under `prefix`. The root itself is not reported.
fn walk_sorted(
    root: &Path,
    prefix: &str,
    visit: &mut dyn FnMut(&str, &Path, WalkKind) -> io::Result<()>,
) -> io::Result<()> {
    let mut children = fs::read_dir(root)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    children.sort();
    for child in children {
        let utf8 = child
            .to_str()
            .ok_or_else(|| invalid("non-UTF-8 file name"))?;
        let name = if prefix.is_empty() {
            utf8.to_owned()
        } else {
            format!("{}/{utf8}", prefix.trim_end_matches('/'))
        };
        let path = root.join(&child);
        let file_type = fs::symlink_metadata(&path)?.file_type();
        if file_type.is_symlink() {
            visit(&name, &path, WalkKind::Symlink)?;
        } else if file_type.is_dir() {
            visit(&name, &path, WalkKind::Dir)?;
            walk_sorted(&path, &name, visit)?;
        } else if file_type.is_file() {
            visit(&name, &path, WalkKind::File)?;
        } else {
            return Err(invalid("special file cannot be archived"));
        }
    }
    Ok(())
}
