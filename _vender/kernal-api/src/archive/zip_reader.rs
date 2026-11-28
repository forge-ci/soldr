//! Private entry-streaming seam for the authenticated archive experiment.
//! Uses the native extractor's preflight, metadata ceiling, and copy bounds.
use super::{
    copy_entry_bounded, invalid, open_zip, relative_path, ExtractionLimits, MetadataBudget,
};
use std::fs::File;
use std::io::{self, Write};

pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) bytes: u64,
}

pub(super) struct Reader {
    archive: zip::ZipArchive<MetadataBudget>,
    limits: ExtractionLimits,
    remaining: u64,
    poisoned: bool,
}

impl Reader {
    pub(super) fn new(file: File, limits: ExtractionLimits) -> io::Result<Self> {
        Ok(Self {
            archive: open_zip(file, limits)?,
            limits,
            remaining: limits.max_output_bytes,
            poisoned: false,
        })
    }

    pub(super) fn entry(&mut self, index: usize) -> io::Result<Option<Entry>> {
        if self.poisoned {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if index >= self.archive.len() {
            return Ok(None);
        }
        let entry = self.archive.by_index(index).map_err(super::zip_error)?;
        validate_entry(&entry, self.limits)?;
        // Name length is bounded before the guest-facing record is allocated.
        Ok(Some(Entry {
            name: entry.name().map_err(super::zip_error)?.into_owned(),
            bytes: entry.size(),
        }))
    }

    /// Copy on a blocking worker into a capacity-awaited sink. No borrowed ZIP
    /// entry or native path escapes. Only one copy can own the reader at once.
    pub(super) fn copy_entry(&mut self, index: usize, sink: &mut impl Write) -> io::Result<u64> {
        if self.poisoned {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        // The shared extraction helper stops on error. In particular, a
        // partially successful write_all need not debit its partial bytes.
        // Fail closed rather than allowing a retry with reusable accounting.
        self.poisoned = true;
        let mut entry = self.archive.by_index(index).map_err(super::zip_error)?;
        validate_entry(&entry, self.limits)?;
        if entry.size() > self.remaining {
            return Err(invalid("archive output exceeds byte limit"));
        }
        let before = self.remaining;
        let expected = entry.size();
        copy_entry_bounded(
            &mut entry,
            sink,
            &mut self.remaining,
            self.limits.max_entry_bytes,
        )?;
        let copied = before - self.remaining;
        if copied != expected {
            return Err(invalid("ZIP entry size disagrees with its metadata"));
        }
        self.poisoned = false;
        Ok(copied)
    }
}

fn validate_entry<R: io::Read + ?Sized>(
    entry: &zip::read::ZipFile<'_, R>,
    limits: ExtractionLimits,
) -> io::Result<()> {
    // Initial streamed-entry capability is regular-files only. Links, dirs,
    // and special entries must not silently turn into ordinary byte streams.
    let kind = entry.unix_mode().unwrap_or(0) & 0o170000;
    if entry.is_dir() || entry.is_symlink() || !matches!(kind, 0 | 0o100000) {
        return Err(invalid("unsupported streamed ZIP entry kind"));
    }
    if entry.size() > limits.max_entry_bytes {
        return Err(invalid("ZIP entry exceeds byte limit"));
    }
    relative_path(
        &entry.name().map_err(super::zip_error)?,
        limits.max_path_bytes,
    )?;
    Ok(())
}
