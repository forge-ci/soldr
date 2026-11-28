//! Attributing captured addresses to their loaded module (#725).
//!
//! # Why absolute addresses cannot leave the process
//!
//! A capture yields absolute return addresses, which are only meaningful
//! inside the process that produced them and only until it exits: the same
//! build loads at a different base next time. Symbolization therefore consumes
//! `(module, offset)` — stable against ASLR, and re-resolvable against the
//! same binary long afterwards.
//!
//! This is the conversion, and it has to happen here, in the capturing
//! process, because that is the only place the module bases exist.
//!
//! # Getting this wrong is worse than not doing it
//!
//! An address attributed to the wrong module produces an offset that is
//! meaningless in that module — and a later, entirely correct symbol lookup
//! will turn it into a confident, wrong function name. Nothing downstream can
//! detect that. So an address that falls in no known module is reported as
//! unattributed rather than being assigned to the nearest one.

use super::modules::LoadedModule;
use super::Snapshot;

/// A module referenced by an attributed capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributedModule {
    /// File name, e.g. `_native.pyd`.
    pub name: String,
    /// Full path on disk, when known — the symbol file is found beside it.
    pub path: Option<String>,
    /// Exact symbol identity captured from the loaded module.
    pub debug_id: Option<String>,
    /// Sanitized native symbol filename captured from the loaded module.
    pub debug_file: Option<String>,
    /// Base the module was loaded at. Recorded for provenance only; the
    /// offsets below are already relative, so nothing downstream needs it.
    pub base: u64,
}

/// One frame, expressed relative to a module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttributedFrame {
    /// Index into [`AttributedCapture::modules`], or `None` when the address
    /// fell outside every loaded module.
    pub module_index: Option<u32>,
    /// Offset from the module's base, or the raw address when unattributed.
    ///
    /// Never empty: an offset without a module is still evidence, and
    /// discarding it would lose the only trace of that frame.
    pub relative_address: u64,
}

/// One thread's attributed frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributedThread {
    /// OS thread id, carried through so the mixed-mode pairing survives.
    pub os_tid: u64,
    /// Frames, innermost first.
    pub frames: Vec<AttributedFrame>,
}

/// A capture with every address expressed as module + offset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttributedCapture {
    /// Modules actually referenced, in first-reference order.
    ///
    /// Only referenced modules are listed. A process maps hundreds; carrying
    /// all of them would make the payload mostly noise.
    pub modules: Vec<AttributedModule>,
    /// Attributed threads, in capture order.
    pub threads: Vec<AttributedThread>,
}

impl AttributedCapture {
    /// How many frames could not be attributed to any module.
    ///
    /// Reported so a consumer can tell a sparse symbolization from a broken
    /// one: many unattributed frames mean the module inventory did not match
    /// the capture, not that symbols were missing.
    pub fn unattributed_frames(&self) -> usize {
        self.threads
            .iter()
            .flat_map(|t| &t.frames)
            .filter(|f| f.module_index.is_none())
            .count()
    }
}

/// Express every frame in `snapshot` relative to its module.
///
/// `modules` must come from the same process and the same moment as the
/// capture; bases from anywhere else describe a different address space.
pub fn attribute(snapshot: &Snapshot, modules: &[LoadedModule]) -> AttributedCapture {
    let mut out = AttributedCapture::default();
    // Maps a module's base to its index in `out.modules`, so a module
    // referenced by many frames is listed once.
    let mut index_by_base: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();

    for sample in &snapshot.threads {
        let mut frames = Vec::with_capacity(sample.frames.len());
        for &address in &sample.frames {
            match modules.iter().find(|m| m.contains(address)) {
                Some(module) => {
                    let next = u32::try_from(out.modules.len()).unwrap_or(u32::MAX);
                    let index = *index_by_base.entry(module.base).or_insert_with(|| {
                        out.modules.push(AttributedModule {
                            name: module_name(module),
                            path: module.path.clone(),
                            debug_id: module.debug_id.clone(),
                            debug_file: module.debug_file.clone(),
                            base: module.base,
                        });
                        next
                    });
                    frames.push(AttributedFrame {
                        module_index: Some(index),
                        relative_address: address - module.base,
                    });
                }
                // Outside every known module. Keep the address rather than
                // guessing an owner — a wrong attribution becomes a wrong
                // function name that nothing downstream can catch.
                None => frames.push(AttributedFrame {
                    module_index: None,
                    relative_address: address,
                }),
            }
        }
        out.threads.push(AttributedThread {
            os_tid: sample.os_tid,
            frames,
        });
    }
    out
}

fn module_name(module: &LoadedModule) -> String {
    module
        .path
        .as_deref()
        // Captures can be decoded or tested on a different host OS, so accept
        // both native separator styles instead of delegating to host `Path`.
        .and_then(|path| path.rsplit(['/', '\\']).find(|part| !part.is_empty()))
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{:#x}", module.base))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::modules::Section;
    use crate::snapshot::{CaptureKind, ThreadSample};

    fn module(base: u64, size: u64, path: Option<&str>) -> LoadedModule {
        LoadedModule {
            base,
            size,
            mapped_ranges: Vec::new(),
            executable_ranges: Vec::new(),
            path: path.map(str::to_owned),
            debug_id: None,
            debug_file: None,
            sections: Vec::<Section>::new(),
        }
    }

    fn snapshot_with(threads: Vec<(u64, Vec<u64>)>) -> Snapshot {
        Snapshot {
            threads: threads
                .into_iter()
                .map(|(os_tid, frames)| ThreadSample {
                    os_tid,
                    stack_pointer: 0,
                    instruction_pointer: 0,
                    frame_pointer: 0,
                    link_register: None,
                    stack_bytes: Vec::new(),
                    truncated: false,
                    kind: CaptureKind::RawContext,
                    frames,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn an_address_becomes_an_offset_from_its_module() {
        let modules = vec![module(0x1000, 0x1000, Some(r"C:\app\a.dll"))];
        let capture = attribute(&snapshot_with(vec![(7, vec![0x1234])]), &modules);

        assert_eq!(capture.modules.len(), 1);
        assert_eq!(capture.modules[0].name, "a.dll");
        assert_eq!(capture.threads[0].frames[0].module_index, Some(0));
        assert_eq!(capture.threads[0].frames[0].relative_address, 0x234);
    }

    /// The failure that matters: an address outside every module must not be
    /// assigned to one.
    #[test]
    fn an_address_outside_every_module_is_left_unattributed() {
        let modules = vec![module(0x1000, 0x1000, Some("a.dll"))];
        let capture = attribute(&snapshot_with(vec![(7, vec![0x9999])]), &modules);

        assert!(capture.modules.is_empty(), "no module was referenced");
        assert_eq!(capture.threads[0].frames[0].module_index, None);
        assert_eq!(
            capture.threads[0].frames[0].relative_address, 0x9999,
            "the raw address must survive so the frame is not lost"
        );
        assert_eq!(capture.unattributed_frames(), 1);
    }

    /// Picking the wrong module of several is the silent-wrong-name failure.
    #[test]
    fn each_address_lands_in_its_own_module() {
        let modules = vec![
            module(0x1000, 0x1000, Some("a.dll")),
            module(0x8000, 0x1000, Some("b.dll")),
        ];
        let capture = attribute(&snapshot_with(vec![(7, vec![0x8100, 0x1100])]), &modules);

        let by_name: Vec<_> = capture.threads[0]
            .frames
            .iter()
            .map(|f| {
                (
                    capture.modules[f.module_index.unwrap() as usize]
                        .name
                        .as_str(),
                    f.relative_address,
                )
            })
            .collect();
        assert_eq!(by_name, vec![("b.dll", 0x100), ("a.dll", 0x100)]);
    }

    #[test]
    fn a_module_referenced_twice_is_listed_once() {
        let modules = vec![module(0x1000, 0x1000, Some("a.dll"))];
        let capture = attribute(
            &snapshot_with(vec![(7, vec![0x1100, 0x1200, 0x1300])]),
            &modules,
        );

        assert_eq!(capture.modules.len(), 1, "one module, three frames");
        for frame in &capture.threads[0].frames {
            assert_eq!(frame.module_index, Some(0));
        }
    }

    /// Unreferenced modules must not be carried: a process maps hundreds.
    #[test]
    fn only_referenced_modules_are_listed() {
        let modules = vec![
            module(0x1000, 0x1000, Some("used.dll")),
            module(0x8000, 0x1000, Some("unused.dll")),
        ];
        let capture = attribute(&snapshot_with(vec![(7, vec![0x1100])]), &modules);

        assert_eq!(capture.modules.len(), 1);
        assert_eq!(capture.modules[0].name, "used.dll");
    }

    #[test]
    fn thread_identity_and_order_survive() {
        let modules = vec![module(0x1000, 0x1000, Some("a.dll"))];
        let capture = attribute(
            &snapshot_with(vec![(100, vec![0x1100]), (200, vec![0x1200])]),
            &modules,
        );
        assert_eq!(capture.threads[0].os_tid, 100);
        assert_eq!(capture.threads[1].os_tid, 200);
    }

    /// A module with no path still needs a name a report can print.
    #[test]
    fn a_pathless_module_is_named_by_its_base() {
        let modules = vec![module(0x4000, 0x1000, None)];
        let capture = attribute(&snapshot_with(vec![(7, vec![0x4010])]), &modules);
        assert_eq!(capture.modules[0].name, "0x4000");
        assert_eq!(capture.modules[0].path, None);
    }

    /// End to end against this process: real capture, real modules.
    #[cfg(windows)]
    #[test]
    fn a_real_capture_attributes_most_of_its_frames() {
        use crate::snapshot::modules::enumerate_modules;
        use crate::snapshot::{capture_and_resolve, SnapshotConfig};

        let snapshot = capture_and_resolve(&SnapshotConfig::default()).expect("capture");
        let modules = enumerate_modules().expect("modules");
        let capture = attribute(&snapshot, &modules);

        let total: usize = capture.threads.iter().map(|t| t.frames.len()).sum();
        if total == 0 {
            // No sibling threads were captured, so there is nothing to
            // attribute. Locally that is a legitimate outcome; under CI it
            // would mean this test asserts nothing while reporting green,
            // which is worse than a failure.
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "captured no frames during a CI run; this test would assert nothing"
            );
            return;
        }
        // Assert the INVARIANT, not a coverage ratio. What must hold is that
        // every attributed frame's offset lies inside the module it was
        // attributed to — that is what a wrong attribution would violate, and
        // it is what makes a later symbol lookup trustworthy.
        //
        // A ratio would be the wrong assertion: the proportion of frames
        // landing in a known module depends on how deep the walks went and
        // whether any ended in a truncated tail, which varies with what else
        // the test binary is doing. An earlier version of this test asserted
        // ">= 90% attributed", passed when run alone (4 frames), and failed in
        // the full suite (9 of 28 unattributed) — measuring the environment,
        // not the code.
        for thread in &capture.threads {
            for frame in &thread.frames {
                let Some(index) = frame.module_index else {
                    continue;
                };
                let module = &capture.modules[index as usize];
                let size = modules
                    .iter()
                    .find(|m| m.base == module.base)
                    .map(|m| m.size)
                    .expect("attributed module must come from the inventory");
                assert!(
                    frame.relative_address < size,
                    "offset {:#x} exceeds {}'s size {size:#x}; the frame was                      attributed to the wrong module",
                    frame.relative_address,
                    module.name
                );
            }
        }

        // And something must actually have been attributed, or the loop above
        // is vacuous.
        let attributed = total - capture.unattributed_frames();
        assert!(
            attributed > 0,
            "no frame of {total} matched any module; attribution is not working"
        );
        assert!(
            !capture.modules.is_empty(),
            "attributed frames but listed no modules"
        );
    }
}
