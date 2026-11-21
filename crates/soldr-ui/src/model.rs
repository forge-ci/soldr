//! Typed UI state; CLI JSON remains an additive boundary, never a display protocol.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Status,
    Daemon,
    Doctor,
    Logs,
    Builds,
    Session,
    Cache,
    Dist,
}
impl Source {
    pub const ALL: [Self; 8] = [
        Self::Status,
        Self::Daemon,
        Self::Doctor,
        Self::Logs,
        Self::Builds,
        Self::Session,
        Self::Cache,
        Self::Dist,
    ];
    pub fn args(self) -> &'static [&'static str] {
        match self {
            Self::Status => &["status", "--json"],
            Self::Daemon => &["daemon", "status", "--json"],
            Self::Doctor => &["doctor", "--json"],
            Self::Logs => &["logs", "paths", "--json"],
            Self::Builds => &["logs", "list", "--limit", "5", "--json"],
            Self::Session => &["cache", "report", "--json"],
            Self::Cache => &["cache", "stats", "--json"],
            Self::Dist => &["dist", "status", "--json"],
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Status => "Soldr version",
            Self::Daemon => "Daemon status",
            Self::Doctor => "Health check",
            Self::Logs => "Logs",
            Self::Builds => "Recent builds",
            Self::Session => "Cache hits",
            Self::Cache => "Cache size",
            Self::Dist => "Distributed builds",
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Reply {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Freshness {
    #[default]
    Loading,
    Fresh {
        at: u64,
    },
    Stale {
        at: Option<u64>,
        failed_at: u64,
        reason: String,
    },
    Unsupported,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LastGood {
    pub at: u64,
    #[serde(default)]
    pub generation: u64,
    pub value: Value,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Sample {
    pub last_good: Option<LastGood>,
    pub freshness: Freshness,
    pub reply: Reply,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonState {
    Ready,
    Building { n: u64 },
    Stopped,
    Starting,
    Connecting,
    NeedsAttention { reason: String },
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DistCapability {
    Unsupported,
    Off,
    Client,
    Node,
    #[default]
    Unknown,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Start,
    Stop,
    Restart,
    Logs,
    Diagnostics,
    Clean,
    Share(bool),
    Pair,
}
impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Self::Start => "Starting soldr",
            Self::Stop => "Stopping soldr",
            Self::Restart => "Restarting soldr",
            Self::Logs => "Opening logs",
            Self::Diagnostics => "Copying diagnostics",
            Self::Clean => "Cleaning cache",
            Self::Share(true) => "Starting sharing",
            Self::Share(false) => "Stopping sharing",
            Self::Pair => "Generating a pairing code",
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ActionState {
    #[default]
    Idle,
    Pending {
        action: Action,
    },
    Succeeded {
        action: Action,
        message: String,
        #[serde(default)]
        stdout: String,
        #[serde(default)]
        stderr: String,
    },
    Failed {
        action: Action,
        message: String,
        #[serde(default)]
        stdout: String,
        stderr: String,
        #[serde(default)]
        partial_restart: bool,
    },
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Sharing {
    pub confirmed: bool,
    pub requested: Option<bool>,
    pub pending: bool,
    #[serde(default)]
    pub awaiting_refresh: bool,
    #[serde(default)]
    pub requested_at: u64,
}
impl Sharing {
    pub fn displayed(&self) -> bool {
        self.requested.unwrap_or(self.confirmed)
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Pairing {
    #[default]
    Idle,
    Generating,
    AwaitingPeer {
        code: String,
        since: u64,
    },
    Connected,
    Failed {
        reason: String,
    },
    Expired,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Preview {
    pub narrow: bool,
    pub large_text: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct State {
    // `sources` is the fixture/CLI input envelope. Runtime decisions use typed samples.
    pub sources: BTreeMap<Source, Reply>,
    pub samples: BTreeMap<Source, Sample>,
    pub generation: u64,
    pub now: u64,
    pub visible: bool,
    pub last_frame_at: u64,
    pub dist: DistCapability,
    pub sharing: Sharing,
    pub action: ActionState,
    pub pairing: Pairing,
    pub preview: Preview,
}
impl Default for State {
    fn default() -> Self {
        Self {
            sources: BTreeMap::new(),
            samples: BTreeMap::new(),
            generation: 0,
            now: 1_700_000_000,
            visible: false,
            last_frame_at: 0,
            dist: DistCapability::Unknown,
            sharing: Sharing::default(),
            action: ActionState::Idle,
            pairing: Pairing::Idle,
            preview: Preview::default(),
        }
    }
}
impl State {
    pub fn hydrate(mut self) -> Self {
        for (source, reply) in self.sources.clone() {
            if !self.samples.contains_key(&source) {
                self.apply(self.generation, source, reply);
            }
        }
        self
    }
    pub fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }
    pub fn polling_visible(&self) -> bool {
        self.visible && self.now.saturating_sub(self.last_frame_at) <= 5
    }
    pub fn value(&self, source: Source) -> &Value {
        self.samples
            .get(&source)
            .and_then(|s| s.last_good.as_ref())
            .map_or(&Value::Null, |s| &s.value)
    }
    pub fn apply(&mut self, generation: u64, source: Source, reply: Reply) -> bool {
        if generation != self.generation {
            return false;
        }
        if source == Source::Dist && dist_unsupported(&reply) {
            self.dist = DistCapability::Unsupported;
            self.samples.insert(
                source,
                Sample {
                    freshness: Freshness::Unsupported,
                    ..Sample::default()
                },
            );
            self.sources.remove(&source);
            return true;
        }
        let parsed = if source == Source::Doctor {
            parse_json(source, &reply)
        } else {
            parse(source, &reply)
        };
        let sample = self.samples.entry(source).or_default();
        match parsed {
            Ok(value) => {
                sample.last_good = Some(LastGood {
                    at: self.now,
                    generation,
                    value: value.clone(),
                });
                sample.freshness = Freshness::Fresh { at: self.now };
                if source == Source::Dist {
                    self.reconcile_dist(&value);
                }
            }
            Err(reason) => {
                sample.freshness = Freshness::Stale {
                    at: sample.last_good.as_ref().map(|v| v.at),
                    failed_at: self.now,
                    reason,
                }
            }
        }
        self.samples.entry(source).or_default().reply = reply;
        true
    }
    fn reconcile_dist(&mut self, value: &Value) {
        self.dist = match value["role"].as_str() {
            Some("off") => DistCapability::Off,
            Some("client") => DistCapability::Client,
            Some("node" | "dist_node" | "dist node") => DistCapability::Node,
            _ => DistCapability::Unknown,
        };
        self.sharing.confirmed = value["sharing_enabled"]
            .as_bool()
            .unwrap_or(self.dist == DistCapability::Node);
        if !self.sharing.pending && self.sharing.requested == Some(self.sharing.confirmed) {
            self.sharing.requested = None;
            self.sharing.awaiting_refresh = false;
        }
        if value["pairing"]["state"] == "connected" {
            self.pairing = Pairing::Connected;
        }
    }
    pub fn tick(&mut self, now: u64) {
        self.now = now;
        if matches!(self.pairing,Pairing::AwaitingPeer {since,..} if now.saturating_sub(since)>=300)
        {
            self.pairing = Pairing::Expired;
        }
        if self.sharing.awaiting_refresh && now.saturating_sub(self.sharing.requested_at) >= 30 {
            let requested = self.sharing.requested.unwrap_or(self.sharing.confirmed);
            self.sharing.requested = None;
            self.sharing.awaiting_refresh = false;
            let (stdout, stderr) = match &self.action {
                ActionState::Succeeded { stdout, stderr, .. } => (stdout.clone(), stderr.clone()),
                _ => (String::new(), String::new()),
            };
            self.action = ActionState::Failed {
                action: Action::Share(requested),
                message: "Sharing wasn't confirmed. The last confirmed setting is shown.".into(),
                stdout,
                stderr:format!("{stderr}\nThe status refresh did not confirm the requested setting within 30 seconds."),
                partial_restart: false,
            };
        }
    }
    pub fn running(&self) -> Option<bool> {
        self.value(Source::Daemon)["running"].as_bool()
    }
    pub fn daemon(&self) -> DaemonState {
        let current = self
            .samples
            .get(&Source::Daemon)
            .and_then(|s| s.last_good.as_ref())
            .is_some_and(|v| v.generation == self.generation);
        if matches!(
            self.action,
            ActionState::Failed {
                partial_restart: true,
                ..
            }
        ) && !current
        {
            return DaemonState::Stopped;
        }
        if matches!(
            self.action,
            ActionState::Pending {
                action: Action::Start | Action::Restart
            }
        ) {
            return DaemonState::Starting;
        }
        if matches!(
            self.samples.get(&Source::Daemon).map(|s| &s.freshness),
            Some(Freshness::Stale { .. })
        ) {
            return DaemonState::NeedsAttention {
                reason: "Couldn't reach soldr. Check the logs or copy diagnostics.".into(),
            };
        }
        if !current {
            match self.action {
                ActionState::Succeeded {
                    action: Action::Start | Action::Restart,
                    ..
                } => return DaemonState::Starting,
                ActionState::Succeeded {
                    action: Action::Stop,
                    ..
                } => return DaemonState::Stopped,
                _ => {}
            }
        }
        if self.running() == Some(false) {
            return DaemonState::Stopped;
        }
        let doctor = self.value(Source::Doctor);
        if doctor["drift"] == true
            || doctor["cache_health"]["zero_hit_storm"] == true
            || doctor["cache_health"]["daemon_churn"] == true
        {
            return DaemonState::NeedsAttention {
                reason: "Cache health check needs attention.".into(),
            };
        }
        match self.running() {
            Some(true) => match self.value(Source::Daemon)["in_flight_compiles"]
                .as_u64()
                .unwrap_or(0)
            {
                0 => DaemonState::Ready,
                n => DaemonState::Building { n },
            },
            _ => DaemonState::Connecting,
        }
    }
    pub fn supported_dist(&self) -> bool {
        matches!(
            self.dist,
            DistCapability::Off | DistCapability::Client | DistCapability::Node
        )
    }
    pub fn can_submit(&self, action: Action) -> bool {
        if matches!(self.action, ActionState::Pending { .. }) || self.sharing.awaiting_refresh {
            return false;
        }
        match action {
            Action::Start => self.daemon() == DaemonState::Stopped,
            Action::Stop | Action::Restart => {
                self.running() == Some(true)
                    && matches!(
                        self.samples.get(&Source::Daemon).map(|s| &s.freshness),
                        Some(Freshness::Fresh { .. })
                    )
                    && !matches!(
                        self.daemon(),
                        DaemonState::Connecting | DaemonState::Starting | DaemonState::Stopped
                    )
            }
            Action::Share(_) => {
                self.supported_dist()
                    && self.running() == Some(true)
                    && !matches!(
                        self.daemon(),
                        DaemonState::Stopped | DaemonState::Starting | DaemonState::Connecting
                    )
                    && !self.sharing.pending
                    && !self.sharing.awaiting_refresh
            }
            Action::Pair => {
                self.supported_dist()
                    && self.running() == Some(true)
                    && !matches!(
                        self.daemon(),
                        DaemonState::Stopped | DaemonState::Starting | DaemonState::Connecting
                    )
                    && !matches!(
                        self.pairing,
                        Pairing::Generating | Pairing::AwaitingPeer { .. }
                    )
            }
            _ => true,
        }
    }
    pub fn begin(&mut self, action: Action) -> bool {
        if !self.can_submit(action) {
            return false;
        }
        self.next_generation();
        self.action = ActionState::Pending { action };
        if let Action::Share(requested) = action {
            self.sharing.requested = Some(requested);
            self.sharing.pending = true;
            self.sharing.requested_at = self.now;
        }
        if action == Action::Pair {
            self.pairing = Pairing::Generating;
        }
        true
    }
    pub fn stale_rows(&self) -> Vec<String> {
        self.samples
            .iter()
            .filter_map(|(source, sample)| {
                if let Freshness::Stale { at, .. } = &sample.freshness {
                    Some(at.map_or_else(
                        || format!("{} could not refresh.", source.label()),
                        |at| {
                            format!(
                                "{} · last updated {} ago",
                                source.label(),
                                age(self.now.saturating_sub(at))
                            )
                        },
                    ))
                } else {
                    None
                }
            })
            .collect()
    }
    pub fn problems(&self) -> Vec<String> {
        let mut rows = self.stale_rows();
        if let DaemonState::NeedsAttention { reason } = self.daemon() {
            rows.push(reason);
        }
        if let ActionState::Failed { message, .. } = &self.action {
            rows.push(message.clone());
        }
        rows.sort();
        rows.dedup();
        rows
    }
}
pub fn age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m", seconds / 60)
    }
}
pub fn parse(source: Source, reply: &Reply) -> Result<Value, String> {
    if !reply.success {
        return Err(format!(
            "{}: {}\n{}",
            source.label(),
            reply.stderr,
            reply.stdout
        ));
    }
    parse_json(source, reply)
}
pub fn parse_json(source: Source, reply: &Reply) -> Result<Value, String> {
    let value: Value = serde_json::from_str(&reply.stdout)
        .map_err(|e| format!("{e}\n{}\n{}", reply.stderr, reply.stdout))?;
    let valid = match source {
        Source::Status => value["soldr_version"].is_string(),
        Source::Daemon => value["running"].is_boolean(),
        Source::Doctor => value["drift"].is_boolean(),
        Source::Logs => value["paths"].is_array(),
        Source::Builds => value["launches"].is_array(),
        Source::Session => value
            .get("last_session")
            .is_some_and(|v| v.is_object() || v.is_null()),
        Source::Cache => value["on_disk_bytes"].is_u64(),
        Source::Dist => value["role"].is_string(),
    };
    if !valid {
        return Err(format!(
            "Invalid {} response.\n{}\n{}",
            source.label(),
            reply.stderr,
            reply.stdout
        ));
    }
    Ok(value)
}
pub fn dist_unsupported(reply: &Reply) -> bool {
    !reply.success
        && [
            "unrecognized subcommand 'dist'",
            "unknown subcommand",
            "unknown command",
            "unexpected argument 'dist'",
        ]
        .iter()
        .any(|s| reply.stderr.to_lowercase().contains(s))
}
