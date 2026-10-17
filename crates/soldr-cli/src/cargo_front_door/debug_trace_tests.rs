use super::*;

#[test]
fn disabled_values_are_recognized() {
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for value in ["0", "false", "no", "off", "", "  "] {
        let _guard = crate::EnvVarGuard::set(DEBUG_TRACE_ENV_VAR, value);
        assert!(!enabled(), "{value:?} must not enable tracing");
    }
    let _guard = crate::EnvVarGuard::remove(DEBUG_TRACE_ENV_VAR);
    assert!(!enabled(), "unset must not enable tracing");
}

#[test]
fn truthy_values_enable_tracing() {
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for value in ["1", "true", "on", "yes"] {
        let _guard = crate::EnvVarGuard::set(DEBUG_TRACE_ENV_VAR, value);
        assert!(enabled(), "{value:?} must enable tracing");
    }
}

#[test]
fn json_strings_escape_quotes_backslashes_and_control_characters() {
    assert_eq!(json_string("plain"), r#""plain""#);
    assert_eq!(json_string(r#"a"b"#), r#""a\"b""#);
    assert_eq!(json_string(r"C:\cargo"), r#""C:\\cargo""#);
    assert_eq!(json_string("a\nb\tc"), r#""a\nb\tc""#);
    assert_eq!(json_string("\u{1}"), r#""\u0001""#);
}

#[test]
fn rendered_argv_joins_program_and_args() {
    let mut command = std::process::Command::new("cargo");
    command.args(["build", "--target", "x86_64-pc-windows-gnu"]);
    assert_eq!(
        render_argv(&command),
        "cargo build --target x86_64-pc-windows-gnu"
    );
}

/// soldr#2546 acceptance: with the flag absent, spawning through the traced
/// path performs no tracing work — it is exactly `Command::spawn`.
#[test]
fn spawn_traced_without_the_flag_only_spawns() {
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = crate::EnvVarGuard::remove(DEBUG_TRACE_ENV_VAR);
    let mut command =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "exit 0"]);
            command
        } else {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "exit 0"]);
            command
        };
    let mut child = spawn_traced(&mut command, "test noop").expect("spawn");
    let status = child.wait().expect("wait");
    assert!(status.success());
}

/// soldr#2658 item 2 (soldr#2546 acceptance): non-Unicode argv must
/// round-trip byte-for-byte through the observed spawn. The
/// running-process#1023 seam carries the real `std::process::Command`, so
/// no String conversion sits between soldr and exec — this proves it end
/// to end with an invalid-UTF-8 argument on Unix.
#[test]
fn observed_spawn_round_trips_non_unicode_argv() {
    if matches!(
        crate::platform::host::facts::os(),
        crate::platform::host::facts::HostOs::Windows
    ) {
        // Windows argv is UTF-16 at the OS boundary; arbitrary-byte argv
        // is a Unix concern.
        return;
    }
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = crate::EnvVarGuard::set(DEBUG_TRACE_ENV_VAR, "1");

    let dir = std::env::temp_dir().join(format!(
        "non-unicode-argv-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let script = dir.join("echo-args.sh");
    let out_path = dir.join("argv.bin");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > \"{}\"\n",
            out_path.display()
        ),
    )
    .expect("write script");
    crate::platform::fs::permissions::make_executable(&script).expect("chmod script");

    // 0xFF can never appear in well-formed UTF-8. SAFETY: on Unix the
    // OS-string encoding is arbitrary bytes, so any byte sequence is a
    // valid encoded OS string; the Windows early-return above keeps this
    // construction off the platform whose encoding (WTF-8) it could
    // violate. Spelled through the encoding-neutral constructor so no
    // `std::os::unix` path appears outside the platform crate
    // (platform-cfg boundary ratchet).
    let raw = unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(b"--cfg=weird\xFFbytes") };
    let mut command = std::process::Command::new(&script);
    command.arg(raw);
    let status = run_observed_inheriting_stdio(
        &mut command,
        "non-unicode fixture",
        Some(std::time::Duration::from_secs(30)),
        std::time::Duration::from_secs(10),
        None,
    )
    .expect("observed spawn must run the fixture");
    assert!(status.success(), "fixture exited nonzero");

    let echoed = std::fs::read(&out_path).expect("fixture argv capture");
    assert_eq!(
        echoed, b"--cfg=weird\xffbytes",
        "argv bytes must survive the observed spawn unchanged"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// soldr#2658 item 2, environment half: the argv test above proves
/// arguments survive the observed spawn; the acceptance item is
/// "argv/env", and the environment travels a different `Command` setter.
/// `Command::env` takes `AsRef<OsStr>` for both halves, so a non-UTF-8
/// value has no more reason to be lossy than a non-UTF-8 argument -- but
/// "no reason to break" is what a proof is for, and this is the pair that
/// a `String`-typed convenience wrapper would silently mangle first.
#[test]
fn observed_spawn_round_trips_non_unicode_env() {
    if matches!(
        crate::platform::host::facts::os(),
        crate::platform::host::facts::HostOs::Windows
    ) {
        // Windows environment blocks are UTF-16 at the OS boundary;
        // arbitrary-byte values are a Unix concern. Same split as the
        // argv test above.
        return;
    }
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = crate::EnvVarGuard::set(DEBUG_TRACE_ENV_VAR, "1");

    let dir = std::env::temp_dir().join(format!(
        "non-unicode-env-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let script = dir.join("echo-env.sh");
    let out_path = dir.join("env.bin");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s' \"$SOLDR_NON_UNICODE_FIXTURE\" > \"{}\"\n",
            out_path.display()
        ),
    )
    .expect("write script");
    crate::platform::fs::permissions::make_executable(&script).expect("chmod script");

    // Same construction and the same safety argument as the argv test:
    // 0xFF cannot appear in well-formed UTF-8, on Unix an OS string is
    // arbitrary bytes, and the Windows early-return keeps this off the
    // platform whose WTF-8 encoding it would violate. The
    // encoding-neutral constructor keeps `std::os::unix` out of this
    // crate (platform-cfg boundary ratchet).
    let raw = unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(b"weird\xFFvalue") };
    let mut command = std::process::Command::new(&script);
    command.env("SOLDR_NON_UNICODE_FIXTURE", raw);
    let status = run_observed_inheriting_stdio(
        &mut command,
        "non-unicode env fixture",
        Some(std::time::Duration::from_secs(30)),
        std::time::Duration::from_secs(10),
        None,
    )
    .expect("observed spawn must run the fixture");
    assert!(status.success(), "fixture exited nonzero");

    let echoed = std::fs::read(&out_path).expect("fixture env capture");
    assert_eq!(
        echoed, b"weird\xffvalue",
        "environment bytes must survive the observed spawn unchanged"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
