//! Private revision-8 compiler operations over the existing scalar imports.
use super::*;
use crate::operations::OpaqueToken;

pub(super) fn dispatch(
    state: &mut ThreadStoreState,
    kind: u32,
    arg0: u64,
    arg1: u64,
) -> Option<u64> {
    CompilerImports {
        hub: &state.operations,
        store: state.store_owner,
        runtime: state.runtime.as_ref(),
        memory: &state.controller.memory,
        grant: &mut state.initial_compiler,
    }
    .submit(kind, arg0, arg1)
}

struct CompilerImports<'a> {
    hub: &'a Arc<OperationHub>,
    store: u64,
    runtime: Option<&'a crate::async_engine::RuntimeHandle>,
    memory: &'a SharedMemory,
    grant: &'a mut Option<u64>,
}

impl CompilerImports<'_> {
    fn submit(&mut self, kind: u32, arg0: u64, arg1: u64) -> Option<u64> {
        if !(35..=48).contains(&kind) {
            return None;
        }
        // Validate reserved arguments before taking grants or other authority.
        if kind != 38 && kind != 48 && arg1 != 0 {
            return Some(0);
        }
        let hub = self.hub;
        let store = self.store;
        let token = OpaqueToken::from_wire(arg0);
        Some(match kind {
            35 if arg0 == 0 => self.grant.take().unwrap_or(0),
            36 | 37 | 43 | 45 => {
                let Some(runtime) = self.runtime.cloned() else {
                    return Some(0);
                };
                let operation = match kind {
                    36 => hub.submit_compiler_spawn(runtime, store, token),
                    37 => hub.submit_compiler_output(runtime, store, token),
                    43 => hub.submit_compiler_wait(runtime, store, token),
                    _ => hub.submit_compiler_close(runtime, store, token),
                };
                operation.map(OpaqueToken::wire).unwrap_or(0)
            }
            38 => {
                let capacity = (arg1 >> 32) as usize;
                if capacity < 65536 {
                    return Some(0x80);
                }
                let Some(cells) = shared_range(self.memory, arg1 as u32 as i32, 65536) else {
                    return Some(0x80);
                };
                hub.collect_compiler_output_wire(store, token, capacity, |event| {
                    use crate::{
                        ProcessOutputChunk as Chunk, ProcessOutputCompletion as Completion,
                        ProcessOutputEvent as Event,
                    };
                    let (tag, bytes): (u64, &[u8]) = match event {
                        Some(Event::Chunk(Chunk::Stdout(bytes))) => (1, bytes),
                        Some(Event::Chunk(Chunk::Stderr(bytes))) => (2, bytes),
                        Some(Event::Completion(completion)) => (
                            match completion {
                                Completion::StdoutEof => 3,
                                Completion::StderrEof => 4,
                                Completion::StdoutAbandoned => 5,
                                Completion::StderrAbandoned => 6,
                                Completion::StdoutError(_) => 7,
                                Completion::StderrError(_) => 8,
                            },
                            &[],
                        ),
                        None => (9, &[]),
                    };
                    for (cell, byte) in cells.iter().zip(bytes) {
                        // SAFETY: shared_range validated pinned memory before
                        // collection; atomic stores tolerate other guest threads.
                        unsafe { AtomicU8::from_ptr(cell.get()) }.store(*byte, Ordering::Relaxed);
                    }
                    ((bytes.len() as u64) << 8) | tag
                })
                .unwrap_or(0x80)
            }
            39 => u64::from(hub.abandon_compiler_output_wire(store, token).is_ok()),
            40 => u64::from(hub.abandon_compiler_resource(store, token, false).is_ok()),
            41 => u64::from(hub.abandon_compiler_resource(store, token, true).is_ok()),
            42 => u64::from(hub.abandon_compiler_spawn(store, token).is_ok()),
            44 => hub.collect_compiler_wait(store, token).unwrap_or(0x80),
            46 => u64::from(hub.abandon_compiler_scalar(store, token, true).is_ok()),
            47 => u64::from(hub.abandon_compiler_scalar(store, token, false).is_ok()),
            48 => {
                // The Core ABI pointer is a 32-bit wasm address. Do not let a
                // wider host-side value alias a valid low address by truncation.
                if arg1 >> 32 != 0 {
                    return Some(0);
                }
                let Some(cells) = shared_range(self.memory, arg1 as u32 as i32, 32) else {
                    return Some(0);
                };
                let mut key = [0; 32];
                for (destination, cell) in key.iter_mut().zip(cells) {
                    // SAFETY: shared_range validated the pinned 32-byte key.
                    *destination =
                        unsafe { AtomicU8::from_ptr(cell.get()) }.load(Ordering::Relaxed);
                }
                match hub.compiler_cache_status(store, token, key) {
                    Ok(true) => 1,
                    Ok(false) => 2,
                    Err(_) => 0,
                }
            }
            _ => 0,
        })
    }
}

#[cfg(test)]
pub(in crate::wasm) mod tests {
    use super::*;

    pub(in crate::wasm) const CACHE_KEY: [u8; 32] = [
        0xdb, 0xed, 0xcb, 0xc5, 0x83, 0xf5, 0x1d, 0x14, 0x3b, 0xae, 0xb1, 0x9d, 0xbe, 0xac, 0xfd,
        0x3f, 0xa0, 0xcb, 0x40, 0x8c, 0xe8, 0x39, 0x9b, 0x56, 0xce, 0xc7, 0x22, 0x24, 0xd8, 0x54,
        0xe9, 0x93,
    ];

    pub(in crate::wasm) fn compiler_helper_spec() -> crate::SpawnSpec {
        let spec = crate::SpawnSpec::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("wasm::compiler_dispatch::tests::compiler_guest_native_helper")
            .arg("--nocapture")
            .current_dir(std::env::current_dir().unwrap())
            .clear_env(true)
            .env("KERNAL_COMPILER_WIRE_HELPER", "dual");
        // The Windows process loader and Rust test harness require these
        // host-selected system variables and Cargo's DLL loader search path
        // even for an otherwise empty fixture environment.
        #[cfg(windows)]
        let spec = [
            "APPDATA",
            "COMSPEC",
            "HOMEDRIVE",
            "HOMEPATH",
            "LOCALAPPDATA",
            "PATH",
            "PATHEXT",
            "SYSTEMDRIVE",
            "SYSTEMROOT",
            "TEMP",
            "TMP",
            "USERPROFILE",
            "WINDIR",
        ]
        .into_iter()
        .fold(spec, |spec, key| match std::env::var_os(key) {
            Some(value) => spec.env(key, value),
            None => spec,
        });
        spec
    }

    #[test]
    fn compiler_imports_validate_grant_arguments_and_output_ranges_before_consumption() {
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).unwrap();
        let memory = SharedMemory::new(&compiler.engine, MemoryType::shared(1, 1)).unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    let hub = OperationHub::new(4, 2).unwrap();
                    let spec = compiler_helper_spec();
                    let grant = hub
                        .grant_compiler_with_cache(
                            7,
                            spec,
                            Duration::from_secs(15),
                            Some((CACHE_KEY, false)),
                        )
                        .unwrap()
                        .wire();
                    let mut initial = Some(grant);
                    let handle = runtime.handle();
                    let mut imports = CompilerImports {
                        hub: &hub,
                        store: 7,
                        runtime: Some(&handle),
                        memory: &memory,
                        grant: &mut initial,
                    };
                    assert_eq!(imports.submit(35, 1, 0), Some(0));
                    assert_eq!(imports.submit(35, 0, 1), Some(0));
                    assert_eq!(imports.submit(35, 0, 0), Some(grant));
                    assert_eq!(imports.submit(35, 0, 0), Some(0));
                    assert_eq!(imports.submit(48, grant, u64::from(u32::MAX)), Some(0));
                    for (cell, byte) in memory.data().iter().zip(CACHE_KEY) {
                        // SAFETY: this test owns the single shared-memory fixture.
                        unsafe { AtomicU8::from_ptr(cell.get()) }.store(byte, Ordering::Relaxed);
                    }
                    // A 64-bit host value must not alias the valid Wasm address zero.
                    assert_eq!(imports.submit(48, grant, 1_u64 << 32), Some(0));
                    assert_eq!(imports.submit(48, grant, 0), Some(2));
                    assert_eq!(imports.submit(36, grant, 1), Some(0));
                    let spawn = imports.submit(36, grant, 0).unwrap();
                    let process = crate::async_engine::timeout(Duration::from_secs(5), async {
                        loop {
                            let result = hub.poll_wire(7, spawn);
                            if result != 0 {
                                assert_eq!(result as u8, 1);
                                break result >> 8;
                            }
                            crate::async_engine::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    let mut read = imports.submit(37, process, 0).unwrap();
                    hub.suspend_wire(7, read).unwrap().notified().await;
                    assert_eq!(imports.submit(38, read, 65535_u64 << 32), Some(0x80));
                    assert_eq!(imports.submit(38, read, (65536_u64 << 32) | 1), Some(0x80));
                    assert_eq!(
                        imports.submit(38, read, (65536_u64 << 32) | u64::from(u32::MAX)),
                        Some(0x80)
                    );
                    imports.store = 8;
                    assert_eq!(imports.submit(38, read, 65536_u64 << 32), Some(0x80));
                    imports.store = 7;
                    assert_eq!(hub.poll_wire(7, read), 0x80);
                    // stdout and stderr EOF events may race their buffered chunks.
                    // The transfer proof requires a copied chunk, so consume only a
                    // small, fixed number of normal EOF events first.
                    let mut chunk = None;
                    for _ in 0..8 {
                        let result = imports.submit(38, read, 65536_u64 << 32).unwrap();
                        // The outer wire adds terminal status below the event payload.
                        // The event tag therefore occupies bits 8..16 and the copied
                        // byte count begins at bit 16.
                        assert_eq!(result as u8, 1);
                        match (result >> 8) as u8 {
                            1 | 2 => {
                                chunk = Some(result);
                                break;
                            }
                            3 | 4 => {
                                read = imports.submit(37, process, 0).unwrap();
                                hub.suspend_wire(7, read).unwrap().notified().await;
                            }
                            tag => panic!("unexpected packed output tag {tag}: {result:#x}"),
                        }
                    }
                    let result = chunk.expect("fixture produced no output chunk");
                    assert!((1..=65536).contains(&(result >> 16)));
                    // SAFETY: read the same pinned shared cell atomically.
                    assert_ne!(
                        unsafe { AtomicU8::from_ptr(memory.data()[0].get()) }
                            .load(Ordering::Relaxed),
                        0
                    );
                    assert_eq!(imports.submit(39, read, 0), Some(0));
                    assert_eq!(imports.submit(40, process, 1), Some(0));
                    assert_eq!(imports.submit(40, process, 0), Some(1));
                    hub.close_all(crate::operations::Terminal::Closed);
                    hub.join_process_jobs().await.unwrap();
                    assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
                    assert_eq!(hub.snapshot().retained_process_jobs, 0);
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    fn compiler_guest_native_helper() {
        if std::env::var("KERNAL_COMPILER_WIRE_HELPER").as_deref() != Ok("dual") {
            return;
        }
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        let mut stderr = std::io::stderr().lock();
        for _ in 0..512 {
            stdout.write_all(&[0xf1; 4096]).unwrap();
            stderr.write_all(&[0xf2; 4096]).unwrap();
        }
    }

    #[test]
    #[ignore = "requires freshly built KERNAL_COMPILER_GUEST_WASM revision-8 guest"]
    fn compiler_actual_guest_spawns_drains_hashes_persists_and_waits() {
        let output = tempfile::tempdir().unwrap();
        let artifact = output.path().join("compiler-artifact");
        execute_actual_guest(artifact.clone(), compiler_helper_spec());
        assert_verified_artifact(&artifact);
    }

    fn execute_actual_guest(output: std::path::PathBuf, spec: crate::SpawnSpec) {
        let bytes = std::fs::read(
            std::env::var_os("KERNAL_COMPILER_GUEST_WASM").expect("compiler guest artifact"),
        )
        .unwrap();
        // The miss persists two verified 2 MiB streams as one exact artifact.
        // This fixture's larger storage and transfer bounds are explicit; the
        // ordinary sketch defaults remain unchanged.
        let blobs =
            SketchBlobLimits::new(64 * 1024, 4 * 1024 * 1024, 8 * 1024 * 1024, 128, 128, 128)
                .unwrap()
                .with_maximum_transfer_bytes(24 * 1024 * 1024)
                .unwrap();
        let execution = SketchExecutionLimits::default()
            .with_blob_limits(blobs)
            .with_fuel_limits(SketchFuelLimits::new(501_600_000, 500_000_000, 100_000).unwrap())
            .unwrap();
        let compiler = SketchCompiler::new(
            SketchCompilerConfig::default()
                .with_execution_limits(execution)
                .unwrap(),
        )
        .unwrap();
        let policy =
            SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES).unwrap();
        let sketch = compiler.admit(&bytes, policy).unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(
            runtime
                .run_within(
                    sketch.execute_threaded_root_with_grant(
                        runtime.handle(),
                        crate::async_engine::CancellationSource::new().token(),
                        RootGrants {
                            compiler: Some(RootCompilerGrant {
                                spec,
                                deadline: Duration::from_secs(15),
                            }),
                            output: Some(output.clone()),
                            #[cfg(all(test, feature = "archive-auth-test-support"))]
                            archive: None,
                            #[cfg(feature = "tauri-webview")]
                            webview: None,
                        },
                    ),
                    std::time::Duration::from_secs(60)
                )
                .expect("test runtime budget"),
            Ok(ThreadedRootOutcome::Started)
        );
        assert_verified_artifact(&output);
        let snapshot = sketch
            .root_execution_observation_for_test()
            .unwrap()
            .operation_snapshot
            .unwrap();
        assert_eq!(snapshot.live_resources, 0);
        assert_eq!(snapshot.pending_operations, 0);
        assert_eq!(snapshot.retained_process_jobs, 0);
        assert_eq!(snapshot.retained_transfer_capacity, 0);
        sketch.close_threaded_root().unwrap();
        assert_eq!(
            compiler.execution_limits_snapshot(),
            SketchExecutionSnapshot::default()
        );
    }

    fn assert_verified_artifact(output: &std::path::Path) {
        let bytes = std::fs::read(output).unwrap();
        assert_eq!(bytes.len(), 4 * 1024 * 1024);
        assert_eq!(
            bytes.iter().filter(|byte| **byte == 0xf1).count(),
            2 * 1024 * 1024
        );
        assert_eq!(
            bytes.iter().filter(|byte| **byte == 0xf2).count(),
            2 * 1024 * 1024
        );
    }
}
