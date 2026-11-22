//! Invocation priority shared by the CLI and daemon transport.

/// Forwarded to child processes and carried in the compile session envelope.
/// The daemon consumes it before passing the environment to zccache.
pub const ENV: &str = "SOLDR_LINT_PRIORITY";

/// Cache root in which an ancestor holds resident capacity.
pub const RESIDENT_ENV: &str = "SOLDR_RESIDENT_CAPACITY_ROOT";

/// Opaque embedded zccache class understood by Soldr's host scheduler.
pub const HOST_CLASS: u32 = 1;

/// Ask the native Nextest wrapper to acquire ordinary capacity for each test.
pub const NEXTEST_DAEMON_ADMISSION_ENV: &str = "SOLDR_NEXTEST_DAEMON_ADMISSION";

/// Cache root whose Nextest execution is supervised by `soldr ci-test`.
pub const NEXTEST_STAGE_ROOT_ENV: &str = "SOLDR_NEXTEST_STAGE_CAPACITY_ROOT";
const TRACE_ENV: &str = "SOLDR_LINT_ADMISSION_TRACE";

/// Enable daemon-side queue and artifact observations for a measured run.
pub fn trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os(TRACE_ENV).is_some_and(|value| value == "1"))
}

/// Use the same absolute root for markers across working directories and
/// symlink aliases. A route may still be starting when the marker is set.
pub fn root_marker(root: &std::path::Path) -> std::io::Result<String> {
    std::fs::canonicalize(root)
        .or_else(|_| std::path::absolute(root))
        .map(|path| path.to_string_lossy().into_owned())
}

/// Consume the scheduling marker at the compile boundary. It must never
/// become a compiler environment input or a cache key.
pub fn take_from_compile_env(env: &mut Vec<(String, String)>) -> bool {
    let mut lint = false;
    env.retain(|(key, value)| {
        if matches!(
            key.as_str(),
            RESIDENT_ENV | NEXTEST_STAGE_ROOT_ENV | NEXTEST_DAEMON_ADMISSION_ENV | TRACE_ENV
        ) {
            return false;
        }
        if key != ENV {
            return true;
        }
        lint |= value == "1";
        false
    });
    lint
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_is_consumed_without_changing_compiler_inputs() {
        for (marker, expected) in [("1", true), ("0", false), ("other", false)] {
            let inputs = vec![("RUSTFLAGS".to_string(), "-Cdebuginfo=0".to_string())];
            let mut env = inputs.clone();
            env.push((ENV.to_string(), marker.to_string()));
            env.push((RESIDENT_ENV.to_string(), "/cache".to_string()));
            env.push((NEXTEST_STAGE_ROOT_ENV.to_string(), "/cache".to_string()));
            env.push((NEXTEST_DAEMON_ADMISSION_ENV.to_string(), "1".to_string()));
            env.push((TRACE_ENV.to_string(), "1".to_string()));
            assert_eq!(take_from_compile_env(&mut env), expected);
            assert_eq!(env, inputs);
            assert!(!take_from_compile_env(&mut env));
        }
    }
}
