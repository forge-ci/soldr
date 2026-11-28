//! #189: the opt-in placement surface is facade-owned. `running-process` is a
//! private backend, so these contracts are exercised only through
//! `kernal_api` names; no test here names a substrate type.
#![cfg(feature = "independent-spawn")]

use std::io;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use kernal_api::{
    spawn_with_options, IndependentBackend, LaunchSpec, Readiness, SpawnExit, SpawnHandle,
    SpawnLifetime, SpawnMode, SpawnOptions,
};

fn spec() -> LaunchSpec {
    LaunchSpec {
        program: std::env::current_exe().unwrap().into_os_string(),
        args: Vec::new(),
        cwd: std::env::temp_dir().into_os_string(),
        environment: Vec::new(),
        stdout: None,
        stderr: None,
        readiness: Readiness::default(),
    }
}

fn rejection(options: SpawnOptions, cancelled: bool) -> io::ErrorKind {
    match spawn_with_options(&spec(), &options, &AtomicBool::new(cancelled)) {
        Ok(_) => panic!("{options:?} must be rejected before launch"),
        Err(error) => error.kind(),
    }
}

#[test]
fn defaults_require_no_external_authority() {
    assert_eq!(
        SpawnOptions::default(),
        SpawnOptions {
            mode: SpawnMode::Inherited,
            lifetime: SpawnLifetime::KillOnDrop,
            backend: None,
            timeout: Duration::from_secs(30),
        }
    );
    assert!(matches!(Readiness::default(), Readiness::ProcessStarted));
}

#[test]
fn invalid_requests_are_rejected_with_their_documented_kinds() {
    assert_eq!(
        rejection(SpawnOptions::default(), true),
        io::ErrorKind::Interrupted
    );
    for timeout in [Duration::ZERO, Duration::from_secs(31)] {
        let options = SpawnOptions {
            timeout,
            ..SpawnOptions::default()
        };
        assert_eq!(rejection(options, false), io::ErrorKind::InvalidInput);
    }
    let inherited_with_backend = SpawnOptions {
        backend: Some(IndependentBackend::ExternalBroker {
            endpoint: "broker".to_owned(),
        }),
        ..SpawnOptions::default()
    };
    assert_eq!(
        rejection(inherited_with_backend, false),
        io::ErrorKind::InvalidInput
    );
    let independent_without_authority = SpawnOptions {
        mode: SpawnMode::Independent,
        ..SpawnOptions::default()
    };
    assert_eq!(
        rejection(independent_without_authority, false),
        io::ErrorKind::Unsupported
    );
}

#[test]
fn handle_operations_use_facade_types() {
    let _: fn(&SpawnHandle) -> u32 = SpawnHandle::id;
    let _: fn(&SpawnHandle) -> SpawnMode = SpawnHandle::actual_mode;
    let _: fn(&mut SpawnHandle) -> io::Result<bool> = SpawnHandle::is_alive;
    let _: fn(&mut SpawnHandle, Duration) -> io::Result<()> = SpawnHandle::stop;
    let _: fn(&mut SpawnHandle, Duration, &AtomicBool) -> io::Result<SpawnExit> = SpawnHandle::wait;
}
