mod dashboard;
mod data;
mod host;
mod model;
mod process;
#[cfg(test)]
mod tests;
mod ui;
mod view;
mod worker;
use model::State;
use std::path::PathBuf;
use view::{Mode, Tab};

/// Options supplied by the soldr CLI; this crate defines no executable.
pub struct Options {
    /// Canonically named soldr image used by live child commands.
    pub executable: Option<PathBuf>,
    pub window: bool,
    /// Render panel and dashboard without any live Soldr command.
    pub screenshot: Option<PathBuf>,
    pub fixture: Option<PathBuf>,
    pub hotkey: Option<String>,
    /// Manual large-text fallback where the facade cannot query system preferences.
    pub text_scale: f32,
    pub tab: String,
}
pub fn run(
    args: Options,
    runtime: &tokio::runtime::Handle,
) -> Result<(), Box<dyn std::error::Error>> {
    if !args.text_scale.is_finite() || !(1.0..=2.0).contains(&args.text_scale) {
        return Err("--text-scale must be between 1 and 2".into());
    }
    let tab = match args.tab.as_str() {
        "builds" => Tab::Builds,
        "nodes" => Tab::Nodes,
        "diagnostics" => Tab::Diagnostics,
        _ => Tab::Overview,
    };
    if let Some(path) = &args.fixture {
        let state: State = serde_json::from_slice(&std::fs::read(path)?)?;
        let state = state.hydrate();
        if let Some(out) = args.screenshot {
            view::screenshot(state.clone(), Mode::Panel, Tab::Overview, &out)?;
            let stem = out
                .file_stem()
                .ok_or("Screenshot needs a filename")?
                .to_string_lossy();
            let full = out.with_file_name(format!("{stem}-window.png"));
            view::screenshot(state, Mode::Dashboard, tab, &full)?;
            println!("Rendered {} and {}", out.display(), full.display());
        } else {
            let mode = if args.window {
                Mode::Dashboard
            } else {
                Mode::Panel
            };
            let mut panel = view::Panel::fixture(state, mode);
            panel.tab = tab;
            if args.text_scale != 1.0 {
                panel.scale = args.text_scale;
            }
            kernal_api::gfx::run(view::options(&panel.state, mode, false, panel.scale), panel)?;
        }
        return Ok(());
    }
    process::set_executable(
        args.executable
            .ok_or("Live UI needs its soldr executable")?,
    )?;
    host::run(runtime, args.window, args.hotkey, args.text_scale)
}

/// One-hop authorization for the UI's explicitly launched CLI children.
pub const COMMAND_EDGE_ENV: &str = "SOLDR_INTERNAL_UI_COMMAND";
