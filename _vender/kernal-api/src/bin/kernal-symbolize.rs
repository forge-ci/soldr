//! The symbolization worker: one capture in, one report out (#637).
//!
//! Reads a protobuf `RawCapture` on stdin and writes a protobuf `SymbolReport`
//! on
//! stdout, exits zero. Any failure exits non-zero with a diagnostic on stderr
//! and nothing on stdout — a partial report would be indistinguishable from a
//! complete one describing a quieter program.
//!
//! One capture per invocation, deliberately. The process is the unit of
//! isolation, so reusing it across captures would let one malformed input take
//! down an unrelated caller's work.

use std::io::{Read as _, Write as _};

use kernal_api::symbolize::RawCapture;

#[path = "../symbolize/discovery.rs"]
mod discovery;
#[path = "../symbolize/line_numbers.rs"]
mod line_numbers;
#[cfg(not(target_os = "windows"))]
#[path = "../symbolize/object_symbols.rs"]
mod object_symbols;
#[cfg(target_os = "windows")]
// The shared parser also carries Windows verification probes used by its unit tests.
#[allow(dead_code)]
#[path = "../symbolize/pdb_symbols.rs"]
mod pdb_symbols;
#[path = "../symbolize/render.rs"]
mod render;
#[path = "../symbolize/resolve.rs"]
mod resolve;

/// Largest capture accepted.
///
/// Enforced against the byte count *before* parsing, so an oversized payload
/// is refused rather than allocated. Sized well above any real capture: the
/// S6 stack cap is 256 KiB per thread, so this admits thousands of threads.
const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

fn main() {
    if let Err(message) = run() {
        eprintln!("kernal-symbolize: {message}");
        // Non-zero exit IS the contract: the daemon reads it as a degraded
        // symbolization and keeps running.
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut input = Vec::new();
    std::io::stdin()
        .lock()
        // take() bounds the read itself, so an endless stdin cannot exhaust
        // memory while we wait to check a length we never reach.
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut input)
        .map_err(|e| format!("cannot read capture from stdin: {e}"))?;

    if input.len() as u64 > MAX_INPUT_BYTES {
        return Err(format!(
            "capture exceeds the {MAX_INPUT_BYTES}-byte limit; refusing to parse it"
        ));
    }

    let capture = RawCapture::decode_wire(&input)
        .map_err(|e| format!("capture is not valid protobuf for this schema: {e}"))?;

    let report = resolve::symbolize(&capture).map_err(|e| format!("symbolization failed: {e}"))?;

    // `--text` renders for a person; the default stays protobuf because the
    // daemon is the usual caller and parses it.
    let encoded = if std::env::args().any(|a| a == "--text") {
        render::render_text(&report).into_bytes()
    } else {
        report.encode_wire()
    };

    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&encoded)
        .map_err(|e| format!("cannot write the report: {e}"))?;
    stdout
        .flush()
        .map_err(|e| format!("cannot flush the report: {e}"))?;
    Ok(())
}
