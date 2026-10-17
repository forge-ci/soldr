//! soldr#2924 — fail fast when a direct nested Cargo would self-lock.
//!
//! A build script (or a compiler it drives) that runs
//! `Command::new($CARGO) build` against the outer build's target directory
//! waits forever: the outer Cargo holds the build-directory lock for its whole
//! compile phase, and the nested Cargo blocks on it. The nested process runs
//! the real toolchain Cargo, never Soldr, so the `IN_SOLDR_PID` entry guard
//! ([`crate::reentrancy_guard`]) is structurally unable to see it — that guard
//! owns nested *Soldr* entries; this one owns direct Cargo descendants that
//! bypass Soldr entirely.
//!
//! # What counts as a hazard
//!
//! The front door already observes its Cargo child's tree through the
//! running-process descendant monitor (the same backend `--debug` tracing
//! uses; one observer per run feeds both). A descendant is a hazard when all
//! of these hold:
//!
//! 1. it is a Cargo invocation whose verb may take the build lock
//!    ([`super::nested_cargo::classify_cargo_descendant`]; unknown verbs and
//!    aliases fail closed);
//! 2. its ancestry reaches a *lock-held phase* — a build script or compiler
//!    whose parent is a Cargo process — with no Soldr process in between. A
//!    Soldr process owns its own subtree: the nested front door runs its own
//!    guard and the re-entrancy guard;
//! 3. its command line does not prove a target directory distinct from every
//!    such lock holder's. Only an explicit `--target-dir` is proof; the lock
//!    holder's target is read from the build script's own path (or the
//!    compiler's `--out-dir`), which always lies inside it. An env-only
//!    `CARGO_TARGET_DIR` or a `--config` override is not observable portably
//!    and is never guessed as safe.
//!
//! Anchoring on the lock-held phase, rather than on "any Cargo below the
//! direct child", keeps three legitimate shapes out: `cargo clippy` running
//! `cargo check` through `$CARGO` (no build script between them), nested Soldr
//! fixtures (a Soldr boundary), and test bodies that build a scratch project
//! (a test binary is not a lock-held phase — Cargo releases the build lock
//! before it runs test binaries, verified on Linux with Cargo 1.88/1.94/1.98).
//!
//! # Modes
//!
//! `SOLDR_NESTED_CARGO` on the outer invocation selects the response:
//! unset / `enforce` terminates the Cargo tree and fails the run with exit 1;
//! `report` records and warns without terminating; `allow` is the explicit
//! permit for a nested build isolated some way the observer cannot prove.
//! Every hazard writes a redacted record under `<soldr root>/logs/nested-cargo/`.
//! The front door strips the variable from Cargo's environment, so the permit
//! is scoped to one outer Cargo run and never reaches a nested Soldr entry.
//!
//! # Platform coverage
//!
//! Linux and macOS report each descendant's parent pid, which the ancestry
//! rule needs. Windows discovers descendants through the Job Object wired at
//! spawn: the post-hoc attach used by the capture modes observes nothing there
//! and its events carry no parent pid, so the guard is not armed on Windows.

use crate::platform::sync::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::nested_cargo::{
    bounded_head, classify_cargo_descendant, executable_stem, CargoDescendant, LockingCargo,
    TargetDirClaim,
};

/// Outer-invocation control for the guard. Stripped from Cargo's environment.
pub(crate) const NESTED_CARGO_ENV_VAR: &str = "SOLDR_NESTED_CARGO";
/// How often the pump re-examines the observed tree when no event arrives.
pub(crate) const GUARD_TICK: Duration = Duration::from_millis(100);
/// A freshly forked process can still carry its parent's argv until it
/// execs; argv is re-read this long after a process is first seen.
const EXEC_SETTLE_WINDOW: Duration = Duration::from_secs(1);
const AUDIT_SCHEMA_VERSION: u32 = 1;

/// The guard's response to a hazard, from [`NESTED_CARGO_ENV_VAR`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardMode {
    /// Terminate the Cargo tree and fail the run (the default).
    Enforce,
    /// Record and warn, never terminate.
    Report,
    /// The explicit permit: record, note, and let the nested build run.
    Allow,
}

impl GuardMode {
    /// Parse the variable's value. An unrecognized value fails closed to
    /// [`GuardMode::Enforce`] and returns a warning naming the valid values.
    pub(crate) fn parse(raw: Option<&str>) -> (Self, Option<String>) {
        let Some(raw) = raw else {
            return (Self::Enforce, None);
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "enforce" => (Self::Enforce, None),
            "report" => (Self::Report, None),
            "allow" => (Self::Allow, None),
            _ => (
                Self::Enforce,
                Some(format!(
                    "soldr warning: {NESTED_CARGO_ENV_VAR}={raw:?} is not recognized \
                     (expected `enforce`, `report`, or `allow`); enforcing"
                )),
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Enforce => "enforce",
            Self::Report => "report",
            Self::Allow => "allow",
        }
    }
}

/// The raw [`NESTED_CARGO_ENV_VAR`] value this front door consumed.
static CONSUMED_MODE_ENV: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Read the outer invocation's mode and remove the variable from this
/// process's environment before anything is spawned, so no Cargo — the build
/// itself or a metadata probe — and no nested Soldr entry below it inherits
/// the permit. Call once at front-door entry; later calls return the same
/// mode. An unrecognized value warns and enforces.
pub(crate) fn consume_mode_env() -> GuardMode {
    let raw = CONSUMED_MODE_ENV.get_or_init(|| {
        let raw = std::env::var(NESTED_CARGO_ENV_VAR).ok();
        if raw.is_some() {
            std::env::remove_var(NESTED_CARGO_ENV_VAR);
        }
        raw
    });
    let (mode, warning) = GuardMode::parse(raw.as_deref());
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    mode
}

/// Hand the consumed mode to a Soldr-to-Soldr retry of the *same* outer run
/// (the `-Zthreads` and no-cache retries), which is still that run's scope.
pub(crate) fn forward_mode_to_front_door_retry(command: &mut std::process::Command) {
    if let Some(Some(raw)) = CONSUMED_MODE_ENV.get() {
        command.env(NESTED_CARGO_ENV_VAR, raw);
    }
}

/// What an observed process is, as far as the guard is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Role {
    /// argv could not be read (yet).
    Unknown,
    /// A Soldr process: its subtree belongs to its own front door and guards.
    Soldr,
    /// Cargo. `None` when the command cannot take the build lock.
    Cargo(Option<LockingCargo>),
    /// A build script; `evidence` is its absolute executable path, which
    /// lies inside the running Cargo's build directory.
    BuildScript { evidence: Option<PathBuf> },
    /// A compiler invocation (rustc, or a wrapper in front of it); `evidence`
    /// is its absolute `--out-dir`, inside the running Cargo's target.
    Compiler { evidence: Option<PathBuf> },
    /// Anything else.
    Other,
}

impl Role {
    /// Classify one observed argument vector (`argv[0]` is the executable).
    pub(crate) fn of(argv: &[String]) -> Self {
        let Some(exe) = argv.first() else {
            return Self::Unknown;
        };
        let args = &argv[1..];
        let stem = executable_stem(exe).to_ascii_lowercase();
        if stem == "soldr" || stem.starts_with("soldr-") {
            return Self::Soldr;
        }
        match classify_cargo_descendant(exe, args) {
            CargoDescendant::NotCargo => {}
            CargoDescendant::NonLocking => return Self::Cargo(None),
            CargoDescendant::Locking(locking) => return Self::Cargo(Some(locking)),
        }
        if stem.starts_with("build-script-") {
            return Self::BuildScript {
                evidence: absolute(exe),
            };
        }
        // A doctest run is `rustdoc --test`: it executes after Cargo's
        // compile phase, so it is not a lock-held phase.
        if stem == "rustdoc" && args.iter().any(|arg| arg == "--test") {
            return Self::Other;
        }
        if args.iter().any(|arg| arg == "--crate-name") {
            return Self::Compiler {
                evidence: flag_value(args, "--out-dir").and_then(absolute),
            };
        }
        Self::Other
    }

    fn is_cargo(&self) -> bool {
        matches!(self, Self::Cargo(_))
    }

    fn hop_kind(&self) -> Option<&'static str> {
        match self {
            Self::BuildScript { .. } => Some("build script"),
            Self::Compiler { .. } => Some("compiler"),
            _ => None,
        }
    }

    fn hop_evidence(&self) -> Option<&Path> {
        match self {
            Self::BuildScript { evidence } | Self::Compiler { evidence } => evidence.as_deref(),
            _ => None,
        }
    }
}

fn absolute(path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    path.is_absolute().then_some(path)
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().map(String::as_str);
        }
        if let Some(value) = arg
            .strip_prefix(flag)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return Some(value);
        }
    }
    None
}

#[derive(Debug, Clone)]
struct Node {
    ppid: Option<u32>,
    argv: Option<Vec<String>>,
    role: Role,
    first_seen: Instant,
}

/// Why a nested Cargo cannot be proven isolated from the lock holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HazardReason {
    NoTargetDir,
    ConfigTargetDir,
    UnresolvableTargetDir,
    OuterTargetUnknown,
    SameTarget,
}

impl HazardReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::NoTargetDir => "no_target_dir",
            Self::ConfigTargetDir => "config_target_dir",
            Self::UnresolvableTargetDir => "unresolvable_target_dir",
            Self::OuterTargetUnknown => "outer_target_unknown",
            Self::SameTarget => "same_target_dir",
        }
    }

    fn explain(self) -> &'static str {
        match self {
            Self::NoTargetDir => {
                "it names no --target-dir, so it resolves the outer build's target \
                 (an env-only CARGO_TARGET_DIR is not observable and is not trusted)"
            }
            Self::ConfigTargetDir => {
                "its target directory comes from a --config override, which is not \
                 trusted as proof of isolation"
            }
            Self::UnresolvableTargetDir => {
                "its relative --target-dir cannot be resolved without the process's \
                 working directory"
            }
            Self::OuterTargetUnknown => "the lock holder's target directory is not observable",
            Self::SameTarget => "its --target-dir is the outer build's target directory",
        }
    }
}

/// A nested Cargo that will wait on an ancestor Cargo's build lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hazard {
    pub(crate) nested_pid: u32,
    /// The Cargo holding the lock (the nearest build script's parent).
    pub(crate) lock_holder_pid: u32,
    /// The build script or compiler between the lock holder and the nested
    /// Cargo.
    pub(crate) phase_pid: u32,
    pub(crate) phase_kind: &'static str,
    pub(crate) exe_name: String,
    pub(crate) verb: String,
    pub(crate) head: String,
    pub(crate) reason: HazardReason,
}

/// The guard's verdict on one observed process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Assessment {
    /// Not a lock-taking Cargo invocation, or a Cargo that Cargo itself
    /// spawned (a fork that has not exec'd yet).
    NotCandidate,
    /// Ancestry is not fully observed yet; assess again on the next tick.
    Pending,
    /// A Soldr process owns this subtree.
    SoldrBoundary,
    /// No build script or compiler under a Cargo above it: no lock is held.
    NoLockHeld,
    /// An explicit `--target-dir` distinct from every lock holder's target.
    DistinctTargetDir,
    Hazard(Hazard),
}

/// The observed descendant tree of the front door's Cargo child.
#[derive(Debug)]
pub(crate) struct ProcessTree {
    root_pid: u32,
    nodes: HashMap<u32, Node>,
}

impl ProcessTree {
    pub(crate) fn new(root_pid: u32) -> Self {
        Self {
            root_pid,
            nodes: HashMap::new(),
        }
    }

    pub(crate) fn started(&mut self, pid: u32, ppid: Option<u32>, argv: Option<Vec<String>>) {
        self.started_at(pid, ppid, argv, Instant::now());
    }

    fn started_at(&mut self, pid: u32, ppid: Option<u32>, argv: Option<Vec<String>>, at: Instant) {
        let role = argv.as_deref().map_or(Role::Unknown, Role::of);
        self.nodes.insert(
            pid,
            Node {
                ppid,
                argv,
                role,
                first_seen: at,
            },
        );
    }

    pub(crate) fn exited(&mut self, pid: u32) {
        self.nodes.remove(&pid);
    }

    /// Re-read argv where it can still change meaningfully: processes seen
    /// within [`EXEC_SETTLE_WINDOW`] (a fork that has not exec'd shows its
    /// parent's argv), and every process below a lock-held phase, where an
    /// exec into Cargo is exactly the hazard.
    fn refresh(&mut self, now: Instant, read_argv: &dyn Fn(u32) -> Option<Vec<String>>) {
        let pids: Vec<u32> = self
            .nodes
            .iter()
            .filter(|(pid, node)| {
                now.duration_since(node.first_seen) < EXEC_SETTLE_WINDOW
                    || self.below_lock_held_phase(**pid)
            })
            .map(|(pid, _)| *pid)
            .collect();
        for pid in pids {
            let Some(argv) = read_argv(pid) else {
                continue;
            };
            let Some(node) = self.nodes.get_mut(&pid) else {
                continue;
            };
            if node.argv.as_ref() != Some(&argv) {
                node.role = Role::of(&argv);
                node.argv = Some(argv);
            }
        }
    }

    /// Rebuild the tree from a direct child enumeration of the root's
    /// descendants (the Linux feed, see [`NestedCargoGuard::walks_tree`]).
    /// Compiler subtrees are not descended into: a compiler's children are
    /// linkers, and only an exotic proc macro would launch Cargo there. Soldr
    /// subtrees are not either: [`Self::assess`] stops at a Soldr boundary,
    /// and that Soldr's own front door guards what is below it — which keeps
    /// the walk small under suites that spawn many nested Soldr commands.
    fn walk(
        &mut self,
        children_of: &dyn Fn(u32) -> Option<Vec<u32>>,
        read_argv: &dyn Fn(u32) -> Option<Vec<String>>,
    ) {
        let mut current: HashMap<u32, u32> = HashMap::new();
        let mut stack = vec![self.root_pid];
        while let Some(pid) = stack.pop() {
            let opaque = self
                .nodes
                .get(&pid)
                .is_some_and(|node| matches!(node.role, Role::Compiler { .. } | Role::Soldr));
            if opaque {
                continue;
            }
            for child in children_of(pid).unwrap_or_default() {
                if child != self.root_pid && current.insert(child, pid).is_none() {
                    stack.push(child);
                }
            }
        }
        self.nodes.retain(|pid, _| current.contains_key(pid));
        for (pid, ppid) in current {
            let known = self
                .nodes
                .get(&pid)
                .is_some_and(|node| node.ppid == Some(ppid));
            if !known {
                self.started(pid, Some(ppid), read_argv(pid));
            }
        }
    }

    fn is_cargo(&self, pid: u32) -> bool {
        pid == self.root_pid
            || self
                .nodes
                .get(&pid)
                .is_some_and(|node| node.role.is_cargo())
    }

    /// Whether `node` is a lock-held phase: a build script or compiler whose
    /// parent is a Cargo process.
    fn is_lock_held_phase(&self, node: &Node) -> bool {
        node.role.hop_kind().is_some() && node.ppid.is_some_and(|ppid| self.is_cargo(ppid))
    }

    fn below_lock_held_phase(&self, pid: u32) -> bool {
        let mut cur = self.nodes.get(&pid).and_then(|node| node.ppid);
        let mut steps = 0;
        while let Some(ancestor) = cur {
            if ancestor == self.root_pid || steps > self.nodes.len() {
                return false;
            }
            let Some(node) = self.nodes.get(&ancestor) else {
                return false;
            };
            if self.is_lock_held_phase(node) {
                return true;
            }
            cur = node.ppid;
            steps += 1;
        }
        false
    }

    fn candidates(&self) -> Vec<u32> {
        self.nodes
            .iter()
            .filter(|(_, node)| matches!(node.role, Role::Cargo(Some(_))))
            .map(|(pid, _)| *pid)
            .collect()
    }

    /// Assess one observed process. `cwd_of` answers a process's working
    /// directory (only consulted for a relative `--target-dir`).
    pub(crate) fn assess(&self, pid: u32, cwd_of: &dyn Fn(u32) -> Option<PathBuf>) -> Assessment {
        let Some(node) = self.nodes.get(&pid) else {
            return Assessment::NotCandidate;
        };
        let Role::Cargo(Some(locking)) = &node.role else {
            return Assessment::NotCandidate;
        };
        let Some(parent) = node.ppid else {
            return Assessment::Pending;
        };
        // Cargo never launches `cargo` itself; a Cargo-looking child of Cargo
        // is a fork that has not exec'd its real image yet.
        if self.is_cargo(parent) {
            return Assessment::NotCandidate;
        }
        struct Phase<'a> {
            pid: u32,
            holder: u32,
            kind: &'static str,
            evidence: Option<&'a Path>,
        }
        let mut phases: Vec<Phase<'_>> = Vec::new();
        let mut cur = parent;
        let mut steps = 0;
        while cur != self.root_pid {
            let Some(ancestor) = self.nodes.get(&cur) else {
                return Assessment::Pending;
            };
            match &ancestor.role {
                Role::Soldr => return Assessment::SoldrBoundary,
                Role::Unknown => return Assessment::Pending,
                role => {
                    if let (Some(kind), Some(holder)) = (role.hop_kind(), ancestor.ppid) {
                        if self.is_cargo(holder) {
                            phases.push(Phase {
                                pid: cur,
                                holder,
                                kind,
                                evidence: role.hop_evidence(),
                            });
                        }
                    }
                }
            }
            let Some(next) = ancestor.ppid else {
                return Assessment::Pending;
            };
            steps += 1;
            if steps > self.nodes.len() {
                return Assessment::Pending;
            }
            cur = next;
        }
        if phases.is_empty() {
            return Assessment::NoLockHeld;
        }
        let hazard = |reason, phase: &Phase<'_>| {
            let args = node.argv.as_deref().map(|argv| &argv[1..]).unwrap_or(&[]);
            Assessment::Hazard(Hazard {
                nested_pid: pid,
                lock_holder_pid: phase.holder,
                phase_pid: phase.pid,
                phase_kind: phase.kind,
                exe_name: node
                    .argv
                    .as_ref()
                    .and_then(|argv| argv.first())
                    .map(|exe| {
                        exe.rsplit(['/', '\\'])
                            .next()
                            .unwrap_or(exe.as_str())
                            .to_string()
                    })
                    .unwrap_or_default(),
                verb: locking.verb.clone(),
                head: bounded_head(args),
                reason,
            })
        };
        let nearest = &phases[0];
        let (path, change_dir) = match &locking.target_dir {
            TargetDirClaim::Absent => return hazard(HazardReason::NoTargetDir, nearest),
            TargetDirClaim::ConfigOverride => {
                return hazard(HazardReason::ConfigTargetDir, nearest)
            }
            TargetDirClaim::Explicit { path, change_dir } => (path, change_dir),
        };
        let Some(target) = resolve_target_dir(path, change_dir.as_deref(), || cwd_of(pid)) else {
            return hazard(HazardReason::UnresolvableTargetDir, nearest);
        };
        let target = canonicalish(&target);
        for phase in &phases {
            let Some(evidence) = phase.evidence else {
                return hazard(HazardReason::OuterTargetUnknown, phase);
            };
            // The lock holder's target is an ancestor of the evidence path,
            // so a target that is the evidence or one of its ancestors may be
            // the very directory being held.
            if canonicalish(evidence).starts_with(&target) {
                return hazard(HazardReason::SameTarget, phase);
            }
        }
        Assessment::DistinctTargetDir
    }
}

/// Resolve an explicit `--target-dir` the way Cargo does: relative to `-C`,
/// which is itself relative to the process's working directory.
fn resolve_target_dir(
    path: &str,
    change_dir: Option<&str>,
    cwd: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    let base = match change_dir.map(Path::new) {
        Some(dir) if dir.is_absolute() => dir.to_path_buf(),
        Some(dir) => cwd()?.join(dir),
        None => cwd()?,
    };
    Some(base.join(path))
}

/// Canonicalize the longest existing prefix of `path` (resolving symlinks and
/// junctions) and append the rest, so a target directory Cargo has not
/// created yet still compares against real paths.
pub(crate) fn canonicalish(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(mut resolved) = std::fs::canonicalize(existing) {
            for name in rest.iter().rev() {
                resolved.push(name);
            }
            return resolved;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return lexical_normalize(path),
        }
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A hazard the guard acted on in [`GuardMode::Enforce`].
#[derive(Debug, Clone)]
pub(crate) struct Violation {
    pub(crate) hazard: Hazard,
    nested_start_token: Option<u64>,
    pub(crate) message: String,
}

#[derive(Debug, Default)]
struct GuardState {
    tree: Option<ProcessTree>,
    handled: HashSet<u32>,
    violation: Option<Violation>,
    last_refresh: Option<Instant>,
}

/// One outer Cargo run's nested-Cargo guard. Shared between the observer
/// pump (which feeds and ticks it) and the waiter (which enforces it).
#[derive(Debug)]
pub(crate) struct NestedCargoGuard {
    mode: GuardMode,
    /// Whether the tree is rebuilt from direct child enumeration each tick
    /// instead of from observer events. See [`Self::walks_tree`].
    walk: bool,
    audit_dir: Option<PathBuf>,
    tripped: AtomicBool,
    state: Mutex<GuardState>,
}

impl NestedCargoGuard {
    /// The guard for this front-door run, or `None` on a host whose observer
    /// cannot report descendant ancestry (see the module docs).
    pub(crate) fn for_front_door(mode: GuardMode) -> Option<Arc<Self>> {
        use crate::platform::host::facts::{os, HostOs};
        if os() == HostOs::Windows {
            return None;
        }
        let audit_dir = crate::core::SoldrPaths::new()
            .ok()
            .map(|paths| paths.root.join("logs").join("nested-cargo"));
        let walk = crate::platform::process::inspect::child_pids(std::process::id()).is_some();
        Some(Arc::new(Self::new(mode, walk, audit_dir)))
    }

    pub(crate) fn new(mode: GuardMode, walk: bool, audit_dir: Option<PathBuf>) -> Self {
        Self {
            mode,
            walk,
            audit_dir,
            tripped: AtomicBool::new(false),
            state: Mutex::new(GuardState::default()),
        }
    }

    fn state(&self) -> crate::platform::sync::MutexGuard<'_, GuardState> {
        self.state.lock()
    }

    /// Whether this guard feeds itself by enumerating children rather than
    /// consuming observer events.
    ///
    /// running-process 4.10.14's Linux descendant monitor reads only
    /// `/proc/<pid>/task/<pid>/children`, the main thread's list. Cargo
    /// launches build scripts and compilers from job threads, so that monitor
    /// never sees them or anything below them — the whole class this guard
    /// exists for. Where the platform can enumerate a process's children
    /// across all of its threads (Linux), the guard walks the tree itself,
    /// bounded to the observed Cargo tree; elsewhere (macOS, whose monitor
    /// diffs whole-system snapshots) it consumes the observer's events. The
    /// walk should give way to the observer once the upstream monitor reads
    /// every task (running-process#1221).
    pub(crate) fn walks_tree(&self) -> bool {
        self.walk
    }

    /// Start tracking the tree below the spawned Cargo child `root_pid`.
    pub(crate) fn bind_root(&self, root_pid: u32) {
        self.state().tree = Some(ProcessTree::new(root_pid));
    }

    /// Feed one observer event.
    pub(crate) fn observe(&self, event: &running_process::ObserverEvent) {
        use running_process::ObserverEventKind;
        if self.walk {
            return;
        }
        match event.kind {
            ObserverEventKind::DescendantStarted => {
                let argv = read_argv(event.pid);
                if let Some(tree) = self.state().tree.as_mut() {
                    tree.started(event.pid, event.ppid, argv);
                }
            }
            ObserverEventKind::DescendantExited => {
                if let Some(tree) = self.state().tree.as_mut() {
                    tree.exited(event.pid);
                }
            }
            _ => {}
        }
    }

    /// Re-examine the tree. Called by the pump after each event batch and at
    /// least every [`GUARD_TICK`].
    pub(crate) fn tick(&self) {
        if self.tripped.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        let mut state = self.state();
        let refresh_due = state
            .last_refresh
            .is_none_or(|last| now.duration_since(last) >= GUARD_TICK);
        if refresh_due {
            state.last_refresh = Some(now);
            let walk = self.walk;
            if let Some(tree) = state.tree.as_mut() {
                if walk {
                    tree.walk(&crate::platform::process::inspect::child_pids, &read_argv);
                }
                tree.refresh(now, &read_argv);
            }
        }
        let Some(tree) = state.tree.as_ref() else {
            return;
        };
        let root_pid = tree.root_pid;
        let hazards: Vec<(Hazard, Duration)> = tree
            .candidates()
            .into_iter()
            .filter(|pid| !state.handled.contains(pid))
            .filter_map(|pid| match tree.assess(pid, &working_directory) {
                Assessment::Hazard(hazard) => {
                    let seen = tree.nodes[&pid].first_seen;
                    Some((hazard, now.duration_since(seen)))
                }
                _ => None,
            })
            .collect();
        for (hazard, after) in hazards {
            state.handled.insert(hazard.nested_pid);
            let audit = self.write_audit(root_pid, &hazard, after);
            match self.mode {
                GuardMode::Enforce => {
                    let message = enforce_message(root_pid, &hazard, audit.as_deref());
                    state.violation = Some(Violation {
                        nested_start_token: crate::platform::process::inspect::process_start_token(
                            hazard.nested_pid,
                        ),
                        hazard,
                        message,
                    });
                    self.tripped.store(true, Ordering::Release);
                    return;
                }
                GuardMode::Report => eprintln!(
                    "soldr warning: {} ({NESTED_CARGO_ENV_VAR}=report: not terminated)",
                    describe(root_pid, &hazard)
                ),
                GuardMode::Allow => eprintln!(
                    "soldr: nested Cargo pid {} (`{}`) permitted by {NESTED_CARGO_ENV_VAR}=allow \
                     although {}",
                    hazard.nested_pid,
                    hazard.head,
                    hazard.reason.explain()
                ),
            }
        }
    }

    /// The enforced violation, once the guard has tripped.
    pub(crate) fn violation(&self) -> Option<Violation> {
        if !self.tripped.load(Ordering::Acquire) {
            return None;
        }
        self.state().violation.clone()
    }

    /// After the Cargo tree was killed, make sure the nested Cargo is gone
    /// too — it may have left the process group. Identity-checked so a
    /// recycled pid is never signalled.
    pub(crate) fn kill_nested_if_alive(violation: &Violation) {
        use crate::platform::process::inspect::{is_alive, process_start_token};
        let pid = violation.hazard.nested_pid;
        if violation.nested_start_token.is_some()
            && is_alive(pid)
            && process_start_token(pid) == violation.nested_start_token
        {
            let _ = crate::platform::process::terminate::signal_pid(pid, true);
        }
    }

    fn write_audit(&self, root_pid: u32, hazard: &Hazard, after: Duration) -> Option<PathBuf> {
        let dir = self.audit_dir.as_ref()?;
        std::fs::create_dir_all(dir).ok()?;
        let unix_ms = super::current_unix_ms();
        let action = match self.mode {
            GuardMode::Enforce => "terminated",
            GuardMode::Report => "reported",
            GuardMode::Allow => "permitted",
        };
        let record = serde_json::json!({
            "schema_version": AUDIT_SCHEMA_VERSION,
            "event": "nested_cargo_self_lock",
            "unix_ms": unix_ms,
            "action": action,
            "mode": self.mode.as_str(),
            "permit": (self.mode == GuardMode::Allow).then_some("allow"),
            "outer_cargo_pid": root_pid,
            "lock_holder_pid": hazard.lock_holder_pid,
            "phase_pid": hazard.phase_pid,
            "phase_kind": hazard.phase_kind,
            "nested_pid": hazard.nested_pid,
            "nested_exe": hazard.exe_name,
            "verb": hazard.verb,
            "head": hazard.head,
            "reason": hazard.reason.as_str(),
            "detected_after_ms": after.as_millis() as u64,
        });
        let path = dir.join(format!(
            "{unix_ms}-{}-{}.json",
            std::process::id(),
            hazard.nested_pid
        ));
        std::fs::write(&path, format!("{record}\n")).ok()?;
        Some(path)
    }
}

fn describe(root_pid: u32, hazard: &Hazard) -> String {
    format!(
        "nested Cargo pid {} (`{} {}`) was started by {} pid {} of Cargo pid {} \
         (outer Cargo pid {root_pid}) with no Soldr process in between, and {}; it would \
         wait on that Cargo's target lock forever",
        hazard.nested_pid,
        hazard.exe_name,
        hazard.head,
        hazard.phase_kind,
        hazard.phase_pid,
        hazard.lock_holder_pid,
        hazard.reason.explain(),
    )
}

fn enforce_message(root_pid: u32, hazard: &Hazard, audit: Option<&Path>) -> String {
    let mut message = format!(
        "{}. soldr terminated the Cargo process tree instead (soldr#2924). Give the nested \
         build a distinct absolute --target-dir, run it through `soldr cargo`, or set \
         {NESTED_CARGO_ENV_VAR}=allow on the outer command if it is isolated another way",
        describe(root_pid, hazard)
    );
    if let Some(audit) = audit {
        message.push_str(&format!("; audit record: {}", audit.display()));
    }
    message
}

fn read_argv(pid: u32) -> Option<Vec<String>> {
    running_process::observer::read_process_argv(pid)
        .ok()
        .filter(|argv| !argv.is_empty())
        .map(|argv| {
            argv.iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        })
}

fn working_directory(pid: u32) -> Option<PathBuf> {
    crate::platform::process::inspect::working_directory(pid)
}

/// Enforce a tripped guard against a std-spawned Cargo child: kill the tree,
/// reap it, make sure the nested Cargo is gone, and return the diagnostic.
pub(crate) fn teardown_std_child(
    child: &mut std::process::Child,
    context: &str,
    violation: &Violation,
) -> crate::core::SoldrError {
    use wait_timeout::ChildExt;
    let mut message = violation.message.clone();
    match super::kill_cargo_process_tree(child) {
        Ok(detail) => message.push_str(&format!("; {detail}")),
        Err(err) => message.push_str(&format!("; kill failed: {err}")),
    }
    match child.wait_timeout(Duration::from_secs(super::KILLED_CARGO_REAP_TIMEOUT_SECS)) {
        Ok(Some(status)) => super::debug_trace::child_exited(child.id(), context, &status),
        Ok(None) => message.push_str(&format!(
            "; process did not exit within {} seconds after kill",
            super::KILLED_CARGO_REAP_TIMEOUT_SECS
        )),
        Err(err) => message.push_str(&format!("; reap after kill failed: {err}")),
    }
    NestedCargoGuard::kill_nested_if_alive(violation);
    crate::core::SoldrError::Other(message)
}

#[cfg(test)]
#[path = "nested_cargo_guard_tests.rs"]
mod tests;
