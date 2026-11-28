//! Capture from the default microphone and print the level: `audio-capture [SECONDS]`.
use kernal_api::audio_capture::{Capture, SAMPLE_RATE};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let mut capture = match Capture::open_default().await {
        Ok(capture) => capture,
        Err(err) => {
            println!("open: {err:?}");
            std::process::exit(2);
        }
    };
    println!(
        "device rate {} Hz -> {SAMPLE_RATE} Hz mono",
        capture.device_rate()
    );
    let wanted = SAMPLE_RATE as usize * seconds as usize;
    let mut samples = Vec::with_capacity(wanted);
    while samples.len() < wanted {
        match capture.next_chunk_within(Duration::from_secs(2)).await {
            Some(chunk) => samples.extend(chunk),
            None => break,
        }
    }
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt();
    println!(
        "captured {} samples ({:.2}s), rms {rms:.5}",
        samples.len(),
        samples.len() as f32 / SAMPLE_RATE as f32
    );
}
