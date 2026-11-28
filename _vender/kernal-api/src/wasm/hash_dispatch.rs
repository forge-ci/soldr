//! Hash operation dispatch over validated, atomically accessed shared memory.

use super::*;
use crate::operations::{OpaqueToken, OperationHub};

pub(super) fn dispatch(
    hub: &OperationHub,
    store: u64,
    memory: &SharedMemory,
    kind: u32,
    arg0: u64,
    arg1: u64,
) -> Option<u64> {
    match kind {
        30 => Some(if arg0 == 0 && arg1 == 0 {
            hub.submit_hash_create(store)
                .map(OpaqueToken::wire)
                .unwrap_or(0)
        } else {
            0
        }),
        31 => {
            let length = (arg1 >> 32) as usize;
            if length > 64 * 1024 {
                return Some(0);
            }
            let Some(cells) = shared_range(memory, arg1 as u32 as i32, length) else {
                return Some(0);
            };
            // A bounded snapshot: no guest-memory borrow survives this import.
            let bytes: Vec<u8> = cells
                .iter()
                .map(|cell| {
                    // SAFETY: shared_range validates pinned bytes; atomic loads
                    // remain defined even if another guest thread changes them.
                    unsafe { AtomicU8::from_ptr(cell.get()) }.load(Ordering::Relaxed)
                })
                .collect();
            Some(
                hub.submit_hash_update(store, OpaqueToken::from_wire(arg0), &bytes)
                    .map(OpaqueToken::wire)
                    .unwrap_or(0),
            )
        }
        32 => {
            if arg1 >> 32 != 32 {
                return Some(0x80);
            }
            let Some(cells) = shared_range(memory, arg1 as u32 as i32, 32) else {
                return Some(0x80);
            };
            let mut digest = [0; 32];
            if hub
                .finish_hash_into(store, OpaqueToken::from_wire(arg0), &mut digest)
                .is_err()
            {
                return Some(0x80);
            }
            for (cell, byte) in cells.iter().zip(digest) {
                // SAFETY: destination was validated before consuming authority;
                // shared-memory stores are atomic and cannot invalidate the range.
                unsafe { AtomicU8::from_ptr(cell.get()) }.store(byte, Ordering::Relaxed);
            }
            Some(1)
        }
        33 => Some(u64::from(
            arg1 == 0
                && hub
                    .abandon_hash(store, OpaqueToken::from_wire(arg0))
                    .is_ok(),
        )),
        34 => Some(u64::from(
            arg1 == 0
                && hub
                    .abandon_hash_operation(store, OpaqueToken::from_wire(arg0))
                    .is_ok(),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;

    #[test]
    #[ignore = "requires freshly built KERNAL_HASH_GUEST_WASM"]
    fn hash_actual_guest_streams_64_mib_through_public_facade() {
        let mut native = crate::hash::Blake3Hasher::new();
        let chunk = [0x5a; 65536];
        for _ in 0..1024 {
            native.update(&chunk);
        }
        assert_eq!(
            native.finalize().to_hex(),
            "357071d554b85545e7abc64c4d5bfe685c0aa5f71327e411baf782a8aeed102c"
        );
        let bytes =
            std::fs::read(std::env::var_os("KERNAL_HASH_GUEST_WASM").expect("hash guest artifact"))
                .unwrap();
        let execution = SketchExecutionLimits::default()
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
                        RootGrants::default(),
                    ),
                    std::time::Duration::from_secs(60)
                )
                .expect("test runtime budget"),
            Ok(ThreadedRootOutcome::Started)
        );
        let snapshot = sketch
            .root_execution_observation_for_test()
            .unwrap()
            .operation_snapshot
            .unwrap();
        assert_eq!(snapshot.live_resources, 0);
        assert_eq!(snapshot.pending_operations, 0);
        sketch.close_threaded_root().unwrap();
        assert_eq!(
            compiler.execution_limits_snapshot(),
            SketchExecutionSnapshot::default()
        );
    }

    #[test]
    fn hash_wire_updates_real_shared_bytes_and_rejects_reserved_arguments() {
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).unwrap();
        let memory = SharedMemory::new(&compiler.engine, MemoryType::shared(1, 1)).unwrap();
        let hub = crate::operations::OperationHub::new(1, 1).unwrap();
        assert_eq!(super::dispatch(&hub, 1, &memory, 30, 1, 0), Some(0));
        assert_eq!(super::dispatch(&hub, 1, &memory, 30, 0, 1), Some(0));
        assert_eq!(super::dispatch(&hub, 1, &memory, 999, 0, 0), None);
        let create = super::dispatch(&hub, 1, &memory, 30, 0, 0).unwrap();
        let hash = hub.poll_wire(1, create) >> 8;
        for (cell, byte) in memory.data()[..3].iter().zip(b"abc") {
            // SAFETY: pinned shared bytes are accessed atomically.
            unsafe { AtomicU8::from_ptr(cell.get()) }.store(*byte, Ordering::Relaxed);
        }
        let update = super::dispatch(&hub, 1, &memory, 31, hash, 3_u64 << 32).unwrap();
        assert_ne!(update, 0);
        assert_eq!(hub.poll_wire(1, update), 1);
        assert_eq!(
            super::dispatch(&hub, 2, &memory, 32, hash, (32_u64 << 32) | 16),
            Some(0x80)
        );
        assert_eq!(super::dispatch(&hub, 1, &memory, 33, hash, 1), Some(0));
        assert_eq!(
            super::dispatch(&hub, 1, &memory, 32, hash, (32_u64 << 32) | 16),
            Some(1)
        );
        let digest: Vec<_> = memory.data()[16..48]
            .iter()
            .map(|cell| {
                // SAFETY: pinned shared bytes are accessed atomically.
                unsafe { AtomicU8::from_ptr(cell.get()) }.load(Ordering::Relaxed)
            })
            .collect();
        assert_eq!(digest, crate::hash::blake3_bytes(b"abc").as_bytes());
    }

    #[test]
    fn hash_wire_invalid_memory_preserves_retry_authority() {
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).unwrap();
        let memory = SharedMemory::new(&compiler.engine, MemoryType::shared(1, 1)).unwrap();
        let hub = crate::operations::OperationHub::new(2, 1).unwrap();
        let operation = super::dispatch(&hub, 1, &memory, 30, 0, 0).unwrap();
        let packed = hub.poll_wire(1, operation);
        assert_eq!(packed as u8, 1);
        let hash = packed >> 8;
        // Digest output crosses the end of memory; no hash may be consumed.
        assert_eq!(
            super::dispatch(&hub, 1, &memory, 32, hash, (32_u64 << 32) | 65535),
            Some(0x80)
        );
        assert_eq!(hub.snapshot().live_resources, 1);
        // Oversized input is rejected before accessing or copying shared bytes.
        assert_eq!(
            super::dispatch(&hub, 1, &memory, 31, hash, 65537_u64 << 32),
            Some(0)
        );
        assert_eq!(
            super::dispatch(&hub, 1, &memory, 32, hash, 32_u64 << 32),
            Some(1)
        );
        let bytes: Vec<_> = memory.data()[..32]
            .iter()
            .map(|cell| {
                // SAFETY: in-bounds pinned shared bytes are accessed atomically.
                unsafe { AtomicU8::from_ptr(cell.get()) }.load(Ordering::Relaxed)
            })
            .collect();
        assert_eq!(bytes, crate::hash::blake3_bytes(b"").as_bytes());
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(
            super::dispatch(&hub, 1, &memory, 32, hash, 32_u64 << 32),
            Some(0x80)
        );
    }
}
