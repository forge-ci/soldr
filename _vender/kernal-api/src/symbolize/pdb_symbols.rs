//! Resolving addresses to function names from a PDB (#637).
//!
//! # Why a sorted table rather than a lookup per address
//!
//! PDB symbols carry a start address but no length, so the function
//! containing an address is the one with the greatest start not exceeding it.
//! That needs the symbols ordered, and a capture has many addresses in the
//! same module — building the table once and binary-searching it turns a
//! repeated linear scan of every symbol into one sort.
//!
//! # Nothing is guessed
//!
//! An address below the first symbol resolves to nothing rather than to the
//! first function. A missing or unreadable PDB yields no map at all. Every one
//! of those paths leaves the frame with its module and offset and a status
//! saying resolution did not happen — a wrong function name would send whoever
//! reads the report somewhere else entirely, and nothing in the output would
//! contradict them.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

// The only PDB parser: `pdb_addr2line` re-exports the `pdb2` fork it builds on.
use pdb::FallibleIterator as _;
use pdb_addr2line::pdb;

use crate::discovery::{self, ResolveOutcome};
use kernal_api::symbolize::wire::{DiscoveryConfig, DiscoverySource, ModuleRef};

/// A module's symbols, ordered by address for containment lookup.
pub struct SymbolTable {
    /// `(relative_virtual_address, name)`, sorted by address.
    entries: Vec<(u32, String)>,
}

impl SymbolTable {
    /// Build a table from the PDB belonging to `image`.
    ///
    /// Returns `None` when no PDB can be found or read. That is an ordinary
    /// outcome — a stripped release binary, or a machine that does not have
    /// the symbols — and it degrades rather than failing the capture.
    pub fn for_image(image: &Path) -> Option<Self> {
        match locate(image, &search_dirs()) {
            Located::Found(path) => Self::from_pdb(&path),
            _ => None,
        }
    }

    /// Build a table from an explicit PDB file.
    pub fn from_pdb(pdb_path: &Path) -> Option<Self> {
        load_pdb(pdb_path).map(|(_, table)| table)
    }

    fn from_open_pdb(mut pdb: pdb::PDB<'_, File>) -> Option<Self> {
        // The address map translates a symbol's internal section:offset into
        // the RVA the loader actually uses. Skipping it yields addresses that
        // look plausible and are wrong.
        let address_map = pdb.address_map().ok()?;
        let mut entries: Vec<(u32, String)> = Vec::new();

        if let Ok(symbols) = pdb.global_symbols() {
            let mut iter = symbols.iter();
            while let Ok(Some(symbol)) = iter.next() {
                if let Ok(pdb::SymbolData::Public(data)) = symbol.parse() {
                    // Only code symbols: a data symbol's address is never a
                    // return address, and including them would let a global
                    // variable claim a frame.
                    if !data.function {
                        continue;
                    }
                    if let Some(rva) = data.offset.to_rva(&address_map) {
                        entries.push((rva.0, data.name.to_string().into_owned()));
                    }
                }
            }
        }

        if entries.is_empty() {
            return None;
        }
        entries.sort_unstable_by_key(|(rva, _)| *rva);
        entries.dedup_by_key(|(rva, _)| *rva);
        Some(Self { entries })
    }

    /// Name of the function containing `relative_address`, if any.
    pub fn lookup(&self, relative_address: u64) -> Option<&str> {
        let target = u32::try_from(relative_address).ok()?;
        // The containing function is the last one starting at or before the
        // address.
        let index = match self.entries.binary_search_by_key(&target, |(rva, _)| *rva) {
            Ok(exact) => exact,
            // An address before every symbol belongs to no function here.
            Err(0) => return None,
            Err(next) => next - 1,
        };
        Some(self.entries[index].1.as_str())
    }

    /// First symbol whose name contains `needle`, as `(rva, name)`.
    ///
    /// Exists for tests that need a symbol they can name in an assertion.
    pub fn symbol_containing_name(&self, needle: &str) -> Option<(u32, String)> {
        self.entries
            .iter()
            .find(|(_, name)| name.contains(needle))
            .cloned()
    }

    /// Up to `limit` addresses whose symbol names contain `needle`.
    ///
    /// Exists for tests that need several real addresses rather than one:
    /// any individual function may have been built without line info, so a
    /// test asserting "lines resolve" has to sample rather than bet on a
    /// single symbol.
    pub fn addresses_for_names_containing(&self, needle: &str, limit: usize) -> Vec<u64> {
        self.entries
            .iter()
            .filter(|(_, name)| name.contains(needle))
            .map(|(rva, _)| u64::from(*rva))
            .take(limit)
            .collect()
    }

    /// Number of symbols in the table.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table holds no symbols.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Identity-gated discovery result for one capture module.
pub enum ModuleSymbols {
    /// A verified PDB and its parsed function table.
    Found {
        /// Parsed symbols.
        table: SymbolTable,
        /// Verified local path or server URL.
        symbol_file: String,
        /// Discovery tier that supplied it.
        source: DiscoverySource,
        /// A server-fetched PDB, kept on disk so lines can be read from it
        /// later (#818).
        ///
        /// `None` for the local tier, where `symbol_file` is already an
        /// openable path. Unlike the DWARF side, this backend resolves lines
        /// in a pre-pass *after* discovery, so the file has to outlive this
        /// value rather than the match arm that produced it.
        retained: Option<tempfile::TempPath>,
    },
    /// No candidate existed.
    NotFound,
    /// Candidates existed but none had the exact build identity.
    Mismatched {
        /// Number of rejected candidates.
        rejected: usize,
    },
    /// Neither the image nor the capture supplied a usable identity.
    NoDebugDirectory,
}

/// The identity a PE records for the PDB it was built with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DebugId {
    /// GUID generated when the PDB was created.
    pub guid: [u8; 16],
    /// How many times the PDB had been written when the image was linked.
    pub age: u32,
}

struct ImageDebugInfo {
    identity: DebugId,
    pdb_name: PathBuf,
}

fn image_debug_info(image: &Path) -> Option<ImageDebugInfo> {
    use object::Object as _;
    use std::io::Read as _;

    let file = File::open(image).ok()?;
    let mut bytes = Vec::new();
    file.take(discovery::MAX_SYMBOL_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > discovery::MAX_SYMBOL_BYTES {
        return None;
    }
    let file = object::File::parse(&*bytes).ok()?;
    let cv = file.pdb_info().ok()??;
    let recorded = String::from_utf8_lossy(cv.path());
    let pdb_name = recorded
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .filter(|name| {
            *name != "."
                && *name != ".."
                && !name.chars().any(|character| {
                    matches!(
                        character,
                        '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                    )
                })
        })
        .map(PathBuf::from)?;
    Some(ImageDebugInfo {
        identity: DebugId {
            guid: guid_pe_to_rfc4122(cv.guid()),
            age: cv.age(),
        },
        pdb_name,
    })
}

/// Read the debug identity out of a PE image.
pub fn image_debug_id(image: &Path) -> Option<DebugId> {
    Some(image_debug_info(image)?.identity)
}

impl DebugId {
    /// Stable manifest/wire spelling: 32 hexadecimal GUID digits, a dash, and
    /// the decimal PDB age.
    pub fn canonical(self) -> String {
        let guid = self
            .guid
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!("pdb:{guid}-{}", self.age)
    }

    /// Parse the canonical manifest/wire spelling.
    pub fn parse(text: &str) -> Option<Self> {
        let (guid, age) = text.strip_prefix("pdb:")?.rsplit_once('-')?;
        if guid.len() != 32 {
            return None;
        }
        let mut bytes = [0_u8; 16];
        for (index, slot) in bytes.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&guid[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(Self {
            guid: bytes,
            age: age.parse().ok()?,
        })
    }
}

/// Convert a PE CodeView GUID to RFC-4122 byte order.
///
/// A GUID is not 16 opaque bytes. Its first three fields are a `u32` and two
/// `u16`, which the PE stores little-endian, while `Uuid::as_bytes` — and
/// therefore the PDB side — yields them big-endian. The trailing 8 bytes are
/// a plain array and identical in both.
///
/// Comparing the two raw forms directly finds them unequal for *every* image,
/// which does not look like a bug: symbolization simply reports "no symbols"
/// forever. CI caught it via `a_binary_matches_its_own_pdb`, on a pair that
/// must match by construction:
///
/// ```text
/// PE : [F9 E2 A7 BC | 1D CF | 63 4A | 86 A8 …]
/// PDB: [BC A7 E2 F9 | CF 1D | 4A 63 | 86 A8 …]
/// ```
fn guid_pe_to_rfc4122(mut raw: [u8; 16]) -> [u8; 16] {
    raw[0..4].reverse();
    raw[4..6].reverse();
    raw[6..8].reverse();
    raw
}

/// Read the debug identity out of a PDB.
fn pdb_debug_id(pdb_path: &Path) -> Option<DebugId> {
    if std::fs::metadata(pdb_path).ok()?.len() > discovery::MAX_SYMBOL_BYTES {
        return None;
    }
    let file = File::open(pdb_path).ok()?;
    let mut pdb = pdb::PDB::open(file).ok()?;
    let info = pdb.pdb_information().ok()?;
    Some(DebugId {
        // `Uuid::as_bytes` is big-endian field order, which is what the PE's
        // CodeView record stores. Reading the fields individually and
        // reassembling them would reintroduce the byte-order bug this avoids.
        guid: *info.guid.as_bytes(),
        age: info.age,
    })
}

/// Open a PDB once, extract its identity, and parse symbols from that same
/// handle so a pathname replacement cannot swap bytes between the two gates.
fn load_pdb(pdb_path: &Path) -> Option<(DebugId, SymbolTable)> {
    let file = File::open(pdb_path).ok()?;
    if file.metadata().ok()?.len() > discovery::MAX_SYMBOL_BYTES {
        return None;
    }
    let mut pdb = pdb::PDB::open(file).ok()?;
    let identity = {
        let info = pdb.pdb_information().ok()?;
        DebugId {
            guid: *info.guid.as_bytes(),
            age: info.age,
        }
    };
    let table = SymbolTable::from_open_pdb(pdb)?;
    Some((identity, table))
}

/// Resolve symbols for a capture module using all #638 discovery tiers.
pub fn discover_module(module: &ModuleRef, config: &DiscoveryConfig) -> ModuleSymbols {
    if module.path_hint.is_none() && module.debug_id.is_none() {
        return ModuleSymbols::NotFound;
    }
    if !crate::discovery::captured_image_still_matches(module) {
        return ModuleSymbols::Mismatched { rejected: 1 };
    }
    let image_info = module
        .path_hint
        .as_deref()
        .and_then(|path| image_debug_info(Path::new(path)));
    let declared_identity = module.debug_id.as_deref().and_then(DebugId::parse);
    let Some(expected) =
        declared_identity.or_else(|| image_info.as_ref().map(|info| info.identity))
    else {
        return ModuleSymbols::NoDebugDirectory;
    };

    let symbol_file_name = module
        .debug_file
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| image_info.map(|info| info.pdb_name))
        .unwrap_or_else(|| PathBuf::from(&module.name).with_extension("pdb"));
    let Some(symbol_file_name) = symbol_file_name.file_name().map(PathBuf::from) else {
        return ModuleSymbols::NotFound;
    };
    let mut verified_table = None;
    let local = discovery::resolve_symbols(
        module,
        config,
        &expected.canonical(),
        crate::discovery::SymbolArtifactFormat::Pdb,
        &symbol_file_name,
        &search_dirs(),
        |candidate| {
            let Some((actual, table)) = load_pdb(candidate) else {
                return false;
            };
            if !identity_matches(expected, actual) {
                return false;
            }
            verified_table = Some(table);
            true
        },
    );
    match local {
        ResolveOutcome::Found(resolved) => {
            let Some(table) = verified_table.take() else {
                return ModuleSymbols::Mismatched { rejected: 1 };
            };
            ModuleSymbols::Found {
                table,
                symbol_file: resolved.path.to_string_lossy().into_owned(),
                source: resolved.source,
                // Local tier: `symbol_file` is already an openable path.
                retained: None,
            }
        }
        ResolveOutcome::NotFound => server_symbols(expected, &symbol_file_name, 0),
        ResolveOutcome::Mismatched {
            rejected: local_rejected,
        } => server_symbols(expected, &symbol_file_name, local_rejected),
    }
}

fn server_symbols(
    expected: DebugId,
    symbol_file_name: &Path,
    local_rejected: usize,
) -> ModuleSymbols {
    match discovery::resolve_configured_server(&expected.canonical(), symbol_file_name, |path| {
        let (actual, table) = load_pdb(path)?;
        identity_matches(expected, actual).then_some(table)
    }) {
        discovery::ServerResolve::Found {
            url,
            value: table,
            retained,
        } => ModuleSymbols::Found {
            table,
            symbol_file: url,
            source: DiscoverySource::ConfiguredServer,
            retained: Some(retained),
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

/// Whether `pdb` has the exact GUID and age recorded by `image`.
///
/// A higher age can be related to the same linker session, but it is not the
/// exact symbol identity captured in the PE CodeView record and is therefore
/// refused by the discovery security boundary.
pub fn identity_matches(image: DebugId, pdb: DebugId) -> bool {
    image == pdb
}

/// Directories to search beyond the image's own, from the environment.
pub(crate) fn search_dirs() -> Vec<PathBuf> {
    parse_search_dirs(std::env::var_os(discovery::SYMBOL_PATH_ENV))
}

/// Split a `PATH`-style value into directories.
///
/// Takes the value rather than reading the environment so it is testable
/// without mutating process-global state, which would make the tests
/// order-dependent on each other and needs `unsafe` besides.
fn parse_search_dirs(raw: Option<std::ffi::OsString>) -> Vec<PathBuf> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    std::env::split_paths(&raw)
        // An empty entry means the current directory to some tools; here it
        // would search wherever the daemon happens to be running, which is
        // not a symbol store and would only add unverified candidates.
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// Why symbolization did or did not find usable symbols for a module.
///
/// A bare "no symbols" conflates situations an operator must act on
/// differently: nothing to find is a build that shipped without symbols, while
/// a candidate that failed verification means the wrong symbols are sitting
/// where the right ones belong — a stale copy in a symbol store, or a search
/// path pointing at another build. The second is a misconfiguration that will
/// keep producing empty reports until someone notices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Located {
    /// A verified symbol file.
    Found(PathBuf),
    /// No file of the expected name existed anywhere searched.
    NotFound,
    /// Files existed but none described this build.
    ///
    /// Carries how many were rejected, because one is a stale copy and
    /// several is a search path aimed at the wrong tree.
    Mismatched {
        /// Candidates that existed but failed verification.
        rejected: usize,
    },
    /// The image itself carries no debug directory, so nothing could ever be
    /// matched against it. A stripped build, not a missing file.
    NoDebugDirectory,
}

/// Locate a PDB for `image`, reporting why when there is none.
///
/// The image's own directory is searched first, so a search path cannot
/// shadow symbols shipped beside a binary. Every candidate is identity-checked
/// — widening where we look must not widen what we trust — and a candidate
/// that fails is skipped rather than aborting the search, since the right file
/// may be later in the path.
pub fn locate(image: &Path, extra: &[PathBuf]) -> Located {
    let Some(expected) = image_debug_id(image) else {
        return Located::NoDebugDirectory;
    };
    let Some(file_name) = image
        .with_extension("pdb")
        .file_name()
        .map(|n| n.to_owned())
    else {
        return Located::NotFound;
    };

    let beside = image.with_extension("pdb");
    let candidates = std::iter::once(beside).chain(extra.iter().map(|dir| dir.join(&file_name)));

    let mut rejected = 0usize;
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        match pdb_debug_id(&candidate) {
            Some(actual) if identity_matches(expected, actual) => return Located::Found(candidate),
            // Present but unusable — the identity disagrees, or it could not
            // be read at all. Both mean a file is sitting where the right one
            // belongs.
            _ => rejected += 1,
        }
    }

    if rejected == 0 {
        Located::NotFound
    } else {
        Located::Mismatched { rejected }
    }
}

/// Locate a PDB for `image`, verifying it describes this exact build.
///
/// Thin wrapper over [`locate`] for callers that only need the path. The
/// reason for a miss is available from `locate` itself, and the report uses
/// it: "no symbols" alone cannot distinguish a stripped build from the wrong
/// symbols sitting where the right ones belong.
pub fn pdb_path_for_with_search(image: &Path, extra: &[PathBuf]) -> Option<PathBuf> {
    match locate(image, extra) {
        Located::Found(path) => Some(path),
        _ => None,
    }
}

/// Resolve `file:line` for a set of module-relative addresses, in one pass
/// over the PDB (#803).
///
/// # Why this takes every address at once
///
/// The DWARF side builds a sorted range table and binary-searches it, because
/// `.debug_line` can be enumerated. A PDB cannot be, at least not through a
/// crate that survives the line programs rustc emits: `pdb` 0.8 panics
/// iterating them (`modi/mod.rs:200`), and `pdb-addr2line` — which does
/// survive them — exposes only a per-address query.
///
/// A per-address query needs a live `Context`, which borrows from the parsed
/// PDB data. Handing one back to a caller would mean storing a value and a
/// borrow of it in the same struct. Taking every address up front avoids that
/// entirely: the `Context` lives inside this call, and the PDB is opened once
/// per module instead of once per frame.
///
/// # What is returned
///
/// Only addresses that resolved appear in the map. A missing entry means the
/// PDB had no line for that address, and the caller should leave the frame at
/// module + offset — the same degradation the rest of this module applies. An
/// unreadable or malformed PDB yields an empty map rather than an error,
/// because no line information is a normal outcome (a dependency built
/// without debug info) and not a failure of the capture.
pub fn resolve_lines(pdb_path: &Path, addresses: &[u64]) -> HashMap<u64, (String, u32)> {
    let mut resolved = HashMap::new();
    // Opening and parsing a PDB is the expensive part; skip it when there is
    // nothing to ask.
    if addresses.is_empty() {
        return resolved;
    }

    let Ok(file) = File::open(pdb_path) else {
        return resolved;
    };
    // `pdb` is `pdb_addr2line`'s re-export (see the import above), so the
    // handle types here and in the symbol table cannot drift apart.
    let Ok(pdb) = pdb::PDB::open(file) else {
        return resolved;
    };
    let Ok(data) = pdb_addr2line::ContextPdbData::try_from_pdb(pdb) else {
        return resolved;
    };
    let Ok(context) = data.make_context() else {
        return resolved;
    };

    for &address in addresses {
        // RVAs in a PDB are 32-bit. An address that does not fit is not a
        // valid relative address for this module, so it resolves to nothing
        // rather than being truncated into a plausible-looking wrong answer.
        let Ok(probe) = u32::try_from(address) else {
            continue;
        };
        let Ok(Some(frames)) = context.find_frames(probe) else {
            continue;
        };
        // The inline stack is ordered inside-out, so the first entry is the
        // innermost — the source location of the instruction itself. That
        // matches what the DWARF side reports for an inlined address, and it
        // is the location a reader wants: the line that actually ran, not the
        // line of the outer function it was folded into.
        let Some(frame) = frames.frames.first() else {
            continue;
        };
        let (Some(file), Some(line)) = (frame.file.as_ref(), frame.line) else {
            continue;
        };
        resolved.insert(address, (file.to_string(), line));
    }

    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PDB of the test binary itself, which a debug build produces.
    ///
    /// Returns `None` when it is absent, which happens on machines whose
    /// compiler cache drops the linker's side-files. Every test using this
    /// then skips — and a silently skipped test is indistinguishable from a
    /// passing one, which is how an earlier revision of this module reported
    /// "the PDB parses correctly" while having parsed nothing at all.
    ///
    /// So under CI the absence is a failure instead. A Windows CI run either
    /// exercises these paths for real or says plainly that it could not.
    fn own_pdb() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let candidate = exe.with_extension("pdb");
        if candidate.is_file() {
            return Some(candidate);
        }
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "no PDB at {} during a CI run; the symbol tests would skip and              assert nothing",
            candidate.display()
        );
        None
    }

    #[test]
    fn a_real_pdb_yields_file_and_line_for_this_crates_code() {
        // Anchored on symbols belonging to THIS crate, so a pass cannot come
        // from resolving something unrelated.
        //
        // Not anchored on `resolve_lines` by name, which is what an earlier
        // revision did and what CI rejected: `SymbolTable` is built from the
        // PDB's *public* symbol stream, and a private Rust function never
        // enters it. The neighbouring name test already establishes that
        // crate-qualified symbols are the ones actually present.
        let Some(pdb_path) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let Some(table) = SymbolTable::from_pdb(&pdb_path) else {
            eprintln!("skipping: PDB had no public function symbols");
            return;
        };

        // Sample many, because any individual function may have been built
        // without line info; the claim under test is that resolution works,
        // not that every symbol carries lines.
        let addresses: Vec<u64> = table
            .entries
            .iter()
            .filter(|(_, name)| name.contains("kernal_api"))
            .map(|(rva, _)| u64::from(*rva))
            .take(64)
            .collect();
        assert!(
            !addresses.is_empty(),
            "no symbol named this crate; the anchor is wrong, not the resolver"
        );

        // Also exercises the bulk contract: many addresses, one PDB pass.
        let resolved = resolve_lines(&pdb_path, &addresses);
        assert!(
            !resolved.is_empty(),
            "none of {} of this crate's symbols resolved to a line",
            addresses.len()
        );

        for (file, line) in resolved.values() {
            assert!(
                file.to_ascii_lowercase().ends_with(".rs"),
                "resolved to {file}, which is not a Rust source file"
            );
            assert!(*line > 0, "line numbers are 1-based; got {line}");
        }
        // The exact path is deliberately not asserted: a PDB records whatever
        // path the build used, which differs between a local checkout and CI.
    }

    #[test]
    fn no_addresses_means_no_work_and_no_answers() {
        let Some(pdb_path) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        assert!(resolve_lines(&pdb_path, &[]).is_empty());
    }

    #[test]
    fn an_address_too_large_for_an_rva_has_no_line() {
        // Truncating into 32 bits would produce a plausible-looking line for
        // an address that is not in this module at all.
        let Some(pdb_path) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let huge = u64::from(u32::MAX) + 1;
        assert!(resolve_lines(&pdb_path, &[huge]).is_empty());
    }

    #[test]
    fn a_missing_pdb_yields_no_lines_rather_than_failing() {
        // No line information is an ordinary outcome — a dependency built
        // without debug info — and must degrade, not error.
        let dir = tempfile::tempdir().expect("tempdir");
        let absent = dir.path().join("nothing.pdb");
        assert!(resolve_lines(&absent, &[0x1000]).is_empty());
    }

    #[test]
    fn a_real_pdb_yields_symbols() {
        let Some(pdb_path) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let table = SymbolTable::from_pdb(&pdb_path).expect("the test binary's PDB has symbols");
        assert!(
            !table.is_empty(),
            "the test binary PDB should list functions"
        );
    }

    #[test]
    fn debug_identity_canonical_form_round_trips() {
        let identity = DebugId {
            guid: [0xAB; 16],
            age: 17,
        };
        assert_eq!(DebugId::parse(&identity.canonical()), Some(identity));
        assert_eq!(
            identity.canonical(),
            "pdb:abababababababababababababababab-17"
        );
    }

    /// Every symbol RVA must land inside an executable section of the PE.
    ///
    /// This is the check that the PDB's address map is actually applied. The
    /// other PDB tests take an address *out of* the table and look it up *in*
    /// the same table, so a uniformly wrong translation round-trips through
    /// them undetected — confirmed by sabotage: replacing
    /// `offset.to_rva(&address_map)` with the raw section offset left every
    /// one of them passing.
    ///
    /// The section layout read from the PE by `object` is an independent
    /// source, which is what makes this able to catch it.
    #[test]
    fn symbol_addresses_fall_inside_executable_sections() {
        use object::{Object as _, ObjectSection as _};

        let Some(pdb_path) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let Some(table) = SymbolTable::from_pdb(&pdb_path) else {
            eprintln!("skipping: PDB had no public function symbols");
            return;
        };

        let exe = std::env::current_exe().expect("current exe");
        let bytes = std::fs::read(&exe).expect("read own image");
        let file = object::File::parse(&*bytes).expect("parse own PE");

        // `section.address()` is a VIRTUAL address — image base included —
        // while PDB symbols are RVAs. Subtracting the base puts both in the
        // same space. Comparing them directly reports every symbol as out of
        // range, which is how the first version of this test failed on CI.
        let base = file.relative_address_base();
        let ranges: Vec<(u64, u64)> = file
            .sections()
            .filter(|s| match s.flags() {
                // IMAGE_SCN_MEM_EXECUTE
                object::SectionFlags::Coff { characteristics } => {
                    characteristics & 0x2000_0000 != 0
                }
                _ => false,
            })
            .map(|s| {
                let start = s.address().saturating_sub(base);
                (start, start + s.size())
            })
            .collect();
        assert!(!ranges.is_empty(), "the PE should have executable sections");

        let outside: Vec<_> = table
            .entries
            .iter()
            .filter(|(rva, _)| {
                let rva = u64::from(*rva);
                !ranges
                    .iter()
                    .any(|(start, end)| rva >= *start && rva < *end)
            })
            .take(5)
            .collect();

        assert!(
            outside.is_empty(),
            "function symbols outside every executable section {ranges:?}:              {outside:?} — the PDB address map is likely not being applied",
        );
    }

    fn id(guid_byte: u8, age: u32) -> DebugId {
        DebugId {
            guid: [guid_byte; 16],
            age,
        }
    }

    /// The three leading GUID fields swap; the trailing eight do not.
    ///
    /// Pinned separately from the end-to-end check so the conversion is
    /// covered on platforms with no PDB to compare against.
    #[test]
    fn the_pe_guid_is_reordered_field_wise() {
        let pe = [
            0xF9, 0xE2, 0xA7, 0xBC, 0x1D, 0xCF, 0x63, 0x4A, 0x86, 0xA8, 0xD1, 0xE6, 0xD2, 0x8B,
            0xB3, 0xD0,
        ];
        let expected = [
            0xBC, 0xA7, 0xE2, 0xF9, 0xCF, 0x1D, 0x4A, 0x63, 0x86, 0xA8, 0xD1, 0xE6, 0xD2, 0x8B,
            0xB3, 0xD0,
        ];
        assert_eq!(guid_pe_to_rfc4122(pe), expected);
    }

    /// Converting twice returns the original: the swap is its own inverse, so
    /// a stray second call is detectable rather than silently harmless.
    #[test]
    fn the_guid_conversion_is_its_own_inverse() {
        let pe = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10,
        ];
        assert_eq!(guid_pe_to_rfc4122(guid_pe_to_rfc4122(pe)), pe);
    }

    #[test]
    fn an_exact_identity_matches() {
        assert!(identity_matches(id(0xAB, 3), id(0xAB, 3)));
    }

    /// A different GUID is a different build, whatever the age.
    #[test]
    fn a_different_guid_never_matches() {
        assert!(!identity_matches(id(0xAB, 3), id(0xCD, 3)));
        assert!(!identity_matches(id(0xAB, 3), id(0xCD, 99)));
    }

    /// A related but newer PDB is not the PE's exact recorded identity.
    #[test]
    fn a_higher_pdb_age_does_not_match() {
        assert!(!identity_matches(id(0xAB, 3), id(0xAB, 4)));
    }

    /// A lower age means the PDB predates the link: a different build.
    #[test]
    fn a_lower_pdb_age_does_not_match() {
        assert!(
            !identity_matches(id(0xAB, 5), id(0xAB, 4)),
            "a PDB older than the image cannot describe it"
        );
    }

    /// The decisive check: the real binary and its own PDB must match.
    ///
    /// This is what validates the GUID byte order. The PE's CodeView record
    /// and the PDB's stream store the GUID differently enough that a naive
    /// field-by-field reassembly mismatches — and a mismatch here would look
    /// exactly like "no symbols available", silently disabling symbolization
    /// rather than failing.
    #[test]
    fn a_binary_matches_its_own_pdb() {
        let Some(pdb_path) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let exe = std::env::current_exe().expect("current exe");

        let Some(image) = image_debug_id(&exe) else {
            panic!("the test binary has no CodeView debug directory");
        };
        let pdb = pdb_debug_id(&pdb_path).expect("the PDB has an identity");

        assert_eq!(
            image.guid, pdb.guid,
            "the image and its own PDB disagree on the GUID; byte order is wrong"
        );
        assert!(
            identity_matches(image, pdb),
            "image {image:?} did not match its own pdb {pdb:?}"
        );
    }

    /// And the lookup that uses it accepts that pair.
    #[test]
    fn the_sibling_pdb_of_this_binary_is_accepted() {
        if own_pdb().is_none() {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        }
        let exe = std::env::current_exe().expect("current exe");
        assert!(
            pdb_path_for_with_search(&exe, &[]).is_some(),
            "the binary's own sibling PDB should pass identity verification"
        );
    }

    /// A PDB that is merely in the right place must not be trusted.
    #[test]
    fn a_sibling_pdb_from_a_different_build_is_rejected() {
        let Some(real) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        // A copy of this binary, with a copy of a PDB that describes a
        // *different* image — modelled by pairing our PDB with an unrelated
        // executable name whose debug id will not match.
        let fake_exe = dir.path().join("other.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &fake_exe).expect("copy exe");
        // Truncate the copied PDB so its identity cannot be read: an
        // unreadable identity must be refused, not assumed to match.
        std::fs::write(
            dir.path().join("other.pdb"),
            &std::fs::read(&real).unwrap()[..64],
        )
        .expect("write pdb");

        assert!(
            pdb_path_for_with_search(&fake_exe, &[]).is_none(),
            "a PDB whose identity cannot be verified must not be accepted"
        );
    }

    /// Path parsing runs everywhere, unlike the search tests below, which
    /// need a real PDB and skip on a machine that produces none.
    #[test]
    fn an_absent_symbol_path_yields_no_directories() {
        assert!(parse_search_dirs(None).is_empty());
    }

    #[test]
    fn empty_entries_are_dropped_rather_than_searching_the_cwd() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let raw = format!("{sep}{sep}");
        assert!(
            parse_search_dirs(Some(raw.into())).is_empty(),
            "empty entries must not become a search of the working directory"
        );
    }

    #[test]
    fn directories_are_split_and_ordered() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let raw = format!("first{sep}second{sep}third");
        let dirs = parse_search_dirs(Some(raw.into()));
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("first"),
                PathBuf::from("second"),
                PathBuf::from("third")
            ],
            "order matters: the search takes the first verified match"
        );
    }

    /// A PDB in a search directory is found when none sits beside the image.
    #[test]
    fn a_search_directory_supplies_a_missing_pdb() {
        let Some(real) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let exe = std::env::current_exe().expect("current exe");
        let dir = tempfile::tempdir().expect("tempdir");

        // A copy of the image with no PDB beside it, and the real PDB in a
        // separate directory under the name the image implies.
        let lonely_exe = dir.path().join("lonely.exe");
        std::fs::copy(&exe, &lonely_exe).expect("copy exe");
        let store = tempfile::tempdir().expect("store");
        std::fs::copy(&real, store.path().join("lonely.pdb")).expect("copy pdb");

        assert!(
            pdb_path_for_with_search(&lonely_exe, &[]).is_none(),
            "with no search path there is nothing beside the image to find"
        );
        let found = pdb_path_for_with_search(&lonely_exe, &[store.path().to_path_buf()])
            .expect("the search directory should supply it");
        assert_eq!(found, store.path().join("lonely.pdb"));
    }

    #[test]
    fn discovery_uses_the_codeview_pdb_basename_not_the_image_stem() {
        let Some(real) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let exe = std::env::current_exe().expect("current exe");
        let dir = tempfile::tempdir().expect("tempdir");
        let renamed_exe = dir.path().join("renamed-image.exe");
        std::fs::copy(&exe, &renamed_exe).expect("copy exe");
        let store = tempfile::tempdir().expect("store");
        let recorded_name = image_debug_info(&renamed_exe).expect("CodeView").pdb_name;
        assert_ne!(recorded_name, PathBuf::from("renamed-image.pdb"));
        std::fs::copy(&real, store.path().join(&recorded_name)).expect("copy recorded PDB");
        let module = ModuleRef {
            name: "renamed-image.exe".into(),
            path_hint: Some(renamed_exe.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let config = DiscoveryConfig {
            registered_symbol_paths: vec![store.path().to_string_lossy().into_owned()],
            ..Default::default()
        };
        assert!(matches!(
            discover_module(&module, &config),
            ModuleSymbols::Found {
                source: DiscoverySource::Registration,
                ..
            }
        ));
    }

    /// A same-named PDB from a different build must be skipped, not accepted
    /// because it was in the search path.
    #[test]
    fn a_search_directory_candidate_is_still_identity_checked() {
        let Some(real) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let exe = std::env::current_exe().expect("current exe");
        let dir = tempfile::tempdir().expect("tempdir");
        let lonely_exe = dir.path().join("lonely.exe");
        std::fs::copy(&exe, &lonely_exe).expect("copy exe");

        // Right name, wrong contents: truncated so its identity cannot be
        // read at all, which must be treated as "not verified".
        let store = tempfile::tempdir().expect("store");
        let bytes = std::fs::read(&real).expect("read pdb");
        std::fs::write(store.path().join("lonely.pdb"), &bytes[..64]).expect("write");

        assert!(
            pdb_path_for_with_search(&lonely_exe, &[store.path().to_path_buf()]).is_none(),
            "a candidate whose identity cannot be verified must not be accepted"
        );
    }

    /// The search continues past a bad candidate — the right file may be
    /// later in the path.
    #[test]
    fn a_bad_candidate_does_not_abort_the_search() {
        let Some(real) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let exe = std::env::current_exe().expect("current exe");
        let dir = tempfile::tempdir().expect("tempdir");
        let lonely_exe = dir.path().join("lonely.exe");
        std::fs::copy(&exe, &lonely_exe).expect("copy exe");

        let bad = tempfile::tempdir().expect("bad");
        let bytes = std::fs::read(&real).expect("read pdb");
        std::fs::write(bad.path().join("lonely.pdb"), &bytes[..64]).expect("write bad");
        let good = tempfile::tempdir().expect("good");
        std::fs::copy(&real, good.path().join("lonely.pdb")).expect("copy good");

        let found = pdb_path_for_with_search(
            &lonely_exe,
            &[bad.path().to_path_buf(), good.path().to_path_buf()],
        )
        .expect("the good candidate later in the path should be found");
        assert_eq!(found, good.path().join("lonely.pdb"));
    }

    /// The image's own directory wins, so a search path cannot shadow the
    /// symbols shipped beside a binary.
    #[test]
    fn the_image_directory_is_searched_first() {
        let Some(real) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let exe = std::env::current_exe().expect("current exe");
        let store = tempfile::tempdir().expect("store");
        std::fs::copy(
            &real,
            store
                .path()
                .join(exe.with_extension("pdb").file_name().unwrap()),
        )
        .expect("copy pdb");

        let found = pdb_path_for_with_search(&exe, &[store.path().to_path_buf()])
            .expect("the sibling PDB should be found");
        assert_eq!(found, exe.with_extension("pdb"), "the sibling must win");
    }

    #[test]
    fn a_missing_pdb_yields_no_table() {
        assert!(SymbolTable::from_pdb(Path::new("no-such-file.pdb")).is_none());
    }

    /// Garbage must be refused, not parsed into confident nonsense.
    #[test]
    fn a_corrupt_pdb_yields_no_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("broken.pdb");
        std::fs::write(&path, [0xFFu8; 8192]).expect("write");
        assert!(SymbolTable::from_pdb(&path).is_none());
    }

    #[test]
    fn an_address_below_every_symbol_resolves_to_nothing() {
        let table = SymbolTable {
            entries: vec![(0x1000, "alpha".into()), (0x2000, "bravo".into())],
        };
        assert_eq!(
            table.lookup(0x0FFF),
            None,
            "must not claim the first symbol"
        );
    }

    #[test]
    fn an_address_inside_a_function_resolves_to_it() {
        let table = SymbolTable {
            entries: vec![(0x1000, "alpha".into()), (0x2000, "bravo".into())],
        };
        assert_eq!(table.lookup(0x1000), Some("alpha"), "exact start");
        assert_eq!(table.lookup(0x1234), Some("alpha"), "inside alpha");
        assert_eq!(table.lookup(0x2000), Some("bravo"), "exact start of bravo");
        // Past the last symbol there is no next start to bound it, so the last
        // function is the best available answer.
        assert_eq!(table.lookup(0x9999), Some("bravo"));
    }

    #[test]
    fn an_address_too_large_for_an_rva_resolves_to_nothing() {
        let table = SymbolTable {
            entries: vec![(0x1000, "alpha".into())],
        };
        assert_eq!(table.lookup(u64::from(u32::MAX) + 1), None);
    }

    /// Names read out of a real PDB must be real symbols, not garbage.
    ///
    /// A parser that silently mis-reads the string table would still produce
    /// *some* name for every address; requiring this crate's own name to
    /// appear proves the bytes were interpreted correctly.
    #[test]
    fn a_real_pdb_yields_recognizable_symbol_names() {
        let Some(pdb_path) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let Some(table) = SymbolTable::from_pdb(&pdb_path) else {
            eprintln!("skipping: PDB had no public function symbols");
            return;
        };

        assert!(
            table
                .entries
                .iter()
                .any(|(_, name)| name.contains("kernal_api")),
            "no symbol mentioned this crate; the {} names read look wrong, e.g. {:?}",
            table.len(),
            table.entries.iter().take(3).collect::<Vec<_>>()
        );
    }

    /// Looking up a symbol's own start address returns that symbol.
    ///
    /// This checks the binary-search arithmetic against real, unevenly spaced
    /// addresses rather than the hand-written pairs above. It does not
    /// exercise module-base subtraction — the caller does that, and
    /// `symbolize` covers it.
    #[test]
    fn every_symbol_resolves_to_itself_at_its_own_address() {
        let Some(pdb_path) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let Some(table) = SymbolTable::from_pdb(&pdb_path) else {
            eprintln!("skipping: PDB had no public function symbols");
            return;
        };

        for (rva, expected) in table.entries.iter().step_by(97) {
            assert_eq!(
                table.lookup(u64::from(*rva)),
                Some(expected.as_str()),
                "symbol at {rva:#x} did not resolve to itself"
            );
        }
    }
}
