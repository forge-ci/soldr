//! Does rustc itself supply the lld for this link? (soldr#3797)
//!
//! Since Rust 1.90, `x86_64-unknown-linux-gnu` links with the toolchain's
//! self-contained `rust-lld` by default: rustc adds
//! `-B<sysroot>/lib/rustlib/<host>/bin/gcc-ld` to the driver command line, so
//! `clang -fuse-ld=lld` resolves `ld.lld` to rustc's shim even when the host
//! has no lld of its own. When that holds, a system clang is a sufficient
//! linker driver and soldr need not download a managed LLVM just to own an lld
//! that rustc would not use.
//!
//! The decision is made from the target triple string and the host triple
//! soldr already computes -- never from `cfg!`. Any doubt answers `false`,
//! which keeps the soldr#3430 path (fetch the managed LLVM) unchanged.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

/// The only triple that qualifies, as target and as host.
const SUPPORTED_TRIPLE: &str = "x86_64-unknown-linux-gnu";
/// First rustc release that defaults this triple to the self-contained lld.
const MIN_RUSTC: (u32, u32) = (1, 90);
/// Upper bound for the one `rustc -V` probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// `(major, minor)` from `rustc 1.98.1 (hash date)` or a bare `1.98.1`.
pub(crate) fn parse_rustc_version(text: &str) -> Option<(u32, u32)> {
    let token = text
        .split_whitespace()
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor))
}

/// The pure predicate. `rustflags` holds each rustflags source verbatim
/// (whitespace- or `\x1f`-separated).
pub(crate) fn rustc_supplies_lld(
    target: &str,
    host: &str,
    rustc_version: Option<(u32, u32)>,
    rustflags: &[String],
) -> bool {
    target == SUPPORTED_TRIPLE
        && host == SUPPORTED_TRIPLE
        && rustc_version.is_some_and(|version| version >= MIN_RUSTC)
        && !rustflags.iter().any(|flags| opts_out_of_rustc_lld(flags))
}

/// Does this rustflags string disable rustc's bundled lld? Looks at every
/// `-C`/`--codegen`/`-Z` option in all spellings (`-C x`, `-Cx`,
/// `--codegen=x`, `--codegen x`).
fn opts_out_of_rustc_lld(flags: &str) -> bool {
    let tokens: Vec<&str> = flags
        .split(|c: char| c.is_whitespace() || c == '\x1f')
        .filter(|token| !token.is_empty())
        .collect();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        index += 1;
        let option = if matches!(token, "-C" | "--codegen" | "-Z") {
            let next = tokens.get(index).copied();
            index += 1;
            next
        } else if let Some(rest) = token.strip_prefix("--codegen=") {
            Some(rest)
        } else {
            token
                .strip_prefix("-C")
                .or_else(|| token.strip_prefix("-Z"))
        };
        if option.is_some_and(option_disables_lld) {
            return true;
        }
    }
    false
}

fn option_disables_lld(option: &str) -> bool {
    let Some((name, value)) = option.split_once('=') else {
        return false;
    };
    match name {
        "link-self-contained" => {
            matches!(value, "no" | "n" | "off" | "false")
                || value.split(',').any(|part| part.starts_with('-'))
        }
        // `-lld` removes lld; `-C linker-features=-lld` / `-Zlinker-features`.
        "linker-features" => value.split(',').any(|part| part.starts_with('-')),
        _ => false,
    }
}

/// The active rustc's `(major, minor)`, probed once per process through the
/// already-resolved rustc. `None` on any failure or timeout.
fn active_rustc_version() -> Option<(u32, u32)> {
    static VERSION: OnceLock<Option<(u32, u32)>> = OnceLock::new();
    *VERSION.get_or_init(|| {
        let rustc = crate::binaries::resolve_toolchain_binary("rustc").ok()?;
        probe_rustc_version(&rustc)
    })
}

fn probe_rustc_version(rustc: &Path) -> Option<(u32, u32)> {
    let mut command = Command::new(rustc);
    command.arg("-V");
    crate::core::suppress_windows_console_window(&mut command);
    let output = crate::core::command_output_with_timeout_duration(
        &mut command,
        "rustc -V (linker driver resolution)",
        PROBE_TIMEOUT,
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_rustc_version(&String::from_utf8_lossy(&output.stdout))
}

const RUSTFLAGS_KEYS: [&str; 3] = [
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
];

/// [`rustc_supplies_lld`] for the cargo child `command` will become: the
/// rustflags it will see (its own env first, the process env otherwise), the
/// target-scoped variable for `target`, and the injection's own `rustflags`.
/// The version probe runs only after every cheaper test has passed.
pub(crate) fn for_command(
    target: &str,
    command: &Command,
    injection_rustflags: Option<&str>,
) -> bool {
    let host = crate::platform::host::facts::triple();
    if target != SUPPORTED_TRIPLE || host != SUPPORTED_TRIPLE {
        return false;
    }
    let target_key = format!(
        "CARGO_TARGET_{}_RUSTFLAGS",
        crate::linker::cargo_target_env_prefix(target)
    );
    let mut flags: Vec<String> = RUSTFLAGS_KEYS
        .iter()
        .copied()
        .chain(std::iter::once(target_key.as_str()))
        .filter_map(|key| crate::toolchain_dir_name::effective_command_env(command, key))
        .map(|value: OsString| value.to_string_lossy().into_owned())
        .collect();
    flags.extend(injection_rustflags.map(str::to_string));
    if flags.iter().any(|flags| opts_out_of_rustc_lld(flags)) {
        return false;
    }
    rustc_supplies_lld(target, host, active_rustc_version(), &flags)
}

#[cfg(test)]
#[path = "rustc_lld_tests.rs"]
mod tests;
