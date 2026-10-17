//! soldr#2546 — opt-in build process tracing (`soldr --debug cargo build`).
//!
//! First slice of the recursive-tree debugging surface: every child soldr
//! itself spawns on the cargo front door is announced on stderr with elapsed
//! time and PID, and appended as a JSONL event beside the build logs
//! (`<soldr root>/logs/debug-trace/<epoch-ms>-<pid>.jsonl`). Descendant
//! (grandchild) observation arrives with the running-process
//! `with_observer_and_command` seam (running-process#1023) in a later slice;
//! the JSONL schema here is the timeline that observer enriches.
//!
//! Off by default: with the flag absent, [`enabled`] is a single env read and
//! no file, thread, or buffer is created. The `--debug` flag publishes
//! [`DEBUG_TRACE_ENV_VAR`] (see `Cli::export_global_env`) so nested soldr
//! invocations inherit tracing.

use std::io::Write;
use std::process::{Child, Command, ExitStatus};

use super::nested_cargo_guard::{NestedCargoGuard, GUARD_TICK};
use crate::platform::sync::Mutex;
use std::sync::OnceLock;
use std::time::Instant;

pub(crate) const DEBUG_TRACE_ENV_VAR: &str = "SOLDR_DEBUG_TRACE";

pub(crate) fn enabled() -> bool {
    match std::env::var(DEBUG_TRACE_ENV_VAR) {
        Ok(value) => crate::core::flag_value(&value),
        Err(_) => false,
    }
}

/// Whether --debug requires the observed (spawn-owning) run mode to see
/// descendants on this host. Windows discovers descendants through the
/// Job Object IOCP wired at spawn, so the capture modes' post-hoc attach
/// (running-process#1026) observes nothing there; Unix monitors walk any
/// live pid and do not need the spawn.
pub(crate) fn observed_spawn_required() -> bool {
    enabled() && crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
}

/// Spawn `command`, emitting a `spawned` trace event when tracing is on.
///
/// The error type stays `std::io::Error` so call sites keep their existing
/// context-specific error mapping.
pub(crate) fn spawn_traced(command: &mut Command, context: &str) -> std::io::Result<Child> {
    if !enabled() {
        // soldr#3098: spawns share, staged writes exclude.
        let _spawn = crate::core::spawn_exclusion::spawn_shared();
        return command.spawn();
    }
    let argv = render_argv(command);
    let child = {
        let _spawn = crate::core::spawn_exclusion::spawn_shared();
        command.spawn()?
    };
    emit(
        context,
        &format!("spawned pid={} ({context}): {argv}", child.id()),
        &format!(
            r#"{{"event":"spawned","t_ms":{},"pid":{},"context":{},"argv":{}}}"#,
            elapsed_ms(),
            child.id(),
            json_string(context),
            json_string(&argv),
        ),
    );
    Ok(child)
}

/// Emit an `exited` trace event. Call after a traced child has been reaped.
pub(crate) fn child_exited(pid: u32, context: &str, status: &ExitStatus) {
    if !enabled() {
        return;
    }
    let code = status.code();
    let rendered = code.map_or_else(|| "signal".to_string(), |code| code.to_string());
    emit(
        context,
        &format!("exited pid={pid} ({context}) code={rendered}"),
        &format!(
            r#"{{"event":"exited","t_ms":{},"pid":{},"context":{},"exit_code":{}}}"#,
            elapsed_ms(),
            pid,
            json_string(context),
            code.map_or_else(|| "null".to_string(), |code| code.to_string()),
        ),
    );
}

fn render_argv(command: &Command) -> String {
    let mut parts = vec![command.get_program().to_string_lossy().into_owned()];
    parts.extend(
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned()),
    );
    parts.join(" ")
}

/// Minimal JSON string encoder for the trace lines. The values are
/// best-effort lossy renderings already; a full serializer dependency is
/// not warranted for two string fields.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn trace_epoch() -> &'static Instant {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now)
}

fn elapsed_ms() -> u128 {
    trace_epoch().elapsed().as_millis()
}

fn elapsed_secs_display() -> String {
    format!("{:.2}", trace_epoch().elapsed().as_secs_f64())
}

/// The JSONL sink, created lazily on the first traced event. `None` when the
/// file could not be created — tracing then degrades to stderr-only rather
/// than failing the build.
fn sink() -> &'static Option<Mutex<std::fs::File>> {
    static SINK: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    SINK.get_or_init(|| {
        let dir = match crate::core::SoldrPaths::new() {
            Ok(paths) => paths.root.join("logs").join("debug-trace"),
            Err(_) => return None,
        };
        if std::fs::create_dir_all(&dir).is_err() {
            return None;
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("{now_ms}-{}.jsonl", std::process::id()));
        match std::fs::File::create(&path) {
            Ok(mut file) => {
                let _ = writeln!(
                    file,
                    r#"{{"event":"anchor","epoch_ms":{now_ms},"soldr_pid":{}}}"#,
                    std::process::id()
                );
                eprintln!("soldr debug: process timeline -> {}", path.display());
                Some(Mutex::new(file))
            }
            Err(_) => None,
        }
    })
}

fn emit(_context: &str, human: &str, json: &str) {
    eprintln!("soldr debug: [+{}s] {human}", elapsed_secs_display());
    if let Some(file) = sink() {
        {
            let mut file = file.lock();
            let _ = writeln!(file, "{json}");
        }
    }
}

#[cfg(test)]
#[path = "debug_trace_tests.rs"]
mod tests;

/// soldr#2546 slice 2: run an inherited-stdio cargo child under the
/// running-process observer so the timeline includes descendants
/// (grandchildren: rustc, build scripts, linkers spawned by cargo).
///
/// Only the inherited-stdio front-door mode routes here, and only when
/// tracing is enabled — the capture modes keep slice 1's direct-child
/// timeline until their pipe plumbing is adapted. Timeout semantics
/// mirror `wait_for_cargo_child_with_heartbeat`: a heartbeat line per
/// interval, and on deadline the observed tree is killed through
/// running-process containment.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) fn run_observed_inheriting_stdio(
    command: &mut Command,
    context: &str,
    timeout: Option<std::time::Duration>,
    heartbeat: std::time::Duration,
    guard: Option<std::sync::Arc<NestedCargoGuard>>,
) -> Result<ExitStatus, crate::core::SoldrError> {
    use running_process::{
        CommandSpec, EventCategory, NativeProcess, ObserverConfig, ProcessConfig, StderrMode,
        StdinMode,
    };

    let argv = render_argv(command);
    // `with_observer_and_command` consumes the Command; the config's own
    // command field is ignored in favor of the override.
    let owned = std::mem::replace(command, Command::new("soldr-debug-trace-consumed"));
    let config = ProcessConfig {
        command: CommandSpec::Argv(vec!["soldr-debug-trace-override".to_string()]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    };
    let observer =
        ObserverConfig::with_categories([EventCategory::Lifecycle, EventCategory::Process]);
    let (process, subscriber) = NativeProcess::with_observer_and_command(owned, config, observer);
    {
        // soldr#3098: spawns share, staged writes exclude.
        let _spawn = crate::core::spawn_exclusion::spawn_shared();
        process.start().map_err(|err| {
            crate::core::SoldrError::Other(format!("spawn {context} failed: {err}"))
        })?;
    }
    let pid = process.pid().unwrap_or(0);
    if let Some(guard) = guard.as_ref() {
        guard.bind_root(pid);
    }
    emit(
        context,
        &format!("spawned pid={pid} ({context}, observed): {argv}"),
        &format!(
            r#"{{"event":"spawned","t_ms":{},"pid":{pid},"context":{},"argv":{},"observed":true}}"#,
            elapsed_ms(),
            json_string(context),
            json_string(&argv),
        ),
    );

    // Drain descendant events on a dedicated thread; it ends when the
    // process closes and the emitter side of the channel drops.
    // Shared counters feed the end-of-run summary event: descendants whose
    // exit was never observed are the "incomplete/unobserved exits" the
    // soldr#2546 acceptance list wants identified.
    let descendants_started = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let descendants_exited = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let started_counter = std::sync::Arc::clone(&descendants_started);
    let exited_counter = std::sync::Arc::clone(&descendants_exited);
    let pump_context = context.to_string();
    let pump_guard = guard.clone();
    let pump = std::thread::spawn(move || loop {
        match subscriber.recv_timeout(GUARD_TICK) {
            Ok(event) => {
                handle_descendant_event(&pump_context, &event, &started_counter, &exited_counter);
                if let Some(guard) = pump_guard.as_ref() {
                    guard.observe(&event);
                    guard.tick();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(guard) = pump_guard.as_ref() {
                    guard.tick();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    });

    let started = Instant::now();
    let mut next_heartbeat = heartbeat;
    let code = loop {
        if let Some(violation) = guard.as_ref().and_then(|guard| guard.violation()) {
            // soldr#2924: running-process containment owns this spawn, so its
            // kill takes the observed tree down.
            let _ = process.kill();
            let _ = process.wait(Some(std::time::Duration::from_secs(5)));
            let _ = process.close();
            drop(pump);
            NestedCargoGuard::kill_nested_if_alive(&violation);
            return Err(crate::core::SoldrError::Other(format!(
                "{}; the observed process tree was terminated",
                violation.message
            )));
        }
        let elapsed = started.elapsed();
        if let Some(limit) = timeout {
            if elapsed >= limit {
                let _ = process.kill();
                let _ = process.wait(Some(std::time::Duration::from_secs(5)));
                let _ = process.close();
                drop(pump);
                return Err(crate::core::SoldrError::Other(format!(
                    "{context} exceeded its {}s wall-clock deadline under --debug \
                     observation; the observed process tree was terminated",
                    limit.as_secs()
                )));
            }
        }
        let mut slice = next_heartbeat.saturating_sub(elapsed);
        if let Some(limit) = timeout {
            slice = slice.min(limit.saturating_sub(elapsed));
        }
        if guard.is_some() {
            slice = slice.min(GUARD_TICK);
        }
        match process.wait(Some(slice)) {
            Ok(code) => break code,
            Err(running_process::ProcessError::Timeout) => {
                if started.elapsed() >= next_heartbeat {
                    eprintln!(
                        "soldr: {context} still running after {}s (--debug observed)",
                        started.elapsed().as_secs()
                    );
                    next_heartbeat += heartbeat;
                }
            }
            Err(err) => {
                let _ = process.close();
                drop(pump);
                return Err(crate::core::SoldrError::Other(format!(
                    "wait on {context} failed: {err}"
                )));
            }
        }
    };
    // Give the descendant backend a beat to flush trailing exit events,
    // then detach the pump: it ends when `process` drops and the observer
    // emitter's channel closes. Joining here would deadlock — the emitter
    // stays alive as long as the process handle does, so `recv()` cannot
    // return `None` before this function's own scope ends.
    std::thread::sleep(std::time::Duration::from_millis(150));
    let _ = process.close();
    drop(pump);
    child_exited(
        pid,
        context,
        &crate::platform::process::exit::exit_status_from_code(code),
    );
    let started = descendants_started.load(std::sync::atomic::Ordering::Relaxed);
    let exited = descendants_exited.load(std::sync::atomic::Ordering::Relaxed);
    emit(
        context,
        &format!(
            "summary ({context}): descendants started={started} exited={exited} incomplete={}",
            started.saturating_sub(exited)
        ),
        &format!(
            r#"{{"event":"summary","t_ms":{},"context":{},"descendants_started":{started},"descendants_exited":{exited},"incomplete_exits":{}}}"#,
            elapsed_ms(),
            json_string(context),
            started.saturating_sub(exited),
        ),
    );
    Ok(crate::platform::process::exit::exit_status_from_code(code))
}

/// Shared descendant-event renderer for both observation modes.
fn handle_descendant_event(
    context: &str,
    event: &running_process::ObserverEvent,
    started: &std::sync::atomic::AtomicUsize,
    exited: &std::sync::atomic::AtomicUsize,
) {
    use running_process::ObserverEventKind;
    match event.kind {
        ObserverEventKind::DescendantStarted => {
            started.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let cmdline =
                running_process::observer::read_process_cmdline(event.pid).unwrap_or_default();
            // running-process#1025: the platform monitors report the
            // immediate parent where discovery knows it (Linux, macOS);
            // render 0 for unknown so the JSONL field stays numeric and
            // the tree edge stays reconstructible.
            let ppid = event.ppid.unwrap_or(0);
            emit(
                context,
                &format!(
                    "descendant-started pid={} ppid={ppid} ({context}): {cmdline}",
                    event.pid
                ),
                &format!(
                    r#"{{"event":"descendant-started","t_ms":{},"pid":{},"ppid":{ppid},"context":{},"cmdline":{}}}"#,
                    elapsed_ms(),
                    event.pid,
                    json_string(context),
                    json_string(&cmdline),
                ),
            );
        }
        ObserverEventKind::DescendantExited => {
            exited.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            emit(
                context,
                &format!("descendant-exited pid={} ({context})", event.pid),
                &format!(
                    r#"{{"event":"descendant-exited","t_ms":{},"pid":{},"context":{}}}"#,
                    elapsed_ms(),
                    event.pid,
                    json_string(context),
                ),
            );
        }
        // The direct child's Started/Exited are already covered by the
        // `spawned`/`exited` events this module emits itself.
        _ => {}
    }
}

fn emit_descendant_summary(
    context: &str,
    started: &std::sync::atomic::AtomicUsize,
    exited: &std::sync::atomic::AtomicUsize,
) {
    let started = started.load(std::sync::atomic::Ordering::Relaxed);
    let exited = exited.load(std::sync::atomic::Ordering::Relaxed);
    emit(
        context,
        &format!(
            "summary ({context}): descendants started={started} exited={exited} incomplete={}",
            started.saturating_sub(exited)
        ),
        &format!(
            r#"{{"event":"summary","t_ms":{},"context":{},"descendants_started":{started},"descendants_exited":{exited},"incomplete_exits":{}}}"#,
            elapsed_ms(),
            json_string(context),
            started.saturating_sub(exited),
        ),
    );
}

/// soldr#2546 slice 3: descendant observation for the capture front-door
/// modes, which own their pipe plumbing and therefore cannot route through
/// [`run_observed_inheriting_stdio`]'s owned spawn. running-process#1026's
/// `observe_launched_tree` attaches the per-OS descendant monitor to the
/// already-spawned cargo pid; timeline and summary match the
/// inherited-stdio mode's. Windows' monitor is spawn-tied upstream, so
/// this attach observes nothing there today.
pub(crate) struct DescendantObservation {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pump: std::thread::JoinHandle<()>,
    started: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    exited: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    context: String,
    traced: bool,
}

impl DescendantObservation {
    /// Attach to `pid` when tracing is enabled or a nested-Cargo `guard`
    /// (soldr#2924) needs the tree; `None` when neither does. One observer
    /// feeds both consumers.
    pub(crate) fn attach(
        pid: u32,
        context: &str,
        guard: Option<std::sync::Arc<NestedCargoGuard>>,
    ) -> Option<Self> {
        let traced = enabled();
        if !traced && guard.is_none() {
            return None;
        }
        if let Some(guard) = guard.as_ref() {
            guard.bind_root(pid);
        }
        // A guard that walks the tree itself (Linux) needs no observer; only
        // the timeline does. Without either, the pump is a plain ticker.
        let needs_observer = traced || !guard.as_ref().is_some_and(|guard| guard.walks_tree());
        let subscriber = needs_observer.then(|| {
            running_process::observer::observe_launched_tree(
                pid,
                running_process::ObserverConfig::with_categories([
                    running_process::EventCategory::Process,
                ]),
            )
        });
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let exited = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pump_stop = std::sync::Arc::clone(&stop);
        let started_counter = std::sync::Arc::clone(&started);
        let exited_counter = std::sync::Arc::clone(&exited);
        let pump_context = context.to_string();
        // The pump owns the subscriber: its Drop is what stops the
        // platform monitor, so the thread must end for teardown. It polls
        // the stop flag between events; `finish` sets the flag, so the
        // join below is bounded by one poll interval.
        let pump = std::thread::spawn(move || loop {
            let next = match subscriber.as_ref() {
                Some(subscriber) => subscriber.recv_timeout(GUARD_TICK),
                None => {
                    std::thread::sleep(GUARD_TICK);
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                }
            };
            match next {
                Ok(event) => {
                    if traced {
                        handle_descendant_event(
                            &pump_context,
                            &event,
                            &started_counter,
                            &exited_counter,
                        );
                    }
                    if let Some(guard) = guard.as_ref() {
                        guard.observe(&event);
                        guard.tick();
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if pump_stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    if let Some(guard) = guard.as_ref() {
                        guard.tick();
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        });
        Some(Self {
            stop,
            pump,
            started,
            exited,
            context: context.to_string(),
            traced,
        })
    }

    /// Flush trailing exit events, stop the monitor, and emit the summary.
    /// Call after the observed child has been reaped. Without tracing there
    /// is no timeline to flush: the pump is told to stop and left to exit on
    /// its own within one tick, so a guarded build pays no teardown wait.
    pub(crate) fn finish(self) {
        if !self.traced {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = crate::platform::wait::join_for(self.pump, crate::platform::wait::HELPER_JOIN);
        emit_descendant_summary(&self.context, &self.started, &self.exited);
    }
}
