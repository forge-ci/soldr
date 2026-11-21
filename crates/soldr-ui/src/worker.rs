//! One actor owns refreshes and actions. Older generations cannot overwrite intent.
use crate::{model::*, process::command};
use soldr_platform::sync::Mutex;
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
pub type Shared = Arc<Mutex<State>>;

#[derive(Default)]
pub struct RefreshPolicy {
    pub doctor_at: u64,
    fast_at: u64,
    slow_at: u64,
    hidden_at: u64,
    pub dist_supported: Option<bool>,
}
impl RefreshPolicy {
    pub fn due(&mut self, now: u64, visible: bool, force: bool) -> Vec<Source> {
        let mut out = Vec::new();
        if !visible {
            if now.saturating_sub(self.hidden_at) >= 30 {
                self.hidden_at = now;
                out.push(Source::Daemon);
            }
            return out;
        }
        if force || now.saturating_sub(self.fast_at) >= 2 {
            self.fast_at = now;
            out.extend([Source::Status, Source::Daemon, Source::Session]);
            if self.dist_supported != Some(false) {
                out.push(Source::Dist);
            }
        }
        if force || now.saturating_sub(self.slow_at) >= 15 {
            self.slow_at = now;
            out.extend([Source::Builds, Source::Cache, Source::Logs]);
        }
        if now.saturating_sub(self.doctor_at) >= 60 {
            self.doctor_at = now;
            out.push(Source::Doctor);
        }
        out
    }
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |v| v.as_secs())
}
enum Completion {
    Source {
        source: Source,
        request: u64,
        epoch: u64,
        reply: Reply,
        capability: Option<bool>,
    },
    Action {
        action: Action,
        outcome: Outcome,
    },
}
struct Coordinator {
    state: Shared,
    policy: RefreshPolicy,
    tasks: tokio::task::JoinSet<Completion>,
    requests: std::collections::BTreeMap<Source, u64>,
    sequence: u64,
    clipboard: Arc<Mutex<Option<arboard::Clipboard>>>,
    action_running: bool,
}
impl Coordinator {
    fn schedule(&mut self, force: bool) {
        let (at, visible, epoch) = {
            let mut state = self.state.lock();
            state.tick(now());
            (state.now, state.polling_visible(), state.generation)
        };
        // An action gets one confirmation refresh even if its panel just closed.
        for source in self.policy.due(at, visible || force, force) {
            if self.requests.contains_key(&source)
                && (!force || (source == Source::Dist && self.policy.dist_supported.is_none()))
            {
                continue;
            }
            self.sequence += 1;
            let request = self.sequence;
            self.requests.insert(source, request);
            let probe = source == Source::Dist && self.policy.dist_supported.is_none();
            self.tasks.spawn(async move {
                let mut capability = None;
                let reply = if probe {
                    let help = command(&["help", "dist"]).await;
                    if help.success {
                        capability = Some(true);
                        command(source.args()).await
                    } else {
                        if dist_unsupported(&help) {
                            capability = Some(false);
                        }
                        help
                    }
                } else {
                    command(source.args()).await
                };
                Completion::Source {
                    source,
                    request,
                    epoch,
                    reply,
                    capability,
                }
            });
        }
    }
    fn action(&mut self, action: Action) {
        if self.action_running {
            return;
        }
        self.action_running = true;
        if action == Action::Diagnostics {
            self.policy.doctor_at = now();
        }
        let state = self.state.clone();
        let clipboard = self.clipboard.clone();
        self.tasks.spawn(async move {
            Completion::Action {
                action,
                outcome: perform(action, &clipboard, &state).await,
            }
        });
    }
    fn complete(&mut self, result: Completion) {
        match result {
            Completion::Source {
                source,
                request,
                epoch,
                reply,
                capability,
            } => {
                if let Some(supported) = capability {
                    self.policy.dist_supported = Some(supported);
                }
                if self.requests.get(&source) != Some(&request) {
                    return;
                }
                self.requests.remove(&source);
                let mut state = self.state.lock();
                state.tick(now());
                if capability == Some(false) {
                    // Availability is cached independently from a mutable status generation.
                    let generation = state.generation;
                    state.apply(generation, source, reply);
                } else {
                    state.apply(epoch, source, reply);
                }
            }
            Completion::Action { action, outcome } => {
                self.action_running = false;
                {
                    let mut state = self.state.lock();
                    state.tick(now());
                    finish(&mut state, action, outcome);
                }
                self.schedule(true);
            }
        }
    }
    async fn run(mut self, mut rx: mpsc::Receiver<Action>) {
        let mut timer = tokio::time::interval(Duration::from_millis(250));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {biased;
                action=rx.recv()=> {let Some(action)=action else {break;};self.action(action);},
                Some(result)=self.tasks.join_next(),if !self.tasks.is_empty()=>match result {
                    Ok(result)=>self.complete(result),Err(error)=>eprintln!("soldr-ui background task failed: {error}"),
                },
                _=timer.tick()=>self.schedule(false),
            }
        }
    }
}
pub fn start(runtime: &tokio::runtime::Handle, state: Shared) -> mpsc::Sender<Action> {
    let (tx, rx) = mpsc::channel(1);
    runtime.spawn(
        Coordinator {
            state,
            policy: RefreshPolicy::default(),
            tasks: tokio::task::JoinSet::new(),
            requests: std::collections::BTreeMap::new(),
            sequence: 0,
            clipboard: Arc::new(Mutex::new(None)),
            action_running: false,
        }
        .run(rx),
    );
    tx
}
#[derive(Debug)]
pub struct Outcome {
    pub success: bool,
    pub message: String,
    pub stdout: String,
    pub stderr: String,
    pub partial_restart: bool,
}
impl Outcome {
    fn reply(reply: Reply, message: &str) -> Self {
        Self {
            success: reply.success,
            message: message.into(),
            stdout: reply.stdout,
            stderr: reply.stderr,
            partial_restart: false,
        }
    }
    fn error(message: &str, stderr: String) -> Self {
        Self {
            success: false,
            message: message.into(),
            stdout: String::new(),
            stderr,
            partial_restart: false,
        }
    }
}
pub fn restart_outcome(stopped: Reply, started: Option<Reply>) -> Outcome {
    if !stopped.success {
        return Outcome::reply(stopped, "Couldn't stop soldr; restart was cancelled.");
    }
    let Some(started) = started else {
        return Outcome::error(
            "Restart didn't finish.",
            "Start result was not received.".into(),
        );
    };
    Outcome {
        success: started.success,
        message: if started.success {
            "Soldr restarted."
        } else {
            "Soldr stopped, but couldn't start again. Choose Start daemon to retry."
        }
        .into(),
        stdout: format!("{}\n{}", stopped.stdout, started.stdout),
        stderr: format!("{}\n{}", stopped.stderr, started.stderr),
        partial_restart: !started.success,
    }
}
pub fn finish(state: &mut State, action: Action, outcome: Outcome) {
    state.next_generation();
    if let Action::Share(_) = action {
        state.sharing.pending = false;
        if outcome.success {
            state.sharing.awaiting_refresh =
                state.sharing.requested != Some(state.sharing.confirmed);
            if !state.sharing.awaiting_refresh {
                state.sharing.requested = None;
            }
        } else {
            state.sharing.requested = None;
            state.sharing.awaiting_refresh = false;
        }
    }
    if action == Action::Pair {
        state.pairing = if outcome.success {
            Pairing::AwaitingPeer {
                code: outcome.stdout.trim().into(),
                since: state.now,
            }
        } else {
            Pairing::Failed {
                reason: outcome.message.clone(),
            }
        };
    }
    state.action = if outcome.success {
        ActionState::Succeeded {
            action,
            message: outcome.message,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
        }
    } else {
        ActionState::Failed {
            action,
            message: outcome.message,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            partial_restart: outcome.partial_restart,
        }
    };
}
async fn perform(
    action: Action,
    clipboard: &Arc<Mutex<Option<arboard::Clipboard>>>,
    state: &Shared,
) -> Outcome {
    match action {
        Action::Restart => {
            let stopped = command(&["daemon", "stop"]).await;
            if !stopped.success {
                return restart_outcome(stopped, None);
            }
            let started = command(&["daemon", "start"]).await;
            restart_outcome(stopped, Some(started))
        }
        Action::Diagnostics => {
            let reply = command(Source::Doctor.args()).await;
            {
                let mut s = state.lock();
                s.tick(now());
                let gen = s.generation;
                s.apply(gen, Source::Doctor, reply.clone());
            }
            if let Err(e) = parse_json(Source::Doctor, &reply) {
                return Outcome::error("Couldn't collect diagnostics.", e);
            }
            let text = format!("{}\n{}", reply.stdout, reply.stderr);
            let mut clipboard = clipboard.lock();
            if clipboard.is_none() {
                match arboard::Clipboard::new() {
                    Ok(c) => *clipboard = Some(c),
                    Err(e) => {
                        return Outcome::error("Couldn't access the clipboard.", e.to_string())
                    }
                }
            }
            if let Err(e) = clipboard.as_mut().expect("clipboard").set_text(text) {
                return Outcome::error("Couldn't copy diagnostics.", e.to_string());
            }
            let mut outcome = Outcome::reply(reply, "Diagnostics copied.");
            if !outcome.success {
                outcome.message = "Diagnostics copied. The health check reported issues.".into();
            }
            outcome.success = true;
            outcome
        }
        Action::Logs => open_logs().await,
        _ => {
            let (args, message): (&[&str], &str) = match action {
                Action::Start => (&["daemon", "start"], "Soldr started."),
                Action::Stop => (&["daemon", "stop"], "Soldr stopped."),
                Action::Clean => (&["clean"], "Cache cleaned."),
                Action::Share(true) => (&["dist", "enable"], "Sharing requested."),
                Action::Share(false) => (&["dist", "disable"], "Stop sharing requested."),
                Action::Pair => (&["dist", "pair"], "Share this code with your other device."),
                _ => unreachable!(),
            };
            let reply = command(args).await;
            let message = if reply.success {
                message
            } else {
                match action {
                    Action::Start => "Couldn't start soldr.",
                    Action::Stop => "Couldn't stop soldr.",
                    Action::Clean => "Couldn't clean the cache.",
                    Action::Share(_) => {
                        "Couldn't confirm sharing. The last confirmed setting is shown."
                    }
                    Action::Pair => "Couldn't generate a pairing code.",
                    _ => "The action didn't finish.",
                }
            };
            Outcome::reply(reply, message)
        }
    }
}
async fn open_logs() -> Outcome {
    let reply = command(Source::Logs.args()).await;
    let value = match parse(Source::Logs, &reply) {
        Ok(v) => v,
        Err(e) => return Outcome::error("Couldn't locate the logs.", e),
    };
    let path = value["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == "zccache-embedded-logs" && p["exists"] == true)
        .and_then(|p| p["path"].as_str())
        .or_else(|| value["root"].as_str());
    let Some(path) = path else {
        return Outcome::error(
            "Couldn't locate the logs.",
            "No directory was reported.".into(),
        );
    };
    match open::that(path) {
        Ok(()) => Outcome::reply(reply, "Logs folder opened."),
        Err(e) => Outcome::error(
            "Couldn't open the logs folder.",
            format!("{e}\n{}\n{}", reply.stdout, reply.stderr),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coordinator_rejects_superseded_requests_within_an_action_generation() {
        let state = Arc::new(Mutex::new(State::default()));
        let reply = |running| Reply {
            success: true,
            stderr: String::new(),
            stdout: serde_json::json!({"running":running}).to_string(),
        };
        state.lock().apply(0, Source::Daemon, reply(true));
        let mut coordinator = Coordinator {
            state: state.clone(),
            policy: RefreshPolicy::default(),
            tasks: tokio::task::JoinSet::new(),
            requests: std::collections::BTreeMap::from([(Source::Daemon, 2)]),
            sequence: 2,
            clipboard: Arc::new(Mutex::new(None)),
            action_running: false,
        };
        coordinator.complete(Completion::Source {
            source: Source::Daemon,
            request: 1,
            epoch: 0,
            reply: reply(false),
            capability: None,
        });
        assert_eq!(state.lock().running(), Some(true));
        assert_eq!(coordinator.requests.get(&Source::Daemon), Some(&2));
        coordinator.complete(Completion::Source {
            source: Source::Daemon,
            request: 2,
            epoch: 0,
            reply: reply(false),
            capability: None,
        });
        assert_eq!(state.lock().running(), Some(false));
        state.lock().next_generation();
        coordinator.requests.insert(Source::Daemon, 3);
        coordinator.complete(Completion::Source {
            source: Source::Daemon,
            request: 3,
            epoch: 0,
            reply: reply(true),
            capability: None,
        });
        assert_eq!(state.lock().running(), Some(false));
    }
}
