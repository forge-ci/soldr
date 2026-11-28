//! Turn raw captures into return addresses (#635).
//!
//! # This runs after every thread is resumed
//!
//! Unwinding never touches a live thread. It reads only the register values
//! and stack bytes copied during the suspend window, so it can allocate,
//! take locks, and take as long as it needs — the target is already running.
//! That separation is the reason the capture path can stay as small as it is.
//!
//! The mechanism is `read_stack`: framehop asks for a `u64` at an address, and
//! we answer out of the copied slice at `addr - stack_pointer`. Addresses
//! outside the copied window return `Err`, which framehop treats as the end of
//! what it can walk. A truncated capture therefore yields a shorter stack
//! rather than a wrong one.
//!
//! # Addresses, not symbols
//!
//! The output is return addresses. Resolving them to function names is
//! symbolization, which happens off-process in a later slice.

use std::ops::Range;

#[cfg(windows)]
use framehop::ModuleSectionInfo;
use framehop::{Module, Unwinder};

#[cfg(target_arch = "aarch64")]
use framehop::aarch64::{
    CacheAarch64 as ArchCache, UnwindRegsAarch64 as ArchRegs, UnwinderAarch64 as ArchUnwinder,
};
#[cfg(target_arch = "x86_64")]
use framehop::x86_64::{
    CacheX86_64 as ArchCache, UnwindRegsX86_64 as ArchRegs, UnwinderX86_64 as ArchUnwinder,
};

/// Build the architecture's register set from a captured sample.
///
/// The two constructors take different triples, and the difference is easy to
/// get wrong silently: x86_64 wants the instruction pointer, aarch64 wants the
/// **link register**. A leaf frame's return address lives in LR on aarch64, so
/// passing the PC there yields a plausible-looking but wrong innermost frame.
#[cfg(target_arch = "x86_64")]
fn arch_regs(sample: &ThreadSample) -> ArchRegs {
    ArchRegs::new(
        sample.instruction_pointer,
        sample.stack_pointer,
        sample.frame_pointer,
    )
}

#[cfg(all(target_arch = "aarch64", not(target_os = "macos")))]
fn arch_regs(sample: &ThreadSample) -> ArchRegs {
    ArchRegs::new(
        // Falls back to the PC only if the capture had no LR, which would
        // itself be a capture bug rather than a normal state.
        sample.link_register.unwrap_or(sample.instruction_pointer),
        sample.stack_pointer,
        sample.frame_pointer,
    )
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn arch_regs(sample: &ThreadSample) -> ArchRegs {
    let mask = framehop::aarch64::PtrAuthMask::new_24_40();
    ArchRegs::new_with_ptr_auth_mask(
        mask,
        sample.link_register.unwrap_or(sample.instruction_pointer),
        sample.stack_pointer,
        sample.frame_pointer,
    )
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn instruction_pointer(sample: &ThreadSample) -> u64 {
    framehop::aarch64::PtrAuthMask::new_24_40().strip_ptr_auth(sample.instruction_pointer)
}

#[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
fn instruction_pointer(sample: &ThreadSample) -> u64 {
    sample.instruction_pointer
}

use super::modules::LoadedModule;
use super::{Snapshot, ThreadSample};

/// Sections framehop may ask for. `.pdata` plus `.rdata`/`.xdata` carry the
/// x86_64 unwind tables; `.text` anchors code addresses.
#[cfg(windows)]
const WANTED_SECTIONS: &[&str] = &[".text", ".pdata", ".rdata", ".xdata"];

/// Adapts a [`LoadedModule`] to framehop's section interface.
///
/// Section bytes are read straight from the mapped image — the module is in
/// our own address space, so this is a slice, not file I/O.
#[cfg(windows)]
struct MappedModuleSections {
    base: u64,
    sections: Vec<(String, Range<u64>)>,
}

#[cfg(windows)]
impl MappedModuleSections {
    fn new(module: &LoadedModule) -> Self {
        Self {
            base: module.base,
            sections: module
                .sections
                .iter()
                .filter(|s| WANTED_SECTIONS.contains(&s.name.as_str()))
                .map(|s| (s.name.clone(), s.range.clone()))
                .collect(),
        }
    }

    fn find(&self, name: &[u8]) -> Option<Range<u64>> {
        let name = std::str::from_utf8(name).ok()?;
        self.sections
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, r)| r.clone())
    }
}

#[cfg(windows)]
impl ModuleSectionInfo<Vec<u8>> for MappedModuleSections {
    fn base_svma(&self) -> u64 {
        // For PE this is the image base, and because we read the *mapped*
        // image its stated and actual bases coincide.
        self.base
    }

    fn section_svma_range(&mut self, name: &[u8]) -> Option<Range<u64>> {
        self.find(name)
    }

    fn section_data(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        let range = self.find(name)?;
        let len = usize::try_from(range.end.saturating_sub(range.start)).ok()?;
        if len == 0 {
            return None;
        }
        // Safety: the range came from the mapped module's own section table,
        // so it is committed memory inside this process for as long as the
        // module stays loaded.
        #[allow(unsafe_code)]
        let bytes = unsafe { std::slice::from_raw_parts(range.start as *const u8, len) }.to_vec();
        Some(bytes)
    }
}

/// Build an unwinder covering `modules`.
///
/// Crate-private: the return type is a framehop type, so a public signature
/// here would make a client speak backend vocabulary. `resolve_frames` is the
/// facade-typed entry point.
#[cfg(windows)]
pub(crate) fn build_unwinder(modules: &[LoadedModule]) -> ArchUnwinder<Vec<u8>> {
    let mut unwinder = ArchUnwinder::new();
    for module in modules {
        let range = module.range();
        unwinder.add_module(Module::new(
            format!("{:#x}", module.base),
            range.clone(),
            module.base,
            MappedModuleSections::new(module),
        ));
    }
    unwinder
}

/// Unwind one captured thread into return addresses.
///
/// Reads only `sample`'s copied bytes; the thread itself is long since
/// resumed.
///
/// Crate-private for the same reason as `build_unwinder`: the unwinder and
/// cache parameters are framehop types.
pub(crate) fn unwind_sample(
    unwinder: &ArchUnwinder<Vec<u8>>,
    cache: &mut ArchCache,
    sample: &ThreadSample,
    _modules: &[LoadedModule],
) -> Vec<u64> {
    let sp = sample.stack_pointer;
    let bytes = &sample.stack_bytes;

    // Answer stack reads out of the copy. Anything outside the captured window
    // is Err, which ends the walk — a truncated capture yields a shorter
    // stack, never a fabricated one.
    let mut read_stack = |addr: u64| -> Result<u64, ()> {
        let offset = addr.checked_sub(sp).ok_or(())?;
        let offset = usize::try_from(offset).map_err(|_| ())?;
        let end = offset.checked_add(8).ok_or(())?;
        if end > bytes.len() {
            return Err(());
        }
        let mut word = [0u8; 8];
        word.copy_from_slice(&bytes[offset..end]);
        Ok(u64::from_le_bytes(word))
    };

    let regs = arch_regs(sample);

    let mut frames = Vec::new();
    let mut iter = unwinder.iter_frames(instruction_pointer(sample), regs, cache, &mut read_stack);
    while let Ok(Some(frame)) = iter.next() {
        frames.push(frame.address());
    }

    // Coverage instrumentation and hand-written assembly can expose valid
    // frame-pointer chains that are not described by usable ELF/Mach-O CFI.
    // Recover those callers conservatively from the already-copied stack.
    // Windows deliberately remains metadata-only so its PE unwind-table path
    // is still exercised by the known-stack acceptance test.
    #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
    if let Some(suspect_from) = frames
        .iter()
        .skip(1)
        .position(|address| {
            !_modules
                .iter()
                .any(|module| module.contains_executable(*address))
        })
        .map(|index| index + 1)
        .or((frames.len() <= 1).then_some(frames.len()))
    {
        let recovered = frame_pointer_fallback(sample, |address| {
            _modules
                .iter()
                .any(|module| module.contains_executable(address))
        });
        replace_suspect_callers(&mut frames, suspect_from, recovered);
    }

    frames
}

/// Replace an invalid metadata-derived caller tail with a validated chain.
#[cfg(all(
    target_arch = "x86_64",
    any(test, target_os = "linux", target_os = "macos")
))]
fn replace_suspect_callers(frames: &mut Vec<u64>, suspect_from: usize, recovered: Vec<u64>) {
    let valid_prefix_len = suspect_from.min(frames.len());
    let valid_callers = valid_prefix_len.saturating_sub(1);
    let overlap = if valid_callers == 0 {
        0
    } else {
        (1..=valid_callers.min(recovered.len()))
            .rev()
            .find(|&len| frames[valid_prefix_len - len..valid_prefix_len] == recovered[..len])
            .unwrap_or(0)
    };

    // Always discard the known-invalid metadata tail. With no metadata caller
    // prefix, the independently validated chain can be used in full. Otherwise
    // extend only when the two unwind methods overlap, preserving frameless
    // metadata-derived callers that are absent from an RBP chain.
    frames.truncate(valid_prefix_len);
    if valid_callers == 0 || overlap != 0 {
        frames.extend_from_slice(&recovered[overlap..]);
    }
}

/// Walk a conventional x86_64 RBP chain inside the bounded stack copy.
///
/// Every read is range-checked. Alignment and strictly increasing frame
/// pointers reject arbitrary general-purpose RBP values, every return address
/// must satisfy `is_return_address`, and the depth cap makes corrupt cyclic
/// chains terminate.
#[cfg(all(
    target_arch = "x86_64",
    any(test, target_os = "linux", target_os = "macos")
))]
fn frame_pointer_fallback(
    sample: &ThreadSample,
    mut is_return_address: impl FnMut(u64) -> bool,
) -> Vec<u64> {
    const MAX_FRAMES: usize = 256;

    let stack_start = sample.stack_pointer;
    let Some(stack_end) = stack_start.checked_add(sample.stack_bytes.len() as u64) else {
        return Vec::new();
    };
    let mut frame_pointer = sample.frame_pointer;
    let mut frames = Vec::new();

    let read_word = |address: u64| -> Option<u64> {
        if !address.is_multiple_of(8) {
            return None;
        }
        let offset = usize::try_from(address.checked_sub(stack_start)?).ok()?;
        let end = offset.checked_add(8)?;
        let bytes = sample.stack_bytes.get(offset..end)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    };

    for _ in 0..MAX_FRAMES {
        if frame_pointer < stack_start
            || frame_pointer
                .checked_add(16)
                .is_none_or(|end| end > stack_end)
        {
            break;
        }
        let Some(previous) = read_word(frame_pointer) else {
            break;
        };
        let Some(return_address) = read_word(frame_pointer + 8) else {
            break;
        };
        if return_address == 0 || !is_return_address(return_address) {
            break;
        }
        frames.push(return_address);
        if previous <= frame_pointer {
            break;
        }
        frame_pointer = previous;
    }

    frames
}

#[cfg(all(test, target_arch = "x86_64"))]
mod frame_pointer_tests {
    use super::*;
    use crate::snapshot::CaptureKind;

    fn sample(frame_pointer: u64, stack_bytes: Vec<u8>) -> ThreadSample {
        ThreadSample {
            os_tid: 1,
            stack_pointer: 0x1000,
            instruction_pointer: 0x2000,
            frame_pointer,
            link_register: None,
            stack_bytes,
            truncated: false,
            kind: CaptureKind::RawContext,
            frames: Vec::new(),
        }
    }

    fn write_word(stack: &mut [u8], address: u64, value: u64) {
        let offset = usize::try_from(address - 0x1000).unwrap();
        stack[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn frame_pointer_fallback_walks_a_bounded_monotonic_chain() {
        let mut stack = vec![0u8; 64];
        write_word(&mut stack, 0x1000, 0x1020);
        write_word(&mut stack, 0x1008, 0xaaaa);
        write_word(&mut stack, 0x1020, 0);
        write_word(&mut stack, 0x1028, 0xbbbb);

        assert_eq!(
            frame_pointer_fallback(&sample(0x1000, stack), |_| true),
            vec![0xaaaa, 0xbbbb]
        );
    }

    #[test]
    fn frame_pointer_fallback_rejects_unaligned_or_backward_chains() {
        let mut stack = vec![0u8; 64];
        write_word(&mut stack, 0x1000, 0x1000);
        write_word(&mut stack, 0x1008, 0xaaaa);

        assert!(frame_pointer_fallback(&sample(0x1001, stack.clone()), |_| true).is_empty());
        assert_eq!(
            frame_pointer_fallback(&sample(0x1000, stack), |_| true),
            vec![0xaaaa]
        );
    }

    #[test]
    fn frame_pointer_fallback_stops_at_an_unattributed_return_address() {
        let mut stack = vec![0u8; 64];
        write_word(&mut stack, 0x1000, 0x1020);
        write_word(&mut stack, 0x1008, 0xaaaa);
        write_word(&mut stack, 0x1020, 0);
        write_word(&mut stack, 0x1028, 0xbbbb);

        assert_eq!(
            frame_pointer_fallback(&sample(0x1000, stack), |address| address == 0xaaaa),
            vec![0xaaaa]
        );
    }

    #[test]
    fn recovered_chain_replaces_the_suspect_framehop_tail() {
        let mut frames = vec![0x2000, 0xdead];

        replace_suspect_callers(&mut frames, 1, vec![0xaaaa, 0xbbbb]);

        assert_eq!(frames, vec![0x2000, 0xaaaa, 0xbbbb]);
    }

    #[test]
    fn recovered_chain_preserves_duplicates_and_valid_framehop_prefix() {
        let mut frames = vec![0x2000, 0xaaaa, 0xdead];

        replace_suspect_callers(&mut frames, 2, vec![0xaaaa, 0xaaaa, 0xbbbb]);

        assert_eq!(frames, vec![0x2000, 0xaaaa, 0xaaaa, 0xbbbb]);
    }

    #[test]
    fn recovered_chain_without_overlap_keeps_only_the_valid_prefix() {
        let mut frames = vec![0x2000, 0x1111, 0xdead];

        replace_suspect_callers(&mut frames, 2, vec![0xaaaa, 0xbbbb]);

        assert_eq!(frames, vec![0x2000, 0x1111]);
    }
}

/// Unwind every sample in `snapshot`, recording the result.
///
/// Sets `frames_resolved` only here — the one place frames actually exist.
#[cfg(windows)]
pub fn resolve_frames(snapshot: &mut Snapshot, modules: &[LoadedModule]) {
    let unwinder = build_unwinder(modules);
    let mut cache = ArchCache::new();

    for sample in &mut snapshot.threads {
        sample.frames = unwind_sample(&unwinder, &mut cache, sample, modules);
    }
    snapshot.frames_resolved = true;
}

/// Add Linux ELF or macOS Mach-O unwind metadata from the modules currently
/// mapped in this process. Object parsing happens only after capture, when
/// every sibling is running again.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn build_unix_unwinder(modules: &[LoadedModule]) -> ArchUnwinder<Vec<u8>> {
    use framehop::ExplicitModuleSectionInfo;
    #[cfg(target_os = "macos")]
    use object::ObjectSegment;
    use object::{Object, ObjectSection};

    fn section(file: &object::File<'_>, names: &[&str]) -> (Option<Range<u64>>, Option<Vec<u8>>) {
        for section in file.sections() {
            let Ok(name) = section.name() else {
                continue;
            };
            if !names.contains(&name) {
                continue;
            }
            let range = section.address()..section.address().saturating_add(section.size());
            let data = section
                .uncompressed_data()
                .ok()
                .map(|data| data.into_owned());
            return (Some(range), data);
        }
        (None, None)
    }

    let mut unwinder = ArchUnwinder::new();
    for module in modules {
        let Some(path) = module.path.as_deref() else {
            continue;
        };
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        let Ok(file) = object::File::parse(data.as_slice()) else {
            continue;
        };

        let (text_svma, text) = section(&file, &[".text", "__text"]);
        let (eh_frame_svma, eh_frame) = section(&file, &[".eh_frame", "__eh_frame"]);
        let (eh_frame_hdr_svma, eh_frame_hdr) =
            section(&file, &[".eh_frame_hdr", "__eh_frame_hdr"]);
        let (_, unwind_info) = section(&file, &["__unwind_info"]);
        let (stubs_svma, _) = section(&file, &["__stubs"]);
        let (stub_helper_svma, _) = section(&file, &["__stub_helper"]);
        let (got_svma, _) = section(&file, &[".got", "__got"]);

        #[cfg(target_os = "linux")]
        let base_svma = 0;
        #[cfg(target_os = "macos")]
        let base_svma = file.relative_address_base();

        #[cfg(target_os = "macos")]
        let mut text_segment_svma = None;
        #[cfg(not(target_os = "macos"))]
        let text_segment_svma = None;
        #[cfg(target_os = "macos")]
        let mut text_segment = None;
        #[cfg(not(target_os = "macos"))]
        let text_segment = None;
        #[cfg(target_os = "macos")]
        for segment in file.segments() {
            if segment.name().ok().flatten() == Some("__TEXT") {
                text_segment_svma =
                    Some(segment.address()..segment.address().saturating_add(segment.size()));
                text_segment = segment.data().ok().map(ToOwned::to_owned);
                break;
            }
        }

        let info = ExplicitModuleSectionInfo {
            base_svma,
            text_svma,
            text,
            stubs_svma,
            stub_helper_svma,
            got_svma,
            unwind_info,
            eh_frame_svma,
            eh_frame,
            eh_frame_hdr_svma,
            eh_frame_hdr,
            text_segment_svma,
            text_segment,
            ..Default::default()
        };
        unwinder.add_module(Module::new(
            path.to_owned(),
            module.range(),
            module.base,
            info,
        ));
    }
    unwinder
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
/// Resolve every raw sample against the current process's ELF/Mach-O images.
///
/// One-shot: builds the module inventory and unwinder, uses them once, and
/// drops them. A session that resolves repeatedly should hold a
/// [`FrameResolver`] instead, which is the whole reason that type exists.
pub fn resolve_frames_for_current_process(snapshot: &mut Snapshot) -> std::io::Result<()> {
    FrameResolver::new().resolve(snapshot)
}

/// A frame resolver that keeps its module inventory and unwinder between
/// captures.
///
/// Building those costs far more than the capture they describe. Measured on
/// a debug `--all-features` build, one `resolve_frames_for_current_process`
/// spent ~736 ms: ~560 ms enumerating modules and ~176 ms constructing the
/// unwinder. A session that called it per tick therefore sampled at whatever
/// rate 736 ms per capture allows, no matter what `hz` asked for (#131).
///
/// The image set is not fixed for the life of a process -- `dlopen` adds one,
/// `dlclose` removes one -- so reuse is conditional rather than unconditional:
/// every call recomputes a cheap digest of the mapped images and rebuilds only
/// when it changes. That check reads the mapping table and hashes it without
/// opening or parsing a single image, so it costs a fraction of a rebuild
/// rather than being a cheaper version of one.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct FrameResolver {
    /// Set once the inventory has been built. Until then `signature` means
    /// nothing and the first `refresh` builds unconditionally.
    built: bool,
    signature: u64,
    modules: std::sync::Arc<Vec<LoadedModule>>,
    unwinder: ArchUnwinder<Vec<u8>>,
    cached_threads: std::collections::BTreeMap<u64, CachedThread>,
    next_unwind_index: usize,
    /// How many times the inventory has been rebuilt. Exists so a test can
    /// assert the cache's behaviour directly instead of inferring it from
    /// timing or from a module count that legitimately excludes images
    /// without resolvable unwind metadata.
    #[cfg(test)]
    rebuilds: u64,
}

// The neutral cache shape is shared source even where a host does not build
// the Unix FrameResolver; selecting its OS in this file would violate the
// platform boundary.
#[allow(dead_code)]
struct CachedThread {
    instruction_pointer: u64,
    stack_pointer: u64,
    frame_pointer: u64,
    link_register: Option<u64>,
    truncated: bool,
    stack_bytes: Vec<u8>,
    frames: Vec<u64>,
    fresh: bool,
}

#[allow(dead_code)]
impl CachedThread {
    fn matches(&self, sample: &super::ThreadSample) -> bool {
        self.instruction_pointer == sample.instruction_pointer
            && self.stack_pointer == sample.stack_pointer
            && self.frame_pointer == sample.frame_pointer
            && self.link_register == sample.link_register
            && self.truncated == sample.truncated
            && self.stack_bytes == sample.stack_bytes
    }

    fn from_sample(sample: &super::ThreadSample) -> Self {
        Self {
            instruction_pointer: sample.instruction_pointer,
            stack_pointer: sample.stack_pointer,
            frame_pointer: sample.frame_pointer,
            link_register: sample.link_register,
            truncated: sample.truncated,
            stack_bytes: sample.stack_bytes.clone(),
            frames: sample.frames.clone(),
            fresh: true,
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Default for FrameResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl FrameResolver {
    /// Prepare a resolver. The inventory is built by the first [`Self::resolve`].
    ///
    /// Deliberately does no work: building here would bake in a signature
    /// taken before the caller's own first capture, and the very next
    /// `refresh` would see a different one and rebuild immediately --
    /// paying for the inventory twice on the first tick, which is exactly
    /// what the first version of this type did.
    pub fn new() -> Self {
        Self {
            built: false,
            signature: 0,
            modules: std::sync::Arc::new(Vec::new()),
            unwinder: ArchUnwinder::new(),
            cached_threads: std::collections::BTreeMap::new(),
            next_unwind_index: 0,
            #[cfg(test)]
            rebuilds: 0,
        }
    }

    /// Build the inventory, or rebuild it if the mapped images changed.
    fn refresh(&mut self) -> std::io::Result<()> {
        let signature = super::modules::loaded_image_signature()?;
        if self.built && signature == self.signature {
            return Ok(());
        }
        self.built = true;
        self.modules = std::sync::Arc::new(super::modules::enumerate_modules()?);
        self.unwinder = build_unix_unwinder(&self.modules);
        self.cached_threads.clear();
        self.next_unwind_index = 0;
        self.signature = signature;
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
        Ok(())
    }

    /// Resolve every raw sample in `snapshot` against the current images.
    pub fn resolve(&mut self, snapshot: &mut Snapshot) -> std::io::Result<()> {
        self.resolve_with_limit(snapshot, None)
    }

    /// Bound changed-stack unwinds only for crash sampling. Unselected
    /// changed threads outside the window report only their live IP.
    pub(crate) fn resolve_with_limit(
        &mut self,
        snapshot: &mut Snapshot,
        max_unwinds_per_tick: Option<usize>,
    ) -> std::io::Result<()> {
        self.refresh()?;
        let mut cache = ArchCache::new();
        let mut next = std::collections::BTreeMap::new();
        // The rotating window prevents busy low-TID threads from starving
        // newer threads, while an uncached thread still contributes its IP.
        let thread_count = snapshot.threads.len();
        let start = self.next_unwind_index % thread_count.max(1);
        for (index, sample) in snapshot.threads.iter_mut().enumerate() {
            let previous = self.cached_threads.get(&sample.os_tid);
            let unchanged = previous.is_some_and(|cached| cached.fresh && cached.matches(sample));
            let in_window = max_unwinds_per_tick
                .is_none_or(|limit| (index + thread_count - start) % thread_count.max(1) < limit);
            let fresh = unchanged || in_window;
            sample.frames = if unchanged {
                previous.expect("unchanged implies cached").frames.clone()
            } else if in_window {
                unwind_sample(&self.unwinder, &mut cache, sample, &self.modules)
            } else {
                vec![sample.instruction_pointer]
            };
            let mut cached = CachedThread::from_sample(sample);
            cached.fresh = fresh;
            next.insert(sample.os_tid, cached);
        }
        self.next_unwind_index =
            (start + max_unwinds_per_tick.unwrap_or(thread_count)) % thread_count.max(1);
        self.cached_threads = next;
        snapshot.frames_resolved = true;
        Ok(())
    }

    /// How many times the inventory has been rebuilt since construction.
    #[cfg(test)]
    pub(crate) fn rebuild_count(&self) -> u64 {
        self.rebuilds
    }

    /// The number of modules in the cached inventory.
    ///
    /// Exposed so a caller can observe that the cache is being reused rather
    /// than rebuilt, and so tests can assert the invalidation story.
    pub fn module_count(&self) -> usize {
        self.modules.len()
    }

    /// Reuse this resolver's inventory for frame attribution after capture.
    pub(crate) fn modules(&self) -> std::sync::Arc<Vec<LoadedModule>> {
        std::sync::Arc::clone(&self.modules)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::super::modules::{enumerate_modules, module_for_address};
    use super::super::{capture_all_threads, SnapshotConfig};
    use super::*;

    #[inline(never)]
    fn inner_frame(flag: &std::sync::atomic::AtomicBool) {
        // Spin briefly so the sibling capture observes this frame on the stack.
        while !flag.load(std::sync::atomic::Ordering::Relaxed) {
            std::hint::spin_loop();
        }
    }

    #[test]
    fn unwinder_covers_every_enumerated_module() {
        let modules = enumerate_modules().expect("modules");
        let unwinder = build_unwinder(&modules);
        // Construction must not panic and must accept every module we found.
        // (framehop has no public module count, so this asserts the build path
        // is total rather than a specific number.)
        let _ = unwinder;
        assert!(!modules.is_empty());
    }

    #[test]
    fn reads_outside_the_captured_window_end_the_walk() {
        let sample = ThreadSample {
            os_tid: 1,
            stack_pointer: 0x1000,
            instruction_pointer: 0x2000,
            frame_pointer: 0,
            link_register: None,
            // Deliberately tiny: any read past 16 bytes must fail rather than
            // read adjacent memory.
            stack_bytes: vec![0u8; 16],
            truncated: true,
            kind: super::super::CaptureKind::RawContext,
            frames: Vec::new(),
        };
        let modules = enumerate_modules().expect("modules");
        let unwinder = build_unwinder(&modules);
        let mut cache = ArchCache::new();

        // Must terminate, not panic or spin, on a stack it cannot follow.
        let frames = unwind_sample(&unwinder, &mut cache, &sample, &modules);
        assert!(
            frames.len() <= 2,
            "a 16-byte stack cannot yield a deep walk, got {}",
            frames.len()
        );
    }

    /// The real check: unwind a live capture and confirm at least one returned
    /// address lands inside a module's `.text`.
    ///
    /// The oracle is the independently-verified module inventory from #700 —
    /// the unwinder is not allowed to confirm itself.
    #[test]
    fn unwound_addresses_fall_inside_known_text_sections() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || inner_frame(&stop))
        };
        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut snapshot = capture_all_threads(&SnapshotConfig::default()).expect("capture");
        let modules = enumerate_modules().expect("modules");
        resolve_frames(&mut snapshot, &modules);

        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();

        assert!(snapshot.frames_resolved, "resolve_frames must mark this");

        let mut in_text = 0usize;
        for sample in &snapshot.threads {
            for &addr in &sample.frames {
                if let Some(m) = module_for_address(&modules, addr) {
                    if m.section(".text").is_some_and(|t| t.range.contains(&addr)) {
                        in_text += 1;
                    }
                }
            }
        }

        assert!(
            in_text > 0,
            "no unwound address landed in any module's .text; frames were {:?}",
            snapshot
                .threads
                .iter()
                .map(|s| s.frames.len())
                .collect::<Vec<_>>()
        );
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod resolver_tests {
    use super::*;
    use crate::snapshot::{capture_all_threads, CaptureKind, SnapshotConfig};

    fn capture() -> Snapshot {
        capture_all_threads(&SnapshotConfig::default()).expect("capture")
    }

    /// Serialize these tests. They observe a *process-wide* signal -- the
    /// mapped image table -- and the invalidation tests change it deliberately,
    /// so running them concurrently makes one test's rebuild look like
    /// another's. Holding this for the body of each test keeps them
    /// independent without changing what they assert.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The first resolve builds the inventory exactly once.
    ///
    /// Pins a real defect: an earlier version built in `new()`, so the first
    /// `resolve` saw a signature taken before the caller's own capture, found
    /// it different, and rebuilt immediately -- paying for the inventory twice
    /// on the first tick in a session that only ever got one.
    #[test]
    fn builds_the_inventory_once_on_the_first_resolve() {
        let _serial = serial();
        let mut resolver = FrameResolver::new();
        assert_eq!(resolver.rebuild_count(), 0, "construction must do no work");
        assert_eq!(
            resolver.module_count(),
            0,
            "no inventory before the first resolve"
        );
        let mut snapshot = capture();
        resolver.resolve(&mut snapshot).expect("resolve");
        assert_eq!(
            resolver.rebuild_count(),
            1,
            "the first resolve must build once, not twice"
        );
        assert!(resolver.module_count() > 0, "inventory must not be empty");
    }

    /// Resolving must work on every capture, not only the first.
    #[test]
    fn resolves_frames_across_repeated_captures() {
        let _serial = serial();
        let mut resolver = FrameResolver::new();
        for round in 0..3 {
            let mut snapshot = capture();
            resolver.resolve(&mut snapshot).expect("resolve");
            assert!(
                snapshot.frames_resolved,
                "round {round} left frames unresolved"
            );
        }
    }

    #[test]
    fn changed_thread_outside_unwind_window_reports_current_ip() {
        let _serial = serial();
        let mut resolver = FrameResolver::new();
        let mut snapshot = Snapshot::default();
        for tid in 1..=17 {
            snapshot.threads.push(ThreadSample {
                os_tid: tid,
                stack_pointer: 0,
                instruction_pointer: 0x1000,
                frame_pointer: 0,
                link_register: None,
                stack_bytes: Vec::new(),
                truncated: false,
                kind: CaptureKind::RawContext,
                frames: Vec::new(),
            });
        }
        let mut stale = snapshot.threads[16].clone();
        stale.instruction_pointer = 0x2000;
        stale.frames = vec![0x2000, 0x3000];
        resolver
            .cached_threads
            .insert(stale.os_tid, CachedThread::from_sample(&stale));
        resolver
            .resolve_with_limit(&mut snapshot, Some(16))
            .expect("resolve");
        assert_eq!(snapshot.threads[16].frames, vec![0x1000]);
        assert!(!resolver.cached_threads[&17].fresh);
    }

    /// Once built, an unchanged image set is reused rather than rebuilt.
    #[test]
    fn reuses_the_inventory_when_no_module_changes() {
        let _serial = serial();
        let mut resolver = FrameResolver::new();
        let mut snapshot = capture();
        resolver.resolve(&mut snapshot).expect("resolve");
        let built = resolver.rebuild_count();
        let modules = resolver.module_count();
        assert!(built > 0);

        let mut snapshot = capture();
        resolver.resolve(&mut snapshot).expect("resolve");
        assert_eq!(
            resolver.rebuild_count(),
            built,
            "an unchanged image set must not rebuild"
        );
        assert_eq!(resolver.module_count(), modules);
    }

    /// A library to load for the invalidation tests: the first that opens and
    /// is not already part of the process.
    ///
    /// By soname, which the loader resolves through its cache. That is how a
    /// normal glibc host works and how CI resolves it; a host without a
    /// populated cache (NixOS, for one) resolves none of these, and the tests
    /// below say so and skip rather than pretend to have run. Hard-coding an
    /// absolute path would only move the problem to every other host.
    #[cfg(target_os = "linux")]
    fn load_a_new_module() -> Option<*mut libc::c_void> {
        const CANDIDATES: [&std::ffi::CStr; 8] = [
            c"libbz2.so.1.0",
            c"libexpat.so.1",
            c"liblzma.so.5",
            c"libncurses.so.6",
            c"libpcre2-8.so.0",
            c"libffi.so.8",
            c"libxml2.so.2",
            c"libreadline.so.8",
        ];
        let before = super::super::modules::loaded_image_signature().ok()?;
        for candidate in CANDIDATES {
            let handle = unsafe { libc::dlopen(candidate.as_ptr(), libc::RTLD_NOW) };
            if handle.is_null() {
                continue;
            }
            // `dlopen` on a library the process already holds returns a handle
            // but changes nothing, so a candidate is only usable once the
            // digest has actually moved. Checking here keeps the tests from
            // asserting against a no-op.
            if super::super::modules::loaded_image_signature().ok()? != before {
                return Some(handle);
            }
            unsafe { libc::dlclose(handle) };
        }
        None
    }

    /// Skip with a visible note. A silent pass would read as coverage the run
    /// did not have.
    #[cfg(target_os = "linux")]
    macro_rules! require_a_loadable_module {
        () => {
            match load_a_new_module() {
                Some(handle) => handle,
                None => {
                    println!(
                        "SKIP: no candidate library resolvable by soname on this host, so \
                         mid-session module loading was not exercised"
                    );
                    return;
                }
            }
        };
    }

    /// The digest must move when the mapped images move, or the cache would
    /// serve a stale inventory forever.
    #[cfg(target_os = "linux")]
    #[test]
    fn signature_tracks_loaded_images() {
        let _serial = serial();
        let handle = require_a_loadable_module!();
        let during = super::super::modules::loaded_image_signature().expect("signature");
        unsafe { libc::dlclose(handle) };
        let after = super::super::modules::loaded_image_signature().expect("signature");
        assert_ne!(
            during, after,
            "unloading a module must change the digest as loading one does"
        );
    }

    /// The cache must respond to a changed image set -- that is what makes
    /// reuse safe rather than merely fast.
    ///
    /// Tolerant about the exact count on purpose: tests in this binary run in
    /// parallel threads, and any of them that loads a library moves the
    /// process-wide mapping table, which is precisely the signal being
    /// watched. The digest halves are asserted exactly, because loading and
    /// unloading a module the process did not already hold must move it; the
    /// rebuild half asserts that a change produces at least one rebuild.
    #[cfg(target_os = "linux")]
    #[test]
    fn rebuilds_when_the_image_set_changes() {
        let _serial = serial();
        let mut resolver = FrameResolver::new();
        let mut snapshot = capture();
        resolver.resolve(&mut snapshot).expect("resolve");
        let before = resolver.rebuild_count();

        let handle = require_a_loadable_module!();
        let mut snapshot = capture();
        resolver.resolve(&mut snapshot).expect("resolve");
        let after = resolver.rebuild_count();
        unsafe { libc::dlclose(handle) };

        assert!(
            after > before,
            "a module loaded mid-session must cause a rebuild: {before} -> {after}"
        );
    }
}
