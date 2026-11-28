//! The screenshot application lives here, inside the actual Wasm module.
//! No URL, native path, image bytes, host runtime, or platform API enters it.

use crossbeam_channel::{bounded, Receiver};
use dashmap::DashMap;
use kernal_api::guest::{self as kernel, OperationError, OutputFile, WebviewUrl};
use std::{
    collections::hash_map::DefaultHasher,
    hash::BuildHasherDefault,
    sync::{Arc, Condvar, Mutex},
};
#[allow(dead_code)]
#[path = "../../status.rs"]
mod status;
use status::{Cause, Failure, Step};

/// Exercise ordinary shared-state Rust before any host capability is granted.
///
/// The two workers share the module's Wasm memory, coordinate their startup
/// through a standard mutex/condition-variable pair, publish over bounded
/// Crossbeam channels, and mutate a DashMap concurrently. They are joined
/// before the screenshot flow begins, so this creates neither a guest executor
/// nor a background thread that can outlive root teardown.
fn threaded_policy_probe() -> Result<(), OperationError> {
    // DashMap's default random state imports ambient WASI entropy. The guest
    // contract permits only the generated capability imports, so this probe
    // uses a deterministic standard hasher instead.
    let values = Arc::new(DashMap::with_hasher(
        BuildHasherDefault::<DefaultHasher>::default(),
    ));
    // `(released, ready_workers)`: the root parks until both workers have
    // published their bounded messages, then wakes them for the map mutation.
    let gate = Arc::new((Mutex::new((false, 0_u8)), Condvar::new()));
    let (ready_tx, ready_rx) = bounded(2);
    let (done_tx, done_rx) = bounded(2);
    let mut workers = Vec::with_capacity(2);

    for value in 0_u32..2 {
        let values = Arc::clone(&values);
        let gate = Arc::clone(&gate);
        let ready_tx = ready_tx.clone();
        let done_tx = done_tx.clone();
        workers.push(std::thread::spawn(move || -> Result<(), OperationError> {
            let (lock, condition) = &*gate;
            let mut state = lock.lock().map_err(|_| OperationError::Failed)?;
            ready_tx
                .try_send(value)
                .map_err(|_| OperationError::Failed)?;
            state.1 += 1;
            condition.notify_all();
            while !state.0 {
                state = condition.wait(state).map_err(|_| OperationError::Failed)?;
            }
            drop(state);
            values.insert(value, value + 1);
            done_tx.try_send(value).map_err(|_| OperationError::Failed)
        }));
    }
    drop(ready_tx);
    drop(done_tx);

    {
        let (lock, condition) = &*gate;
        let mut state = lock.lock().map_err(|_| OperationError::Failed)?;
        while state.1 != 2 {
            state = condition.wait(state).map_err(|_| OperationError::Failed)?;
        }
        state.0 = true;
        condition.notify_all();
    }
    for worker in workers {
        worker.join().map_err(|_| OperationError::Failed)??;
    }
    receive_pair_without_ambient_clock(&ready_rx)?;
    receive_pair_without_ambient_clock(&done_rx)?;
    if values.len() != 2
        || values.get(&0).as_deref() != Some(&1)
        || values.get(&1).as_deref() != Some(&2)
    {
        return Err(OperationError::Failed);
    }
    Ok(())
}

/// Crossbeam's blocking receive uses WASI poll-oneoff. The closed guest ABI
/// intentionally omits that ambient import, so the root reads the two messages
/// only after their workers have joined and they must already be available.
fn receive_pair_without_ambient_clock(receiver: &Receiver<u32>) -> Result<(), OperationError> {
    let mut seen = 0_u8;
    for _ in 0..2 {
        match receiver.try_recv().map_err(|_| OperationError::Failed)? {
            0 => seen |= 0b01,
            1 => seen |= 0b10,
            _ => return Err(OperationError::Failed),
        }
    }
    if seen == 0b11 {
        Ok(())
    } else {
        Err(OperationError::Failed)
    }
}

async fn screenshot(step: &mut Step) -> Result<(), OperationError> {
    threaded_policy_probe()?;
    let url = WebviewUrl::granted()?.ok_or(OperationError::Rejected)?;
    *step = Step::OutputGrant;
    let output = OutputFile::granted()?.ok_or(OperationError::Rejected)?;
    *step = Step::Open;
    let view = url.open().await?;
    *step = Step::Load;
    view.wait_until_loaded().await?;
    // Submitted only after the matching top-level load completion. The host
    // monotonic timer owns the wait; there is no guest clock import.
    *step = Step::Sleep;
    kernel::sleep(5_000).await?;
    *step = Step::Capture;
    let snapshot = view.capture_visible_png().await?;
    // Acceptance-only fault in the real guest, after the native result has
    // crossed the generated ABI. No native replacement orchestration runs.
    if cfg!(feature = "proof-trap-after-capture") {
        std::arch::wasm32::unreachable();
    }
    if cfg!(feature = "proof-block-after-capture") {
        // The threaded Rust standard library parks on shared-memory atomics.
        // No producer can notify this condition: only process containment can
        // reclaim the live view and completed snapshot after this point.
        let mutex = Mutex::new(());
        let condition = Condvar::new();
        let mut guard = mutex.lock().unwrap();
        loop {
            guard = condition.wait(guard).unwrap();
        }
    }
    *step = Step::Write;
    output.write_blob(&snapshot).await?;
    // Successful exact-output commit consumes the snapshot and output grants.
    *step = Step::Close;
    view.close().await?;
    Ok(())
}

#[export_name = "kernal-api-run"]
pub extern "C" fn kernal_api_run() -> u32 {
    let mut step = Step::UrlGrant;
    match kernel::run(screenshot(&mut step)) {
        Ok(()) => 0,
        Err(error) => Failure {
            step,
            cause: match error {
                OperationError::Rejected => Cause::Rejected,
                OperationError::Cancelled => Cause::Cancelled,
                OperationError::Closed => Cause::Closed,
                OperationError::Failed => Cause::Failed,
                OperationError::TimedOut => Cause::TimedOut,
            },
        }
        .code(),
    }
}

fn main() {
    let status = kernal_api_run();
    if status != 0 {
        // The already-admitted command-exit boundary preserves this scalar;
        // actual Wasm traps remain distinct from semantic operation failures.
        std::process::exit(status as i32);
    }
}
