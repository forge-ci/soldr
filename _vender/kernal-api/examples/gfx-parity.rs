//! Render one search-popover-shaped UI both ways, so a windowless
//! [`gfx::snapshot`] can be diffed against what a live window shows.
//!
//! ```console
//! soldr cargo run --example gfx-parity --features gfx-egui -- snapshot /tmp/snap.pam
//! soldr cargo run --example gfx-parity --features gfx-egui -- window /tmp/win.pam
//! ```
//!
//! Each writes a PAM (`P7`, RGB_ALPHA) image: no image crate is needed to
//! read it back. The window mode needs a compositor that answers frame
//! callbacks (see docs/gfx-backend.md, "When frames happen").

use std::io::Write as _;

use kernal_api::gfx::{self, App, Event, Frame, WindowOptions};

/// Frames drawn before the window asks for its own pixels.
const SETTLE: u32 = 10;

struct Popover {
    query: String,
    frames: u32,
    out: Option<std::path::PathBuf>,
}

impl App for Popover {
    fn frame(&mut self, frame: &mut Frame<'_>) {
        let captured = frame.events().iter().find_map(|event| match event {
            Event::Screenshot {
                width,
                height,
                rgba,
            } => Some((*width, *height, rgba.clone())),
            _ => None,
        });
        if let Some((width, height, rgba)) = captured {
            // Taken only once the capture is here: `out` also marks "still
            // waiting" for the repaint below.
            if let Some(path) = self.out.take() {
                write_pam(&path, width, height, &rgba).expect("write capture");
                println!("{}", path.display());
                frame.close();
            }
        }
        frame.text_field(&mut self.query);
        frame.separator();
        frame.scroll(300.0, |frame| {
            frame.list(|frame| {
                frame.selectable_row("Firefox  —  Application", true);
                frame.selectable_row("Files  —  Application", false);
            });
        });
        self.frames += 1;
        if self.out.is_some() {
            if self.frames == SETTLE {
                frame.request_screenshot();
            }
            frame.request_repaint();
        }
    }
}

fn write_pam(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> std::io::Result<()> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(
        file,
        "P7\nWIDTH {width}\nHEIGHT {height}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
    )?;
    file.write_all(rgba)?;
    file.flush()
}

fn main() -> Result<(), gfx::GfxError> {
    let mut args = std::env::args().skip(1);
    let (Some(mode), Some(path)) = (args.next(), args.next()) else {
        eprintln!("usage: gfx-parity snapshot|window OUT.pam");
        std::process::exit(2);
    };
    let options = WindowOptions::new("gfx parity")
        .size(640.0, 400.0)
        .resizable(false)
        .decorations(false);
    let mut app = Popover {
        query: "fire".to_owned(),
        frames: 0,
        out: None,
    };
    match mode.as_str() {
        "snapshot" => {
            let shot = gfx::snapshot(&options, &mut app)?;
            write_pam(path.as_ref(), shot.width(), shot.height(), shot.rgba())
                .expect("write snapshot");
            println!("{path}");
            Ok(())
        }
        "window" => {
            app.out = Some(path.into());
            gfx::run(options, app)
        }
        _ => {
            eprintln!("usage: gfx-parity snapshot|window OUT.pam");
            std::process::exit(2);
        }
    }
}
