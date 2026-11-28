//! Native measurement front door for issue #13. Never executes guest code.
use kernal_api::wasm::{SketchCompiler, SketchCompilerConfig, SketchModulePolicy};
use std::io::Read;
use std::time::Instant;

const MAX_MODULE_BYTES: usize = 32 * 1024 * 1024;

fn read_bounded(reader: impl Read, maximum: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "module exceeds admission upload limit",
        ));
    }
    Ok(bytes)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments
        .next()
        .ok_or("usage: wasm-admission <module.wasm>")?;
    if arguments.next().is_some() {
        return Err("usage: wasm-admission <module.wasm>".into());
    }
    let bytes = read_bounded(std::fs::File::open(path)?, MAX_MODULE_BYTES)?;
    let start = Instant::now();
    let compiler = SketchCompiler::new(SketchCompilerConfig::default())?;
    let compiler_setup_ns = start.elapsed().as_nanos();
    let policy = SketchModulePolicy::threaded_rust_v1(MAX_MODULE_BYTES, 16_384)?;
    let start = Instant::now();
    let sketch = compiler.admit(&bytes, policy)?;
    let admission_ns = start.elapsed().as_nanos();
    // Print only after the public policy and private engine compiler accept.
    // Admission includes native engine compilation, not just Wasm parsing.
    println!(
        "{{\"schema\":1,\"profile\":\"threaded-rust-v1\",\"module_bytes\":{},\"compiler_setup_ns\":{compiler_setup_ns},\"admission_ns\":{admission_ns},\"executed\":false}}",
        bytes.len()
    );
    drop(sketch);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_limit_rejects_overflow_and_accepts_exact_bound() {
        assert_eq!(read_bounded(&b"abcd"[..], 4).unwrap(), b"abcd");
        assert_eq!(
            read_bounded(&b"abcde"[..], 4).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn upload_stops_after_one_overflow_byte_even_for_an_endless_reader() {
        struct Counted {
            read: usize,
        }
        impl Read for Counted {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                output.fill(0);
                self.read += output.len();
                Ok(output.len())
            }
        }
        let mut input = Counted { read: 0 };
        assert!(read_bounded(&mut input, 4).is_err());
        assert_eq!(input.read, 5);
    }

    #[test]
    fn invalid_input_never_reaches_engine_compilation() {
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).unwrap();
        let policy = SketchModulePolicy::threaded_rust_v1(MAX_MODULE_BYTES, 16_384).unwrap();
        assert!(compiler.admit(b"not wasm", policy).is_err());
        assert_eq!(compiler.compiled_module_count(), 0);
    }
}
