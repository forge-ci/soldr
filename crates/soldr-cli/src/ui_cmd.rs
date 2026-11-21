//! The UI is a library inside the single soldr executable.
use crate::core::SoldrError;
use std::path::PathBuf;

#[derive(clap::Args, Debug)]
pub(crate) struct UiArgs {
    #[arg(long)]
    pub window: bool,
    /// Render panel and dashboard without running live Soldr commands.
    #[arg(long, requires = "fixture")]
    pub screenshot: Option<PathBuf>,
    #[arg(long)]
    pub fixture: Option<PathBuf>,
    #[arg(long)]
    pub hotkey: Option<String>,
    /// Manual large-text fallback for system preferences.
    #[arg(long, default_value_t = 1.0)]
    pub text_scale: f32,
    #[arg(long, default_value = "overview", value_parser = ["overview", "builds", "nodes", "diagnostics"])]
    pub tab: String,
}

/// Identify the built-in before global delegation or broker startup.
pub(crate) fn is_invocation(raw_args: &[String]) -> bool {
    crate::broker_spawn::first_command_positional(raw_args).is_some_and(|arg| arg == "ui")
}

#[cfg(feature = "ui")]
pub(crate) fn run(args: UiArgs) -> Result<i32, SoldrError> {
    // Materialize an argv[0] alias for future tray/autostart consumers. It is
    // the same executable image, never an independently built GUI binary.
    let executable = if args.fixture.is_none() {
        let paths = crate::core::SoldrPaths::new()?;
        crate::binaries::ui_shim_binary(&paths)?;
        Some(crate::binaries::ui_cli_binary(&paths)?)
    } else {
        None
    };
    let options = soldr_ui::Options {
        executable,
        window: args.window,
        screenshot: args.screenshot,
        fixture: args.fixture,
        hotkey: args.hotkey,
        text_scale: args.text_scale,
        tab: args.tab,
    };
    // This handler stays on the main thread, which native event loops require.
    // Background work shares soldr's process-lifetime runtime (no nested runtime).
    soldr_ui::run(options, &tokio::runtime::Handle::current())
        .map(|()| 0)
        .map_err(|error| SoldrError::Other(format!("soldr ui: {error}")))
}

#[cfg(not(feature = "ui"))]
pub(crate) fn run(_args: UiArgs) -> Result<i32, SoldrError> {
    eprintln!("soldr ui: this soldr build has no UI (built without the ui feature)");
    crate::exit_guard::mark_spoke();
    Ok(2)
}

#[cfg(all(test, not(feature = "ui")))]
mod tests {
    #[test]
    fn a_build_without_ui_returns_exit_two() {
        use clap::Parser;
        let cli = crate::cli_args::Cli::try_parse_from(["soldr", "ui"]).unwrap();
        let crate::cli_args::Commands::Ui(args) = cli.command else {
            panic!("UI command")
        };
        assert_eq!(super::run(args).unwrap(), 2);
    }
}

#[cfg(test)]
mod startup_tests {
    #[test]
    fn ui_is_a_broker_free_local_command_even_with_global_flags() {
        for args in [
            vec![
                "soldr",
                "ui",
                "--fixture",
                "state.json",
                "--screenshot",
                "out.png",
            ],
            vec!["soldr", "--debug", "ui", "--window"],
            vec!["soldr", "--jobs", "2", "ui"],
        ] {
            let args: Vec<_> = args.into_iter().map(str::to_string).collect();
            assert!(super::is_invocation(&args));
            assert!(!crate::broker_spawn::front_door_command_shape(&args));
        }
        let args = ["soldr", "cargo", "run", "--", "ui"].map(str::to_string);
        assert!(!super::is_invocation(&args));
    }
}
