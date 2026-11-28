//! Locating the best image inside an `.ico` blob (#577).
//!
//! # This parses untrusted input
//!
//! An icon supplied as bytes may be anything — a truncated download, a file
//! that is not an icon at all, or something crafted. Every field that becomes
//! an offset or a length is therefore checked against the actual buffer before
//! use, and a blob that does not describe a valid image is *refused* rather
//! than passed to the OS with a length the OS will trust.
//!
//! Handing `CreateIconFromResourceEx` an offset past the end of the buffer
//! would have it read whatever follows in our address space.
//!
//! # Why we choose the image rather than the OS
//!
//! `LoadImage` picks from a file on disk; there is no equivalent that takes a
//! whole `.ico` from memory. `CreateIconFromResourceEx` wants the bytes of one
//! image, so the directory has to be walked to find which.

/// Size of the `ICONDIR` header: reserved, type, count — 2 bytes each.
const ICONDIR_LEN: usize = 6;
/// Size of one `ICONDIRENTRY`.
const ICONDIRENTRY_LEN: usize = 16;
/// `ICONDIR::idType` for icons. 2 would be a cursor, which is a different
/// thing wearing the same layout.
const TYPE_ICON: u16 = 1;

/// Why an icon blob could not be used.
#[derive(Debug, PartialEq, Eq)]
pub enum IcoError {
    /// Too small to contain a directory, or truncated mid-entry.
    Truncated,
    /// Not an icon: bad reserved field or wrong type.
    NotAnIcon,
    /// The directory claims no images.
    NoImages,
    /// An entry's offset or length falls outside the buffer.
    EntryOutOfBounds,
}

impl std::fmt::Display for IcoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Truncated => "icon data is truncated",
            Self::NotAnIcon => "data is not an icon (bad ICONDIR header)",
            Self::NoImages => "icon directory contains no images",
            Self::EntryOutOfBounds => "an icon entry points outside the supplied data",
        };
        f.write_str(text)
    }
}

/// The byte range of the chosen image within the blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageSpan {
    /// Offset of the image data.
    pub offset: usize,
    /// Length of the image data.
    pub len: usize,
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Find the largest image in an `.ico` blob.
///
/// Largest by pixel dimensions, then by colour depth. A window icon is scaled
/// down by the OS, so starting from the biggest image gives the best result at
/// whatever size is actually drawn; picking the first entry would often give a
/// 16×16 that looks blurred in the taskbar.
pub fn best_image(bytes: &[u8]) -> Result<ImageSpan, IcoError> {
    if bytes.len() < ICONDIR_LEN {
        return Err(IcoError::Truncated);
    }
    // idReserved must be 0 and idType must be 1. Checking both is what
    // separates "an icon" from "any file that happens to start with 6 bytes".
    if u16_at(bytes, 0) != 0 || u16_at(bytes, 2) != TYPE_ICON {
        return Err(IcoError::NotAnIcon);
    }
    let count = u16_at(bytes, 4) as usize;
    if count == 0 {
        return Err(IcoError::NoImages);
    }

    // The directory itself must fit before any entry is read.
    let directory_end = ICONDIR_LEN
        .checked_add(
            count
                .checked_mul(ICONDIRENTRY_LEN)
                .ok_or(IcoError::Truncated)?,
        )
        .ok_or(IcoError::Truncated)?;
    if bytes.len() < directory_end {
        return Err(IcoError::Truncated);
    }

    let mut best: Option<(u32, u16, ImageSpan)> = None;
    for index in 0..count {
        let entry = ICONDIR_LEN + index * ICONDIRENTRY_LEN;
        // 0 in the width/height byte means 256 — the field is one byte and
        // 256 does not fit. Treating it as 0 would rank the largest image
        // last.
        let width = match bytes[entry] {
            0 => 256u32,
            w => u32::from(w),
        };
        let height = match bytes[entry + 1] {
            0 => 256u32,
            h => u32::from(h),
        };
        let bit_count = u16_at(bytes, entry + 6);
        let len = u32_at(bytes, entry + 8) as usize;
        let offset = u32_at(bytes, entry + 12) as usize;

        // Every entry is bounds-checked even though only one is used: a blob
        // whose entries do not fit is malformed, and quietly skipping the bad
        // ones would let a crafted file steer the choice.
        let end = offset.checked_add(len).ok_or(IcoError::EntryOutOfBounds)?;
        if len == 0 || end > bytes.len() || offset < directory_end {
            return Err(IcoError::EntryOutOfBounds);
        }

        let pixels = width * height;
        let candidate = (pixels, bit_count, ImageSpan { offset, len });
        match &best {
            Some((best_pixels, best_depth, _))
                if (*best_pixels, *best_depth) >= (pixels, bit_count) => {}
            _ => best = Some(candidate),
        }
    }

    best.map(|(_, _, span)| span).ok_or(IcoError::NoImages)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an `.ico` with `entries` of `(width, height, bit_count, payload)`.
    fn ico(entries: &[(u8, u8, u16, &[u8])]) -> Vec<u8> {
        let count = entries.len();
        let mut out = Vec::new();
        out.extend_from_slice(&0u16.to_le_bytes()); // reserved
        out.extend_from_slice(&TYPE_ICON.to_le_bytes());
        out.extend_from_slice(&(count as u16).to_le_bytes());

        let mut offset = ICONDIR_LEN + count * ICONDIRENTRY_LEN;
        let mut payloads = Vec::new();
        for (w, h, bits, data) in entries {
            out.push(*w);
            out.push(*h);
            out.push(0); // colour count
            out.push(0); // reserved
            out.extend_from_slice(&1u16.to_le_bytes()); // planes
            out.extend_from_slice(&bits.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += data.len();
            payloads.push(*data);
        }
        for data in payloads {
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn a_single_image_is_found() {
        let bytes = ico(&[(32, 32, 32, b"IMAGEDATA")]);
        let span = best_image(&bytes).expect("valid icon");
        assert_eq!(&bytes[span.offset..span.offset + span.len], b"IMAGEDATA");
    }

    #[test]
    fn the_largest_image_wins() {
        let bytes = ico(&[
            (16, 16, 32, b"small"),
            (64, 64, 32, b"BIGGEST"),
            (32, 32, 32, b"mid"),
        ]);
        let span = best_image(&bytes).unwrap();
        assert_eq!(&bytes[span.offset..span.offset + span.len], b"BIGGEST");
    }

    /// A zero width/height byte means 256, not 0 — the field cannot hold 256.
    #[test]
    fn a_zero_dimension_means_256_and_therefore_wins() {
        let bytes = ico(&[(64, 64, 32, b"sixtyfour"), (0, 0, 32, b"TWOFIFTYSIX")]);
        let span = best_image(&bytes).unwrap();
        assert_eq!(
            &bytes[span.offset..span.offset + span.len],
            b"TWOFIFTYSIX",
            "a 0 dimension byte encodes 256 and should outrank 64x64"
        );
    }

    #[test]
    fn colour_depth_breaks_a_size_tie() {
        let bytes = ico(&[(32, 32, 4, b"shallow"), (32, 32, 32, b"DEEPEST")]);
        let span = best_image(&bytes).unwrap();
        assert_eq!(&bytes[span.offset..span.offset + span.len], b"DEEPEST");
    }

    #[test]
    fn empty_input_is_truncated_not_a_panic() {
        assert_eq!(best_image(&[]), Err(IcoError::Truncated));
        assert_eq!(best_image(&[0, 0, 1]), Err(IcoError::Truncated));
    }

    #[test]
    fn a_non_icon_is_refused() {
        // Right length, wrong type: this is a cursor.
        let mut bytes = ico(&[(32, 32, 32, b"data")]);
        bytes[2] = 2;
        assert_eq!(best_image(&bytes), Err(IcoError::NotAnIcon));

        // Non-zero reserved field.
        let mut bytes = ico(&[(32, 32, 32, b"data")]);
        bytes[0] = 9;
        assert_eq!(best_image(&bytes), Err(IcoError::NotAnIcon));
    }

    #[test]
    fn a_directory_claiming_no_images_is_refused() {
        let bytes = ico(&[]);
        assert_eq!(best_image(&bytes), Err(IcoError::NoImages));
    }

    /// The check that matters: a length running past the buffer must be
    /// refused, not handed to the OS to read.
    #[test]
    fn an_entry_running_past_the_buffer_is_refused() {
        let mut bytes = ico(&[(32, 32, 32, b"data")]);
        let len_at = ICONDIR_LEN + 8;
        bytes[len_at..len_at + 4].copy_from_slice(&0xFFFF_u32.to_le_bytes());
        assert_eq!(best_image(&bytes), Err(IcoError::EntryOutOfBounds));
    }

    #[test]
    fn an_entry_offset_past_the_buffer_is_refused() {
        let mut bytes = ico(&[(32, 32, 32, b"data")]);
        let offset_at = ICONDIR_LEN + 12;
        bytes[offset_at..offset_at + 4].copy_from_slice(&0xFFFF_u32.to_le_bytes());
        assert_eq!(best_image(&bytes), Err(IcoError::EntryOutOfBounds));
    }

    /// An offset pointing back into the directory would make the image
    /// overlap its own metadata — malformed, and a way to confuse a decoder.
    #[test]
    fn an_offset_inside_the_directory_is_refused() {
        let mut bytes = ico(&[(32, 32, 32, b"data")]);
        let offset_at = ICONDIR_LEN + 12;
        bytes[offset_at..offset_at + 4].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(best_image(&bytes), Err(IcoError::EntryOutOfBounds));
    }

    #[test]
    fn a_zero_length_entry_is_refused() {
        let mut bytes = ico(&[(32, 32, 32, b"data")]);
        let len_at = ICONDIR_LEN + 8;
        bytes[len_at..len_at + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(best_image(&bytes), Err(IcoError::EntryOutOfBounds));
    }

    /// A count larger than the data is the classic overflow lure.
    #[test]
    fn a_count_exceeding_the_buffer_is_truncated() {
        let mut bytes = ico(&[(32, 32, 32, b"data")]);
        bytes[4..6].copy_from_slice(&1000u16.to_le_bytes());
        assert_eq!(best_image(&bytes), Err(IcoError::Truncated));
    }

    /// Arbitrary bytes must never panic — this runs the parser over many
    /// malformed inputs derived from a valid one.
    #[test]
    fn corrupted_icons_never_panic() {
        let original = ico(&[(16, 16, 32, b"aa"), (32, 32, 32, b"bbbb")]);
        for index in 0..original.len() {
            for replacement in [0u8, 0xFF, 0x7F] {
                let mut bytes = original.clone();
                bytes[index] = replacement;
                // Any outcome is acceptable; a panic is not.
                let _ = best_image(&bytes);
            }
            // Truncation at every length, too.
            let _ = best_image(&original[..index]);
        }
    }
}
