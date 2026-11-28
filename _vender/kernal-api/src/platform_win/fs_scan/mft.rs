//! Bounds-checked NTFS FILE and mapping-pair decoders, tested on every host.
use std::io;

#[derive(Debug, Clone)]
pub struct Record {
    pub id: u64,
    pub sequence: u16,
    pub links: u64,
    pub parent: u64,
    pub name: String,
    pub allocated: u64,
    pub apparent: u64,
    pub directory: bool,
    pub runs: Vec<Run>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub vcn: u64,
    pub lcn: u64,
    pub clusters: u64,
}

pub fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "unsupported or corrupt NTFS record; use directory fallback",
    )
}
fn bytes<const N: usize>(b: &[u8], p: usize) -> io::Result<[u8; N]> {
    b.get(p..p.checked_add(N).ok_or_else(invalid)?)
        .and_then(|v| v.try_into().ok())
        .ok_or_else(invalid)
}
pub fn u16_at(b: &[u8], p: usize) -> io::Result<u16> {
    Ok(u16::from_le_bytes(bytes(b, p)?))
}
pub fn u32_at(b: &[u8], p: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(bytes(b, p)?))
}
pub fn u64_at(b: &[u8], p: usize) -> io::Result<u64> {
    Ok(u64::from_le_bytes(bytes(b, p)?))
}

pub fn runlist(bytes: &[u8], start_vcn: u64) -> io::Result<Vec<Run>> {
    let mut runs = Vec::new();
    let mut cursor = 0;
    let mut lcn = 0i64;
    let mut vcn = start_vcn;
    loop {
        let header = *bytes.get(cursor).ok_or_else(invalid)?;
        cursor += 1;
        if header == 0 {
            return Ok(runs);
        }
        let n = (header & 15) as usize;
        let m = (header >> 4) as usize;
        if n == 0 || n > 8 || m == 0 || m > 8 {
            return Err(invalid());
        }
        let a = bytes.get(cursor..cursor + n).ok_or_else(invalid)?;
        cursor += n;
        let b = bytes.get(cursor..cursor + m).ok_or_else(invalid)?;
        cursor += m;
        let mut count = [0u8; 8];
        count[..n].copy_from_slice(a);
        let clusters = u64::from_le_bytes(count);
        let mut delta = [if b[m - 1] & 128 != 0 { 255 } else { 0 }; 8];
        delta[..m].copy_from_slice(b);
        lcn = lcn
            .checked_add(i64::from_le_bytes(delta))
            .ok_or_else(invalid)?;
        if clusters == 0 || lcn < 0 {
            return Err(invalid());
        }
        runs.push(Run {
            vcn,
            lcn: lcn as u64,
            clusters,
        });
        vcn = vcn.checked_add(clusters).ok_or_else(invalid)?;
    }
}

pub fn fixup(record: &mut [u8], sector: usize) -> io::Result<()> {
    if record.get(..4) != Some(b"FILE") || sector < 256 || !record.len().is_multiple_of(sector) {
        return Err(invalid());
    }
    let offset = usize::from(u16_at(record, 4)?);
    let count = usize::from(u16_at(record, 6)?);
    if count != record.len() / sector + 1 || offset < 8 || offset + count * 2 > sector - 2 {
        return Err(invalid());
    }
    let sequence = u16_at(record, offset)?;
    for i in 1..count {
        let end = i * sector - 2;
        if u16_at(record, end)? != sequence {
            return Err(invalid());
        }
        let replacement = u16_at(record, offset + i * 2)?;
        record[end..end + 2].copy_from_slice(&replacement.to_le_bytes());
    }
    Ok(())
}

pub fn parse(raw: &[u8], id: u64, sector: usize) -> io::Result<Option<Record>> {
    // Unused slots may be zeroed and do not need update-sequence validation.
    if raw.iter().all(|b| *b == 0) {
        return Ok(None);
    }
    if u16_at(raw, 22)? & 1 == 0 {
        return Ok(None);
    }
    let mut b = raw.to_vec();
    fixup(&mut b, sector)?;
    if u64_at(&b, 32)? != 0 {
        return Err(invalid());
    } // extension: force complete fallback
    let used = u32_at(&b, 24)? as usize;
    let b = b.get(..used).ok_or_else(invalid)?;
    let mut result = Record {
        id,
        sequence: u16_at(b, 16)?,
        links: u64::from(u16_at(b, 18)?),
        parent: 0,
        name: String::new(),
        allocated: 0,
        apparent: 0,
        directory: u16_at(b, 22)? & 2 != 0,
        runs: Vec::new(),
    };
    let mut cursor = usize::from(u16_at(b, 20)?);
    let mut namespace = 0;
    let mut has_data = false;
    if cursor < 48 {
        return Err(invalid());
    }
    loop {
        let kind = u32_at(b, cursor)?;
        if kind == u32::MAX {
            break;
        }
        let len = u32_at(b, cursor + 4)? as usize;
        if len < 24 || !len.is_multiple_of(8) {
            return Err(invalid());
        }
        let a = b
            .get(cursor..cursor.checked_add(len).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        match kind {
            0x20 | 0xc0 => return Err(invalid()), // ATTRIBUTE_LIST/reparse: preserve correctness by walking
            0x30 => filename(a, &mut result, &mut namespace)?,
            0x80 | 0xa0 => {
                if kind == 0x80 && a[9] != 0 {
                    return Err(invalid());
                } // named DATA streams require native coverage first
                if kind == 0x80 {
                    has_data = true;
                }
                data(a, &mut result, kind == 0x80)?;
            }
            _ => {}
        }
        cursor += len;
    }
    if result.name.is_empty() || (!result.directory && !has_data) {
        return Err(invalid());
    }
    Ok(Some(result))
}

fn filename(a: &[u8], r: &mut Record, preferred: &mut u8) -> io::Result<()> {
    if a[8] != 0 {
        return Err(invalid());
    }
    let start = usize::from(u16_at(a, 20)?);
    let size = u32_at(a, 16)? as usize;
    let v = a
        .get(start..start.checked_add(size).ok_or_else(invalid)?)
        .ok_or_else(invalid)?;
    let n = *v.get(64).ok_or_else(invalid)? as usize;
    let ns = *v.get(65).ok_or_else(invalid)?;
    if ns == 2 {
        return Ok(());
    } // DOS-only alias
    if ns > 3 {
        return Err(invalid());
    }
    let rank = if ns == 1 || ns == 3 { 2 } else { 1 };
    if rank <= *preferred {
        return Ok(());
    }
    let name = v.get(66..66 + n * 2).ok_or_else(invalid)?;
    let utf16: Vec<u16> = name
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    r.name = String::from_utf16(&utf16).map_err(|_| invalid())?;
    if r.name.is_empty()
        || r.name.contains(['\\', '/', '\0'])
        || (r.name == "." && r.id != 5)
        || r.name == ".."
    {
        return Err(invalid());
    }
    r.parent = u64_at(v, 0)?;
    *preferred = rank;
    Ok(())
}

fn data(a: &[u8], r: &mut Record, is_data: bool) -> io::Result<()> {
    if a[8] == 0 {
        let size = u64::from(u32_at(a, 16)?);
        let offset = usize::from(u16_at(a, 20)?);
        a.get(offset..offset.checked_add(size as usize).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        if is_data {
            r.apparent = r.apparent.checked_add(size).ok_or_else(invalid)?;
        }
        return Ok(());
    }
    let flags = u16_at(a, 12)?;
    let allocated = u64_at(a, if flags & 0x8001 != 0 { 64 } else { 40 })?;
    r.allocated = r.allocated.checked_add(allocated).ok_or_else(invalid)?;
    if is_data {
        r.apparent = r.apparent.checked_add(u64_at(a, 48)?).ok_or_else(invalid)?;
    }
    if r.id == 0 {
        let off = usize::from(u16_at(a, 32)?);
        r.runs
            .extend(runlist(a.get(off..).ok_or_else(invalid)?, u64_at(a, 16)?)?);
    }
    Ok(())
}

pub fn usn_record(b: &[u8]) -> io::Result<(usize, u64, u64, String)> {
    let length = u32_at(b, 0)? as usize;
    let b = b.get(..length).ok_or_else(invalid)?;
    if length < 60 || !length.is_multiple_of(8) || u16_at(b, 4)? != 2 {
        return Err(invalid());
    }
    let name_length = usize::from(u16_at(b, 56)?);
    let offset = usize::from(u16_at(b, 58)?);
    if offset < 60 || !name_length.is_multiple_of(2) {
        return Err(invalid());
    }
    let name = b
        .get(offset..offset.checked_add(name_length).ok_or_else(invalid)?)
        .ok_or_else(invalid)?;
    let utf16: Vec<_> = name
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let name = String::from_utf16(&utf16).map_err(|_| invalid())?;
    if name.is_empty()
        || name.contains(['\\', '/', '\0'])
        || (name == "." && u64_at(b, 8)? & 0x0000_ffff_ffff_ffff != 5)
        || name == ".."
    {
        return Err(invalid());
    }
    Ok((length, u64_at(b, 8)?, u64_at(b, 16)?, name))
}

/// NTFS reserves the first 16 slots; record 5 is the user-visible volume root.
pub fn reserved(id: u64) -> bool {
    let id = id & 0x0000_ffff_ffff_ffff;
    id < 16 && id != 5
}

/// Resolve the emitted path, excluding actual NTFS metadata, including $Extend
/// descendants. User dollar names such as $RECYCLE.BIN remain visible.
pub fn path_for(
    id: u64,
    records: &std::collections::HashMap<u64, Record>,
    root: &std::path::Path,
) -> io::Result<Option<std::path::PathBuf>> {
    let mut current = id;
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while current != 5 {
        if reserved(current) {
            return Ok(None);
        }
        if !seen.insert(current) {
            return Err(invalid());
        }
        let r = records.get(&current).ok_or_else(invalid)?;
        chain.push(r.name.as_str());
        current = r.parent & 0x0000_ffff_ffff_ffff;
        let parent = records.get(&current).ok_or_else(invalid)?;
        if !parent.directory || parent.sequence != (r.parent >> 48) as u16 {
            return Err(invalid());
        }
    }
    let mut path = root.to_path_buf();
    for name in chain.into_iter().rev() {
        path.push(name);
    }
    Ok(Some(path))
}

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "Native fixture assertions intentionally fail the test harness"
)]
mod tests {
    use super::*;
    #[test]
    fn volume_root_dot_and_user_dollar_names() -> io::Result<()> {
        let mut b = fixture();
        b[22..24].copy_from_slice(&3u16.to_le_bytes());
        b[144] = 1;
        b[146..148].copy_from_slice(&(b'.' as u16).to_le_bytes());
        b[160..164].copy_from_slice(&0x90u32.to_le_bytes());
        let root = parse(&b, 5, 512)?.ok_or_else(invalid)?;
        assert_eq!(root.name, ".");
        assert!(root.directory);
        assert!(parse(&b, 42, 512).is_err());
        let mut user = root.clone();
        user.id = 42;
        user.name = "$RECYCLE.BIN".into();
        user.parent = (u64::from(root.sequence) << 48) | 5;
        let mut metadata = root.clone();
        metadata.id = 11;
        metadata.name = "$Extend".into();
        let mut extension = user.clone();
        extension.id = 43;
        extension.name = "$UsnJrnl".into();
        extension.parent = (u64::from(root.sequence) << 48) | 11;
        let records = std::collections::HashMap::from([
            (5, root),
            (42, user),
            (11, metadata),
            (43, extension),
        ]);
        let base = std::path::Path::new("volume");
        assert_eq!(path_for(5, &records, base)?, Some(base.to_path_buf()));
        assert_eq!(
            path_for(42, &records, base)?,
            Some(base.join("$RECYCLE.BIN"))
        );
        assert_eq!(path_for(11, &records, base)?, None);
        assert_eq!(path_for(43, &records, base)?, None);
        Ok(())
    }

    #[test]
    fn nonresident_allocation_and_physical_sparse_size() -> io::Result<()> {
        for (flags, allocated, apparent, physical) in [
            (0u16, 4096u64, 900u64, 0u64),
            (0x8000, 1 << 20, 1 << 20, 4096),
            (1, 65536, 32768, 8192),
        ] {
            let mut b = fixture();
            b[160..248].fill(0);
            let a = &mut b[160..248];
            a[..4].copy_from_slice(&0x80u32.to_le_bytes());
            a[4..8].copy_from_slice(&88u32.to_le_bytes());
            a[8] = 1;
            a[12..14].copy_from_slice(&flags.to_le_bytes());
            a[32..34].copy_from_slice(&72u16.to_le_bytes());
            a[40..48].copy_from_slice(&allocated.to_le_bytes());
            a[48..56].copy_from_slice(&apparent.to_le_bytes());
            a[64..72].copy_from_slice(&physical.to_le_bytes());
            a[72..76].copy_from_slice(&[0x11, 2, 42, 0]);
            b[248..252].copy_from_slice(&u32::MAX.to_le_bytes());
            b[24..28].copy_from_slice(&256u32.to_le_bytes());
            let r = parse(&b, 42, 512)?.ok_or_else(invalid)?;
            assert_eq!(r.allocated, if flags == 0 { allocated } else { physical });
            assert_eq!(r.apparent, apparent);
            let mft = parse(&b, 0, 512)?.ok_or_else(invalid)?;
            assert_eq!(
                mft.runs,
                vec![Run {
                    vcn: 0,
                    lcn: 42,
                    clusters: 2
                }]
            );
        }
        Ok(())
    }
    fn fixture() -> Vec<u8> {
        let mut b = vec![0u8; 1024];
        b[..4].copy_from_slice(b"FILE");
        b[4..6].copy_from_slice(&48u16.to_le_bytes());
        b[6..8].copy_from_slice(&3u16.to_le_bytes());
        b[16..18].copy_from_slice(&7u16.to_le_bytes());
        b[20..22].copy_from_slice(&56u16.to_le_bytes());
        b[22..24].copy_from_slice(&1u16.to_le_bytes());
        b[24..28].copy_from_slice(&224u32.to_le_bytes());
        b[48..50].copy_from_slice(&0xabceu16.to_le_bytes());
        for end in [510, 1022] {
            b[end..end + 2].copy_from_slice(&0xabceu16.to_le_bytes());
        }
        let a = &mut b[56..160];
        a[0..4].copy_from_slice(&0x30u32.to_le_bytes());
        a[4..8].copy_from_slice(&104u32.to_le_bytes());
        a[16..20].copy_from_slice(&80u32.to_le_bytes());
        a[20..22].copy_from_slice(&24u16.to_le_bytes());
        a[24..32].copy_from_slice(&((3u64 << 48) | 5).to_le_bytes());
        a[88] = 4;
        a[89] = 1;
        for (i, c) in "test".encode_utf16().enumerate() {
            a[90 + i * 2..92 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        let a = &mut b[160..216];
        a[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        a[4..8].copy_from_slice(&56u32.to_le_bytes());
        a[16..20].copy_from_slice(&5u32.to_le_bytes());
        a[20..22].copy_from_slice(&24u16.to_le_bytes());
        a[24..29].copy_from_slice(b"hello");
        b[216..220].copy_from_slice(&u32::MAX.to_le_bytes());
        b
    }
    #[test]
    fn constructed_resident_file_and_torn_sector() -> io::Result<()> {
        let mut b = fixture();
        let r = parse(&b, 42, 512)?.ok_or_else(invalid)?;
        assert_eq!(
            (
                r.id,
                r.sequence,
                r.parent,
                r.name.as_str(),
                r.allocated,
                r.apparent,
                r.directory,
                r.runs.len()
            ),
            (42, 7, (3u64 << 48) | 5, "test", 0, 5, false, 0)
        );
        b[510] = 0;
        assert!(parse(&b, 42, 512).is_err());
        Ok(())
    }
    #[test]
    fn mapping_pairs_signed_deltas_and_malformed_lengths() -> io::Result<()> {
        assert_eq!(
            runlist(&[0x11, 3, 20, 0x11, 2, 0xfd, 0], 0)?,
            vec![
                Run {
                    vcn: 0,
                    lcn: 20,
                    clusters: 3
                },
                Run {
                    vcn: 3,
                    lcn: 17,
                    clusters: 2
                }
            ]
        );
        for b in [&[0x11, 3][..], &[0x10, 1, 0], &[0x91, 1, 0], &[0x01, 1, 0]] {
            assert!(runlist(b, 0).is_err());
        }
        Ok(())
    }
    #[test]
    fn truncation_attribute_list_and_extension_fail_closed() {
        let b = fixture();
        for n in 0..1024 {
            assert!(parse(&b[..n], 42, 512).is_err() || n == 0);
        }
        let mut b = fixture();
        b[56..60].copy_from_slice(&0x20u32.to_le_bytes());
        assert!(parse(&b, 42, 512).is_err());
        let mut b = fixture();
        b[32] = 1;
        assert!(parse(&b, 42, 512).is_err());
    }

    #[test]
    fn usn_v2_names_and_versions() -> io::Result<()> {
        let mut b = vec![0u8; 64];
        b[..4].copy_from_slice(&64u32.to_le_bytes());
        b[4..6].copy_from_slice(&2u16.to_le_bytes());
        b[8..16].copy_from_slice(&42u64.to_le_bytes());
        b[16..24].copy_from_slice(&5u64.to_le_bytes());
        b[56..58].copy_from_slice(&2u16.to_le_bytes());
        b[58..60].copy_from_slice(&60u16.to_le_bytes());
        b[60] = b'a';
        assert_eq!(usn_record(&b)?, (64, 42, 5, "a".into()));
        for n in 0..64 {
            assert!(usn_record(&b[..n]).is_err());
        }
        b[8..16].copy_from_slice(&5u64.to_le_bytes());
        b[60] = b'.';
        assert_eq!(usn_record(&b)?.3, ".");
        b[8..16].copy_from_slice(&42u64.to_le_bytes());
        assert!(usn_record(&b).is_err());
        b[60] = b'a';
        b[4] = 3;
        assert!(usn_record(&b).is_err());
        Ok(())
    }
}
