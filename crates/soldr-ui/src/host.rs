//! A single resident controller owns one UI surface and one refresh worker.
use crate::{
    model::State,
    ui,
    view::{self, HostEvent, HostEvents, Mode, Panel},
    worker::{self, Shared},
};
use kernal_api::{
    gfx,
    global_hotkey::{Chord, HotkeyEvent, HotkeyHandle, HotkeyOptions},
    system_tray::{TrayEvent, TrayHandle, TrayOptions},
};
use soldr_platform::sync::Mutex;
use std::{
    sync::{mpsc, Arc},
    time::Duration,
};

#[derive(Default, Debug)]
pub struct Controller {
    pub open: Option<Mode>,
}
impl Controller {
    pub fn event(&mut self, event: HostEvent) -> Option<Mode> {
        match event {
            HostEvent::Toggle => {
                self.open = if self.open.is_some() {
                    None
                } else {
                    Some(Mode::Panel)
                };
            }
            HostEvent::Dashboard => self.open = Some(Mode::Dashboard),
            HostEvent::Quit => self.open = None,
        }
        self.open
    }
}
pub fn run(
    runtime: &tokio::runtime::Handle,
    window: bool,
    hotkey: Option<String>,
    scale: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(Mutex::new(State::default()));
    let actions = worker::start(runtime, state.clone());
    let (tx, rx) = mpsc::channel();
    let events: HostEvents = Arc::new(Mutex::new(rx));
    let fallback = Arc::new(Mutex::new(false));
    let task = runtime.spawn(tray(
        state.clone(),
        tx.clone(),
        fallback.clone(),
        hotkey,
        window,
    ));
    if window {
        tx.send(HostEvent::Dashboard)?;
    }
    let mut controller = Controller::default();
    let mut pending = None;
    loop {
        let event = match pending.take() {
            Some(e) => e,
            None => match soldr_platform::wait::recv_for(&events.lock(), Duration::from_secs(1)) {
                Ok(event) => event,
                Err(soldr_platform::wait::WaitError::TimedOut(_)) => continue,
                Err(error) => return Err(error.into()),
            },
        };
        if event == HostEvent::Quit {
            break;
        }
        let Some(mode) = controller.event(event) else {
            continue;
        };
        {
            let mut state = state.lock();
            state.visible = true;
            state.last_frame_at = worker::now();
        }
        let snapshot = state.lock().clone();
        let mut panel = Panel::fixture(snapshot, mode);
        panel.scale = scale;
        panel.fallback = *fallback.lock();
        panel.events = Some(events.clone());
        panel.actions = Some(actions.clone());
        panel.shared = Some(state.clone());
        let next = panel.next.clone();
        let opts = view::options(&panel.state, mode, panel.fallback, scale);
        let result = gfx::run(opts, panel);
        state.lock().visible = false;
        controller.open = None;
        pending = next.lock().take();
        result?;
        // A decorated fallback has no tray resident to return to.
        if *fallback.lock() && pending.is_none() {
            break;
        }
    }
    task.abort();
    drop(actions);
    Ok(())
}
async fn tray(
    state: Shared,
    tx: mpsc::Sender<HostEvent>,
    fallback: Arc<Mutex<bool>>,
    hotkey: Option<String>,
    initial_window: bool,
) {
    let icon = (0..32 * 32)
        .flat_map(|i| {
            let (x, y) = (i % 32 - 16_i32, i / 32 - 16_i32);
            if x * x + y * y < 180 {
                [255, 180, 190, 200]
            } else {
                [0, 0, 0, 0]
            }
        })
        .collect();
    let options = TrayOptions::new("dev.soldr.ui", "soldr", 32, icon).and_then(|o| {
        o.with_labels(
            "Quick launch",
            "Open dashboard",
            "Quit soldr UI",
            "Build activity and distributed builds",
        )
    });
    let tray = match options {
        Ok(options) => match TrayHandle::register(options).await {
            Ok(tray) if tray.is_online() => tray,
            result => {
                eprintln!(
                    "soldr-ui: no desktop tray host: {:?}; using a decorated compact window",
                    result.err()
                );
                *fallback.lock() = true;
                if !initial_window {
                    let _ = tx.send(HostEvent::Toggle);
                }
                return;
            }
        },
        Err(error) => {
            eprintln!("soldr-ui: tray options failed: {error:?}");
            let _ = tx.send(HostEvent::Quit);
            return;
        }
    };
    let mut shortcut = hotkey_handle(hotkey).await;
    let mut timer = tokio::time::interval(Duration::from_millis(250));
    let mut last_title = String::new();
    loop {
        timer.tick().await;
        let event = match tray.try_event() {
            Some(TrayEvent::Activate) => Some(HostEvent::Toggle),
            Some(TrayEvent::OpenDashboard) => Some(HostEvent::Dashboard),
            Some(TrayEvent::Quit) => Some(HostEvent::Quit),
            None => None,
        };
        if let Some(event) = event {
            if tx.send(event).is_err() {
                break;
            }
        }
        if shortcut.as_mut().and_then(|h| h.try_event()) == Some(HotkeyEvent::Pressed)
            && tx.send(HostEvent::Toggle).is_err()
        {
            break;
        }
        let title = {
            let state = state.lock();
            format!("soldr: {}", ui::status(&state).trim_start_matches("• "))
        };
        if title != last_title {
            if let Err(e) = tray.set_title(&title).await {
                eprintln!("soldr-ui: tray status update failed: {e:?}");
            }
            last_title = title;
        }
    }
}
async fn hotkey_handle(chord: Option<String>) -> Option<HotkeyHandle> {
    let chord = chord?;
    let options = match Chord::parse(&chord)
        .and_then(|c| HotkeyOptions::new("dev.soldr.ui", "Open soldr quick launch", c))
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("soldr-ui: invalid hotkey: {e:?}");
            return None;
        }
    };
    match HotkeyHandle::register(options).await {
        Ok(handle) => Some(handle),
        Err(error) => {
            eprintln!("soldr-ui: hotkey could not register: {error:?}");
            None
        }
    }
}
