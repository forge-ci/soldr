//! Fixed-layout crash records shared by the signal handler and daemon.
//!
//! The writer side deliberately does not use serde/prost: a fatal-signal
//! callback may not allocate, lock, format, or retry an I/O operation. The
//! entire record is prepared ahead of time and emitted with one bounded OS
//! write. Parsing happens in the daemon, where ordinary Rust is safe again.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Override for the owner-private directory containing pending records.
pub const SPOOL_DIR_ENV: &str = "KERNAL_API_CRASH_SPOOL_DIR";
/// Override for durable JSON crash reports written by `rpprobed`.
pub const REPORT_DIR_ENV: &str = "KERNAL_API_CRASH_REPORT_DIR";

pub(crate) const MAGIC: &[u8; 8] = b"RPCRASH1";
pub(crate) const VERSION: u32 = 2;
/// One write, bounded independently of the crashing application's heap size.
pub const RECORD_SIZE: usize = 16 * 1024;
pub(crate) const HEADER_SIZE: usize = 128;
pub(crate) const TEXT_SIZE: usize = 64;
pub(crate) const TEXT_FIELDS: usize = 4;
pub(crate) const MODULE_OFFSET: usize = HEADER_SIZE + TEXT_SIZE * TEXT_FIELDS;
pub(crate) const MAX_MODULES: usize = 16;
pub(crate) const MODULE_SIZE: usize = 128;
pub(crate) const THREAD_OFFSET: usize = MODULE_OFFSET + MAX_MODULES * MODULE_SIZE;
pub(crate) const MAX_THREADS: usize = 32;
pub(crate) const MAX_FRAMES: usize = 16;
pub(crate) const FRAME_SIZE: usize = 16;
pub(crate) const THREAD_SIZE: usize = 16 + MAX_FRAMES * FRAME_SIZE;
pub(crate) const RAW_OFFSET: usize = THREAD_OFFSET + MAX_THREADS * THREAD_SIZE;
pub(crate) const CWD_SIZE: usize = 1024;
pub(crate) const CWD_OFFSET: usize = RECORD_SIZE - CWD_SIZE;
pub(crate) const MAX_RAW_CONTEXT: usize = CWD_OFFSET - RAW_OFFSET;
const V1_MAX_RAW_CONTEXT: usize = RECORD_SIZE - RAW_OFFSET;

const OFF_VERSION: usize = 8;
const OFF_RECORD_SIZE: usize = 12;
const OFF_PID: usize = 16;
const OFF_TID: usize = 24;
const OFF_FAULT_CODE: usize = 32;
const OFF_FAULT_ADDRESS: usize = 40;
const OFF_UNIX_MS: usize = 48;
const OFF_THREAD_COUNT: usize = 56;
const OFF_RAW_LEN: usize = 60;
const OFF_FLAGS: usize = 64;
const OFF_MODULE_COUNT: usize = 68;
const OFF_CREATION_TIME_MS: usize = 72;
const FLAG_TRUNCATED_THREADS: u32 = 1;
const FLAG_TRUNCATED_CONTEXT: u32 = 2;
const FLAG_TRUNCATED_MODULES: u32 = 4;
const KNOWN_FLAGS: u32 = FLAG_TRUNCATED_THREADS | FLAG_TRUNCATED_CONTEXT | FLAG_TRUNCATED_MODULES;

/// Identity copied before the handler is armed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashMetadata {
    /// Coarse application class.
    pub app_class: String,
    /// Human-readable application name.
    pub app_name: String,
    /// Application version.
    pub app_version: String,
    /// Optional instance discriminator.
    pub instance_name: String,
    /// Process creation/install time, paired with `pid` to guard PID reuse.
    pub creation_time_ms: u64,
    /// Working directory captured before entering compromised context.
    pub cwd: String,
}

/// Module identity captured before ASLR state disappears.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashModule {
    /// Canonical path when known, otherwise a stable module name.
    pub identity: String,
}

/// One frame expressed as a stable module-relative offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashFrame {
    /// Index into [`RawCrashReport::modules`], or `None` when unattributed.
    pub module_index: Option<u32>,
    /// Offset from the module base, or raw address when unattributed.
    pub relative_address: u64,
}

/// One pre-captured thread in a crash report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashThread {
    /// OS thread id.
    pub os_tid: u64,
    /// Unsymbolized, ASLR-stable frames, innermost first.
    pub frames: Vec<CrashFrame>,
}

/// Parsed fixed-layout crash report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawCrashReport {
    /// Process id.
    pub pid: u32,
    /// Faulting OS thread id.
    pub tid: u64,
    /// Signal or exception code, in the faulting platform's own namespace.
    ///
    /// The namespace is platform-specific and is not itself recorded, so a
    /// reader must already know which OS produced the record (normally true,
    /// since a spool file is read on the host that wrote it):
    ///
    /// - Linux/Android: the raw `siginfo_t.ssi_signo` value (e.g. `11` for
    ///   `SIGSEGV`).
    /// - Windows: the NTSTATUS `ExceptionCode`, sign-extended from the
    ///   backing `crash-context` crate's `i32` field rather than kept as the
    ///   positive `u32` bit pattern — `STATUS_ACCESS_VIOLATION` (`0xC0000005`)
    ///   arrives as `-1073741819`, not `0xC000_0005`. A decoder must compare
    ///   against `fault_code as i32 as u32` (or the pre-computed negative
    ///   constant), not the positive hex literal.
    /// - macOS: **not a single namespace.** A hardware/Mach fault reports the
    ///   Mach exception kind (e.g. `6` for `EXC_BREAKPOINT`), but the SIGABRT
    ///   predecessor-chain path (see `handle_abort` and the module-level
    ///   abort-chain functions) reports the raw Unix signal number instead
    ///   (`6` for `SIGABRT`, which collides numerically with
    ///   `EXC_BREAKPOINT`). A label decoder for macOS records cannot
    ///   distinguish the two from `fault_code` alone.
    pub fault_code: i64,
    /// Best-effort fault address.
    pub fault_address: u64,
    /// Wall-clock time of the fatal callback.
    pub crash_unix_ms: u64,
    /// Application identity prepared at install time.
    pub metadata: CrashMetadata,
    /// Modules referenced by the sampled frames.
    pub modules: Vec<CrashModule>,
    /// Most recent all-thread cooperative sample.
    pub threads: Vec<CrashThread>,
    /// Raw platform crash context.
    pub raw_context: Vec<u8>,
    /// Whether any bounded field was truncated.
    pub truncated: bool,
}

/// Default owner-private spool directory.
pub fn spool_dir() -> PathBuf {
    std::env::var_os(SPOOL_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| default_owner_root().join("probe-spool"))
}

/// Default durable report directory.
pub fn report_dir() -> PathBuf {
    std::env::var_os(REPORT_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| default_owner_root().join("probe-crashes"))
}

#[cfg(unix)]
fn default_owner_root() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let runtime = PathBuf::from(runtime);
        if runtime.is_absolute() {
            return runtime.join("kernal-api");
        }
    }
    // SAFETY: `geteuid` is side-effect-free and always available on Unix.
    let uid = unsafe { libc::geteuid() };
    std::env::temp_dir().join(format!("kernal-api-{uid}"))
}

#[cfg(not(unix))]
fn default_owner_root() -> PathBuf {
    std::env::temp_dir().join("kernal-api")
}

pub(crate) fn create_sink(
    metadata: &CrashMetadata,
) -> io::Result<(File, PathBuf, [u8; RECORD_SIZE])> {
    create_sink_in(&spool_dir(), metadata)
}

/// Split from [`create_sink`] so the reap can be tested without `set_var`.
pub(crate) fn create_sink_in(
    dir: &Path,
    metadata: &CrashMetadata,
) -> io::Result<(File, PathBuf, [u8; RECORD_SIZE])> {
    create_private_dir(dir)?;
    // Arming is the natural point to pay for one directory read: a sink whose
    // owner exited through `std::process::exit` never ran `CrashGuard::drop`
    // and would otherwise stay here forever (#97).
    reap_abandoned_sinks(dir);
    let now = unix_ms();
    let path = dir.join(format!(
        "pending-{}-{now}-{:016x}.rpcrash",
        std::process::id(),
        random_suffix()
    ));

    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    let mut record = [0u8; RECORD_SIZE];
    initialize(&mut record, metadata);
    Ok((file, path, record))
}

/// Remove sinks their owner can no longer write to (#97).
///
/// The rule is deliberately timid, because losing a real crash record is a far
/// worse outcome than keeping an empty file for one more run. A sink is
/// removed only when the PID in its name is provably gone *and* the file is
/// short of a whole record. Liveness is settled first: a process observed dead
/// cannot have written anything afterwards, whereas reading the size first
/// would let a live process crash into the sink between the two observations.
///
/// Everything ambiguous stays: an unreadable directory, a name this crate did
/// not write, a PID that is alive or that the host declines to answer for, a
/// PID the host has since reissued, a full-length record waiting for a reader,
/// and a `.rpcrash.draining` file a drainer is part-way through converting.
fn reap_abandoned_sinks(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(pid) = sink_owner_pid(&path) else {
            continue;
        };
        if !owner_is_gone(pid) {
            continue;
        }
        // Only now is the size meaningful: nothing more can be appended.
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_file() || metadata.len() >= RECORD_SIZE as u64 {
            continue;
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// The PID out of a `pending-<pid>-<ms>-<suffix>.rpcrash` name this crate wrote.
///
/// The suffix must match exactly, so a `.rpcrash.draining` file — or anything
/// else sharing the directory — is simply not ours to reason about. The PID
/// segment is checked digit by digit rather than handed straight to `parse`,
/// which would accept a leading `+`.
fn sink_owner_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let (pid, _) = name
        .strip_prefix("pending-")?
        .strip_suffix(".rpcrash")?
        .split_once('-')?;
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let pid = pid.parse::<u32>().ok()?;
    (pid != 0).then_some(pid)
}

/// Whether the host says that PID names no process that can still run.
///
/// The number is never cast into a signalling call here; `ProcessLiveness`
/// owns that validation, and a handle it hands back pins the identity, so an
/// exit observed through the handle is the owner's exit and not a reissued
/// PID's. Only an outright "no such process" counts as gone — a refusal, an
/// unsupported host primitive, and a PID outside this host's range are all
/// answers this reap treats as "still alive, leave it alone".
fn owner_is_gone(pid: u32) -> bool {
    use crate::platform::process::ProcessInspectErrorKind;

    match crate::ProcessLiveness::open(pid) {
        Ok(handle) => !handle.is_alive(),
        Err(error) => matches!(error.kind, ProcessInspectErrorKind::NotFound),
    }
}

fn random_suffix() -> u64 {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_ok() {
        u64::from_le_bytes(bytes)
    } else {
        unix_ms() ^ u64::from(std::process::id())
    }
}

/// Create and verify an owner-only directory.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)?;
        let metadata = std::fs::symlink_metadata(path)?;
        // SAFETY: `geteuid` is side-effect-free and always available on Unix.
        let uid = unsafe { libc::geteuid() };
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != uid
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "crash directory must be a non-symlink owned by this user with mode 0700",
            ));
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)?;
    Ok(())
}

pub(crate) fn initialize(record: &mut [u8; RECORD_SIZE], metadata: &CrashMetadata) {
    record.fill(0);
    record[..8].copy_from_slice(MAGIC);
    put_u32(record, OFF_VERSION, VERSION);
    put_u32(record, OFF_RECORD_SIZE, RECORD_SIZE as u32);
    put_u32(record, OFF_PID, std::process::id());
    put_metadata(record, metadata);
}

pub(crate) fn put_metadata(record: &mut [u8; RECORD_SIZE], metadata: &CrashMetadata) {
    record[HEADER_SIZE..MODULE_OFFSET].fill(0);
    record[CWD_OFFSET..].fill(0);
    put_text(record, HEADER_SIZE, &metadata.app_class);
    put_text(record, HEADER_SIZE + TEXT_SIZE, &metadata.app_name);
    put_text(record, HEADER_SIZE + TEXT_SIZE * 2, &metadata.app_version);
    put_text(record, HEADER_SIZE + TEXT_SIZE * 3, &metadata.instance_name);
    put_u64(record, OFF_CREATION_TIME_MS, metadata.creation_time_ms);
    put_text_sized(record, CWD_OFFSET, CWD_SIZE, &metadata.cwd);
}

pub(crate) fn put_sample(
    record: &mut [u8; RECORD_SIZE],
    modules: &[CrashModule],
    threads: &[CrashThread],
) {
    record[MODULE_OFFSET..RAW_OFFSET].fill(0);
    put_u32(record, OFF_FLAGS, 0);
    let module_count = modules.len().min(MAX_MODULES);
    put_u32(record, OFF_MODULE_COUNT, module_count as u32);
    if modules.len() > MAX_MODULES {
        set_flag(record, FLAG_TRUNCATED_MODULES);
    }
    for (index, module) in modules.iter().take(MAX_MODULES).enumerate() {
        let raw = module.identity.as_bytes();
        if raw.len() >= MODULE_SIZE {
            set_flag(record, FLAG_TRUNCATED_MODULES);
        }
        put_text_sized(
            record,
            MODULE_OFFSET + index * MODULE_SIZE,
            MODULE_SIZE,
            &module.identity,
        );
    }

    let count = threads.len().min(MAX_THREADS);
    put_u32(record, OFF_THREAD_COUNT, count as u32);
    if threads.len() > MAX_THREADS {
        set_flag(record, FLAG_TRUNCATED_THREADS);
    }
    for (index, thread) in threads.iter().take(MAX_THREADS).enumerate() {
        let offset = THREAD_OFFSET + index * THREAD_SIZE;
        put_u64(record, offset, thread.os_tid);
        let frame_count = thread
            .frames
            .iter()
            .filter(|frame| {
                frame
                    .module_index
                    .is_none_or(|module| (module as usize) < module_count)
            })
            .count()
            .min(MAX_FRAMES);
        put_u32(record, offset + 8, frame_count as u32);
        if thread.frames.len() > frame_count {
            set_flag(record, FLAG_TRUNCATED_THREADS);
        }
        for (frame_index, frame) in thread
            .frames
            .iter()
            .filter(|frame| {
                frame
                    .module_index
                    .is_none_or(|module| (module as usize) < module_count)
            })
            .take(MAX_FRAMES)
            .enumerate()
        {
            let frame_offset = offset + 16 + frame_index * FRAME_SIZE;
            let module_index = frame.module_index.unwrap_or(u32::MAX);
            put_u32(record, frame_offset, module_index);
            put_u64(record, frame_offset + 8, frame.relative_address);
        }
    }
}

/// Populate crash-only fields. Called from the compromised context.
///
/// # Safety
///
/// `record` must point at a writable [`RECORD_SIZE`] buffer exclusively owned
/// by the handler for the duration of this call. `raw_context` must be valid
/// for `raw_len` bytes.
pub(crate) unsafe fn put_crash(
    record: *mut u8,
    tid: u64,
    fault_code: i64,
    fault_address: u64,
    raw_context: *const u8,
    raw_len: usize,
) {
    // SAFETY: guaranteed by the caller; no allocation occurs.
    let bytes = unsafe { std::slice::from_raw_parts_mut(record, RECORD_SIZE) };
    put_u64(bytes, OFF_TID, tid);
    put_i64(bytes, OFF_FAULT_CODE, fault_code);
    put_u64(bytes, OFF_FAULT_ADDRESS, fault_address);
    put_u64(bytes, OFF_UNIX_MS, unix_ms_signal_safe());
    let copied = raw_len.min(MAX_RAW_CONTEXT);
    put_u32(bytes, OFF_RAW_LEN, copied as u32);
    if raw_len > MAX_RAW_CONTEXT {
        set_flag(bytes, FLAG_TRUNCATED_CONTEXT);
    }
    if copied != 0 {
        // SAFETY: both ranges are valid and disjoint by the caller contract.
        unsafe {
            std::ptr::copy_nonoverlapping(raw_context, bytes.as_mut_ptr().add(RAW_OFFSET), copied);
        }
    }
}

/// Decode a complete handler record.
pub fn parse(bytes: &[u8]) -> io::Result<RawCrashReport> {
    if bytes.len() != RECORD_SIZE || &bytes[..8] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incomplete or invalid crash record",
        ));
    }
    let version = get_u32(bytes, OFF_VERSION);
    if (version != 1 && version != VERSION)
        || get_u32(bytes, OFF_RECORD_SIZE) as usize != RECORD_SIZE
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported crash record version",
        ));
    }
    let flags = get_u32(bytes, OFF_FLAGS);
    let module_count = get_u32(bytes, OFF_MODULE_COUNT) as usize;
    let thread_count = get_u32(bytes, OFF_THREAD_COUNT) as usize;
    let raw_len = get_u32(bytes, OFF_RAW_LEN) as usize;
    let max_raw_context = if version == 1 {
        V1_MAX_RAW_CONTEXT
    } else {
        MAX_RAW_CONTEXT
    };
    if flags & !KNOWN_FLAGS != 0
        || module_count > MAX_MODULES
        || thread_count > MAX_THREADS
        || raw_len > max_raw_context
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "out-of-range crash record field",
        ));
    }
    let modules = (0..module_count)
        .map(|index| CrashModule {
            identity: get_text_sized(bytes, MODULE_OFFSET + index * MODULE_SIZE, MODULE_SIZE),
        })
        .collect::<Vec<_>>();
    let mut threads = Vec::with_capacity(thread_count);
    for index in 0..thread_count {
        let offset = THREAD_OFFSET + index * THREAD_SIZE;
        let frame_count = get_u32(bytes, offset + 8) as usize;
        if frame_count > MAX_FRAMES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "out-of-range crash frame count",
            ));
        }
        let mut frames = Vec::with_capacity(frame_count);
        for frame_index in 0..frame_count {
            let frame_offset = offset + 16 + frame_index * FRAME_SIZE;
            let raw_module = get_u32(bytes, frame_offset);
            let module_index = if raw_module == u32::MAX {
                None
            } else if (raw_module as usize) < module_count {
                Some(raw_module)
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "crash frame references an unknown module",
                ));
            };
            frames.push(CrashFrame {
                module_index,
                relative_address: get_u64(bytes, frame_offset + 8),
            });
        }
        threads.push(CrashThread {
            os_tid: get_u64(bytes, offset),
            frames,
        });
    }
    Ok(RawCrashReport {
        pid: get_u32(bytes, OFF_PID),
        tid: get_u64(bytes, OFF_TID),
        fault_code: get_i64(bytes, OFF_FAULT_CODE),
        fault_address: get_u64(bytes, OFF_FAULT_ADDRESS),
        crash_unix_ms: get_u64(bytes, OFF_UNIX_MS),
        metadata: CrashMetadata {
            app_class: get_text(bytes, HEADER_SIZE),
            app_name: get_text(bytes, HEADER_SIZE + TEXT_SIZE),
            app_version: get_text(bytes, HEADER_SIZE + TEXT_SIZE * 2),
            instance_name: get_text(bytes, HEADER_SIZE + TEXT_SIZE * 3),
            creation_time_ms: if version == 1 {
                0
            } else {
                get_u64(bytes, OFF_CREATION_TIME_MS)
            },
            cwd: if version == 1 {
                String::new()
            } else {
                get_text_sized(bytes, CWD_OFFSET, CWD_SIZE)
            },
        },
        modules,
        threads,
        raw_context: bytes[RAW_OFFSET..RAW_OFFSET + raw_len].to_vec(),
        truncated: flags != 0,
    })
}

/// Encode a report outside a compromised context.
///
/// The native callback uses the lower-level preallocated writer; this helper
/// exists for daemon compatibility tests and future spool migrations.
pub fn encode(report: &RawCrashReport) -> [u8; RECORD_SIZE] {
    let mut bytes = [0; RECORD_SIZE];
    initialize(&mut bytes, &report.metadata);
    put_sample(&mut bytes, &report.modules, &report.threads);
    // SAFETY: both fixed buffers remain valid for the duration of this call.
    unsafe {
        put_crash(
            bytes.as_mut_ptr(),
            report.tid,
            report.fault_code,
            report.fault_address,
            report.raw_context.as_ptr(),
            report.raw_context.len(),
        );
    }
    // Preserve a supplied pid/time for compatibility fixtures.
    put_u32(&mut bytes, OFF_PID, report.pid);
    put_u64(&mut bytes, OFF_UNIX_MS, report.crash_unix_ms);
    bytes
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn unix_ms_signal_safe() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: CLOCK_REALTIME and a valid pointer; clock_gettime is
    // async-signal-safe on the supported Unix platforms.
    if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &raw mut ts) } == 0 {
        (ts.tv_sec as u64)
            .saturating_mul(1_000)
            .saturating_add((ts.tv_nsec as u64) / 1_000_000)
    } else {
        0
    }
}

#[cfg(windows)]
fn unix_ms_signal_safe() -> u64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
    let mut ft = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // SAFETY: the OS fills the stack-local FILETIME.
    unsafe { GetSystemTimeAsFileTime(&raw mut ft) };
    let ticks = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    ticks.saturating_sub(116_444_736_000_000_000) / 10_000
}

fn put_text(bytes: &mut [u8], offset: usize, value: &str) {
    put_text_sized(bytes, offset, TEXT_SIZE, value);
}

fn put_text_sized(bytes: &mut [u8], offset: usize, size: usize, value: &str) {
    let raw = value.as_bytes();
    let length = raw.len().min(size - 1);
    bytes[offset..offset + length].copy_from_slice(&raw[..length]);
}

fn get_text(bytes: &[u8], offset: usize) -> String {
    get_text_sized(bytes, offset, TEXT_SIZE)
}

fn get_text_sized(bytes: &[u8], offset: usize, size: usize) -> String {
    let slice = &bytes[offset..offset + size];
    let end = slice.iter().position(|byte| *byte == 0).unwrap_or(size);
    String::from_utf8_lossy(&slice[..end]).into_owned()
}

fn set_flag(bytes: &mut [u8], flag: u32) {
    put_u32(bytes, OFF_FLAGS, get_u32(bytes, OFF_FLAGS) | flag);
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed range"))
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("fixed range"))
}

fn put_i64(bytes: &mut [u8], offset: usize, value: i64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_i64(bytes: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("fixed range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_record_round_trips_identity_threads_and_context() {
        let metadata = CrashMetadata {
            app_class: "compiler".into(),
            app_name: "worker".into(),
            app_version: "4.6.4".into(),
            instance_name: "west".into(),
            creation_time_ms: 1234,
            cwd: "/work".into(),
        };
        let mut bytes = [0; RECORD_SIZE];
        initialize(&mut bytes, &metadata);
        let modules = vec![CrashModule {
            identity: "/app/worker".into(),
        }];
        put_sample(
            &mut bytes,
            &modules,
            &[CrashThread {
                os_tid: 42,
                frames: vec![
                    CrashFrame {
                        module_index: Some(0),
                        relative_address: 0x1234,
                    },
                    CrashFrame {
                        module_index: Some(0),
                        relative_address: 0x5678,
                    },
                ],
            }],
        );
        let raw = [1u8, 2, 3, 4];
        // SAFETY: both fixed buffers remain valid for the call.
        unsafe {
            put_crash(bytes.as_mut_ptr(), 42, 11, 0xdead, raw.as_ptr(), raw.len());
        }
        let report = parse(&bytes).expect("parse");
        assert_eq!(report.metadata, metadata);
        assert_eq!(report.tid, 42);
        assert_eq!(report.fault_code, 11);
        assert_eq!(report.fault_address, 0xdead);
        assert_eq!(report.modules, modules);
        assert_eq!(report.threads[0].frames[0].relative_address, 0x1234);
        assert_eq!(report.raw_context, raw);
    }

    #[test]
    fn version_one_records_remain_readable_without_new_tags() {
        let metadata = CrashMetadata {
            app_class: "legacy".into(),
            app_name: "worker".into(),
            app_version: "1".into(),
            instance_name: String::new(),
            creation_time_ms: 123,
            cwd: "/new-layout".into(),
        };
        let mut bytes = [0; RECORD_SIZE];
        initialize(&mut bytes, &metadata);
        put_u32(&mut bytes, OFF_VERSION, 1);
        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed.metadata.app_class, "legacy");
        assert_eq!(parsed.metadata.creation_time_ms, 0);
        assert!(parsed.metadata.cwd.is_empty());
    }

    #[test]
    fn partial_record_is_refused() {
        assert!(parse(&[0; 100]).is_err());
    }

    #[test]
    fn excess_threads_and_frames_are_explicitly_truncated() {
        let metadata = CrashMetadata {
            app_class: "a".into(),
            app_name: "b".into(),
            app_version: "c".into(),
            instance_name: String::new(),
            creation_time_ms: 1,
            cwd: "/test".into(),
        };
        let mut bytes = [0; RECORD_SIZE];
        initialize(&mut bytes, &metadata);
        let threads = (0..MAX_THREADS + 1)
            .map(|tid| CrashThread {
                os_tid: tid as u64,
                frames: vec![
                    CrashFrame {
                        module_index: None,
                        relative_address: 1,
                    };
                    MAX_FRAMES + 1
                ],
            })
            .collect::<Vec<_>>();
        put_sample(&mut bytes, &[], &threads);
        let parsed = parse(&bytes).unwrap();
        assert!(parsed.truncated);
        assert_eq!(parsed.threads.len(), MAX_THREADS);
        assert_eq!(parsed.threads[0].frames.len(), MAX_FRAMES);
    }

    #[test]
    fn impossible_counts_and_unknown_flags_are_rejected() {
        let metadata = CrashMetadata {
            app_class: "a".into(),
            app_name: "b".into(),
            app_version: "c".into(),
            instance_name: String::new(),
            creation_time_ms: 1,
            cwd: "/test".into(),
        };
        let mut bytes = [0; RECORD_SIZE];
        initialize(&mut bytes, &metadata);
        put_u32(&mut bytes, OFF_THREAD_COUNT, (MAX_THREADS + 1) as u32);
        assert!(parse(&bytes).is_err());

        initialize(&mut bytes, &metadata);
        put_u32(&mut bytes, OFF_FLAGS, 1 << 31);
        assert!(parse(&bytes).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn permissive_or_symlinked_spool_directories_are_rejected() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let root = tempfile::tempdir().unwrap();
        let permissive = root.path().join("permissive");
        std::fs::create_dir(&permissive).unwrap();
        std::fs::set_permissions(&permissive, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(create_private_dir(&permissive).is_err());

        let private = root.path().join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        let linked = root.path().join("linked");
        symlink(&private, &linked).unwrap();
        assert!(create_private_dir(&linked).is_err());
    }

    /// A PID this host has finished with, obtained the only way that proves
    /// it: run a process to completion and collect it.
    fn exited_pid() -> u32 {
        #[cfg(unix)]
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("spawn");
        let pid = child.id();
        child.wait().expect("collect");
        pid
    }

    /// Arming reclaims the sink of an owner that exited without dropping its
    /// guard, and nothing else in the directory (#97).
    #[test]
    fn arming_reaps_only_an_incomplete_sink_whose_owner_is_gone() {
        let dead = exited_pid();
        let root = tempfile::tempdir().unwrap();
        // Owner-private, exactly as `create_sink` insists on finding it.
        let dir = root.path().join("probe-spool");
        create_private_dir(&dir).expect("spool directory");
        let write = |name: &str, len: usize| {
            let path = dir.join(name);
            std::fs::write(&path, vec![0u8; len]).unwrap();
            path
        };

        let abandoned = write(&format!("pending-{dead}-1-000000000000000a.rpcrash"), 0);
        let survivors = [
            // A live owner's sink, incomplete only because it has not crashed.
            write(
                &format!("pending-{}-2-000000000000000b.rpcrash", std::process::id()),
                0,
            ),
            // A whole record: the owner is gone because it crashed and wrote.
            write(
                &format!("pending-{dead}-3-000000000000000c.rpcrash"),
                RECORD_SIZE,
            ),
            // A drainer's in-progress conversion, which this crate does not own.
            write(
                &format!("pending-{dead}-4-000000000000000d.rpcrash.draining"),
                0,
            ),
            // Names this crate did not write, so no PID can be read out of them.
            write("pending-not-a-pid-5-000000000000000e.rpcrash", 0),
            write(&format!("pending-+{dead}-6-000000000000000f.rpcrash"), 0),
            write("pending-0-7-0000000000000010.rpcrash", 0),
            write(&format!("pending-{dead}.rpcrash"), 0),
            write("crash-report.txt", 0),
        ];

        let metadata = CrashMetadata {
            app_class: "a".into(),
            app_name: "b".into(),
            app_version: "c".into(),
            instance_name: String::new(),
            creation_time_ms: 1,
            cwd: "/test".into(),
        };
        let (_file, fresh, _record) = create_sink_in(&dir, &metadata).expect("sink");

        assert!(!abandoned.exists(), "a dead owner's empty sink must go");
        for survivor in survivors {
            assert!(
                survivor.exists(),
                "must be left alone: {}",
                survivor.display()
            );
        }
        assert!(fresh.exists(), "this process's own sink must survive");
    }
}
