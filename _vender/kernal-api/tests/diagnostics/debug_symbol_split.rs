//! Round-trip for the post-link debug-symbol split (#78).
//!
//! The point of these tests is that none of them assert on file sizes. #78 is
//! a footgun precisely because the size number can be right while the sidecar
//! resolves nothing, so every positive assertion here ends in a symbol name
//! that came back through the produced symbol file.
//!
//! Linux only, and skipped rather than failed when the host has no binutils
//! or no compiler to build the fixture with: a machine without `objcopy` is a
//! machine that cannot exercise this mechanism, which is a different fact from
//! the mechanism being broken.

#![cfg(all(target_os = "linux", feature = "symbolize-split"))]

use std::path::{Path, PathBuf};
use std::process::Command;

use kernal_api::symbolize::split::{
    split_debug_symbols, DebugSplitError, DebugSplitRequest, SplitMechanism,
};

const RUST_FIXTURE: &str = r#"
#[no_mangle]
#[inline(never)]
pub extern "C" fn kernal_fixture_target(value: u64) -> u64 {
    value.wrapping_mul(2654435761)
}

fn main() {
    println!("{}", kernal_fixture_target(std::env::args().count() as u64));
}
"#;

const C_FIXTURE: &str = r#"
#include <stdio.h>

__attribute__((noinline)) unsigned long kernal_fixture_target(unsigned long value) {
    return value * 2654435761UL;
}

int main(int argc, char **argv) {
    (void)argv;
    printf("%lu\n", kernal_fixture_target((unsigned long)argc));
    return 0;
}
"#;

/// Build a tiny executable with debug info, optionally with a GNU build-id.
///
/// `rustc` first because it is what a consumer of this crate ships, `cc` as a
/// fallback so a host with a toolchain but no `rustc` on `PATH` still runs the
/// round trip. `None` means neither compiler is available, and the caller
/// skips.
fn build_fixture(directory: &Path, name: &str, build_id: bool) -> Option<PathBuf> {
    let binary = directory.join(name);
    let rust_source = directory.join(format!("{name}.rs"));
    std::fs::write(&rust_source, RUST_FIXTURE).expect("write the Rust fixture source");
    // `--build-id=none` is spelled out rather than left to the default: some
    // distributions patch their linker driver to pass `--build-id`, which
    // would make the negative test silently assert nothing.
    let link_argument = if build_id {
        "-Clink-arg=-Wl,--build-id"
    } else {
        "-Clink-arg=-Wl,--build-id=none"
    };
    // Compile from the fixture's own directory, never from the repository:
    // there `rust-toolchain.toml` pins a channel, and a `rustc` that is a
    // rustup proxy on a host without that channel (the aarch64 test runner)
    // downloads it -- concurrently, from every fixture at once -- which hung
    // a test past its 120 s budget. `RUSTUP_AUTO_INSTALL=0` keeps a test from
    // ever installing a toolchain: no usable `rustc` falls through to `cc`.
    let rustc = Command::new("rustc")
        .current_dir(directory)
        .env("RUSTUP_AUTO_INSTALL", "0")
        .args(["-g", "-Cdebuginfo=2", link_argument, "-o"])
        .arg(&binary)
        .arg(&rust_source)
        .output();
    if matches!(&rustc, Ok(output) if output.status.success()) {
        return Some(binary);
    }

    let c_source = directory.join(format!("{name}.c"));
    std::fs::write(&c_source, C_FIXTURE).expect("write the C fixture source");
    let cc = Command::new("cc")
        .args(["-g", "-O0"])
        .arg(if build_id {
            "-Wl,--build-id"
        } else {
            "-Wl,--build-id=none"
        })
        .arg("-o")
        .arg(&binary)
        .arg(&c_source)
        .output();
    matches!(&cc, Ok(output) if output.status.success()).then_some(binary)
}

/// Run a binutils command, reporting whether the tool was there at all.
fn run_binutils(program: &str, arguments: &[&std::ffi::OsStr]) -> bool {
    Command::new(program)
        .args(arguments)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn skip(reason: &str) {
    eprintln!("skipping debug-symbol-split test: {reason}");
}

/// A split fixture, or `None` when this host cannot build or split one.
fn split_fixture(directory: &Path, name: &str) -> Option<kernal_api::symbolize::split::DebugSplit> {
    let binary = build_fixture(directory, name, true)?;
    match split_debug_symbols(&DebugSplitRequest::new(&binary)) {
        Ok(split) => Some(split),
        Err(DebugSplitError::MechanismUnavailable { missing, .. }) => {
            skip(&format!("this host has no {}", missing.join(" or ")));
            None
        }
        Err(error) => panic!("splitting {} failed: {error}", binary.display()),
    }
}

#[test]
fn the_split_moves_the_debug_info_out_of_the_shipped_binary() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let Some(binary) = build_fixture(directory.path(), "fixture", true) else {
        skip("neither rustc nor cc could build the fixture");
        return;
    };
    let Some(split) = split_fixture(directory.path(), "fixture") else {
        return;
    };

    assert_eq!(split.mechanism(), SplitMechanism::GnuDebugLink);
    // The reported mechanism is what a caller gates on; #78's whole complaint
    // is that a partial split reported nothing at all.
    assert!(split.mechanism().covers_linked_image());
    assert_eq!(split.binary(), binary.as_path());
    assert_eq!(split.symbol_file(), directory.path().join("fixture.debug"));
    assert!(
        split.build_identity().starts_with("elf:"),
        "expected an ELF build-id identity, got {}",
        split.build_identity()
    );
    assert!(split.symbol_file().is_file());

    // `split_debug_symbols` already refused any pair that failed inspection,
    // so reaching here means the DWARF is in the sidecar, gone from the
    // binary, and the `.gnu_debuglink` CRC-32 matches. Size is checked only
    // as a sanity signal, never as the verdict.
    let binary_bytes = std::fs::metadata(split.binary())
        .expect("stripped binary")
        .len();
    let symbol_bytes = std::fs::metadata(split.symbol_file())
        .expect("symbol file")
        .len();
    assert!(
        symbol_bytes > 0 && binary_bytes > 0,
        "both artifacts must exist: binary {binary_bytes}, symbols {symbol_bytes}"
    );
}

#[test]
fn a_binary_without_a_build_id_is_refused_before_any_tool_runs() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let Some(binary) = build_fixture(directory.path(), "no-build-id", false) else {
        skip("neither rustc nor cc could build the fixture");
        return;
    };
    let before = std::fs::metadata(&binary).expect("fixture").len();

    match split_debug_symbols(&DebugSplitRequest::new(&binary)) {
        Err(DebugSplitError::BuildIdentityMissing { binary: reported }) => {
            assert_eq!(reported, binary);
        }
        // A host whose linker driver forces `--build-id` cannot produce the
        // input this test needs; that is a property of the host, not a
        // failure of the operation.
        Ok(split) => {
            assert!(split.build_identity().starts_with("elf:"));
            skip("this host's linker always emits a build-id");
            return;
        }
        Err(DebugSplitError::MechanismUnavailable { missing, .. }) => {
            skip(&format!("this host has no {}", missing.join(" or ")));
        }
        Err(error) => panic!("unexpected failure: {error}"),
    }

    // Whatever happened, the refusal path must not have half-stripped the
    // input: an identity check that damages the artifact it rejects is worse
    // than no check.
    assert_eq!(std::fs::metadata(&binary).expect("fixture").len(), before);
    assert!(!directory.path().join("no-build-id.debug").exists());
}

#[test]
fn an_already_stripped_binary_is_refused_rather_than_given_an_empty_sidecar() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let Some(binary) = build_fixture(directory.path(), "prestripped", true) else {
        skip("neither rustc nor cc could build the fixture");
        return;
    };
    // This is the second half of #78: `strip` in the release profile makes the
    // size number right and the sidecar useless. Splitting such a binary must
    // fail loudly instead of writing a symbol file that resolves nothing.
    if !run_binutils("strip", &["--strip-debug".as_ref(), binary.as_os_str()]) {
        skip("this host has no strip");
        return;
    }

    match split_debug_symbols(&DebugSplitRequest::new(&binary)) {
        Err(DebugSplitError::DebugInfoMissing { binary: reported }) => {
            assert_eq!(reported, binary);
        }
        Err(DebugSplitError::MechanismUnavailable { missing, .. }) => {
            skip(&format!("this host has no {}", missing.join(" or ")));
        }
        other => panic!("a stripped binary must be refused, got {other:?}"),
    }
}

/// The resolution half of "verify, do not assume".
///
/// Gated on the worker feature because the proof is deliberately off-process:
/// the symbol file is parsed by `kernal-symbolize`, the same isolated worker a
/// production caller would use, not by an in-process parser written for the
/// test.
#[cfg(feature = "symbolize-worker")]
mod resolution {
    use super::*;

    use kernal_api::symbolize::SymbolizerWorker;

    /// The function every positive test resolves.
    ///
    /// `#[no_mangle]`/`noinline` in both fixture languages, so the
    /// linker-level name is exactly this and the body cannot be folded away.
    const TARGET_FUNCTION: &str = "kernal_fixture_target";

    /// The isolated worker, or `None` when this lane never built it.
    ///
    /// Cargo defines `CARGO_BIN_EXE_kernal-symbolize` only where it actually
    /// produces the binary, so `env!` made this file impossible to compile
    /// under `cargo check --all-targets` -- which is the lane the boundary
    /// lints run in. `option_env!` keeps the compile-time path under
    /// `cargo test`, where the round trip still really runs, and degrades to a
    /// skip in a check-only lane rather than to a compile error. A run-time
    /// `env::var` would not do: Cargo sets this for the compiler, not for the
    /// test process, so it would skip everywhere and prove nothing.
    ///
    /// nextest's `NEXTEST_BIN_EXE_…` is the exception, and it comes first: a
    /// replayed archive carries the builder's compile-time path, which does
    /// not exist on the host running it, while nextest exports where it
    /// actually extracted the binary. Same order as `wasm_worker_containment`.
    fn worker() -> Option<SymbolizerWorker> {
        if let Some(exported) = std::env::var_os("NEXTEST_BIN_EXE_kernal-symbolize") {
            return Some(SymbolizerWorker::new(exported));
        }
        let Some(path) = option_env!("CARGO_BIN_EXE_kernal-symbolize") else {
            skip("this lane did not build kernal-symbolize");
            return None;
        };
        Some(SymbolizerWorker::new(path))
    }

    #[tokio::test]
    async fn the_symbolizer_resolves_a_known_function_through_the_symbol_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let Some(split) = split_fixture(directory.path(), "resolved") else {
            return;
        };
        let Some(worker) = worker() else {
            return;
        };

        let verified = split
            .verify_resolves(&worker, TARGET_FUNCTION)
            .await
            .expect("the produced symbol file must resolve the fixture function");

        assert_eq!(verified.function(), TARGET_FUNCTION);
        assert!(verified.module_offset() > 0);
        // The name came out of the symbol file, not out of the stripped
        // binary's own table: that is the difference between a sidecar that
        // ships and a sidecar that works.
        assert_eq!(verified.symbol_file(), split.symbol_file());
    }

    #[tokio::test]
    async fn a_symbol_file_from_another_build_fails_verification() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let Some(split) = split_fixture(directory.path(), "mine") else {
            return;
        };
        let Some(other) = split_fixture(directory.path(), "theirs") else {
            return;
        };
        let Some(worker) = worker() else {
            return;
        };
        assert_ne!(split.build_identity(), other.build_identity());

        // Exactly the failure a size check cannot see: a well-formed symbol
        // file of the right shape and size that describes a different build.
        std::fs::copy(other.symbol_file(), split.symbol_file()).expect("swap the symbol file");

        match split.verify_resolves(&worker, TARGET_FUNCTION).await {
            Err(DebugSplitError::Unresolved { module_status, .. }) => {
                assert_ne!(
                    module_status,
                    kernal_api::symbolize::wire::ModuleSymbolStatus::Resolved
                );
            }
            other => panic!("a mismatched symbol file must be refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unknown_function_is_reported_rather_than_guessed() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let Some(split) = split_fixture(directory.path(), "unknown") else {
            return;
        };
        let Some(worker) = worker() else {
            return;
        };

        match split
            .verify_resolves(&worker, "no_such_function_exists")
            .await
        {
            Err(DebugSplitError::FunctionAddressUnknown { function, .. }) => {
                assert_eq!(function, "no_such_function_exists");
            }
            other => panic!("an absent function must be reported, got {other:?}"),
        }
    }
}
