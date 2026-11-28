//! Post-link debug-symbol splitting: a stripped binary plus a matched symbol
//! file (#78).
//!
//! # Why this happens after linking
//!
//! Cargo's `split-debuginfo = "packed"` splits only the DWARF of the crates
//! rustc compiles in that build. The precompiled standard library ships with
//! its debug info already linked in, and every C dependency built through the
//! `cc` crate embeds DWARF in its object files before the linker ever runs.
//! Both land in the executable regardless, so the shipped artifact keeps the
//! debug info *and* gains a `.dwp` beside it — the same bytes twice, and the
//! copy every user downloads is the large one.
//!
//! Carving after the link captures what the linker actually wrote, which is
//! the only description of the shipped image that includes the parts no single
//! compiler invocation knew about.
//!
//! # Verify, do not assume
//!
//! The second half of the same footgun is that a size check is not a
//! correctness check. Adding `strip` to the release profile returns the small
//! number and silently removes the skeleton units a `.dwp` needs, leaving a
//! sidecar that packages cleanly, checksums correctly, and symbolizes nothing.
//!
//! So this operation never reports success on file sizes. [`split_debug_symbols`]
//! re-reads both artifacts it produced and refuses a pair whose debug info did
//! not actually move or whose identities do not match, and
//! [`DebugSplit::verify_resolves`] asks this crate's own symbolizer to resolve a
//! named function through the produced symbol file.
//!
//! # Report the mechanism
//!
//! The platforms genuinely differ, so the result names the [`SplitMechanism`]
//! obtained rather than pretending they are the same. A caller that needs
//! complete symbols can then refuse a mechanism that does not cover the whole
//! linked image instead of discovering months later that its sidecar never
//! covered the C libraries.
//!
//! # Build identity
//!
//! A symbol file is only useful if it can be matched to the binary it came
//! from, and matching is by build identity, never by filename. Rust does not
//! emit a GNU build-id on Linux by default: a release lane needs
//! `-C link-arg=-Wl,--build-id`, which belongs scoped to that lane rather than
//! in `.cargo/config.toml`, where it changes `RUSTFLAGS` for every build and
//! invalidates every cached compile. A binary without an identity is refused
//! by [`DebugSplitError::BuildIdentityMissing`] before any tool runs.

use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Command;

use object::{Object as _, ObjectSymbol as _};

use super::wire::{
    CaptureFormat, DiscoveryConfig, FrameStatus, ModuleRef, ModuleSymbolStatus, RawCapture,
    RawFrame, RawThread,
};
use super::{SymbolizerWorker, WorkerError};

/// Largest image or symbol file this operation will parse.
///
/// Enforced against the file length *before* reading, so an implausible input
/// is refused rather than allocated. Sized well above any real shipped
/// executable and its debug half: the artifact that motivated #78 was 73.8 MiB
/// with a 48.5 MiB sidecar.
const MAX_IMAGE_BYTES: u64 = 1024 * 1024 * 1024;

/// Bytes of a failed tool's standard error retained in a [`DebugSplitError`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAX_TOOL_STDERR_BYTES: usize = 4096;

/// How the symbol file was separated from the binary.
///
/// Every mechanism here is a post-link carve or an artifact the linker already
/// produced separately, so all of them describe the whole linked image. That
/// is the guarantee [`SplitMechanism::covers_linked_image`] exists to state:
/// a caller can require it without matching each variant, and a mechanism that
/// only covered part of the image — a Cargo `packed` split, say — would have to
/// answer `false`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SplitMechanism {
    /// `objcopy --only-keep-debug`, `strip --strip-debug`, then
    /// `objcopy --add-gnu-debuglink`.
    ///
    /// GNU binutils and the LLVM equivalents both work; whichever is on `PATH`
    /// is used.
    GnuDebugLink,
    /// `dsymutil` into a `.dSYM` bundle, then `strip -x`.
    ///
    /// The symbol file is a directory, not a file. Mach-O has no `objcopy`
    /// equivalent: `dsymutil` follows the debug map in the executable's symbol
    /// table back to the object files and links the DWARF itself.
    DsymBundle,
    /// The MSVC linker already emitted a `.pdb`; nothing is carved.
    ///
    /// Windows debug info was never in the executable, so the operation
    /// locates and validates the pair rather than producing it.
    LinkerPdb,
}

impl SplitMechanism {
    /// Whether the symbol file describes the entire linked image.
    ///
    /// True for every mechanism this operation can report. It is spelled out
    /// so a caller that needs complete symbols has something to gate on, and
    /// so a partial mechanism could never be introduced silently.
    pub const fn covers_linked_image(self) -> bool {
        match self {
            Self::GnuDebugLink | Self::DsymBundle | Self::LinkerPdb => true,
        }
    }

    /// Human-readable mechanism name used in diagnostics.
    const fn label(self) -> &'static str {
        match self {
            Self::GnuDebugLink => "gnu-debuglink",
            Self::DsymBundle => "dsym-bundle",
            Self::LinkerPdb => "linker-pdb",
        }
    }
}

impl std::fmt::Display for SplitMechanism {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// What to split, and where the symbols should go.
#[derive(Clone, Debug)]
pub struct DebugSplitRequest {
    binary: PathBuf,
    symbol_file: Option<PathBuf>,
}

impl DebugSplitRequest {
    /// Split the debug info out of this linked binary.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            symbol_file: None,
        }
    }

    /// Write the symbol file somewhere other than the platform default.
    ///
    /// The default — `<binary>.debug`, `<binary>.dSYM`, or the `.pdb` the
    /// linker recorded — is not arbitrary. It is what `--add-gnu-debuglink`
    /// and `dsymutil` look for, and what this crate's `AdjacentNative`
    /// discovery tier finds beside a module without any configuration.
    /// Moving it means the consumer has to declare it.
    pub fn symbol_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.symbol_file = Some(path.into());
        self
    }

    /// Binary this request splits.
    pub fn binary(&self) -> &Path {
        &self.binary
    }
}

/// A stripped binary and the matched symbol file carved out of it.
#[derive(Clone, Debug)]
pub struct DebugSplit {
    binary: PathBuf,
    symbol_file: PathBuf,
    mechanism: SplitMechanism,
    identity: String,
}

impl DebugSplit {
    /// The stripped binary, in place at its original path.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// The symbol file. On macOS this is a `.dSYM` bundle directory.
    pub fn symbol_file(&self) -> &Path {
        &self.symbol_file
    }

    /// Which mechanism produced this pair.
    pub fn mechanism(&self) -> SplitMechanism {
        self.mechanism
    }

    /// Build identity shared by both artifacts.
    ///
    /// The canonical spelling this crate uses everywhere a symbol file is
    /// matched to a module: `elf:<hex>`, `macho:<hex>`, or `pdb:<guid>-<age>`.
    /// It is what a caller records in a manifest and what
    /// [`ModuleRef::debug_id`](super::wire::ModuleRef::debug_id) carries.
    pub fn build_identity(&self) -> &str {
        &self.identity
    }

    /// Confirm this crate's symbolizer resolves `function` through the symbol
    /// file.
    ///
    /// The address is read from the *stripped* binary and the name comes back
    /// from the *symbol file*, so a sidecar that describes another build, or
    /// that lost the units a resolver needs, fails here rather than months
    /// later against a production crash.
    ///
    /// `function` is the linker-level symbol name — the name in the symbol
    /// table, not a demangled or source-level spelling.
    ///
    /// Resolution runs in the caller-supplied worker, off-process, because a
    /// malformed symbol file must never be parsed inside a long-lived process.
    pub async fn verify_resolves(
        &self,
        worker: &SymbolizerWorker,
        function: &str,
    ) -> Result<VerifiedResolution, DebugSplitError> {
        let module_offset = self.function_offset(function)?;
        let image = self.resolution_image()?;
        let capture = RawCapture {
            format: CaptureFormat::CooperativeFrames,
            discovery: DiscoveryConfig::default(),
            modules: vec![ModuleRef {
                name: file_name(&self.binary),
                debug_id: Some(self.identity.clone()),
                path_hint: Some(image.to_string_lossy().into_owned()),
                ..ModuleRef::default()
            }],
            threads: vec![RawThread {
                frames: vec![RawFrame {
                    module_index: 0,
                    relative_address: module_offset,
                }],
                ..RawThread::default()
            }],
        };

        let report = worker.symbolize(&capture).await?;
        let module_status = report
            .modules
            .first()
            .map_or(ModuleSymbolStatus::NotFound, |module| module.status);
        let symbol_file = report
            .modules
            .first()
            .and_then(|module| module.symbol_file.clone());
        let frame = report
            .threads
            .first()
            .and_then(|thread| thread.frames.first());
        let resolved = frame.and_then(|frame| frame.function.clone());
        let frame_status = frame.map_or(FrameStatus::RawOnly, |frame| frame.status);

        if module_status != ModuleSymbolStatus::Resolved || resolved.as_deref() != Some(function) {
            return Err(DebugSplitError::Unresolved {
                function: function.to_owned(),
                module_offset,
                module_status,
                frame_status,
                resolved,
            });
        }

        // Which file answered is part of the proof. Discovery has tiers below
        // the one this capture names -- a build-id cache, a configured store --
        // and one of them holding a same-identity file would otherwise let a
        // useless sidecar pass on someone else's symbols.
        let answered = symbol_file.as_deref();
        if self.mechanism != SplitMechanism::LinkerPdb
            && answered != Some(image.to_string_lossy().as_ref())
        {
            return Err(incomplete(
                self.mechanism,
                &format!(
                    "the symbolizer answered from {} rather than the produced symbol file",
                    answered.unwrap_or("<nothing>")
                ),
            ));
        }

        Ok(VerifiedResolution {
            function: function.to_owned(),
            module_offset,
            symbol_file: symbol_file.map_or_else(|| image.clone(), PathBuf::from),
            source_line: frame.and_then(|frame| frame.file.clone().zip(frame.line)),
        })
    }

    /// Offset of `function` from the stripped binary's image base.
    ///
    /// Read from the binary rather than the symbol file on purpose: an address
    /// taken from the sidecar and looked up in the same sidecar would prove
    /// only that the sidecar is self-consistent.
    fn function_offset(&self, function: &str) -> Result<u64, DebugSplitError> {
        let bytes = read_bounded(&self.binary)?;
        let file = parse_object(&self.binary, &bytes)?;
        let base = file.relative_address_base();
        let address = file
            .symbols()
            .chain(file.dynamic_symbols())
            .find(|symbol| !symbol.is_undefined() && symbol.name() == Ok(function))
            .map(|symbol| symbol.address())
            // A PE keeps no symbol table, so an exported name is the only
            // address this side of a PDB parser -- which stays in the worker.
            .or_else(|| {
                file.exports().ok().and_then(|exports| {
                    exports
                        .iter()
                        .find(|export| export.name() == function.as_bytes())
                        .map(object::read::Export::address)
                })
            });
        match address {
            Some(address) if address >= base => Ok(address - base),
            _ => Err(DebugSplitError::FunctionAddressUnknown {
                function: function.to_owned(),
                binary: self.binary.clone(),
            }),
        }
    }

    /// The file a symbolizer should be pointed at to exercise this split.
    ///
    /// ELF and Mach-O keep a usable symbol table inside the stripped image, so
    /// naming the image would let the image answer and leave the sidecar
    /// untested. Naming the symbol file instead makes it the only possible
    /// source. A PE has no such table: the PDB the executable names is already
    /// the only source, so the executable stays the image there.
    fn resolution_image(&self) -> Result<PathBuf, DebugSplitError> {
        match self.mechanism {
            SplitMechanism::GnuDebugLink => Ok(self.symbol_file.clone()),
            SplitMechanism::DsymBundle => Ok(dsym_dwarf_binary(&self.symbol_file, &self.binary)),
            SplitMechanism::LinkerPdb => Ok(self.binary.clone()),
        }
    }
}

/// Proof that the symbolizer resolved a known function through the split.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedResolution {
    function: String,
    module_offset: u64,
    symbol_file: PathBuf,
    source_line: Option<(String, u32)>,
}

impl VerifiedResolution {
    /// The function that was resolved.
    pub fn function(&self) -> &str {
        &self.function
    }

    /// Offset of that function from the binary's image base.
    pub fn module_offset(&self) -> u64 {
        self.module_offset
    }

    /// The symbol file the symbolizer verified and read.
    pub fn symbol_file(&self) -> &Path {
        &self.symbol_file
    }

    /// Source file and line, when the caller opted into line numbers.
    ///
    /// `None` is ordinary: `file:line` resolution costs a line-program parse
    /// per module and is off unless `KERNAL_API_SYMBOL_LINE_NUMBERS` is set
    /// for the worker.
    pub fn source_line(&self) -> Option<(&str, u32)> {
        self.source_line
            .as_ref()
            .map(|(file, line)| (file.as_str(), *line))
    }
}

/// Why a split could not be produced, or could not be trusted.
#[derive(Debug, thiserror::Error)]
pub enum DebugSplitError {
    /// This host does not have the tools the platform mechanism needs.
    ///
    /// Typed rather than silently degrading: a caller that cannot split here
    /// must be able to tell "no toolchain" from "split, but incomplete".
    #[error("{mechanism} needs {missing} on PATH", missing = missing.join(" or "))]
    MechanismUnavailable {
        /// Mechanism that would have been used.
        mechanism: SplitMechanism,
        /// Tool names probed, none of which ran.
        missing: Vec<String>,
    },
    /// The target has no mechanism in this crate.
    #[error("no debug-symbol split mechanism for this target")]
    UnsupportedTarget,
    /// The binary carries no build identity, so no symbol file could be
    /// matched to it later.
    #[error(
        "{binary} has no build identity; a Linux release lane needs \
         -C link-arg=-Wl,--build-id, which Rust does not pass by default",
        binary = binary.display()
    )]
    BuildIdentityMissing {
        /// Binary that was inspected.
        binary: PathBuf,
    },
    /// The binary has no debug info to carve out.
    ///
    /// Usually `strip` in the release profile, or a profile with `debug = 0`:
    /// splitting would then produce a sidecar that resolves nothing.
    #[error("{binary} carries no debug info to split out", binary = binary.display())]
    DebugInfoMissing {
        /// Binary that was inspected.
        binary: PathBuf,
    },
    /// The symbol file the platform expects does not exist.
    #[error("{path} does not exist", path = path.display())]
    SymbolFileMissing {
        /// Expected symbol file.
        path: PathBuf,
    },
    /// A tool ran and failed.
    #[error("{program} failed with status {status:?}: {stderr}")]
    Tool {
        /// Program that was executed.
        program: String,
        /// Exit code, when the platform reports one.
        status: Option<i32>,
        /// Bounded diagnostic from the tool.
        stderr: String,
    },
    /// A file could not be read, written, or replaced.
    #[error("{path}: {source}", path = path.display())]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Operating-system failure.
        source: std::io::Error,
    },
    /// A file was too large to inspect.
    #[error("{path} is {bytes} bytes, over the {limit}-byte inspection limit", path = path.display())]
    ImageTooLarge {
        /// Path involved.
        path: PathBuf,
        /// Actual length.
        bytes: u64,
        /// Limit enforced.
        limit: u64,
    },
    /// A produced or supplied file is not a readable object file.
    #[error("{path} is not a readable object file: {reason}", path = path.display())]
    UnreadableImage {
        /// Path involved.
        path: PathBuf,
        /// Parser diagnostic.
        reason: String,
    },
    /// The pair was produced but does not hold together.
    ///
    /// This is the check that a size comparison cannot make: debug info still
    /// in the binary, missing from the symbol file, identities that disagree,
    /// or a `.gnu_debuglink` that names or checksums another file.
    #[error("{mechanism} produced an unusable pair: {reason}")]
    IncompleteSplit {
        /// Mechanism that produced the pair.
        mechanism: SplitMechanism,
        /// What did not hold.
        reason: String,
    },
    /// The requested function has no address in the binary.
    #[error("{binary} has no symbol named {function}", binary = binary.display())]
    FunctionAddressUnknown {
        /// Linker-level name that was looked for.
        function: String,
        /// Binary that was inspected.
        binary: PathBuf,
    },
    /// The symbolizer did not resolve the function through the symbol file.
    #[error(
        "symbolizing {function} at +{module_offset:#x} returned {resolved:?} \
         (module {module_status:?}, frame {frame_status:?})"
    )]
    Unresolved {
        /// Linker-level name that was asked for.
        function: String,
        /// Offset that was symbolized.
        module_offset: u64,
        /// How far module discovery got.
        module_status: ModuleSymbolStatus,
        /// How far frame resolution got.
        frame_status: FrameStatus,
        /// Name the symbolizer returned, if any.
        resolved: Option<String>,
    },
    /// The symbolizer worker could not be run.
    #[error("symbolizer worker: {0}")]
    Worker(#[from] WorkerError),
}

/// Split the debug info out of a linked binary.
///
/// On success the binary at [`DebugSplitRequest::binary`] has been replaced by
/// its stripped self and the symbol file exists beside it. Both artifacts are
/// re-read before this returns; see the module documentation for why a size
/// check is not enough.
pub fn split_debug_symbols(request: &DebugSplitRequest) -> Result<DebugSplit, DebugSplitError> {
    #[cfg(target_os = "linux")]
    {
        split_gnu_debuglink(request)
    }
    #[cfg(target_os = "macos")]
    {
        split_dsym_bundle(request)
    }
    #[cfg(windows)]
    {
        locate_linker_pdb(request)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = request;
        Err(DebugSplitError::UnsupportedTarget)
    }
}

/// `objcopy --only-keep-debug`, `strip --strip-debug`, `--add-gnu-debuglink`.
#[cfg(target_os = "linux")]
fn split_gnu_debuglink(request: &DebugSplitRequest) -> Result<DebugSplit, DebugSplitError> {
    let mechanism = SplitMechanism::GnuDebugLink;
    let binary = absolute(&request.binary)?;
    let identity = {
        let bytes = read_bounded(&binary)?;
        let file = parse_object(&binary, &bytes)?;
        // Both gates run before any tool does: a build without an identity
        // cannot be matched to its symbols afterwards, and a build without
        // debug info would yield a sidecar that resolves nothing.
        if !has_debug_info(&file) {
            return Err(DebugSplitError::DebugInfoMissing {
                binary: binary.clone(),
            });
        }
        identity_of(&file).ok_or_else(|| DebugSplitError::BuildIdentityMissing {
            binary: binary.clone(),
        })?
    };

    let symbol_file = match &request.symbol_file {
        Some(path) => absolute(path)?,
        None => appended_extension(&binary, ".debug"),
    };
    let objcopy = probe_tool(mechanism, &["objcopy", "llvm-objcopy"])?;
    let strip = probe_tool(mechanism, &["strip", "llvm-strip"])?;
    let directory = parent_directory(&binary);

    run_tool(
        &objcopy,
        &directory,
        &[
            "--only-keep-debug".as_ref(),
            binary.as_os_str(),
            symbol_file.as_os_str(),
        ],
    )?;

    // Strip to a sibling and rename, rather than in place: a failure between
    // the two steps would otherwise leave a half-written shipped binary.
    let stripped = appended_extension(&binary, ".kernal-split-stripped");
    let linked = appended_extension(&binary, ".kernal-split-linked");
    let result = (|| {
        run_tool(
            &strip,
            &directory,
            &[
                "--strip-debug".as_ref(),
                "-o".as_ref(),
                stripped.as_os_str(),
                binary.as_os_str(),
            ],
        )?;
        // Last, and only after the symbol file is final: this step records the
        // symbol file's CRC-32 as it is at this moment.
        run_tool(
            &objcopy,
            &directory,
            &[
                debuglink_argument(&symbol_file, &directory).as_os_str(),
                stripped.as_os_str(),
                linked.as_os_str(),
            ],
        )?;
        copy_permissions(&binary, &linked)?;
        rename(&linked, &binary)
    })();
    let _ = std::fs::remove_file(&stripped);
    if result.is_err() {
        let _ = std::fs::remove_file(&linked);
    }
    result?;

    let split = DebugSplit {
        binary,
        symbol_file,
        mechanism,
        identity,
    };
    inspect_gnu_debuglink(&split)?;
    Ok(split)
}

/// Re-read the produced pair and refuse anything a size check would pass.
#[cfg(target_os = "linux")]
fn inspect_gnu_debuglink(split: &DebugSplit) -> Result<(), DebugSplitError> {
    let binary_bytes = read_bounded(&split.binary)?;
    let binary = parse_object(&split.binary, &binary_bytes)?;
    let symbol_bytes = read_bounded(&split.symbol_file)?;
    let symbols = parse_object(&split.symbol_file, &symbol_bytes)?;

    if has_debug_info(&binary) {
        return Err(incomplete(
            split.mechanism,
            "the stripped binary still carries DWARF; the debug info was duplicated, not moved",
        ));
    }
    if !has_debug_info(&symbols) {
        return Err(incomplete(
            split.mechanism,
            "the symbol file carries no DWARF, so it can resolve nothing",
        ));
    }
    if identity_of(&symbols).as_deref() != Some(split.identity.as_str()) {
        return Err(incomplete(
            split.mechanism,
            "the symbol file's build identity does not match the binary",
        ));
    }

    // `.gnu_debuglink` is how a debugger finds the sidecar without being told,
    // and its CRC-32 is how it rejects a stale one. A missing or mismatched
    // link is a pair that works only while both files stay together and are
    // never rebuilt.
    let Ok(Some((name, crc))) = binary.gnu_debuglink() else {
        return Err(incomplete(
            split.mechanism,
            "the stripped binary has no .gnu_debuglink section",
        ));
    };
    let expected_name = file_name(&split.symbol_file);
    if String::from_utf8_lossy(name) != expected_name {
        return Err(incomplete(
            split.mechanism,
            "the .gnu_debuglink names a different file than the symbol file produced",
        ));
    }
    if crc != crc32(&symbol_bytes) {
        return Err(incomplete(
            split.mechanism,
            "the .gnu_debuglink CRC-32 does not match the symbol file's bytes",
        ));
    }
    Ok(())
}

/// `dsymutil` into a bundle, then `strip -x`.
#[cfg(target_os = "macos")]
fn split_dsym_bundle(request: &DebugSplitRequest) -> Result<DebugSplit, DebugSplitError> {
    let mechanism = SplitMechanism::DsymBundle;
    let binary = absolute(&request.binary)?;
    let identity = {
        let bytes = read_bounded(&binary)?;
        let file = parse_object(&binary, &bytes)?;
        // No debug-section gate here: Mach-O keeps DWARF in the object files
        // and only a debug map in the executable, which is exactly what
        // `dsymutil` follows. Absence is checked on the bundle it produces.
        identity_of(&file).ok_or_else(|| DebugSplitError::BuildIdentityMissing {
            binary: binary.clone(),
        })?
    };

    let bundle = match &request.symbol_file {
        Some(path) => absolute(path)?,
        None => appended_extension(&binary, ".dSYM"),
    };
    let dsymutil = probe_tool(mechanism, &["dsymutil"])?;
    let strip = probe_tool(mechanism, &["strip"])?;
    let directory = parent_directory(&binary);

    run_tool(
        &dsymutil,
        &directory,
        &[binary.as_os_str(), "-o".as_ref(), bundle.as_os_str()],
    )?;
    run_tool(&strip, &directory, &["-x".as_ref(), binary.as_os_str()])?;

    let split = DebugSplit {
        binary,
        symbol_file: bundle,
        mechanism,
        identity,
    };
    inspect_dsym_bundle(&split)?;
    Ok(split)
}

/// Re-read the bundle and the stripped image, and refuse an unusable pair.
#[cfg(target_os = "macos")]
fn inspect_dsym_bundle(split: &DebugSplit) -> Result<(), DebugSplitError> {
    let dwarf = dsym_dwarf_binary(&split.symbol_file, &split.binary);
    if !dwarf.is_file() {
        return Err(incomplete(
            split.mechanism,
            "the .dSYM bundle has no DWARF binary inside it",
        ));
    }
    let bytes = read_bounded(&dwarf)?;
    let symbols = parse_object(&dwarf, &bytes)?;
    if !has_debug_info(&symbols) {
        return Err(incomplete(
            split.mechanism,
            "the .dSYM bundle carries no DWARF, so it can resolve nothing",
        ));
    }
    // The UUID is the whole matching story on Mach-O: there is no debuglink,
    // and a bundle from another build is otherwise indistinguishable.
    if identity_of(&symbols).as_deref() != Some(split.identity.as_str()) {
        return Err(incomplete(
            split.mechanism,
            "the .dSYM bundle's UUID does not match the binary",
        ));
    }
    Ok(())
}

/// Locate the `.pdb` the MSVC linker already wrote beside the executable.
#[cfg(windows)]
fn locate_linker_pdb(request: &DebugSplitRequest) -> Result<DebugSplit, DebugSplitError> {
    let mechanism = SplitMechanism::LinkerPdb;
    let binary = absolute(&request.binary)?;
    let bytes = read_bounded(&binary)?;
    let file = parse_object(&binary, &bytes)?;
    // The CodeView record is both the identity and the pointer to the symbol
    // file. Without it the executable was linked without `/DEBUG`, and no
    // `.pdb` beside it can be proven to belong to this build.
    let identity = identity_of(&file).ok_or_else(|| DebugSplitError::BuildIdentityMissing {
        binary: binary.clone(),
    })?;
    let recorded = file
        .pdb_info()
        .ok()
        .flatten()
        .map(|info| PathBuf::from(String::from_utf8_lossy(info.path()).into_owned()));

    let symbol_file = match &request.symbol_file {
        Some(path) => absolute(path)?,
        None => recorded
            .filter(|path| path.is_file())
            // A build machine's absolute path rarely survives to wherever the
            // artifact is packaged, so fall back to the conventional sibling.
            .unwrap_or_else(|| binary.with_extension("pdb")),
    };
    if !symbol_file.is_file() {
        return Err(DebugSplitError::SymbolFileMissing { path: symbol_file });
    }

    Ok(DebugSplit {
        binary,
        symbol_file,
        mechanism,
        identity,
    })
}

/// Path of the Mach-O binary inside a `.dSYM` bundle.
fn dsym_dwarf_binary(bundle: &Path, binary: &Path) -> PathBuf {
    bundle
        .join("Contents")
        .join("Resources")
        .join("DWARF")
        .join(file_name(binary))
}

/// Canonical identity spelling for whatever this object file carries.
fn identity_of(file: &object::File<'_>) -> Option<String> {
    if let Ok(Some(build_id)) = file.build_id() {
        return Some(format!("elf:{}", hex(build_id)));
    }
    if let Ok(Some(uuid)) = file.mach_uuid() {
        return Some(format!("macho:{}", hex(&uuid)));
    }
    let info = file.pdb_info().ok()??;
    // A GUID is not 16 opaque bytes: the PE stores its first three fields
    // little-endian while every other spelling of the same GUID -- including
    // the one the PDB itself reports -- is big-endian. Comparing the raw forms
    // finds them unequal for every image, which reads as "no symbols" forever
    // rather than as a bug.
    let mut guid = info.guid();
    guid[0..4].reverse();
    guid[4..6].reverse();
    guid[6..8].reverse();
    Some(format!("pdb:{}-{}", hex(&guid), info.age()))
}

/// Whether the file carries DWARF, under the ELF or the Mach-O spelling.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn has_debug_info(file: &object::File<'_>) -> bool {
    file.section_by_name(".debug_info").is_some() || file.section_by_name("__debug_info").is_some()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn incomplete(mechanism: SplitMechanism, reason: &str) -> DebugSplitError {
    DebugSplitError::IncompleteSplit {
        mechanism,
        reason: reason.to_owned(),
    }
}

fn absolute(path: &Path) -> Result<PathBuf, DebugSplitError> {
    std::path::absolute(path).map_err(|source| DebugSplitError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn parent_directory(binary: &Path) -> PathBuf {
    binary
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Append a suffix to the whole file name, keeping the existing extension.
///
/// `Path::with_extension` would turn `libfoo.so` into `libfoo.debug` and make
/// two differently versioned libraries share one symbol file name.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn appended_extension(path: &Path, suffix: &str) -> PathBuf {
    let mut appended = path.to_path_buf();
    appended.as_mut_os_string().push(suffix);
    appended
}

/// The `--add-gnu-debuglink` argument, relative when it can be.
///
/// Both GNU objcopy and `llvm-objcopy` record only the basename, which a
/// debugger then resolves against the binary's own directory -- so the pair
/// stays relocatable however this is spelled. A sibling symbol file is still
/// passed by bare name, and one the caller placed elsewhere by full path,
/// because objcopy has to read those bytes to checksum them.
#[cfg(target_os = "linux")]
fn debuglink_argument(symbol_file: &Path, directory: &Path) -> std::ffi::OsString {
    let mut argument = std::ffi::OsString::from("--add-gnu-debuglink=");
    if symbol_file.parent() == Some(directory) {
        argument.push(symbol_file.file_name().unwrap_or(symbol_file.as_os_str()));
    } else {
        argument.push(symbol_file.as_os_str());
    }
    argument
}

/// First tool on `PATH` that runs, or a typed unavailability.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn probe_tool(mechanism: SplitMechanism, names: &[&str]) -> Result<PathBuf, DebugSplitError> {
    for name in names {
        let ran = Command::new(name)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok();
        if ran {
            return Ok(PathBuf::from(name));
        }
    }
    Err(DebugSplitError::MechanismUnavailable {
        mechanism,
        missing: names.iter().map(|name| (*name).to_owned()).collect(),
    })
}

/// Run one tool, in the binary's directory, and turn a failure into a typed
/// error carrying its diagnostic.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_tool(
    program: &Path,
    directory: &Path,
    arguments: &[&std::ffi::OsStr],
) -> Result<(), DebugSplitError> {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(directory)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|source| DebugSplitError::Io {
            path: program.to_path_buf(),
            source,
        })?;
    if output.status.success() {
        return Ok(());
    }
    let mut stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    stderr.truncate(
        (0..=MAX_TOOL_STDERR_BYTES.min(stderr.len()))
            .rev()
            .find(|index| stderr.is_char_boundary(*index))
            .unwrap_or(0),
    );
    Err(DebugSplitError::Tool {
        program: program.to_string_lossy().into_owned(),
        status: output.status.code(),
        stderr,
    })
}

#[cfg(target_os = "linux")]
fn copy_permissions(from: &Path, to: &Path) -> Result<(), DebugSplitError> {
    let permissions = std::fs::metadata(from)
        .map_err(|source| DebugSplitError::Io {
            path: from.to_path_buf(),
            source,
        })?
        .permissions();
    std::fs::set_permissions(to, permissions).map_err(|source| DebugSplitError::Io {
        path: to.to_path_buf(),
        source,
    })
}

#[cfg(target_os = "linux")]
fn rename(from: &Path, to: &Path) -> Result<(), DebugSplitError> {
    std::fs::rename(from, to).map_err(|source| DebugSplitError::Io {
        path: to.to_path_buf(),
        source,
    })
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, DebugSplitError> {
    use std::io::Read as _;

    let file = std::fs::File::open(path).map_err(|source| DebugSplitError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let length = file
        .metadata()
        .map_err(|source| DebugSplitError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if length > MAX_IMAGE_BYTES {
        return Err(DebugSplitError::ImageTooLarge {
            path: path.to_path_buf(),
            bytes: length,
            limit: MAX_IMAGE_BYTES,
        });
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(MAX_IMAGE_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|source| DebugSplitError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(bytes)
}

fn parse_object<'data>(
    path: &Path,
    bytes: &'data [u8],
) -> Result<object::File<'data>, DebugSplitError> {
    object::File::parse(bytes).map_err(|error| DebugSplitError::UnreadableImage {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })
}

/// Reflected IEEE CRC-32, the checksum `.gnu_debuglink` records.
#[cfg(any(target_os = "linux", test))]
fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0_u32; 256];
        let mut index = 0;
        while index < 256 {
            let mut value = index as u32;
            let mut bit = 0;
            while bit < 8 {
                value = if value & 1 == 1 {
                    0xedb8_8320 ^ (value >> 1)
                } else {
                    value >> 1
                };
                bit += 1;
            }
            table[index] = value;
            index += 1;
        }
        table
    };

    let mut crc = 0xffff_ffff_u32;
    for byte in bytes {
        crc = TABLE[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checksum_matches_the_published_crc32_vector() {
        // "123456789" is the standard IEEE CRC-32 check value. Getting this
        // wrong would reject every correct `.gnu_debuglink` this operation
        // writes, which would look like a broken objcopy.
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn the_symbol_file_name_keeps_the_binary_extension() {
        // `with_extension` would collapse `libfoo.so.1` and `libfoo.so.2`
        // onto one symbol file name.
        assert_eq!(
            appended_extension(Path::new("/opt/lib/libfoo.so.1"), ".debug"),
            PathBuf::from("/opt/lib/libfoo.so.1.debug")
        );
    }

    #[test]
    fn every_reported_mechanism_covers_the_whole_linked_image() {
        // The point of reporting a mechanism is that a caller can refuse a
        // partial one. Nothing this operation produces is partial, and this
        // test is what keeps that true.
        for mechanism in [
            SplitMechanism::GnuDebugLink,
            SplitMechanism::DsymBundle,
            SplitMechanism::LinkerPdb,
        ] {
            assert!(mechanism.covers_linked_image(), "{mechanism}");
        }
    }

    #[test]
    fn the_dsym_symbol_file_is_the_binary_inside_the_bundle() {
        // A `.dSYM` is a directory; the parser needs the Mach-O inside it,
        // and that is also the path this crate's discovery tier looks for.
        assert_eq!(
            dsym_dwarf_binary(Path::new("/tmp/app.dSYM"), Path::new("/tmp/app")),
            PathBuf::from("/tmp/app.dSYM/Contents/Resources/DWARF/app")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_sibling_symbol_file_is_linked_by_bare_name() {
        // The recorded name is resolved against the binary's directory, so a
        // bare name is what lets the pair be moved together.
        assert_eq!(
            debuglink_argument(Path::new("/opt/app.debug"), Path::new("/opt")),
            std::ffi::OsString::from("--add-gnu-debuglink=app.debug")
        );
        assert_eq!(
            debuglink_argument(Path::new("/elsewhere/app.debug"), Path::new("/opt")),
            std::ffi::OsString::from("--add-gnu-debuglink=/elsewhere/app.debug")
        );
    }
}
