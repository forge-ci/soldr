//! Compact status panel using only the neutral facade.
//! `soldr cargo run --example gfx-widgets --features gfx-egui -- snapshot OUT.pam`
//! Add `light` after the path for a light snapshot. `window OUT.pam` captures
//! the same frame from a live window, like gfx-parity (requires a compositor).
#[cfg(feature = "gfx-egui")]
#[path = "../tests/support/gfx_widgets.rs"]
mod scene;
#[cfg(feature = "gfx-egui")]
use kernal_api::gfx;
#[cfg(feature = "gfx-egui")]
mod panel {
    use kernal_api::gfx::{self, App, Event, Frame, Theme, ThemeMode, WindowOptions};
    use std::io::Write as _;

    pub struct Panel {
        pub output: Option<std::path::PathBuf>,
        pub frames: u32,
    }

    impl App for Panel {
        fn frame(&mut self, f: &mut Frame<'_>) {
            if let Some((w, h, rgba)) = f.events().iter().find_map(|event| match event {
                Event::Screenshot {
                    width,
                    height,
                    rgba,
                } => Some((*width, *height, rgba.clone())),
                _ => None,
            }) {
                if let Some(path) = self.output.take() {
                    write_pam(&path, w, h, &rgba).expect("write window capture");
                    f.close();
                }
            }
            super::scene::StatusPanel.frame(f);
            self.frames += 1;
            if self.output.is_some() {
                if self.frames == 10 {
                    f.request_screenshot();
                }
                f.request_repaint();
            }
        }
    }

    fn write_pam(
        path: &std::path::Path,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> std::io::Result<()> {
        let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
        write!(
            file,
            "P7\nWIDTH {width}\nHEIGHT {height}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
        )?;
        file.write_all(rgba)?;
        file.flush()
    }

    pub fn run() -> Result<(), gfx::GfxError> {
        let mut args = std::env::args().skip(1);
        let mode = args.next();
        let path = args.next();
        let theme = if args.next().as_deref() == Some("light") {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        let options = WindowOptions::new("soldr")
            .size(360.0, 340.0)
            .theme(Theme::with_mode(theme));
        let mut app = Panel {
            output: None,
            frames: 0,
        };
        match (mode.as_deref(), path) {
            (Some("snapshot"), Some(path)) => {
                let shot = gfx::snapshot(&options, &mut app)?;
                write_pam(path.as_ref(), shot.width(), shot.height(), shot.rgba())
                    .expect("write snapshot");
                Ok(())
            }
            (Some("window"), Some(path)) => {
                app.output = Some(path.into());
                gfx::run(options, app)
            }
            _ => gfx::run(
                options.theme(Theme::with_mode(ThemeMode::FollowSystem)),
                app,
            ),
        }
    }
}

#[cfg(feature = "gfx-egui")]
fn main() -> Result<(), kernal_api::gfx::GfxError> {
    panel::run()
}
#[cfg(not(feature = "gfx-egui"))]
fn main() {
    eprintln!("enable gfx-egui to run this example");
}
