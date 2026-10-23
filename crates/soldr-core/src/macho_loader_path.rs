//! Does a Mach-O binary load libraries relative to itself? (soldr#1300)
//!
//! The macOS counterpart of [`crate::elf_origin`], used by
//! [`crate::self_relocate::exe_has_loader_path_reference`].

/// `@loader_path` is the only Mach-O prefix that is relative to the
/// *image being loaded*. `@executable_path` resolves against the main
/// executable and `@rpath` against `LC_RPATH` entries, which is why
/// neither is checked here: moving the file does not by itself break them.
const LOADER_PATH_TOKEN: &[u8] = b"@loader_path";

pub(crate) fn macho_load_commands_mention_loader_path(bytes: &[u8]) -> bool {
    // Universal ("fat") binary: a big-endian table of slices, each a
    // complete Mach-O. Any slice needing the trampoline condemns the file,
    // since we cannot know which slice will be executed.
    const FAT_MAGIC: u32 = 0xcafe_babe;
    const FAT_MAGIC_64: u32 = 0xcafe_babf;
    if let Some(magic) = read_u32(bytes, 0, false) {
        if magic == FAT_MAGIC || magic == FAT_MAGIC_64 {
            let Some(nfat) = read_u32(bytes, 4, false) else {
                return false;
            };
            // fat_arch is cputype, cpusubtype, offset, size, align (5 x u32);
            // fat_arch_64 widens offset/size to u64 and adds a reserved
            // word. In both layouts `offset` starts at byte 8.
            let entry_size = if magic == FAT_MAGIC { 20 } else { 32 };
            const OFFSET_FIELD: usize = 8;
            // Cap the arch count so a corrupt header cannot spin.
            for i in 0..nfat.min(64) {
                let base = 8 + (i as usize) * entry_size;
                let off = if magic == FAT_MAGIC {
                    read_u32(bytes, base + OFFSET_FIELD, false).map(|v| v as usize)
                } else {
                    read_u64(bytes, base + OFFSET_FIELD, false).map(|v| v as usize)
                };
                let Some(off) = off else { continue };
                if let Some(slice) = bytes.get(off..) {
                    if thin_macho_mentions_loader_path(slice) {
                        return true;
                    }
                }
            }
            return false;
        }
    }
    thin_macho_mentions_loader_path(bytes)
}

fn thin_macho_mentions_loader_path(bytes: &[u8]) -> bool {
    const MH_MAGIC: u32 = 0xfeed_face; // 32-bit, host-endian
    const MH_CIGAM: u32 = 0xcefa_edfe; // 32-bit, byte-swapped
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const MH_CIGAM_64: u32 = 0xcffa_edfe;

    let Some(raw) = read_u32(bytes, 0, true) else {
        return false;
    };
    // `swapped` means the file's fields are big-endian relative to us.
    let (is_64, swapped) = match raw {
        MH_MAGIC_64 => (true, false),
        MH_CIGAM_64 => (true, true),
        MH_MAGIC => (false, false),
        MH_CIGAM => (false, true),
        _ => return false,
    };

    // mach_header: magic, cputype, cpusubtype, filetype, ncmds,
    // sizeofcmds, flags [, reserved on 64-bit].
    let Some(sizeofcmds) = read_u32(bytes, 20, !swapped) else {
        return false;
    };
    let header_len: usize = if is_64 { 32 } else { 28 };
    let end = header_len.saturating_add(sizeofcmds as usize);
    let Some(region) = bytes.get(header_len..end.min(bytes.len())) else {
        return false;
    };
    region
        .windows(LOADER_PATH_TOKEN.len())
        .any(|w| w == LOADER_PATH_TOKEN)
}

/// Read a `u32`, `little` selecting the interpretation. Returns `None`
/// rather than panicking on a short buffer so a truncated file is simply
/// "not position-dependent" instead of a crash in a shim writer.
fn read_u32(bytes: &[u8], at: usize, little: bool) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(if little {
        u32::from_le_bytes(raw)
    } else {
        u32::from_be_bytes(raw)
    })
}

fn read_u64(bytes: &[u8], at: usize, little: bool) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(at..at + 8)?.try_into().ok()?;
    Some(if little {
        u64::from_le_bytes(raw)
    } else {
        u64::from_be_bytes(raw)
    })
}
