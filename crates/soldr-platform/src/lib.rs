//! The single place Soldr selects its host-platform implementation.
//!
//! Every other production crate aliases this crate as `crate::platform`
//! and calls only the neutral capability facades (`process`, `fs`, `ipc`,
//! `executable`, `host`). No other production source file may choose the
//! host with `#[cfg]`, `#[cfg_attr]`, or `cfg!()`, and no other crate may
//! name the concrete implementation trees.
//!
//! The `cfg_select!` block below is the **only** host-selection site in
//! Soldr. There is deliberately no `_` arm and no generic fallback: a
//! host OS Soldr does not implement fails compilation here, loudly, rather
//! than running a half-supported implementation.
//!
//! Host platform is not build target. `crate::platform` answers "what OS
//! is this Soldr executable running on"; `TargetTriple` and runtime target
//! policy answer "what is Soldr building for". A Linux-hosted cross-build
//! still uses `platform_linux` for host mechanics.

#![warn(missing_docs)]

use std::cfg_select;

mod platform;
pub use platform::{executable, fs, host, ipc, process, sync};

cfg_select! {
    target_os = "windows" => {
        mod platform_win;
        pub(crate) use platform_win as platform_imp;
    },
    target_os = "linux" => {
        mod platform_linux;
        pub(crate) use platform_linux as platform_imp;
    },
    target_os = "macos" => {
        mod platform_macos;
        pub(crate) use platform_macos as platform_imp;
    },
}
