// Raw blocking waits are rejected; the timeout-only facade is the way in.
use std::sync::mpsc::channel;

fn main() {
    let (tx, rx) = channel::<u8>();
    tx.send(1).unwrap();
    let _ = rx.recv();
    let handle = std::thread::spawn(|| 1);
    let _ = handle.join();
    let barrier = std::sync::Barrier::new(1);
    let _ = barrier.wait();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let _ = child.wait();
    let _ = std::process::Command::new("true").status();
    let _ = std::process::Command::new("true").output();
}
