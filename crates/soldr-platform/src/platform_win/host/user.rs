//! Windows user identity and elevation.

use std::path::Path;
use std::process::Command;


/// The current user id. Windows has no uid concept in the paths the
/// callers build; the caller only consults this on the Unix branch.
pub fn uid() -> u32 {
    0
}

/// True when the current process holds an administrator token.
///
/// Probes a registry read of `HKLM\SECURITY` — only admin processes can
/// open this key, so a read failure means non-admin.
pub fn is_elevated() -> bool {
    crate::platform::wait::output_for(Command::new("reg").args(["query", r"HKLM\SECURITY"]), crate::platform::wait::PROBE)
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Relaunch the current binary elevated via UAC, blocking until the
/// elevated child exits. Returns the child's exit code or an error
/// explaining why elevation wasn't possible.
///
/// `helper_output_env` is the env var the parent injects so the helper
/// writes its JSON status somewhere the parent can read.
pub fn relaunch_elevated(
    powershell: &Path,
    args: &[String],
    helper_output_path: &Path,
    helper_output_env: &str,
) -> Result<i32, String> {
    let current =
        std::env::current_exe().map_err(|e| format!("failed to resolve current binary: {e}"))?;
    let arg_list = build_powershell_arg_list(args);
    // Why: `-Wait` blocks the parent until the elevated child exits, and
    // `-PassThru` lets us capture its exit code.
    let exe_quoted = ps_quote(&current.display().to_string());
    let output_env_quoted = ps_quote(&helper_output_path.display().to_string());
    let script = format!(
        "$env:{env_name}={out_lit}; $p = Start-Process -FilePath {exe} -ArgumentList {args} -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        env_name = helper_output_env,
        out_lit = output_env_quoted,
        exe = exe_quoted,
        args = arg_list,
    );
    // The elevated child runs the whole relaunched command.
    let status = crate::platform::wait::status_for(
        Command::new(powershell).args(["-NoProfile", "-Command", &script]),
        crate::platform::wait::LONG_CHILD,
    )
    .map_err(|e| format!("failed to relaunch elevated: {e}"))?;
    Ok(status.code().unwrap_or(1))
}

fn build_powershell_arg_list(args: &[String]) -> String {
    if args.is_empty() {
        // Why: PowerShell rejects empty Start-Process ArgumentList; pass
        // a single empty string so the elevated process gets argc == 1.
        return "@(' ')".to_string();
    }
    let parts: Vec<String> = args.iter().map(|a| ps_quote(a)).collect();
    format!("@({})", parts.join(","))
}

fn ps_quote(value: &str) -> String {
    let escaped = value.replace('\'', "''");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_quote_doubles_single_quotes() {
        assert_eq!(ps_quote("a'b"), "'a''b'");
    }

    #[test]
    fn build_powershell_arg_list_emits_array_literal() {
        assert_eq!(
            build_powershell_arg_list(&["a".to_string(), "b c".to_string()]),
            "@('a','b c')"
        );
        assert_eq!(build_powershell_arg_list(&[]), "@(' ')");
    }
}
