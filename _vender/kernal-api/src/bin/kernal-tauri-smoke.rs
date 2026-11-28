//! Native semantic-webview lifecycle proof for the `tauri-webview` facade.
//!
//! Run on Linux with the GTK/WebKit/Xvfb shell documented in issue #18.  This
//! binary exists because a Rust unit test executes on a test-worker thread,
//! while Tauri requires the event loop to be initialized by the process main
//! thread on every supported desktop host.

#[path = "kernal-tauri-smoke/widget.rs"]
mod widget;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use kernal_api::async_engine;
use kernal_api::webview::{
    ExternalWebviewClient, ExternalWebviewHost, WebviewError, WebviewPageBootstrap,
    WebviewPermissions, WebviewUrlGrant, WebviewWindowOptions,
};
use widget::{widget_controls, widget_options, NAVIGATED_QUERY, WIDGET_APP_ID};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let scenario = SmokeScenario::from_args()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let page = scenario.page(&address.to_string());
    let server = std::thread::spawn(move || -> std::io::Result<()> {
        listener.set_nonblocking(true)?;
        // Hosted Linux can spend tens of seconds initializing WebKitGTK on a
        // cold Xvfb process. Keep the proof bounded without mistaking that
        // startup variance for a navigation failure.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let accept = || loop {
            match listener.accept() {
                Ok(connection) => return Ok(connection),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "webview did not complete loopback isolation proof",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        };
        if scenario == SmokeScenario::Bootstrap {
            return bootstrap_server(&accept, deadline, address.port());
        }
        if scenario == SmokeScenario::Timeout {
            let (mut stream, _) = accept().map_err(socket_stage("accept document connection"))?;
            // Accepted sockets inherit the listener's nonblocking mode on the
            // supported Unix CI hosts.  The proof uses a bounded blocking read so
            // an in-flight navigation cannot race the harness into WouldBlock.
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut request = [0_u8; 4096];
            match stream.read(&mut request) {
                Ok(_) => {}
                // A real WebKit navigation may establish its loopback connection
                // just as the semantic timeout tears down the native backing,
                // before it writes HTTP bytes. That is a successful timeout
                // proof, not a server failure.
                Err(error)
                    if scenario == SmokeScenario::Timeout
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) =>
                {
                    return Ok(());
                }
                Err(error) => return Err(socket_stage("read document request")(error)),
            }
            std::thread::sleep(Duration::from_secs(1));
            return Ok(());
        }
        serve_isolated_page(&accept, deadline, "/finished", &page)?;
        if scenario == SmokeScenario::Widget {
            // The widget then navigates the same view in place; the new page
            // must report the same absence of host bridges.
            serve_isolated_page(
                &accept,
                deadline,
                &format!("/finished{NAVIGATED_QUERY}"),
                &page,
            )?;
        }
        Ok(())
    });

    let runtime = async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()?;
    let host = if scenario == SmokeScenario::Widget {
        ExternalWebviewHost::with_app_id(runtime.handle(), WIDGET_APP_ID)
    } else {
        ExternalWebviewHost::new(runtime.handle())
    }
    .map_err(host_error)?;
    let client = host.client();
    let url = format!("http://{address}/finished");
    let (result_sender, result_receiver) = mpsc::channel();
    let task_client = client.clone();
    runtime
        .handle()
        .launch(async move {
            // Keep the whole operation bounded.  A load callback that never
            // arrives must not leave the Wry event loop running forever in CI
            // or in an embedding shell.  Dropping `lifecycle` drops a created
            // NativeWebview, whose Drop requests native close.
            let result = async_engine::timeout(
                Duration::from_secs(60),
                lifecycle(&task_client, &url, scenario),
            )
            .await
            .map_err(|_| WebviewError::TimedOut)
            .and_then(|result| result);
            let _ = result_sender.send(result);
            let _ = task_client.request_exit();
        })
        .detach();

    // This process main thread owns the Wry/Tauri event loop. The awaited
    // operation above uses the same caller-provided kernal-api runtime.
    host.run();
    // A failed lifecycle leaves the server waiting for a page that never
    // comes; report the lifecycle's own error rather than that consequence.
    let lifecycle = result_receiver
        .recv_timeout(Duration::from_secs(60))
        .map_err(|_| "webview lifecycle task did not exit")?;
    lifecycle.map_err(host_error)?;
    server.join().map_err(|_| "loopback server panicked")??;
    Ok(())
}

#[expect(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
async fn lifecycle(
    client: &ExternalWebviewClient,
    url: &str,
    scenario: SmokeScenario,
) -> Result<(), WebviewError> {
    if scenario == SmokeScenario::Bootstrap {
        let window = WebviewWindowOptions::new("kernal-api bootstrap proof", 800, 600)
            .map_err(|error| WebviewError::HostFailure(error.to_string()))?;
        let bootstrap = WebviewPageBootstrap::new(
            "if (Object.isFrozen(kernalWindow) && Number.isFinite(kernalWindow.initialScaleFactor) && kernalWindow.initialScaleFactor > 0) { window.__kernal_bootstrap = 17; }",
        )
            .map_err(|error| WebviewError::HostFailure(error.to_string()))?;
        let outcome = match client
            .open_webview_with_bootstrap(url, window, WebviewPermissions::deny_all(), bootstrap)
            .await
        {
            Ok(webview) => webview.wait_until_terminal(Duration::from_secs(30)).await,
            Err(error) => Err(error),
        };
        return match outcome {
            Err(WebviewError::RejectedNavigation(_)) => assert_clean(client),
            other => Err(WebviewError::HostFailure(format!(
                "bootstrap must reject cross-origin navigation, got {other:?}"
            ))),
        };
    }
    let webview = if scenario == SmokeScenario::Widget {
        client
            .open_webview_with_options(url, widget_options()?, WebviewPermissions::deny_all())
            .await?
    } else if scenario == SmokeScenario::Close {
        let window = WebviewWindowOptions::new("kernal-api configured window", 800, 600)
            .map_err(|error| WebviewError::HostFailure(error.to_string()))?;
        client
            .open_webview_with_options(url, window, WebviewPermissions::deny_all())
            .await?
    } else {
        client.open_webview(url).await?
    };
    if scenario == SmokeScenario::Timeout {
        let timed_out = webview.wait_until_loaded(Duration::from_millis(50)).await;
        if timed_out != Err(WebviewError::TimedOut) {
            return Err(WebviewError::HostFailure(format!(
                "expected typed load timeout, got {timed_out:?}"
            )));
        }
        // The already-reserved terminal operation preserves the reason that
        // revoked the resource. A new semantic operation below must then see
        // the generation as stale.
        if webview.wait_until_terminal(Duration::ZERO).await != Err(WebviewError::TimedOut) {
            return Err(WebviewError::HostFailure(
                "timeout did not publish its typed terminal outcome".into(),
            ));
        }
        require_stale(&webview).await?;
        return assert_clean(client);
    }
    let loaded = webview.wait_until_loaded(Duration::from_secs(30)).await;
    match (scenario, loaded) {
        (SmokeScenario::OpenCancel, Ok(())) => {
            use std::future::Future as _;
            let baseline = client.test_observation();
            let pause = webview.pause_ui_for_test().await?;
            let mut opening = Box::pin(client.open_webview(url));
            let pending = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(opening.as_mut().poll(cx).is_pending())
            })
            .await;
            if !pending {
                return Err(WebviewError::HostFailure(
                    "paused UI open unexpectedly completed".into(),
                ));
            }
            drop(opening);
            let observation = client.test_observation();
            if observation.pending_operations != baseline.pending_operations
                || observation.live_resources != baseline.live_resources
            {
                return Err(WebviewError::HostFailure(format!(
                    "abandoned open retained semantic authority: {observation:?}"
                )));
            }
            drop(pause);
            webview.close().await?;
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while client.test_observation().active_native_opens != 0 {
                if std::time::Instant::now() >= deadline {
                    return Err(WebviewError::TimedOut);
                }
                async_engine::sleep(Duration::from_millis(5)).await;
            }
            assert_clean(client)
        }
        (SmokeScenario::CaptureCancel, Ok(())) => {
            use std::future::Future as _;
            let baseline = client.test_observation().pending_operations;
            let pause = webview.pause_ui_for_test().await?;
            for _ in 0..4 {
                let mut capture = Box::pin(
                    webview.capture_visible_png(Default::default(), Duration::from_secs(10)),
                );
                let pending = std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(capture.as_mut().poll(cx).is_pending())
                })
                .await;
                if !pending {
                    return Err(WebviewError::HostFailure(
                        "paused UI capture unexpectedly completed".into(),
                    ));
                }
                drop(capture);
            }
            let observation = client.test_observation();
            if observation.pending_operations != baseline || observation.active_native_captures != 4
            {
                return Err(WebviewError::HostFailure(format!(
                    "cancelled captures lost native admission: {observation:?}"
                )));
            }
            if !matches!(
                webview
                    .capture_visible_png(Default::default(), Duration::from_secs(10))
                    .await,
                Err(WebviewError::CaptureBusy)
            ) {
                return Err(WebviewError::HostFailure(
                    "cancelled UI queue bypassed native capture bound".into(),
                ));
            }
            drop(pause);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while client.test_observation().active_native_captures != 0 {
                if std::time::Instant::now() >= deadline {
                    return Err(WebviewError::TimedOut);
                }
                async_engine::sleep(Duration::from_millis(5)).await;
            }
            webview.close().await?;
            assert_clean(client)
        }
        (SmokeScenario::Capture, Ok(())) => {
            let snapshot = webview
                .capture_visible_png(Default::default(), Duration::from_secs(20))
                .await?;
            let first = snapshot.read_chunk(64 * 1024)?;
            if first.len() < 24 || &first[..8] != b"\x89PNG\r\n\x1a\n" {
                return Err(WebviewError::HostFailure(
                    "capture did not return a PNG".into(),
                ));
            }
            drop(first);
            loop {
                let chunk = snapshot.read_chunk(64 * 1024)?;
                if chunk.is_empty() {
                    break;
                }
            }
            drop(snapshot);
            let tiny = kernal_api::webview::ViewportCaptureLimits {
                maximum_pixels: 4_000_000,
                maximum_encoded_bytes: 8,
            };
            if !matches!(
                webview
                    .capture_visible_png(tiny, Duration::from_secs(20))
                    .await,
                Err(WebviewError::CaptureByteLimit)
            ) {
                return Err(WebviewError::HostFailure(
                    "capture byte limit did not return its typed failure".into(),
                ));
            }
            let tiny = kernal_api::webview::ViewportCaptureLimits {
                maximum_pixels: 1,
                maximum_encoded_bytes: 1024 * 1024,
            };
            if !matches!(
                webview
                    .capture_visible_png(tiny, Duration::from_secs(20))
                    .await,
                Err(WebviewError::CapturePixelLimit)
            ) {
                return Err(WebviewError::HostFailure(
                    "capture pixel limit did not return its typed failure".into(),
                ));
            }
            webview.close().await?;
            assert_clean(client)
        }
        (SmokeScenario::Close, Ok(())) => {
            let expected = WebviewWindowOptions::new("kernal-api configured window", 800, 600)
                .map_err(|error| WebviewError::HostFailure(error.to_string()))?;
            webview.verify_window_options_for_test(&expected)?;
            webview.close().await
        }
        (SmokeScenario::Widget, Ok(())) => widget_controls(client, webview, url).await,
        (SmokeScenario::Cancel, Ok(())) => {
            webview.cancel();
            if webview.wait_for_terminal().await != Err(WebviewError::Cancelled) {
                return Err(WebviewError::HostFailure(
                    "cancellation did not publish its typed terminal outcome".into(),
                ));
            }
            require_stale(&webview).await?;
            assert_clean(client)
        }
        (SmokeScenario::WindowClose, Ok(())) => {
            for timed in [false, true] {
                let pending = async {
                    if timed {
                        webview.wait_until_terminal(Duration::from_secs(30)).await
                    } else {
                        webview.wait_for_terminal().await
                    }
                };
                let mut pending = std::pin::pin!(pending);
                if async_engine::timeout(Duration::from_millis(20), &mut pending)
                    .await
                    .is_ok()
                {
                    return Err(WebviewError::HostFailure(
                        "interactive wait ended before window closure".into(),
                    ));
                }
                if async_engine::timeout(Duration::from_secs(1), webview.wait_for_terminal())
                    .await
                    .map_err(|_| WebviewError::TimedOut)?
                    != Err(WebviewError::TerminalWaitInProgress)
                {
                    return Err(WebviewError::HostFailure(
                        "overlapping terminal wait was not rejected".into(),
                    ));
                }
                if webview.wait_until_terminal(Duration::ZERO).await
                    != Err(WebviewError::TerminalWaitInProgress)
                {
                    return Err(WebviewError::HostFailure(
                        "overlapping timed wait was not rejected".into(),
                    ));
                }
            }
            let observation = client.test_observation();
            if observation.native_backings != 1 || observation.live_resources != 1 {
                return Err(WebviewError::HostFailure(format!(
                    "cancelled wait revoked its window: {observation:?}"
                )));
            }
            webview.request_window_close_for_test()?;
            if async_engine::timeout(Duration::from_secs(5), webview.wait_for_terminal())
                .await
                .map_err(|_| WebviewError::TimedOut)?
                != Err(WebviewError::WindowClosed)
            {
                return Err(WebviewError::HostFailure(
                    "window close did not revoke the semantic handle".into(),
                ));
            }
            require_stale(&webview).await?;
            assert_clean(client)
        }
        (SmokeScenario::Popup, Ok(())) | (SmokeScenario::ProhibitedRedirect, Ok(())) => {
            match webview.wait_until_terminal(Duration::from_secs(5)).await {
                Err(WebviewError::RejectedNavigation(reason))
                    if reason.contains("popup")
                        || reason.contains("tauri")
                        || reason == "navigation policy" =>
                {
                    Ok(())
                }
                other => Err(WebviewError::HostFailure(format!(
                    "unexpected semantic security terminal: {other:?}"
                ))),
            }
        }
        (SmokeScenario::Popup, Err(WebviewError::RejectedNavigation(reason)))
        | (SmokeScenario::ProhibitedRedirect, Err(WebviewError::RejectedNavigation(reason)))
            if reason.contains("popup")
                || reason.contains("tauri")
                || reason == "navigation policy" =>
        {
            Ok(())
        }
        (_, other) => Err(WebviewError::HostFailure(format!(
            "unexpected semantic webview outcome: {other:?}"
        ))),
    }
}

async fn require_stale(webview: &kernal_api::webview::WebviewHandle) -> Result<(), WebviewError> {
    let grant = WebviewUrlGrant::new("http://127.0.0.1:9/stale")?;
    let outcomes = [
        webview.wait_until_loaded(Duration::ZERO).await,
        webview.set_size(320, 240).await,
        webview.set_position(10, 10).await.map(|_| ()),
        webview.hide().await,
        webview.show().await,
        webview.focus().await,
        webview.navigate(&grant).await,
    ];
    if outcomes
        .iter()
        .all(|outcome| *outcome == Err(WebviewError::WindowClosed))
    {
        Ok(())
    } else {
        Err(WebviewError::HostFailure(format!(
            "revoked webview handle remained usable: {outcomes:?}"
        )))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SmokeScenario {
    Widget,
    OpenCancel,
    CaptureCancel,
    Capture,
    Bootstrap,
    Close,
    Popup,
    ProhibitedRedirect,
    Timeout,
    Cancel,
    WindowClose,
}

impl SmokeScenario {
    fn from_args() -> Result<Self, Box<dyn std::error::Error>> {
        match std::env::args().nth(1).as_deref() {
            Some("bootstrap") => Ok(Self::Bootstrap),
            None | Some("close") => Ok(Self::Close),
            Some("widget") => Ok(Self::Widget),
            Some("capture") => Ok(Self::Capture),
            Some("capture-cancel") => Ok(Self::CaptureCancel),
            Some("open-cancel") => Ok(Self::OpenCancel),
            Some("popup") => Ok(Self::Popup),
            Some("redirect") => Ok(Self::ProhibitedRedirect),
            Some("timeout") => Ok(Self::Timeout),
            Some("cancel") => Ok(Self::Cancel),
            Some("window-close") => Ok(Self::WindowClose),
            Some(other) => Err(format!(
                "unknown smoke scenario {other:?}; use bootstrap, capture, capture-cancel, open-cancel, close, widget, popup, redirect, timeout, cancel, or window-close"
            )
            .into()),
        }
    }

    fn page(self, address: &str) -> String {
        let action = match self {
            Self::Close
            | Self::Widget
            | Self::Bootstrap
            | Self::Capture
            | Self::CaptureCancel
            | Self::OpenCancel => "",
            Self::Timeout | Self::Cancel | Self::WindowClose => "",
            // WebKit requires a genuine user activation before it invokes the
            // new-window callback. The Linux Xvfb proof clicks this link with
            // xdotool; no host script or IPC is injected into the page.
            Self::Popup => "",
            Self::ProhibitedRedirect => "location.href = 'tauri://localhost/';",
        };
        format!(
            "<!doctype html><title>kernal-api</title>\
             <a id=\"popup\" target=\"_blank\" href=\"https://example.test/\" \
             style=\"display:block;position:absolute;left:100px;top:100px;width:200px;height:100px\">popup</a><script>\
             const probe = `ipc=${{Number(typeof window.ipc !== 'undefined')}}&\
             tauri=${{Number(typeof window.__TAURI_INTERNALS__ !== 'undefined')}}&\
             platform=${{Number(Boolean(window.webkit?.messageHandlers?.ipc))}}`;\
             const xhr = new XMLHttpRequest();\
             xhr.open('GET', 'http://{address}/_isolation?' + probe, false); xhr.send();\
             {action}</script>"
        )
    }
}

fn bootstrap_server(
    accept: &impl Fn() -> std::io::Result<(std::net::TcpStream, std::net::SocketAddr)>,
    deadline: std::time::Instant,
    port: u16,
) -> std::io::Result<()> {
    // Every stage must report bootstrap-before-page execution, no subframe
    // execution, and absence of host IPC. The second document is a real reload
    // into a fresh global, followed by a prohibited same-port/different-host URL.
    for stage in 1..=2 {
        let (mut document, request) = accept_http_request(accept, deadline)
            .map_err(socket_stage("read bootstrap document"))?;
        let path = if stage == 1 { "/finished" } else { "/reload" };
        if !request.starts_with(&format!("GET {path} HTTP/1.")) {
            return Err(std::io::Error::other(format!(
                "unexpected bootstrap document: {request:?}"
            )));
        }
        let next = if stage == 1 {
            "/reload".to_owned()
        } else {
            format!("http://localhost:{port}/rejected")
        };
        let page = format!(
            r#"<!doctype html><body><script>
const beforePage = window.__kernal_bootstrap === 17;
const noIpc = typeof window.ipc === 'undefined' && typeof window.__TAURI_INTERNALS__ === 'undefined' && !window.webkit?.messageHandlers?.ipc;
const frame = document.createElement('iframe');
window.addEventListener('message', (event) => {{
  if (event.source !== frame.contentWindow) return;
  const ok = Number(beforePage && noIpc && event.data === 'undefined');
  const request = new XMLHttpRequest();
  request.open('GET', '/_bootstrap?stage={stage}&ok=' + ok, false);
  request.send();
  location.href = '{next}';
}}, {{ once: true }});
frame.src = '/frame';
document.body.append(frame);
</script>"#
        );
        write!(
            document,
            "HTTP/1.0 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{page}",
            page.len()
        )?;
        drop(document);
        let (mut frame, frame_request) = accept_http_request(accept, deadline)
            .map_err(socket_stage("read bootstrap subframe"))?;
        if !frame_request.starts_with("GET /frame HTTP/1.") {
            return Err(std::io::Error::other(format!(
                "unexpected bootstrap frame: {frame_request:?}"
            )));
        }
        let frame_page =
            "<script>parent.postMessage(typeof window.__kernal_bootstrap, '*')</script>";
        write!(frame, "HTTP/1.0 200 OK\r\nCache-Control: no-store\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{frame_page}", frame_page.len())?;
        drop(frame);
        let (mut report, request) =
            accept_http_request(accept, deadline).map_err(socket_stage("read bootstrap report"))?;
        if !request.starts_with(&format!("GET /_bootstrap?stage={stage}&ok=1 HTTP/1.")) {
            return Err(std::io::Error::other(format!(
                "bootstrap ordering/frame/isolation proof failed: {request:?}"
            )));
        }
        report.write_all(b"HTTP/1.0 204 No Content\r\nContent-Length: 0\r\n\r\n")?;
    }
    Ok(())
}

/// Serve one document at `path`, then require its no-IPC isolation report.
fn serve_isolated_page(
    accept: &impl Fn() -> std::io::Result<(std::net::TcpStream, std::net::SocketAddr)>,
    deadline: std::time::Instant,
    path: &str,
    page: &str,
) -> std::io::Result<()> {
    let (mut stream, request) =
        accept_http_request(accept, deadline).map_err(socket_stage("read document request"))?;
    if !request.starts_with(&format!("GET {path} HTTP/1.")) {
        return Err(std::io::Error::other(format!(
            "unexpected document request: {request:?}, expected {path}"
        )));
    }
    write!(
        stream,
        "HTTP/1.0 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{page}",
        page.len()
    )
    .map_err(socket_stage("write document response"))?;
    let (mut report, report_request) =
        accept_http_request(accept, deadline).map_err(socket_stage("read isolation request"))?;
    if !report_request.starts_with("GET /_isolation?ipc=0&tauri=0&platform=0 ") {
        return Err(std::io::Error::other(format!(
            "page observed a prohibited host bridge: {report_request:?}"
        )));
    }
    report
        .write_all(b"HTTP/1.0 204 No Content\r\nContent-Length: 0\r\n\r\n")
        .map_err(socket_stage("write isolation response"))
}

fn assert_clean(client: &ExternalWebviewClient) -> Result<(), WebviewError> {
    let observation = client.test_observation();
    if observation.native_backings == 0
        && observation.active_native_opens == 0
        && observation.active_native_captures == 0
        && observation.live_blobs == 0
        && observation.retained_transfer_capacity == 0
        && observation.live_resources == 0
        && observation.pending_operations == 0
    {
        Ok(())
    } else {
        Err(WebviewError::HostFailure(format!(
            "webview cleanup leaked semantic state: {observation:?}"
        )))
    }
}

fn host_error(error: WebviewError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

fn socket_stage(stage: &'static str) -> impl FnOnce(std::io::Error) -> std::io::Error {
    move |error| std::io::Error::new(error.kind(), format!("{stage}: {error}"))
}

fn accept_http_request(
    accept: &impl Fn() -> std::io::Result<(std::net::TcpStream, std::net::SocketAddr)>,
    deadline: std::time::Instant,
) -> std::io::Result<(std::net::TcpStream, String)> {
    // Browser speculative connections and automatic favicon requests are not
    // proof messages. Bound both discarded connections and total time; never
    // turn a missing report into success or discard arbitrary unexpected paths.
    for _ in 0..16 {
        if std::time::Instant::now() >= deadline {
            break;
        }
        let (mut stream, _) = accept()?;
        stream.set_nonblocking(true)?;
        let read_deadline = deadline.min(std::time::Instant::now() + Duration::from_secs(5));
        if let Some(request) = read_http_request(&mut stream, read_deadline)? {
            stream.set_nonblocking(false)?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            if matches!(
                request.lines().next(),
                Some("GET /favicon.ico HTTP/1.0" | "GET /favicon.ico HTTP/1.1")
            ) {
                stream.write_all(b"HTTP/1.0 204 No Content\r\nContent-Length: 0\r\n\r\n")?;
                continue;
            }
            return Ok((stream, request));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "no complete HTTP request within proof limits",
    ))
}

// The real socket must be nonblocking so this deadline bounds all partial reads.
fn read_http_request(
    reader: &mut impl Read,
    deadline: std::time::Instant,
) -> std::io::Result<Option<String>> {
    let mut bytes = [0_u8; 4096];
    let mut used = 0;
    loop {
        if std::time::Instant::now() >= deadline {
            return if used == 0 {
                Ok(None)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "incomplete HTTP request",
                ))
            };
        }
        match reader.read(&mut bytes[used..]) {
            Ok(0) if used == 0 => return Ok(None),
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "truncated HTTP request",
                ))
            }
            Ok(count) => used += count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(error)
                if used == 0
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset
                    ) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error),
        }
        if bytes[..used].windows(4).any(|part| part == b"\r\n\r\n") {
            return String::from_utf8(bytes[..used].to_vec())
                .map(Some)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error));
        }
        if used == bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP request exceeds proof header limit",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn browser_favicon_is_answered_before_required_proof_request() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut icon = std::net::TcpStream::connect(address).unwrap();
        icon.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        icon.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut frame = std::net::TcpStream::connect(address).unwrap();
        frame
            .write_all(b"GET /frame HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let (_, request) = super::accept_http_request(
            &|| listener.accept(),
            std::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .unwrap();
        assert!(request.starts_with("GET /frame HTTP/1.1\r\n"));
        let mut response = String::new();
        icon.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.0 204 No Content\r\n"));
    }

    #[test]
    fn speculative_connection_is_skipped_before_real_document() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(std::net::TcpStream::connect(address).unwrap());
        let mut client = std::net::TcpStream::connect(address).unwrap();
        client
            .write_all(b"GET /finished HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let (_, request) = super::accept_http_request(
            &|| listener.accept(),
            std::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .unwrap();
        assert!(request.starts_with("GET /finished HTTP/1.1\r\n"));
    }

    #[test]
    fn fragmented_headers_are_complete_and_oversize_is_rejected() {
        struct OneByte(std::io::Cursor<Vec<u8>>);
        impl std::io::Read for OneByte {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                std::io::Read::read(&mut self.0, &mut buffer[..1])
            }
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let expected = b"GET /_isolation?ipc=0&tauri=0&platform=0 HTTP/1.1\r\n\r\n";
        let actual = super::read_http_request(
            &mut OneByte(std::io::Cursor::new(expected.to_vec())),
            deadline,
        )
        .unwrap()
        .unwrap();
        assert_eq!(actual.as_bytes(), expected);
        let error = super::read_http_request(&mut std::io::Cursor::new(vec![b'x'; 4097]), deadline)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn empty_connection_is_not_an_http_request() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        assert!(
            super::read_http_request(&mut std::io::Cursor::new(b""), deadline)
                .unwrap()
                .is_none()
        );
        assert!(super::read_http_request(
            &mut std::io::Cursor::new(b"GET /finished HTTP/1.1\r\n"),
            deadline
        )
        .is_err());
        assert_eq!(
            super::read_http_request(
                &mut std::io::Cursor::new(b"GET /finished HTTP/1.1\r\n\r\n"),
                deadline
            )
            .unwrap()
            .as_deref(),
            Some("GET /finished HTTP/1.1\r\n\r\n")
        );
    }

    #[test]
    fn socket_diagnostic_preserves_failure_kind_and_stage() {
        let error = super::socket_stage("read isolation request")(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "native socket failure",
        ));
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionAborted);
        assert_eq!(
            error.to_string(),
            "read isolation request: native socket failure"
        );
    }
}
