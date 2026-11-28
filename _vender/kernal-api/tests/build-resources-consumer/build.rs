//! Embeds Windows resources through the `kernal-api` build-dependency.

use kernal_api::build_resources::{embed_windows_app_resources, WindowsAppResources};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let resources = WindowsAppResources::new(
        "Build Resources Consumer",
        "kernal-api",
        env!("CARGO_PKG_VERSION"),
    )?
    .with_file_description("kernal-api build-resources fixture")?;
    embed_windows_app_resources(&resources)?;
    Ok(())
}
