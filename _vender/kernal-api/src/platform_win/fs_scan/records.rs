//! Host-testable Windows enumeration and allocation decisions; no syscalls.
use std::io;

pub fn extended(path: &[u16]) -> Vec<u16> {
    if path.starts_with(&[92, 92, 63, 92]) {
        return path.to_vec();
    }
    let mut result: Vec<_> = r"\\?\".encode_utf16().collect();
    if let Some(unc) = path.strip_prefix(&[92, 92]) {
        result.extend(r"UNC\".encode_utf16());
        result.extend(unc);
    } else {
        result.extend(path);
    }
    result
}

pub fn allocation(standard: i64, physical: Option<i64>) -> io::Result<u64> {
    u64::try_from(physical.unwrap_or(standard))
        .map_err(|_| io::Error::other("negative file allocation"))
}

pub fn drive_letter(path: &[u16]) -> Option<u16> {
    match path {
        [92, 92, 63, 92, drive @ (65..=90 | 97..=122), 58, 92] => Some(*drive),
        _ => None,
    }
}

pub fn filename(record: &[u16]) -> io::Result<Option<&[u16]>> {
    let length = record
        .iter()
        .position(|c| *c == 0)
        .ok_or_else(|| io::Error::other("unterminated FindFirstFileExW filename"))?;
    let name = &record[..length];
    if name == [46] || name == [46, 46] {
        Ok(None)
    } else if name.is_empty() || name.iter().any(|c| matches!(*c, 47 | 92)) {
        Err(io::Error::other("invalid FindFirstFileExW filename"))
    } else {
        Ok(Some(name))
    }
}

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "Regression assertions intentionally fail the harness"
)]
mod tests {
    use super::*;

    #[test]
    fn sparse_and_compressed_allocation_uses_physical_size() -> io::Result<()> {
        assert_eq!(allocation(32 * 1024 * 1024, Some(65536))?, 65536);
        assert_eq!(allocation(131072, Some(4096))?, 4096);
        assert_eq!(allocation(65536, Some(0))?, 0);
        assert_eq!(allocation(4096, None)?, 4096);
        assert!(allocation(4096, Some(-1)).is_err());
        assert!(allocation(-1, None).is_err());
        Ok(())
    }

    #[test]
    fn extended_paths_preserve_utf16_units() {
        for prefix in [r"C:\", r"\\server\share\", r"\\?\C:\"] {
            let mut path: Vec<_> = prefix.encode_utf16().collect();
            path.extend([0xD800, 10, 34]);
            let actual = extended(&path);
            assert!(actual.ends_with(&[0xD800, 10, 34]));
            assert_eq!(extended(&actual), actual);
        }
        for (input, expected) in [
            (r"C:\dir", r"\\?\C:\dir"),
            (r"\\server\share", r"\\?\UNC\server\share"),
        ] {
            assert_eq!(
                extended(&input.encode_utf16().collect::<Vec<_>>()),
                expected.encode_utf16().collect::<Vec<_>>()
            );
        }
        assert_eq!(
            drive_letter(&extended(&r"c:\".encode_utf16().collect::<Vec<_>>())),
            Some(99)
        );
        for input in [r"C:\dir", r"\\server\share", r"\\?\1:\"] {
            assert_eq!(
                drive_letter(&extended(&input.encode_utf16().collect::<Vec<_>>())),
                None
            );
        }
    }

    #[test]
    fn find_records_preserve_units_and_skip_only_dot_entries() -> io::Result<()> {
        assert_eq!(filename(&[46, 0, 0xFFFF])?, None);
        assert_eq!(filename(&[46, 46, 0])?, None);
        assert_eq!(filename(&[46, 46, 46, 0])?, Some([46, 46, 46].as_slice()));
        assert_eq!(
            filename(&[0xD800, 10, 34, 0])?,
            Some([0xD800, 10, 34].as_slice())
        );
        for malformed in [&[0][..], &[65], &[65, 47, 66, 0], &[65, 92, 66, 0]] {
            assert!(filename(malformed).is_err());
        }
        Ok(())
    }
}
