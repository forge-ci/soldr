//! Cargo front-door hook (soldr#3896).
//!
//! [`open`] runs before the front door does any work. For an invocation an
//! adapter recognises as a pure check it derives the key; a stored verdict
//! is replayed and the check never starts (no toolchain resolution, no
//! daemon, no `target/`). Otherwise the gate arms, the check runs normally,
//! and [`Armed::finish`] records the verdict if it passed.

use std::path::{Path, PathBuf};

use super::context::Context;
use super::key::Derivation;
use super::profiles;
use super::record::{Record, SCHEMA};
use super::store::{Seen, Store, DEFAULT_MAX_RECORDS};
use super::Mode;

/// Set while a gated check runs so nested front doors (build scripts or
/// tools re-entering `soldr cargo`) are not gated a second time.
pub const NESTED_ENV: &str = "SOLDR_VERDICT_NESTED";

/// `1` prints why each invocation was or was not gated.
pub const DEBUG_ENV: &str = "SOLDR_VERDICT_DEBUG";

static CAPTURED: crate::platform::sync::Mutex<Option<String>> =
    crate::platform::sync::Mutex::new(None);

/// The front door hands over the check's captured diagnostic stream here.
pub(crate) fn note_output(output: Option<&str>) {
    *CAPTURED.lock() = output.map(strip_cargo_status);
}

fn take_output() -> Option<String> {
    CAPTURED.lock().take()
}

/// Drop cargo's progress lines; they describe the original run's work, not
/// its verdict, and would be misleading in a replay.
pub(crate) fn strip_cargo_status(output: &str) -> String {
    const STATUS: &[&str] = &[
        "Compiling",
        "Checking",
        "Finished",
        "Fresh",
        "Blocking",
        "Downloading",
        "Downloaded",
        "Updating",
        "Locking",
        "Adding",
        "Documenting",
        "Building",
    ];
    output
        .lines()
        .filter(|line| {
            let first = line.split_whitespace().next().unwrap_or("");
            !STATUS.contains(&first)
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

pub(crate) enum Gate {
    /// Not a gated check; run normally.
    Pass,
    /// A verdict was replayed; exit with this code without running.
    Replayed(i32),
    /// Run, then call [`Armed::finish`].
    Armed(Box<Armed>),
}

pub(crate) struct Armed {
    store: Store,
    derivation: Derivation,
    mode: Mode,
    /// Per-member keys this run covers, recorded alongside the workspace
    /// verdict on a pass.
    crate_keys: Vec<String>,
    /// The narrowed argv to run instead of the caller's, when only some
    /// members changed.
    args_override: Option<Vec<String>>,
}

fn debug(message: impl FnOnce() -> String) {
    if std::env::var_os(DEBUG_ENV).is_some_and(|v| v == "1") {
        eprintln!("soldr: verdict: {}", message());
    }
}

fn cwd() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

/// Gate a front-door invocation.
pub(crate) fn open(args: &[String]) -> Gate {
    let mode = Mode::from_env();
    if std::env::var_os(NESTED_ENV).is_some() {
        return Gate::Pass;
    }
    let gate = open_top_level(args, mode);
    // An identity probe runs with the ledger off; it is not job work.
    if matches!(gate, Gate::Pass) && mode != Mode::Off {
        note_ungated(args);
    }
    gate
}

/// Log a top-level front-door run the ledger did not gate.
fn note_ungated(args: &[String]) {
    let (Some(cwd), Some(store)) = (cwd(), Store::default_location()) else {
        return;
    };
    let _ = store.note_seen(&Seen {
        tool: String::new(),
        args: args.to_vec(),
        cwd: cwd.display().to_string(),
        key: String::new(),
        hit: false,
        env: None,
    });
}

fn open_top_level(args: &[String], mode: Mode) -> Gate {
    if mode == Mode::Off {
        return Gate::Pass;
    }
    // Only a recognised subcommand pays for a git snapshot.
    let any_adapter = profiles::subcommand_of(args).is_some_and(|sub| {
        profiles::builtin().iter().any(|p| p.subcommand == sub) || has_user_rows()
    });
    if !any_adapter {
        return Gate::Pass;
    }
    let (Some(cwd), Some(store)) = (cwd(), Store::default_location()) else {
        return Gate::Pass;
    };
    let derived = Context::discover(&cwd)
        .and_then(|ctx| ctx.derive(args, &cwd).map(|derivation| (ctx, derivation)));
    let (ctx, derivation) = match derived {
        Ok(pair) => pair,
        Err(reason) => {
            debug(|| format!("running: {reason}"));
            return Gate::Pass;
        }
    };
    let rel_cwd = derivation.cwd.clone();
    let seen = |hit| Seen {
        tool: derivation.tool.clone(),
        args: args.to_vec(),
        cwd: rel_cwd.clone(),
        key: derivation.key.clone(),
        hit,
        env: Some(derivation.selected_env()),
    };
    let (seen_hit, seen_miss) = (seen(true), seen(false));
    if let Some(record) = store.lookup(&derivation.key) {
        let _ = store.note_seen(&seen_hit);
        replay(&record);
        return Gate::Replayed(0);
    }
    // soldr#3897: a workspace run whose members mostly kept their verdicts
    // runs only the members that did not.
    let crate_keys = ctx.derive_crates(args, &cwd).unwrap_or_default();
    let missing: Vec<String> = crate_keys
        .iter()
        .filter(|(_, key)| store.lookup(key).is_none())
        .map(|(name, _)| name.clone())
        .collect();
    let mut armed = Armed {
        store,
        derivation,
        mode,
        crate_keys: Vec::new(),
        args_override: None,
    };
    if !crate_keys.is_empty() && missing.is_empty() {
        let _ = armed.store.note_seen(&seen_hit);
        eprintln!(
            "soldr: {} skipped: every one of {} workspace member(s) matches a passing run",
            armed.derivation.tool,
            crate_keys.len()
        );
        // Every member passed under these inputs, so the workspace did too.
        armed.record_all(None);
        return Gate::Replayed(0);
    }
    let _ = armed.store.note_seen(&seen_miss);
    debug(|| format!("miss {}", armed.derivation.key));
    if !missing.is_empty() && missing.len() < crate_keys.len() {
        eprintln!(
            "soldr: {}: {} of {} workspace member(s) changed; checking only {}",
            armed.derivation.tool,
            missing.len(),
            crate_keys.len(),
            missing.join(", ")
        );
        armed.args_override = Some(super::crates::narrow(args, &missing));
    }
    armed.crate_keys = missing
        .iter()
        .filter_map(|name| crate_keys.get(name).cloned())
        .collect();
    std::env::set_var(NESTED_ENV, "1");
    Gate::Armed(Box::new(armed))
}

fn has_user_rows() -> bool {
    let Some(cwd) = cwd() else {
        return false;
    };
    cwd.ancestors()
        .find(|dir| dir.join("Cargo.toml").is_file() && dir.join(".git").exists())
        .map(profiles::workspace_config)
        .is_some_and(|config| !config.tools.is_empty())
}

fn replay(record: &Record) {
    if let Some(output) = &record.output {
        eprint!("{output}");
    }
    let note = if record.output_captured {
        ""
    } else {
        " (no output was recorded for it: the run wrote to a terminal, or was assembled from per-member verdicts)"
    };
    eprintln!(
        "soldr: {} skipped: inputs match a passing run (verdict {}){note}; SOLDR_VERDICTS=off forces a run",
        record.tool, record.key
    );
}

impl Armed {
    /// The argv the front door should run, when the gate narrowed it.
    pub(crate) fn args_override(&self) -> Option<&[String]> {
        self.args_override.as_deref()
    }

    /// Record the verdict if the check passed.
    pub(crate) fn finish(self, exit_code: i32) {
        std::env::remove_var(NESTED_ENV);
        let output = take_output();
        if exit_code != 0 {
            return;
        }
        let partial = self.args_override.is_some();
        // A narrowed run's output covers only the members it checked.
        self.record_all(if partial { None } else { output.as_deref() });
    }

    fn record_all(self, output: Option<&str>) {
        if self.mode != Mode::On {
            return;
        }
        let keys =
            std::iter::once(self.derivation.key.clone()).chain(self.crate_keys.iter().cloned());
        for (index, key) in keys.enumerate() {
            let record = self.record(key, if index == 0 { output } else { None });
            match self.store.put(&record) {
                Ok(()) => debug(|| format!("recorded {}", record.key)),
                Err(err) => debug(|| format!("could not record {}: {err}", record.key)),
            }
        }
        self.store.gc(DEFAULT_MAX_RECORDS);
    }

    fn record(&self, key: String, output: Option<&str>) -> Record {
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Record {
            v: SCHEMA,
            key,
            tool: self.derivation.tool.clone(),
            argv: self.derivation.normalized_args.clone(),
            cwd: self.derivation.cwd.clone(),
            toolchain: self.derivation.toolchain.clone(),
            soldr: env!("CARGO_PKG_VERSION").to_string(),
            exit: 0,
            created,
            output: None,
            output_captured: false,
        }
        .with_output(output)
    }
}
