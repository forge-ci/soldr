//! Stable protobuf contract shared with the isolated symbolizer worker.
//!
//! Domain types are owned by `kernal-api`; Prost types remain private. Every
//! communication field has a fixed protobuf tag in the private `proto`
//! module, and enum discriminants are explicit so releases can add fields and
//! variants without renumbering the wire.

use prost::Message as _;

/// What kind of capture the payload carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum CaptureFormat {
    /// Frames the client already unwound; the worker only resolves names.
    #[default]
    CooperativeFrames = 0,
    /// A crash minidump the worker must parse itself.
    Minidump = 1,
}

/// One module the capture refers to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleRef {
    /// Display name, e.g. `_native.pyd`.
    pub name: String,
    /// Load address retained only for provenance; resolution uses offsets.
    pub base_avma: u64,
    /// Compiler/linker identity of the binary, if known.
    pub code_id: Option<String>,
    /// Identity of the matching symbol file.
    pub debug_id: Option<String>,
    /// Native symbol filename captured from loaded-module metadata.
    pub debug_file: Option<String>,
    /// Original image path, when known.
    pub path_hint: Option<String>,
    /// Worker-selected embedded symbol payload.
    pub embedded_symbol_path: Option<String>,
}

/// Capture-level symbol-discovery declarations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoveryConfig {
    /// Optional process-level symbol manifest.
    pub registered_manifest: Option<String>,
    /// Explicit symbol files or directories.
    pub registered_symbol_paths: Vec<String>,
}

/// One native frame: a module and an offset into it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawFrame {
    /// Index into [`RawCapture::modules`].
    pub module_index: u32,
    /// Offset of the return address from the module base.
    pub relative_address: u64,
}

/// One interpreter frame, already resolved by the client.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PyFrame {
    /// Source file.
    pub file: String,
    /// Source line.
    pub line: u32,
    /// Function name.
    pub func: String,
}

/// One captured thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawThread {
    /// OS thread id.
    pub os_tid: u64,
    /// Thread name, when known.
    pub name: Option<String>,
    /// Native frames, innermost first.
    pub frames: Vec<RawFrame>,
    /// Interpreter frames, passed through untouched.
    pub py_frames: Vec<PyFrame>,
}

/// A complete capture handed to the worker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawCapture {
    /// Capture representation.
    pub format: CaptureFormat,
    /// Symbol-discovery declarations.
    pub discovery: DiscoveryConfig,
    /// Modules referenced by frames.
    pub modules: Vec<ModuleRef>,
    /// Captured threads.
    pub threads: Vec<RawThread>,
}

impl RawCapture {
    /// Encode this capture using the stable protobuf contract.
    pub fn encode_wire(&self) -> Vec<u8> {
        proto::RawCapture::from(self).encode_to_vec()
    }

    /// Decode a capture from the stable protobuf contract.
    pub fn decode_wire(bytes: &[u8]) -> Result<Self, WireError> {
        proto::RawCapture::decode(bytes)
            .map(Self::from)
            .map_err(WireError::decode)
    }
}

/// How well a frame could be resolved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum FrameStatus {
    /// The module is known; no symbols were available.
    #[default]
    RawOnly = 0,
    /// A function name was found.
    Resolved = 1,
    /// The address could not be attributed to a module.
    ModuleUnknown = 2,
}

/// An inlined call site expanded out of one physical frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineFrame {
    /// Inlined function name.
    pub function: String,
    /// Source file, when present.
    pub file: Option<String>,
    /// Source line, when present.
    pub line: Option<u32>,
}

/// One symbolized native frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SymFrame {
    /// Owning module name.
    pub module: String,
    /// Offset within the module, always retained.
    pub relative_address: u64,
    /// Resolved function name.
    pub function: Option<String>,
    /// Resolved source file.
    pub file: Option<String>,
    /// Resolved source line.
    pub line: Option<u32>,
    /// Expanded inline call sites.
    pub inline_frames: Vec<InlineFrame>,
    /// Resolution status.
    pub status: FrameStatus,
}

/// One thread's symbolized frames.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SymThread {
    /// OS thread id.
    pub os_tid: u64,
    /// Thread name.
    pub name: Option<String>,
    /// Symbolized native frames.
    pub frames: Vec<SymFrame>,
    /// Interpreter frames passed through unchanged.
    pub py_frames: Vec<PyFrame>,
}

/// Why a module's symbols were or were not usable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum ModuleSymbolStatus {
    /// No candidate existed.
    #[default]
    NotFound = 0,
    /// Symbols were found and identity-verified.
    Resolved = 1,
    /// Candidates existed but described another build.
    Mismatched = 2,
    /// The image has no debug directory.
    NoDebugDirectory = 3,
    /// This platform has no applicable reader.
    Unsupported = 4,
}

/// Deterministic discovery tier that supplied a verified symbol file.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
#[repr(i32)]
pub enum DiscoverySource {
    /// Symbols embedded in the module.
    Embedded = 0,
    /// Manifest beside the module.
    AdjacentManifest = 1,
    /// Registration-declared manifest or path.
    Registration = 2,
    /// Conventional native symbol file beside the module.
    AdjacentNative = 3,
    /// Build-id keyed local cache.
    BuildIdCache = 4,
    /// Configured local symbol store.
    ConfiguredStore = 5,
    /// Administrator-configured HTTP(S) symbol server.
    ConfiguredServer = 6,
}

/// What became of one module's symbols.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleReport {
    /// Module name.
    pub name: String,
    /// Resolution outcome.
    pub status: ModuleSymbolStatus,
    /// Verified symbol file used.
    pub symbol_file: Option<String>,
    /// Discovery tier that supplied the file.
    pub symbol_source: Option<DiscoverySource>,
    /// Existing candidates rejected for identity mismatch.
    pub rejected_candidates: usize,
}

/// Complete worker response.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SymbolReport {
    /// Symbolized threads, in capture order.
    pub threads: Vec<SymThread>,
    /// One status entry per referenced module.
    pub modules: Vec<ModuleReport>,
}

impl SymbolReport {
    /// Encode this report using the stable protobuf contract.
    pub fn encode_wire(&self) -> Vec<u8> {
        proto::SymbolReport::from(self).encode_to_vec()
    }

    /// Decode a report from the stable protobuf contract.
    pub fn decode_wire(bytes: &[u8]) -> Result<Self, WireError> {
        proto::SymbolReport::decode(bytes)
            .map(Self::from)
            .map_err(WireError::decode)
    }
}

/// Invalid protobuf on the symbolization communication boundary.
#[derive(Clone, Debug, thiserror::Error)]
#[error("invalid kernal-api symbolization protobuf: {detail}")]
pub struct WireError {
    detail: String,
}

impl WireError {
    fn decode(error: prost::DecodeError) -> Self {
        Self {
            detail: error.to_string(),
        }
    }
}

mod proto {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ModuleRef {
        #[prost(string, tag = "1")]
        pub name: String,
        #[prost(uint64, tag = "2")]
        pub base_avma: u64,
        #[prost(string, optional, tag = "3")]
        pub code_id: Option<String>,
        #[prost(string, optional, tag = "4")]
        pub debug_id: Option<String>,
        #[prost(string, optional, tag = "5")]
        pub debug_file: Option<String>,
        #[prost(string, optional, tag = "6")]
        pub path_hint: Option<String>,
        #[prost(string, optional, tag = "7")]
        pub embedded_symbol_path: Option<String>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct DiscoveryConfig {
        #[prost(string, optional, tag = "1")]
        pub registered_manifest: Option<String>,
        #[prost(string, repeated, tag = "2")]
        pub registered_symbol_paths: Vec<String>,
    }

    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct RawFrame {
        #[prost(uint32, tag = "1")]
        pub module_index: u32,
        #[prost(uint64, tag = "2")]
        pub relative_address: u64,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct PyFrame {
        #[prost(string, tag = "1")]
        pub file: String,
        #[prost(uint32, tag = "2")]
        pub line: u32,
        #[prost(string, tag = "3")]
        pub func: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawThread {
        #[prost(uint64, tag = "1")]
        pub os_tid: u64,
        #[prost(string, optional, tag = "2")]
        pub name: Option<String>,
        #[prost(message, repeated, tag = "3")]
        pub frames: Vec<RawFrame>,
        #[prost(message, repeated, tag = "4")]
        pub py_frames: Vec<PyFrame>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawCapture {
        #[prost(int32, tag = "1")]
        pub format: i32,
        #[prost(message, optional, tag = "2")]
        pub discovery: Option<DiscoveryConfig>,
        #[prost(message, repeated, tag = "3")]
        pub modules: Vec<ModuleRef>,
        #[prost(message, repeated, tag = "4")]
        pub threads: Vec<RawThread>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct InlineFrame {
        #[prost(string, tag = "1")]
        pub function: String,
        #[prost(string, optional, tag = "2")]
        pub file: Option<String>,
        #[prost(uint32, optional, tag = "3")]
        pub line: Option<u32>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct SymFrame {
        #[prost(string, tag = "1")]
        pub module: String,
        #[prost(uint64, tag = "2")]
        pub relative_address: u64,
        #[prost(string, optional, tag = "3")]
        pub function: Option<String>,
        #[prost(string, optional, tag = "4")]
        pub file: Option<String>,
        #[prost(uint32, optional, tag = "5")]
        pub line: Option<u32>,
        #[prost(message, repeated, tag = "6")]
        pub inline_frames: Vec<InlineFrame>,
        #[prost(int32, tag = "7")]
        pub status: i32,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct SymThread {
        #[prost(uint64, tag = "1")]
        pub os_tid: u64,
        #[prost(string, optional, tag = "2")]
        pub name: Option<String>,
        #[prost(message, repeated, tag = "3")]
        pub frames: Vec<SymFrame>,
        #[prost(message, repeated, tag = "4")]
        pub py_frames: Vec<PyFrame>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ModuleReport {
        #[prost(string, tag = "1")]
        pub name: String,
        #[prost(int32, tag = "2")]
        pub status: i32,
        #[prost(string, optional, tag = "3")]
        pub symbol_file: Option<String>,
        #[prost(int32, optional, tag = "4")]
        pub symbol_source: Option<i32>,
        #[prost(uint64, tag = "5")]
        pub rejected_candidates: u64,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct SymbolReport {
        #[prost(message, repeated, tag = "1")]
        pub threads: Vec<SymThread>,
        #[prost(message, repeated, tag = "2")]
        pub modules: Vec<ModuleReport>,
    }
}

macro_rules! vec_into {
    ($value:expr) => {
        $value.into_iter().map(Into::into).collect()
    };
}

impl From<&RawCapture> for proto::RawCapture {
    fn from(value: &RawCapture) -> Self {
        Self {
            format: value.format as i32,
            discovery: Some((&value.discovery).into()),
            modules: value.modules.iter().map(Into::into).collect(),
            threads: value.threads.iter().map(Into::into).collect(),
        }
    }
}

impl From<proto::RawCapture> for RawCapture {
    fn from(value: proto::RawCapture) -> Self {
        Self {
            format: capture_format(value.format),
            discovery: value.discovery.map(Into::into).unwrap_or_default(),
            modules: vec_into!(value.modules),
            threads: vec_into!(value.threads),
        }
    }
}

impl From<&ModuleRef> for proto::ModuleRef {
    fn from(value: &ModuleRef) -> Self {
        Self {
            name: value.name.clone(),
            base_avma: value.base_avma,
            code_id: value.code_id.clone(),
            debug_id: value.debug_id.clone(),
            debug_file: value.debug_file.clone(),
            path_hint: value.path_hint.clone(),
            embedded_symbol_path: value.embedded_symbol_path.clone(),
        }
    }
}

impl From<proto::ModuleRef> for ModuleRef {
    fn from(value: proto::ModuleRef) -> Self {
        Self {
            name: value.name,
            base_avma: value.base_avma,
            code_id: value.code_id,
            debug_id: value.debug_id,
            debug_file: value.debug_file,
            path_hint: value.path_hint,
            embedded_symbol_path: value.embedded_symbol_path,
        }
    }
}

impl From<&DiscoveryConfig> for proto::DiscoveryConfig {
    fn from(value: &DiscoveryConfig) -> Self {
        Self {
            registered_manifest: value.registered_manifest.clone(),
            registered_symbol_paths: value.registered_symbol_paths.clone(),
        }
    }
}

impl From<proto::DiscoveryConfig> for DiscoveryConfig {
    fn from(value: proto::DiscoveryConfig) -> Self {
        Self {
            registered_manifest: value.registered_manifest,
            registered_symbol_paths: value.registered_symbol_paths,
        }
    }
}

impl From<&RawFrame> for proto::RawFrame {
    fn from(value: &RawFrame) -> Self {
        Self {
            module_index: value.module_index,
            relative_address: value.relative_address,
        }
    }
}

impl From<proto::RawFrame> for RawFrame {
    fn from(value: proto::RawFrame) -> Self {
        Self {
            module_index: value.module_index,
            relative_address: value.relative_address,
        }
    }
}

impl From<&PyFrame> for proto::PyFrame {
    fn from(value: &PyFrame) -> Self {
        Self {
            file: value.file.clone(),
            line: value.line,
            func: value.func.clone(),
        }
    }
}

impl From<proto::PyFrame> for PyFrame {
    fn from(value: proto::PyFrame) -> Self {
        Self {
            file: value.file,
            line: value.line,
            func: value.func,
        }
    }
}

impl From<&RawThread> for proto::RawThread {
    fn from(value: &RawThread) -> Self {
        Self {
            os_tid: value.os_tid,
            name: value.name.clone(),
            frames: value.frames.iter().map(Into::into).collect(),
            py_frames: value.py_frames.iter().map(Into::into).collect(),
        }
    }
}

impl From<proto::RawThread> for RawThread {
    fn from(value: proto::RawThread) -> Self {
        Self {
            os_tid: value.os_tid,
            name: value.name,
            frames: vec_into!(value.frames),
            py_frames: vec_into!(value.py_frames),
        }
    }
}

impl From<&SymbolReport> for proto::SymbolReport {
    fn from(value: &SymbolReport) -> Self {
        Self {
            threads: value.threads.iter().map(Into::into).collect(),
            modules: value.modules.iter().map(Into::into).collect(),
        }
    }
}

impl From<proto::SymbolReport> for SymbolReport {
    fn from(value: proto::SymbolReport) -> Self {
        Self {
            threads: vec_into!(value.threads),
            modules: vec_into!(value.modules),
        }
    }
}

impl From<&InlineFrame> for proto::InlineFrame {
    fn from(value: &InlineFrame) -> Self {
        Self {
            function: value.function.clone(),
            file: value.file.clone(),
            line: value.line,
        }
    }
}

impl From<proto::InlineFrame> for InlineFrame {
    fn from(value: proto::InlineFrame) -> Self {
        Self {
            function: value.function,
            file: value.file,
            line: value.line,
        }
    }
}

impl From<&SymFrame> for proto::SymFrame {
    fn from(value: &SymFrame) -> Self {
        Self {
            module: value.module.clone(),
            relative_address: value.relative_address,
            function: value.function.clone(),
            file: value.file.clone(),
            line: value.line,
            inline_frames: value.inline_frames.iter().map(Into::into).collect(),
            status: value.status as i32,
        }
    }
}

impl From<proto::SymFrame> for SymFrame {
    fn from(value: proto::SymFrame) -> Self {
        Self {
            module: value.module,
            relative_address: value.relative_address,
            function: value.function,
            file: value.file,
            line: value.line,
            inline_frames: vec_into!(value.inline_frames),
            status: frame_status(value.status),
        }
    }
}

impl From<&SymThread> for proto::SymThread {
    fn from(value: &SymThread) -> Self {
        Self {
            os_tid: value.os_tid,
            name: value.name.clone(),
            frames: value.frames.iter().map(Into::into).collect(),
            py_frames: value.py_frames.iter().map(Into::into).collect(),
        }
    }
}

impl From<proto::SymThread> for SymThread {
    fn from(value: proto::SymThread) -> Self {
        Self {
            os_tid: value.os_tid,
            name: value.name,
            frames: vec_into!(value.frames),
            py_frames: vec_into!(value.py_frames),
        }
    }
}

impl From<&ModuleReport> for proto::ModuleReport {
    fn from(value: &ModuleReport) -> Self {
        Self {
            name: value.name.clone(),
            status: value.status as i32,
            symbol_file: value.symbol_file.clone(),
            symbol_source: value.symbol_source.map(|source| source as i32),
            rejected_candidates: value.rejected_candidates as u64,
        }
    }
}

impl From<proto::ModuleReport> for ModuleReport {
    fn from(value: proto::ModuleReport) -> Self {
        Self {
            name: value.name,
            status: module_status(value.status),
            symbol_file: value.symbol_file,
            symbol_source: value.symbol_source.and_then(discovery_source),
            rejected_candidates: usize::try_from(value.rejected_candidates).unwrap_or(usize::MAX),
        }
    }
}

fn capture_format(value: i32) -> CaptureFormat {
    match value {
        1 => CaptureFormat::Minidump,
        _ => CaptureFormat::CooperativeFrames,
    }
}

fn frame_status(value: i32) -> FrameStatus {
    match value {
        1 => FrameStatus::Resolved,
        2 => FrameStatus::ModuleUnknown,
        _ => FrameStatus::RawOnly,
    }
}

fn module_status(value: i32) -> ModuleSymbolStatus {
    match value {
        1 => ModuleSymbolStatus::Resolved,
        2 => ModuleSymbolStatus::Mismatched,
        3 => ModuleSymbolStatus::NoDebugDirectory,
        4 => ModuleSymbolStatus::Unsupported,
        _ => ModuleSymbolStatus::NotFound,
    }
}

fn discovery_source(value: i32) -> Option<DiscoverySource> {
    Some(match value {
        0 => DiscoverySource::Embedded,
        1 => DiscoverySource::AdjacentManifest,
        2 => DiscoverySource::Registration,
        3 => DiscoverySource::AdjacentNative,
        4 => DiscoverySource::BuildIdCache,
        5 => DiscoverySource::ConfiguredStore,
        6 => DiscoverySource::ConfiguredServer,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_capture_decodes_with_defaults() {
        let capture = RawCapture::decode_wire(&[]).unwrap();
        assert_eq!(capture.format, CaptureFormat::CooperativeFrames);
        assert!(capture.threads.is_empty());
    }

    #[test]
    fn capture_round_trips_through_protobuf() {
        let capture = RawCapture {
            modules: vec![ModuleRef {
                name: "_native.pyd".into(),
                base_avma: 0x7fff_0000,
                debug_id: Some("ABCD".into()),
                ..Default::default()
            }],
            threads: vec![RawThread {
                os_tid: 42,
                name: Some("worker".into()),
                frames: vec![RawFrame {
                    module_index: 0,
                    relative_address: 0x1234,
                }],
                py_frames: vec![PyFrame {
                    file: "t.py".into(),
                    line: 9,
                    func: "main".into(),
                }],
            }],
            ..Default::default()
        };
        assert_eq!(
            RawCapture::decode_wire(&capture.encode_wire()).unwrap(),
            capture
        );
    }

    #[test]
    fn unknown_enum_values_degrade_without_fabricating_status() {
        let proto = proto::SymbolReport {
            threads: vec![proto::SymThread {
                frames: vec![proto::SymFrame {
                    status: 999,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            modules: vec![proto::ModuleReport {
                status: 999,
                symbol_source: Some(999),
                ..Default::default()
            }],
        };
        let report = SymbolReport::decode_wire(&proto.encode_to_vec()).unwrap();
        assert_eq!(report.threads[0].frames[0].status, FrameStatus::RawOnly);
        assert_eq!(report.modules[0].status, ModuleSymbolStatus::NotFound);
        assert_eq!(report.modules[0].symbol_source, None);
    }
}
