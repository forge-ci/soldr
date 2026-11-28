//! ELF/DWARF and Mach-O/dSYM discovery and function lookup (#638).
//!
//! The parser lives in the disposable worker for the same reason as the PDB
//! parser: malformed debug data must never enter the long-lived daemon.

use std::path::{Path, PathBuf};

use object::{Object as _, ObjectSection as _, ObjectSymbol as _};

use crate::discovery::{self, ResolveOutcome};
use crate::line_numbers::line_numbers_requested;
use kernal_api::symbolize::wire::{DiscoveryConfig, DiscoverySource, ModuleRef};

/// A verified ELF or Mach-O symbol source.
pub enum ModuleSymbols {
    /// Verified identity and a usable symbol table.
    Found {
        /// Parsed function symbols.
        table: SymbolTable,
        /// DWARF line records, when line resolution was asked for (#803).
        ///
        /// `None` means either the opt-in was off or the image carries no
        /// line program — both ordinary, and both degrade to name-only
        /// resolution rather than failing the capture.
        lines: Option<LineTable>,
        /// Verified local path or server URL.
        symbol_file: String,
        /// Winning discovery tier.
        source: DiscoverySource,
    },
    /// No candidate existed.
    NotFound,
    /// Candidates existed but failed exact identity or parsing.
    Mismatched {
        /// Number of rejected candidates.
        rejected: usize,
    },
    /// The module had no supported build identity.
    NoDebugDirectory,
}

/// Ordered function starts normalized to module-relative addresses.
pub struct SymbolTable {
    entries: Vec<(u64, String)>,
}

impl SymbolTable {
    /// Parse the symbol table straight out of an object file.
    ///
    /// Public so tests outside this module can build one — the discovery path
    /// verifies build identity, which a synthesised capture cannot satisfy,
    /// and that verification is not what a resolution test is checking.
    #[cfg(test)]
    pub fn from_object_path(path: &Path) -> Option<Self> {
        let bytes = read_bounded(path)?;
        let file = object::File::parse(&*bytes).ok()?;
        Self::from_file(&file)
    }

    fn from_file(file: &object::File<'_>) -> Option<Self> {
        // Capture offsets are relative to the same image base used by the
        // module inventory: zero/load-bias for ELF and the Mach-O relative
        // address base for dyld images. Subtracting the first text section
        // would incorrectly shift every ELF symbol by its section offset.
        let image_base = file.relative_address_base();
        let mut entries = file
            .symbols()
            .chain(file.dynamic_symbols())
            .filter(|symbol| {
                symbol.kind() == object::SymbolKind::Text
                    && symbol.address() >= image_base
                    && !symbol.is_undefined()
            })
            .filter_map(|symbol| {
                Some((
                    symbol.address() - image_base,
                    symbol.name().ok()?.to_string(),
                ))
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|(address, _)| *address);
        entries.dedup_by_key(|(address, _)| *address);
        (!entries.is_empty()).then_some(Self { entries })
    }

    /// Resolve the containing function by module-relative address.
    pub fn lookup(&self, relative_address: u64) -> Option<&str> {
        let index = match self
            .entries
            .binary_search_by_key(&relative_address, |(address, _)| *address)
        {
            Ok(exact) => exact,
            Err(0) => return None,
            Err(next) => next - 1,
        };
        Some(self.entries[index].1.as_str())
    }
}

/// A module's DWARF line records, for resolving a frame to `file:line` (#803).
///
/// Built separately from [`SymbolTable`] on purpose. Function names come from
/// a symbol-table walk; line numbers require parsing `.debug_line`, which
/// costs materially more. #803 wants that behind an opt-in, so the two cannot
/// share a constructor.
pub struct LineTable {
    /// `(start, end_exclusive, file, line)`, sorted by start.
    ///
    /// Ranges, not points. DWARF tells us how far each record extends, and
    /// keeping that is what lets an address past the last line resolve to
    /// nothing instead of being attributed to whatever came before it.
    entries: Vec<(u64, u64, String, u32)>,
}

impl LineTable {
    /// Build a line table from an object file on disk.
    ///
    /// `None` when the image carries no usable DWARF line program — a
    /// dependency built with `debug = false`, or a stripped binary. That is an
    /// ordinary outcome and degrades to name-only resolution.
    pub fn from_path(path: &Path) -> Option<Self> {
        // `Loader` rather than a bare `Context`: it owns the mapped bytes, so
        // there is no self-referential borrow to arrange.
        let loader = addr2line::Loader::new(path).ok()?;
        // Records come back in the object's own address space. A capture
        // carries `(module, offset)` pairs, so everything is normalised
        // against the same base the module inventory uses. Skipping this
        // yields line numbers that look plausible and are silently shifted —
        // the worst failure available in a diagnostic tool.
        let base = loader.relative_address_base();

        let mut entries: Vec<(u64, u64, String, u32)> = Vec::new();
        // Walk the ranges DWARF declares rather than probing on a stride: any
        // stride either misses short lines or wastes work on long ones.
        let mut ranges = loader.find_location_range(base, u64::MAX).ok()?;
        for (start, length, location) in ranges.by_ref() {
            let (Some(file), Some(line)) = (location.file, location.line) else {
                continue;
            };
            let relative = start.saturating_sub(base);
            entries.push((relative, relative + length.max(1), file.to_string(), line));
        }

        if entries.is_empty() {
            return None;
        }
        entries.sort_unstable_by_key(|(start, _, _, _)| *start);
        entries.dedup_by_key(|(start, _, _, _)| *start);
        Some(Self { entries })
    }

    /// The `(file, line)` for a module-relative address.
    ///
    /// Containment, not nearest-preceding. A return address lands inside the
    /// range a record opened, so the preceding record is the right answer —
    /// but only if the address is actually within it. Unlike
    /// [`SymbolTable::lookup`], which has no end addresses to work with, this
    /// can tell "past the end of all code" from "inside the last function",
    /// and says so.
    pub fn lookup(&self, relative_address: u64) -> Option<(&str, u32)> {
        let index = match self
            .entries
            .binary_search_by_key(&relative_address, |(start, _, _, _)| *start)
        {
            Ok(exact) => exact,
            Err(0) => return None,
            Err(next) => next - 1,
        };
        let (start, end, file, line) = &self.entries[index];
        (relative_address >= *start && relative_address < *end).then_some((file.as_str(), *line))
    }

    /// How many line records were loaded.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Discover and parse exact-build symbols for one ELF or Mach-O module.
pub fn discover_module(module: &ModuleRef, config: &DiscoveryConfig) -> ModuleSymbols {
    if module.path_hint.is_none() && module.debug_id.is_none() {
        return ModuleSymbols::NotFound;
    }
    if !crate::discovery::captured_image_still_matches(module) {
        return ModuleSymbols::Mismatched { rejected: 1 };
    }
    let Some(image_path) = module.path_hint.as_deref().map(Path::new) else {
        return ModuleSymbols::NoDebugDirectory;
    };
    let expected = module
        .debug_id
        .as_deref()
        .filter(|identity| identity.starts_with("elf:") || identity.starts_with("macho:"))
        .map(str::to_owned)
        .or_else(|| object_identity(image_path));
    let Some(expected) = expected else {
        return ModuleSymbols::NoDebugDirectory;
    };

    // Normal ELF/Mach-O debug sections live in the module itself. GNU
    // MiniDebugInfo is an XZ-compressed ELF in `.gnu_debugdata`; extract it
    // into worker-private storage before the normal identity/usability gate.
    let extracted = extracted_gnu_debugdata(image_path);
    let extracted_is_usable = extracted
        .as_ref()
        .is_some_and(|file| load_object_for_identity(file.path(), &expected).is_some());
    let embedded_path = if extracted_is_usable {
        extracted.as_ref().expect("checked above").path()
    } else {
        image_path
    };
    let mut with_embedded = module.clone();
    with_embedded.embedded_symbol_path = Some(embedded_path.to_string_lossy().into_owned());
    let native_name = native_symbol_name(image_path, &expected);
    let format = if expected.starts_with("macho:") {
        crate::discovery::SymbolArtifactFormat::MachoDsym
    } else {
        crate::discovery::SymbolArtifactFormat::ElfDwarf
    };
    let mut verified_table = None;
    let local = discovery::resolve_symbols(
        &with_embedded,
        config,
        &expected,
        format,
        &native_name,
        &configured_stores(),
        |candidate| {
            let Some(table) = load_object_for_identity(candidate, &expected) else {
                return false;
            };
            verified_table = Some(table);
            true
        },
    );
    match local {
        ResolveOutcome::Found(resolved) => {
            let Some(table) = verified_table.take() else {
                return ModuleSymbols::Mismatched { rejected: 1 };
            };
            let lines = line_numbers_requested()
                .then(|| LineTable::from_path(&resolved.path))
                .flatten();
            ModuleSymbols::Found {
                table,
                lines,
                symbol_file: resolved.path.to_string_lossy().into_owned(),
                source: resolved.source,
            }
        }
        ResolveOutcome::NotFound => server_symbols(&expected, &native_name, 0),
        ResolveOutcome::Mismatched {
            rejected: local_rejected,
        } => server_symbols(&expected, &native_name, local_rejected),
    }
}

fn server_symbols(expected: &str, native_name: &Path, local_rejected: usize) -> ModuleSymbols {
    match discovery::resolve_configured_server(expected, native_name, |path| {
        load_object_for_identity(path, expected)
    }) {
        // The verified download is retained (#818), so a server-fetched
        // symbol file gets a line table exactly like a local one. Built here,
        // while the file is still on disk, because this side's table is eager
        // — nothing needs the file afterwards, so the handle drops with the
        // match arm.
        discovery::ServerResolve::Found {
            url,
            value: table,
            retained,
        } => ModuleSymbols::Found {
            table,
            lines: line_numbers_requested()
                .then(|| LineTable::from_path(&retained))
                .flatten(),
            symbol_file: url,
            source: DiscoverySource::ConfiguredServer,
        },
        discovery::ServerResolve::NotFound if local_rejected == 0 => ModuleSymbols::NotFound,
        discovery::ServerResolve::NotFound => ModuleSymbols::Mismatched {
            rejected: local_rejected,
        },
        discovery::ServerResolve::Mismatched { rejected } => ModuleSymbols::Mismatched {
            rejected: local_rejected + rejected,
        },
    }
}

fn object_identity(path: &Path) -> Option<String> {
    let bytes = read_bounded(path)?;
    let file = object::File::parse(&*bytes).ok()?;
    identity_from_file(&file)
}

fn load_object_for_identity(path: &Path, expected: &str) -> Option<SymbolTable> {
    let bytes = read_bounded(path)?;
    let file = object::File::parse(&*bytes).ok()?;
    if identity_from_file(&file).as_deref() != Some(expected) {
        return None;
    }
    SymbolTable::from_file(&file)
}

fn identity_from_file(file: &object::File<'_>) -> Option<String> {
    let identity = if let Some(build_id) = file.build_id().ok()? {
        format!("elf:{}", hex(build_id))
    } else {
        let uuid = file.mach_uuid().ok()??;
        format!("macho:{}", hex(&uuid))
    };
    Some(identity)
}

fn extracted_gnu_debugdata(image: &Path) -> Option<tempfile::NamedTempFile> {
    let bytes = read_bounded(image)?;
    let file = object::File::parse(&*bytes).ok()?;
    let compressed = file.section_by_name(".gnu_debugdata")?.data().ok()?;
    decompress_xz_to_temp(compressed)
}

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read as _;

    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(discovery::MAX_SYMBOL_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= discovery::MAX_SYMBOL_BYTES).then_some(bytes)
}

fn decompress_xz_to_temp(compressed: &[u8]) -> Option<tempfile::NamedTempFile> {
    use std::io::{Cursor, Error, ErrorKind, Write as _};

    struct BoundedWriter<W> {
        inner: W,
        written: u64,
    }

    impl<W: std::io::Write> std::io::Write for BoundedWriter<W> {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            let remaining = discovery::MAX_SYMBOL_BYTES.saturating_sub(self.written);
            if buffer.len() as u64 > remaining {
                return Err(Error::new(
                    ErrorKind::FileTooLarge,
                    "decompressed MiniDebugInfo exceeds symbol limit",
                ));
            }
            let written = self.inner.write(buffer)?;
            self.written += written as u64;
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    let mut file = tempfile::NamedTempFile::new().ok()?;
    {
        let mut writer = BoundedWriter {
            inner: file.as_file_mut(),
            written: 0,
        };
        lzma_rs::xz_decompress(&mut Cursor::new(compressed), &mut writer).ok()?;
        writer.flush().ok()?;
    }
    Some(file)
}

fn native_symbol_name(image: &Path, identity: &str) -> PathBuf {
    let name = image
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("module"));
    if identity.starts_with("macho:") {
        let mut bundle = name.clone();
        bundle.as_mut_os_string().push(".dSYM");
        bundle
            .join("Contents")
            .join("Resources")
            .join("DWARF")
            .join(name)
    } else {
        let mut debug = name;
        debug.as_mut_os_string().push(".debug");
        debug
    }
}

fn configured_stores() -> Vec<PathBuf> {
    std::env::var_os(discovery::SYMBOL_PATH_ENV).map_or_else(Vec::new, |value| {
        std::env::split_paths(&value)
            .filter(|path| !path.as_os_str().is_empty())
            .collect()
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn this_test_binary_has_a_typed_build_identity() {
        let exe = std::env::current_exe().unwrap();
        let identity = object_identity(&exe).expect("ELF/Mach-O test binary identity");
        assert!(
            identity.starts_with("elf:") || identity.starts_with("macho:"),
            "{identity}"
        );
    }

    #[test]
    fn embedded_symbols_from_this_exact_build_resolve() {
        let exe = std::env::current_exe().unwrap();
        let module = ModuleRef {
            name: exe.file_name().unwrap().to_string_lossy().into_owned(),
            path_hint: Some(exe.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let ModuleSymbols::Found { source, table, .. } =
            discover_module(&module, &DiscoveryConfig::default())
        else {
            panic!("the unstripped test binary must resolve its embedded symbols");
        };
        assert_eq!(source, DiscoverySource::Embedded);
        assert!(table
            .entries
            .iter()
            .any(|(_, name)| name.contains("embedded_symbols_from_this_exact_build_resolve")));
    }

    #[test]
    fn symbol_addresses_use_the_capture_module_base_convention() {
        let exe = std::env::current_exe().unwrap();
        let bytes = std::fs::read(&exe).unwrap();
        let file = object::File::parse(&*bytes).unwrap();
        let (expected_offset, expected_name) = file
            .symbols()
            .chain(file.dynamic_symbols())
            .filter(|symbol| symbol.kind() == object::SymbolKind::Text)
            .find_map(|symbol| {
                let name = symbol.name().ok()?;
                name.contains("symbol_addresses_use_the_capture_module_base_convention")
                    .then(|| {
                        (
                            symbol.address() - file.relative_address_base(),
                            name.to_owned(),
                        )
                    })
            })
            .expect("this test function must be present in the object symbol table");
        let table = SymbolTable::from_object_path(&exe).unwrap();
        assert_eq!(table.lookup(expected_offset), Some(expected_name.as_str()));
    }

    #[test]
    fn gnu_minidebug_xz_extraction_round_trips_in_worker_private_storage() {
        let payload = b"bounded mini debug payload";
        let mut compressed = Vec::new();
        lzma_rs::xz_compress(&mut Cursor::new(payload), &mut compressed).unwrap();
        let extracted = decompress_xz_to_temp(&compressed).expect("extract xz payload");
        assert_eq!(std::fs::read(extracted.path()).unwrap(), payload);
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod line_table_tests {
    use super::*;

    /// This test binary itself — an ELF built by the same toolchain and
    /// profile as everything else, so whatever DWARF the workspace actually
    /// emits is what gets exercised.
    ///
    /// Not a committed fixture: a checked-in ELF drifts from the toolchain and
    /// could keep passing while real builds emitted nothing.
    fn self_image() -> std::path::PathBuf {
        std::env::current_exe().expect("current exe")
    }

    #[test]
    fn the_test_binarys_own_dwarf_yields_line_records() {
        let Some(table) = LineTable::from_path(&self_image()) else {
            eprintln!("skipping: this binary carries no DWARF line program");
            return;
        };
        assert!(
            table.len() > 50,
            "only {} line records — line tables look absent",
            table.len()
        );
    }

    #[test]
    fn a_symbol_from_this_module_resolves_to_a_rust_source_file() {
        let Some(lines) = LineTable::from_path(&self_image()) else {
            eprintln!("skipping: no DWARF line program (see sibling test)");
            return;
        };
        let Some(symbols) = SymbolTable::from_object_path(&self_image()) else {
            eprintln!("skipping: no symbol table");
            return;
        };

        // A symbol this file defines, so a correct answer must name this
        // crate's own source rather than std or a dependency. That is what
        // makes the assertion falsifiable: an address-space mix-up would
        // resolve to some other file, not to nothing.
        let Some((address, name)) = symbols
            .entries
            .iter()
            .find(|(_, name)| name.contains("object_symbols"))
            .cloned()
        else {
            eprintln!("skipping: no object_symbols symbol in this build");
            return;
        };

        let Some((file, line)) = lines.lookup(address) else {
            panic!("no line record covers {name} at {address:#x}");
        };
        assert!(
            file.ends_with(".rs"),
            "expected a Rust source path for {name}, got {file:?}"
        );
        assert!(line > 0, "line number is zero for {name}: {file}:{line}");
    }

    /// Records what `relative_address_base()` is here, and says plainly that
    /// the normalisation is therefore untested on this platform.
    ///
    /// Verified by sabotage: deleting the `- base` subtraction leaves all
    /// three tests above passing, because ELF images here report a base of 0
    /// and `x - 0 == x`. The arithmetic only bites on images with a non-zero
    /// base (Mach-O dyld images, and PIE layouts that report one), which this
    /// suite cannot produce.
    ///
    /// So this is a known blind spot, not a covered case. A wrong base does
    /// not produce an error — it produces plausible line numbers that are
    /// uniformly shifted, which is the worst outcome available in a
    /// diagnostic tool. Anyone extending this to Mach-O should add a fixture
    /// with a non-zero base before trusting the output.
    #[test]
    fn the_address_base_on_this_platform_is_recorded_not_assumed() {
        let loader = match addr2line::Loader::new(self_image()) {
            Ok(loader) => loader,
            Err(_) => {
                eprintln!("skipping: image not loadable");
                return;
            }
        };
        let base = loader.relative_address_base();
        eprintln!("relative_address_base() = {base:#x}");
        if base == 0 {
            eprintln!(
                "note: base is 0, so the `- base` normalisation is a no-op                  here and this suite does not exercise it"
            );
        }
        // No assertion on the value itself: it is a property of the platform,
        // not of this code. The point is that it is reported rather than
        // silently assumed.
    }

    #[test]
    fn an_address_past_every_record_resolves_to_nothing() {
        let Some(lines) = LineTable::from_path(&self_image()) else {
            eprintln!("skipping: no DWARF line program (see sibling test)");
            return;
        };
        // Far past any real code. A nearest-preceding lookup would cheerfully
        // return the last line in the binary; containment must not. This is
        // the assertion that makes the range bookkeeping load-bearing.
        //
        // Note relative address 0 is NOT a good probe for this: 0 is the image
        // base, which legitimately carries a record. An earlier draft asserted
        // that and failed for a correct reason.
        assert_eq!(lines.lookup(u64::MAX / 2), None);
    }
}
