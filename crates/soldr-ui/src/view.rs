use crate::{
    dashboard, data,
    model::*,
    ui,
    worker::{self, Shared},
};
use kernal_api::gfx::{self, App, Event, Frame, Key, Support, WindowOptions};
use soldr_platform::sync::Mutex;
use std::{
    path::Path,
    sync::{mpsc, Arc},
    time::Duration,
};
use tokio::sync::mpsc::Sender;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Panel,
    Dashboard,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostEvent {
    Toggle,
    Dashboard,
    Quit,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Overview,
    Builds,
    Nodes,
    Diagnostics,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    Stop,
    Clean,
}
pub fn confirmation(state: &State, action: Action) -> Option<Confirm> {
    match action {
        Action::Clean => Some(Confirm::Clean),
        Action::Stop
            if state.value(Source::Daemon)["in_flight_compiles"]
                .as_u64()
                .is_some_and(|n| n > 0) =>
        {
            Some(Confirm::Stop)
        }
        _ => None,
    }
}
pub type HostEvents = Arc<Mutex<mpsc::Receiver<HostEvent>>>;

pub fn options(state: &State, mode: Mode, fallback: bool, scale: f32) -> WindowOptions {
    let full = mode == Mode::Dashboard;
    let width = if state.preview.narrow {
        300.0
    } else if full {
        900.0
    } else {
        360.0
    };
    let height = if full {
        if state.preview.large_text || scale > 1.0 || state.preview.narrow {
            1100.0
        } else {
            820.0
        }
    } else if state.preview.large_text || scale > 1.0 {
        630.0
    } else if state.preview.narrow {
        520.0
    } else {
        420.0
    };
    let caps = gfx::capabilities();
    WindowOptions::new(if full { "soldr Dashboard" } else { "soldr" })
        .size(width, height)
        .resizable(full)
        .decorations(full || fallback || caps.undecorated == Support::Unsupported)
        .always_on_top(!full && !fallback && caps.always_on_top != Support::Unsupported)
        .theme(ui::theme(state, scale))
}
pub struct Panel {
    pub state: State,
    pub shared: Option<Shared>,
    pub actions: Option<Sender<Action>>,
    pub events: Option<HostEvents>,
    pub next: Arc<Mutex<Option<HostEvent>>>,
    pub mode: Mode,
    pub fallback: bool,
    pub tab: Tab,
    pub confirm: Option<Confirm>,
    pub details: bool,
    pub more: bool,
    pub selected_build: Option<usize>,
    pub scale: f32,
    seen_focus: bool,
    notice: String,
}
impl Panel {
    pub fn fixture(state: State, mode: Mode) -> Self {
        let scale = if state.preview.large_text { 1.5 } else { 1.0 };
        Self {
            state,
            shared: None,
            actions: None,
            events: None,
            next: Arc::new(Mutex::new(None)),
            mode,
            fallback: false,
            tab: Tab::Overview,
            confirm: None,
            details: false,
            more: false,
            selected_build: None,
            scale,
            seen_focus: false,
            notice: String::new(),
        }
    }
    pub fn escape(&mut self) -> bool {
        self.confirm.take().is_none()
    }
    fn request(&mut self, action: Action) {
        if self.actions.is_none() {
            self.notice = "Preview only. Live actions are off.".into();
            return;
        }
        let shared = self.shared.as_ref().expect("live state");
        if let Some(mut state) = shared.try_lock() {
            if !state.begin(action) {
                return;
            }
            if let Err(e) = self.actions.as_ref().expect("sender").try_send(action) {
                worker::finish(
                    &mut state,
                    action,
                    worker::Outcome {
                        success: false,
                        message: "Couldn't queue the action.".into(),
                        stdout: String::new(),
                        stderr: e.to_string(),
                        partial_restart: false,
                    },
                );
            }
            self.state = state.clone();
        }
    }
    fn go_dashboard(&mut self, frame: &mut Frame<'_>) {
        if self.shared.is_none() {
            self.mode = Mode::Dashboard;
            return;
        }
        *self.next.lock() = Some(HostEvent::Dashboard);
        frame.close();
    }
    fn compact(&mut self, f: &mut Frame<'_>) {
        ui::status_row(f, &self.state, self.scale);
        f.space(10.0 * self.scale);
        self.hero(f);
        let builds = data::builds(&self.state);
        if let Some(build) = builds.first() {
            let mut shown = build.clone();
            shown.project = ui::fit(
                f,
                &shown.project,
                14.0 * self.scale,
                f.available_width() * 0.45,
            );
            // Stack complete measurements; never clip part of a count.
            let summary = format!("Last build: {}", shown.summary());
            if f.text_width(&summary, 14.0 * self.scale) > f.available_width() {
                let duration = shown.duration_label();
                f.label(&format!(
                    "Last build: {}{}",
                    shown.project,
                    if duration.is_empty() {
                        String::new()
                    } else {
                        format!(" · {duration}")
                    }
                ));
                let cached = shown.cached_label();
                if !cached.is_empty() {
                    f.label(&cached);
                }
            } else {
                f.label(&summary);
            }
        } else if !matches!(
            self.state.daemon(),
            DaemonState::Connecting | DaemonState::Starting
        ) {
            f.label("No builds yet.");
        }
        if let Some(cache) = data::cache_label(&self.state) {
            f.label(&cache);
        }
        if let Some(stale) = self.state.stale_rows().first() {
            f.label(stale);
        }
        if self.state.running() != Some(false) && self.state.supported_dist() {
            f.separator();
            f.label("Distributed builds");
            let helpers: Vec<_> = data::peers(&self.state)
                .into_iter()
                .filter(|p| p.kind == data::PeerKind::Helper)
                .collect();
            if !helpers.is_empty() {
                let used: u64 = helpers.iter().filter_map(|p| p.used).sum();
                let total: u64 = helpers.iter().filter_map(|p| p.total).sum();
                f.label(&format!(
                    "{} helper nodes{}",
                    helpers.len(),
                    if total > 0 {
                        format!(" · {used} of {total} slots")
                    } else {
                        String::new()
                    }
                ));
                let names = helpers
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let names = ui::fit(
                    f,
                    &format!("Using {names}"),
                    14.0 * self.scale,
                    f.available_width(),
                );
                f.label(&names);
            } else if self.state.dist == DistCapability::Off {
                f.label("Distributed builds are off.");
            } else if self.state.dist == DistCapability::Node {
                f.label("Sharing this machine with trusted devices.");
            }
            if let Some(contribution) = data::contribution(&self.state) {
                f.label(&contribution);
            }
        }
        f.separator();
        self.controls(f, false);
        self.feedback(f);
    }
    pub fn hero(&self, f: &mut Frame<'_>) {
        if let Some(rate) = data::session_rate(&self.state) {
            if self.state.daemon() == DaemonState::Stopped {
                f.label("Last session");
            }
            f.heading(&format!("{:.0}% cache hits", rate * 100.0));
        } else {
            match self.state.daemon() {
                DaemonState::Stopped => f.heading("Ready when you are"),
                DaemonState::Connecting | DaemonState::Starting => {}
                _ => f.heading("Your next build starts here"),
            }
        }
        match self.state.daemon() {
            DaemonState::Stopped => f.label("soldr starts automatically on your next build."),
            DaemonState::NeedsAttention { reason } => f.label(&reason),
            _ => {}
        }
    }
    pub fn controls(&mut self, f: &mut Frame<'_>, full: bool) {
        if ui::pending_action(f, &self.state) {
            return;
        }
        if self.state.sharing.awaiting_refresh {
            f.label(if self.state.sharing.displayed() {
                "Starting sharing · waiting for confirmation…"
            } else {
                "Stopping sharing · waiting for confirmation…"
            });
            return;
        }
        if let Some(confirm) = self.confirm {
            let question = match confirm {
                Confirm::Stop => {
                    "Stop soldr while this build is active? Running compiles may fail.".into()
                }
                Confirm::Clean => format!(
                    "Remove {} of cached build artifacts? Future builds may take longer.",
                    self.state.value(Source::Cache)["on_disk_bytes"]
                        .as_u64()
                        .map(data::bytes)
                        .unwrap_or_else(|| "the saved set".into())
                ),
            };
            f.label(&question);
            match ui::row_buttons(
                f,
                &[
                    if confirm == Confirm::Stop {
                        "Stop daemon"
                    } else {
                        "Clean cache"
                    },
                    "Cancel",
                ],
                self.scale,
            ) {
                Some(0) => {
                    self.confirm = None;
                    self.request(if confirm == Confirm::Stop {
                        Action::Stop
                    } else {
                        Action::Clean
                    });
                }
                Some(1) => self.confirm = None,
                _ => {}
            }
            return;
        }
        let (primary, label) = match self.state.daemon() {
            DaemonState::Stopped => (Some(Action::Start), "Start daemon"),
            DaemonState::NeedsAttention { .. } if self.state.can_submit(Action::Restart) => {
                (Some(Action::Restart), "Restart daemon")
            }
            DaemonState::NeedsAttention { .. } => (Some(Action::Diagnostics), "Copy diagnostics"),
            _ => (None, "Open dashboard"),
        };
        if full {
            self.more_actions(f);
            return;
        }
        self.compact_actions(f, primary, label);
    }
    fn compact_actions(&mut self, f: &mut Frame<'_>, primary: Option<Action>, label: &str) {
        match self.state.daemon() {
            DaemonState::Stopped => {
                match ui::row_buttons(f, &["Start daemon", "Logs"], self.scale) {
                    Some(0) => self.request(Action::Start),
                    Some(1) => self.request(Action::Logs),
                    _ => {}
                }
            }
            DaemonState::NeedsAttention { .. } => {
                let mut buttons = vec![label];
                if primary != Some(Action::Diagnostics) {
                    buttons.push("Copy diagnostics");
                }
                buttons.push("Logs");
                if let Some(index) = ui::row_buttons(f, &buttons, self.scale) {
                    if index == 0 {
                        self.request(primary.unwrap_or(Action::Diagnostics));
                    } else if index == buttons.len() - 1 {
                        self.request(Action::Logs);
                    } else {
                        self.request(Action::Diagnostics);
                    }
                }
            }
            _ => {
                match ui::row_buttons(f, &[label, "Logs", "More…"], self.scale) {
                    Some(0) => {
                        if let Some(action) = primary {
                            self.request(action)
                        } else {
                            self.go_dashboard(f)
                        }
                    }
                    Some(1) => self.request(Action::Logs),
                    Some(2) => self.more = !self.more,
                    _ => {}
                }
                if self.more {
                    self.more_actions(f);
                }
            }
        }
    }
    fn more_actions(&mut self, f: &mut Frame<'_>) {
        let mut choices = vec![
            ("Restart daemon", Action::Restart),
            ("Stop daemon", Action::Stop),
            ("Copy diagnostics", Action::Diagnostics),
            ("Open logs folder", Action::Logs),
        ];
        if self.state.value(Source::Cache)["on_disk_bytes"]
            .as_u64()
            .is_some_and(|n| n > 0)
        {
            choices.push(("Clean cache…", Action::Clean));
        }
        if self.mode == Mode::Dashboard {
            choices.insert(0, ("Start daemon", Action::Start));
        }
        if let Some(action) = ui::action_buttons(f, &self.state, &choices, self.scale) {
            if let Some(confirm) = confirmation(&self.state, action) {
                self.confirm = Some(confirm);
            } else {
                self.request(action);
            }
        }
    }
    pub fn dist_controls(&mut self, f: &mut Frame<'_>) {
        if !self.state.supported_dist() || self.state.running() != Some(true) {
            return;
        }
        if ui::pending_action(f, &self.state) {
            return;
        }
        if self.state.sharing.pending || self.state.sharing.awaiting_refresh {
            f.label(if self.state.sharing.displayed() {
                "Starting sharing · waiting for confirmation…"
            } else {
                "Stopping sharing · waiting for confirmation…"
            });
        } else {
            let mut enabled = self.state.sharing.displayed();
            if f.toggle(&mut enabled, "Share this machine") {
                self.request(Action::Share(enabled));
            }
            f.label("Offer this machine to your trusted devices.");
        }
        match &self.state.pairing {
            Pairing::Generating => f.label("Generating a pairing code…"),
            Pairing::AwaitingPeer { code, .. } => {
                f.label("Awaiting your other device");
                f.label(code);
            }
            Pairing::Connected => f.label("Device connected."),
            Pairing::Expired => f.label("Pairing code expired. Generate a new code."),
            Pairing::Failed { reason } => f.label(reason),
            Pairing::Idle => {}
        }
        if ui::action_button(f, &self.state, "Pair a device", Action::Pair) {
            self.request(Action::Pair);
        }
    }
    pub fn feedback(&mut self, f: &mut Frame<'_>) {
        if !self.notice.is_empty() {
            f.label(&self.notice);
        }
        match &self.state.action {
            ActionState::Succeeded { message, .. } => {
                f.label(message);
                if f.button("Dismiss") {
                    self.dismiss();
                }
            }
            ActionState::Failed { message, .. } => {
                if self.mode == Mode::Panel || self.tab == Tab::Builds {
                    f.label(message);
                }
                if f.button("Dismiss") {
                    self.dismiss();
                }
            }
            _ => {}
        }
        if self.mode == Mode::Panel
            && matches!(
                self.state.action,
                ActionState::Failed {
                    action: Action::Start,
                    ..
                }
            )
        {
            match ui::row_buttons(f, &["Copy diagnostics", "Open logs folder"], self.scale) {
                Some(0) => self.request(Action::Diagnostics),
                Some(1) => self.request(Action::Logs),
                _ => {}
            }
        }
        if self.mode == Mode::Panel
            && (!self.state.problems().is_empty()
                || matches!(self.state.action, ActionState::Succeeded { .. }))
        {
            if f.button(if self.details {
                "Hide details"
            } else {
                "Show details"
            }) {
                self.details = !self.details;
            }
            if self.details {
                for problem in self.state.problems() {
                    f.label(&problem);
                }
                for sample in self.state.samples.values() {
                    if let Freshness::Stale { reason, .. } = &sample.freshness {
                        f.label(reason);
                    }
                }
                match &self.state.action {
                    ActionState::Succeeded { stdout, stderr, .. }
                    | ActionState::Failed { stdout, stderr, .. } => {
                        if !stdout.is_empty() {
                            f.label(stdout);
                        }
                        if !stderr.is_empty() {
                            f.label(stderr);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    fn dismiss(&mut self) {
        self.state.action = ActionState::Idle;
        if let Some(shared) = &self.shared {
            if let Some(mut s) = shared.try_lock() {
                if !matches!(s.action, ActionState::Pending { .. }) {
                    s.action = ActionState::Idle;
                }
            }
        }
    }
    fn events(&mut self, f: &mut Frame<'_>) {
        let events = f.events().to_vec();
        for event in events {
            match event {
                Event::Key {
                    key: Key::Escape,
                    pressed: true,
                    ..
                } => {
                    if self.escape() {
                        f.close();
                    }
                }
                Event::Focused(true) => self.seen_focus = true,
                Event::Focused(false)
                    if self.seen_focus && self.mode == Mode::Panel && !self.fallback =>
                {
                    f.close()
                }
                _ => {}
            }
        }
        let host = self
            .events
            .as_ref()
            .and_then(|events| events.try_lock().and_then(|events| events.try_recv().ok()));
        match host {
            Some(HostEvent::Toggle) => f.close(),
            Some(HostEvent::Dashboard) if self.mode != Mode::Dashboard => self.go_dashboard(f),
            Some(HostEvent::Quit) => {
                *self.next.lock() = Some(HostEvent::Quit);
                f.close();
            }
            _ => {}
        }
    }
}
impl App for Panel {
    fn frame(&mut self, f: &mut Frame<'_>) {
        if let Some(shared) = &self.shared {
            if let Some(mut s) = shared.try_lock() {
                s.last_frame_at = worker::now();
                self.state = s.clone();
            }
            self.state.tick(worker::now());
        }
        self.events(f);
        f.set_theme(ui::theme(&self.state, self.scale));
        if self.mode == Mode::Dashboard {
            f.scroll(f.available_height(), |f| dashboard::draw(self, f));
        } else if self.more
            || self.confirm.is_some()
            || self.details
            || matches!(
                self.state.action,
                ActionState::Succeeded { .. } | ActionState::Failed { .. }
            )
            || self.scale > 1.0
            || self.state.preview.large_text
            || self.state.preview.narrow
        {
            f.scroll(f.available_height(), |f| self.compact(f));
        } else {
            self.compact(f);
        }
        if self.shared.is_some() {
            let ms = if matches!(self.state.action, ActionState::Pending { .. }) {
                500
            } else {
                2000
            };
            f.request_repaint_after(Duration::from_millis(ms));
        }
    }
}
pub fn screenshot(
    state: State,
    mode: Mode,
    tab: Tab,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut panel = Panel::fixture(state, mode);
    panel.tab = tab;
    let image = gfx::snapshot(&options(&panel.state, mode, false, panel.scale), &mut panel)?;
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, image.width(), image.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(image.rgba())?;
    writer.finish()?;
    Ok(())
}
