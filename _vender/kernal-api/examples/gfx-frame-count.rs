//! Count `App::frame` calls for a few seconds, then close and print them.
//!
//! ```console
//! soldr cargo run --example gfx-frame-count --features gfx-egui -- 5 repaint
//! ```
//!
//! Arguments: seconds to run (default 5) and `repaint` (ask for a frame every
//! frame) or `after` (ask for one a second later). Used to observe whether a
//! compositor is delivering frames to a window; see `docs/gfx-backend.md`,
//! "When frames happen".

use std::time::{Duration, Instant};

use kernal_api::gfx::{self, App, Frame, WindowOptions};

struct Counter {
    start: Instant,
    run_for: Duration,
    every_frame: bool,
    frames: u64,
}

impl App for Counter {
    fn frame(&mut self, frame: &mut Frame<'_>) {
        self.frames += 1;
        frame.label(&format!("frame {}", self.frames));
        if self.start.elapsed() >= self.run_for {
            frame.close();
        } else if self.every_frame {
            frame.request_repaint();
        } else {
            frame.request_repaint_after(Duration::from_secs(1));
        }
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        println!(
            "frames={} elapsed={:.3}s",
            self.frames,
            self.start.elapsed().as_secs_f64()
        );
    }
}

fn main() -> Result<(), gfx::GfxError> {
    let mut args = std::env::args().skip(1);
    let seconds = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let every_frame = args.next().as_deref() != Some("after");
    let app = Counter {
        start: Instant::now(),
        run_for: Duration::from_secs(seconds),
        every_frame,
        frames: 0,
    };
    gfx::run(WindowOptions::new("gfx-frame-count").size(200.0, 60.0), app)
}
