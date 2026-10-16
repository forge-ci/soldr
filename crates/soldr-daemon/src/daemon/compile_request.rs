//! Shared argv → `CompileRequest` parser (soldr#2388 Step 6 / #2365 Q2).
//!
//! fable5's ruling: a SESSION `SessionStart` carries the raw rustc argv + env +
//! cwd, and the argv→`CompileRequest` conversion lives daemon-side in **one**
//! shared function so the `RUSTC_WRAPPER` client and the daemon's SESSION
//! codec-bridge can never disagree on parsing.
//!
//! - The wrapper reads its own `std::env` cwd/vars and calls
//!   [`build_compile_request_from`] (see `soldr_cli::compile_dispatch`).
//! - The SESSION endpoint (Step 6) passes the `SessionStart`'s carried cwd/env,
//!   never the daemon's own process env.
//!
//! The env is filtered by [`is_compile_env_var`] exactly as the legacy wrapper
//! path — the surface is never widened (fable5 Q2 condition 1). The filter is
//! idempotent, so passing an already-filtered env is safe.

use std::time::SystemTime;

use crate::daemon::protocol::{CompileLifecycle, CompileRequest};

/// Build a [`CompileRequest`] from a rustc-style argv (`argv[0]` is the rustc
/// path, `argv[1..]` the rustc arguments) plus an explicit `cwd` and `env`.
///
/// Taking `cwd`/`env` explicitly is what lets the daemon build a request from a
/// `SessionStart`'s carried environment rather than its own. `env` is filtered
/// by [`is_compile_env_var`]; pass the raw environment.
pub fn build_compile_request_from(
    rustc_argv: &[String],
    cwd: String,
    env: impl IntoIterator<Item = (String, String)>,
) -> CompileRequest {
    let env: Vec<(String, String)> = env
        .into_iter()
        .filter(|(k, _)| is_compile_env_var(k))
        .collect();
    let lifecycle = build_compile_lifecycle_from(rustc_argv, &env);
    CompileRequest {
        args: rustc_argv.to_vec(),
        cwd,
        env,
        stdin: Vec::new(),
        lifecycle,
        ipc_busy_retries: 0,
    }
}

/// Derive the build-history lifecycle from the (already-filtered) env. Both the
/// build-session id and `CARGO_TARGET_DIR` come from the request `env`, never
/// `std::env`, keeping the daemon path honest to the SessionStart (soldr#3644).
fn build_compile_lifecycle_from(
    rustc_argv: &[String],
    env: &[(String, String)],
) -> Option<CompileLifecycle> {
    let session_id = env
        .iter()
        .find(|(k, _)| k == soldr_cache::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR)?
        .1
        .parse::<u64>()
        .ok()?;
    let rustc_args = rustc_argv.get(1..).unwrap_or_default();
    let cargo_target_dir = env
        .iter()
        .find(|(k, _)| k == "CARGO_TARGET_DIR")
        .map(|(_, v)| std::ffi::OsStr::new(v.as_str()));
    let target_dir =
        // The client's CARGO_TARGET_DIR, never the daemon's own (soldr#3644);
        // no cwd: a relative value falls through to the out-dir walk.
        soldr_cache::cache_lib::target_registry::resolve_workspace_target_dir_with(
            rustc_args,
            cargo_target_dir,
            None,
        )
        .or_else(|| target_dir_from_cachedir_tag(rustc_args))?;
    Some(CompileLifecycle {
        session_id,
        crate_name: parse_crate_name(rustc_args)
            .unwrap_or("unknown")
            .to_string(),
        target_dir: target_dir.display().to_string(),
        started_at_ms: current_unix_ms(),
    })
}

/// Fallback for a relocated target dir not named `target` (e.g. a custom
/// `CARGO_TARGET_DIR` the client did not forward): walk up from `--out-dir`
/// to the nearest ancestor holding cargo's `CACHEDIR.TAG`.
fn target_dir_from_cachedir_tag(args: &[String]) -> Option<std::path::PathBuf> {
    let mut iter = args.iter();
    let mut out_dir = None;
    while let Some(arg) = iter.next() {
        if arg == "--out-dir" {
            out_dir = iter.next().cloned();
            break;
        }
        if let Some(rest) = arg.strip_prefix("--out-dir=") {
            out_dir = Some(rest.to_string());
            break;
        }
    }
    let out_dir = std::path::PathBuf::from(out_dir?);
    let found = out_dir
        .ancestors()
        .find(|dir| dir.join("CACHEDIR.TAG").is_file())?
        .to_path_buf();
    Some(std::fs::canonicalize(&found).unwrap_or(found))
}

fn parse_crate_name(args: &[String]) -> Option<&str> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--crate-name" {
            return args.next().map(String::as_str);
        }
        if let Some(value) = arg.strip_prefix("--crate-name=") {
            return Some(value);
        }
    }
    None
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Whether an environment variable is forwarded on the compile request.
///
/// A small denylist strips desktop/login-session and shell-prompt plumbing so
/// session noise never crosses the wire; everything else — including vars we
/// have never heard of — is forwarded, matching the standalone zccache wrapper
/// client's whole-env forwarding. zccache's fingerprint only hashes `CARGO_*`
/// vars, so the extra forwarded vars do not churn cache keys.
pub fn is_compile_env_var(name: &str) -> bool {
    // Desktop / login-session plumbing. Prefix matches.
    const NOISE_PREFIXES: &[&str] = &[
        "XDG_",     // XDG_SESSION_TYPE, XDG_RUNTIME_DIR, ...
        "GNOME_",   // GNOME_TERMINAL_SERVICE, ...
        "GDM",      // GDM_LANG, GDMSESSION
        "DESKTOP_", // DESKTOP_SESSION-adjacent
    ];
    if NOISE_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    // Shell-prompt / terminal-session state. Exact matches.
    !matches!(
        name,
        "PROMPT"
            | "PS1"
            | "PS2"
            | "PS4"
            | "OLDPWD"
            | "SHLVL"
            | "DISPLAY"
            | "WAYLAND_DISPLAY"
            | "DBUS_SESSION_BUS_ADDRESS"
            | "SESSION_MANAGER"
            | "LS_COLORS"
            | "LSCOLORS"
            | "PSModulePath"
            | "ChocolateyInstall"
            | "ChocolateyLastPathUpdate"
            | "WSL_DISTRO_NAME"
            | "WSL_INTEROP"
            | "WT_SESSION"
            | "WT_PROFILE_ID"
            | "WINDOWID"
            | "COLORTERM"
            | "VTE_VERSION"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use soldr_cache::cache_lib::target_registry::resolve_workspace_target_dir_with;

    #[test]
    fn supplied_cargo_target_dir_is_used_not_process_env() {
        let b = tempfile::tempdir().expect("tempdir");
        let got = resolve_workspace_target_dir_with(&[], Some(b.path().as_os_str()), None);
        assert_eq!(got, Some(std::fs::canonicalize(b.path()).unwrap()));
    }

    #[test]
    fn relative_cargo_target_dir_without_cwd_resolves_nothing() {
        let rel = std::ffi::OsStr::new("relative/target");
        assert_eq!(
            resolve_workspace_target_dir_with(&[], Some(rel), None),
            None
        );
    }

    fn session_env() -> (String, String) {
        (
            soldr_cache::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR.to_string(),
            "7".to_string(),
        )
    }

    #[test]
    fn lifecycle_target_dir_comes_from_request_env() {
        let b = tempfile::TempDir::new().unwrap();
        let env = vec![
            session_env(),
            (
                "CARGO_TARGET_DIR".to_string(),
                b.path().display().to_string(),
            ),
        ];
        let argv: Vec<String> = ["rustc", "--crate-name", "foo"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let lifecycle = build_compile_lifecycle_from(&argv, &env).unwrap();
        assert_eq!(
            lifecycle.target_dir,
            std::fs::canonicalize(b.path())
                .unwrap()
                .display()
                .to_string()
        );
    }

    #[test]
    fn lifecycle_target_dir_falls_back_to_out_dir_cachedir_tag() {
        let a = tempfile::TempDir::new().unwrap();
        std::fs::write(
            a.path().join("CACHEDIR.TAG"),
            b"Signature: 8a477f597d28d172789f06886806bc55",
        )
        .unwrap();
        let deps = a.path().join("debug").join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        let env = vec![session_env()];
        let argv: Vec<String> = vec![
            "rustc".into(),
            "--crate-name".into(),
            "foo".into(),
            "--out-dir".into(),
            deps.display().to_string(),
        ];
        let lifecycle = build_compile_lifecycle_from(&argv, &env).unwrap();
        assert_eq!(
            lifecycle.target_dir,
            std::fs::canonicalize(a.path())
                .unwrap()
                .display()
                .to_string()
        );
    }
}
