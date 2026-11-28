#![cfg(feature = "wasm-sketch-host")]

use kernal_api::wasm::{
    SketchCompiler, SketchCompilerConfig, SketchModuleError, SketchModulePolicy,
};

// This deliberately has no WAT or parser test dependency. It is the smallest
// structurally valid representation of the closed ThreadedRustV1 contract.
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
    let mut section_contents = Vec::new();
    text(name, &mut section_contents);
    section_contents.extend(contents);
    section(0, section_contents, output);
}

#[derive(Default)]
struct RawThreaded {
    extra_p1_import: bool,
    wrong_entry_signature: bool,
    wrong_entry_kind: bool,
    missing_start: bool,
    mismatched_start: bool,
    missing_memory_export: bool,
    wrong_memory_export: bool,
    missing_feature: bool,
    forbidden_feature: bool,
    disabled_feature: bool,
    duplicate_feature: bool,
    memory_initial: u32,
    memory_maximum: u32,
}

#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn raw_threaded_wasm(options: RawThreaded) -> Vec<u8> {
    let mut wasm = b"\0asm\x01\0\0\0".to_vec();

    // 0: [] -> [], 1: [i32] -> i32, 2: clock, 3: pair -> i32,
    // 4: fd_write, 5: proc_exit, 6: [] -> i32, 7: main/entry,
    // 8: wasi_thread_start.
    let mut types = Vec::new();
    leb(9, &mut types);
    types.extend([
        0x60, 0, 0, // 0
        0x60, 1, 0x7f, 1, 0x7f, // 1
        0x60, 3, 0x7f, 0x7e, 0x7f, 1, 0x7f, // 2
        0x60, 2, 0x7f, 0x7f, 1, 0x7f, // 3
        0x60, 4, 0x7f, 0x7f, 0x7f, 0x7f, 1, 0x7f, // 4
        0x60, 1, 0x7f, 0, // 5
        0x60, 0, 1, 0x7f, // 6
        0x60, 0, 1, 0x7f, // 7
        0x60, 2, 0x7f, 0x7f, 0, // 8
    ]);
    section(1, types, &mut wasm);

    let mut imports = Vec::new();
    leb(9 + u32::from(options.extra_p1_import), &mut imports);
    text("env", &mut imports);
    text("memory", &mut imports);
    imports.extend([2, 3]);
    leb(
        if options.memory_initial == 0 {
            17
        } else {
            options.memory_initial
        },
        &mut imports,
    );
    leb(
        if options.memory_maximum == 0 {
            16_384
        } else {
            options.memory_maximum
        },
        &mut imports,
    );
    text("kernal-api:v1", &mut imports);
    text("kernel_yield", &mut imports);
    imports.extend([0, 0]);
    text("wasi", &mut imports);
    text("thread-spawn", &mut imports);
    imports.extend([0, 1]);
    for (name, type_index) in [
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
        leb(type_index, &mut imports);
    }
    if options.extra_p1_import {
        text("wasi_snapshot_preview1", &mut imports);
        text("random_get", &mut imports);
        imports.push(0);
        leb(3, &mut imports);
    }
    section(2, imports, &mut wasm);

    let mut functions = Vec::new();
    leb(5, &mut functions);
    leb(0, &mut functions); // _start
    leb(7, &mut functions); // __main_void
    leb(8, &mut functions); // wasi_thread_start
    leb(
        if options.wrong_entry_signature { 0 } else { 7 },
        &mut functions,
    );
    leb(0, &mut functions); // dedicated, unexported Rust start function
    section(3, functions, &mut wasm);

    let imported_functions = 8 + u32::from(options.extra_p1_import);
    let mut exports = Vec::new();
    leb(
        if options.missing_memory_export { 4 } else { 5 },
        &mut exports,
    );
    if !options.missing_memory_export {
        text("memory", &mut exports);
        exports.push(if options.wrong_memory_export { 0 } else { 2 });
        leb(
            if options.wrong_memory_export {
                imported_functions
            } else {
                0
            },
            &mut exports,
        );
    }
    for (name, index) in [
        ("_start", imported_functions),
        ("__main_void", imported_functions + 1),
        ("wasi_thread_start", imported_functions + 2),
        ("kernal-api-run", imported_functions + 3),
    ] {
        text(name, &mut exports);
        exports.push(if options.wrong_entry_kind && name == "kernal-api-run" {
            3
        } else {
            0
        });
        leb(index, &mut exports);
    }
    section(7, exports, &mut wasm);

    if !options.missing_start {
        section(
            8,
            vec![
                (if options.mismatched_start {
                    imported_functions + 1
                } else {
                    imported_functions + 4
                }) as u8,
            ],
            &mut wasm,
        );
    }

    // Five bodies include the dedicated, unexported start function.
    let mut code = Vec::new();
    leb(5, &mut code);
    code.extend([2, 0, 0x0b]);
    code.extend([4, 0, 0x41, 0, 0x0b]);
    code.extend([2, 0, 0x0b]);
    if options.wrong_entry_signature {
        code.extend([2, 0, 0x0b]);
    } else {
        code.extend([4, 0, 0x41, 0, 0x0b]);
    }
    code.extend([2, 0, 0x0b]);
    section(10, code, &mut wasm);

    let mut features = Vec::new();
    let mut feature_names = vec![
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
    if options.missing_feature {
        feature_names.pop();
    }
    if options.forbidden_feature {
        feature_names.push("simd128");
    }
    if options.duplicate_feature {
        feature_names.push("atomics");
    }
    leb(feature_names.len() as u32, &mut features);
    for name in feature_names {
        features.push(if options.disabled_feature && name == "atomics" {
            b'-'
        } else {
            b'+'
        });
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

fn threaded_policy(bytes: &[u8]) -> SketchModulePolicy {
    SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, 16_384).expect("threaded policy")
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn raw_threaded_profile_admits_and_rejections_do_not_compile() {
    let positive = raw_threaded_wasm(RawThreaded::default());
    let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
    compiler
        .admit(&positive, threaded_policy(&positive))
        .expect("positive raw threaded profile must admit");
    assert_eq!(compiler.compiled_module_count(), 1);

    let cases: Vec<(RawThreaded, SketchModuleError)> = vec![
        (
            RawThreaded {
                extra_p1_import: true,
                ..Default::default()
            },
            SketchModuleError::ForbiddenImport {
                module: "wasi_snapshot_preview1".to_owned(),
                name: "random_get".to_owned(),
            },
        ),
        (
            RawThreaded {
                wrong_entry_signature: true,
                ..Default::default()
            },
            SketchModuleError::ImportTypeMismatch {
                module: "kernal-api:v1".to_owned(),
                name: "kernal-api-run".to_owned(),
            },
        ),
        (
            RawThreaded {
                wrong_entry_kind: true,
                ..Default::default()
            },
            SketchModuleError::ExportNotAllowed {
                name: "kernal-api-run".to_owned(),
            },
        ),
        (
            RawThreaded {
                missing_start: true,
                ..Default::default()
            },
            SketchModuleError::StartMismatch,
        ),
        (
            RawThreaded {
                mismatched_start: true,
                ..Default::default()
            },
            SketchModuleError::StartMismatch,
        ),
        (
            RawThreaded {
                missing_memory_export: true,
                ..Default::default()
            },
            SketchModuleError::MemoryExportMismatch,
        ),
        (
            RawThreaded {
                wrong_memory_export: true,
                ..Default::default()
            },
            SketchModuleError::MemoryExportMismatch,
        ),
        (
            RawThreaded {
                missing_feature: true,
                ..Default::default()
            },
            SketchModuleError::TargetFeaturesMismatch,
        ),
        (
            RawThreaded {
                forbidden_feature: true,
                ..Default::default()
            },
            SketchModuleError::TargetFeaturesMismatch,
        ),
        (
            RawThreaded {
                disabled_feature: true,
                ..Default::default()
            },
            SketchModuleError::TargetFeaturesMismatch,
        ),
        (
            RawThreaded {
                duplicate_feature: true,
                ..Default::default()
            },
            SketchModuleError::TargetFeaturesMismatch,
        ),
        (
            RawThreaded {
                memory_initial: 16,
                ..Default::default()
            },
            SketchModuleError::ThreadedMemoryMismatch {
                minimum_pages: 16,
                maximum_pages: 16_384,
            },
        ),
        (
            RawThreaded {
                memory_maximum: 16_383,
                ..Default::default()
            },
            SketchModuleError::ThreadedMemoryMismatch {
                minimum_pages: 17,
                maximum_pages: 16_383,
            },
        ),
    ];
    for (case_index, (options, expected)) in cases.into_iter().enumerate() {
        let bytes = raw_threaded_wasm(options);
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
        assert_eq!(
            compiler.admit(&bytes, threaded_policy(&bytes)).err(),
            Some(expected),
            "fixture case {case_index}"
        );
        assert_eq!(
            compiler.compiled_module_count(),
            0,
            "rejected before compilation"
        );
    }

    let bytes = raw_threaded_wasm(RawThreaded::default());
    let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
    let policy = SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, 16_383).expect("policy");
    assert!(matches!(
        compiler.admit(&bytes, policy),
        Err(SketchModuleError::SharedMemoryExceedsPolicy {
            policy_pages: 16_383,
            ..
        })
    ));
    assert_eq!(
        compiler.compiled_module_count(),
        0,
        "rejected before compilation"
    );
}
