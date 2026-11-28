use super::*;
use std::io::Cursor;

// Inspect physical tar records before the backend can allocate longname/PAX
// payloads. The only retained payload is one bounded extension record.
struct Guard<R> {
    source: R,
    pending: Cursor<Vec<u8>>,
    body: u64,
    padding: u64,
    pax_size: Option<u64>,
    metadata: u64,
    stream: u64,
    entries: u64,
    max_entry_bytes: u64,
    ended: bool,
}

impl<R: Read> Guard<R> {
    fn new(source: R, limits: ExtractionLimits) -> Self {
        Self {
            source,
            pending: Cursor::new(Vec::new()),
            body: 0,
            padding: 0,
            pax_size: None,
            metadata: limits.max_metadata_bytes,
            stream: limits
                .max_output_bytes
                .saturating_add(limits.max_metadata_bytes),
            entries: limits.max_entries,
            max_entry_bytes: limits.max_entry_bytes,
            ended: false,
        }
    }

    fn read_source(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let allowed = self.stream.saturating_add(1).min(output.len() as u64) as usize;
        let count = loop {
            match self.source.read(&mut output[..allowed]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        if count as u64 > self.stream {
            return Err(invalid("tar decompression byte limit exceeded"));
        }
        self.stream -= count as u64;
        Ok(count)
    }

    fn read_exact_source(&mut self, mut output: &mut [u8]) -> io::Result<()> {
        while !output.is_empty() {
            let count = self.read_source(output)?;
            if count == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            output = &mut output[count..];
        }
        Ok(())
    }

    fn next_header(&mut self) -> io::Result<()> {
        let mut bytes = [0; 512];
        self.read_exact_source(&mut bytes)?;
        if bytes.iter().all(|b| *b == 0) {
            self.ended = true;
            self.pending = Cursor::new(bytes.to_vec());
            return Ok(());
        }
        self.entries = self
            .entries
            .checked_sub(1)
            .ok_or_else(|| invalid("too many tar entries"))?;
        self.metadata = self
            .metadata
            .checked_sub(512)
            .ok_or_else(|| invalid("tar metadata limit exceeded"))?;
        let header = tar::Header::from_byte_slice(&bytes);
        let kind = header.entry_type();
        // The backend returns a global PAX header as an entry, consuming any
        // preceding local extension state. Match that boundary so a stale
        // local size cannot hide later metadata records from this guard.
        if kind.is_pax_global_extensions() {
            self.pax_size = None;
        }
        if kind.is_gnu_sparse() {
            return Err(invalid("GNU sparse tar needs a bounded sparse decoder"));
        }
        let is_extension = kind.is_gnu_longname()
            || kind.is_gnu_longlink()
            || kind.is_pax_local_extensions()
            || kind.is_pax_global_extensions();
        let size = if is_extension {
            header.entry_size()?
        } else {
            self.pax_size.take().unwrap_or(header.entry_size()?)
        };
        let padding = (512 - size % 512) % 512;
        if is_extension {
            if size > 64 * 1024 {
                return Err(invalid("tar extension record exceeds 64 KiB"));
            }
            self.metadata = self
                .metadata
                .checked_sub(size + padding)
                .ok_or_else(|| invalid("tar metadata limit exceeded"))?;
            let mut record = bytes.to_vec();
            record.resize(512 + (size + padding) as usize, 0);
            self.read_exact_source(&mut record[512..])?;
            if kind.is_pax_local_extensions() || kind.is_pax_global_extensions() {
                let mut archive = tar::Archive::new(Cursor::new(&record));
                let mut entries = archive.entries()?.raw(true);
                let mut entry = entries
                    .next()
                    .ok_or_else(|| invalid("missing PAX record"))??;
                if let Some(extensions) = entry.pax_extensions()? {
                    for extension in extensions {
                        let extension = extension?;
                        let key = extension.key().map_err(|_| invalid("non-UTF-8 PAX key"))?;
                        if key.starts_with("GNU.sparse") {
                            return Err(invalid("PAX sparse tar is unsupported"));
                        }
                        if key == "size"
                            && kind.is_pax_local_extensions()
                            && self.pax_size.is_none()
                        {
                            self.pax_size = Some(
                                extension
                                    .value()
                                    .map_err(|_| invalid("non-UTF-8 PAX size"))?
                                    .parse()
                                    .map_err(|_| invalid("invalid PAX size"))?,
                            );
                        }
                    }
                }
            }
            self.pending = Cursor::new(record);
        } else {
            if size > self.max_entry_bytes {
                return Err(invalid("tar entry exceeds byte limit"));
            }
            if size > self.stream {
                return Err(invalid("tar member exceeds decompression limit"));
            }
            self.body = size;
            self.padding = padding;
            self.pending = Cursor::new(bytes.to_vec());
        }
        Ok(())
    }
}

impl<R: Read> Read for Guard<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let count = self.pending.read(output)?;
            if count != 0 {
                return Ok(count);
            }
            if self.ended {
                let count = self.read_source(output)?;
                if output[..count].iter().any(|b| *b != 0) {
                    return Err(invalid("nonzero trailing tar data"));
                }
                return Ok(count);
            }
            let remaining = if self.body != 0 {
                self.body
            } else {
                self.padding
            };
            if remaining != 0 {
                let size = remaining.min(output.len() as u64) as usize;
                let count = self.read_source(&mut output[..size])?;
                if count == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                if self.body != 0 {
                    self.body -= count as u64;
                } else {
                    self.padding -= count as u64;
                }
                return Ok(count);
            }
            self.next_header()?;
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
pub(super) fn extract_tar(
    source: impl Read,
    dest: &Path,
    format: ArchiveFormat,
    limits: ExtractionLimits,
    member: Option<PathBuf>,
) -> io::Result<()> {
    let compression = super::stream::TarCompression::for_format(format)
        .ok_or_else(|| invalid("expected tar encoding"))?;
    let decoder = super::stream::decoder(source, compression)?;
    let root = if member.is_none() {
        Some(prepare_destination(dest, limits.existing_destination)?)
    } else {
        None
    };
    let mut archive = tar::Archive::new(Guard::new(decoder, limits));
    let mut remaining = limits.max_output_bytes;
    let mut found = false;
    let mut links = BTreeMap::new();
    let mut link_bytes = limits.max_metadata_bytes;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() {
            continue;
        }
        let name = entry.path()?;
        let name = name.to_str().ok_or_else(|| invalid("non-UTF-8 tar path"))?;
        if kind.is_dir() && matches!(name, "." | "./") {
            continue;
        }
        let relative = relative_path(name, limits.max_path_bytes)?;
        if !kind.is_file() && !kind.is_dir() && !kind.is_symlink() && !kind.is_hard_link() {
            return Err(invalid("special tar entry is unsupported"));
        }
        if member.as_ref().is_some_and(|wanted| wanted != &relative) {
            continue;
        }
        if kind.is_symlink() || kind.is_hard_link() {
            if member.is_some() {
                return Err(invalid("selected tar member is not a regular file"));
            }
            let target = entry
                .link_name()?
                .ok_or_else(|| invalid("missing tar link target"))?;
            let target = target
                .to_str()
                .ok_or_else(|| invalid("non-UTF-8 tar link target"))?;
            if target.is_empty()
                || target.len() > limits.max_path_bytes
                || target.contains([':', '\0'])
            {
                return Err(invalid("unsafe tar link target"));
            }
            link_bytes = link_bytes
                .checked_sub(target.len() as u64)
                .ok_or_else(|| invalid("tar link metadata limit exceeded"))?;
            let target = if kind.is_hard_link() {
                relative_path(target, limits.max_path_bytes)?
            } else {
                PathBuf::from(target)
            };
            if links
                .insert(
                    relative,
                    ArchiveLink {
                        target,
                        hard: kind.is_hard_link(),
                    },
                )
                .is_some()
            {
                return Err(invalid("duplicate tar link"));
            }
            continue;
        }
        let output = root
            .as_ref()
            .map_or_else(|| dest.to_path_buf(), |root| root.join(relative));
        if kind.is_dir() {
            if member.is_some() {
                return Err(invalid("selected tar member is not a regular file"));
            }
            fs::create_dir_all(output)?;
            continue;
        }
        if entry.size() > remaining {
            return Err(invalid("tar output exceeds byte limit"));
        }
        let mut file = match &root {
            Some(root) => create_output(root, &output, limits.existing_destination)?,
            None => {
                if let Some(parent) = output.parent() {
                    fs::create_dir_all(parent)?;
                }
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&output)?
            }
        };
        copy_entry_bounded(
            &mut entry,
            &mut file,
            &mut remaining,
            limits.max_entry_bytes,
        )?;
        file.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                &output,
                fs::Permissions::from_mode(entry.header().mode()? & 0o777),
            )?;
        }
        found = true;
    }
    // Force codec EOF/checksum and validate trailing padding, even for a
    // selected member near the beginning of the archive.
    io::copy(&mut archive.into_inner(), &mut io::sink())?;
    if let Some(root) = root {
        install_links(
            &root,
            links,
            limits.max_link_steps,
            limits.dangling_links,
            limits.existing_destination,
        )?;
    }
    if member.is_some() && !found {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "tar member not found",
        ));
    }
    Ok(())
}
