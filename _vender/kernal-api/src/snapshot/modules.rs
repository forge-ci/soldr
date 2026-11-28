//! Loaded-module inventory and in-memory PE section lookup (#635).
//!
//! Unwinding a captured stack needs, for every loaded module, its image base
//! and the address ranges of specific sections — on Windows, `.pdata` and
//! `.xdata` carry the unwind tables. This module supplies that inventory.
//!
//! # Why parse the mapped image rather than the file
//!
//! The module is already mapped into this process, so its headers are directly
//! readable and no file I/O is needed. That matters because this inventory is
//! built to interpret captures taken while threads were suspended: touching
//! the filesystem here would make the capture path depend on disk
//! availability, and a module can be deleted or replaced on disk while still
//! mapped.
//!
//! # What this deliberately does not do
//!
//! No unwinding, and no symbolization. This is the address bookkeeping an
//! unwinder consumes, split out so it can be verified on its own — the ranges
//! it reports are checkable against known function addresses without any
//! unwinder existing yet.

#![allow(unsafe_code)] // Module enumeration and header reads are FFI/raw-pointer work.

use std::ops::Range;

#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAX_MODULE_IMAGE_BYTES: u64 = 512 * 1024 * 1024;

#[cfg(windows)]
use std::io;
#[cfg(windows)]
use winapi::shared::minwindef::{DWORD, HMODULE};
#[cfg(windows)]
use winapi::um::processthreadsapi::GetCurrentProcess;
#[cfg(windows)]
use winapi::um::psapi::{
    EnumProcessModules, GetModuleFileNameExW, GetModuleInformation, MODULEINFO,
};
#[cfg(windows)]
use winapi::um::winnt::{
    IMAGE_DOS_HEADER, IMAGE_DOS_SIGNATURE, IMAGE_NT_HEADERS64, IMAGE_NT_SIGNATURE,
    IMAGE_SECTION_HEADER,
};

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(target_os = "linux")]
struct LoadedElfIdentity {
    load_bias: u64,
    mapped_ranges: Vec<Range<u64>>,
    debug_id: String,
}

#[cfg(target_os = "linux")]
fn loaded_elf_identities() -> Vec<LoadedElfIdentity> {
    unsafe extern "C" fn visit(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        data: *mut libc::c_void,
    ) -> libc::c_int {
        const MAX_NOTE_BYTES: usize = 1024 * 1024;
        let info = unsafe { &*info };
        let out = unsafe { &mut *data.cast::<Vec<LoadedElfIdentity>>() };
        if info.dlpi_phdr.is_null() || info.dlpi_phnum == 0 {
            return 0;
        }
        // libc exposes Elf_Addr as u64 on our 64-bit CI hosts and as a
        // narrower integer on 32-bit Linux; this widening keeps both valid.
        #[allow(clippy::unnecessary_cast)]
        let load_bias = info.dlpi_addr as u64;
        let headers =
            unsafe { std::slice::from_raw_parts(info.dlpi_phdr, usize::from(info.dlpi_phnum)) };
        let mapped_ranges = headers
            .iter()
            .filter(|header| header.p_type == libc::PT_LOAD && header.p_memsz > 0)
            .filter_map(|header| {
                let start = load_bias.checked_add(header.p_vaddr)?;
                let end = start.checked_add(header.p_memsz)?;
                Some(start..end)
            })
            .collect::<Vec<_>>();
        for header in headers {
            if header.p_type != libc::PT_NOTE {
                continue;
            }
            let Ok(length) = usize::try_from(header.p_memsz) else {
                continue;
            };
            if length == 0 || length > MAX_NOTE_BYTES {
                continue;
            }
            let Some(address) = load_bias.checked_add(header.p_vaddr) else {
                continue;
            };
            let Some(note_end) = address.checked_add(length as u64) else {
                continue;
            };
            let is_mapped = headers.iter().any(|load| {
                if load.p_type != libc::PT_LOAD || load.p_flags & libc::PF_R == 0 {
                    return false;
                }
                let Some(start) = load_bias.checked_add(load.p_vaddr) else {
                    return false;
                };
                let Some(end) = start.checked_add(load.p_memsz) else {
                    return false;
                };
                address >= start && note_end <= end
            });
            if address == 0 || !is_mapped {
                continue;
            }
            let notes = unsafe { std::slice::from_raw_parts(address as *const u8, length) };
            if let Some(build_id) = gnu_build_id_from_notes(notes) {
                out.push(LoadedElfIdentity {
                    load_bias,
                    mapped_ranges,
                    debug_id: format!("elf:{}", hex_bytes(build_id)),
                });
                break;
            }
        }
        0
    }

    let mut out = Vec::new();
    unsafe {
        libc::dl_iterate_phdr(
            Some(visit),
            (&mut out as *mut Vec<LoadedElfIdentity>).cast::<libc::c_void>(),
        );
    }
    out
}

#[cfg(target_os = "linux")]
fn gnu_build_id_from_notes(mut notes: &[u8]) -> Option<&[u8]> {
    fn aligned(value: usize) -> Option<usize> {
        value.checked_add(3).map(|value| value & !3)
    }

    while notes.len() >= 12 {
        let name_len = usize::try_from(u32::from_ne_bytes(notes[0..4].try_into().ok()?)).ok()?;
        let desc_len = usize::try_from(u32::from_ne_bytes(notes[4..8].try_into().ok()?)).ok()?;
        let kind = u32::from_ne_bytes(notes[8..12].try_into().ok()?);
        let name_end = 12usize.checked_add(name_len)?;
        let desc_start = 12usize.checked_add(aligned(name_len)?)?;
        let desc_end = desc_start.checked_add(desc_len)?;
        let next = desc_start.checked_add(aligned(desc_len)?)?;
        if next > notes.len() || name_end > notes.len() || desc_end > notes.len() {
            return None;
        }
        if kind == 3 && notes.get(12..name_end)?.starts_with(b"GNU") && desc_len > 0 {
            return notes.get(desc_start..desc_end);
        }
        notes = &notes[next..];
    }
    None
}

#[cfg(windows)]
unsafe fn loaded_pe_debug_info(base: u64, image_size: u64) -> Option<(String, String)> {
    const DEBUG_DIRECTORY_INDEX: usize = 6;
    const IMAGE_DEBUG_TYPE_CODEVIEW: u32 = 2;
    const DEBUG_DIRECTORY_SIZE: usize = 28;

    let dos = unsafe { &*(base as *const IMAGE_DOS_HEADER) };
    if dos.e_magic != IMAGE_DOS_SIGNATURE {
        return None;
    }
    let nt_offset = usize::try_from(dos.e_lfanew).ok()?;
    if nt_offset.checked_add(std::mem::size_of::<IMAGE_NT_HEADERS64>())?
        > usize::try_from(image_size).ok()?
    {
        return None;
    }
    let nt_address = (base as usize).checked_add(nt_offset)?;
    let nt = unsafe { &*(nt_address as *const IMAGE_NT_HEADERS64) };
    if nt.Signature != IMAGE_NT_SIGNATURE {
        return None;
    }
    let directory = nt.OptionalHeader.DataDirectory[DEBUG_DIRECTORY_INDEX];
    let directory_start = u64::from(directory.VirtualAddress);
    let directory_size = usize::try_from(directory.Size).ok()?;
    if directory_start
        .checked_add(directory_size as u64)?
        .gt(&image_size)
    {
        return None;
    }
    let directory_address = base.checked_add(directory_start)?;
    let bytes =
        unsafe { std::slice::from_raw_parts(directory_address as *const u8, directory_size) };
    for entry in bytes.as_chunks::<DEBUG_DIRECTORY_SIZE>().0 {
        let kind = u32::from_le_bytes(entry[12..16].try_into().ok()?);
        if kind != IMAGE_DEBUG_TYPE_CODEVIEW {
            continue;
        }
        let size = usize::try_from(u32::from_le_bytes(entry[16..20].try_into().ok()?)).ok()?;
        let rva = u64::from(u32::from_le_bytes(entry[20..24].try_into().ok()?));
        if size < 24 || rva.checked_add(size as u64)?.gt(&image_size) {
            continue;
        }
        let record_address = base.checked_add(rva)?;
        let record = unsafe { std::slice::from_raw_parts(record_address as *const u8, size) };
        if record.get(..4) != Some(b"RSDS") {
            continue;
        }
        let mut guid: [u8; 16] = record.get(4..20)?.try_into().ok()?;
        guid[0..4].reverse();
        guid[4..6].reverse();
        guid[6..8].reverse();
        let age = u32::from_le_bytes(record.get(20..24)?.try_into().ok()?);
        let path_bytes = record.get(24..)?;
        let path_end = path_bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(path_bytes.len());
        let recorded = String::from_utf8_lossy(&path_bytes[..path_end]);
        let pdb_name = recorded
            .rsplit(['/', '\\'])
            .find(|part| !part.is_empty())?
            .to_owned();
        if pdb_name == "."
            || pdb_name == ".."
            || pdb_name.chars().any(|character| {
                matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            })
        {
            return None;
        }
        return Some((format!("pdb:{}-{age}", hex_bytes(&guid)), pdb_name));
    }
    None
}

#[cfg(target_os = "macos")]
unsafe fn loaded_macho_uuid(header: *const u8) -> Option<[u8; 16]> {
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_UUID: u32 = 0x1b;
    const MACH_HEADER_64_SIZE: usize = 32;
    const MAX_LOAD_COMMAND_BYTES: usize = 1024 * 1024;
    const MAX_LOAD_COMMANDS: u32 = 4096;

    let magic = unsafe { (header.cast::<u32>()).read_unaligned() };
    if magic != MH_MAGIC_64 {
        return None;
    }
    let ncmds = unsafe { header.add(16).cast::<u32>().read_unaligned() };
    let sizeofcmds =
        usize::try_from(unsafe { header.add(20).cast::<u32>().read_unaligned() }).ok()?;
    if ncmds > MAX_LOAD_COMMANDS || sizeofcmds > MAX_LOAD_COMMAND_BYTES {
        return None;
    }
    let commands =
        unsafe { std::slice::from_raw_parts(header.add(MACH_HEADER_64_SIZE), sizeofcmds) };
    let mut offset = 0usize;
    for _ in 0..ncmds {
        let prefix = commands.get(offset..offset.checked_add(8)?)?;
        let command = u32::from_le_bytes(prefix[0..4].try_into().ok()?);
        let size = usize::try_from(u32::from_le_bytes(prefix[4..8].try_into().ok()?)).ok()?;
        if size < 8 || offset.checked_add(size)? > commands.len() {
            return None;
        }
        if command == LC_UUID && size >= 24 {
            return commands.get(offset + 8..offset + 24)?.try_into().ok();
        }
        offset += size;
    }
    None
}

/// One section of a mapped module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// Section name as written in the PE header, e.g. `.text`.
    ///
    /// PE names are 8 bytes and are NOT NUL-terminated when exactly 8 long, so
    /// this is the trimmed form rather than a raw C string.
    pub name: String,
    /// Address range of the section as mapped in this process.
    pub range: Range<u64>,
}

/// A module loaded in this process.
#[derive(Clone, Debug)]
pub struct LoadedModule {
    /// Base address the module is mapped at.
    pub base: u64,
    /// Total mapped size.
    pub size: u64,
    /// Actual mapped address ranges when they differ from `base..base+size`.
    pub(crate) mapped_ranges: Vec<Range<u64>>,
    /// Mapped ranges whose OS protection permits instruction execution.
    #[cfg_attr(any(windows, not(target_arch = "x86_64")), allow(dead_code))]
    pub(crate) executable_ranges: Vec<Range<u64>>,
    /// Full path of the module on disk, when the OS could report it.
    ///
    /// Needed downstream to find the symbol file, which lives beside the
    /// binary. `None` rather than a guess when the query fails: a wrong path
    /// would load a *different* build's symbols and produce confidently wrong
    /// function names.
    pub path: Option<String>,
    /// Build identity observed while this module inventory was taken.
    ///
    /// This is read from the loaded image (or, on Linux, from the exact
    /// device/inode still backing the mapping), so later path replacement
    /// cannot change which symbols the capture expects.
    pub debug_id: Option<String>,
    /// Sanitized native symbol filename captured from the loaded image.
    pub debug_file: Option<String>,
    /// Sections parsed from the mapped headers.
    pub sections: Vec<Section>,
}

impl LoadedModule {
    /// Address range covered by the whole module.
    pub fn range(&self) -> Range<u64> {
        match (self.mapped_ranges.first(), self.mapped_ranges.last()) {
            (Some(first), Some(last)) => first.start..last.end,
            _ => self.base..self.base + self.size,
        }
    }

    /// Whether `address` falls inside this module.
    pub fn contains(&self, address: u64) -> bool {
        if self.mapped_ranges.is_empty() {
            self.range().contains(&address)
        } else {
            self.mapped_ranges
                .iter()
                .any(|range| range.contains(&address))
        }
    }

    /// Whether `address` falls inside an executable mapping for this module.
    #[cfg_attr(any(windows, not(target_arch = "x86_64")), allow(dead_code))]
    pub(crate) fn contains_executable(&self, address: u64) -> bool {
        self.executable_ranges
            .iter()
            .any(|range| range.contains(&address))
    }

    /// Look up a section by name, e.g. `.text` or `.pdata`.
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }
}

/// Read the section table out of a module already mapped at `base`.
///
/// # Safety
///
/// `base` must be the base address of a PE image currently mapped into this
/// process. Callers get that from [`enumerate_modules`], which obtains it from
/// the OS.
#[cfg(windows)]
unsafe fn read_sections(base: u64) -> Option<Vec<Section>> {
    let dos = base as *const IMAGE_DOS_HEADER;
    if (*dos).e_magic != IMAGE_DOS_SIGNATURE {
        return None;
    }

    // e_lfanew is a signed offset from the image base to the NT headers.
    let lfanew = (*dos).e_lfanew;
    if lfanew < 0 {
        return None;
    }
    let nt = (base + lfanew as u64) as *const IMAGE_NT_HEADERS64;
    if (*nt).Signature != IMAGE_NT_SIGNATURE {
        return None;
    }

    let section_count = (*nt).FileHeader.NumberOfSections as usize;
    // The section table follows the optional header, whose size is declared
    // rather than fixed — using size_of::<IMAGE_OPTIONAL_HEADER64>() would
    // silently misread images with a different optional-header size.
    let opt_size = (*nt).FileHeader.SizeOfOptionalHeader as u64;
    let opt_start = base + lfanew as u64 + 4 /* Signature */ + 20 /* FileHeader */;
    let table = (opt_start + opt_size) as *const IMAGE_SECTION_HEADER;

    let mut sections = Vec::with_capacity(section_count);
    for i in 0..section_count {
        let header = &*table.add(i);

        // PE section names occupy exactly 8 bytes and are only NUL-terminated
        // when shorter, so take bytes up to the first NUL rather than assuming
        // one exists.
        let raw = &header.Name;
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let name = String::from_utf8_lossy(&raw[..end]).into_owned();

        let start = base + u64::from(header.VirtualAddress);
        // VirtualSize is the in-memory size; SizeOfRawData is the on-disk one
        // and can differ (BSS-like sections have raw size 0).
        let size = u64::from(unsafe { *header.Misc.VirtualSize() });

        sections.push(Section {
            name,
            range: start..start + size,
        });
    }
    Some(sections)
}

/// Enumerate every module mapped into this process.
#[cfg(windows)]
pub fn enumerate_modules() -> io::Result<Vec<LoadedModule>> {
    let process = unsafe { GetCurrentProcess() };

    // Two-pass: ask how many bytes are needed, then fetch. A single fixed-size
    // pass would silently truncate in a process with many DLLs loaded.
    let mut needed: DWORD = 0;
    let ok = unsafe { EnumProcessModules(process, std::ptr::null_mut(), 0, &mut needed) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    let count = needed as usize / std::mem::size_of::<HMODULE>();
    let mut handles: Vec<HMODULE> = vec![std::ptr::null_mut(); count];
    let mut needed2: DWORD = 0;
    let ok = unsafe {
        EnumProcessModules(
            process,
            handles.as_mut_ptr(),
            (handles.len() * std::mem::size_of::<HMODULE>()) as DWORD,
            &mut needed2,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // A module can load between the two calls; honor the smaller count.
    let usable = (needed2 as usize / std::mem::size_of::<HMODULE>()).min(handles.len());

    let mut modules = Vec::with_capacity(usable);
    for handle in handles.into_iter().take(usable) {
        let mut info: MODULEINFO = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetModuleInformation(
                process,
                handle,
                &mut info,
                std::mem::size_of::<MODULEINFO>() as DWORD,
            )
        };
        if ok == 0 {
            // Unloaded between enumeration and query. Skip rather than fail
            // the whole inventory.
            continue;
        }

        let base = info.lpBaseOfDll as u64;
        let sections = match unsafe { read_sections(base) } {
            Some(s) => s,
            None => continue,
        };
        let (debug_id, debug_file) = unsafe {
            loaded_pe_debug_info(base, u64::from(info.SizeOfImage))
                .map(|(identity, file)| (Some(identity), Some(file)))
                .unwrap_or((None, None))
        };

        modules.push(LoadedModule {
            base,
            size: u64::from(info.SizeOfImage),
            mapped_ranges: Vec::new(),
            executable_ranges: Vec::new(),
            path: unsafe { module_path(process, handle) },
            debug_id,
            debug_file,
            sections,
        });
    }

    Ok(modules)
}

/// Full path of a loaded module, or `None` if the OS would not say.
///
/// # Safety
///
/// `handle` must be a module handle obtained from `process`.
#[cfg(windows)]
unsafe fn module_path(process: winapi::um::winnt::HANDLE, handle: HMODULE) -> Option<String> {
    let mut buffer = [0u16; 32768];
    let len = GetModuleFileNameExW(process, handle, buffer.as_mut_ptr(), buffer.len() as DWORD);
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..len as usize]))
}

/// Find the module containing `address`.
pub fn module_for_address(modules: &[LoadedModule], address: u64) -> Option<&LoadedModule> {
    modules.iter().find(|m| m.contains(address))
}

#[cfg(target_os = "linux")]
fn next_maps_field(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    (!input.is_empty()).then_some((&input[..end], &input[end..]))
}

#[cfg(target_os = "linux")]
struct LinuxImage {
    mapped_ranges: Vec<Range<u64>>,
    executable_ranges: Vec<Range<u64>>,
    path: String,
    device_major: u64,
    device_minor: u64,
    inode: String,
}

#[cfg(target_os = "linux")]
type LinuxImageKey = (String, String, String, u64);

#[cfg(target_os = "linux")]
struct LinuxMapping {
    range: Range<u64>,
    executable: bool,
}

#[cfg(target_os = "linux")]
fn linux_images() -> std::io::Result<Vec<LinuxImage>> {
    use std::collections::BTreeMap;

    // (path, device, inode, load instance) -> individual mapped ranges.
    let mut images: BTreeMap<LinuxImageKey, Vec<LinuxMapping>> = BTreeMap::new();
    for line in std::fs::read_to_string("/proc/self/maps")?.lines() {
        let Some((range, rest)) = next_maps_field(line) else {
            continue;
        };
        let Some((perms, rest)) = next_maps_field(rest) else {
            continue;
        };
        let Some((offset, rest)) = next_maps_field(rest) else {
            continue;
        };
        let Some((dev, rest)) = next_maps_field(rest) else {
            continue;
        };
        let Some((inode, rest)) = next_maps_field(rest) else {
            continue;
        };
        let path = rest.trim_start();
        if !path.starts_with('/') {
            continue;
        }
        if path.ends_with(" (deleted)") {
            // Reopening the same pathname could read a replacement build,
            // producing plausible but wrong unwind rules. A deleted mapping
            // is safer left raw.
            continue;
        }
        let path = path.to_owned();
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end), Ok(offset)) = (
            u64::from_str_radix(start, 16),
            u64::from_str_radix(end, 16),
            u64::from_str_radix(offset, 16),
        ) else {
            continue;
        };
        let candidate_base = start.saturating_sub(offset);
        images
            .entry((path, dev.to_owned(), inode.to_owned(), candidate_base))
            .or_default()
            .push(LinuxMapping {
                range: start..end,
                executable: perms.as_bytes().get(2) == Some(&b'x'),
            });
    }

    Ok(images
        .into_iter()
        .filter_map(|((path, device, inode, _load_bias), mut mappings)| {
            let (major, minor) = device.split_once(':')?;
            let device_major = u64::from_str_radix(major, 16).ok()?;
            let device_minor = u64::from_str_radix(minor, 16).ok()?;
            mappings.sort_by_key(|mapping| mapping.range.start);
            Some(LinuxImage {
                mapped_ranges: mappings
                    .iter()
                    .map(|mapping| mapping.range.clone())
                    .collect(),
                executable_ranges: mappings
                    .into_iter()
                    .filter_map(|mapping| mapping.executable.then_some(mapping.range))
                    .collect(),
                path,
                device_major,
                device_minor,
                inode,
            })
        })
        .collect())
}

#[cfg(target_os = "linux")]
/// Enumerate ELF images mapped in the current Linux process.
pub fn enumerate_modules() -> std::io::Result<Vec<LoadedModule>> {
    use object::{Object, ObjectKind, ObjectSection};
    use std::io::Read as _;

    let mut modules = Vec::new();
    let loaded_identities = loaded_elf_identities();
    for LinuxImage {
        mapped_ranges,
        executable_ranges,
        path,
        device_major,
        device_minor,
        inode,
    } in linux_images()?
    {
        let Some(mapped_start) = mapped_ranges.first().map(|range| range.start) else {
            continue;
        };
        let Some(mapped_end) = mapped_ranges.last().map(|range| range.end) else {
            continue;
        };
        use std::os::unix::fs::MetadataExt as _;

        let Ok(file_handle) = std::fs::File::open(&path) else {
            continue;
        };
        let Ok(metadata) = file_handle.metadata() else {
            continue;
        };
        if u64::from(libc::major(metadata.dev())) != device_major
            || u64::from(libc::minor(metadata.dev())) != device_minor
            || metadata.ino().to_string() != inode
        {
            // The pathname no longer names the object in /proc/self/maps.
            continue;
        }
        if metadata.len() > MAX_MODULE_IMAGE_BYTES {
            continue;
        }
        let mut data = Vec::new();
        if file_handle
            .take(MAX_MODULE_IMAGE_BYTES + 1)
            .read_to_end(&mut data)
            .is_err()
            || data.len() as u64 > MAX_MODULE_IMAGE_BYTES
        {
            // A deleted/replaced mapping remains valid for raw capture but
            // cannot safely provide unwind metadata from disk. Leave it out
            // rather than attribute it to a different build.
            continue;
        }
        let Ok(file) = object::File::parse(data.as_slice()) else {
            continue;
        };
        let Some(loaded_identity) = loaded_identities.iter().find(|identity| {
            identity.mapped_ranges.iter().any(|loaded| {
                mapped_ranges
                    .iter()
                    .any(|mapped| loaded.start < mapped.end && mapped.start < loaded.end)
            })
        }) else {
            continue;
        };
        let base = if file.kind() == ObjectKind::Executable {
            0
        } else {
            loaded_identity.load_bias
        };
        let debug_id = loaded_identity.debug_id.clone();
        let file_debug_id = file
            .build_id()
            .ok()
            .flatten()
            .map(|build_id| format!("elf:{}", hex_bytes(build_id)));
        if file_debug_id.as_deref() != Some(debug_id.as_str()) {
            // Device/inode stability is not enough: an in-place overwrite can
            // preserve both. Never consume section metadata unless the file
            // still carries the build-id observed in the mapped PT_NOTE.
            continue;
        }
        let sections = file
            .sections()
            .filter_map(|section| {
                let name = section.name().ok()?.to_owned();
                let start = base.checked_add(section.address())?;
                let end = start.checked_add(section.size())?;
                Some(Section {
                    name,
                    range: start..end,
                })
            })
            .collect();

        modules.push(LoadedModule {
            base,
            size: mapped_end.saturating_sub(mapped_start),
            mapped_ranges,
            executable_ranges,
            path: Some(path),
            debug_id: Some(debug_id),
            debug_file: None,
            sections,
        });
    }
    modules.sort_by_key(|module| module.base);
    Ok(modules)
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_header(image_index: u32) -> *const libc::c_void;
    fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
    fn _dyld_get_image_name(image_index: u32) -> *const libc::c_char;
}

#[cfg(target_os = "macos")]
fn add_slide(address: u64, slide: isize) -> Option<u64> {
    if slide >= 0 {
        address.checked_add(slide as u64)
    } else {
        address.checked_sub(slide.unsigned_abs() as u64)
    }
}

#[cfg(target_os = "macos")]
/// Enumerate Mach-O images loaded by dyld in the current macOS process.
pub fn enumerate_modules() -> std::io::Result<Vec<LoadedModule>> {
    use object::{Object, ObjectSection, ObjectSegment};
    use std::ffi::CStr;
    use std::io::Read as _;

    let count = unsafe { _dyld_image_count() };
    let mut modules = Vec::with_capacity(count as usize);
    for index in 0..count {
        let name = unsafe { _dyld_get_image_name(index) };
        let header = unsafe { _dyld_get_image_header(index) };
        if name.is_null() || header.is_null() {
            continue;
        }
        let path = unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned();
        let Some(loaded_uuid) = (unsafe { loaded_macho_uuid(header.cast()) }) else {
            continue;
        };
        let Ok(file_handle) = std::fs::File::open(&path) else {
            // Some system images live only in the shared dyld cache. Their
            // raw frames remain unattributed rather than being paired with
            // metadata read from a different file.
            continue;
        };
        let mut data = Vec::new();
        if file_handle
            .take(MAX_MODULE_IMAGE_BYTES + 1)
            .read_to_end(&mut data)
            .is_err()
            || data.len() as u64 > MAX_MODULE_IMAGE_BYTES
        {
            continue;
        }
        let Ok(file) = object::File::parse(data.as_slice()) else {
            continue;
        };
        if file.mach_uuid().ok().flatten() != Some(loaded_uuid) {
            // The path can be replaced while dyld keeps the original image
            // mapped. Only use on-disk sections for the exact loaded UUID.
            continue;
        }
        let slide = unsafe { _dyld_get_image_vmaddr_slide(index) };
        let debug_id = Some(format!("macho:{}", hex_bytes(&loaded_uuid)));
        let base_svma = file.relative_address_base();
        let base = add_slide(base_svma, slide).unwrap_or(header as u64);

        let mut mapped_start = u64::MAX;
        let mut mapped_end = 0u64;
        let mut executable_ranges = Vec::new();
        for segment in file.segments() {
            if segment.name().ok().flatten() == Some("__PAGEZERO") {
                continue;
            }
            let Some(start) = add_slide(segment.address(), slide) else {
                continue;
            };
            let Some(end) = start.checked_add(segment.size()) else {
                continue;
            };
            mapped_start = mapped_start.min(start);
            mapped_end = mapped_end.max(end);
            if matches!(
                segment.flags(),
                object::SegmentFlags::MachO { initprot, .. }
                    if initprot & object::macho::VM_PROT_EXECUTE != 0
            ) {
                executable_ranges.push(start..end);
            }
        }
        if mapped_start == u64::MAX || mapped_end <= base {
            continue;
        }

        let sections = file
            .sections()
            .filter_map(|section| {
                let name = section.name().ok()?.to_owned();
                let start = add_slide(section.address(), slide)?;
                let end = start.checked_add(section.size())?;
                Some(Section {
                    name,
                    range: start..end,
                })
            })
            .collect();
        let _ = mapped_start;
        modules.push(LoadedModule {
            base,
            size: mapped_end - base,
            mapped_ranges: Vec::new(),
            executable_ranges,
            path: Some(path),
            debug_id,
            debug_file: None,
            sections,
        });
    }
    modules.sort_by_key(|module| module.base);
    Ok(modules)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// A distinctive function whose address is used to locate `.text` below.
    #[inline(never)]
    fn landmark() -> u64 {
        // The black_box keeps this from being optimized into nothing, which
        // would make its address meaningless.
        std::hint::black_box(0xD1A6_0057_u64)
    }

    #[test]
    fn enumeration_finds_at_least_the_executable_and_some_dlls() {
        let modules = enumerate_modules().expect("enumerate");
        assert!(
            modules.len() >= 2,
            "expected the exe plus at least one DLL, got {}",
            modules.len()
        );
    }

    #[test]
    fn every_module_reports_a_nonempty_range_and_sections() {
        for m in enumerate_modules().expect("enumerate") {
            assert!(m.base != 0, "module with null base");
            assert!(m.size > 0, "module with zero size at {:#x}", m.base);
            assert!(
                !m.sections.is_empty(),
                "module at {:#x} parsed no sections",
                m.base
            );
        }
    }

    /// The decisive check: a real function's address must land inside the
    /// `.text` range of the module reporting it.
    ///
    /// This verifies the section arithmetic end-to-end without any unwinder —
    /// a wrong base, a wrong optional-header size, or a misread VirtualAddress
    /// all fail here.
    #[test]
    fn text_section_contains_a_known_function_address() {
        let addr = (landmark as fn() -> u64) as usize as u64;
        let modules = enumerate_modules().expect("enumerate");

        let owner = module_for_address(&modules, addr)
            .unwrap_or_else(|| panic!("no module contains {addr:#x}"));

        let text = owner
            .section(".text")
            .unwrap_or_else(|| panic!("module at {:#x} has no .text", owner.base));

        assert!(
            text.range.contains(&addr),
            "function at {addr:#x} is outside its module's .text ({:#x}..{:#x})",
            text.range.start,
            text.range.end
        );
        // Sanity: the landmark still evaluates, so it was not optimized away.
        assert_eq!(landmark(), 0xD1A6_0057_u64);
    }

    #[test]
    fn loaded_pe_owner_carries_its_codeview_identity() {
        let addr = (landmark as fn() -> u64) as usize as u64;
        let modules = enumerate_modules().expect("enumerate");
        let owner = module_for_address(&modules, addr).expect("owning module");
        assert!(
            owner
                .debug_id
                .as_deref()
                .is_some_and(|identity| identity.starts_with("pdb:")),
            "loaded PE did not expose its mapped CodeView GUID+age: {:?}",
            owner.debug_id
        );
    }

    #[test]
    fn module_lookup_rejects_an_address_outside_every_module() {
        let modules = enumerate_modules().expect("enumerate");
        // A deliberately implausible user-mode address.
        assert!(module_for_address(&modules, 0x1).is_none());
    }

    /// Unwinding needs `.pdata`; confirm the inventory actually surfaces it for
    /// the module holding our own code.
    #[test]
    fn own_module_exposes_unwind_sections() {
        let addr = (landmark as fn() -> u64) as usize as u64;
        let modules = enumerate_modules().expect("enumerate");
        let owner = module_for_address(&modules, addr).expect("owning module");

        assert!(
            owner.section(".pdata").is_some(),
            "x86_64 PE modules carry .pdata unwind tables; sections found: {:?}",
            owner.sections.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn sections_do_not_extend_past_their_module() {
        for m in enumerate_modules().expect("enumerate") {
            let module_end = m.base + m.size;
            for s in &m.sections {
                assert!(
                    s.range.start >= m.base && s.range.start <= module_end,
                    "section {} at {:#x} lies outside module {:#x}..{:#x}",
                    s.name,
                    s.range.start,
                    m.base,
                    module_end
                );
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    #[inline(never)]
    fn landmark() {}

    #[test]
    fn maps_path_preserves_spaces_and_deleted_suffix() {
        let line = "1000-2000 r-xp 00000000 08:01 42 /tmp/a file (deleted)";
        let (_, rest) = next_maps_field(line).unwrap();
        let (_, rest) = next_maps_field(rest).unwrap();
        let (_, rest) = next_maps_field(rest).unwrap();
        let (_, rest) = next_maps_field(rest).unwrap();
        let (_, rest) = next_maps_field(rest).unwrap();
        assert_eq!(rest.trim_start(), "/tmp/a file (deleted)");
        assert!(rest.trim_start().ends_with(" (deleted)"));
    }

    #[test]
    fn elf_load_bias_does_not_expand_mapped_coverage_to_zero() {
        let module = LoadedModule {
            base: 0,
            size: 0x2000,
            mapped_ranges: vec![0x400000..0x401000, 0x402000..0x403000],
            executable_ranges: std::iter::once(0x400000..0x401000).collect(),
            path: Some("/tmp/non-pie".into()),
            debug_id: None,
            debug_file: None,
            sections: Vec::new(),
        };
        assert!(module.contains(0x400100));
        assert!(module.contains(0x402100));
        assert!(!module.contains(0x401100));
        assert!(!module.contains(1));
        assert!(module.contains_executable(0x400100));
        assert!(!module.contains_executable(0x402100));
    }

    #[test]
    fn loaded_elf_owner_carries_its_pt_note_build_id() {
        let address = landmark as fn() as usize as u64;
        let modules = enumerate_modules().expect("enumerate");
        let owner = module_for_address(&modules, address).expect("owning module");
        assert!(
            owner
                .debug_id
                .as_deref()
                .is_some_and(|identity| identity.starts_with("elf:")),
            "loaded ELF did not expose its PT_NOTE build-id: {:?}",
            owner.debug_id
        );
    }

    #[test]
    fn gnu_note_parser_extracts_the_build_id() {
        let mut note = Vec::new();
        note.extend_from_slice(&4u32.to_ne_bytes());
        note.extend_from_slice(&4u32.to_ne_bytes());
        note.extend_from_slice(&3u32.to_ne_bytes());
        note.extend_from_slice(b"GNU\0");
        note.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
        assert_eq!(
            gnu_build_id_from_notes(&note),
            Some(&[0xaa, 0xbb, 0xcc, 0xdd][..])
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    #[inline(never)]
    fn landmark() {}

    #[test]
    fn loaded_macho_owner_carries_its_lc_uuid() {
        let address = landmark as fn() as usize as u64;
        let modules = enumerate_modules().expect("enumerate");
        let owner = module_for_address(&modules, address).expect("owning module");
        assert!(
            owner
                .debug_id
                .as_deref()
                .is_some_and(|identity| identity.starts_with("macho:")),
            "loaded Mach-O did not expose its LC_UUID: {:?}",
            owner.debug_id
        );
    }
}

#[cfg(target_os = "linux")]
/// A cheap digest of the images currently mapped into this process.
///
/// Deliberately far cheaper than [`enumerate_modules`], which is why it
/// exists: this reads the mapping table and hashes it, but never opens or
/// parses a module image -- the work that dominates enumeration. Callers use
/// it to decide whether a cached inventory and unwinder are still valid
/// (#131). Any change to the mapped set, its paths, its identities, or its
/// address ranges changes the digest.
pub(crate) fn loaded_image_signature() -> std::io::Result<u64> {
    use std::hash::{Hash as _, Hasher as _};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let images = linux_images()?;
    images.len().hash(&mut hasher);
    for image in &images {
        image.path.hash(&mut hasher);
        image.device_major.hash(&mut hasher);
        image.device_minor.hash(&mut hasher);
        image.inode.hash(&mut hasher);
        for range in &image.mapped_ranges {
            range.start.hash(&mut hasher);
            range.end.hash(&mut hasher);
        }
        for range in &image.executable_ranges {
            range.start.hash(&mut hasher);
            range.end.hash(&mut hasher);
        }
    }
    Ok(hasher.finish())
}

#[cfg(target_os = "macos")]
/// A cheap digest of the images currently loaded into this process.
///
/// Deliberately far cheaper than [`enumerate_modules`], which is why it
/// exists: it asks dyld for the loaded-image list and hashes what it returns,
/// but never opens or parses a Mach-O -- the work that dominates enumeration.
/// Callers use it to decide whether a cached inventory and unwinder are still
/// valid (#131). The image header pointer is included so a dropped and
/// re-loaded image at the same index is still seen as a change.
pub(crate) fn loaded_image_signature() -> std::io::Result<u64> {
    use std::ffi::CStr;
    use std::hash::{Hash as _, Hasher as _};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let count = unsafe { _dyld_image_count() };
    count.hash(&mut hasher);
    for index in 0..count {
        let name = unsafe { _dyld_get_image_name(index) };
        let header = unsafe { _dyld_get_image_header(index) };
        if name.is_null() || header.is_null() {
            continue;
        }
        let path = unsafe { CStr::from_ptr(name) };
        path.to_bytes().hash(&mut hasher);
        (header as u64).hash(&mut hasher);
        unsafe { _dyld_get_image_vmaddr_slide(index) }.hash(&mut hasher);
    }
    Ok(hasher.finish())
}
