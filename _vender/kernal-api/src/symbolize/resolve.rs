//! Turning raw frames into a report (#637).
//!
//! Two steps. Attribution matches every frame to its module and keeps its
//! offset; resolution asks that module's symbol file for a function name.
//! Each frame carries a status saying how far it got, and the offset survives
//! regardless — a report of bare offsets is still actionable against the right
//! build, which is what makes degradation acceptable.
//!
//! Attribution is the step that must never be wrong: a frame assigned to the
//! wrong module would make an otherwise correct symbol lookup produce a
//! confidently wrong name.
//!
//! # Platform coverage
//!
//! Resolution reads PE/PDB, which is the only capture backend that exists
//! today (#635). Elsewhere every frame stays `RawOnly` — the honest result,
//! rather than an empty report or a guess. ELF/DWARF joins when the Unix
//! capture backends land.

use kernal_api::symbolize::wire::{
    CaptureFormat, FrameStatus, ModuleReport, ModuleSymbolStatus, RawCapture, RawThread, SymFrame,
    SymThread, SymbolReport,
};

/// Why symbolization could not produce a report.
#[derive(Debug, thiserror::Error)]
pub enum SymbolizeError {
    /// The capture asked for a path this build does not implement.
    #[error("capture format {0:?} is not supported yet")]
    UnsupportedFormat(CaptureFormat),
}

/// Name used when a frame cannot be attributed to any module.
///
/// A visible placeholder rather than an empty string: a report reader should
/// be able to tell "unattributable" from "the module had no name".
pub const UNKNOWN_MODULE: &str = "<unknown>";

/// Symbolize a capture.
///
/// Threads, their order, their ids, and their interpreter frames all pass
/// through unchanged — only the native frames are touched.
pub fn symbolize(capture: &RawCapture) -> Result<SymbolReport, SymbolizeError> {
    match capture.format {
        CaptureFormat::CooperativeFrames => {}
        // Parsing a minidump requires the crash path from S7. Refusing is the
        // honest answer; returning an empty report would read as "this crash
        // had no threads".
        other => return Err(SymbolizeError::UnsupportedFormat(other)),
    }

    let cache = build_symbol_cache(capture);
    let lines = build_line_cache(
        capture,
        &cache,
        crate::line_numbers::line_numbers_requested(),
    );
    let threads = capture
        .threads
        .iter()
        .map(|thread| symbolize_thread(capture, &cache, &lines, thread))
        .collect();
    let modules = module_reports(capture, &cache);

    Ok(SymbolReport { threads, modules })
}

/// Symbol tables, built once per module and reused across every frame.
///
/// Opening and sorting a PDB per frame would reparse the same file dozens of
/// times for one thread. Entries are `None` when the module has no usable
/// symbols, so a miss is remembered rather than retried.
#[cfg(target_os = "windows")]
type SymbolCache = Vec<crate::pdb_symbols::ModuleSymbols>;
#[cfg(not(target_os = "windows"))]
type SymbolCache = Vec<crate::object_symbols::ModuleSymbols>;

#[cfg(target_os = "windows")]
fn build_symbol_cache(capture: &RawCapture) -> SymbolCache {
    capture
        .modules
        .iter()
        .map(|module| crate::pdb_symbols::discover_module(module, &capture.discovery))
        .collect()
}

/// No symbol-file parser exists for this platform yet, so every frame stays
/// `RawOnly` — the module and offset are still reported.
#[cfg(not(target_os = "windows"))]
fn build_symbol_cache(capture: &RawCapture) -> SymbolCache {
    capture
        .modules
        .iter()
        .map(|module| crate::object_symbols::discover_module(module, &capture.discovery))
        .collect()
}

/// Per-module account of the symbol lookup.
#[cfg(target_os = "windows")]
fn module_reports(capture: &RawCapture, cache: &SymbolCache) -> Vec<ModuleReport> {
    use crate::pdb_symbols::ModuleSymbols;

    capture
        .modules
        .iter()
        .zip(cache)
        .map(|(module, symbols)| {
            let (status, symbol_file, symbol_source, rejected) = match symbols {
                ModuleSymbols::Found {
                    symbol_file,
                    source,
                    ..
                } => (
                    ModuleSymbolStatus::Resolved,
                    Some(symbol_file.clone()),
                    Some(*source),
                    0,
                ),
                ModuleSymbols::NotFound => (ModuleSymbolStatus::NotFound, None, None, 0),
                ModuleSymbols::Mismatched { rejected } => {
                    (ModuleSymbolStatus::Mismatched, None, None, *rejected)
                }
                ModuleSymbols::NoDebugDirectory => {
                    (ModuleSymbolStatus::NoDebugDirectory, None, None, 0)
                }
            };
            ModuleReport {
                name: module.name.clone(),
                status,
                symbol_file,
                symbol_source,
                rejected_candidates: rejected,
            }
        })
        .collect()
}

/// No symbol reader on this platform, so every module reports as much rather
/// than as "not found", which would blame the build for a missing parser.
#[cfg(not(target_os = "windows"))]
fn module_reports(capture: &RawCapture, cache: &SymbolCache) -> Vec<ModuleReport> {
    use crate::object_symbols::ModuleSymbols;

    capture
        .modules
        .iter()
        .zip(cache)
        .map(|(module, symbols)| {
            let (status, symbol_file, symbol_source, rejected) = match symbols {
                ModuleSymbols::Found {
                    symbol_file,
                    source,
                    ..
                } => (
                    ModuleSymbolStatus::Resolved,
                    Some(symbol_file.clone()),
                    Some(*source),
                    0,
                ),
                ModuleSymbols::NotFound => (ModuleSymbolStatus::NotFound, None, None, 0),
                ModuleSymbols::Mismatched { rejected } => {
                    (ModuleSymbolStatus::Mismatched, None, None, *rejected)
                }
                ModuleSymbols::NoDebugDirectory => {
                    (ModuleSymbolStatus::NoDebugDirectory, None, None, 0)
                }
            };
            ModuleReport {
                name: module.name.clone(),
                status,
                symbol_file,
                symbol_source,
                rejected_candidates: rejected,
            }
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn lookup(cache: &SymbolCache, module_index: usize, relative_address: u64) -> Option<String> {
    let crate::pdb_symbols::ModuleSymbols::Found { table, .. } = cache.get(module_index)? else {
        return None;
    };
    table.lookup(relative_address).map(str::to_owned)
}

#[cfg(not(target_os = "windows"))]
fn lookup(cache: &SymbolCache, module_index: usize, relative_address: u64) -> Option<String> {
    let crate::object_symbols::ModuleSymbols::Found { table, .. } = cache.get(module_index)? else {
        return None;
    };
    table.lookup(relative_address).map(str::to_owned)
}

/// Resolved `file:line` per module, keyed by module-relative address (#803).
///
/// Built once per capture, alongside the symbol cache and for the same
/// reason: a PDB is expensive to open and every frame in a module would
/// otherwise reopen it.
#[cfg(target_os = "windows")]
type LineCache = Vec<std::collections::HashMap<u64, (String, u32)>>;

/// The DWARF side resolves through a range table held in the symbol cache, so
/// it needs no second cache.
///
/// A named zero-sized type rather than `()`: binding a unit value trips
/// clippy's `let_unit_value` at the call site, and that fires only on the
/// non-Windows lanes, so the host build would not have shown it.
#[cfg(not(target_os = "windows"))]
struct LineCache;

/// Resolve every frame's line in one pass per module.
///
/// # Why this is a pre-pass rather than a lookup
///
/// `pdb_addr2line` answers one address at a time and its query borrows the
/// parsed PDB, so nothing queryable can be handed back to a caller. Gathering
/// the addresses first turns that constraint into an advantage: each PDB is
/// opened exactly once, and the borrow stays inside `resolve_lines`.
///
/// # Both discovery tiers, not just the local one
///
/// The local tier's `symbol_file` is an openable path. The server tier
/// reports a URL and hands over the verified download itself, retained for
/// the worker's lifetime (#818) — so a server-sourced module resolves lines
/// exactly like a local one, rather than silently degrading to name-only for
/// a reason that has nothing to do with the build being diagnosed.
#[cfg(target_os = "windows")]
fn build_line_cache(capture: &RawCapture, cache: &SymbolCache, requested: bool) -> LineCache {
    use crate::pdb_symbols::ModuleSymbols;
    use std::collections::HashMap;

    let mut per_module: Vec<HashMap<u64, (String, u32)>> =
        vec![HashMap::new(); capture.modules.len()];
    // Off by default: line programs cost a parse per module, and a caller who
    // only asked "which function" must not pay for it.
    //
    // Taken as an argument rather than read here, so this is testable without
    // `set_var` — env-mutating tests race under a parallel runner and this
    // repo has been bitten by that.
    if !requested {
        return per_module;
    }

    // Gather first, resolve second — the whole point of the bulk API.
    let mut wanted: Vec<Vec<u64>> = vec![Vec::new(); capture.modules.len()];
    for thread in &capture.threads {
        for frame in &thread.frames {
            if let Some(addresses) = wanted.get_mut(frame.module_index as usize) {
                addresses.push(frame.relative_address);
            }
        }
    }

    for (index, addresses) in wanted.iter_mut().enumerate() {
        if addresses.is_empty() {
            continue;
        }
        // One entry per address; duplicates across frames are common in a
        // recursive stack and would re-ask the same question.
        addresses.sort_unstable();
        addresses.dedup();

        let Some(ModuleSymbols::Found {
            symbol_file,
            retained,
            ..
        }) = cache.get(index)
        else {
            continue;
        };
        // The server tier reports a URL, which is not openable, and hands the
        // verified download over instead (#818). The local tier's
        // `symbol_file` is already a path.
        let path = match retained {
            Some(retained) => retained.as_ref(),
            None => std::path::Path::new(symbol_file.as_str()),
        };
        if !path.is_file() {
            continue;
        }
        per_module[index] = crate::pdb_symbols::resolve_lines(path, addresses);
    }

    per_module
}

/// No pre-pass needed off Windows; see [`LineCache`].
#[cfg(not(target_os = "windows"))]
fn build_line_cache(_capture: &RawCapture, _cache: &SymbolCache, _requested: bool) -> LineCache {
    LineCache
}

/// The `(file, line)` for a frame, when line resolution was asked for (#803).
#[cfg(not(target_os = "windows"))]
fn lookup_line(
    cache: &SymbolCache,
    _lines: &LineCache,
    module_index: usize,
    relative_address: u64,
) -> Option<(String, u32)> {
    let crate::object_symbols::ModuleSymbols::Found { lines, .. } = cache.get(module_index)? else {
        return None;
    };
    lines
        .as_ref()?
        .lookup(relative_address)
        .map(|(file, line)| (file.to_owned(), line))
}

/// The `(file, line)` for a frame, read out of the pre-pass.
#[cfg(target_os = "windows")]
fn lookup_line(
    _cache: &SymbolCache,
    lines: &LineCache,
    module_index: usize,
    relative_address: u64,
) -> Option<(String, u32)> {
    lines.get(module_index)?.get(&relative_address).cloned()
}

fn symbolize_thread(
    capture: &RawCapture,
    cache: &SymbolCache,
    lines: &LineCache,
    thread: &RawThread,
) -> SymThread {
    let frames = thread
        .frames
        .iter()
        .map(|frame| {
            match capture.modules.get(frame.module_index as usize) {
                Some(module) => {
                    let function =
                        lookup(cache, frame.module_index as usize, frame.relative_address);
                    let (file, line) = match lookup_line(
                        cache,
                        lines,
                        frame.module_index as usize,
                        frame.relative_address,
                    ) {
                        Some((file, line)) => (Some(file), Some(line)),
                        None => (None, None),
                    };
                    SymFrame {
                        module: module.name.clone(),
                        relative_address: frame.relative_address,
                        // Resolved only when a symbol file actually produced a
                        // name; everything else keeps module + offset and says
                        // so.
                        status: if function.is_some() {
                            FrameStatus::Resolved
                        } else {
                            FrameStatus::RawOnly
                        },
                        function,
                        file,
                        line,
                        inline_frames: Vec::new(),
                    }
                }
                // An out-of-range index is a malformed capture, but one bad
                // frame must not discard the rest of the thread: the
                // surrounding frames are still evidence.
                None => SymFrame {
                    module: UNKNOWN_MODULE.to_string(),
                    relative_address: frame.relative_address,
                    function: None,
                    file: None,
                    line: None,
                    inline_frames: Vec::new(),
                    status: FrameStatus::ModuleUnknown,
                },
            }
        })
        .collect();

    SymThread {
        os_tid: thread.os_tid,
        name: thread.name.clone(),
        frames,
        py_frames: thread.py_frames.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::symbolize::wire::{ModuleRef, PyFrame, RawFrame};

    fn capture_with(modules: Vec<ModuleRef>, frames: Vec<RawFrame>) -> RawCapture {
        RawCapture {
            format: CaptureFormat::CooperativeFrames,
            discovery: Default::default(),
            modules,
            threads: vec![RawThread {
                os_tid: 11,
                name: Some("t".into()),
                frames,
                py_frames: Vec::new(),
            }],
        }
    }

    fn module(name: &str) -> ModuleRef {
        ModuleRef {
            name: name.into(),
            ..Default::default()
        }
    }

    /// Every module gets an account, so a reader can tell why a frame is
    /// unsymbolized rather than only that it is.
    #[test]
    fn every_module_is_accounted_for() {
        let capture = capture_with(vec![module("a.dll"), module("b.dll")], Vec::new());
        let report = symbolize(&capture).unwrap();
        assert_eq!(report.modules.len(), 2);
        assert_eq!(report.modules[0].name, "a.dll");
        assert_eq!(report.modules[1].name, "b.dll");
    }

    /// A module with no path could never be looked beside; that is a property
    /// of the capture, and must not be reported as a stripped build.
    #[test]
    fn a_module_without_a_path_is_not_found_rather_than_stripped() {
        let capture = capture_with(vec![module("ghost.dll")], Vec::new());
        let status = symbolize(&capture).unwrap().modules[0].status;
        assert!(
            matches!(
                status,
                ModuleSymbolStatus::NotFound | ModuleSymbolStatus::Unsupported
            ),
            "got {status:?}"
        );
    }

    /// A missing binary yields "not found", never "mismatched": nothing was
    /// rejected, so nothing is misconfigured.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_missing_binary_reports_not_found_with_no_rejections() {
        let capture = RawCapture {
            format: CaptureFormat::CooperativeFrames,
            discovery: Default::default(),
            modules: vec![ModuleRef {
                name: "gone.dll".into(),
                path_hint: Some("no-such-binary-anywhere.dll".into()),
                ..Default::default()
            }],
            threads: Vec::new(),
        };
        let entry = symbolize(&capture).unwrap().modules.remove(0);
        assert_eq!(entry.status, ModuleSymbolStatus::NoDebugDirectory);
        assert_eq!(entry.rejected_candidates, 0);
        assert!(entry.symbol_file.is_none());
    }

    /// Numeric values are the protobuf wire surface and must never be reused.
    #[test]
    fn module_status_uses_stable_wire_numbers() {
        assert_eq!(ModuleSymbolStatus::NotFound as i32, 0);
        assert_eq!(ModuleSymbolStatus::Mismatched as i32, 2);
        assert_eq!(ModuleSymbolStatus::NoDebugDirectory as i32, 3);
    }

    #[test]
    fn frames_are_attributed_to_their_module() {
        let capture = capture_with(
            vec![module("a.dll"), module("b.dll")],
            vec![
                RawFrame {
                    module_index: 1,
                    relative_address: 0x20,
                },
                RawFrame {
                    module_index: 0,
                    relative_address: 0x10,
                },
            ],
        );
        let report = symbolize(&capture).unwrap();
        let frames = &report.threads[0].frames;

        assert_eq!(frames[0].module, "b.dll");
        assert_eq!(frames[0].relative_address, 0x20);
        assert_eq!(frames[1].module, "a.dll");
        assert_eq!(frames[1].relative_address, 0x10);
    }

    /// Offsets must survive even when nothing else could be determined — a
    /// report of bare offsets is still actionable against the right build.
    #[test]
    fn an_unknown_module_index_keeps_the_offset() {
        let capture = capture_with(
            vec![module("a.dll")],
            vec![RawFrame {
                module_index: 99,
                relative_address: 0xDEAD,
            }],
        );
        let frame = &symbolize(&capture).unwrap().threads[0].frames[0];

        assert_eq!(frame.status, FrameStatus::ModuleUnknown);
        assert_eq!(frame.relative_address, 0xDEAD);
        assert_eq!(frame.module, UNKNOWN_MODULE);
        assert!(frame.function.is_none());
    }

    /// One malformed frame must not cost the frames around it.
    #[test]
    fn a_bad_frame_does_not_discard_its_neighbours() {
        let capture = capture_with(
            vec![module("a.dll")],
            vec![
                RawFrame {
                    module_index: 0,
                    relative_address: 1,
                },
                RawFrame {
                    module_index: 7,
                    relative_address: 2,
                },
                RawFrame {
                    module_index: 0,
                    relative_address: 3,
                },
            ],
        );
        let frames = &symbolize(&capture).unwrap().threads[0].frames;

        assert_eq!(frames.len(), 3, "no frame may be dropped");
        assert_eq!(frames[0].status, FrameStatus::RawOnly);
        assert_eq!(frames[1].status, FrameStatus::ModuleUnknown);
        assert_eq!(frames[2].status, FrameStatus::RawOnly);
    }

    /// No symbol source has been consulted, so nothing may claim `Resolved`.
    #[test]
    fn nothing_is_reported_as_resolved_without_symbols() {
        let capture = capture_with(
            vec![module("a.dll")],
            vec![RawFrame {
                module_index: 0,
                relative_address: 0x40,
            }],
        );
        for frame in &symbolize(&capture).unwrap().threads[0].frames {
            assert_ne!(
                frame.status,
                FrameStatus::Resolved,
                "a name was claimed without any symbol file being read"
            );
            assert!(frame.function.is_none());
        }
    }

    #[test]
    fn python_frames_pass_through_untouched() {
        let py = PyFrame {
            file: "app.py".into(),
            line: 12,
            func: "handler".into(),
        };
        let mut capture = capture_with(vec![module("a.dll")], Vec::new());
        capture.threads[0].py_frames = vec![py.clone()];

        let thread = &symbolize(&capture).unwrap().threads[0];
        assert_eq!(thread.py_frames, vec![py]);
    }

    /// The tid is the join key for mixed-mode pairing; losing it would
    /// silently unpair the two halves the client matched up.
    #[test]
    fn thread_identity_and_order_survive() {
        let capture = RawCapture {
            format: CaptureFormat::CooperativeFrames,
            discovery: Default::default(),
            modules: vec![module("a.dll")],
            threads: vec![
                RawThread {
                    os_tid: 100,
                    name: Some("first".into()),
                    ..Default::default()
                },
                RawThread {
                    os_tid: 200,
                    name: None,
                    ..Default::default()
                },
            ],
        };
        let report = symbolize(&capture).unwrap();

        assert_eq!(report.threads.len(), 2);
        assert_eq!(report.threads[0].os_tid, 100);
        assert_eq!(report.threads[0].name.as_deref(), Some("first"));
        assert_eq!(report.threads[1].os_tid, 200);
        assert_eq!(report.threads[1].name, None);
    }

    /// End-to-end: a real binary, a real symbol file, a real function name.
    ///
    /// Everything above uses synthetic modules that can never resolve. This
    /// takes the test binary's own PDB, picks a symbol out of it, and feeds
    /// that symbol's address through the public `symbolize` entry point —
    /// so the module lookup, the cache, the status decision, and the PDB
    /// arithmetic all have to agree for the expected name to come back.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_real_address_resolves_to_a_real_function_name() {
        use crate::pdb_symbols::SymbolTable;

        let exe = std::env::current_exe().expect("current exe");
        let pdb = exe.with_extension("pdb");
        if !pdb.is_file() {
            // A silent skip here would report "symbolization works" while
            // symbolizing nothing; CI must say so instead.
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "no PDB at {} during a CI run; this test would assert nothing",
                pdb.display()
            );
            eprintln!("skipping: no PDB beside the test binary");
            return;
        }
        let Some(table) = SymbolTable::from_pdb(&pdb) else {
            eprintln!("skipping: PDB had no public function symbols");
            return;
        };
        // A symbol from this crate, so the expected name is recognizable
        // rather than an arbitrary runtime internal.
        let Some((rva, expected)) = table.symbol_containing_name("kernal_api") else {
            eprintln!("skipping: no symbol from this crate in the PDB");
            return;
        };

        let capture = RawCapture {
            format: CaptureFormat::CooperativeFrames,
            discovery: Default::default(),
            modules: vec![ModuleRef {
                name: "self".into(),
                path_hint: Some(exe.to_string_lossy().into_owned()),
                ..Default::default()
            }],
            threads: vec![RawThread {
                os_tid: 1,
                frames: vec![RawFrame {
                    module_index: 0,
                    relative_address: u64::from(rva),
                }],
                ..Default::default()
            }],
        };

        let frame = &symbolize(&capture).unwrap().threads[0].frames[0];
        assert_eq!(frame.status, FrameStatus::Resolved);
        assert_eq!(frame.function.as_deref(), Some(expected.as_str()));
        assert_eq!(
            frame.relative_address,
            u64::from(rva),
            "the offset must survive symbolization"
        );
    }

    /// The opt-in reaches the frame: real addresses get real `file:line`.
    ///
    /// Samples many of this crate's symbols and requires at least one to
    /// resolve, rather than betting on a single function carrying line info.
    /// Anchored on this crate and asserting a Rust source file, so it cannot
    /// pass by resolving an unrelated runtime internal.
    #[cfg(target_os = "windows")]
    #[test]
    fn real_addresses_resolve_to_files_and_lines_when_asked() {
        use crate::pdb_symbols::SymbolTable;

        let exe = std::env::current_exe().expect("current exe");
        let pdb = exe.with_extension("pdb");
        if !pdb.is_file() {
            // The cached build drops the linker's PDB (zackees/soldr#2148),
            // so this skips on a local box. CI must not skip silently.
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "no PDB at {} during a CI run; this test would assert nothing",
                pdb.display()
            );
            eprintln!("skipping: no PDB beside the test binary");
            return;
        }
        let Some(table) = SymbolTable::from_pdb(&pdb) else {
            eprintln!("skipping: PDB had no public function symbols");
            return;
        };
        let addresses = table.addresses_for_names_containing("kernal_api", 64);
        assert!(
            !addresses.is_empty(),
            "no symbol named this crate; the anchor is wrong, not the wiring"
        );

        let capture = capture_for_self(&exe, &addresses);
        let cache = build_symbol_cache(&capture);
        let lines = build_line_cache(&capture, &cache, true);

        let resolved: Vec<(String, u32)> = addresses
            .iter()
            .filter_map(|address| lookup_line(&cache, &lines, 0, *address))
            .collect();

        if resolved.is_empty() {
            // Every sampled symbol lacking line info is possible with a
            // mismatched local PDB, but on CI it means the wiring is dead.
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "none of {} sampled symbols resolved to a line during a CI run",
                addresses.len()
            );
            eprintln!("skipping: no sampled symbol carried a line record");
            return;
        }

        for (file, line) in &resolved {
            assert!(
                file.to_ascii_lowercase().ends_with(".rs"),
                "resolved to {file}, which is not a Rust source file"
            );
            assert!(*line > 0, "line numbers are 1-based; got {line}");
        }
    }

    /// Not asking costs nothing and yields nothing.
    ///
    /// The gate is the reason line resolution is affordable by default; a
    /// regression here would make every capture parse line programs.
    #[cfg(target_os = "windows")]
    #[test]
    fn lines_are_absent_unless_the_caller_opts_in() {
        let exe = std::env::current_exe().expect("current exe");
        let capture = capture_for_self(&exe, &[0x1000]);
        let cache = build_symbol_cache(&capture);

        let lines = build_line_cache(&capture, &cache, false);
        assert!(
            lines.iter().all(|module| module.is_empty()),
            "the opt-in was off, so no line program should have been parsed"
        );
    }

    /// A one-module capture with a frame per supplied address.
    #[cfg(target_os = "windows")]
    fn capture_for_self(exe: &std::path::Path, addresses: &[u64]) -> RawCapture {
        RawCapture {
            format: CaptureFormat::CooperativeFrames,
            discovery: Default::default(),
            modules: vec![ModuleRef {
                name: "self".into(),
                path_hint: Some(exe.to_string_lossy().into_owned()),
                ..Default::default()
            }],
            threads: vec![RawThread {
                os_tid: 1,
                frames: addresses
                    .iter()
                    .map(|address| RawFrame {
                        module_index: 0,
                        relative_address: *address,
                    })
                    .collect(),
                ..Default::default()
            }],
        }
    }

    /// A module with no symbol file must degrade, not fail.
    #[test]
    fn a_module_without_symbols_stays_raw_only() {
        let capture = RawCapture {
            format: CaptureFormat::CooperativeFrames,
            discovery: Default::default(),
            modules: vec![ModuleRef {
                name: "ghost.dll".into(),
                path_hint: Some("no-such-binary-anywhere.dll".into()),
                ..Default::default()
            }],
            threads: vec![RawThread {
                os_tid: 1,
                frames: vec![RawFrame {
                    module_index: 0,
                    relative_address: 0x40,
                }],
                ..Default::default()
            }],
        };
        let frame = &symbolize(&capture).unwrap().threads[0].frames[0];
        assert_eq!(frame.status, FrameStatus::RawOnly);
        assert!(frame.function.is_none(), "no symbols means no name");
        assert_eq!(frame.relative_address, 0x40);
    }

    /// An unimplemented path must refuse rather than return an empty report,
    /// which would read as "this crash had no threads".
    #[test]
    fn the_minidump_path_refuses_rather_than_returning_nothing() {
        let capture = RawCapture {
            format: CaptureFormat::Minidump,
            ..Default::default()
        };
        assert!(matches!(
            symbolize(&capture),
            Err(SymbolizeError::UnsupportedFormat(CaptureFormat::Minidump))
        ));
    }
}
