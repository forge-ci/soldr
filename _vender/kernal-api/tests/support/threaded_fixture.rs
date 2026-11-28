#[path = "abi_metadata.rs"]
pub(super) mod abi_metadata;

pub(crate) fn leb(mut value: u32, output: &mut Vec<u8>) {
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
pub(crate) fn text(value: &str, output: &mut Vec<u8>) {
    leb(value.len() as u32, output);
    output.extend(value.as_bytes());
}
pub(crate) fn section(id: u8, contents: Vec<u8>, output: &mut Vec<u8>) {
    output.push(id);
    leb(contents.len() as u32, output);
    output.extend(contents);
}
pub(crate) fn custom(name: &str, contents: &[u8], output: &mut Vec<u8>) {
    let mut body = Vec::new();
    text(name, &mut body);
    body.extend(contents);
    section(0, body, output);
}
#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
pub(crate) fn threaded_root_wasm(
    proc_exit: Option<i32>,
    rejected: bool,
    fault: bool,
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
    if rejected {
        code.extend([
            32, 0, 0x41, 0, 0x10, 1, 0x41, 0, 0x4a, 0x45, 0x04, 0x40, 0x41, 6, 0x10, 6, 0x0b, 0x41,
            1, 0x10, 1, 0x41, 0x7f, 0x46, 0x45, 0x04, 0x40, 0x41, 7, 0x10, 6, 0x0b, 0x0b, 4, 0,
            0x41, 0, 0x0b, 6, 0, 0x20, 1, 0x10, 6, 0x0b, 4, 0, 0x41, 0, 0x0b,
        ]);
    } else if loop_root {
        code.extend([
            7, 0, 0x03, 0x40, 0x0c, 0, 0x0b, 0x0b, 4, 0, 0x41, 0, 0x0b, 2, 0, 0x0b, 4, 0, 0x41, 0,
            0x0b,
        ]);
    } else {
        code.extend([
            2, 0, 0x0b, 4, 0, 0x41, 0, 0x0b, 2, 0, 0x0b, 4, 0, 0x41, 0, 0x0b,
        ]);
    }
    if let Some(exit) = proc_exit {
        code.extend([6, 0, 0x41, exit as u8, 0x10, 6, 0x0b]);
    } else if rejected {
        code.extend([2, 0, 0x0b]);
    } else if fault {
        code.extend([
            32, 0, 0x41, 1, 0x41, 0x7f, 0x41, 1, 0x41, 0, 0x10, 5, 0x41, 21, 0x47, 0x04, 0x40,
            0x00, 0x0b, 0x41, 0, 0xfe, 0x10, 2, 0, 0x41, 42, 0x47, 0x04, 0x40, 0x00, 0x0b, 0x0b,
        ]);
    } else {
        code.extend([2, 0, 0x0b]);
    }
    section(10, code, &mut wasm);
    if fault {
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
    custom("kernal-api.abi", abi_metadata::METADATA, &mut wasm);
    wasm
}
#[allow(dead_code)] // used by the sibling wasm_epoch_cancellation integration crate.
pub(crate) fn looping_root_wasm() -> Vec<u8> {
    threaded_root_wasm(None, false, false, true)
}

/// A true root-level `unreachable` trap.  Keep this body shape aligned with
/// the core-Wasm host fixtures: only the exported root is replaced.
#[allow(dead_code)] // used only by the wasm_worker_containment integration crate.
pub(crate) fn unreachable_root_wasm() -> Vec<u8> {
    threaded_code_fixture([
        vec![0, 0x00, 0x0b],
        vec![0, 0x41, 0, 0x0b],
        vec![0, 0x0b],
        vec![0, 0x00, 0x0b],
        vec![0, 0x0b],
    ])
}

/// The exact shared-memory `memory.atomic.wait32(0, 0, -1)` blocker used by
/// the private core-Wasm fixture.  It deliberately has no wake-up path.
#[allow(dead_code)] // used only by the wasm_worker_containment integration crate.
pub(crate) fn atomic_wait32_wasm() -> Vec<u8> {
    threaded_code_fixture([
        vec![
            0, 0x10, 0, 0x41, 0, 0x41, 0, 0x42, 0x7f, 0xfe, 0x01, 0x02, 0, 0x1a, 0x0b,
        ],
        vec![0, 0x41, 0, 0x0b],
        vec![0, 0x0b],
        vec![0, 0x41, 0, 0x0b],
        vec![0, 0x0b],
    ])
}

#[allow(dead_code)] // reached exclusively through the two containment-only fixtures above.
fn threaded_code_fixture(bodies: [Vec<u8>; 5]) -> Vec<u8> {
    let bytes = threaded_root_wasm(None, false, false, false);
    let mut section_offset = 8;
    loop {
        let id = bytes[section_offset];
        let mut body_at = section_offset + 1;
        let mut length = 0_usize;
        let mut shift = 0;
        loop {
            let byte = bytes[body_at];
            body_at += 1;
            length |= usize::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let end = body_at + length;
        if id == 10 {
            let mut code = Vec::new();
            leb(bodies.len() as u32, &mut code);
            for body in bodies {
                leb(body.len() as u32, &mut code);
                code.extend(body);
            }
            let mut output = bytes[..section_offset].to_vec();
            section(10, code, &mut output);
            output.extend_from_slice(&bytes[end..]);
            return output;
        }
        section_offset = end;
    }
}
