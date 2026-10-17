//! Per-phase startup breadcrumbs for the CLI front door (soldr#2571).
//!
//! [`crate::broker_bringup`] did this for `soldr broker serve` after soldr#2493
//! showed that a broker which hangs between "starting" and "started" is
//! undiagnosable. soldr#2571 is the same failure one process to the left:
//! `doctor --json` wedged for the full 15 s test budget having written **zero
//! bytes to stdout and stderr**, with the fixture's `broker-spawn.log` proving
//! the broker came up in 86 ms and `daemon-spawn.log` proving the daemon was
//! serving. Both peers were exonerated and the client still could not say what
//! it was doing, because the front door announces nothing until the command
//! body produces output — and `doctor` produces its first byte only at the
//! closing `print_json`.
//!
//! So every startup boundary reports the moment it is crossed, eagerly, and a
//! wedged process leaves behind a trail whose **last line names the phase that
//! was entered but never finished**.
//!
//! # Why stderr only, and no durable log
//!
//! [`crate::broker_bringup`] also writes `broker-bringup.jsonl`, because a
//! production broker's stderr goes to a log nobody reads interactively. The
//! front door is the opposite: its stderr is right there in the terminal, and
//! in CI it is captured by whatever spawned it (`run_soldr_with_timeout`
//! already dumps both streams into its panic message).
//!
//! More decisively, a file sink would have to resolve `~/.soldr` first — and
//! home resolution, path canonicalization, and broker-directory creation are
//! themselves candidate wedge phases here. **A tracer must not depend on the
//! machinery it is tracing.** `eprintln!` needs nothing but the inherited fd.
//!
//! # Why this is not `SOLDR_PROFILE_STARTUP`
//!
//! [`crate::startup_profile`] measures the same kind of thing but prints its
//! breakdown from `finish()`, at the end. A process that never reaches the end
//! prints nothing at all, which is precisely the soldr#2571 signature. This
//! module trades the tidy summary for a line per boundary as it happens.
//!
//! # Contract notes
//!
//! * `total_ms` is measured from the **first** [`phase`] call, not from process
//!   start: the `main.rs` re-spawn onto the big-stack thread and argv collection
//!   precede it. That gap is sub-millisecond and unmeasurable from here anyway;
//!   what matters is that *no* trace lines at all still means "wedged before the
//!   first mark", which is a diagnosis of its own.
//! * Output is opt-in via `SOLDR_STARTUP_TRACE`, so it does **not** violate the
//!   soldr#2554 rule that `--json` / `--shell-export` output must stay
//!   machine-parseable when stdout and stderr are merged. Setting the variable
//!   *is* the caller's consent to extra stderr; an unset variable is byte-for-
//!   byte the old behavior.
//! * For the same reason the marks in `broker_spawn::ensure_stable_broker_ready`
//!   are **not** gated on that module's `diagnostics_eligible`. That flag
//!   suppresses unasked-for warnings on `--json` / `--shell-export` invocations
//!   — which is precisely the shape that wedged in soldr#2571, so folding these
//!   marks into it would silence the one case they exist to explain. Do not
//!   "fix" the asymmetry; it is the point.

use crate::platform::sync::Mutex;
use std::sync::OnceLock;
use std::time::Instant;

/// Opt-in switch. Scoped name per the `SOLDR_<SCOPE>_<TRACE|DEBUG>` convention
/// already used by `SOLDR_DEBUG_TRACE` / `SOLDR_DAEMON_TRACE` /
/// `SOLDR_BROKER_DEBUG`.
pub const STARTUP_TRACE_ENV_VAR: &str = "SOLDR_STARTUP_TRACE";

/// Phase labels. Fixed strings rather than ad-hoc literals at each call site so
/// the trace stays greppable and a renamed phase is a compile-time change.
pub mod phase {
    // Front-door entry (`soldr_main::run` / `run_main`).
    pub const REENTRANCY_GUARD: &str = "reentrancy_guard";
    pub const MULTICALL_DISPATCH: &str = "multicall_dispatch";
    pub const SELF_RELOCATE: &str = "self_relocate";
    pub const BROKER_CONTROL_TRANSPORT: &str = "broker_control_transport";
    pub const BROKER_FRONT_DOOR: &str = "broker_front_door";
    pub const VERSION_PIN: &str = "version_pin";
    pub const GLOBAL_UPGRADE: &str = "global_upgrade";
    pub const TOKIO_RUNTIME: &str = "tokio_runtime";
    pub const CLAP_PARSE: &str = "clap_parse";
    /// The command body itself -- everything `run_cli` does.
    ///
    /// soldr#2785: without this the trace stops at `clap_parse`, so the last
    /// line of a wedged or slow process names argument parsing no matter where
    /// the time actually went. A `gc list` poll measured at ~278ms reported 5ms
    /// of traced startup and nothing to account for the rest, and the natural
    /// reading of that trace -- the one I made -- was that startup was the
    /// cost.
    pub const COMMAND_DISPATCH: &str = "command_dispatch";

    // Cargo front door. These opt-in diagnostics distinguish slow pre-Cargo
    // setup from the timeout that begins only after Cargo is spawned.
    pub const CARGO_FRONT_DOOR_ENTERED: &str = "cargo_front_door_entered";
    pub const CARGO_FRONT_DOOR_TOOLCHAIN_RESOLVED: &str = "cargo_front_door_toolchain_resolved";
    pub const CARGO_FRONT_DOOR_PRE_SPAWN: &str = "cargo_front_door_pre_spawn";

    // Broker readiness (`broker_spawn::ensure_stable_broker_ready`).
    pub const BROKER_ENDPOINT_RESOLVE: &str = "broker_endpoint_resolve";
    pub const BROKER_OWNER_DIRECTORIES: &str = "broker_owner_directories";
    pub const BROKER_PROBE_RUNTIME: &str = "broker_probe_runtime";
    pub const BROKER_ADMIN_PROBE: &str = "broker_admin_probe";
    pub const BROKER_LEASE: &str = "broker_lease";
    pub const BROKER_IMAGE_HASH: &str = "broker_image_hash";
    pub const BROKER_STAGE_IMAGE: &str = "broker_stage_image";
    pub const BROKER_SPAWN_WAIT: &str = "broker_spawn_wait";

    // `doctor` body — every collector runs before the single closing
    // `print_json`, so all of it is inside the silent window soldr#2571 saw.
    pub const DOCTOR_MANIFEST: &str = "doctor_manifest";
    pub const DOCTOR_ZCCACHE_BUNDLE: &str = "doctor_zccache_bundle";
    pub const DOCTOR_DEBUG_INFO: &str = "doctor_debug_info";
    pub const DOCTOR_DEFENDER_PROBE: &str = "doctor_defender_probe";
    pub const DOCTOR_COOK_STATS: &str = "doctor_cook_stats";
    pub const DOCTOR_FALLBACK_ROLLUP: &str = "doctor_fallback_rollup";
    pub const DOCTOR_CACHE_HEALTH: &str = "doctor_cache_health";
    pub const DOCTOR_BROKER_INVENTORY: &str = "doctor_broker_inventory";
}

struct TraceClock {
    started: Instant,
    phase_started: Instant,
}

/// `None` once resolved-and-disabled, so the disabled path costs one atomic
/// load per mark after the first — no env lookup, no `Instant::now()`.
static CLOCK: OnceLock<Option<Mutex<TraceClock>>> = OnceLock::new();

/// Whether `value` turns the trace on. Presence alone is not enough: a CI
/// matrix that exports `SOLDR_STARTUP_TRACE=0` for its quiet lanes must get
/// silence. Mirrors `cargo_front_door::debug_trace::enabled`.
/// Does `value` turn the trace on? (soldr#2740)
///
/// `SOLDR_STARTUP_TRACE` is soldr-owned, so it takes the allowlist rule from
/// `soldr_core::core::env_flag` -- an unrecognised value is off. Presence
/// alone is still not enough: a CI matrix that exports
/// `SOLDR_STARTUP_TRACE=0` for its quiet lanes must get silence.
fn value_enables(value: &str) -> bool {
    crate::core::flag_value(value)
}

fn clock() -> Option<&'static Mutex<TraceClock>> {
    CLOCK
        .get_or_init(|| {
            let enabled = std::env::var(STARTUP_TRACE_ENV_VAR)
                .map(|value| value_enables(&value))
                .unwrap_or(false);
            enabled.then(|| {
                let now = Instant::now();
                Mutex::new(TraceClock {
                    started: now,
                    phase_started: now,
                })
            })
        })
        .as_ref()
}

/// Record that `name` just finished, and start timing the next phase.
///
/// A no-op unless [`STARTUP_TRACE_ENV_VAR`] is set to an enabling value.
/// Never panics: a poisoned lock loses the line rather than the process.
pub(crate) fn phase(name: &str) {
    let Some(clock) = clock() else {
        return;
    };
    let mut clock = clock.lock();
    let now = Instant::now();
    let phase_ms = now.duration_since(clock.phase_started).as_millis();
    let total_ms = now.duration_since(clock.started).as_millis();
    clock.phase_started = now;
    // Drop the guard before writing: `eprintln!` takes its own stderr lock, and
    // holding two locks in one order here and the other order anywhere else is
    // how a diagnostic becomes the deadlock it was meant to diagnose.
    drop(clock);
    eprintln!("{}", render_line(name, phase_ms, total_ms));
}

/// Render one trace line. Pure so its shape is unit-testable without I/O or
/// environment state.
///
/// The `soldr front-door:` prefix is deliberately distinct from the broker's
/// `soldr broker: bringup phase=…`, which `pep517_daemon_smoke.py` and
/// `index_build_logs.py` grep for.
fn render_line(phase: &str, phase_ms: u128, total_ms: u128) -> String {
    format!("soldr front-door: startup phase={phase} ms={phase_ms} total_ms={total_ms}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_carries_the_phase_and_both_timings() {
        let line = render_line(phase::BROKER_ADMIN_PROBE, 17, 950);
        assert_eq!(
            line,
            "soldr front-door: startup phase=broker_admin_probe ms=17 total_ms=950"
        );
        // One record per line: a newline would split one phase into two.
        assert!(!line.contains('\n'));
    }

    #[test]
    fn the_prefix_cannot_be_confused_with_the_brokers_bringup_lines() {
        // `pep517_daemon_smoke.py` and `index_build_logs.py` key on the
        // broker's own lines; a front-door mark must not match them.
        let line = render_line(phase::TOKIO_RUNTIME, 1, 2);
        assert!(!line.contains("soldr broker:"));
        assert!(!line.contains("bringup phase="));
    }

    #[test]
    fn only_enabling_values_switch_the_trace_on() {
        // soldr#2740: `SOLDR_STARTUP_TRACE` is soldr-owned, so it takes the
        // allowlist rule -- `verbose` used to enable and no longer does.
        for value in ["1", "true", "yes", "on", "TRUE", " 1 "] {
            assert!(value_enables(value), "{value} should enable");
        }
        // Explicitly-off spellings and blanks stay silent, so a lane can export
        // the variable unconditionally and still opt out.
        for value in ["", "  ", "0", "false", "FALSE", "no", "off", "Off"] {
            assert!(!value_enables(value), "{value:?} should not enable");
        }
    }

    #[test]
    fn marking_without_the_env_var_is_a_no_op() {
        // The dominant production path: no panic, no output, no clock.
        phase(phase::CLAP_PARSE);
    }
}
