#![cfg(feature = "wasm-sketch-host")]

use crate::threaded_fixture::threaded_root_wasm;
use kernal_api::async_engine::{RuntimeBuilder, RuntimeHandle};
use kernal_api::wasm::{
    SketchCompiler, SketchCompilerConfig, SketchExecutionError, SketchModulePolicy,
    ThreadedRootOutcome,
};

#[test]
fn admitted_threaded_profile_executes_its_start_once_with_a_facade_runtime_handle() {
    let bytes = threaded_root_wasm(None, false, false, false);
    let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
    let policy = SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, 16_384).expect("policy");
    let sketch = compiler.admit(&bytes, policy).expect("admission");
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let outcome = runtime
        .run_within(
            async {
                sketch
                    .execute_threaded_root(RuntimeHandle::current().expect("runtime handle"))
                    .await
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    assert_eq!(
        outcome.expect("root execution"),
        ThreadedRootOutcome::Started
    );
    assert_eq!(
        compiler.compiled_module_count(),
        1,
        "root execution must not compile again"
    );
    let repeated = runtime
        .run_within(
            async {
                sketch
                    .execute_threaded_root(RuntimeHandle::current().expect("runtime handle"))
                    .await
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    assert_eq!(
        repeated.expect("repeated root execution"),
        ThreadedRootOutcome::Started
    );
}

#[test]
fn synthetic_profile_cannot_prepare_or_allocate_a_threaded_root() {
    let bytes = synthetic_root_wasm();
    let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
    let sketch = compiler
        .admit(
            &bytes,
            SketchModulePolicy::new(bytes.len() + 1, 16_384).expect("synthetic policy"),
        )
        .expect("synthetic admission");
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let error = runtime
        .run_within(
            async {
                sketch
                    .execute_threaded_root(RuntimeHandle::current().expect("runtime handle"))
                    .await
                    .expect_err("synthetic execution must fail before preparation")
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    assert_eq!(error, SketchExecutionError::ThreadedProfileRequired);
}

#[test]
fn proc_exit_zero_is_a_controlled_root_completion() {
    let bytes = threaded_root_wasm(Some(0), false, false, false);
    let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
    let policy = SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, 16_384).expect("policy");
    let sketch = compiler.admit(&bytes, policy).expect("admission");
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let outcome = runtime
        .run_within(
            async {
                sketch
                    .execute_threaded_root(RuntimeHandle::current().expect("runtime handle"))
                    .await
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    assert_eq!(
        outcome.expect("normal proc_exit"),
        ThreadedRootOutcome::Exited
    );
}

#[test]
fn proc_exit_nonzero_and_thread_spawn_rejection_are_semantic() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let bytes = threaded_root_wasm(Some(7), false, false, false);
    let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
    let policy = SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, 16_384).expect("policy");
    let sketch = compiler.admit(&bytes, policy).expect("admission");
    let error = runtime
        .run_within(
            async {
                sketch
                    .execute_threaded_root(RuntimeHandle::current().expect("runtime handle"))
                    .await
                    .expect_err("nonzero proc exit")
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    assert_eq!(error, SketchExecutionError::NonzeroExit { code: 7 });

    let bytes = threaded_root_wasm(None, true, false, false);
    let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
    let policy = SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, 16_384)
        .expect("policy")
        .with_max_guest_threads(1)
        .expect("one child cap");
    let sketch = compiler.admit(&bytes, policy).expect("admission");
    let outcome = runtime
        .run_within(
            async {
                sketch
                    .execute_threaded_root(RuntimeHandle::current().expect("runtime handle"))
                    .await
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    match outcome.expect("the second spawn must be rejected without a reservation leak") {
        ThreadedRootOutcome::StartedWithThreadRejections(summary) => {
            assert_eq!(summary.capacity(), 1);
            assert_eq!(summary.closing(), 0);
        }
        outcome => panic!("unexpected root outcome: {outcome:?}"),
    }
    let repeated = runtime
        .run_within(
            async {
                sketch
                    .execute_threaded_root(RuntimeHandle::current().expect("runtime handle"))
                    .await
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    assert!(matches!(
        repeated,
        Ok(ThreadedRootOutcome::StartedWithThreadRejections(summary))
            if summary.capacity() == 1 && summary.closing() == 0
    ));
}

#[test]
fn fd_write_rejects_an_out_of_bounds_iovec_before_writing() {
    let bytes = threaded_root_wasm(None, false, true, false);
    let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
    let policy = SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, 16_384).expect("policy");
    let sketch = compiler.admit(&bytes, policy).expect("admission");
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let outcome = runtime
        .run_within(
            async {
                sketch
                    .execute_threaded_root(RuntimeHandle::current().expect("runtime handle"))
                    .await
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    assert_eq!(
        outcome.expect("invalid iovec is contained"),
        ThreadedRootOutcome::Started
    );
}

fn leb(mut value: u32, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            return;
        }
    }
}
fn text(value: &str, output: &mut Vec<u8>) {
    leb(value.len() as u32, output);
    output.extend(value.as_bytes());
}
fn section(id: u8, contents: Vec<u8>, output: &mut Vec<u8>) {
    output.push(id);
    leb(contents.len() as u32, output);
    output.extend(contents);
}
fn custom(name: &str, contents: &[u8], output: &mut Vec<u8>) {
    let mut body = Vec::new();
    text(name, &mut body);
    body.extend(contents);
    section(0, body, output);
}

// Minimal structurally valid ThreadedRustV1 artifact. The start function is
// intentionally unexported: Wasmtime runs it during instantiate, not by
// calling exported `_start` or `kernal-api-run` separately.
#[allow(dead_code)]
#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn legacy_threaded_root_wasm(
    proc_exit: Option<i32>,
    assert_thread_spawn_rejection: bool,
    assert_fd_write_fault: bool,
    loop_root: bool,
) -> Vec<u8> {
    let mut wasm = b"\0asm\x01\0\0\0".to_vec();
    let mut types = Vec::new();
    leb(9, &mut types);
    types.extend([
        0x60, 0, 0, 0x60, 1, 0x7f, 1, 0x7f, 0x60, 3, 0x7f, 0x7e, 0x7f, 1, 0x7f, 0x60, 2, 0x7f,
        0x7f, 1, 0x7f, 0x60, 4, 0x7f, 0x7f, 0x7f, 0x7f, 1, 0x7f, 0x60, 1, 0x7f, 0, 0x60, 0, 1,
        0x7f, 0x60, 0, 1, 0x7f, 0x60, 2, 0x7f, 0x7f, 0,
    ]);
    section(1, types, &mut wasm);

    let mut imports = Vec::new();
    leb(9, &mut imports);
    text("env", &mut imports);
    text("memory", &mut imports);
    imports.extend([2, 3]);
    leb(17, &mut imports);
    leb(16_384, &mut imports);
    text("kernal-api:v1", &mut imports);
    text("kernel_yield", &mut imports);
    imports.extend([0, 0]);
    text("wasi", &mut imports);
    text("thread-spawn", &mut imports);
    imports.extend([0, 1]);
    for (name, ty) in [
        ("clock_time_get", 2),
        ("environ_get", 3),
        ("environ_sizes_get", 3),
        ("fd_write", 4),
        ("proc_exit", 5),
        ("sched_yield", 6),
    ] {
        text("wasi_snapshot_preview1", &mut imports);
        text(name, &mut imports);
        imports.push(0);
        leb(ty, &mut imports);
    }
    section(2, imports, &mut wasm);
    let mut functions = Vec::new();
    leb(5, &mut functions);
    for ty in [0, 7, 8, 7, 0] {
        leb(ty, &mut functions);
    }
    section(3, functions, &mut wasm);
    let mut exports = Vec::new();
    leb(5, &mut exports);
    text("memory", &mut exports);
    exports.push(2);
    leb(0, &mut exports);
    for (name, index) in [
        ("_start", 8),
        ("__main_void", 9),
        ("wasi_thread_start", 10),
        ("kernal-api-run", 11),
    ] {
        text(name, &mut exports);
        exports.push(0);
        leb(index, &mut exports);
    }
    section(7, exports, &mut wasm);
    section(8, vec![12], &mut wasm);
    let mut code = Vec::new();
    leb(5, &mut code);
    // Root `_start`, `__main_void`, child `wasi_thread_start`, and the
    // `kernal-api-run` export. The module start below remains a no-op: this
    // proves the host enters the command only through `_start`, and that a
    // positive thread-spawn result means a fresh child instance ran and was
    // joined before execute_threaded_root returns.
    if assert_thread_spawn_rejection {
        code.extend([
            // First TID must be positive; the second must be exactly -1.
            // Either ABI violation calls proc_exit with a nonzero code.
            32, 0, 0x41, 0, 0x10, 1, 0x41, 0, 0x4a, 0x45, 0x04, 0x40, 0x41, 6, 0x10, 6, 0x0b, 0x41,
            1, 0x10, 1, 0x41, 0x7f, 0x46, 0x45, 0x04, 0x40, 0x41, 7, 0x10, 6, 0x0b, 0x0b, 4, 0,
            0x41, 0, 0x0b, 6, 0, 0x20, 1, 0x10, 6, 0x0b, // child: proc_exit(arg)
            4, 0, 0x41, 0, 0x0b,
        ]);
    } else if loop_root {
        // `_start`: plain compute loop; it has epoch checks but no atomic wait.
        code.extend([
            10, 0, 0x02, 0x40, 0x03, 0x40, 0x0c, 0, 0x0b, 0x0b, 4, 0, 0x41, 0, 0x0b, 2, 0, 0x0b, 4,
            0, 0x41, 0, 0x0b,
        ]);
    } else {
        code.extend([
            2, 0, 0x0b, 4, 0, 0x41, 0, 0x0b, 2, 0, 0x0b, 4, 0, 0x41, 0, 0x0b,
        ]);
    }
    if let Some(exit_code) = proc_exit {
        // Imported function index 6 is `wasi_snapshot_preview1::proc_exit`.
        code.extend([6, 0, 0x41, exit_code as u8, 0x10, 6, 0x0b]);
    } else if assert_thread_spawn_rejection {
        code.extend([2, 0, 0x0b]);
    } else if assert_fd_write_fault {
        // `fd_write` import index 5 must reject the negative iovec pointer.
        code.extend([
            32, 0, 0x41, 1, 0x41, 0x7f, 0x41, 1, 0x41, 0, 0x10, 5, 0x41, 21, 0x47, 0x04, 0x40,
            0x00, 0x0b, 0x41, 0, 0xfe, 0x10, 2, 0, 0x41, 42, 0x47, 0x04, 0x40, 0x00, 0x0b, 0x0b,
        ]);
    } else {
        code.extend([2, 0, 0x0b]);
    }
    section(10, code, &mut wasm);
    if assert_fd_write_fault {
        // A nonzero `nwritten` sentinel proves bad iovecs cannot trigger the
        // host's zero-byte result write before validation has failed.
        section(11, vec![1, 0, 0x41, 0, 0x0b, 4, 42, 0, 0, 0], &mut wasm);
    }
    let mut features = Vec::new();
    let names = [
        "atomics",
        "bulk-memory",
        "bulk-memory-opt",
        "call-indirect-overlong",
        "extended-const",
        "multivalue",
        "mutable-globals",
        "nontrapping-fptoint",
        "reference-types",
        "sign-ext",
    ];
    leb(names.len() as u32, &mut features);
    for name in names {
        features.push(b'+');
        text(name, &mut features);
    }
    custom("target_features", &features, &mut wasm);
    custom(
        "kernal-api.abi",
        crate::threaded_fixture::abi_metadata::METADATA,
        &mut wasm,
    );
    wasm
}

fn synthetic_root_wasm() -> Vec<u8> {
    let mut wasm = b"\0asm\x01\0\0\0".to_vec();
    let mut types = Vec::new();
    leb(2, &mut types);
    types.extend([0x60, 0, 0, 0x60, 1, 0x7f, 1, 0x7f]);
    section(1, types, &mut wasm);
    let mut imports = Vec::new();
    leb(3, &mut imports);
    text("env", &mut imports);
    text("memory", &mut imports);
    imports.extend([2, 3]);
    leb(17, &mut imports);
    leb(16_384, &mut imports);
    text("kernal-api:v1", &mut imports);
    text("kernel_yield", &mut imports);
    imports.extend([0, 0]);
    text("wasi", &mut imports);
    text("thread-spawn", &mut imports);
    imports.extend([0, 1]);
    section(2, imports, &mut wasm);
    section(3, vec![1, 0], &mut wasm);
    let mut exports = Vec::new();
    leb(1, &mut exports);
    text("kernal-api-run", &mut exports);
    exports.extend([0, 2]);
    section(7, exports, &mut wasm);
    section(10, vec![1, 2, 0, 0x0b], &mut wasm);
    custom(
        "kernal-api.abi",
        crate::threaded_fixture::abi_metadata::METADATA,
        &mut wasm,
    );
    custom("kernal-api.profile", b"threaded-core-wasm-v1", &mut wasm);
    wasm
}
