//! The `widget` scenario: a floating-widget window (app id, undecorated,
//! transparent, keep-above, skip-taskbar, initial position) driven through
//! every handle control, then navigated in place, leaving nothing behind.

use std::time::Duration;

use kernal_api::async_engine;
use kernal_api::webview::{
    BestEffort, ExternalWebviewClient, WebviewError, WebviewHandle, WebviewTestPresentation,
    WebviewUrlGrant, WebviewWindowOptions, WindowOptionsError,
};

pub const WIDGET_APP_ID: &str = "dev.kernal-api.smoke-widget";

/// Appended to the first page's URL to make the page the widget navigates
/// to in place; the loopback server requires its no-IPC report as well.
pub const NAVIGATED_QUERY: &str = "?navigated=1";

const INITIAL_POSITION: (i32, i32) = (40, 60);
const MOVED_POSITION: (i32, i32) = (120, 90);

pub fn widget_options() -> Result<WebviewWindowOptions, WebviewError> {
    widget_options_sized(360, 240)
}

fn widget_options_sized(width: u32, height: u32) -> Result<WebviewWindowOptions, WebviewError> {
    WebviewWindowOptions::new("kernal-api widget", width, height)
        .and_then(|options| options.initial_position(INITIAL_POSITION.0, INITIAL_POSITION.1))
        .map_err(|error| WebviewError::HostFailure(error.to_string()))
        .map(|options| {
            options
                .decorations(false)
                .transparent(true)
                .always_on_top(true)
                .skip_taskbar(true)
        })
}

// On an X11 or Wayland display, keep-above is the window manager's or
// compositor's decision: Wayland compositors ignore the request, and a bare
// Xvfb has no window manager to grant it. The toolkit then reports what the
// display decided, so only a host without such a display (WebView2, AppKit)
// must report it. This reads the launch environment, not the host OS.
fn keep_above_is_display_policy() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

async fn await_presentation(
    webview: &WebviewHandle,
    what: &str,
    ready: impl Fn(&WebviewTestPresentation) -> bool,
) -> Result<WebviewTestPresentation, WebviewError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let presentation = webview.presentation_for_test()?;
        if ready(&presentation) {
            return Ok(presentation);
        }
        if std::time::Instant::now() >= deadline {
            return Err(WebviewError::HostFailure(format!(
                "window did not become {what}: {presentation:?}"
            )));
        }
        async_engine::sleep(Duration::from_millis(20)).await;
    }
}

fn at(presentation: &WebviewTestPresentation, (x, y): (i32, i32)) -> bool {
    (presentation.logical_x - f64::from(x)).abs() <= 1.0
        && (presentation.logical_y - f64::from(y)).abs() <= 1.0
}

// Moves the 360x240 undecorated widget to the primary work area's
// bottom-right corner and reads the position back, where position is honoured.
async fn place_at_work_area_corner(
    client: &ExternalWebviewClient,
    webview: &WebviewHandle,
    position: BestEffort,
) -> Result<(), WebviewError> {
    let monitors = client.monitors().await?;
    let Some(monitors) = monitors else {
        return if position == BestEffort::Unsupported {
            Ok(())
        } else {
            Err(mismatch("monitors unavailable where position is honoured"))
        };
    };
    if position == BestEffort::Unsupported {
        return Err(mismatch(
            "monitors reported where the compositor places windows",
        ));
    }
    let mut primaries = monitors.iter().filter(|monitor| monitor.primary);
    let (Some(primary), None) = (primaries.next(), primaries.next()) else {
        return Err(mismatch(format!(
            "not exactly one primary monitor: {monitors:?}"
        )));
    };
    if let Some(monitor) = monitors
        .iter()
        .find(|monitor| !monitor.bounds.contains(&monitor.work_area))
    {
        return Err(mismatch(format!(
            "work area outside its monitor: {monitor:?}"
        )));
    }
    let area = primary.work_area;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "work-area edges are bounded logical screen coordinates"
    )]
    let corner = (
        (area.x + area.width - 360.0).floor() as i32,
        (area.y + area.height - 240.0).floor() as i32,
    );
    webview.set_position(corner.0, corner.1).await?;
    let placed = await_presentation(webview, "at the work area corner", |presentation| {
        at(presentation, corner)
    })
    .await?;
    eprintln!("widget placed at work area corner: {placed:?} in {primary:?}");
    Ok(())
}

fn mismatch(message: impl Into<String>) -> WebviewError {
    WebviewError::HostFailure(message.into())
}

/// Verify the toolkit received each option, then resize, move, hide, show,
/// focus, navigate in place, and close, leaving no semantic or native state
/// behind. The loopback server has already required the no-IPC isolation
/// report for this page and requires it again for the navigated one.
pub async fn widget_controls(
    client: &ExternalWebviewClient,
    webview: WebviewHandle,
    url: &str,
) -> Result<(), WebviewError> {
    let options = widget_options()?;
    let support = client.window_support();
    eprintln!("widget window support: {support:?}");
    webview.verify_window_options_for_test(&options)?;
    let initial = webview.presentation_for_test()?;
    eprintln!("widget initial presentation: {initial:?}");
    if initial.decorated {
        return Err(mismatch("undecorated widget reports decorations"));
    }
    if !keep_above_is_display_policy() && !initial.always_on_top {
        return Err(mismatch(
            "keep-above request did not reach the native window",
        ));
    }
    if support.position == BestEffort::Requested {
        await_presentation(&webview, "at its initial position", |presentation| {
            at(presentation, INITIAL_POSITION)
        })
        .await?;
    }
    place_at_work_area_corner(client, &webview, support.position).await?;
    if webview.set_size(0, 240).await
        != Err(WebviewError::InvalidWindowOptions(
            WindowOptionsError::InvalidSize,
        ))
    {
        return Err(mismatch(
            "invalid resize was not rejected before native effects",
        ));
    }
    webview.set_size(480, 320).await?;
    let resized = await_presentation(&webview, "480x320", |presentation| {
        (presentation.logical_width - 480.0).abs() <= 1.0
            && (presentation.logical_height - 320.0).abs() <= 1.0
    })
    .await?;
    eprintln!("widget resized: {resized:?}");
    if webview.set_position(32768, 0).await
        != Err(WebviewError::InvalidWindowOptions(
            WindowOptionsError::InvalidPosition,
        ))
    {
        return Err(mismatch(
            "invalid move was not rejected before native effects",
        ));
    }
    let moved = webview
        .set_position(MOVED_POSITION.0, MOVED_POSITION.1)
        .await?;
    if moved != support.position {
        return Err(mismatch(format!(
            "set_position reported {moved:?}, host support is {:?}",
            support.position
        )));
    }
    if moved == BestEffort::Requested {
        let moved = await_presentation(&webview, "moved", |presentation| {
            at(presentation, MOVED_POSITION)
        })
        .await?;
        eprintln!("widget moved: {moved:?}");
    }
    webview.hide().await?;
    await_presentation(&webview, "hidden", |presentation| !presentation.visible).await?;
    webview.show().await?;
    await_presentation(&webview, "visible", |presentation| presentation.visible).await?;
    // Shown again, the window must still be kept out of the taskbar.
    webview.verify_window_options_for_test(&widget_options_sized(480, 320)?)?;
    webview.focus().await?;
    let navigated = WebviewUrlGrant::new(&format!("{url}{NAVIGATED_QUERY}"))?;
    webview.navigate(&navigated).await?;
    webview.wait_until_loaded(Duration::from_secs(30)).await?;
    let observation = client.test_observation();
    if observation.live_resources != 1 || observation.native_backings != 1 {
        return Err(mismatch(format!(
            "window operations changed view ownership: {observation:?}"
        )));
    }
    webview.close().await?;
    super::assert_clean(client)
}
