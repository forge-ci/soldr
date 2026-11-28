//! Turning instruction pointers into names, off the hot path (S15 / #644).
//!
//! Runs after sampling, never between samples. Parsing PDB/DWARF/Mach-O costs
//! orders of magnitude more than capturing a stack, so doing it inline would
//! make the profiler the dominant entry in its own profile.
//!
//! # It works after the target has exited
//!
//! A raw sample is `(module, relative address)` once the module list is
//! attached. Neither depends on the process still existing, so a profile of a
//! program that crashed mid-session still symbolizes — which is exactly when
//! it is most wanted. This is why the sampler records the module list once, at
//! session start, instead of resolving names as it goes.

use std::collections::HashMap;

use crate::snapshot::modules::LoadedModule;

/// A frame with whatever identity could be established for it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Frame {
    /// Function name, or a synthesized `module+0xoffset` when unresolved.
    pub function: String,
    /// Owning module file name, empty when the address matched none.
    pub module: String,
    /// Address relative to the module base — ASLR-independent, so it means the
    /// same thing on the machine that symbolizes as on the one that sampled.
    pub relative_address: u64,
}

/// Resolves addresses to frames.
///
/// A trait so tests can supply known names, and so a richer resolver (the S8
/// off-process worker, with real debug info) can be substituted without the
/// export code knowing.
pub trait FrameResolver {
    /// Resolve one absolute instruction pointer.
    fn resolve(&mut self, address: u64) -> Frame;
}

/// The default resolver: module attribution, no debug info.
///
/// Produces `module!+0x1234` names. That is deliberately honest about what it
/// knows — a profile whose frames say `module+0xoffset` is obviously
/// unsymbolized, whereas one that invented plausible names would be worse than
/// useless. Real names come from the S8 worker when symbol files are
/// available; this is the floor, not the ceiling.
#[derive(Debug)]
pub struct ModuleResolver {
    modules: Vec<LoadedModule>,
    cache: HashMap<u64, Frame>,
}

impl ModuleResolver {
    /// Build a resolver over a module list captured at session start.
    pub fn new(modules: Vec<LoadedModule>) -> Self {
        Self {
            modules,
            cache: HashMap::new(),
        }
    }

    /// Build a resolver over the current process's modules.
    pub fn for_current_process() -> std::io::Result<Self> {
        Ok(Self::new(crate::snapshot::modules::enumerate_modules()?))
    }

    /// How many modules were captured.
    pub fn module_count(&self) -> usize {
        self.modules.len()
    }
}

impl FrameResolver for ModuleResolver {
    fn resolve(&mut self, address: u64) -> Frame {
        // Cached because a hot loop revisits the same handful of addresses
        // thousands of times, and the module search is linear.
        if let Some(frame) = self.cache.get(&address) {
            return frame.clone();
        }

        let frame = match crate::snapshot::modules::module_for_address(&self.modules, address) {
            Some(module) => {
                // `path` is `None` when the OS refused to report it, which is
                // a real outcome and not worth guessing at — an invented path
                // would send the symbolizer to a different build's symbols and
                // produce confidently wrong names.
                let name = module
                    .path
                    .as_deref()
                    .map(|path| {
                        std::path::Path::new(path)
                            .file_name()
                            .map(|leaf| leaf.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.to_string())
                    })
                    .unwrap_or_else(|| format!("<module@0x{:x}>", module.base));
                let relative = address.saturating_sub(module.base);
                Frame {
                    function: format!("{name}+0x{relative:x}"),
                    module: name,
                    relative_address: relative,
                }
            }
            // An address in no known module is real and worth keeping: JIT
            // output and dynamically generated thunks live there. Dropping the
            // frame would silently reparent its callees onto the wrong caller.
            None => Frame {
                function: format!("0x{address:x}"),
                module: String::new(),
                relative_address: address,
            },
        };
        self.cache.insert(address, frame.clone());
        frame
    }
}

/// A resolver backed by a fixed table, for tests and for replaying a capture
/// whose names were resolved elsewhere.
#[derive(Debug, Default)]
pub struct TableResolver {
    names: HashMap<u64, String>,
}

impl TableResolver {
    /// Map `address` to `name`.
    pub fn with(mut self, address: u64, name: &str) -> Self {
        self.names.insert(address, name.to_string());
        self
    }
}

impl FrameResolver for TableResolver {
    fn resolve(&mut self, address: u64) -> Frame {
        Frame {
            function: self
                .names
                .get(&address)
                .cloned()
                .unwrap_or_else(|| format!("0x{address:x}")),
            module: String::new(),
            relative_address: address,
        }
    }
}
