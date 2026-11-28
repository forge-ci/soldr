//! Public contract tests for generation-safe PID management.

use kernal_api::platform::process::{capture_identity, ProcessIdentityCapture};
use std::hash::{Hash, Hasher};

#[test]
fn current_process_has_a_facade_owned_opaque_identity() {
    let first = capture_identity(std::process::id());
    let second = capture_identity(std::process::id());
    let (ProcessIdentityCapture::Found(first), ProcessIdentityCapture::Found(second)) =
        (first, second)
    else {
        panic!("the current process must have a native creation identity");
    };
    assert_eq!(first, second);
    assert_eq!(first.pid(), std::process::id());
    let mut first_hash = std::collections::hash_map::DefaultHasher::new();
    let mut second_hash = std::collections::hash_map::DefaultHasher::new();
    first.hash(&mut first_hash);
    second.hash(&mut second_hash);
    assert_eq!(first_hash.finish(), second_hash.finish());
}

#[test]
fn absent_pid_is_never_reported_with_a_default_generation() {
    assert!(matches!(
        capture_identity(i32::MAX as u32),
        ProcessIdentityCapture::Exited | ProcessIdentityCapture::Error(_)
    ));
}
