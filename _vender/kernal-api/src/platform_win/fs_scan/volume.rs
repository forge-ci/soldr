use std::collections::{HashMap, HashSet};
use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use windows_sys::Win32::Storage::FileSystem::{ReadFile, SetFilePointerEx, FILE_BEGIN};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_GET_NTFS_FILE_RECORD, FSCTL_GET_NTFS_VOLUME_DATA,
    FSCTL_GET_RETRIEVAL_POINTERS,
};

use super::mft::{self, invalid, u32_at, u64_at, Record};
use super::{ioctl, open, root};
use crate::fs_scan::{EntryKind, Metadata, OwnedEntry as Entry, Summary};

fn read_at(handle: &OwnedHandle, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
    let offset = i64::try_from(offset).map_err(|_| invalid())?;
    if unsafe { SetFilePointerEx(handle.as_raw_handle(), offset, null_mut(), FILE_BEGIN) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut done = 0;
    while done < buffer.len() {
        let mut n = 0;
        if unsafe {
            ReadFile(
                handle.as_raw_handle(),
                buffer[done..].as_mut_ptr(),
                (buffer.len() - done) as u32,
                &raw mut n,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short MFT read",
            ));
        }
        done += n as usize;
    }
    Ok(())
}

pub(super) fn mft(volume: &OwnedHandle, path: &Path) -> io::Result<HashMap<u64, Record>> {
    let mut data = [0u8; 128];
    let size = ioctl(volume, FSCTL_GET_NTFS_VOLUME_DATA, &[], &mut data)?;
    let data = &data[..size];
    let sector = u32_at(data, 40)? as usize;
    let cluster = u64::from(u32_at(data, 44)?);
    let record_size = u32_at(data, 48)? as usize;
    let valid = u64_at(data, 56)?;
    if !(512..=65536).contains(&record_size)
        || cluster == 0
        || sector == 0
        || valid % record_size as u64 != 0
    {
        return Err(invalid());
    }
    let mut first = vec![0u8; record_size + 16];
    let returned = ioctl(
        volume,
        FSCTL_GET_NTFS_FILE_RECORD,
        &0u64.to_le_bytes(),
        &mut first,
    )?;
    if returned < record_size + 12 || u64_at(&first, 0)? & 0x0000_ffff_ffff_ffff != 0 {
        return Err(invalid());
    }
    let mut raw = vec![0u8; record_size];
    read_at(
        volume,
        u64_at(data, 64)?.checked_mul(cluster).ok_or_else(invalid)?,
        &mut raw,
    )?;
    let first = mft::parse(&raw, 0, sector)?.ok_or_else(invalid)?;
    let runs = first.runs;
    if runs.is_empty() {
        return Err(invalid());
    }
    // GET_RETRIEVAL_POINTERS requires a file handle, not the raw volume. NTFS
    // often refuses opening $MFT; the first record's mapping pairs are the fallback.
    if let Ok(mft_handle) = open(
        &path.join("$MFT"),
        windows_sys::Win32::Foundation::GENERIC_READ,
        0,
    ) {
        let mut pointers = vec![0u8; 1024 * 1024];
        if let Ok(n) = ioctl(
            &mft_handle,
            FSCTL_GET_RETRIEVAL_POINTERS,
            &0i64.to_le_bytes(),
            &mut pointers,
        ) {
            if n < 32 || u64_at(&pointers, 24)? != runs[0].lcn {
                return Err(invalid());
            }
        }
    }
    let mut records = HashMap::new();
    let mut logical = 0u64;
    let mut buffer = vec![0u8; 4 * 1024 * 1024];
    let mut pending = Vec::with_capacity(record_size);
    let mut id = 0;
    for run in runs {
        if run.vcn.checked_mul(cluster).ok_or_else(invalid)? != logical {
            return Err(invalid());
        }
        let mut remaining = run
            .clusters
            .checked_mul(cluster)
            .ok_or_else(invalid)?
            .min(valid.saturating_sub(logical));
        let mut offset = run.lcn.checked_mul(cluster).ok_or_else(invalid)?;
        while remaining > 0 {
            let count = remaining.min(buffer.len() as u64) as usize;
            read_at(volume, offset, &mut buffer[..count])?;
            for chunk in buffer[..count].chunks(record_size) {
                pending.extend_from_slice(chunk);
                if pending.len() >= record_size {
                    if let Some(record) = mft::parse(&pending[..record_size], id, sector)? {
                        records.insert(id, record);
                    }
                    pending.drain(..record_size);
                    id += 1;
                }
            }
            offset += count as u64;
            remaining -= count as u64;
            logical += count as u64;
        }
        if logical == valid {
            break;
        }
    }
    if logical != valid || !pending.is_empty() || !records.contains_key(&5) {
        return Err(invalid());
    }
    Ok(records)
}

pub(super) fn emit(
    records: HashMap<u64, Record>,
    root_meta: Metadata,
    sink: &mut dyn FnMut(Entry) -> bool,
) -> io::Result<Option<Summary>> {
    // Validate the complete graph before the first callback; fallback must not emit twice.
    for id in records.keys() {
        mft::path_for(*id, &records, Path::new(""))?;
    }
    let mut summary = Summary::default();
    let mut ids: Vec<_> = records.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        let r = &records[&id];
        let Some(path) = mft::path_for(id, &records, Path::new(""))? else {
            summary.metadata_allocated += r.allocated;
            continue;
        };
        let meta = Metadata {
            device: root_meta.device,
            inode: (u64::from(r.sequence) << 48) | id,
            allocated: r.allocated,
            apparent: r.apparent,
            is_dir: r.directory,
            links: r.links,
            kind: if r.directory {
                EntryKind::Directory
            } else {
                EntryKind::File
            },
            mount: None,
        };
        let parent = if id == 5 {
            meta
        } else {
            Metadata {
                inode: r.parent,
                ..root_meta
            }
        };
        let entry = Entry::new(path, meta, parent.file_identity());
        summary.allocated += r.allocated;
        summary.apparent += r.apparent;
        summary.entries += 1;
        if !sink(entry) {
            summary.cancelled = true;
            break;
        }
    }
    Ok(Some(summary))
}

pub(super) fn usn(
    volume: &OwnedHandle,
    path: &Path,
) -> io::Result<(Vec<(PathBuf, Metadata)>, u64)> {
    let mut input = [0u8; 24];
    input[16..24].copy_from_slice(&i64::MAX.to_le_bytes());
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut names: HashMap<u64, (u64, String)> = HashMap::new();
    loop {
        let n = match ioctl(volume, FSCTL_ENUM_USN_DATA, &input, &mut buffer) {
            Ok(n) => n,
            Err(e) if e.raw_os_error() == Some(38) => break, // ERROR_HANDLE_EOF
            Err(e) => return Err(e),
        };
        if n < 8 {
            return Err(invalid());
        }
        let next = u64_at(&buffer, 0)?;
        if next <= u64_at(&input, 0)? {
            return Err(invalid());
        }
        let mut cursor = 8;
        while cursor < n {
            let (len, id, parent, name) = mft::usn_record(&buffer[cursor..n])?;
            names.insert(id, (parent, name));
            cursor += len;
        }
        input[..8].copy_from_slice(&next.to_le_bytes());
    }
    if names.is_empty() {
        return Err(invalid());
    }
    let root_meta = root(path, false)?;
    let root_id = names
        .keys()
        .find(|id| **id & 0x0000_ffff_ffff_ffff == root_meta.inode & 0x0000_ffff_ffff_ffff)
        .copied()
        .ok_or_else(invalid)?;
    let mut entries = vec![(path.to_path_buf(), root_meta)];
    let mut metadata_allocated = 0u64;
    for id in names.keys().copied() {
        if id == root_id {
            continue;
        }
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut current = id;
        let mut metadata_file = false;
        while current != root_id {
            metadata_file |= mft::reserved(current);
            if !seen.insert(current) {
                return Err(invalid());
            }
            let (parent, name) = names.get(&current).ok_or_else(invalid)?;
            chain.push(name);
            current = *parent;
        }
        let mut p = path.to_path_buf();
        for name in chain.into_iter().rev() {
            p.push(name);
        }
        // USN supplies no sizes. Every identity must be checked via a handle.
        let meta = root(&p, false)?;
        if meta.inode & 0x0000_ffff_ffff_ffff != id & 0x0000_ffff_ffff_ffff {
            return Err(invalid());
        }
        if metadata_file {
            metadata_allocated = metadata_allocated.saturating_add(meta.allocated);
        } else {
            entries.push((p, meta));
        }
    }
    Ok((entries, metadata_allocated))
}
