//! Windows implementation root for the process capability.

#[cfg(feature = "global-hotkey")]
#[path = "platform_win/global_hotkey.rs"]
pub(crate) mod global_hotkey;
#[cfg(feature = "app-catalog")]
#[path = "platform_win/app_catalog.rs"]
pub(crate) mod app_catalog;
#[cfg(feature = "desktop-input")]
#[path = "platform_win/desktop_input.rs"]
pub(crate) mod desktop_input;

#[cfg(feature = "desktop-input-daemon")]
#[path = "platform_win/input_daemon.rs"]
pub(crate) mod input_daemon;

#[path = "platform_win/dylib.rs"]
pub(crate) mod dylib;
#[cfg(feature = "control-signal")]
#[path = "platform_win/control_signal.rs"]
pub(crate) mod control_signal;

#[cfg(feature = "system-tray")]
#[path = "platform_win/system_tray.rs"]
pub(crate) mod system_tray;
#[cfg(feature = "gfx-egui")]
#[path = "platform_win/gfx_dock.rs"]
pub(crate) mod gfx_dock;


#[cfg(feature = "wasm-sketch-worker")]
#[path = "platform_win/scratch_directory.rs"]
pub(crate) mod scratch_directory;

#[cfg(feature = "tauri-webview")]
#[path = "platform_win/viewport_capture.rs"]
pub(crate) mod viewport_capture;

#[cfg(feature = "tauri-webview")]
#[path = "platform_win/webview_window.rs"]
pub(crate) mod webview_window;

#[path = "platform_win/autostart.rs"]
pub(crate) mod autostart;

#[path = "platform_win/resources.rs"]
pub(crate) mod resources;
pub use resources::{
    available_space as resources_available_space,
    fd_exhaustion_error as resources_fd_exhaustion_error,
    inode_capacity as resources_inode_capacity,
    signals_fd_exhaustion as resources_signals_fd_exhaustion,
    signals_storage_exhaustion as resources_signals_storage_exhaustion,
    storage_exhaustion_error as resources_storage_exhaustion_error,
    total_space as resources_total_space,
};

pub use autostart::{
    register as autostart_register, render_registration as autostart_render_registration,
    unregister as autostart_unregister,
};

#[path = "platform_win/process_inspect.rs"]
pub(crate) mod process_inspect;
pub use process_inspect::{process_same_executable_path, ProcessLiveness};

#[path = "platform_win/process_control.rs"]
mod process_control;
pub use process_control::NativeJobserver;
pub(crate) use process_control::{
    configure_session_leader_command as process_configure_session_leader_command,
    detach_standard_streams as process_detach_standard_streams,
    force_terminate_child_process_group as process_force_terminate_child_process_group,
    native_jobserver_supported as process_native_jobserver_supported,
    redirect_standard_streams_to_log as process_redirect_standard_streams_to_log,
    set_priority_identity as process_set_priority_identity,
};

#[path = "platform_win/process_usage.rs"]
mod process_usage;
pub(crate) use process_usage::{
    cpu_ticks_for_pid as process_cpu_ticks_for_pid,
    peak_rss_bytes_for_pid as process_peak_rss_bytes_for_pid,
    tree_rss_bytes_for_pid as process_tree_rss_bytes_for_pid,
    MAX_TREE_RSS_PROCESSES as PROCESS_MAX_TREE_RSS_PROCESSES,
    PEAK_RSS_READABLE_AFTER_EXIT as PROCESS_PEAK_RSS_READABLE_AFTER_EXIT,
};

#[path = "platform_win/raw_write.rs"]
pub(crate) mod raw_write;
pub use raw_write::write_all_to_descriptor as fs_write_all_to_descriptor;

#[path = "platform_win/shutdown_request.rs"]
pub(crate) mod shutdown_request;
pub use shutdown_request::install_shutdown_request_handler as process_install_shutdown_request_handler;

#[path = "platform_win/process_exit_watch.rs"]
pub(crate) mod process_exit_watch;
pub use process_exit_watch::ProcessExitWatch;

#[path = "platform_win/process_owner_death.rs"]
pub(crate) mod process_owner_death;
pub use process_owner_death::{
    install_owner_death_cleanup as process_install_owner_death_cleanup,
    owner_death_cleanup_target as process_owner_death_cleanup_target,
    spawner_lifetime_enforcement as process_spawner_lifetime_enforcement,
};

#[path = "platform_win/host.rs"]
pub(crate) mod host;
pub use host::login_environment_block as host_login_environment_block;
pub use host::{
    cpu_compatibility_features as host_cpu_compatibility_features,
    boot_id as host_boot_id, current_process_privilege as host_current_process_privilege,
    current_user as host_current_user,
    environment_keys_are_case_insensitive as host_environment_keys_are_case_insensitive,
    filesystem_device_id as host_filesystem_device_id, home_dir as host_home_dir,
    hostname as host_hostname, is_elevated as host_is_elevated,
    login_environment as host_login_environment, machine_id as host_machine_id,
    namespace_id as host_namespace_id, user_machine_identity as host_user_machine_identity,
    PrivilegedIdentity as HostPrivilegedIdentity,
};

#[cfg(feature = "fs")]
#[path = "platform_win/fs.rs"]
pub(crate) mod fs;
#[cfg(feature = "fs")]
pub use fs::{
    ensure_no_extended_acl as fs_ensure_no_extended_acl,
    create_dir_all_private as fs_create_dir_all_private,
    create_private_file as fs_create_private_file,
    ensure_dir_private as fs_ensure_dir_private,
    open_shared_append as fs_open_shared_append, decode_path_bytes as fs_decode_path_bytes,
    encode_path_bytes as fs_encode_path_bytes, file_identity as fs_file_identity,
    is_lock_conflict as fs_is_lock_conflict, lock_exclusive as fs_lock_exclusive,
    lock_shared as fs_lock_shared, open_lock_file as fs_open_lock_file,
    path_identity as fs_path_identity,
    read_context_regular_file_bounded as fs_read_context_regular_file_bounded,
    read_private_regular_file_bounded as fs_read_private_regular_file_bounded,
    replace_file as fs_replace_file, set_file_mtime as fs_set_file_mtime,
    sync_directory as fs_sync_directory, try_lock_exclusive as fs_try_lock_exclusive,
    try_lock_shared as fs_try_lock_shared, unlock as fs_unlock,
    user_config_dir as fs_user_config_dir, user_data_dir as fs_user_data_dir,
    user_run_data_root as fs_user_run_data_root, user_runtime_dir as fs_user_runtime_dir,
    user_state_dir as fs_user_state_dir, FileIdentity as FsFileIdentity,
};

// Cache-materialization mechanics; the neutral facade reaches them through
// the crate-root `native_fs_materialize` name.
#[cfg(feature = "fs")]
#[path = "platform_win/fs_materialize.rs"]
pub(crate) mod fs_materialize;

#[cfg(feature = "fs-watch")]
#[path = "platform_win/fs_watch.rs"]
pub(crate) mod fs_watch;
#[cfg(feature = "fs-watch")]
pub use fs_watch::Watcher as FsWatchWatcher;

#[path = "platform_win/executable.rs"]
pub(crate) mod executable;
pub use executable::{
    file_name as executable_file_name, file_name_os as executable_file_name_os,
    find_in_paths as executable_find_in_paths,
    native_library_name as executable_native_library_name,
    sibling_of_current_image as executable_sibling_of_current_image,
    stem_matches as executable_stem_matches,
    unlock_for_replacement as executable_unlock_for_replacement, EXECUTABLE_EXTENSION,
};

#[cfg(feature = "ipc")]
#[path = "platform_win/ipc.rs"]
pub(crate) mod ipc;
#[cfg(feature = "ipc")]
#[path = "platform_win/ipc_owner_only.rs"]
mod ipc_owner_only;
#[cfg(feature = "ipc")]
pub use ipc_owner_only::{
    retire_socket_endpoint as ipc_retire_socket_endpoint, LocalPipeClient as IpcLocalPipeClient,
    LocalSocketListener as IpcLocalSocketListener, LocalSocketStream as IpcLocalSocketStream,
    OwnerOnlyPipeInstance as IpcOwnerOnlyPipeInstance,
};
#[cfg(any(feature = "ipc", feature = "fs"))]
#[path = "platform_win/ipc_private_dir.rs"]
mod ipc_private_dir;
#[cfg(feature = "ipc")]
pub use ipc::{
    current_user_id as ipc_current_user_id,
    endpoint_is_filesystem_backed as ipc_endpoint_is_filesystem_backed,
    nonblocking_zero_read_is_pending as ipc_nonblocking_zero_read_is_pending,
    select_endpoint_address as ipc_select_endpoint_address, Endpoint as IpcEndpoint,
    InheritedListener as IpcInheritedListener, Listener as IpcListener,
    ListenerNonblockingMode as IpcListenerNonblockingMode, PeerIdentity as IpcPeerIdentity,
    PeerIdentitySource as IpcPeerIdentitySource, Stream as IpcStream,
};
#[cfg(feature = "ipc")]
pub const LEGACY_SCM_RIGHTS_TRANSPORT_SUPPORTED: bool = false;
#[cfg(feature = "ipc")]
pub const LEGACY_DUPLICATE_HANDLE_TRANSPORT_SUPPORTED: bool = true;
#[cfg(feature = "ipc")]
pub use ipc::legacy_duplicate_handle;
#[cfg(feature = "ipc")]
pub fn legacy_send_fd_to(
    _socket: &std::path::Path,
    _sent_fd: i32,
    _payload: &[u8],
) -> Result<(), crate::LegacyHandoffError> {
    Err(crate::LegacyHandoffError::new(
        crate::platform::ipc::HandoffTransferErrorKind::Unsupported,
        None,
    ))
}
#[cfg(feature = "ipc")]
pub fn legacy_send_fd_over(
    _socket_fd: i32,
    _sent_fd: i32,
    _payload: &[u8],
) -> Result<(), crate::LegacyHandoffError> {
    Err(crate::LegacyHandoffError::new(
        crate::platform::ipc::HandoffTransferErrorKind::Unsupported,
        None,
    ))
}
#[cfg(feature = "ipc")]
pub use ipc_private_dir::{
    ensure_owner_private_directory as ipc_ensure_owner_private_directory,
    owner_private_directory as ipc_owner_private_directory,
};
#[cfg(feature = "ipc")]
pub fn ipc_broker_endpoint_name(bare_name: &str, _path_scoped: bool) -> std::io::Result<String> {
    Ok(format!(r"\\.\pipe\{bare_name}"))
}

/// Windows named-pipe names are capped by `MAX_PATH` while the long-path
/// prefix is not in use.
#[cfg(feature = "ipc")]
const WINDOWS_MAX_PATH: usize = 260;

#[cfg(feature = "ipc")]
pub fn ipc_endpoint_name_limit() -> crate::platform::ipc::EndpointNameLimit {
    crate::platform::ipc::EndpointNameLimit {
        max_bytes: WINDOWS_MAX_PATH,
        label: "Windows MAX_PATH",
    }
}

#[cfg(feature = "ipc")]
pub fn ipc_broker_v1_endpoint_path(
    bare_name: &str,
) -> Result<String, crate::platform::ipc::EndpointNameTooLong> {
    let path = format!(r"\\.\pipe\{bare_name}");
    if path.len() > WINDOWS_MAX_PATH {
        return Err(crate::platform::ipc::EndpointNameTooLong {
            len: path.len(),
            max: WINDOWS_MAX_PATH,
            limit_label: "Windows MAX_PATH",
        });
    }
    Ok(path)
}

#[cfg(feature = "ipc")]
pub fn ipc_endpoint_scope_bytes(path: &std::path::Path) -> Vec<u8> {
    // Windows paths and named pipes are case-insensitive. Hash one
    // slash/case-normalized spelling so callers cannot split the broker
    // merely by varying path presentation.
    path.to_string_lossy()
        .replace('\\', "/")
        .to_lowercase()
        .into_bytes()
}

#[cfg(feature = "ipc")]
pub fn ipc_broker_v2_runtime_dir() -> std::path::PathBuf {
    // Named pipes have no directory, so this is chosen rather than derived.
    // `data_local_dir` is per-user and non-roaming, which is what a
    // machine-local endpoint file wants -- a roaming profile would carry a
    // port from another machine.
    dirs::data_local_dir()
        .map(|dir| dir.join("running-process").join("broker-v2"))
        .unwrap_or_else(crate::platform::ipc::per_user_runtime_fallback)
}
#[cfg(feature = "ipc-async")]
pub use ipc::{
    AsyncListener as IpcAsyncListener, AsyncReadHalf as IpcAsyncReadHalf,
    AsyncStream as IpcAsyncStream, AsyncWriteHalf as IpcAsyncWriteHalf,
    IntoAsyncListener as IpcIntoAsyncListener, IntoAsyncStream as IpcIntoAsyncStream,
};

#[cfg(feature = "session-relay")]
#[path = "platform_win_session_relay.rs"]
mod session_relay;
#[cfg(feature = "session-relay")]
pub use session_relay::relay_local_socket_session;

#[path = "platform_win/console.rs"]
mod console;
pub use console::monitor_console_windows;

#[path = "platform_win/terminal.rs"]
pub mod terminal;
pub use terminal::active_graphics_probe;

#[path = "platform_win/terminal_input.rs"]
pub mod terminal_input;

#[cfg(feature = "window-icon")]
#[path = "platform_win/window_icon.rs"]
mod window_icon;
#[cfg(feature = "window-icon")]
pub use window_icon::{icon_support as window_icon_support_impl, set_icon as set_window_icon_impl};

#[path = "platform_win_descendants.rs"]
mod descendants;
pub use descendants::{assign_child_to_windows_job, WindowsJobHandle};

pub fn exact_trace_capability() -> crate::platform::process::ExactTraceCapability {
    crate::platform::process::ExactTraceCapability {
        available: false,
        backend: "windows-debug-process",
        reason: "the exact DEBUG_PROCESS supervisor is not available in this build",
        non_invasive_backend: "job-object-iocp",
        non_invasive_grade:
            crate::platform::process::NonInvasiveObservationGrade::KernelNotification,
    }
}

pub fn current_executable_build_id() -> Option<Vec<u8>> {
    None
}

pub struct TracedChild(std::process::Child);

impl TracedChild {
    pub fn id(&self) -> u32 {
        self.0.id()
    }

    pub fn try_wait_code(&mut self) -> std::io::Result<Option<i32>> {
        self.0.try_wait().map(|status| status.map(exit_code))
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        self.0.kill()
    }

    pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.0.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.0.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.0.stderr.take()
    }

    pub fn wait_code(&mut self) -> std::io::Result<i32> {
        self.0.wait().map(exit_code)
    }
}

pub fn configure_exact_trace(_command: &mut std::process::Command) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        exact_trace_capability().reason,
    ))
}

pub fn start_exact_trace(
    _command: std::process::Command,
    _emit: Box<dyn Fn(crate::platform::process::ExactTraceEvent) + Send>,
    _complete: Box<dyn FnOnce() + Send>,
) -> std::io::Result<TracedChild> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        exact_trace_capability().reason,
    ))
}

pub fn shell_command(command: &str) -> std::process::Command {
    let mut shell = std::process::Command::new("cmd.exe");
    shell.args(["/D", "/S", "/C", command]);
    shell
}

pub fn compat_shell_command(command: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;

    let mut shell = std::process::Command::new("cmd");
    shell.raw_arg("/D /S /C \"");
    shell.raw_arg(command);
    shell.raw_arg("\"");
    shell
}

pub fn canonical_environment_pairs(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut seen = std::collections::BTreeMap::new();
    for (key, value) in pairs {
        seen.insert(key.to_ascii_uppercase(), (key, value));
    }
    seen.into_values().collect()
}

/// Windows reports descendants through its Job Object IOCP path during spawn.
pub fn start_descendant_monitor(
    _root_pid: u32,
    _stop: std::sync::Arc<crate::platform::process::DescendantMonitorStop>,
    _emit: Box<dyn Fn(crate::platform::process::DescendantEvent) + Send>,
) -> std::io::Result<()> {
    Ok(())
}

use std::ffi::OsStr;
use std::io;
use std::io::Read;
use std::os::windows::io::AsRawHandle;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::ERROR_INVALID_HANDLE;
use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};

use crate::SpawnSpec;

#[derive(Default)]
pub struct CaptureCancellation {
    handles: Mutex<CaptureHandles>,
}
#[derive(Default)]
struct CaptureHandles {
    stdout: Option<usize>,
    stderr: Option<usize>,
}
pub fn prepare_capture_reader<R>(
    reader: R,
    cancellation: &CaptureCancellation,
    stream: crate::platform::process::CaptureStream,
) -> io::Result<Box<dyn Read + Send>>
where
    R: Read + AsRawHandle + Send + 'static,
{
    let mut handles = cancellation
        .handles
        .lock()
        .expect("capture pipe handles mutex poisoned");
    match stream {
        crate::platform::process::CaptureStream::Stdout => {
            handles.stdout = Some(reader.as_raw_handle() as usize)
        }
        crate::platform::process::CaptureStream::Stderr => {
            handles.stderr = Some(reader.as_raw_handle() as usize)
        }
    }
    Ok(Box::new(reader))
}
pub fn capture_reader_done(
    cancellation: &CaptureCancellation,
    stream: crate::platform::process::CaptureStream,
) {
    let mut handles = cancellation
        .handles
        .lock()
        .expect("capture pipe handles mutex poisoned");
    match stream {
        crate::platform::process::CaptureStream::Stdout => handles.stdout = None,
        crate::platform::process::CaptureStream::Stderr => handles.stderr = None,
    }
}
pub fn cancel_capture_reader(cancellation: &CaptureCancellation) {
    use winapi::shared::ntdef::HANDLE;
    use winapi::um::ioapiset::CancelIoEx;
    let handles = cancellation
        .handles
        .lock()
        .expect("capture pipe handles mutex poisoned");
    for handle in [handles.stdout, handles.stderr].into_iter().flatten() {
        // SAFETY: the slot remains populated until its reader completion callback runs.
        unsafe {
            CancelIoEx(handle as HANDLE, std::ptr::null_mut());
        }
    }
}

#[path = "platform_win_file_handles.rs"]
mod file_handles;
pub use file_handles::read_process_file_handles;
#[path = "platform_win_cmdline.rs"]
mod cmdline;
pub use cmdline::read_process_cmdline;

#[path = "platform/process_tree.rs"]
mod process_tree;

pub(crate) fn kill_tree_identity(
    identity: crate::platform::process::ProcessIdentity,
    timeout: std::time::Duration,
) -> Result<u32, crate::platform::process::ProcessIdentityActionError> {
    process_tree::kill_tree(
        identity,
        timeout,
        capture_process_identity,
        force_kill_identity,
    )
}

pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// Rebuild an [`std::process::ExitStatus`] from the exact `DWORD` exit code a
/// session recorded.
pub(crate) fn process_exit_status_from_native(native_status: u32) -> std::process::ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(native_status)
}

pub fn set_process_name(_name: &str) {}

pub fn configure_trampoline_command(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

pub fn configure_process_command(
    command: &mut std::process::Command,
    config: crate::platform::process::ProcessCommandConfig,
) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let caller = config.creation_flags.unwrap_or(0);
    let group = if config.create_process_group {
        CREATE_NEW_PROCESS_GROUP
    } else {
        0
    };
    let caller_has_console_opinion =
        caller & (CREATE_NO_WINDOW | CREATE_NEW_CONSOLE | DETACHED_PROCESS) != 0;
    let no_window = if caller_has_console_opinion || parent_has_console() {
        0
    } else {
        CREATE_NO_WINDOW
    };
    let priority = match config.nice {
        Some(value) if value >= 15 => 0x0000_0040,
        Some(value) if value >= 1 => 0x0000_4000,
        Some(value) if value <= -15 => 0x0000_0080,
        Some(value) if value <= -1 => 0x0000_8000,
        _ => 0,
    };
    let flags = caller | group | no_window | priority;
    if flags != 0 {
        command.creation_flags(flags);
    }
    Ok(())
}

pub fn trampoline_exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// Request a Ctrl+Break event for a child-owned Windows process group.
pub fn soft_terminate_process_group(pid: u32) -> io::Result<()> {
    // SAFETY: the Windows API receives only a numeric process-group id and
    // does not retain Rust pointers or references.
    let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    if ok == 0 {
        let error = io::Error::last_os_error();
        // A closed or detached console target no longer needs a soft step.
        if error.raw_os_error() != Some(ERROR_INVALID_HANDLE as i32) {
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) fn capture_process_identity(
    pid: u32,
) -> crate::platform::process::ProcessIdentityCapture {
    use crate::platform::process::{ProcessIdentityCapture as Capture, ProcessIdentityUnavailable};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, FILETIME,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return Capture::Error(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PID zero has no process identity",
        ));
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(code) if code == ERROR_INVALID_PARAMETER as i32 => Capture::Exited,
            Some(code) if code == ERROR_ACCESS_DENIED as i32 => {
                Capture::Unavailable(ProcessIdentityUnavailable::PermissionDenied)
            }
            _ => Capture::Error(error),
        };
    }
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    let error = (ok == 0).then(io::Error::last_os_error);
    unsafe { CloseHandle(handle) };
    match error {
        None => Capture::Found(crate::platform::process::ProcessIdentity::from_native(
            pid,
            [
                (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
                0,
            ],
        )),
        Some(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            Capture::Unavailable(ProcessIdentityUnavailable::PermissionDenied)
        }
        Some(error) => Capture::Error(error),
    }
}

pub(crate) fn force_kill_identity(
    identity: crate::platform::process::ProcessIdentity,
) -> Result<
    crate::platform::process::ProcessIdentityAction,
    crate::platform::process::ProcessIdentityActionError,
> {
    use crate::platform::process::{
        ProcessIdentityAction as Action, ProcessIdentityActionError as Error,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, FILETIME,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE,
    };

    // Query and terminate through the same owned handle. Windows keeps the
    // process object (and therefore its PID generation) pinned while it is
    // open, so no PID can be recycled between revalidation and termination.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
            0,
            identity.pid(),
        )
    };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(code) if code == ERROR_INVALID_PARAMETER as i32 => Ok(Action::AlreadyExited),
            Some(code) if code == ERROR_ACCESS_DENIED as i32 => Err(Error::Unavailable(
                crate::platform::process::ProcessIdentityUnavailable::PermissionDenied,
            )),
            _ => Err(Error::Host(error)),
        };
    }
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let queried =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    let key = [
        (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
        0,
    ];
    if queried == 0 {
        let error = io::Error::last_os_error();
        unsafe { CloseHandle(handle) };
        return Err(Error::Host(error));
    }
    if !identity.has_native_key(key) {
        unsafe { CloseHandle(handle) };
        return Err(Error::StaleIdentity);
    }
    let ok = unsafe { TerminateProcess(handle, 1) };
    let error = (ok == 0).then(io::Error::last_os_error);
    unsafe { CloseHandle(handle) };
    error.map_or(Ok(Action::Performed), |error| Err(Error::Host(error)))
}

pub(crate) fn signal_terminate_identity(
    _identity: crate::platform::process::ProcessIdentity,
) -> Result<
    crate::platform::process::ProcessIdentityAction,
    crate::platform::process::ProcessIdentityActionError,
> {
    Err(
        crate::platform::process::ProcessIdentityActionError::Unavailable(
            crate::platform::process::ProcessIdentityUnavailable::Unsupported,
        ),
    )
}

/// Windows compatibility stub for the Unix post-fork descriptor hook.
///
/// # Safety
/// This shares the Unix API contract and may only be called from the spawn
/// layer's post-fork/pre-exec boundary, even though it is a no-op on Windows.
pub unsafe fn unix_mark_extra_fds_close_on_exec() {}

pub fn configure_sync_daemon_command(_command: &mut std::process::Command) -> io::Result<()> {
    Ok(())
}

pub fn configure_sync_contained_command(_command: &mut std::process::Command) -> io::Result<()> {
    Ok(())
}

pub fn parent_has_console() -> bool {
    unsafe { windows_sys::Win32::System::Console::GetConsoleCP() != 0 }
}

pub fn sync_child_native_handle(child: &std::process::Child) -> usize {
    use std::os::windows::io::AsRawHandle;
    child.as_raw_handle() as usize
}
pub fn observer_backend(
    scope: crate::platform::process::ObserverScope,
    category: crate::platform::process::ObserverCategory,
) -> crate::platform::process::ObserverBackend {
    use crate::platform::process::{
        ObserverBackend as B, ObserverCategory as C, ObserverScope as S, ObserverSupport as P,
    };
    match (scope, category) {
        (S::SystemWide, C::File) | (S::SystemWide, C::Network) | (S::SystemWide, C::Process) => B { support:P::Unavailable, backend:"etw", reason:"Phase 3: Windows ETW backend not yet implemented" },
        (S::LaunchedProcessTree, C::File) => B { support:P::Partial, backend:"nt-handle-snapshot", reason:"Windows NtQuerySystemInformation + DuplicateHandle + NtQueryObject snapshot via read_process_file_handles: NT type-File handles named in the NT object namespace (\\Device\\HarddiskVolumeN\\..), never DOS paths, and PROCESS_DUP_HANDLE on the target is required (#539 slice 4; no streaming file events)" },
        (S::LaunchedProcessTree, C::Network) => B { support:P::Unavailable, backend:"none", reason:"#539: no-admin per-child network backend deferred to a follow-up issue" },
        (S::LaunchedProcessTree, C::Process) => B { support:P::Supported, backend:"job-object-iocp", reason:"Windows Job Object IOCP descendant lifecycle (#539 slice 2)" },
    }
}

pub fn unix_set_priority(_pid: u32, _nice: i32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix priority is unavailable on Windows",
    ))
}
pub fn unix_signal_process(
    _pid: u32,
    _signal: crate::platform::process::UnixSignalKind,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix signals are unavailable on Windows",
    ))
}
pub fn unix_signal_process_group(
    _pid: i32,
    _signal: crate::platform::process::UnixSignalKind,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix signals are unavailable on Windows",
    ))
}
pub fn unix_signal_raw(_signal: crate::platform::process::UnixSignalKind) -> i32 {
    0
}

pub(crate) fn shell_spec(command: &OsStr) -> SpawnSpec {
    SpawnSpec::new("cmd.exe").arg("/C").arg(command)
}
#[path = "platform_win/sync_spawn.rs"]
mod sync_spawn;
#[allow(unused_imports)]
// strict-worker containment is consumed by the next worker-process phase.
pub(crate) use sync_spawn::{
    spawn_contained_worker, spawn_strict_contained_worker, StrictWorkerChild, StrictWorkerLimits,
    StrictWorkerSpawnError, StrictWorkerSpawnStage,
};
pub use sync_spawn::{spawn_sync, spawn_sync_daemon};

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use std::ffi::OsStr;

    #[test]
    fn shell_command_preserves_round_trippable_cmd_quoting_contract() {
        let command_text = "echo alpha beta ^& gamma";
        let mut command = super::shell_command(command_text);
        assert_eq!(command.get_program(), OsStr::new("cmd.exe"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("/D"),
                OsStr::new("/S"),
                OsStr::new("/C"),
                OsStr::new(command_text)
            ]
        );
        let output = command.output().expect("shell command should execute");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"alpha beta & gamma\r\n");
    }

    #[test]
    fn compat_shell_command_preserves_nested_quotes_for_standard_spawn() {
        let command_text = "if \"alpha beta\"==\"alpha beta\" (echo shell-ok)";
        let mut command = super::compat_shell_command(command_text);
        assert_eq!(command.get_program(), OsStr::new("cmd"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("/D /S /C \""),
                OsStr::new(command_text),
                OsStr::new("\"")
            ]
        );
        let output = command
            .output()
            .expect("compat shell command should execute");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"shell-ok\r\n");
    }
}

#[cfg(all(test, feature = "ipc"))]
mod endpoint_naming_tests {
    use super::{ipc_broker_v1_endpoint_path, ipc_endpoint_name_limit, WINDOWS_MAX_PATH};

    #[test]
    fn the_v1_address_is_a_named_pipe_carrying_the_bare_name() {
        let address = ipc_broker_v1_endpoint_path("rpb-v1-abc-shared").expect("derive address");
        assert!(address.starts_with(r"\\.\pipe\"));
        assert!(address.ends_with("rpb-v1-abc-shared"));
    }

    #[test]
    fn an_over_long_name_is_refused_against_max_path() {
        let err = ipc_broker_v1_endpoint_path(&"a".repeat(WINDOWS_MAX_PATH))
            .expect_err("must exceed MAX_PATH");
        assert_eq!(err.max, WINDOWS_MAX_PATH);
        assert_eq!(err.limit_label, "Windows MAX_PATH");
        assert!(err.len > WINDOWS_MAX_PATH);
    }

    #[test]
    fn the_reported_budget_is_max_path() {
        let limit = ipc_endpoint_name_limit();
        assert_eq!(limit.max_bytes, WINDOWS_MAX_PATH);
        assert_eq!(limit.label, "Windows MAX_PATH");
    }

    #[test]
    fn the_scope_spelling_folds_case_and_separators() {
        // Named pipes and paths are case-insensitive here, so two callers
        // spelling the same install differently must hash identically. This
        // pins the spelling itself: changing it re-scopes every deployed
        // broker, and the stability tests upstream would not notice.
        use super::ipc_endpoint_scope_bytes;

        let mixed =
            ipc_endpoint_scope_bytes(std::path::Path::new(r"C:\Program Files\App\Broker.exe"));
        assert_eq!(mixed, b"c:/program files/app/broker.exe".to_vec());

        let other =
            ipc_endpoint_scope_bytes(std::path::Path::new("c:/PROGRAM FILES/app/BROKER.exe"));
        assert_eq!(mixed, other);
    }
}

/// Replace this process's image with `command`.
///
/// Windows has no `execve`, so this always fails. It exists so the facade has
/// one shape on every host; callers check
/// [`can_replace_current_image`](crate::platform::process::can_replace_current_image)
/// first and start a successor instead.
pub fn process_replace_current_image(_command: &mut std::process::Command) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this host cannot replace a running process image",
    )
}

/// This host has no `execve`; a caller must start a successor and exit.
pub const fn process_can_replace_current_image() -> bool {
    false
}
#[cfg(all(feature = "tauri-webview", feature = "wasm-sketch-worker"))]
pub(crate) fn configure_native_worker_environment(command: &mut std::process::Command) {
    for key in ["SystemRoot", "WINDIR", "TEMP", "TMP", "LOCALAPPDATA"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

#[path = "platform_win/interrupt.rs"]
pub(crate) mod interrupt;
#[path = "platform_win/termination.rs"]
pub(crate) mod termination;

#[cfg(feature = "fs-scan")]
#[path = "platform_win/fs_scan/mod.rs"]
pub(crate) mod fs_scan;
