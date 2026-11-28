//! Private, bounded v1 framing for the one-request Wasm worker.
//!
//! This transports one module, an optional bounded acceptance trace, and a
//! terminal observation. It is not a generic resource streaming protocol.

use std::io::{Read, Write};

const MAGIC: [u8; 4] = *b"KWW1";
const VERSION: u16 = 7;
const HEADER_LEN: usize = 11;
pub(super) const MAX_FRAME_PAYLOAD: usize = 1024 * 1024;
/// One-request worker protocol ceiling.  This is intentionally distinct from
/// admission policy: only the process transport is bounded by this contract.
pub(super) const WORKER_PROTOCOL_MAX_MODULE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 1024;
pub(super) const MAX_TRACE_BYTES: usize = 64 * 1024;
const NO_STATUS_CODE: i32 = i32::MIN;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum Kind {
    Hello = 1,
    HelloAck = 2,
    ExecuteStart = 3,
    ModuleChunk = 4,
    ExecuteEnd = 5,
    Cancel = 6,
    Terminal = 7,
    ExecuteAck = 8,
    Trace = 9,
}

impl TryFrom<u8> for Kind {
    type Error = ProtocolError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::HelloAck),
            3 => Ok(Self::ExecuteStart),
            4 => Ok(Self::ModuleChunk),
            5 => Ok(Self::ExecuteEnd),
            6 => Ok(Self::Cancel),
            7 => Ok(Self::Terminal),
            8 => Ok(Self::ExecuteAck),
            9 => Ok(Self::Trace),
            _ => Err(ProtocolError::UnknownKind),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum TerminalKind {
    Completed = 1,
    Cancelled = 2,
    DeadlineExceeded = 3,
    OutOfFuel = 4,
    Trapped = 5,
    NonzeroExit = 6,
    ChildFailure = 7,
    ProtocolFailure = 8,
    WorkerFailure = 9,
    ForcedContainment = 10,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum RootOutcome {
    None = 0,
    Started = 1,
    Exited = 2,
    StartedWithThreadRejections = 3,
    ExitedWithThreadRejections = 4,
}
impl TryFrom<u8> for RootOutcome {
    type Error = ProtocolError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Started),
            2 => Ok(Self::Exited),
            3 => Ok(Self::StartedWithThreadRejections),
            4 => Ok(Self::ExitedWithThreadRejections),
            _ => Err(ProtocolError::InvalidTerminal),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TerminalDetail {
    pub(super) root_outcome: RootOutcome,
    pub(super) capacity_rejections: u32,
    pub(super) closing_rejections: u32,
    pub(super) fuel_rejections: u32,
    pub(super) epoch_rejections: u32,
    pub(super) status_code: Option<i32>,
}
impl TerminalDetail {
    pub(super) const fn none() -> Self {
        Self {
            root_outcome: RootOutcome::None,
            capacity_rejections: 0,
            closing_rejections: 0,
            fuel_rejections: 0,
            epoch_rejections: 0,
            status_code: None,
        }
    }
    fn validate(self, kind: TerminalKind) -> Result<(), ProtocolError> {
        let counts = self.capacity_rejections != 0
            || self.closing_rejections != 0
            || self.fuel_rejections != 0
            || self.epoch_rejections != 0;
        match kind {
            TerminalKind::Completed
                if self.root_outcome == RootOutcome::None || self.status_code.is_some() =>
            {
                Err(ProtocolError::InvalidTerminal)
            }
            TerminalKind::Completed
                if counts
                    && !matches!(
                        self.root_outcome,
                        RootOutcome::StartedWithThreadRejections
                            | RootOutcome::ExitedWithThreadRejections
                    ) =>
            {
                Err(ProtocolError::InvalidTerminal)
            }
            TerminalKind::Completed
                if !counts
                    && matches!(
                        self.root_outcome,
                        RootOutcome::StartedWithThreadRejections
                            | RootOutcome::ExitedWithThreadRejections
                    ) =>
            {
                Err(ProtocolError::InvalidTerminal)
            }
            TerminalKind::NonzeroExit
                if self.root_outcome != RootOutcome::None
                    || counts
                    || self.status_code.is_none() =>
            {
                Err(ProtocolError::InvalidTerminal)
            }
            TerminalKind::ChildFailure if self.root_outcome != RootOutcome::None || counts => {
                Err(ProtocolError::InvalidTerminal)
            }
            TerminalKind::Completed | TerminalKind::NonzeroExit => Ok(()),
            _ if self.root_outcome != RootOutcome::None || counts || self.status_code.is_some() => {
                Err(ProtocolError::InvalidTerminal)
            }
            _ => Ok(()),
        }
    }
}

impl TryFrom<u8> for TerminalKind {
    type Error = ProtocolError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Completed),
            2 => Ok(Self::Cancelled),
            3 => Ok(Self::DeadlineExceeded),
            4 => Ok(Self::OutOfFuel),
            5 => Ok(Self::Trapped),
            6 => Ok(Self::NonzeroExit),
            7 => Ok(Self::ChildFailure),
            8 => Ok(Self::ProtocolFailure),
            9 => Ok(Self::WorkerFailure),
            10 => Ok(Self::ForcedContainment),
            _ => Err(ProtocolError::InvalidTerminal),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ProtocolError {
    BadMagic,
    UnsupportedVersion,
    UnknownKind,
    Truncated,
    TrailingBytes,
    FrameTooLarge,
    InvalidRequestId,
    InvalidPayload,
    LengthOverflow,
    ModuleTooLarge,
    WrongRequestId,
    UnexpectedMessage,
    DuplicateTerminal,
    InvalidTerminal,
    Sequence,
    DiagnosticTooLarge,
}

/// Facade semantic primitives needed to reconstruct compiler/limit settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ExecuteMetadata {
    /// Host-owned URL authority, never guest bytes. Native workers must
    /// revalidate it before creating their per-root grant.
    pub(super) webview_url: Option<String>,
    /// Parent-owned staging destination, never the final output path or guest data.
    pub(super) staged_output: Option<std::path::PathBuf>,
    /// Chunk bytes, blob bytes, sketch bytes, live blobs, reads, writes, transfer bytes.
    pub(super) blob_limits: [u64; 7],
    /// Exact nonzero blob-transfer idle budget, split to preserve all valid
    /// `Duration` values without rounding a sub-millisecond host policy.
    pub(super) blob_progress_idle_timeout_secs: u64,
    pub(super) blob_progress_idle_timeout_nanos: u64,
    pub(super) max_wasm_stack_bytes: u64,
    pub(super) reserved_memory_bytes: u64,
    pub(super) maximum_active_roots: u64,
    pub(super) total_fuel: u64,
    pub(super) root_fuel: u64,
    pub(super) child_fuel: u64,
    pub(super) epoch_deadline_millis: u64,
    pub(super) epoch_tick_millis: u64,
    pub(super) maximum_epoch_registrations: u64,
    pub(super) max_module_bytes: u64,
    pub(super) max_shared_memory_pages: u32,
    pub(super) max_guest_threads: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FinalCounters {
    pub(super) active_roots: u64,
    pub(super) live_stores: u64,
    pub(super) live_instances: u64,
    pub(super) active_epoch_registrations: u64,
    pub(super) live_threads: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Message {
    Trace {
        request_id: u64,
        text: String,
    },
    Hello {
        request_id: u64,
    },
    HelloAck {
        request_id: u64,
    },
    ExecuteAck {
        request_id: u64,
    },
    ExecuteStart {
        request_id: u64,
        module_len: u64,
        metadata: ExecuteMetadata,
    },
    ModuleChunk {
        request_id: u64,
        sequence: u32,
        bytes: Vec<u8>,
    },
    ExecuteEnd {
        request_id: u64,
    },
    Cancel {
        request_id: u64,
    },
    Terminal {
        request_id: u64,
        kind: TerminalKind,
        detail: TerminalDetail,
        diagnostic: String,
        counters: FinalCounters,
    },
}

impl Message {
    pub(super) fn request_id(&self) -> u64 {
        match self {
            Self::Hello { request_id }
            | Self::HelloAck { request_id }
            | Self::ExecuteAck { request_id }
            | Self::ExecuteEnd { request_id }
            | Self::Cancel { request_id } => *request_id,
            Self::ExecuteStart { request_id, .. }
            | Self::Trace { request_id, .. }
            | Self::ModuleChunk { request_id, .. }
            | Self::Terminal { request_id, .. } => *request_id,
        }
    }
    fn kind(&self) -> Kind {
        match self {
            Self::Hello { .. } => Kind::Hello,
            Self::HelloAck { .. } => Kind::HelloAck,
            Self::ExecuteAck { .. } => Kind::ExecuteAck,
            Self::ExecuteStart { .. } => Kind::ExecuteStart,
            Self::ModuleChunk { .. } => Kind::ModuleChunk,
            Self::ExecuteEnd { .. } => Kind::ExecuteEnd,
            Self::Cancel { .. } => Kind::Cancel,
            Self::Terminal { .. } => Kind::Terminal,
            Self::Trace { .. } => Kind::Trace,
        }
    }
}

pub(super) fn encode(message: &Message) -> Result<Vec<u8>, ProtocolError> {
    if message.request_id() == 0 {
        return Err(ProtocolError::InvalidRequestId);
    }
    let mut payload = Vec::new();
    put_u64(&mut payload, message.request_id());
    match message {
        Message::Trace { text, .. } => {
            validate_trace(text.as_bytes())?;
            payload.extend_from_slice(text.as_bytes());
        }
        Message::Hello { .. }
        | Message::HelloAck { .. }
        | Message::ExecuteAck { .. }
        | Message::ExecuteEnd { .. }
        | Message::Cancel { .. } => {}
        Message::ExecuteStart {
            module_len,
            metadata,
            ..
        } => {
            put_u64(&mut payload, *module_len);
            put_metadata(&mut payload, metadata)?;
        }
        Message::ModuleChunk {
            sequence, bytes, ..
        } => {
            put_u32(&mut payload, *sequence);
            payload.extend_from_slice(bytes);
        }
        Message::Terminal {
            kind,
            detail,
            diagnostic,
            counters,
            ..
        } => {
            detail.validate(*kind)?;
            if diagnostic.len() > MAX_DIAGNOSTIC_BYTES {
                return Err(ProtocolError::DiagnosticTooLarge);
            }
            payload.push(*kind as u8);
            payload.push(detail.root_outcome as u8);
            put_u32(&mut payload, detail.capacity_rejections);
            put_u32(&mut payload, detail.closing_rejections);
            put_u32(&mut payload, detail.fuel_rejections);
            put_u32(&mut payload, detail.epoch_rejections);
            put_i32(&mut payload, detail.status_code.unwrap_or(NO_STATUS_CODE));
            put_u16(&mut payload, diagnostic.len() as u16);
            payload.extend_from_slice(diagnostic.as_bytes());
            put_counters(&mut payload, counters);
        }
    }
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(ProtocolError::FrameTooLarge);
    }
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&MAGIC);
    put_u16(&mut frame, VERSION);
    frame.push(message.kind() as u8);
    put_u32(
        &mut frame,
        u32::try_from(payload.len()).map_err(|_| ProtocolError::FrameTooLarge)?,
    );
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub(super) fn decode(frame: &[u8]) -> Result<Message, ProtocolError> {
    if frame.len() < HEADER_LEN {
        return Err(ProtocolError::Truncated);
    }
    if frame[..4] != MAGIC {
        return Err(ProtocolError::BadMagic);
    }
    if get_u16(&frame[4..6])? != VERSION {
        return Err(ProtocolError::UnsupportedVersion);
    }
    let kind = Kind::try_from(frame[6])?;
    let length =
        usize::try_from(get_u32(&frame[7..11])?).map_err(|_| ProtocolError::LengthOverflow)?;
    if length > MAX_FRAME_PAYLOAD {
        return Err(ProtocolError::FrameTooLarge);
    }
    let end = HEADER_LEN
        .checked_add(length)
        .ok_or(ProtocolError::LengthOverflow)?;
    if frame.len() < end {
        return Err(ProtocolError::Truncated);
    }
    if frame.len() != end {
        return Err(ProtocolError::TrailingBytes);
    }
    let mut input = &frame[HEADER_LEN..];
    let request_id = take_u64(&mut input)?;
    if request_id == 0 {
        return Err(ProtocolError::InvalidRequestId);
    }
    let message = match kind {
        Kind::Trace => {
            validate_trace(input)?;
            Message::Trace {
                request_id,
                text: String::from_utf8(std::mem::take(&mut input).to_vec())
                    .map_err(|_| ProtocolError::InvalidPayload)?,
            }
        }
        Kind::Hello => Message::Hello { request_id },
        Kind::HelloAck => Message::HelloAck { request_id },
        Kind::ExecuteAck => Message::ExecuteAck { request_id },
        Kind::ExecuteStart => {
            let module_len = take_u64(&mut input)?;
            let metadata = take_metadata(&mut input)?;
            if module_len > WORKER_PROTOCOL_MAX_MODULE_BYTES
                || module_len > metadata.max_module_bytes
            {
                return Err(ProtocolError::ModuleTooLarge);
            }
            Message::ExecuteStart {
                request_id,
                module_len,
                metadata,
            }
        }
        Kind::ModuleChunk => Message::ModuleChunk {
            request_id,
            sequence: take_u32(&mut input)?,
            // Move the bounded remainder out of the frame so the common
            // trailing-byte check below sees that this variant consumed it.
            bytes: std::mem::take(&mut input).to_vec(),
        },
        Kind::ExecuteEnd => Message::ExecuteEnd { request_id },
        Kind::Cancel => Message::Cancel { request_id },
        Kind::Terminal => {
            let terminal = TerminalKind::try_from(take_u8(&mut input)?)?;
            let detail = TerminalDetail {
                root_outcome: RootOutcome::try_from(take_u8(&mut input)?)?,
                capacity_rejections: take_u32(&mut input)?,
                closing_rejections: take_u32(&mut input)?,
                fuel_rejections: take_u32(&mut input)?,
                epoch_rejections: take_u32(&mut input)?,
                status_code: match take_i32(&mut input)? {
                    NO_STATUS_CODE => None,
                    code => Some(code),
                },
            };
            detail.validate(terminal)?;
            let text_len = usize::from(take_u16(&mut input)?);
            if text_len > MAX_DIAGNOSTIC_BYTES {
                return Err(ProtocolError::DiagnosticTooLarge);
            }
            let text = take(&mut input, text_len)?;
            let diagnostic = std::str::from_utf8(&text)
                .map_err(|_| ProtocolError::InvalidPayload)?
                .to_owned();
            Message::Terminal {
                request_id,
                kind: terminal,
                detail,
                diagnostic,
                counters: take_counters(&mut input)?,
            }
        }
    };
    if !input.is_empty() {
        return Err(ProtocolError::TrailingBytes);
    }
    Ok(message)
}

/// Reads a complete bounded frame. Header validation precedes allocation.
pub(super) fn read_message<R: Read>(reader: &mut R) -> Result<Message, ProtocolError> {
    let mut header = [0; HEADER_LEN];
    reader
        .read_exact(&mut header)
        .map_err(|_| ProtocolError::Truncated)?;
    if header[..4] != MAGIC {
        return Err(ProtocolError::BadMagic);
    }
    if get_u16(&header[4..6])? != VERSION {
        return Err(ProtocolError::UnsupportedVersion);
    }
    let kind = Kind::try_from(header[6])?;
    let length =
        usize::try_from(get_u32(&header[7..11])?).map_err(|_| ProtocolError::LengthOverflow)?;
    if length > MAX_FRAME_PAYLOAD {
        return Err(ProtocolError::FrameTooLarge);
    }
    if kind == Kind::Trace && length > MAX_TRACE_BYTES + 8 {
        return Err(ProtocolError::FrameTooLarge);
    }
    let total = HEADER_LEN
        .checked_add(length)
        .ok_or(ProtocolError::LengthOverflow)?;
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&header);
    frame.resize(total, 0);
    reader
        .read_exact(&mut frame[HEADER_LEN..])
        .map_err(|_| ProtocolError::Truncated)?;
    decode(&frame)
}

fn validate_trace(bytes: &[u8]) -> Result<(), ProtocolError> {
    if bytes.is_empty()
        || bytes.len() > MAX_TRACE_BYTES
        || bytes.iter().any(|byte| !matches!(byte, b'\n' | 32..=126))
    {
        return Err(ProtocolError::InvalidPayload);
    }
    Ok(())
}

pub(super) fn write_message<W: Write>(
    writer: &mut W,
    message: &Message,
) -> Result<(), ProtocolError> {
    let frame = encode(message)?;
    writer
        .write_all(&frame)
        .map_err(|_| ProtocolError::InvalidPayload)?;
    writer.flush().map_err(|_| ProtocolError::InvalidPayload)
}

/// Assembles only an ExecuteStart/chunks/ExecuteEnd sequence for one request.
#[derive(Debug)]
pub(super) struct ModuleAssembler {
    request_id: u64,
    declared: usize,
    caller_max: usize,
    next: u32,
    ended: bool,
    terminal: bool,
    module: Vec<u8>,
}
impl ModuleAssembler {
    pub(super) fn start(
        request_id: u64,
        module_len: u64,
        caller_max: u64,
    ) -> Result<Self, ProtocolError> {
        if request_id == 0 {
            return Err(ProtocolError::InvalidRequestId);
        }
        if module_len > WORKER_PROTOCOL_MAX_MODULE_BYTES || module_len > caller_max {
            return Err(ProtocolError::ModuleTooLarge);
        }
        let declared = usize::try_from(module_len).map_err(|_| ProtocolError::LengthOverflow)?;
        let caller_max = usize::try_from(caller_max).map_err(|_| ProtocolError::LengthOverflow)?;
        Ok(Self {
            request_id,
            declared,
            caller_max,
            next: 0,
            ended: false,
            terminal: false,
            module: Vec::new(),
        })
    }
    pub(super) fn accept(&mut self, message: Message) -> Result<Option<Vec<u8>>, ProtocolError> {
        if message.request_id() != self.request_id {
            return Err(ProtocolError::WrongRequestId);
        }
        match message {
            Message::ModuleChunk {
                sequence, bytes, ..
            } => {
                if self.ended
                    || sequence != self.next
                    || bytes.len() > MAX_FRAME_PAYLOAD.saturating_sub(12)
                {
                    return Err(ProtocolError::Sequence);
                }
                let total = self
                    .module
                    .len()
                    .checked_add(bytes.len())
                    .ok_or(ProtocolError::LengthOverflow)?;
                if total > self.declared || total > self.caller_max {
                    return Err(ProtocolError::ModuleTooLarge);
                }
                self.module.extend_from_slice(&bytes);
                self.next = self
                    .next
                    .checked_add(1)
                    .ok_or(ProtocolError::LengthOverflow)?;
                Ok(None)
            }
            Message::ExecuteEnd { .. } => {
                if self.ended || self.module.len() != self.declared {
                    return Err(ProtocolError::UnexpectedMessage);
                }
                self.ended = true;
                Ok(Some(std::mem::take(&mut self.module)))
            }
            Message::Terminal { .. } => {
                if !self.ended {
                    Err(ProtocolError::UnexpectedMessage)
                } else if self.terminal {
                    Err(ProtocolError::DuplicateTerminal)
                } else {
                    self.terminal = true;
                    Ok(None)
                }
            }
            _ => Err(ProtocolError::UnexpectedMessage),
        }
    }
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn get_u16(input: &[u8]) -> Result<u16, ProtocolError> {
    Ok(u16::from_le_bytes(
        input.try_into().map_err(|_| ProtocolError::Truncated)?,
    ))
}
fn get_u32(input: &[u8]) -> Result<u32, ProtocolError> {
    Ok(u32::from_le_bytes(
        input.try_into().map_err(|_| ProtocolError::Truncated)?,
    ))
}
fn take(input: &mut &[u8], count: usize) -> Result<Vec<u8>, ProtocolError> {
    if input.len() < count {
        return Err(ProtocolError::Truncated);
    }
    let value = input[..count].to_vec();
    *input = &input[count..];
    Ok(value)
}
fn take_u8(input: &mut &[u8]) -> Result<u8, ProtocolError> {
    Ok(take(input, 1)?[0])
}
fn take_u16(input: &mut &[u8]) -> Result<u16, ProtocolError> {
    get_u16(&take(input, 2)?)
}
fn take_u32(input: &mut &[u8]) -> Result<u32, ProtocolError> {
    get_u32(&take(input, 4)?)
}
fn take_u64(input: &mut &[u8]) -> Result<u64, ProtocolError> {
    Ok(u64::from_le_bytes(
        take(input, 8)?
            .try_into()
            .map_err(|_| ProtocolError::Truncated)?,
    ))
}
fn take_i32(input: &mut &[u8]) -> Result<i32, ProtocolError> {
    Ok(i32::from_le_bytes(
        take(input, 4)?
            .try_into()
            .map_err(|_| ProtocolError::Truncated)?,
    ))
}
const MAX_OUTPUT_PATH_BYTES: usize = 65_536;

fn put_output_path(out: &mut Vec<u8>, path: Option<&std::path::Path>) -> Result<(), ProtocolError> {
    let Some(path) = path else {
        put_u32(out, 0);
        return Ok(());
    };
    if !path.is_absolute() {
        return Err(ProtocolError::InvalidPayload);
    }
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        if bytes.contains(&0) || bytes.len() > MAX_OUTPUT_PATH_BYTES {
            return Err(ProtocolError::InvalidPayload);
        }
        bytes.to_vec()
    };
    #[cfg(windows)]
    let bytes = {
        use std::os::windows::ffi::OsStrExt;
        let mut bytes = Vec::new();
        for unit in path.as_os_str().encode_wide() {
            if unit == 0 || bytes.len() >= MAX_OUTPUT_PATH_BYTES {
                return Err(ProtocolError::InvalidPayload);
            }
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    };
    put_u32(out, bytes.len() as u32);
    out.push(if cfg!(windows) { 2 } else { 1 });
    out.extend_from_slice(&bytes);
    Ok(())
}

fn take_output_path(input: &mut &[u8]) -> Result<Option<std::path::PathBuf>, ProtocolError> {
    let length = take_u32(input)? as usize;
    if length == 0 {
        return Ok(None);
    }
    if length > MAX_OUTPUT_PATH_BYTES {
        return Err(ProtocolError::InvalidPayload);
    }
    if take_u8(input)? != if cfg!(windows) { 2 } else { 1 } {
        return Err(ProtocolError::InvalidPayload);
    }
    let bytes = take(input, length)?;
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        if bytes.contains(&0) {
            return Err(ProtocolError::InvalidPayload);
        }
        std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes))
    };
    #[cfg(windows)]
    let path = {
        use std::os::windows::ffi::OsStringExt;
        if bytes.len() % 2 != 0 {
            return Err(ProtocolError::InvalidPayload);
        }
        let units: Vec<_> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        if units.contains(&0) {
            return Err(ProtocolError::InvalidPayload);
        }
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&units))
    };
    if !path.is_absolute() {
        return Err(ProtocolError::InvalidPayload);
    }
    Ok(Some(path))
}

const MAX_WEBVIEW_URL_BYTES: usize = 16 * 1024;

fn put_webview_url(out: &mut Vec<u8>, url: Option<&str>) -> Result<(), ProtocolError> {
    match url {
        None => put_u32(out, 0),
        Some(url) => {
            if url.is_empty() || url.len() > MAX_WEBVIEW_URL_BYTES || url.as_bytes().contains(&0) {
                return Err(ProtocolError::InvalidPayload);
            }
            put_u32(out, url.len() as u32);
            out.extend_from_slice(url.as_bytes());
        }
    }
    Ok(())
}

fn take_webview_url(input: &mut &[u8]) -> Result<Option<String>, ProtocolError> {
    let length = take_u32(input)? as usize;
    if length == 0 {
        return Ok(None);
    }
    if length > MAX_WEBVIEW_URL_BYTES || length > input.len() {
        return Err(ProtocolError::InvalidPayload);
    }
    let (bytes, rest) = input.split_at(length);
    let url = std::str::from_utf8(bytes).map_err(|_| ProtocolError::InvalidPayload)?;
    if bytes.contains(&0) {
        return Err(ProtocolError::InvalidPayload);
    }
    *input = rest;
    Ok(Some(url.to_owned()))
}

fn put_metadata(out: &mut Vec<u8>, value: &ExecuteMetadata) -> Result<(), ProtocolError> {
    put_webview_url(out, value.webview_url.as_deref())?;
    put_output_path(out, value.staged_output.as_deref())?;
    for v in value.blob_limits {
        put_u64(out, v);
    }
    put_u64(out, value.blob_progress_idle_timeout_secs);
    put_u64(out, value.blob_progress_idle_timeout_nanos);
    for v in [
        value.max_wasm_stack_bytes,
        value.reserved_memory_bytes,
        value.maximum_active_roots,
        value.total_fuel,
        value.root_fuel,
        value.child_fuel,
        value.epoch_deadline_millis,
        value.epoch_tick_millis,
        value.maximum_epoch_registrations,
        value.max_module_bytes,
    ] {
        put_u64(out, v);
    }
    put_u32(out, value.max_shared_memory_pages);
    put_u64(out, value.max_guest_threads);
    Ok(())
}
fn take_metadata(input: &mut &[u8]) -> Result<ExecuteMetadata, ProtocolError> {
    Ok(ExecuteMetadata {
        webview_url: take_webview_url(input)?,
        staged_output: take_output_path(input)?,
        blob_limits: [
            take_u64(input)?,
            take_u64(input)?,
            take_u64(input)?,
            take_u64(input)?,
            take_u64(input)?,
            take_u64(input)?,
            take_u64(input)?,
        ],
        blob_progress_idle_timeout_secs: take_u64(input)?,
        blob_progress_idle_timeout_nanos: take_u64(input)?,
        max_wasm_stack_bytes: take_u64(input)?,
        reserved_memory_bytes: take_u64(input)?,
        maximum_active_roots: take_u64(input)?,
        total_fuel: take_u64(input)?,
        root_fuel: take_u64(input)?,
        child_fuel: take_u64(input)?,
        epoch_deadline_millis: take_u64(input)?,
        epoch_tick_millis: take_u64(input)?,
        maximum_epoch_registrations: take_u64(input)?,
        max_module_bytes: take_u64(input)?,
        max_shared_memory_pages: take_u32(input)?,
        max_guest_threads: take_u64(input)?,
    })
}
fn put_counters(out: &mut Vec<u8>, value: &FinalCounters) {
    for v in [
        value.active_roots,
        value.live_stores,
        value.live_instances,
        value.active_epoch_registrations,
        value.live_threads,
    ] {
        put_u64(out, v);
    }
}
fn take_counters(input: &mut &[u8]) -> Result<FinalCounters, ProtocolError> {
    Ok(FinalCounters {
        active_roots: take_u64(input)?,
        live_stores: take_u64(input)?,
        live_instances: take_u64(input)?,
        active_epoch_registrations: take_u64(input)?,
        live_threads: take_u64(input)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_ack_round_trips_and_rejects_trailing_bytes() {
        let frame = encode(&Message::ExecuteAck { request_id: 41 }).expect("encode ack");
        assert_eq!(decode(&frame), Ok(Message::ExecuteAck { request_id: 41 }));
        let mut trailing = frame;
        trailing.push(0);
        assert_eq!(decode(&trailing), Err(ProtocolError::TrailingBytes));
    }
    #[test]
    fn webview_url_transport_is_bounded_and_preserves_absence() {
        for url in [
            None,
            Some("https://example.test/exact?q=1"),
            Some("https://例え.test/"),
        ] {
            let mut bytes = Vec::new();
            put_webview_url(&mut bytes, url).unwrap();
            let mut input = bytes.as_slice();
            assert_eq!(take_webview_url(&mut input).unwrap().as_deref(), url);
            assert!(input.is_empty());
        }
        for invalid in [
            String::new(),
            "x".repeat(MAX_WEBVIEW_URL_BYTES + 1),
            "https://example.test/\0".into(),
        ] {
            assert!(put_webview_url(&mut Vec::new(), Some(&invalid)).is_err());
        }
        for invalid in [
            vec![1, 0, 0, 0, 0xff],
            vec![2, 0, 0, 0, b'x'],
            vec![1, 0, 0, 0, 0],
            u32::MAX.to_le_bytes().to_vec(),
        ] {
            assert!(take_webview_url(&mut invalid.as_slice()).is_err());
        }
        let mut original = metadata();
        original.webview_url = Some("https://example.test/exact".into());
        let mut bytes = Vec::new();
        put_metadata(&mut bytes, &original).unwrap();
        assert_eq!(take_metadata(&mut bytes.as_slice()).unwrap(), original);
    }

    fn metadata() -> ExecuteMetadata {
        ExecuteMetadata {
            webview_url: None,
            staged_output: None,
            blob_limits: [12, 13, 14, 15, 16, 17, 18],
            blob_progress_idle_timeout_secs: 19,
            blob_progress_idle_timeout_nanos: 20,
            max_wasm_stack_bytes: 1,
            reserved_memory_bytes: 2,
            maximum_active_roots: 3,
            total_fuel: 4,
            root_fuel: 5,
            child_fuel: 6,
            epoch_deadline_millis: 7,
            epoch_tick_millis: 8,
            maximum_epoch_registrations: 9,
            max_module_bytes: 32 * 1024 * 1024,
            max_shared_memory_pages: 10,
            max_guest_threads: 11,
        }
    }
    #[test]
    fn round_trip_start_and_terminal() {
        let start = Message::ExecuteStart {
            request_id: 7,
            module_len: 3,
            metadata: metadata(),
        };
        assert_eq!(decode(&encode(&start).unwrap()).unwrap(), start);
        let terminal = Message::Terminal {
            request_id: 7,
            kind: TerminalKind::Completed,
            detail: TerminalDetail {
                root_outcome: RootOutcome::Started,
                ..TerminalDetail::none()
            },
            diagnostic: "ok".into(),
            counters: FinalCounters {
                active_roots: 0,
                live_stores: 0,
                live_instances: 0,
                active_epoch_registrations: 0,
                live_threads: 0,
            },
        };
        assert_eq!(decode(&encode(&terminal).unwrap()).unwrap(), terminal);
    }

    #[test]
    fn staged_output_round_trips_only_bounded_absolute_native_paths() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("completed-output");
        let mut configuration = metadata();
        configuration.staged_output = Some(path.clone());
        let start = Message::ExecuteStart {
            request_id: 1,
            module_len: 1,
            metadata: configuration,
        };
        assert_eq!(decode(&encode(&start).unwrap()).unwrap(), start);
        let mut bytes = Vec::new();
        put_output_path(&mut bytes, Some(&path)).unwrap();
        for length in 0..bytes.len() {
            assert!(take_output_path(&mut &bytes[..length]).is_err());
        }
        let mut foreign = bytes.clone();
        foreign[4] = if cfg!(windows) { 1 } else { 2 };
        assert_eq!(
            take_output_path(&mut &foreign[..]),
            Err(ProtocolError::InvalidPayload)
        );
        assert!(put_output_path(&mut Vec::new(), Some(std::path::Path::new("relative"))).is_err());
        let oversized = ((MAX_OUTPUT_PATH_BYTES + 1) as u32).to_le_bytes();
        assert_eq!(
            take_output_path(&mut &oversized[..]),
            Err(ProtocolError::InvalidPayload)
        );
    }

    #[cfg(unix)]
    #[test]
    fn staged_output_preserves_non_utf8_unix_paths_and_rejects_nul() {
        use std::os::unix::ffi::OsStringExt;
        for bytes in [b"/output/\xff".to_vec(), b"/output/\0".to_vec()] {
            let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes.clone()));
            let mut encoded = Vec::new();
            if bytes.contains(&0) {
                assert!(put_output_path(&mut encoded, Some(&path)).is_err());
            } else {
                put_output_path(&mut encoded, Some(&path)).unwrap();
                assert_eq!(take_output_path(&mut &encoded[..]).unwrap(), Some(path));
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn staged_output_preserves_unpaired_utf16_windows_paths() {
        use std::os::windows::ffi::OsStringExt;
        let path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0xd800,
        ]));
        let mut encoded = Vec::new();
        put_output_path(&mut encoded, Some(&path)).unwrap();
        assert_eq!(take_output_path(&mut &encoded[..]).unwrap(), Some(path));
    }
    #[test]
    fn round_trip_module_chunk_consumes_its_payload() {
        let chunk = Message::ModuleChunk {
            request_id: 7,
            sequence: 3,
            bytes: vec![0, 1, 2, 3],
        };
        assert_eq!(decode(&encode(&chunk).unwrap()).unwrap(), chunk);
    }
    #[test]
    fn multi_chunk_module_is_ordered_and_exact() {
        let chunk = vec![7; MAX_FRAME_PAYLOAD - 12];
        let mut a = ModuleAssembler::start(
            1,
            (chunk.len() * 2) as u64,
            WORKER_PROTOCOL_MAX_MODULE_BYTES,
        )
        .unwrap();
        assert_eq!(
            a.accept(Message::ModuleChunk {
                request_id: 1,
                sequence: 0,
                bytes: chunk.clone()
            })
            .unwrap(),
            None
        );
        assert_eq!(
            a.accept(Message::ModuleChunk {
                request_id: 1,
                sequence: 1,
                bytes: chunk.clone()
            })
            .unwrap(),
            None
        );
        let module = a
            .accept(Message::ExecuteEnd { request_id: 1 })
            .unwrap()
            .unwrap();
        assert_eq!(module.len(), chunk.len() * 2);
    }
    #[test]
    fn rejects_bad_headers_and_trailing_data() {
        let mut frame = encode(&Message::Hello { request_id: 1 }).unwrap();
        frame[0] = 0;
        assert_eq!(decode(&frame), Err(ProtocolError::BadMagic));
        let mut frame = encode(&Message::Hello { request_id: 1 }).unwrap();
        frame[4] = 1;
        assert_eq!(decode(&frame), Err(ProtocolError::UnsupportedVersion));
        let mut frame = encode(&Message::Hello { request_id: 1 }).unwrap();
        frame[6] = 99;
        assert_eq!(decode(&frame), Err(ProtocolError::UnknownKind));
        let frame = encode(&Message::Hello { request_id: 1 }).unwrap();
        assert_eq!(decode(&frame[..8]), Err(ProtocolError::Truncated));
        let mut frame = frame;
        frame.push(0);
        assert_eq!(decode(&frame), Err(ProtocolError::TrailingBytes));
    }
    #[test]
    fn rejects_oversize_and_invalid_module_lengths() {
        assert!(matches!(
            ModuleAssembler::start(
                1,
                WORKER_PROTOCOL_MAX_MODULE_BYTES + 1,
                WORKER_PROTOCOL_MAX_MODULE_BYTES + 1
            ),
            Err(ProtocolError::ModuleTooLarge)
        ));
        assert!(matches!(
            ModuleAssembler::start(1, 17, 16),
            Err(ProtocolError::ModuleTooLarge)
        ));
        let start = Message::ExecuteStart {
            request_id: 1,
            module_len: 17,
            metadata: ExecuteMetadata {
                max_module_bytes: 16,
                ..metadata()
            },
        };
        assert_eq!(
            decode(&encode(&start).unwrap()),
            Err(ProtocolError::ModuleTooLarge)
        );
        let message = Message::ModuleChunk {
            request_id: 1,
            sequence: 0,
            bytes: vec![0; MAX_FRAME_PAYLOAD],
        };
        assert_eq!(encode(&message), Err(ProtocolError::FrameTooLarge));
    }
    #[test]
    fn rejects_sequence_id_end_and_duplicate_terminal_errors() {
        let mut a = ModuleAssembler::start(5, 1, 1).unwrap();
        assert_eq!(
            a.accept(Message::ModuleChunk {
                request_id: 4,
                sequence: 0,
                bytes: vec![1]
            }),
            Err(ProtocolError::WrongRequestId)
        );
        assert_eq!(
            a.accept(Message::ModuleChunk {
                request_id: 5,
                sequence: 1,
                bytes: vec![1]
            }),
            Err(ProtocolError::Sequence)
        );
        assert_eq!(
            a.accept(Message::ExecuteEnd { request_id: 5 }),
            Err(ProtocolError::UnexpectedMessage)
        );
        assert_eq!(
            a.accept(Message::ModuleChunk {
                request_id: 5,
                sequence: 0,
                bytes: vec![1]
            })
            .unwrap(),
            None
        );
        assert!(a
            .accept(Message::ExecuteEnd { request_id: 5 })
            .unwrap()
            .is_some());
        let terminal = Message::Terminal {
            request_id: 5,
            kind: TerminalKind::Completed,
            detail: TerminalDetail {
                root_outcome: RootOutcome::Started,
                ..TerminalDetail::none()
            },
            diagnostic: String::new(),
            counters: FinalCounters {
                active_roots: 0,
                live_stores: 0,
                live_instances: 0,
                active_epoch_registrations: 0,
                live_threads: 0,
            },
        };
        assert_eq!(a.accept(terminal.clone()).unwrap(), None);
        assert_eq!(a.accept(terminal), Err(ProtocolError::DuplicateTerminal));
    }
    #[test]
    fn diagnostic_is_bounded() {
        let message = Message::Terminal {
            request_id: 1,
            kind: TerminalKind::WorkerFailure,
            detail: TerminalDetail::none(),
            diagnostic: "x".repeat(MAX_DIAGNOSTIC_BYTES + 1),
            counters: FinalCounters {
                active_roots: 0,
                live_stores: 0,
                live_instances: 0,
                active_epoch_registrations: 0,
                live_threads: 0,
            },
        };
        assert_eq!(encode(&message), Err(ProtocolError::DiagnosticTooLarge));
    }
    #[test]
    fn typed_root_outcome_rejections_and_status_round_trip() {
        let counters = FinalCounters {
            active_roots: 0,
            live_stores: 0,
            live_instances: 0,
            active_epoch_registrations: 0,
            live_threads: 0,
        };
        let completed = Message::Terminal {
            request_id: 1,
            kind: TerminalKind::Completed,
            detail: TerminalDetail {
                root_outcome: RootOutcome::StartedWithThreadRejections,
                capacity_rejections: 1,
                closing_rejections: 2,
                fuel_rejections: 3,
                epoch_rejections: 4,
                status_code: None,
            },
            diagnostic: "context".into(),
            counters,
        };
        assert_eq!(decode(&encode(&completed).unwrap()).unwrap(), completed);
        let exit = Message::Terminal {
            request_id: 1,
            kind: TerminalKind::NonzeroExit,
            detail: TerminalDetail {
                status_code: Some(-9),
                ..TerminalDetail::none()
            },
            diagnostic: String::new(),
            counters,
        };
        assert_eq!(decode(&encode(&exit).unwrap()).unwrap(), exit);
    }
    #[test]
    fn terminal_rejects_illegal_machine_combinations() {
        let counters = FinalCounters {
            active_roots: 0,
            live_stores: 0,
            live_instances: 0,
            active_epoch_registrations: 0,
            live_threads: 0,
        };
        let missing_outcome = Message::Terminal {
            request_id: 1,
            kind: TerminalKind::Completed,
            detail: TerminalDetail::none(),
            diagnostic: String::new(),
            counters,
        };
        assert_eq!(
            encode(&missing_outcome),
            Err(ProtocolError::InvalidTerminal)
        );
        let missing_status = Message::Terminal {
            request_id: 1,
            kind: TerminalKind::NonzeroExit,
            detail: TerminalDetail::none(),
            diagnostic: String::new(),
            counters,
        };
        assert_eq!(encode(&missing_status), Err(ProtocolError::InvalidTerminal));
        let cancelled_with_status = Message::Terminal {
            request_id: 1,
            kind: TerminalKind::Cancelled,
            detail: TerminalDetail {
                status_code: Some(1),
                ..TerminalDetail::none()
            },
            diagnostic: String::new(),
            counters,
        };
        assert_eq!(
            encode(&cancelled_with_status),
            Err(ProtocolError::InvalidTerminal)
        );
    }
    #[test]
    fn trace_batch_is_bounded_printable_and_round_trips() {
        for text in [
            "kernal-webview-trace phase=poll\n".to_owned(),
            "x".repeat(MAX_TRACE_BYTES),
        ] {
            let message = Message::Trace {
                request_id: 3,
                text,
            };
            assert_eq!(decode(&encode(&message).unwrap()).unwrap(), message);
        }
        for text in [
            String::new(),
            "x".repeat(MAX_TRACE_BYTES + 1),
            "escape\x1b[31m".into(),
            "nul\0".into(),
            "é".into(),
        ] {
            assert!(encode(&Message::Trace {
                request_id: 3,
                text
            })
            .is_err());
        }
        let mut frame = encode(&Message::Trace {
            request_id: 3,
            text: "ok".into(),
        })
        .unwrap();
        *frame.last_mut().unwrap() = 0xff;
        assert!(decode(&frame).is_err());
        let mut header = frame[..HEADER_LEN].to_vec();
        header[7..11].copy_from_slice(&((MAX_TRACE_BYTES + 9) as u32).to_le_bytes());
        assert_eq!(
            read_message(&mut header.as_slice()),
            Err(ProtocolError::FrameTooLarge)
        );
    }
}
