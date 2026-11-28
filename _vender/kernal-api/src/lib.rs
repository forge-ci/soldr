// docs.rs passes `--cfg docsrs` (see `[package.metadata.docs.rs]`). Without
// this, that flag is dead config and no gated item gets an "Available on
// crate feature" badge -- which is how four public daemon modules stayed
// invisible in the rendered documentation.
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = include_str!("../README.md")]

//! Facade-owned asynchronous systems operations.
//!
//! This crate is the public systems boundary shared by its clients. Backend
//! crates remain private implementation details: higher layers receive
//! `kernal_api` types and never name the underlying async runtime, allocator,
//! profiler, symbol parser, or native platform APIs directly.

// Guest ABI selection is separate from native operating-system selection.
std::cfg_select! {
    target_family = "wasm" => {
        pub mod guest;
    }
    _ => {
use std::cfg_select;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

mod process_adapter;

// Facade-owned independent resource placement; `running-process` stays a
// private backend (see the module documentation).
#[cfg(feature = "independent-spawn")]
mod independent_spawn;
#[cfg(feature = "independent-spawn")]
pub use independent_spawn::{
    spawn_with_options, IndependentBackend, LaunchSpec, Readiness, SpawnExit, SpawnHandle,
    SpawnLifetime, SpawnMode, SpawnOptions,
};

#[cfg(feature = "command-arguments")]
pub mod arguments;
/// Desktop-wide key chords, with explicit unsupported-host fallback.
#[cfg(feature = "global-hotkey")]
pub mod global_hotkey;
#[cfg(feature = "global-hotkey")]
use platform_imp::global_hotkey as native_global_hotkey;
/// Installed applications, launching them, and opening URIs.
#[cfg(feature = "app-catalog")]
pub mod app_catalog;
#[cfg(feature = "app-catalog")]
use platform_imp::app_catalog as native_app_catalog;
/// Synthesized keyboard and pointer input (Linux through the `kernal-inputd` daemon).
#[cfg(feature = "desktop-input")]
pub mod desktop_input;
#[cfg(feature = "desktop-input")]
use platform_imp::desktop_input as native_desktop_input;
#[cfg(feature = "desktop-input-daemon")]
use platform_imp::input_daemon as native_input_daemon;
/// Microphone capture as mono 16 kHz chunks.
#[cfg(feature = "audio-capture")]
pub mod audio_capture;
/// Daemon control signals: SIGUSR1/SIGUSR2/SIGTERM and their Windows equivalents.
#[cfg(feature = "control-signal")]
pub mod control_signal;
#[cfg(feature = "control-signal")]
use platform_imp::control_signal as native_control_signal;
/// Optional desktop status area, with explicit unsupported-host fallback.
#[cfg(feature = "gfx")]
pub mod gfx;
#[cfg(feature = "gfx-egui")]
mod gfx_egui;
#[cfg(feature = "gfx-egui")]
use platform_imp::gfx_dock as native_gfx_dock;
#[cfg(feature = "system-tray")]
pub mod system_tray;
#[cfg(feature = "system-tray")]
use platform_imp::system_tray as native_system_tray;

/// Bounded command-line schema parsing with facade-owned values and diagnostics.
#[cfg(feature = "command-schema")]
pub mod command;
/// Owned application error context and source chaining.
#[cfg(feature = "error-context")]
pub mod error;

/// Bounded configuration decoding with caller-owned schemas and defaults.
#[cfg(feature = "config-toml")]
pub mod config;
/// Bounded JSON values and encoding with caller-owned schemas.
#[cfg(feature = "json")]
pub mod json;

/// Actually Portable Executable launch support: detection, loader planning,
/// and the recovery every spawn applies when a host refuses an APE image.
pub mod ape;

/// Wall-clock epoch seconds and milliseconds.
pub mod clock;

/// Kernel-owned BLAKE3 content hashing for bytes, readers, and files, plus
/// an incremental hasher and key-derivation domain separation.
pub mod hash;

/// Synchronous, bounded SQLite storage mechanics.
///
/// This facade owns connection safety policy and semantic result values; the
/// application owns its schema, SQL, migrations, and choice of blocking
/// worker. Do not call it on an async executor thread.
#[cfg(feature = "sqlite")]
pub mod sqlite;

/// Bounded source analysis; callers own language integration and product policy.
#[cfg(feature = "source-cpp")]
pub mod source;

/// Bounded, fallible operating-system entropy without token-format policy.
#[cfg(feature = "secure-random")]
pub mod random;
#[cfg(feature = "text-similarity")]
pub mod text;

/// Bounded terminal key decoding and owned native polling.
#[cfg(feature = "terminal-input")]
pub mod keys;

/// Allocation-free diagnostic styling and native ANSI output preparation.
#[cfg(feature = "terminal-style")]
pub mod terminal_style;

/// Bounded extraction into caller-exclusive staging directories.
#[cfg(feature = "archive")]
pub mod archive;
#[cfg(any(feature = "http-client", feature = "http-client-rustls"))]
pub mod http;
#[cfg(feature = "http-server")]
pub mod http_server;
/// Owned native pseudo-terminal sessions without backend descriptor types.
#[cfg(feature = "pty")]
pub mod pty;

/// Facade-owned identity, sidecar, probe, and endpoint-mux semantics for an
/// existing daemon endpoint.
///
/// This opt-in surface preserves the native substrate's frozen v1 wire while
/// deliberately leaving endpoint naming, product payload protocols, and
/// daemon lifecycle policy to the application.
#[cfg(feature = "daemon-identity")]
pub mod daemon_identity;

/// Facade-owned frozen v1 daemon-frame envelope codec.
///
/// This opt-in surface preserves the shared frame bytes while leaving endpoint
/// selection, product payload identifiers, and connection policy to callers.
#[cfg(feature = "daemon-frame-v1")]
pub mod daemon_frame_v1;

/// Facade-owned frozen v1 daemon-registration records and persistence.
///
/// This opt-in surface retains the established v1 manifest and service-
/// definition bytes while leaving endpoints, broker client policy, and daemon
/// lifecycle policy to applications.
#[cfg(feature = "daemon-registration")]
pub mod daemon_registration;

/// Facade-owned frozen v2 service-definition registration and persistence.
///
/// This opt-in surface retains the established v2 path, validation, private
/// directory, and non-atomic persistence behavior while leaving v1 records,
/// broker negotiation, transport, identity, and runtime policy to callers.
#[cfg(feature = "daemon-registration-v2")]
pub mod daemon_registration_v2;

/// Facade-owned broker client: backend connect and refusal classification.
///
/// This opt-in adapter keeps the broker implementation in the private
/// substrate while applications receive owned request, connection, route,
/// refusal, and error values. Endpoint naming, payload protocols, and fallback
/// policy remain with callers.
#[cfg(feature = "broker-client")]
pub mod broker_client;

/// Facade-owned broker v2 daemon lifecycle: publish, discover, and connect.
///
/// Daemons publish their backend endpoint where the v2 broker resolves it;
/// clients negotiate through a bound broker or dial the published identity.
/// Payload protocols and daemon start policy remain with callers.
#[cfg(feature = "broker-v2")]
pub mod broker_v2;

/// Build-script helpers: Windows executable resources for an application's
/// own `build.rs`. Depend on it as a renamed build-dependency so the
/// application's `kernal-api/<feature>` entries never reach the build script
/// (see the module documentation).
#[cfg(feature = "build-resources")]
pub mod build_resources;

/// Canonical async runtime, task, I/O, network, and synchronization facade.
pub mod async_engine;

// The generated Wasm ABI and opt-in native webview capability share one
// logical operation/resource authority.  The implementation is deliberately
// runtime-neutral: enabling `tauri-webview` must not pull Wasmtime into the
// default or native-only graph.
#[cfg(any(feature = "wasm-sketch-host", feature = "tauri-webview"))]
#[path = "wasm/operations.rs"]
#[allow(dead_code)] // Native-only builds use the hub's webview subset.
pub(crate) mod operations;

// The native implementation is intentionally private until #16's generated
// operation and resource registry can own the semantic handles.  In
// particular, no Tauri/Wry/platform type crosses this boundary.
#[cfg(all(
    feature = "tauri-webview",
    any(target_os = "linux", target_os = "macos", target_os = "windows")
))]
mod tauri;

#[cfg(feature = "tauri-webview")]
pub(crate) use platform_imp::viewport_capture as native_viewport_capture;
#[cfg(feature = "tauri-webview")]
pub(crate) use platform_imp::webview_window as native_webview_window;

#[cfg(feature = "wasm-sketch-worker")]
pub(crate) use platform_imp::scratch_directory::Anchor as ScratchDirectoryAnchor;

/// Semantic, opt-in external-webview operations.
///
/// This module exposes no Tauri, Wry, Tokio, or platform-window types.  It
/// is available only with the heavyweight `tauri-webview` feature.
#[cfg(all(
    feature = "tauri-webview",
    any(target_os = "linux", target_os = "macos", target_os = "windows")
))]
pub mod webview {
    #[cfg(feature = "tauri-webview-test-support")]
    pub use crate::tauri::capture::WebviewTestUiPause;
    #[cfg(feature = "tauri-webview-test-support")]
    pub use crate::tauri::WebviewTestObservation;
    #[cfg(feature = "tauri-webview-test-support")]
    pub use crate::tauri::WebviewTestPresentation;
    #[cfg(feature = "tauri-webview-test-support")]
    pub use crate::tauri::WebviewTestTraceEvent;
    pub use crate::tauri::{
        BestEffort, ExternalWebviewClient, ExternalWebviewHost, PageBootstrapError,
        ViewportCaptureLimits, WebviewError, WebviewHandle, WebviewPageBootstrap,
        WebviewPermissions, WebviewSnapshot, WebviewSnapshotChunk, WebviewUrlGrant,
        WebviewWindowOptions, WebviewWindowSupport, WindowOptionsError,
    };
    pub use crate::tauri::{WebviewLogicalRect, WebviewMonitor};
}

#[cfg(all(
    feature = "tauri-webview",
    not(any(target_os = "linux", target_os = "macos", target_os = "windows"))
))]
compile_error!("the `tauri-webview` feature supports native Linux, macOS, and Windows hosts only");

/// Admission policy for opt-in Rust WebAssembly sketches.
///
/// This module privately admits and starts the bounded threaded-root profile.
/// Thread scheduling, generated async ABI, resources, and worker-process
/// containment land in later sketch-host slices.
#[cfg(feature = "wasm-sketch-host")]
pub mod wasm;

/// Cooperative all-thread stack capture and deferred unwinding.
#[cfg(feature = "snapshot")]
pub mod snapshot;

/// Bounded CPU/off-CPU profiling with pprof, Firefox, and collapsed exports.
#[cfg(feature = "profile")]
pub mod profile;

/// PDB/DWARF/Mach-O symbol discovery and resolution.
///
/// Parse untrusted symbol files in a disposable worker process; the API is
/// kept here so every client uses the same ASLR-independent capture schema.
#[cfg(feature = "symbolize")]
pub mod symbolize;

/// Dormant-by-default mimalloc heap profiling and dump support.
#[cfg(feature = "allocator")]
pub mod allocator;

/// Cooperative crash capture using the crate's single native handler.
#[cfg(feature = "crash")]
pub mod crash;

/// Runtime diagnostics implementation; use [`async_engine`] as the public path.
#[cfg(feature = "tokio-console")]
#[doc(hidden)]
mod runtime;

/// Neutral capability indexes for the eventual workspace-wide host boundary.
///
/// The indexes intentionally expose no operations yet: phase 2 establishes
/// ownership names before later phases move a capability behind them.
pub mod platform;

#[cfg(feature = "fs-scan")]
pub mod fs_scan;
#[cfg(feature = "fs-scan")]
use platform_imp::fs_scan as native_fs_scan;

// This is deliberately the crate's only host selector.  Facade modules are
// neutral; native details live behind the selected private root.
cfg_select! {
    target_os = "windows" => {
        mod platform_win;
        pub(crate) use platform_win as platform_imp;
    }
    target_os = "linux" => {
        mod platform_linux;
        pub(crate) use platform_linux as platform_imp;
    }
    target_os = "macos" => {
        mod platform_macos;
        pub(crate) use platform_macos as platform_imp;
    }
}

// Re-export the selected implementation once from this allowed host-selector
// root. Neutral capability facades re-export only crate-root names and never
// name the private `platform_imp` alias themselves.
pub use platform_imp::{
    active_graphics_probe, assign_child_to_windows_job, cancel_capture_reader,
    canonical_environment_pairs, capture_reader_done, compat_shell_command, configure_exact_trace,
    configure_process_command, configure_sync_contained_command, configure_sync_daemon_command,
    configure_trampoline_command, current_executable_build_id, exact_trace_capability, exit_code,
    monitor_console_windows, parent_has_console, prepare_capture_reader, set_process_name,
    shell_command, soft_terminate_process_group, spawn_sync, spawn_sync_daemon,
    start_descendant_monitor, start_exact_trace, sync_child_native_handle, trampoline_exit_code,
    unix_mark_extra_fds_close_on_exec, unix_set_priority, unix_signal_process,
    unix_signal_process_group, unix_signal_raw, CaptureCancellation, TracedChild, WindowsJobHandle,
};

// Window-icon mechanics are a GUI capability, not part of the default async
// process/host HAL: on Linux they carry a PNG decoder and an X11 client.
#[cfg(feature = "window-icon")]
pub use platform_imp::{set_window_icon_impl, window_icon_support_impl};

pub use platform_imp::{autostart_register, autostart_render_registration, autostart_unregister};

pub use platform_imp::{process_install_owner_death_cleanup, process_owner_death_cleanup_target};

pub use platform_imp::{process_spawner_lifetime_enforcement, ProcessExitWatch};

pub use platform_imp::process_install_shutdown_request_handler;

pub use platform_imp::fs_write_all_to_descriptor;

pub use platform_imp::{process_can_replace_current_image, process_replace_current_image};

pub use platform_imp::{process_same_executable_path, ProcessLiveness};

// Reading the image a PID was started from stays crate-private: it is
// addressed by PID, so it is only sound behind a retained `ProcessLiveness`
// reference, which `platform::process::executable_path` requires.
pub(crate) use platform_imp::process_inspect::process_executable_path;

pub use platform_imp::NativeJobserver;

pub(crate) use platform_imp::dylib as native_dylib;
pub(crate) use platform_imp::{
    process_configure_session_leader_command, process_cpu_ticks_for_pid,
    process_detach_standard_streams, process_force_terminate_child_process_group,
    process_native_jobserver_supported, process_peak_rss_bytes_for_pid,
    process_redirect_standard_streams_to_log, process_set_priority_identity,
    process_tree_rss_bytes_for_pid, PROCESS_MAX_TREE_RSS_PROCESSES,
    PROCESS_PEAK_RSS_READABLE_AFTER_EXIT,
};

pub use platform_imp::{
    resources_available_space, resources_fd_exhaustion_error, resources_inode_capacity,
    resources_signals_fd_exhaustion, resources_signals_storage_exhaustion,
    resources_storage_exhaustion_error, resources_total_space,
};

pub use platform_imp::{
    executable_file_name, executable_file_name_os, executable_find_in_paths, executable_native_library_name,
    executable_sibling_of_current_image, executable_stem_matches,
    executable_unlock_for_replacement, EXECUTABLE_EXTENSION,
};

#[cfg(feature = "fs")]
pub use platform_imp::{
    fs_create_dir_all_private, fs_create_private_file, fs_decode_path_bytes, fs_encode_path_bytes,
    fs_ensure_dir_private, fs_ensure_no_extended_acl, fs_file_identity, fs_open_shared_append,
    fs_is_lock_conflict, fs_lock_exclusive, fs_lock_shared, fs_open_lock_file, fs_path_identity,
    fs_replace_file, fs_set_file_mtime, fs_sync_directory, fs_try_lock_exclusive,
    fs_try_lock_shared, fs_unlock, fs_user_config_dir, fs_user_data_dir, fs_user_run_data_root,
    fs_user_runtime_dir, fs_user_state_dir, FsFileIdentity,
};

#[cfg(feature = "fs")]
pub(crate) use platform_imp::fs_materialize as native_fs_materialize;

#[cfg(feature = "fs")]
pub(crate) use platform_imp::fs_read_private_regular_file_bounded;

#[cfg(feature = "fs")]
pub(crate) use platform_imp::fs_read_context_regular_file_bounded;

#[cfg(feature = "fs-watch")]
pub use platform_imp::FsWatchWatcher;

pub use platform_imp::{
    host_boot_id, host_cpu_compatibility_features, host_current_process_privilege,
    host_current_user,
    host_environment_keys_are_case_insensitive, host_filesystem_device_id, host_home_dir,
    host_hostname, host_is_elevated, host_login_environment, host_machine_id, host_namespace_id,
    host_user_machine_identity, HostPrivilegedIdentity,
};

pub use platform_imp::host_login_environment_block;

pub use platform_imp::terminal_input;

#[cfg(feature = "ipc")]
pub use platform_imp::{
    ipc_broker_endpoint_name as IpcBrokerEndpointName, ipc_broker_v1_endpoint_path,
    ipc_broker_v2_runtime_dir, ipc_current_user_id, ipc_endpoint_is_filesystem_backed,
    ipc_endpoint_name_limit, ipc_endpoint_scope_bytes, ipc_ensure_owner_private_directory,
    ipc_nonblocking_zero_read_is_pending, ipc_owner_private_directory, ipc_select_endpoint_address,
    IpcEndpoint, IpcInheritedListener, IpcListener, IpcListenerNonblockingMode, IpcPeerIdentity,
    IpcPeerIdentitySource, IpcStream,
};

#[cfg(feature = "ipc")]
pub use platform_imp::{
    ipc_retire_socket_endpoint, IpcLocalPipeClient, IpcLocalSocketListener, IpcLocalSocketStream,
    IpcOwnerOnlyPipeInstance,
};

/// Failure details for the deprecated 4.x raw descriptor/handle handoff API.
///
/// This type exists only at the crate-root compatibility boundary. New product
/// mechanics use opaque [`platform::ipc::Stream`] operations instead.
#[cfg(feature = "ipc")]
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyHandoffError {
    kind: platform::ipc::HandoffTransferErrorKind,
    raw_os_error: Option<i32>,
    transferred_bytes: Option<usize>,
    expected_bytes: Option<usize>,
    detail: Option<String>,
}

#[cfg(feature = "ipc")]
impl LegacyHandoffError {
    pub(crate) fn new(
        kind: platform::ipc::HandoffTransferErrorKind,
        raw_os_error: Option<i32>,
    ) -> Self {
        Self {
            kind,
            raw_os_error,
            transferred_bytes: None,
            expected_bytes: None,
            detail: None,
        }
    }

    pub(crate) fn with_detail(
        kind: platform::ipc::HandoffTransferErrorKind,
        raw_os_error: Option<i32>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            raw_os_error,
            transferred_bytes: None,
            expected_bytes: None,
            detail: Some(detail.into()),
        }
    }

    #[doc(hidden)]
    pub fn partial(transferred_bytes: usize, expected_bytes: usize) -> Self {
        Self {
            kind: platform::ipc::HandoffTransferErrorKind::Failed,
            raw_os_error: None,
            transferred_bytes: Some(transferred_bytes),
            expected_bytes: Some(expected_bytes),
            detail: Some(format!(
                "SCM_RIGHTS connection transfer was partial ({transferred_bytes}/{expected_bytes} bytes)"
            )),
        }
    }

    /// Return the policy-neutral failure category.
    pub fn kind(&self) -> platform::ipc::HandoffTransferErrorKind {
        self.kind
    }

    /// Return the native error code retained for legacy public diagnostics.
    pub fn raw_os_error(&self) -> Option<i32> {
        self.raw_os_error
    }

    /// Return a partial payload count when the descriptor may have transferred.
    pub fn partial_counts(&self) -> Option<(usize, usize)> {
        self.transferred_bytes.zip(self.expected_bytes)
    }

    pub(crate) fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

/// Whether the deprecated 4.x SCM_RIGHTS compatibility transport is available.
#[cfg(feature = "ipc")]
#[doc(hidden)]
pub const LEGACY_SCM_RIGHTS_TRANSPORT_SUPPORTED: bool =
    platform_imp::LEGACY_SCM_RIGHTS_TRANSPORT_SUPPORTED;

/// Whether the deprecated 4.x DuplicateHandle compatibility transport is available.
#[cfg(feature = "ipc")]
#[doc(hidden)]
pub const LEGACY_DUPLICATE_HANDLE_TRANSPORT_SUPPORTED: bool =
    platform_imp::LEGACY_DUPLICATE_HANDLE_TRANSPORT_SUPPORTED;

/// Root-only adapter for the deprecated raw-descriptor handoff API.
#[cfg(feature = "ipc")]
#[doc(hidden)]
pub fn legacy_send_fd_to(
    socket: &std::path::Path,
    sent_fd: i32,
    payload: &[u8],
) -> Result<(), LegacyHandoffError> {
    platform_imp::legacy_send_fd_to(socket, sent_fd, payload)
}

/// Root-only adapter for the deprecated connected raw-descriptor handoff API.
#[cfg(feature = "ipc")]
#[doc(hidden)]
pub fn legacy_send_fd_over(
    socket_fd: i32,
    sent_fd: i32,
    payload: &[u8],
) -> Result<(), LegacyHandoffError> {
    platform_imp::legacy_send_fd_over(socket_fd, sent_fd, payload)
}

/// Root-only adapter for the deprecated raw-handle duplication API.
#[cfg(feature = "ipc")]
#[doc(hidden)]
pub fn legacy_duplicate_handle(
    source_handle: usize,
    backend_pid: u32,
) -> Result<usize, LegacyHandoffError> {
    platform_imp::legacy_duplicate_handle(source_handle, backend_pid)
}

#[cfg(feature = "ipc-async")]
pub use platform_imp::{
    IpcAsyncListener, IpcAsyncReadHalf, IpcAsyncStream, IpcAsyncWriteHalf, IpcIntoAsyncListener,
    IpcIntoAsyncStream,
};

#[cfg(feature = "pty")]
pub use platform_imp::terminal::{
    before_pty_spawn, current_backend_kind, find_child_processes, find_orphan_conhosts,
    input_payload, is_ignorable_process_control_error, prepare_unmanaged_pty_child,
    query_responses, resize_pty, shell_argv, signal_pty_tree, terminate_pty_child,
    wait_before_pty_close_supported, Backend, ChildProcessInfo, ConPtyBackendKind,
    OrphanConhostInfo, PtyProcessGuard, PtySpawnContext,
};

#[cfg(feature = "terminal-input")]
pub use platform_imp::terminal::TerminalInputSession;

#[cfg(feature = "session-relay")]
pub use platform_imp::relay_local_socket_session;

/// Stdio policy for one child stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    /// Leave the stream connected to the parent process.
    Inherit,
    /// Create an asynchronous pipe owned by the child handle.
    Piped,
    /// Connect the stream to the platform null device.
    Null,
}

/// Relative scheduling intent for one spawned process.
///
/// These are semantic bands, not Unix niceness or Windows priority-class
/// constants. The private substrate maps each band at launch, preserving the
/// product distinction between low and idle background work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProcessPriority {
    /// Prefer this work in the host's non-realtime foreground band.
    High,
    /// Preserve the host's ordinary process scheduling policy.
    #[default]
    Normal,
    /// Defer this work behind ordinary foreground activity.
    Low,
    /// Run only when the host has no more urgent work.
    Idle,
}

impl ProcessPriority {
    /// Translate semantic priority into the selected substrate's existing
    /// launch policy. These values are private implementation details: Unix
    /// treats them as nice levels while Windows maps their bands to process
    /// priority classes.
    pub(crate) const fn substrate_nice(self) -> Option<i32> {
        match self {
            // The substrate consumes a portable niceness hint. Its Windows
            // mapping is intentionally different from Unix's numeric scale:
            // -15 selects HIGH, +1 BELOW_NORMAL, and +15 IDLE.
            Self::High => {
                #[cfg(windows)]
                {
                    Some(-15)
                }
                #[cfg(not(windows))]
                {
                    Some(-5)
                }
            }
            Self::Normal => None,
            Self::Low => {
                #[cfg(windows)]
                {
                    Some(1)
                }
                #[cfg(not(windows))]
                {
                    Some(10)
                }
            }
            Self::Idle => {
                #[cfg(windows)]
                {
                    Some(15)
                }
                #[cfg(not(windows))]
                {
                    Some(19)
                }
            }
        }
    }
}

/// Caller-owned exclusion acquired immediately around native process creation.
///
/// The closure runs on the substrate's spawning lane right before the child is
/// created, once per native spawn attempt. Its returned permit is held across
/// that attempt only -- fork/exec on Unix, `CreateProcess` on Windows -- and is
/// dropped before the spawn result reaches the caller, whether the attempt
/// succeeded or failed. It is not held for the child's lifetime. Because it
/// never crosses an `.await`, the permit itself need not be `Send`.
///
/// An `Err` from the closure fails the spawn with that same [`io::Error`]; no
/// child is created. Cloning shares the same closure.
#[derive(Clone)]
pub struct SpawnAdmission {
    inner: running_process::SpawnAdmission,
}

impl SpawnAdmission {
    /// Capture the admission function run before each native spawn attempt.
    pub fn new<F, G>(acquire: F) -> Self
    where
        F: Fn() -> io::Result<G> + Send + Sync + 'static,
        G: 'static,
    {
        Self {
            inner: running_process::SpawnAdmission::new(acquire),
        }
    }

    pub(crate) fn into_inner(self) -> running_process::SpawnAdmission {
        self.inner
    }
}

impl std::fmt::Debug for SpawnAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SpawnAdmission { .. }")
    }
}

/// Whether a scheduling-band request may be dropped when the host refuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PriorityPolicy {
    /// A refused band fails the spawn.
    Required,
    /// A refused band is dropped and the child inherits the spawner's band.
    BestEffort,
}

/// Typed spawn description accepted by the blessed process boundary.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    program: OsString,
    args: Vec<OsString>,
    current_dir: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
    clear_env: bool,
    stdin: StreamMode,
    stdout: StreamMode,
    stderr: StreamMode,
    create_process_group: bool,
    lifetime_owner: Option<platform::process::LifetimeOwner>,
    priority: ProcessPriority,
    priority_policy: PriorityPolicy,
    admission: Option<SpawnAdmission>,
}

impl SpawnSpec {
    /// Create a direct (non-shell) command description.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            env: Vec::new(),
            clear_env: false,
            stdin: StreamMode::Inherit,
            stdout: StreamMode::Inherit,
            stderr: StreamMode::Inherit,
            create_process_group: false,
            lifetime_owner: None,
            priority: ProcessPriority::Normal,
            priority_policy: PriorityPolicy::Required,
            admission: None,
        }
    }

    /// Append one argument without requiring UTF-8.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append every argument in order without requiring UTF-8.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set the child working directory.
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    /// Add an environment override.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Start with an empty inherited environment before applying overrides.
    pub fn clear_env(mut self, clear: bool) -> Self {
        self.clear_env = clear;
        self
    }

    /// Configure child stdin.
    pub fn stdin(mut self, mode: StreamMode) -> Self {
        self.stdin = mode;
        self
    }

    /// Configure child stdout.
    pub fn stdout(mut self, mode: StreamMode) -> Self {
        self.stdout = mode;
        self
    }

    /// Configure child stderr.
    pub fn stderr(mut self, mode: StreamMode) -> Self {
        self.stderr = mode;
        self
    }

    /// Put the child in its own process group.
    ///
    /// This is what makes a group-wide soft signal addressable at all:
    /// [`PlatformChild::terminate_group_soft`] is a no-op without
    /// it, because on POSIX the negative-PID signal would otherwise reach the
    /// caller's own group, and on Windows `GenerateConsoleCtrlEvent` only
    /// routes to children spawned with `CREATE_NEW_PROCESS_GROUP`. It also
    /// detaches the child from the parent's console Ctrl+C, so it is opt-in.
    /// It enables only [`PlatformChild::terminate_group_soft`] and
    /// [`ProcessSession::terminate_group_soft`]; direct kill, drop cleanup,
    /// and owner-death remain direct-child operations rather than a promise of
    /// descendant-tree containment.
    pub fn create_process_group(mut self, create: bool) -> Self {
        self.create_process_group = create;
        self
    }

    /// Kill this child when the spawning process exits unexpectedly.
    ///
    /// Linux uses `PR_SET_PDEATHSIG(SIGTERM)` and macOS uses a kqueue
    /// supervisor. Those Unix mechanisms cover the direct child only; they
    /// are not process-tree containment. Windows uses its native kill-on-close
    /// Job Object policy. The opt-in group is for explicit soft termination,
    /// not a portable tree-cleanup promise. Use a separately contained
    /// operation when tree containment is required.
    ///
    /// This is the spelling for the common case. [`Self::bind_lifetime`] is
    /// the same request with the owner named, and is what a caller wanting to
    /// know *which* enforcement it got should reach for.
    ///
    /// `false` clears whatever binding was configured, including one made by
    /// [`Self::bind_lifetime`] against another owner. It means "this child may
    /// outlive anything", not "not that particular owner".
    pub fn kill_when_owner_dies(mut self, kill: bool) -> Self {
        self.lifetime_owner = kill.then_some(platform::process::LifetimeOwner::Spawner);
        self
    }

    /// Ask that this child not outlive `owner`.
    ///
    /// The enforcement obtained depends on the host and on who the owner is,
    /// and the two are not interchangeable guarantees -- read
    /// [`Self::lifetime_enforcement`] before spawning, or
    /// [`PlatformChild::lifetime_enforcement`] afterwards, and refuse rather
    /// than assume if a watcher is not good enough.
    ///
    /// [`LifetimeOwner::Spawner`] is the only owner a kernel can be asked to
    /// enforce, because both kernel mechanisms are relationships the spawn
    /// itself creates. Naming any other process gets a facade-owned watcher:
    /// a [`ProcessExitWatch`] on the owner, and an identity-addressed kill of
    /// the child when it fires. That is reuse-safe -- the watch is pinned to
    /// the owner at spawn time and the kill is pinned to the child's creation
    /// generation -- but it is user-space, so it reaps nothing if this process
    /// is killed too.
    ///
    /// The owner is resolved before the child is started, so an owner that
    /// has already exited fails the spawn instead of producing a child with
    /// nothing watching it.
    ///
    /// [`LifetimeOwner`]: platform::process::LifetimeOwner
    /// [`LifetimeOwner::Spawner`]: platform::process::LifetimeOwner::Spawner
    pub fn bind_lifetime(mut self, owner: platform::process::LifetimeOwner) -> Self {
        self.lifetime_owner = Some(owner);
        self
    }

    /// What this host would enforce for the configured owner, before spawning.
    ///
    /// `None` means no binding was requested, which is not the same as a weak
    /// one: nothing will clean this child up on its owner's behalf at all.
    #[must_use]
    pub fn lifetime_enforcement(&self) -> Option<platform::process::LifetimeEnforcement> {
        self.lifetime_owner
            .map(platform::process::lifetime_enforcement_for)
    }

    /// Select the semantic scheduling band for the spawned process.
    ///
    /// The mapping stays private so callers do not need to conditionalize on
    /// Unix niceness versus Windows process priority classes.
    ///
    /// A band the host refuses fails the spawn: on Unix, raising priority
    /// (for example [`ProcessPriority::High`] without the privilege to lower
    /// niceness) reports `PermissionDenied`. Use [`Self::priority_best_effort`]
    /// when the band is a preference rather than a requirement.
    pub fn priority(mut self, priority: ProcessPriority) -> Self {
        self.priority = priority;
        self.priority_policy = PriorityPolicy::Required;
        self
    }

    /// Select a scheduling band that must never prevent the spawn.
    ///
    /// The band is applied at creation exactly like [`Self::priority`] where
    /// the host permits it. When the host refuses it with `PermissionDenied`,
    /// the spawn is retried once without a band, so the child runs in the
    /// band it inherits from the spawner. The retry re-runs any
    /// [`SpawnAdmission`], because each native spawn attempt is admitted.
    /// A retry that fails still reports its own error. [`ProcessPriority::Normal`]
    /// requests no band and so never retries.
    pub fn priority_best_effort(mut self, priority: ProcessPriority) -> Self {
        self.priority = priority;
        self.priority_policy = PriorityPolicy::BestEffort;
        self
    }

    /// Admit each native spawn attempt through a caller-owned exclusion.
    ///
    /// See [`SpawnAdmission`] for exactly when the permit is held. This
    /// applies to [`Self::spawn`] and [`Self::spawn_session`].
    /// [`run_bounded_command`] cannot place an admission around its native
    /// spawn and refuses a spec carrying one with `InvalidInput`.
    pub fn spawn_admission(mut self, admission: SpawnAdmission) -> Self {
        self.admission = Some(admission);
        self
    }

    /// Spawn using the canonical asynchronous platform operation.
    ///
    /// The private native substrate owns process creation, containment, and
    /// owner-death installation. This facade retains the semantic command
    /// description and never exposes the substrate's child or runtime types.
    pub async fn spawn(self) -> io::Result<PlatformChild> {
        process_adapter::spawn(self).await
    }

    /// Start a bounded streaming process session.
    ///
    /// This session owns direct-child lifecycle observation independently from
    /// its bounded stdout/stderr queue. Configure streams as [`StreamMode::Piped`]
    /// to receive their typed output events. The same optional child-owned
    /// process group and owner-death policy selected on this `SpawnSpec` are
    /// forwarded to the private native substrate. Session kill and drop policy
    /// target the direct child only; a descendant that retains a pipe is
    /// reported as output abandonment after grace rather than killed.
    pub async fn spawn_session(self, options: ProcessSessionOptions) -> io::Result<ProcessSession> {
        process_adapter::spawn_session(self, options).await
    }
}

/// Owned child handle returned by [`SpawnSpec::spawn`].
pub struct PlatformChild {
    inner: process_adapter::ProcessAdapter,
    lifetime: process_adapter::LifetimeBinding,
}

impl PlatformChild {
    pub(crate) fn new(
        inner: process_adapter::ProcessAdapter,
        lifetime: process_adapter::LifetimeBinding,
    ) -> Self {
        Self { inner, lifetime }
    }

    /// Which enforcement the requested lifetime binding actually obtained.
    ///
    /// `None` means none was requested. A caller that needs a guarantee no
    /// user-space participant has to survive should check
    /// [`LifetimeEnforcement::is_kernel_enforced`] here -- or, better, on
    /// [`SpawnSpec::lifetime_enforcement`] before there is a child to clean
    /// up.
    ///
    /// [`LifetimeEnforcement::is_kernel_enforced`]: platform::process::LifetimeEnforcement::is_kernel_enforced
    #[must_use]
    pub fn lifetime_enforcement(&self) -> Option<platform::process::LifetimeEnforcement> {
        self.lifetime.enforcement()
    }

    /// Return the operating-system process identifier while this handle has
    /// not observed a terminal lifecycle result.
    ///
    /// A successful [`Self::wait`] or [`Self::kill`] clears this value so a
    /// completed child PID is not presented as a reusable live identity.
    pub fn id(&self) -> Option<u32> {
        self.inner.pid()
    }

    /// Wait for completion without capturing output.
    #[deprecated(note = "waits forever for a live child; use `wait_within` (#433)")]
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait_unbounded().await
    }

    async fn wait_unbounded(&mut self) -> io::Result<ExitStatus> {
        let exit = self.inner.wait().await;
        if exit.is_ok() {
            // Nothing left to protect: keeping the owner watch open past this
            // would hold a kernel subscription for a process that is gone.
            self.lifetime.release();
        }
        exit
    }

    /// Wait at most `timeout` for completion without capturing output.
    ///
    /// An expired wait leaves the child running and this handle usable: the
    /// caller decides whether to wait again or [`Self::kill`] it.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::TimedOut`] if the child is still running when
    /// `timeout` elapses, or the error the child's wait reports. Requires
    /// a runtime with its timer enabled.
    pub async fn wait_within(&mut self, timeout: std::time::Duration) -> io::Result<ExitStatus> {
        async_engine::timeout(timeout, self.wait_unbounded())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "child did not exit within its timeout"))?
    }

    /// Terminate the child and wait for its exit.
    pub async fn kill(&mut self) -> io::Result<()> {
        let killed = self.inner.kill().await;
        if killed.is_ok() {
            self.lifetime.release();
        }
        killed
    }

    /// Request graceful termination for the child-owned process group.
    ///
    /// Returns `Ok(false)` unless [`SpawnSpec::create_process_group`] was
    /// enabled, or when the child has already exited.
    pub async fn terminate_group_soft(&self) -> io::Result<bool> {
        self.inner.terminate_group_soft().await
    }

    /// Write bytes to piped stdin.
    pub async fn write_stdin(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_stdin(bytes).await
    }

    /// Close the piped stdin handle, delivering EOF to the child.
    ///
    /// The substrate makes a successful close idempotent. An inherited or
    /// null stdin reports that no writable child pipe exists.
    pub async fn close_stdin(&mut self) -> io::Result<()> {
        self.inner.close_stdin().await
    }

    /// Capture piped stdout and stderr after the child exits, retaining at
    /// most one aggregate byte limit in the returned [`ProcessOutput`].
    ///
    /// The limit covers both returned streams together: on success,
    /// `stdout.len() + stderr.len()` is at most `limit`. Excess bytes are not
    /// returned, and the completed capture returns
    /// [`ProcessCaptureError::OutputLimitExceeded`] without partial output.
    /// The running-process 4.10.9 actor separately keeps a fixed 16 MiB
    /// diagnostic output log while draining. That log evicts older records,
    /// is not configurable through this substrate API, and is independent of
    /// this method's returned-output limit.
    /// This operation waits for both streams to reach EOF and the child to
    /// exit naturally; it does not terminate a child merely because retained
    /// output reached the limit.
    #[deprecated(note = "waits forever for a live child or a descendant-held pipe; use `wait_with_output_bounded_within` (#433)")]
    pub async fn wait_with_output_bounded(
        mut self,
        limit: usize,
    ) -> Result<ProcessOutput, ProcessCaptureError> {
        self.inner.capture_bounded(limit).await
    }

    /// [`Self::wait_with_output_bounded`] with a time budget.
    ///
    /// If the child has not exited and closed both streams within `timeout`,
    /// the capture is abandoned, the child is killed, and this returns
    /// [`ProcessCaptureError::Io`] with [`io::ErrorKind::TimedOut`]; output
    /// captured so far is discarded.
    pub async fn wait_with_output_bounded_within(
        mut self,
        limit: usize,
        timeout: std::time::Duration,
    ) -> Result<ProcessOutput, ProcessCaptureError> {
        match async_engine::timeout(timeout, self.inner.capture_bounded(limit)).await {
            Ok(result) => result,
            Err(_) => {
                self.kill().await.map_err(ProcessCaptureError::Io)?;
                Err(ProcessCaptureError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "child output was not captured within its timeout",
                )))
            }
        }
    }
}

/// Post-exit output-drain policy for [`ProcessSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessPostExitDrain {
    /// Preserve strict historical EOF behavior, even when a descendant holds
    /// an inherited output pipe after the direct child has exited.
    #[deprecated(
        note = "a descendant holding the pipe makes this wait forever; use `AbandonAfter` (#433)"
    )]
    WaitForEof,
    /// Abandon a still-pending pipe read after this cumulative post-exit
    /// budget. Buffered bytes remain observable and bounded queue delivery
    /// does not consume the budget.
    AbandonAfter(std::time::Duration),
}

/// Explicit bounds and terminal-owner policy for [`ProcessSession`].
///
/// The output queue is lossless: a full queue applies backpressure to the
/// matching child pipe instead of discarding compiler output. The post-exit
/// drain policy limits only a pending pipe read after direct-child exit;
/// delivery blocked by the bounded queue does not spend that budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessSessionOptions {
    /// Maximum queued stdout/stderr chunks before the session applies
    /// backpressure. Must be greater than zero.
    pub max_queued_chunks: usize,
    /// Maximum bytes in one stdin or output chunk. Must be greater than zero.
    pub max_chunk_bytes: usize,
    /// Pipe-read behavior after direct-child exit.
    pub post_exit_drain: ProcessPostExitDrain,
    /// Whether dropping the terminal session owner terminates and reaps the
    /// direct child. This does not target descendants. `false` detaches
    /// delivery while the private owner drains output and reaps naturally.
    pub kill_on_drop: bool,
}

impl Default for ProcessSessionOptions {
    fn default() -> Self {
        Self {
            max_queued_chunks: 256,
            max_chunk_bytes: 8 * 1024,
            post_exit_drain: ProcessPostExitDrain::AbandonAfter(std::time::Duration::from_millis(
                250,
            )),
            kill_on_drop: true,
        }
    }
}

/// One lossless output payload from a [`ProcessSession`].
///
/// The distinct variants keep stdout and stderr typed at the facade boundary;
/// callers never receive a raw pipe or a backend stream identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessOutputChunk {
    /// Bytes read from the child's stdout pipe.
    Stdout(Vec<u8>),
    /// Bytes read from the child's stderr pipe.
    Stderr(Vec<u8>),
}

impl ProcessOutputChunk {
    /// Borrow the chunk's raw bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Stdout(bytes) | Self::Stderr(bytes) => bytes,
        }
    }
}

/// Portable details for an output-stream I/O failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutputFault {
    kind: std::io::ErrorKind,
    message: String,
    raw_os_error: Option<i32>,
}

impl ProcessOutputFault {
    /// Describe an output-stream failure from its portable parts.
    ///
    /// Sessions construct these themselves; this is public so a consumer can
    /// build the fault its own handling code receives -- to test how it turns
    /// a [`ProcessOutputCompletion::StdoutError`] into its own error type,
    /// say -- without provoking a real pipe failure.
    pub fn new(
        kind: std::io::ErrorKind,
        message: impl Into<String>,
        raw_os_error: Option<i32>,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            raw_os_error,
        }
    }

    /// Portable category of the failed stream operation.
    #[must_use]
    pub const fn kind(&self) -> std::io::ErrorKind {
        self.kind
    }

    /// The underlying I/O failure text, preserved for durable diagnostics.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Native operating-system error, when the stream operation exposed one.
    #[must_use]
    pub const fn raw_os_error(&self) -> Option<i32> {
        self.raw_os_error
    }
}

/// Terminal state for one [`ProcessSession`] output stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessOutputCompletion {
    /// Stdout reached EOF normally.
    StdoutEof,
    /// Stderr reached EOF normally.
    StderrEof,
    /// Stdout remained open after direct-child exit and the configured grace
    /// elapsed, so the facade explicitly closed its reader.
    StdoutAbandoned,
    /// Stderr remained open after direct-child exit and the configured grace
    /// elapsed, so the facade explicitly closed its reader.
    StderrAbandoned,
    /// Stdout reading ended with an I/O fault.
    StdoutError(ProcessOutputFault),
    /// Stderr reading ended with an I/O fault.
    StderrError(ProcessOutputFault),
}

/// One output event from a [`ProcessSession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessOutputEvent {
    /// A bounded stdout or stderr payload.
    Chunk(ProcessOutputChunk),
    /// A stream completed, was explicitly abandoned after grace, or failed.
    Completion(ProcessOutputCompletion),
}

/// Terminal-owner facade for a bounded streaming child process.
///
/// Lifecycle controls do not wait on stdout, stderr, or stdin work. In
/// particular, [`Self::wait`] returns when the direct child is reaped even if
/// a descendant retains an inherited output pipe; that pipe later reports an
/// abandonment completion after [`ProcessSessionOptions::post_exit_drain`].
/// Direct kill and drop policy likewise address only the direct child; they do
/// not promise descendant-tree cleanup.
pub struct ProcessSession {
    inner: process_adapter::ProcessSessionAdapter,
    lifetime: process_adapter::LifetimeBinding,
}

impl ProcessSession {
    /// Which enforcement the requested lifetime binding actually obtained.
    ///
    /// `None` means none was requested. See
    /// [`PlatformChild::lifetime_enforcement`].
    #[must_use]
    pub fn lifetime_enforcement(&self) -> Option<platform::process::LifetimeEnforcement> {
        self.lifetime.enforcement()
    }

    pub(crate) fn new(
        inner: process_adapter::ProcessSessionAdapter,
        lifetime: process_adapter::LifetimeBinding,
    ) -> Self {
        Self { inner, lifetime }
    }

    /// Return the direct child's launch-bound numeric identifier.
    ///
    /// It is diagnostic only. Every lifecycle operation remains bound to the
    /// private child identity rather than addressing this reusable number.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.inner.pid()
    }

    /// Receive the next bounded output event.
    ///
    /// `None` means every configured output stream has completed, been
    /// abandoned, or detached. A slow caller backpressures the child pipes;
    /// it never causes unreported output eviction.
    ///
    /// Only one output receive may be active at a time. It is otherwise
    /// independent from lifecycle controls, so callers may await direct-child
    /// exit, kill, or CPU time while this receive waits on a descendant-held
    /// pipe.
    pub async fn next_output(&self) -> Option<ProcessOutputEvent> {
        self.inner.next_output().await
    }

    /// Stop output delivery and await cleanup of native output readers.
    ///
    /// Discards queued output and waits for the output pumps and outstanding
    /// native reads to finish cleanup. Already-returned events remain owned by
    /// their callers. Success does not mean the child or its descendants have
    /// exited; use lifecycle controls separately.
    ///
    /// This wakes a pending [`Self::next_output`] receive. Multiple shutdown
    /// callers may wait concurrently. Once polled, cancelling this future does
    /// not undo the shutdown request; retrying observes the same cleanup result.
    /// An error is not acknowledgement that native output storage was reclaimed.
    pub async fn shutdown_output(&self) -> io::Result<()> {
        self.inner.shutdown_output().await
    }

    /// Wait only for the direct child to exit and be reaped.
    ///
    /// This is independent from output completion, so it remains observable
    /// when descendants retain inherited pipe ends.
    pub async fn wait(&self) -> io::Result<ProcessSessionExit> {
        let exit = self.inner.wait().await;
        if exit.is_ok() {
            // Nothing left to protect, and a session is meant to be held well
            // past this while output drains -- so the owner watch would
            // otherwise outlive the child by the whole drain.
            self.lifetime.release();
        }
        exit
    }

    /// Observe whether the direct child has already been reaped.
    pub async fn poll(&self) -> io::Result<Option<ProcessSessionExit>> {
        self.inner.poll().await
    }

    /// Hard-terminate and reap the direct child without waiting for output
    /// delivery or a blocked stdin writer. This does not target descendants;
    /// an inherited descendant pipe is explicitly abandoned after grace.
    pub async fn kill(&self) -> io::Result<()> {
        let killed = self.inner.kill().await;
        if killed.is_ok() {
            self.lifetime.release();
        }
        killed
    }

    /// Request graceful termination for the child-owned process group.
    ///
    /// Returns `false` unless [`SpawnSpec::create_process_group`] was enabled,
    /// or when the direct child has already exited.
    pub async fn terminate_group_soft(&self) -> io::Result<bool> {
        self.inner.terminate_group_soft().await
    }

    /// Queue one bounded stdin payload and flush it to the child's piped
    /// stdin. A payload larger than `max_chunk_bytes` returns `InvalidInput`.
    pub async fn write_stdin(&self, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_stdin(bytes).await
    }

    /// Close piped stdin, delivering EOF to the direct child.
    pub async fn close_stdin(&self) -> io::Result<()> {
        self.inner.close_stdin().await
    }

    /// Sample direct-child CPU time when the host can still prove its launch
    /// identity. Unsupported or no-longer-observable processes return `None`.
    pub async fn cpu_time(&self) -> io::Result<Option<std::time::Duration>> {
        self.inner.cpu_time().await
    }
}

/// Status-preserving result of bounded child-output capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    /// The operating system's unmodified child status.
    pub status: ExitStatus,
    /// Bytes captured from stdout.
    pub stdout: Vec<u8>,
    /// Bytes captured from stderr.
    pub stderr: Vec<u8>,
}

/// Failure returned by [`PlatformChild::wait_with_output_bounded`].
#[derive(Debug, thiserror::Error)]
pub enum ProcessCaptureError {
    /// The returned stdout/stderr pair would have exceeded its shared limit.
    #[error("captured process output exceeded the {limit}-byte limit")]
    OutputLimitExceeded {
        /// Aggregate stdout/stderr capture limit.
        limit: usize,
    },
    /// Waiting for or capturing from the started child failed.
    #[error("{0}")]
    Io(#[source] io::Error),
}

/// Status-preserving output from [`run_bounded_command`].
///
/// This is a one-shot operation: the private substrate starts a contained
/// child, captures both output streams, and reaps it before returning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedProcessOutput {
    /// Raw child termination result represented without a platform handle.
    pub exit: BoundedProcessExit,
    /// Bytes captured from stdout.
    pub stdout: Vec<u8>,
    /// Bytes captured from stderr.
    pub stderr: Vec<u8>,
}

/// Platform-neutral raw termination result for a bounded command.
///
/// A normal exit is its native exit code. On Unix, signal termination is the
/// negative signal number, matching the private substrate's exact contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedProcessExit {
    raw_code: i32,
}

impl BoundedProcessExit {
    pub(crate) const fn new(raw_code: i32) -> Self {
        Self { raw_code }
    }

    /// Return the unmodified native exit code.
    pub const fn raw_code(self) -> i32 {
        self.raw_code
    }

    /// Whether the command exited successfully.
    pub const fn is_success(self) -> bool {
        self.raw_code == 0
    }
}

/// Facade-owned native session termination result.
///
/// This deliberately differs from [`BoundedProcessExit`], the legacy one-shot
/// bounded-run code. Session clients retain both structured exit/signal
/// meaning and every bit of the native status word, which is needed to
/// classify Windows process failures without exposing `ExitStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessSessionExit {
    exit_code: Option<i32>,
    signal: Option<i32>,
    native_status: u32,
}

impl ProcessSessionExit {
    pub(crate) const fn from_native(
        exit_code: Option<i32>,
        signal: Option<i32>,
        native_status: u32,
    ) -> Self {
        Self {
            exit_code,
            signal,
            native_status,
        }
    }

    /// Return the ordinary process exit code, if the process was not killed
    /// by a Unix signal.
    #[must_use]
    pub const fn exit_code(self) -> Option<i32> {
        self.exit_code
    }

    /// Return the Unix terminating signal when the host reported one.
    ///
    /// Windows and ordinary exits return `None`.
    #[must_use]
    pub const fn signal(self) -> Option<i32> {
        self.signal
    }

    /// Return the unmodified host-native status word.
    ///
    /// On Windows this is the exact `DWORD` process exit status. On Unix this
    /// is the native wait-status word. It is intentionally `u32` so every
    /// Windows status bit survives signed integer conversion.
    #[must_use]
    pub const fn native_status(self) -> u32 {
        self.native_status
    }

    /// Whether the process completed successfully.
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self.exit_code, Some(0))
    }

    /// Rebuild the host's [`ExitStatus`] from the native status word.
    ///
    /// This is lossless: the result reports the same code, signal, and
    /// success as the status the host returned, so a caller assembling a
    /// [`std::process::Output`] keeps the child's exact termination.
    #[must_use]
    pub fn exit_status(self) -> ExitStatus {
        platform_imp::process_exit_status_from_native(self.native_status)
    }
}

/// Backward-compatible spelling for the one-shot bounded command exit.
pub type ProcessExit = BoundedProcessExit;

/// Failure returned by [`run_bounded_command`].
#[derive(Debug, thiserror::Error)]
pub enum BoundedProcessError {
    /// The command exceeded its requested wall-clock timeout and was cleaned up.
    #[error("bounded command timed out")]
    TimedOut,
    /// Captured stdout and stderr exceeded their shared byte limit.
    #[error("bounded command output exceeded the {limit}-byte limit")]
    OutputLimitExceeded {
        /// Aggregate stdout/stderr capture limit.
        limit: usize,
    },
    /// The command could not be started.
    #[error("{0}")]
    Spawn(#[source] io::Error),
    /// Capturing, waiting for, or cleaning up the command failed.
    #[error("{0}")]
    Io(#[source] io::Error),
}

/// Failure returned by [`run_bounded_command_async`].
#[derive(Debug, thiserror::Error)]
pub enum BoundedProcessAsyncError {
    /// The one-shot command returned a semantic bounded-process error.
    #[error("{0}")]
    Bounded(#[source] BoundedProcessError),
    /// The canonical async engine could not join its blocking-lane task.
    #[error("{0}")]
    Task(#[source] async_engine::TaskError),
}

/// Run a contained one-shot command with bounded capture.
///
/// The limit covers stdout and stderr together. The deadline is required:
/// pass a [`std::time::Duration`] (or `Some(duration)`). `None` no longer
/// means "no deadline"; it is refused with [`std::io::ErrorKind::InvalidInput`]
/// before anything is spawned.
/// Timeout and output-limit failures request hard cleanup before return.
pub fn run_bounded_command(
    spec: SpawnSpec,
    timeout: impl Into<Option<std::time::Duration>>,
    output_limit: usize,
) -> Result<BoundedProcessOutput, BoundedProcessError> {
    process_adapter::run_bounded(spec, timeout.into(), output_limit)
}

/// Run [`run_bounded_command`] on the canonical async engine's blocking lane.
///
/// This keeps the current runtime responsive without constructing a second
/// runtime or exposing its implementation types. The deadline is required
/// exactly as for [`run_bounded_command`].
pub async fn run_bounded_command_async(
    spec: SpawnSpec,
    timeout: impl Into<Option<std::time::Duration>>,
    output_limit: usize,
) -> Result<BoundedProcessOutput, BoundedProcessAsyncError> {
    let timeout = timeout.into();
    async_engine::launch_blocking(move || run_bounded_command(spec, timeout, output_limit))
        .await
        .map_err(BoundedProcessAsyncError::Task)?
        .map_err(BoundedProcessAsyncError::Bounded)
}

/// Build a shell command using the host platform's supported shell.
pub fn shell_spec(command: impl AsRef<OsStr>) -> SpawnSpec {
    platform_imp::shell_spec(command.as_ref())
}

#[cfg(test)]
mod tests {
    use super::{
        run_bounded_command, run_bounded_command_async, shell_spec, BoundedProcessAsyncError,
        BoundedProcessError, BoundedProcessOutput, ProcessPriority, SpawnSpec, StreamMode,
    };
    #[cfg(target_os = "linux")]
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    #[cfg(target_os = "linux")]
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn bounded_error_messages_and_sources_stay_stable() {
        use std::error::Error as _;
        use std::io;

        let timeout = BoundedProcessError::TimedOut;
        assert_eq!(timeout.to_string(), "bounded command timed out");
        assert!(timeout.source().is_none());

        let spawn = BoundedProcessError::Spawn(io::Error::other("spawn sentinel"));
        assert_eq!(spawn.to_string(), "spawn sentinel");
        assert_eq!(spawn.source().unwrap().to_string(), "spawn sentinel");

        let async_error = BoundedProcessAsyncError::Bounded(spawn);
        assert_eq!(async_error.to_string(), "spawn sentinel");
        assert_eq!(async_error.source().unwrap().to_string(), "spawn sentinel");

        let capture = super::ProcessCaptureError::Io(io::Error::other("capture sentinel"));
        assert_eq!(capture.to_string(), "capture sentinel");
        assert_eq!(capture.source().unwrap().to_string(), "capture sentinel");
    }

    fn fixture_command() -> SpawnSpec {
        #[cfg(windows)]
        {
            shell_spec("echo async-platform-internal")
        }
        #[cfg(not(windows))]
        {
            shell_spec("printf async-platform-internal")
        }
    }

    #[test]
    fn semantic_priority_bands_have_exact_private_substrate_mappings() {
        #[cfg(windows)]
        assert_eq!(ProcessPriority::High.substrate_nice(), Some(-15));
        #[cfg(not(windows))]
        assert_eq!(ProcessPriority::High.substrate_nice(), Some(-5));
        assert_eq!(ProcessPriority::Normal.substrate_nice(), None);
        #[cfg(windows)]
        assert_eq!(ProcessPriority::Low.substrate_nice(), Some(1));
        #[cfg(not(windows))]
        assert_eq!(ProcessPriority::Low.substrate_nice(), Some(10));
        #[cfg(windows)]
        assert_eq!(ProcessPriority::Idle.substrate_nice(), Some(15));
        #[cfg(not(windows))]
        assert_eq!(ProcessPriority::Idle.substrate_nice(), Some(19));
    }

    #[tokio::test]
    async fn blessed_spawn_captures_output_without_sync_wait() {
        let output = fixture_command()
            .stdout(StreamMode::Piped)
            .stderr(StreamMode::Piped)
            .spawn()
            .await
            .expect("spawn")
            .wait_with_output_bounded_within(1024, std::time::Duration::from_secs(5))
            .await
            .expect("wait with output");

        assert!(output.status.success());
        let expected = if cfg!(windows) {
            b"async-platform-internal\r\n".as_slice()
        } else {
            b"async-platform-internal".as_slice()
        };
        assert_eq!(output.stdout, expected);
        assert!(output.stderr.is_empty());
    }

    #[tokio::test]
    async fn blessed_spawn_reports_missing_program() {
        let result = SpawnSpec::new("kernal-api-program-that-does-not-exist")
            .spawn()
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn bounded_output_closes_owned_stdin() {
        #[cfg(windows)]
        let spec = shell_spec("more > nul & echo done");
        #[cfg(not(windows))]
        let spec = shell_spec("cat > /dev/null; printf done");

        let output = spec
            .stdin(StreamMode::Piped)
            .stdout(StreamMode::Piped)
            .stderr(StreamMode::Piped)
            .spawn()
            .await
            .expect("spawn")
            .wait_with_output_bounded_within(1024, std::time::Duration::from_secs(2))
            .await
            .expect("stdin is closed for one-shot output");

        let expected = if cfg!(windows) {
            b"done\r\n".as_slice()
        } else {
            b"done".as_slice()
        };
        assert_eq!(output.stdout, expected);
    }

    fn nonzero_capture_command() -> SpawnSpec {
        #[cfg(windows)]
        {
            shell_spec("echo bounded-stdout & echo bounded-stderr 1>&2 & exit /b 7")
        }
        #[cfg(not(windows))]
        {
            shell_spec("printf bounded-stdout; printf bounded-stderr >&2; exit 7")
        }
    }

    fn slow_command() -> SpawnSpec {
        #[cfg(windows)]
        {
            shell_spec("ping 127.0.0.1 -n 31 > nul")
        }
        #[cfg(not(windows))]
        {
            shell_spec("sleep 30")
        }
    }

    fn overflow_command() -> SpawnSpec {
        #[cfg(windows)]
        {
            shell_spec("for /L %i in (1,1,1000000000) do @echo x")
        }
        #[cfg(not(windows))]
        {
            shell_spec("yes x")
        }
    }

    #[test]
    fn bounded_command_requires_a_duration_deadline() {
        let _: fn(SpawnSpec, Duration, usize) -> Result<BoundedProcessOutput, BoundedProcessError> =
            run_bounded_command;
    }

    #[cfg(target_os = "linux")]
    fn helper_test_name(name: &str) -> String {
        match module_path!().split_once("::") {
            Some((_crate_root, module)) => format!("{module}::{name}"),
            None => name.to_owned(),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn helper_bounded_owner_death_child() {
        if std::env::var("KERNAL_API_BOUNDED_OWNER_DEATH_HELPER")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }

        let result = run_bounded_command(
            shell_spec("echo $$ > \"$KERNAL_API_BOUNDED_OWNER_DEATH_PID_FILE\"; exec sleep 30")
                .kill_when_owner_dies(true),
            Duration::from_secs(120),
            4096,
        );
        assert!(
            matches!(result, Err(BoundedProcessError::TimedOut)),
            "helper must run to its deadline unless the owner dies: {result:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_command_forwards_owner_death_to_the_native_runner() {
        let temporary = tempfile::tempdir().expect("temporary pid directory");
        let pid_file = temporary.path().join("bounded-owner-death.pid");
        let mut owner = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg(helper_test_name("helper_bounded_owner_death_child"))
            .arg("--nocapture")
            .env("KERNAL_API_BOUNDED_OWNER_DEATH_HELPER", "1")
            .env("KERNAL_API_BOUNDED_OWNER_DEATH_PID_FILE", &pid_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bounded owner helper");

        let startup_deadline = Instant::now() + Duration::from_secs(5);
        let child_pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                break pid
                    .trim()
                    .parse::<u32>()
                    .expect("helper wrote a numeric child pid");
            }
            if let Some(status) = owner.try_wait().expect("check helper") {
                panic!("bounded owner exited before reporting its child pid: {status}");
            }
            assert!(
                Instant::now() < startup_deadline,
                "bounded owner did not report a child pid"
            );
            thread::sleep(Duration::from_millis(20));
        };

        owner.kill().expect("kill bounded owner");
        owner.wait().expect("reap bounded owner");

        let death_deadline = Instant::now() + Duration::from_secs(5);
        while linux_pid_is_running(child_pid) {
            assert!(
                Instant::now() < death_deadline,
                "bounded child {child_pid} survived owner death"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(target_os = "linux")]
    fn linux_pid_is_running(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                let state = stat
                    .rsplit_once(") ")
                    .and_then(|(_, tail)| tail.as_bytes().first().copied());
                state != Some(b'Z')
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => {
                (unsafe { libc::kill(pid as libc::pid_t, 0) == 0 })
                    || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
            }
        }
    }

    #[test]
    fn bounded_command_refuses_a_missing_deadline_before_spawning() {
        let marker = std::env::temp_dir().join(format!(
            "kernal-api-no-deadline-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&marker);
        // Redirection with a double-quoted path means the same in sh and cmd.
        let spec = shell_spec(format!("echo x > \"{}\"", marker.display()));

        let error = run_bounded_command(spec, None, 1024)
            .expect_err("a run without a deadline must be refused");

        assert!(
            matches!(&error, BoundedProcessError::Io(io) if io.kind() == std::io::ErrorKind::InvalidInput),
            "unexpected error: {error:?}"
        );
        assert!(!marker.exists(), "the refused command must not have run");
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_strengthens_a_disabled_group_policy_for_containment() {
        let output = run_bounded_command(
            shell_spec("pgid=$(ps -o pgid= -p $$ | tr -d '[:space:]'); test \"$pgid\" = \"$$\"")
                .create_process_group(false),
            Duration::from_secs(2),
            1024,
        )
        .expect("bounded capture must strengthen group containment");

        assert!(output.exit.is_success());
    }

    #[test]
    fn bounded_command_preserves_separate_streams_and_raw_nonzero_exit() {
        let output = run_bounded_command(
            nonzero_capture_command()
                .stdin(StreamMode::Inherit)
                .stdout(StreamMode::Inherit)
                .stderr(StreamMode::Null),
            Duration::from_secs(2),
            1024,
        )
        .expect("bounded nonzero command completes");

        assert_eq!(output.exit.raw_code(), 7);
        assert!(!output.exit.is_success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("bounded-stdout"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("bounded-stderr"));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_preserves_cwd_clear_env_and_environment_overrides() {
        let temporary_directory = tempfile::tempdir().expect("temporary cwd");
        // macOS exposes /var as a symlink to /private/var, and `pwd` reports
        // the resolved path. Compare the canonical form so the assertion
        // reflects directory identity rather than that implementation detail.
        let expected_cwd = std::fs::canonicalize(temporary_directory.path())
            .expect("canonical temporary cwd")
            .to_string_lossy()
            .into_owned();
        let output = run_bounded_command(
            SpawnSpec::new("/bin/sh")
                .arg("-c")
                .arg("pwd; /usr/bin/env")
                .current_dir(temporary_directory.path())
                .clear_env(true)
                .env("KERNAL_API_BOUNDED_OVERRIDE", "expected-value"),
            Duration::from_secs(2),
            1024,
        )
        .expect("cwd and environment configuration are preserved");

        let environment = String::from_utf8(output.stdout).expect("fixture output is UTF-8");
        let mut lines = environment.lines();
        assert_eq!(lines.next(), Some(expected_cwd.as_str()));
        assert!(lines.any(|line| line == "KERNAL_API_BOUNDED_OVERRIDE=expected-value"));
        assert!(
            !environment.lines().any(|line| line.starts_with("HOME=")),
            "env_clear must remove the inherited HOME variable: {environment}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_preserves_the_negative_signal_exit_code() {
        let output = run_bounded_command(shell_spec("kill -TERM $$"), Duration::from_secs(2), 1024)
            .expect("signalled command completes");

        assert_eq!(output.exit.raw_code(), -libc::SIGTERM);
        assert!(!output.exit.is_success());
    }

    #[test]
    fn bounded_command_timeout_hard_kills_and_reaps_promptly() {
        let started = Instant::now();
        let error = run_bounded_command(slow_command(), Duration::from_millis(100), 1024)
            .expect_err("slow command must time out after hard cleanup");

        assert!(matches!(error, BoundedProcessError::TimedOut));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout cleanup was not bounded: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn bounded_command_overflow_requests_hard_kill_and_returns_promptly() {
        let started = Instant::now();
        let error = run_bounded_command(overflow_command(), Duration::from_secs(2), 16)
            .expect_err("unbounded output must exceed the aggregate capture limit");

        assert!(matches!(
            error,
            BoundedProcessError::OutputLimitExceeded { limit: 16 }
        ));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "overflow cleanup was not bounded: {:?}",
            started.elapsed()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn bounded_command_applies_the_capture_limit_across_both_streams() {
        let error = run_bounded_command(
            shell_spec("printf 123456; printf abcdef >&2"),
            Duration::from_secs(2),
            10,
        )
        .expect_err("six stdout bytes plus six stderr bytes must exceed one aggregate limit");

        assert!(matches!(
            error,
            BoundedProcessError::OutputLimitExceeded { limit: 10 }
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_command_timeout_cancels_readers_held_by_an_escaped_descendant() {
        let started = Instant::now();
        let error = run_bounded_command(
            shell_spec("setsid sh -c 'sleep 60' & sleep 30"),
            Duration::from_millis(100),
            1024,
        )
        .expect_err("outer process must time out");

        // The escaped descendant holds the capture pipes for 60 s. Returning
        // within 10 s proves the readers were cancelled rather than drained to
        // EOF, with a margin that host load cannot consume.
        assert!(matches!(error, BoundedProcessError::TimedOut));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "escaped descendant retained bounded-capture readers for {:?}",
            started.elapsed()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_command_overflow_cancels_readers_held_by_an_escaped_descendant() {
        let started = Instant::now();
        let error = run_bounded_command(
            shell_spec("setsid sh -c 'sleep 60' & yes x"),
            Duration::from_secs(30),
            16,
        )
        .expect_err("overflowing parent must not wait for an escaped pipe holder");

        // The escaped descendant holds the capture pipes for 60 s and the
        // deadline is 30 s. An overflow error returned within 10 s proves the
        // overflow path cancelled the readers itself, without the deadline or
        // the descendant's exit doing it.
        assert!(matches!(
            error,
            BoundedProcessError::OutputLimitExceeded { limit: 16 }
        ));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "escaped descendant retained bounded-capture readers for {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn bounded_command_reports_a_missing_program_as_a_facade_spawn_error() {
        let error = run_bounded_command(
            SpawnSpec::new("kernal-api-bounded-program-that-does-not-exist"),
            Duration::from_secs(2),
            1024,
        )
        .expect_err("missing program must fail before capture");

        assert!(matches!(error, BoundedProcessError::Spawn(_)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_command_preserves_non_utf8_program_args_and_environment() {
        use std::os::unix::ffi::OsStringExt;

        let output = run_bounded_command(
            SpawnSpec::new(std::ffi::OsString::from_vec(b"/bin/sh".to_vec()))
                .arg("-c")
                .arg("printf %s \"$KERNAL_API_BOUNDED_BYTES\"")
                .env(
                    std::ffi::OsString::from_vec(b"KERNAL_API_BOUNDED_BYTES".to_vec()),
                    std::ffi::OsString::from_vec(b"value-\xff".to_vec()),
                ),
            Duration::from_secs(2),
            1024,
        )
        .expect("non-UTF-8 command inputs are preserved");

        assert_eq!(output.stdout, b"value-\xff");
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_preserves_working_directory() {
        let directory = tempfile::tempdir().expect("temporary working directory");
        let output = run_bounded_command(
            SpawnSpec::new("/bin/sh")
                .arg("-c")
                .arg("pwd")
                .current_dir(directory.path()),
            Some(Duration::from_secs(2)),
            1024,
        )
        .expect("command with explicit working directory");

        assert_eq!(
            std::str::from_utf8(&output.stdout)
                .expect("pwd emits a UTF-8 path")
                .trim_end(),
            std::fs::canonicalize(directory.path())
                .expect("temporary working directory remains available")
                .to_string_lossy(),
        );
    }

    #[test]
    fn bounded_child_wait_times_out_and_leaves_the_child_killable() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        runtime.run_within(async {
            let mut child = slow_command().spawn().await.expect("spawn slow child");
            let started = Instant::now();
            let error = child
                .wait_within(Duration::from_millis(200))
                .await
                .expect_err("a 30 s child must outlive a 200 ms wait");
            let waited = started.elapsed();
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
            assert!(waited >= Duration::from_millis(200), "returned early after {waited:?}");
            assert!(child.id().is_some(), "a timed-out wait must not report the child gone");
            child.kill().await.expect("kill after an expired wait");
            assert!(child.id().is_none());
        }, std::time::Duration::from_secs(60)).expect("test runtime budget");
    }

    #[test]
    fn bounded_capture_times_out_and_kills_the_child() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        runtime.run_within(async {
            let child = slow_command().spawn().await.expect("spawn slow child");
            let started = Instant::now();
            let error = child
                .wait_with_output_bounded_within(1024, Duration::from_millis(200))
                .await
                .expect_err("a 30 s child must outlive a 200 ms capture");
            let waited = started.elapsed();
            assert!(
                matches!(&error, crate::ProcessCaptureError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut),
                "{error}"
            );
            assert!(waited >= Duration::from_millis(200), "returned early after {waited:?}");
            assert!(waited < Duration::from_secs(20), "waited for natural exit: {waited:?}");
        }, std::time::Duration::from_secs(60)).expect("test runtime budget");
    }

    #[test]
    fn bounded_command_async_uses_the_current_runtime_blocking_lane() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        runtime.run_within(async {
            // A task on the same current-thread runtime counts timer ticks
            // while the command runs. If the bounded command blocked the
            // runtime thread, this task could not be polled until the await
            // below returned, and no tick could be observed when it does.
            let ticks = Arc::new(Mutex::new(0_u32));
            let ticks_for_task = Arc::clone(&ticks);
            let _ticker = crate::async_engine::launch(async move {
                loop {
                    crate::async_engine::sleep(Duration::from_millis(10)).await;
                    *ticks_for_task.lock().expect("ticker lock") += 1;
                }
            });

            let error = run_bounded_command_async(slow_command(), Duration::from_secs(2), 1024)
                .await
                .expect_err("slow command must time out through the blocking lane");

            // Read before yielding again: every tick counted here happened
            // while the command was still in flight.
            let observed = *ticks.lock().expect("ticker lock");
            assert!(matches!(
                error,
                BoundedProcessAsyncError::Bounded(BoundedProcessError::TimedOut)
            ));
            assert!(
                observed >= 2,
                "current-thread runtime made no timer progress while the command ran: {observed} ticks"
            );
        }, std::time::Duration::from_secs(60)).expect("test runtime budget");
    }
}
    }
}
