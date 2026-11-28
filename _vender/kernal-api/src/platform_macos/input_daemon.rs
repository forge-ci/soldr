//! `kernal-inputd` is Linux-only; this host injects in-process (see `desktop_input`).
use std::path::Path;

pub async fn run(_socket: &Path, _allowed_uids: Vec<u32>) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "kernal-inputd is Linux-only; this host injects in-process",
    ))
}
