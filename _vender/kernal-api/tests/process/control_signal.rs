//! Control signals reach this process's own receiver through `send`.
#![cfg(feature = "control-signal")]

use kernal_api::control_signal::{send, ControlReceiver, ControlSignal};
use std::time::Duration;

#[test]
fn signals_sent_to_this_process_reach_its_receiver() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let mut receiver = ControlReceiver::listen().expect("listen");
        for signal in [ControlSignal::User1, ControlSignal::User2] {
            send(std::process::id(), signal).expect("send");
            let got = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
                .await
                .expect("delivered within 10 s");
            assert_eq!(got, Some(signal));
        }
    });
}
