//! Decoder for the explicitly requested Darwin attribute set. No Darwin ABI types.
use std::io;

pub const NAME: u32 = 1;
pub const DEVID: u32 = 2;
pub const OBJTYPE: u32 = 8;
pub const FILEID: u32 = 0x0200_0000;
pub const ERROR: u32 = 0x2000_0000;
pub const RETURNED: u32 = 0x8000_0000;
pub const TOTALSIZE: u32 = 0x200; // ATTR_FILE_DATALENGTH (not all-forks TOTALSIZE)
pub const ALLOCSIZE: u32 = 4;
pub const LINKCOUNT: u32 = 1;

#[derive(Debug)]
pub struct BulkEntry<'a> {
    pub name: &'a [u8],
    pub kind: Option<u32>,
    pub device: Option<u32>,
    pub inode: Option<u64>,
    pub links: Option<u32>,
    pub allocated: Option<u64>,
    pub apparent: Option<u64>,
    pub error: i32,
}

fn word<const N: usize>(bytes: &[u8], offset: usize) -> io::Result<[u8; N]> {
    bytes
        .get(offset..offset.checked_add(N).ok_or_else(invalid)?)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(invalid)
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid getattrlistbulk record")
}
pub fn length(bytes: &[u8]) -> io::Result<usize> {
    Ok(u32::from_le_bytes(word(bytes, 0)?) as usize)
}

pub fn parse(bytes: &[u8]) -> io::Result<BulkEntry<'_>> {
    let len = length(bytes)?;
    let bytes = bytes.get(..len).ok_or_else(invalid)?;
    let common = u32::from_le_bytes(word(bytes, 4)?);
    let file = u32::from_le_bytes(word(bytes, 16)?);
    if common & (NAME | RETURNED) != NAME | RETURNED
        || common & !(NAME | DEVID | OBJTYPE | FILEID | ERROR | RETURNED) != 0
        || file & !(LINKCOUNT | TOTALSIZE | ALLOCSIZE) != 0
    {
        return Err(invalid());
    }
    let mut cursor = 24;
    let error = if common & ERROR != 0 {
        let v = i32::from_le_bytes(word(bytes, cursor)?);
        cursor += 4;
        v
    } else {
        0
    };
    let offset = i32::from_le_bytes(word(bytes, cursor)?);
    let start = cursor
        .checked_add_signed(offset as isize)
        .ok_or_else(invalid)?;
    let size = u32::from_le_bytes(word(bytes, cursor + 4)?) as usize;
    let name = bytes
        .get(start..start.checked_add(size).ok_or_else(invalid)?)
        .and_then(|s| s.strip_suffix(&[0]))
        .ok_or_else(invalid)?;
    if name.is_empty() || name.contains(&0) || name.contains(&b'/') {
        return Err(invalid());
    }
    cursor += 8;
    let mut take32 = |mask, bit| -> io::Result<Option<u32>> {
        if mask & bit == 0 {
            return Ok(None);
        }
        let v = u32::from_le_bytes(word(bytes, cursor)?);
        cursor += 4;
        Ok(Some(v))
    };
    let device = take32(common, DEVID)?;
    let kind = take32(common, OBJTYPE)?;
    let inode = if common & FILEID != 0 {
        let v = u64::from_le_bytes(word(bytes, cursor)?);
        cursor += 8;
        Some(v)
    } else {
        None
    };
    let links = if file & LINKCOUNT != 0 {
        let v = u32::from_le_bytes(word(bytes, cursor)?);
        cursor += 4;
        Some(v)
    } else {
        None
    };
    let mut take64 = |bit| -> io::Result<Option<u64>> {
        if file & bit == 0 {
            return Ok(None);
        }
        let v = i64::from_le_bytes(word(bytes, cursor)?);
        cursor += 8;
        if v < 0 {
            return Err(invalid());
        }
        Ok(Some(v as u64))
    };
    let allocated = take64(ALLOCSIZE)?;
    let apparent = take64(TOTALSIZE)?;
    if start < cursor {
        return Err(invalid());
    }
    Ok(BulkEntry {
        name,
        kind,
        device,
        inode,
        links,
        allocated,
        apparent,
        error,
    })
}

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "Native fixture assertions intentionally fail the test harness"
)]
mod tests {
    use super::*;
    #[test]
    fn names_preserve_bytes_with_record_padding_and_returned_masks() -> io::Result<()> {
        for error in [0, ERROR] {
            let reference = if error == 0 { 24 } else { 28 };
            let name = b"bad-\xff\n\"\0";
            let start = reference + 8;
            let length = (start + name.len() + 7) & !7;
            let mut bytes = vec![0xCC; length + 32];
            bytes[..length].fill(0);
            bytes[..4].copy_from_slice(&(length as u32).to_le_bytes());
            bytes[4..8].copy_from_slice(&(RETURNED | NAME | error).to_le_bytes());
            bytes[reference..reference + 4].copy_from_slice(&8i32.to_le_bytes());
            bytes[reference + 4..reference + 8].copy_from_slice(&(name.len() as u32).to_le_bytes());
            bytes[start..start + name.len()].copy_from_slice(name);
            assert_eq!(parse(&bytes)?.name, &name[..name.len() - 1]);
            // The reference cannot borrow a name from the next record.
            bytes[reference + 4..reference + 8].copy_from_slice(&64u32.to_le_bytes());
            assert!(parse(&bytes).is_err());
        }
        Ok(())
    }

    #[test]
    fn packed_record_with_error_slot_and_unaligned_sizes() -> io::Result<()> {
        let mut b = vec![0u8; 72];
        b[0..4].copy_from_slice(&72u32.to_le_bytes());
        b[4..8]
            .copy_from_slice(&(RETURNED | ERROR | NAME | DEVID | OBJTYPE | FILEID).to_le_bytes());
        b[16..20].copy_from_slice(&(LINKCOUNT | TOTALSIZE | ALLOCSIZE).to_le_bytes());
        b[28..32].copy_from_slice(&40i32.to_le_bytes());
        b[32..36].copy_from_slice(&4u32.to_le_bytes());
        b[36..40].copy_from_slice(&7u32.to_le_bytes());
        b[40..44].copy_from_slice(&1u32.to_le_bytes());
        b[44..52].copy_from_slice(&42u64.to_le_bytes());
        b[52..56].copy_from_slice(&2u32.to_le_bytes());
        b[56..64].copy_from_slice(&4096u64.to_le_bytes());
        // Expand to fit allocation + name.
        b.resize(76, 0);
        b[0..4].copy_from_slice(&76u32.to_le_bytes());
        b[28..32].copy_from_slice(&44i32.to_le_bytes());
        b[64..72].copy_from_slice(&999u64.to_le_bytes());
        b[72..76].copy_from_slice(b"abc\0");
        let e = parse(&b)?;
        assert_eq!(
            (
                e.name,
                e.device,
                e.kind,
                e.inode,
                e.links,
                e.apparent,
                e.allocated,
                e.error
            ),
            (
                b"abc".as_slice(),
                Some(7),
                Some(1),
                Some(42),
                Some(2),
                Some(999),
                Some(4096),
                0
            )
        );
        for n in 0..b.len() {
            assert!(parse(&b[..n]).is_err());
        }
        b[28..32].copy_from_slice(&i32::MIN.to_le_bytes());
        assert!(parse(&b).is_err());
        Ok(())
    }
}
