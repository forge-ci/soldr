#![cfg(feature = "wasm-sketch-host")]
#![allow(clippy::field_reassign_with_default)]

use kernal_api::wasm::{
    SketchCompiler, SketchCompilerConfig, SketchModuleError, SketchModulePolicy,
};

fn generated_abi_metadata() -> Vec<u8> {
    crate::threaded_fixture::abi_metadata::METADATA.to_vec()
}

fn compiler() -> SketchCompiler {
    SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler")
}
/// Fixtures embed the complete generated ABI contract, which grows with every
/// operation revision, and the duplicate-metadata case embeds it twice. Keep
/// the module ceiling well above that so each case reaches the admission check
/// it names; `ModuleTooLarge` is exercised with its own tiny policy.
fn policy() -> SketchModulePolicy {
    SketchModulePolicy::new(64 * 1024, 16).expect("policy")
}
fn leb(mut n: u64, o: &mut Vec<u8>) {
    loop {
        let mut b = (n & 127) as u8;
        n >>= 7;
        if n != 0 {
            b |= 128;
        }
        o.push(b);
        if n == 0 {
            return;
        }
    }
}
fn text(s: &str, o: &mut Vec<u8>) {
    leb(s.len() as u64, o);
    o.extend(s.as_bytes());
}
fn section(id: u8, b: Vec<u8>, o: &mut Vec<u8>) {
    o.push(id);
    leb(b.len() as u64, o);
    o.extend(b);
}
fn custom(n: &str, d: &[u8], o: &mut Vec<u8>) {
    let mut b = Vec::new();
    text(n, &mut b);
    b.extend(d);
    section(0, b, o);
}

#[derive(Clone, Copy)]
struct F {
    thread: bool,
    yield_: bool,
    shared: bool,
    max: Option<u32>,
    mem64: bool,
    entry_param: bool,
    extra: Option<u8>,
    start: bool,
    duplicate: bool,
    bad_meta: bool,
    duplicate_memory: bool,
    alternate_memory: bool,
    ambient_wasi: bool,
    custom_page_size: bool,
    oversized_metadata: bool,
    defined_memory: bool,
    initial: u32,
    wrong_thread_signature: bool,
    wrong_yield_signature: bool,
    non_function_yield: bool,
    legacy_yield_name: bool,
    non_function_entry: bool,
}
impl Default for F {
    fn default() -> Self {
        Self {
            thread: true,
            yield_: true,
            shared: true,
            max: Some(16),
            mem64: false,
            entry_param: false,
            extra: None,
            start: false,
            duplicate: false,
            bad_meta: false,
            duplicate_memory: false,
            alternate_memory: false,
            ambient_wasi: false,
            custom_page_size: false,
            oversized_metadata: false,
            defined_memory: false,
            initial: 1,
            wrong_thread_signature: false,
            wrong_yield_signature: false,
            non_function_yield: false,
            legacy_yield_name: false,
            non_function_entry: false,
        }
    }
}
#[expect(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn wasm(f: F) -> Vec<u8> {
    let mut w = b"\0asm\x01\0\0\0".to_vec();
    let mut t = Vec::new();
    let has_non_function = f.non_function_yield || f.non_function_entry;
    leb(
        2 + u64::from(f.entry_param) + u64::from(has_non_function),
        &mut t,
    );
    t.extend([0x60, 0, 0, 0x60, 1, 0x7f, 1, 0x7f]);
    if f.entry_param {
        t.extend([0x60, 1, 0x7f, 0]);
    }
    if has_non_function {
        // GC struct with no fields. It is deliberately not a function type.
        t.extend([0x5f, 0]);
    }
    section(1, t, &mut w);
    let mut i = Vec::new();
    leb(
        (1 + u32::from(f.thread)
            + u32::from(f.yield_)
            + u32::from(f.duplicate_memory)
            + u32::from(f.alternate_memory)
            + u32::from(f.ambient_wasi)) as u64,
        &mut i,
    );
    text("env", &mut i);
    text("memory", &mut i);
    i.push(2);
    let mut flags = if f.max.is_some() { 1 } else { 0 };
    if f.shared {
        flags |= 2
    };
    if f.mem64 {
        flags |= 4
    };
    if f.custom_page_size {
        flags |= 8
    };
    leb(flags, &mut i);
    leb(f.initial as u64, &mut i);
    if let Some(x) = f.max {
        leb(x as u64, &mut i)
    };
    if f.custom_page_size {
        leb(0, &mut i)
    };
    if f.duplicate_memory {
        text("env", &mut i);
        text("memory", &mut i);
        i.push(2);
        leb(3, &mut i);
        leb(3, &mut i);
        leb(3, &mut i);
    };
    if f.alternate_memory {
        text("other", &mut i);
        text("memory", &mut i);
        i.push(2);
        leb(3, &mut i);
        leb(1, &mut i);
        leb(1, &mut i);
    };
    if f.ambient_wasi {
        text("wasi_snapshot_preview1", &mut i);
        text("fd_write", &mut i);
        i.push(0);
        leb(0, &mut i);
    };
    if f.thread {
        text("wasi", &mut i);
        text("thread-spawn", &mut i);
        i.push(0);
        leb(if f.wrong_thread_signature { 0 } else { 1 }, &mut i)
    };
    if f.yield_ {
        text("kernal-api:v1", &mut i);
        text(
            if f.legacy_yield_name {
                "kernel-yield"
            } else {
                "kernel_yield"
            },
            &mut i,
        );
        i.push(0);
        let non_function_index = 2 + u64::from(f.entry_param);
        leb(
            if f.non_function_yield {
                non_function_index
            } else if f.wrong_yield_signature {
                1
            } else {
                0
            },
            &mut i,
        )
    };
    section(2, i, &mut w);
    let mut fun = Vec::new();
    leb(1, &mut fun);
    let non_function_index = 2 + u64::from(f.entry_param);
    leb(
        if f.non_function_entry {
            non_function_index
        } else if f.entry_param {
            2
        } else {
            0
        },
        &mut fun,
    );
    section(3, fun, &mut w);
    if f.defined_memory {
        let mut m = Vec::new();
        leb(1, &mut m);
        leb(1, &mut m);
        leb(1, &mut m);
        leb(1, &mut m);
        section(5, m, &mut w);
    }
    let imports = u32::from(f.thread) + u32::from(f.yield_) + u32::from(f.ambient_wasi);
    let mut e = Vec::new();
    leb(if f.extra.is_some() { 2 } else { 1 }, &mut e);
    text("kernal-api-run", &mut e);
    e.push(0);
    leb(imports as u64, &mut e);
    if let Some(k) = f.extra {
        text("extra", &mut e);
        e.push(k);
        leb(0, &mut e)
    };
    section(7, e, &mut w);
    if f.start {
        section(8, vec![imports as u8], &mut w)
    };
    section(10, vec![1, 2, 0, 0x0b], &mut w);
    let mut metadata = generated_abi_metadata();
    if f.bad_meta {
        let declaration = b"abi_version = 1\n";
        let offset = metadata
            .windows(declaration.len())
            .position(|window| window == declaration)
            .expect("generated ABI version declaration");
        metadata[offset + declaration.len() - 2] = b'X';
    }
    custom("kernal-api.abi", &metadata, &mut w);
    if f.duplicate {
        custom("kernal-api.abi", &generated_abi_metadata(), &mut w)
    };
    if f.oversized_metadata {
        custom("kernal-api.profile", &[b'x'; 129], &mut w)
    };
    custom("kernal-api.profile", b"threaded-core-wasm-v1", &mut w);
    w
}

#[test]
fn valid_module_compiles_once() {
    let c = compiler();
    let a = c.admit(&wasm(F::default()), policy()).expect("admit");
    assert_eq!(c.compiled_module_count(), 1);
    assert_eq!(a.shared_memory().maximum_pages(), 16);
}
#[test]
fn rejected_module_never_compiles() {
    let c = compiler();
    let mut f = F::default();
    f.yield_ = false;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::MissingRequiredImport { .. })
    ));
    assert_eq!(c.compiled_module_count(), 0);
}
#[test]
fn handwritten_legacy_import_name_is_rejected_before_compilation() {
    let c = compiler();
    let mut f = F::default();
    f.legacy_yield_name = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::ForbiddenImport { module, name })
            if module == "kernal-api:v1" && name == "kernel-yield"
    ));
    assert_eq!(c.compiled_module_count(), 0);
}
#[test]
fn memory_forms_are_rejected_precompile() {
    let c = compiler();
    let mut f = F::default();
    f.shared = false;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::UnsharedMemory)
    ));
    let mut f = F::default();
    f.max = None;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::SharedMemoryWithoutMaximum)
    ));
    let mut f = F::default();
    f.mem64 = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::Memory64)
    ));
    let mut f = F::default();
    f.custom_page_size = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::UnsupportedMemoryPageSize)
    ));
    let mut f = F::default();
    f.initial = 17;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::MemoryInitialExceedsMaximum { .. })
    ));
    assert_eq!(c.compiled_module_count(), 0);
    let mut f = F::default();
    f.defined_memory = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::DefinedMemoryForbidden)
    ));
    assert_eq!(c.compiled_module_count(), 0);
}
#[test]
fn duplicate_and_alternate_memory_imports_are_rejected_precompile() {
    let c = compiler();
    let mut f = F::default();
    f.duplicate_memory = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::MultipleMemoryImports)
    ));
    assert_eq!(c.compiled_module_count(), 0);

    let mut f = F::default();
    f.alternate_memory = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::ForbiddenImport { module, name })
            if module == "other" && name == "memory"
    ));
    assert_eq!(c.compiled_module_count(), 0);

    let mut f = F::default();
    f.ambient_wasi = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::ForbiddenImport { module, name })
            if module == "wasi_snapshot_preview1" && name == "fd_write"
    ));
    assert_eq!(c.compiled_module_count(), 0);
}
#[test]
fn metadata_start_and_export_allowlist_are_strict() {
    let c = compiler();
    let mut f = F::default();
    f.start = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::StartFunctionForbidden)
    ));
    let mut f = F::default();
    f.duplicate = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::DuplicateMetadata { .. })
    ));
    let mut f = F::default();
    f.bad_meta = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::MetadataMismatch { .. })
    ));
    let mut f = F::default();
    f.oversized_metadata = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::MetadataTooLarge { .. })
    ));
    for k in [0, 1, 2, 3, 4] {
        let mut f = F::default();
        f.extra = Some(k);
        assert!(matches!(
            c.admit(&wasm(f), policy()),
            Err(SketchModuleError::ExportNotAllowed { .. })
        ));
    }
}
#[test]
fn malformed_oversized_and_abi_mismatches_have_stable_errors() {
    let c = compiler();
    assert!(matches!(c.admit(&[0,1,2],policy()),Err(error) if error.code()=="invalid-binary"));
    let mut f = F::default();
    f.entry_param = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::ImportTypeMismatch { .. })
    ));
    assert_eq!(c.compiled_module_count(), 0);
    let mut f = F::default();
    f.wrong_thread_signature = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::ImportTypeMismatch { .. })
    ));
    assert_eq!(c.compiled_module_count(), 0);
    let mut f = F::default();
    f.wrong_yield_signature = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::ImportTypeMismatch { .. })
    ));
    assert_eq!(c.compiled_module_count(), 0);
    let mut f = F::default();
    f.non_function_yield = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::ImportTypeMismatch { .. })
    ));
    assert_eq!(c.compiled_module_count(), 0);
    let mut f = F::default();
    f.non_function_entry = true;
    assert!(matches!(
        c.admit(&wasm(f), policy()),
        Err(SketchModuleError::EntrypointMismatch)
    ));
    assert_eq!(c.compiled_module_count(), 0);
    let p = SketchModulePolicy::new(4, 16).unwrap();
    assert!(matches!(
        c.admit(&wasm(F::default()), p),
        Err(SketchModuleError::ModuleTooLarge { .. })
    ));
}
