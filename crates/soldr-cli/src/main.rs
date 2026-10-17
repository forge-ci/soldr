//! Thin binary shim (#1490 Phase 1). The whole CLI — mode detection,
//! clap dispatch, multicall — lives in the library crate
//! (`src/soldr_main.rs`), so the lib and bin no longer compile the
//! same ~40K LOC module tree twice per build.

/// Stack for the thread that actually runs the CLI (soldr#1802).
///
/// Windows gives the main thread 1 MiB. Building soldr's clap `Command` is
/// deeply recursive — every verb, every alias, every `global = true` arg
/// cloned into each subcommand — and in a debug build, where none of that
/// recursion is inlined away, it had come within **one argument** of
/// exhausting that 1 MiB: adding two `#[arg(long)] bool` fields to `Cli`
/// overflowed the stack before `main` could run, on every invocation
/// including `soldr --version`.
///
/// The failure mode is what makes this worth fixing rather than working
/// around. `thread 'main' has overflowed its stack` names no argument and
/// no subcommand, so it reads as memory corruption in soldr rather than
/// "the CLI outgrew its stack" — the misattribution class soldr#1999 is
/// about. It also reproduces only in debug, so release-only checks miss it.
const CLI_STACK_BYTES: usize = 16 * 1024 * 1024;

// Process-lifetime join on the CLI thread: it is the whole command, which
// may legitimately run indefinitely (`soldr cargo watch`) (soldr#3836).
#[cfg_attr(dylint_lib = "ban_raw_sync_primitive", allow(ban_raw_sync_primitive))]
fn main() -> std::process::ExitCode {
    // Run on a thread we size ourselves rather than the OS-provided main
    // stack. Spawning costs microseconds against a process about to link
    // or compile.
    match std::thread::Builder::new()
        .name("soldr-cli".into())
        .stack_size(CLI_STACK_BYTES)
        .spawn(soldr_cli::run)
    {
        Ok(handle) => match handle.join() {
            Ok(code) => code,
            // The CLI thread panicked; the default hook already reported
            // it. Re-panicking here would print a second, less
            // informative trace over the real one.
            Err(_) => std::process::ExitCode::FAILURE,
        },
        // If the thread cannot be spawned, fall back to the main stack:
        // every CLI path that fits still runs, and refusing to start
        // would be worse than the overflow this exists to prevent.
        Err(_) => soldr_cli::run(),
    }
}
