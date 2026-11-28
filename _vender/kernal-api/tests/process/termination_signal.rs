//! Ambient shutdown-signal subscriptions for launched `'static` work:
//! `async_engine::TerminationSignal` (SIGTERM / console break, close,
//! shutdown) and the free `async_engine::wait_for_interrupt` (Ctrl+C).

use kernal_api::async_engine::{self, RuntimeBuilder, TerminationSignal};
use std::future::Future;
use std::time::{Duration, Instant};

#[test]
fn ambient_subscriptions_require_an_entered_runtime() {
    let error = TerminationSignal::new().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    let error = poll_ready_once(async_engine::wait_for_interrupt()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

/// Drive a future that must complete on its first poll, without a runtime.
fn poll_ready_once<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(output) => output,
        std::task::Poll::Pending => panic!("expected an immediate result without a runtime"),
    }
}

#[test]
fn native_termination_and_interrupt_delivery_from_launched_work() {
    const CHILD: &str = "KERNAL_TERMINATION_SIGNAL_CHILD";
    if std::env::var_os(CHILD).is_some() {
        prepare_isolated_console();
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    // Registered before the signal is sent: must be delivered.
                    let mut termination = TerminationSignal::new().unwrap();
                    send_termination_to_this_child();
                    let delivered = async_engine::launch(async move {
                        let first = termination.recv().await;
                        (first, termination)
                    });
                    let (first, mut termination) =
                        async_engine::timeout(Duration::from_secs(5), delivered)
                            .await
                            .unwrap()
                            .unwrap();
                    assert_eq!(first, Some(()));

                    // A cancelled wait must not consume a later notification.
                    {
                        let mut pending = std::pin::pin!(termination.recv());
                        std::future::poll_fn(|context| {
                            assert!(pending.as_mut().poll(context).is_pending());
                            std::task::Poll::Ready(())
                        })
                        .await;
                    }
                    send_termination_to_this_child();
                    assert_eq!(
                        async_engine::timeout(Duration::from_secs(5), termination.recv())
                            .await
                            .unwrap(),
                        Some(())
                    );

                    // The ambient Ctrl+C wait registers on first poll.
                    let mut interrupt = Box::pin(async_engine::wait_for_interrupt());
                    std::future::poll_fn(|context| {
                        assert!(interrupt.as_mut().poll(context).is_pending());
                        std::task::Poll::Ready(())
                    })
                    .await;
                    send_interrupt_to_this_child();
                    async_engine::timeout(Duration::from_secs(5), interrupt)
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
            "termination_signal::native_termination_and_interrupt_delivery_from_launched_work",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
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
fn send_termination_to_this_child() {
    // SAFETY: target only this isolated fixture process; the listener is
    // already installed, so SIGTERM no longer takes its default action.
    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
}

#[cfg(unix)]
fn send_interrupt_to_this_child() {
    // SAFETY: target only this isolated fixture process; listener installed.
    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGINT) }, 0);
}

#[cfg(windows)]
fn prepare_isolated_console() {
    // SAFETY: this process is an isolated child; detaching cannot affect the
    // parent's attachment. A fresh console scopes generated events to us.
    unsafe { winapi::um::wincon::FreeConsole() };
    // SAFETY: no arguments; allocates a console owned by this child.
    assert_ne!(unsafe { winapi::um::consoleapi::AllocConsole() }, 0);
    // Clear an inherited CTRL+C ignore flag (nextest launches tests with
    // `CREATE_NEW_PROCESS_GROUP`); see `interrupt_notification`.
    // SAFETY: NULL handler with FALSE only clears this process's ignore flag.
    assert_ne!(
        unsafe { winapi::um::consoleapi::SetConsoleCtrlHandler(None, 0) },
        0
    );
}

#[cfg(windows)]
fn send_termination_to_this_child() {
    // SAFETY: group zero broadcasts only within this child's fresh console.
    assert_ne!(
        unsafe {
            winapi::um::wincon::GenerateConsoleCtrlEvent(winapi::um::wincon::CTRL_BREAK_EVENT, 0)
        },
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
