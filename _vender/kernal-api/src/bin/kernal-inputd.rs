//! Privileged keyboard/pointer injection daemon for Linux:
//! `kernal-inputd [--socket PATH] --allow-uid UID [--allow-uid UID ...]`.

fn main() {
    let mut socket = std::path::PathBuf::from("/run/kernal-inputd/socket");
    let mut allowed = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--socket", Some(path)) => socket = path.into(),
            ("--allow-uid", Some(uid)) => {
                allowed.push(uid.parse().expect("--allow-uid takes a number"))
            }
            _ => {
                eprintln!("usage: kernal-inputd [--socket PATH] --allow-uid UID...");
                std::process::exit(2);
            }
        }
    }
    if allowed.is_empty() {
        eprintln!("kernal-inputd: at least one --allow-uid is required");
        std::process::exit(2);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    if let Err(err) = runtime.block_on(kernal_api::desktop_input::daemon::run(&socket, allowed)) {
        eprintln!("kernal-inputd: {err}");
        std::process::exit(1);
    }
}
