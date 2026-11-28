//! Register a desktop-wide chord and print its events: `global-hotkey [CHORD] [SECONDS]`.
use kernal_api::global_hotkey::{Chord, HotkeyHandle, HotkeyOptions};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let chord = args.next().unwrap_or_else(|| "Ctrl+Alt+F9".into());
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let options = HotkeyOptions::new(
        "kernal-api-example",
        "kernal-api global-hotkey example",
        Chord::parse(&chord).expect("chord"),
    )
    .expect("options");
    let mut handle = match HotkeyHandle::register(options).await {
        Ok(handle) => handle,
        Err(err) => {
            println!("register {chord}: {err:?}");
            std::process::exit(2);
        }
    };
    println!("registered {chord}; press it within {seconds}s");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    while let Some(left) = deadline.checked_duration_since(tokio::time::Instant::now()) {
        match handle.next_event_within(left).await {
            Some(event) => println!("{event:?}"),
            None => break,
        }
    }
}
