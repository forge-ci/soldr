//! Per-phase compile tracing for soldr-981 diagnosis.
//!
//! When the `SOLDR_DAEMON_TRACE` env var points to a writable file
//! path, every call to [`record`] appends a single JSONL line with
//! the phase name, elapsed microseconds, and the compile identifier
//! that produced it. Off by default — zero cost when the env var is
//! unset (one atomic-bool read per call).
//!
//! Output format (one JSON object per line):
//! ```jsonl
//! {"ts_ns":<u128>, "phase":"<name>", "micros":<u64>, "compile_id":"<str>"}
//! ```
//!
//! Loaded by `bench/parse_compile_trace.py` (or any JSONL reader) for
//! per-phase p50/p99/total analysis.

use crate::platform::sync::Mutex;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static TRACE_FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

fn init() -> Option<Mutex<std::fs::File>> {
    let path = std::env::var_os("SOLDR_DAEMON_TRACE")?;
    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(file) => {
            // One-line startup confirmation so the harness can verify
            // the daemon process actually picked up the env var.
            eprintln!(
                "soldr-daemon: SOLDR_DAEMON_TRACE active, writing to {}",
                path.display()
            );
            Some(Mutex::new(file))
        }
        Err(e) => {
            eprintln!(
                "soldr-daemon: SOLDR_DAEMON_TRACE set to {} but open failed: {e}",
                path.display()
            );
            None
        }
    }
}

fn writer() -> Option<&'static Mutex<std::fs::File>> {
    TRACE_FILE.get_or_init(init).as_ref()
}

/// Append a single phase-record line to the trace file. No-op when
/// `SOLDR_DAEMON_TRACE` is unset. Errors are silently dropped — the
/// trace file is diagnostic-only and must never block compile work.
pub fn record(phase: &str, micros: u64, compile_id: &str) {
    let Some(file_mu) = writer() else { return };
    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let line = format!(
        r#"{{"ts_ns":{ts_ns},"phase":"{}","micros":{micros},"compile_id":"{}"}}{nl}"#,
        escape(phase),
        escape(compile_id),
        nl = "\n",
    );
    {
        let mut guard = file_mu.lock();
        let _ = guard.write_all(line.as_bytes());
    }
}

fn escape(s: &str) -> String {
    // Tight enough escape for our phase names and content-addressed
    // compile ids. Both come from internal call sites; we don't need
    // a full JSON encoder for this.
    s.replace('\\', r"\\").replace('"', r#"\""#)
}

/// Convenience RAII guard. Construct with the phase name; records the
/// elapsed micros on drop. Use when the phase is a single scope.
pub struct Phase<'a> {
    name: &'a str,
    compile_id: &'a str,
    start: std::time::Instant,
}

impl<'a> Phase<'a> {
    pub fn start(name: &'a str, compile_id: &'a str) -> Self {
        Self {
            name,
            compile_id,
            start: std::time::Instant::now(),
        }
    }
}

impl Drop for Phase<'_> {
    fn drop(&mut self) {
        let micros = self.start.elapsed().as_micros() as u64;
        record(self.name, micros, self.compile_id);
    }
}
