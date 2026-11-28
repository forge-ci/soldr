use kernal_api::async_engine::RuntimeBuilder;

// The runtime is deliberately built without drivers, and `run_within` needs
// the timer driver, so this keeps the deprecated unbounded bridge (#433).
#[allow(deprecated)]
#[test]
fn interrupt_registration_requires_enabled_drivers() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let error = runtime.interrupt_signal().err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    let error = runtime.run(runtime.wait_for_interrupt()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn native_interrupt_delivery_is_owned_and_cancellation_safe() {
    use std::time::{Duration, Instant};
    const CHILD: &str = "KERNAL_INTERRUPT_NOTIFICATION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        prepare_isolated_console();
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut first = runtime.interrupt_signal().unwrap();
        let mut second = runtime.interrupt_signal().unwrap();
        // Registration must precede the first wait and even the first run.
        send_interrupt_to_this_child();
        runtime
            .run_within(
                async {
                    kernal_api::async_engine::timeout(Duration::from_secs(2), first.wait())
                        .await
                        .unwrap()
                        .unwrap();
                    kernal_api::async_engine::timeout(Duration::from_secs(2), second.wait())
                        .await
                        .unwrap()
                        .unwrap();
                    // Poll and discard a pending wait: cancellation must not consume
                    // an interrupt that arrives only after that wait was dropped.
                    {
                        use std::future::Future;
                        let mut pending = std::pin::pin!(first.wait());
                        std::future::poll_fn(|context| {
                            assert!(pending.as_mut().poll(context).is_pending());
                            std::task::Poll::Ready(())
                        })
                        .await;
                    }
                    send_interrupt_to_this_child();
                    kernal_api::async_engine::timeout(Duration::from_secs(2), first.wait())
                        .await
                        .unwrap()
                        .unwrap();
                    kernal_api::async_engine::timeout(Duration::from_secs(2), second.wait())
                        .await
                        .unwrap()
                        .unwrap();
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        runtime
            .run_within(
                async {
                    use std::future::Future;
                    let mut lazy = std::pin::pin!(runtime.wait_for_interrupt());
                    std::future::poll_fn(|context| {
                        assert!(lazy.as_mut().poll(context).is_pending());
                        std::task::Poll::Ready(())
                    })
                    .await;
                    send_interrupt_to_this_child();
                    kernal_api::async_engine::timeout(Duration::from_secs(2), lazy)
                        .await
                        .unwrap()
                        .unwrap();
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "interrupt_notification::native_interrupt_delivery_is_owned_and_cancellation_safe",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "isolated signal fixture failed: {status}");
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("isolated signal fixture exceeded its deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn prepare_isolated_console() {}

#[cfg(unix)]
fn send_interrupt_to_this_child() {
    // SAFETY: target only this isolated fixture process; listeners are already
    // installed. Never send a signal to the test runner or a process group.
    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGINT) }, 0);
}

#[cfg(windows)]
fn prepare_isolated_console() {
    // SAFETY: this process is an isolated child; detaching cannot affect the
    // parent's attachment. A fresh console scopes generated events to us.
    unsafe { winapi::um::wincon::FreeConsole() };
    // SAFETY: no arguments; allocates a console owned by this child.
    assert_ne!(unsafe { winapi::um::consoleapi::AllocConsole() }, 0);
    // A process created with `CREATE_NEW_PROCESS_GROUP` starts with CTRL+C
    // ignored, and its children inherit that. nextest launches every test that
    // way on Windows so it can stop one, so under nextest this fixture's own
    // `CTRL_C_EVENT` was silently dropped and the first wait timed out; under
    // `cargo test` the flag was never set. A NULL handler with FALSE restores
    // normal CTRL+C processing, so the fixture no longer depends on how its
    // test runner launched it.
    // SAFETY: NULL handler with FALSE only clears this process's ignore flag;
    // it installs nothing and cannot affect the parent.
    assert_ne!(
        unsafe { winapi::um::consoleapi::SetConsoleCtrlHandler(None, 0) },
        0
    );
}

#[cfg(windows)]
fn send_interrupt_to_this_child() {
    // SAFETY: group zero broadcasts only within this child's fresh console.
    assert_ne!(
        unsafe {
            winapi::um::wincon::GenerateConsoleCtrlEvent(winapi::um::wincon::CTRL_C_EVENT, 0)
        },
        0
    );
}
