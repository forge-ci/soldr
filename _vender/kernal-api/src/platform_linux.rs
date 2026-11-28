//! Linux implementation root for the process capability.

#[cfg(feature = "global-hotkey")]
#[path = "platform_linux/global_hotkey.rs"]
pub(crate) mod global_hotkey;
#[cfg(feature = "app-catalog")]
#[path = "platform_linux/app_catalog.rs"]
pub(crate) mod app_catalog;
#[cfg(feature = "desktop-input")]
#[path = "platform_linux/desktop_input.rs"]
pub(crate) mod desktop_input;

#[cfg(feature = "desktop-input-daemon")]
#[path = "platform_linux/input_daemon.rs"]
pub(crate) mod input_daemon;

#[path = "platform_linux/dylib.rs"]
pub(crate) mod dylib;
#[cfg(feature = "control-signal")]
#[path = "platform_linux/control_signal.rs"]
pub(crate) mod control_signal;

#[cfg(feature = "system-tray")]
#[path = "platform_linux/system_tray.rs"]
pub(crate) mod system_tray;
#[cfg(feature = "gfx-egui")]
#[path = "platform_linux/gfx_dock.rs"]
pub(crate) mod gfx_dock;
#[cfg(feature = "gfx-egui")]
#[path = "platform_linux/gfx_keymap.rs"]
pub(crate) mod gfx_keymap;


#[cfg(feature = "wasm-sketch-worker")]
#[path = "platform/scratch_directory.rs"]
pub(crate) mod scratch_directory;

#[cfg(feature = "tauri-webview")]
#[path = "platform_linux/viewport_capture.rs"]
pub(crate) mod viewport_capture;

#[cfg(feature = "tauri-webview")]
#[path = "platform_linux/webview_window.rs"]
pub(crate) mod webview_window;

#[path = "platform_linux/autostart.rs"]
pub(crate) mod autostart;

#[path = "platform_linux/resources.rs"]
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
    register as autostart_register,
    render_registration as autostart_render_registration,
    unregister as autostart_unregister,
};

#[path = "platform_linux/process_inspect.rs"]
pub(crate) mod process_inspect;
pub use process_inspect::{process_same_executable_path, ProcessLiveness};

#[path = "platform_linux/process_control.rs"]
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

#[path = "platform_linux/process_usage.rs"]
mod process_usage;
pub(crate) use process_usage::{
    cpu_ticks_for_pid as process_cpu_ticks_for_pid,
    peak_rss_bytes_for_pid as process_peak_rss_bytes_for_pid,
    tree_rss_bytes_for_pid as process_tree_rss_bytes_for_pid,
    MAX_TREE_RSS_PROCESSES as PROCESS_MAX_TREE_RSS_PROCESSES,
    PEAK_RSS_READABLE_AFTER_EXIT as PROCESS_PEAK_RSS_READABLE_AFTER_EXIT,
};

#[path = "platform_linux/raw_write.rs"]
pub(crate) mod raw_write;
pub use raw_write::write_all_to_descriptor as fs_write_all_to_descriptor;

#[path = "platform_linux/shutdown_request.rs"]
pub(crate) mod shutdown_request;
pub use shutdown_request::install_shutdown_request_handler as process_install_shutdown_request_handler;

#[path = "platform_linux/process_exit_watch.rs"]
pub(crate) mod process_exit_watch;
pub use process_exit_watch::ProcessExitWatch;

#[path = "platform_linux/process_owner_death.rs"]
pub(crate) mod process_owner_death;
pub use process_owner_death::{
    install_owner_death_cleanup as process_install_owner_death_cleanup,
    owner_death_cleanup_target as process_owner_death_cleanup_target,
    spawner_lifetime_enforcement as process_spawner_lifetime_enforcement,
};

#[path = "platform_linux/host.rs"]
pub(crate) mod host;
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
pub use host::login_environment_block as host_login_environment_block;

#[cfg(feature = "fs")]
#[path = "platform_linux/fs.rs"]
pub(crate) mod fs;
#[cfg(feature = "fs")]
pub use fs::{
    ensure_no_extended_acl as fs_ensure_no_extended_acl,
    create_dir_all_private as fs_create_dir_all_private,
    create_private_file as fs_create_private_file,
    ensure_dir_private as fs_ensure_dir_private,
    open_shared_append as fs_open_shared_append,
    decode_path_bytes as fs_decode_path_bytes,
    replace_file as fs_replace_file, sync_directory as fs_sync_directory,
    user_config_dir as fs_user_config_dir,
    user_data_dir as fs_user_data_dir, encode_path_bytes as fs_encode_path_bytes,
    file_identity as fs_file_identity, is_lock_conflict as fs_is_lock_conflict,
    lock_exclusive as fs_lock_exclusive, lock_shared as fs_lock_shared,
    open_lock_file as fs_open_lock_file, path_identity as fs_path_identity,
    set_file_mtime as fs_set_file_mtime,
    try_lock_exclusive as fs_try_lock_exclusive, try_lock_shared as fs_try_lock_shared,
    unlock as fs_unlock,
    user_run_data_root as fs_user_run_data_root, user_runtime_dir as fs_user_runtime_dir,
    user_state_dir as fs_user_state_dir, FileIdentity as FsFileIdentity,
    read_context_regular_file_bounded as fs_read_context_regular_file_bounded,
    read_private_regular_file_bounded as fs_read_private_regular_file_bounded,
};

// Cache-materialization mechanics; the neutral facade reaches them through
// the crate-root `native_fs_materialize` name.
#[cfg(feature = "fs")]
#[path = "platform_linux/fs_materialize.rs"]
pub(crate) mod fs_materialize;

#[cfg(feature = "fs-watch")]
#[path = "platform_linux/fs_watch.rs"]
pub(crate) mod fs_watch;
#[cfg(feature = "fs-watch")]
pub use fs_watch::Watcher as FsWatchWatcher;

#[path = "platform_linux/executable.rs"]
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
#[path = "platform_linux/ipc.rs"]
pub(crate) mod ipc;
#[cfg(feature = "ipc")]
#[path = "platform_linux/ipc_owner_only.rs"]
mod ipc_owner_only;
#[cfg(feature = "ipc")]
pub use ipc_owner_only::{
    retire_socket_endpoint as ipc_retire_socket_endpoint, LocalPipeClient as IpcLocalPipeClient,
    LocalSocketListener as IpcLocalSocketListener, LocalSocketStream as IpcLocalSocketStream,
    OwnerOnlyPipeInstance as IpcOwnerOnlyPipeInstance,
};
#[cfg(feature = "ipc")]
#[path = "platform_linux/ipc_private_dir.rs"]
mod ipc_private_dir;
#[cfg(feature = "ipc")]
pub use ipc::{
    current_user_id as ipc_current_user_id, Endpoint as IpcEndpoint,
    endpoint_is_filesystem_backed as ipc_endpoint_is_filesystem_backed,
    nonblocking_zero_read_is_pending as ipc_nonblocking_zero_read_is_pending,
    select_endpoint_address as ipc_select_endpoint_address,
    InheritedListener as IpcInheritedListener, Listener as IpcListener,
    ListenerNonblockingMode as IpcListenerNonblockingMode, PeerIdentity as IpcPeerIdentity,
    PeerIdentitySource as IpcPeerIdentitySource, Stream as IpcStream,
};
#[cfg(feature = "ipc")]
pub const LEGACY_SCM_RIGHTS_TRANSPORT_SUPPORTED: bool = true;
#[cfg(feature = "ipc")]
pub const LEGACY_DUPLICATE_HANDLE_TRANSPORT_SUPPORTED: bool = false;
#[cfg(feature = "ipc")]
pub use ipc::{legacy_send_fd_over, legacy_send_fd_to};
#[cfg(feature = "ipc")]
pub fn legacy_duplicate_handle(
    _source_handle: usize,
    _backend_pid: u32,
) -> Result<usize, crate::LegacyHandoffError> {
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
pub fn ipc_broker_endpoint_name(bare_name: &str, path_scoped: bool) -> std::io::Result<String> {
    use std::fmt::Write as _;
    use std::path::PathBuf;

    if path_scoped {
        let mut hash = blake3::Hasher::new();
        hash.update(b"running-process:path-scoped-socket:v1\0");
        hash.update(bare_name.as_bytes());
        let mut leaf = String::with_capacity(32);
        for byte in hash.finalize().as_bytes().iter().take(16) { let _ = write!(leaf, "{byte:02x}"); }
        return Ok(PathBuf::from("/tmp").join(format!(".rp-path-{leaf}.sock")).to_string_lossy().into_owned());
    }
    let directory = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(value) => PathBuf::from(value).join("running-process").join("broker-v2"),
        None => PathBuf::from(format!("/tmp/running-process-{}/broker-v2", unsafe { libc::getuid() })),
    };
    Ok(directory.join(format!("{bare_name}.sock")).to_string_lossy().into_owned())
}

/// Linux `sun_path` is 108 bytes including the NUL terminator.
#[cfg(feature = "ipc")]
const LINUX_SUN_PATH_MAX: usize = 108;

#[cfg(feature = "ipc")]
pub fn ipc_endpoint_name_limit() -> crate::platform::ipc::EndpointNameLimit {
    crate::platform::ipc::EndpointNameLimit {
        max_bytes: LINUX_SUN_PATH_MAX,
        label: "Linux sun_path",
    }
}

/// Directory holding v1 broker sockets.
///
/// Deliberately performs no filesystem writes: name derivation stays pure so
/// the hash and length-limit tests remain deterministic. Callers that bind
/// create the parent directory themselves.
#[cfg(feature = "ipc")]
fn broker_v1_socket_dir() -> std::path::PathBuf {
    use std::path::PathBuf;

    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("running-process").join("broker"),
        None => PathBuf::from(format!(
            "/tmp/running-process-{}/broker",
            unsafe { libc::getuid() }
        )),
    }
}

#[cfg(feature = "ipc")]
pub fn ipc_broker_v1_endpoint_path(
    bare_name: &str,
) -> Result<String, crate::platform::ipc::EndpointNameTooLong> {
    // Linux gets 108 bytes and a guaranteed $XDG_RUNTIME_DIR (or a short
    // /tmp fallback), so the full canonical name survives for debuggability.
    let candidate = broker_v1_socket_dir().join(format!("{bare_name}.sock"));
    let candidate = candidate.to_string_lossy();
    // sockaddr_un is NUL-terminated, so the path itself must be strictly
    // shorter than the field width.
    if candidate.len() >= LINUX_SUN_PATH_MAX {
        return Err(crate::platform::ipc::EndpointNameTooLong {
            len: candidate.len(),
            max: LINUX_SUN_PATH_MAX - 1,
            limit_label: "Linux sun_path",
        });
    }
    Ok(candidate.into_owned())
}

#[cfg(feature = "ipc")]
pub fn ipc_endpoint_scope_bytes(path: &std::path::Path) -> Vec<u8> {
    // Linux paths are opaque byte strings; no spelling difference is
    // meaningless, so the bytes are hashed exactly as the OS reports them.
    use std::os::unix::ffi::OsStrExt as _;

    path.as_os_str().as_bytes().to_vec()
}

#[cfg(feature = "ipc")]
pub fn ipc_broker_v2_runtime_dir() -> std::path::PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => std::path::PathBuf::from(dir)
            .join("running-process")
            .join("broker-v2"),
        None => crate::platform::ipc::per_user_runtime_fallback(),
    }
}
#[cfg(feature = "ipc-async")]
pub use ipc::{
    AsyncListener as IpcAsyncListener, AsyncReadHalf as IpcAsyncReadHalf,
    AsyncStream as IpcAsyncStream, AsyncWriteHalf as IpcAsyncWriteHalf,
    IntoAsyncListener as IpcIntoAsyncListener, IntoAsyncStream as IpcIntoAsyncStream,
};

#[cfg(feature = "session-relay")]
#[path = "platform_linux_session_relay.rs"]
mod session_relay;
#[cfg(feature = "session-relay")]
pub use session_relay::relay_local_socket_session;

#[path = "platform_linux/terminal.rs"]
pub mod terminal;
pub use terminal::active_graphics_probe;
pub use crate::platform::terminal_input;

#[cfg(feature = "window-icon")]
#[path = "platform_linux/window_icon.rs"]
mod window_icon;
#[cfg(feature = "window-icon")]
pub use window_icon::{icon_support as window_icon_support_impl, set_icon as set_window_icon_impl};

pub fn shell_command(command: &str) -> std::process::Command {
    let mut shell = std::process::Command::new("/bin/sh");
    shell.arg("-lc").arg(command);
    shell
}

pub fn compat_shell_command(command: &str) -> std::process::Command {
    let mut shell = std::process::Command::new("/bin/sh");
    shell.arg("-lc").arg(command);
    shell
}

pub fn canonical_environment_pairs(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    pairs
}

pub fn monitor_console_windows(
    _duration: std::time::Duration,
) -> Vec<crate::platform::process::ConsoleWindowInfo> {
    Vec::new()
}

use std::ffi::OsStr;
use std::io;
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;

use crate::SpawnSpec;

#[path = "platform_linux_descendants.rs"]
mod descendants;
pub use descendants::start_descendant_monitor;

#[path = "platform_linux_trace.rs"]
mod exact_trace;
pub use exact_trace::{configure_exact_trace, start_exact_trace, TracedChild};

pub fn exact_trace_capability() -> crate::platform::process::ExactTraceCapability {
    crate::platform::process::ExactTraceCapability {
        available: true,
        backend: "linux-ptrace",
        reason: "launch-time PTRACE_TRACEME with follow-fork/clone/exec/exit supervision",
        non_invasive_backend: "proc-descendant-snapshot",
        non_invasive_grade:
            crate::platform::process::NonInvasiveObservationGrade::SnapshotInferred,
    }
}

pub struct WindowsJobHandle;

pub fn assign_child_to_windows_job(
    _child: &std::process::Child,
    _direct_pid: u32,
    _address_space_limit_bytes: Option<u64>,
    _emit: Option<Box<dyn Fn(crate::platform::process::DescendantEvent) + Send>>,
) -> io::Result<WindowsJobHandle> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows Job Objects are unavailable on Linux",
    ))
}

#[derive(Default)]
pub struct CaptureCancellation {
    wakers: Mutex<CaptureWakers>,
}

#[derive(Default)]
struct CaptureWakers {
    stdout: Option<UnixStream>,
    stderr: Option<UnixStream>,
}

struct CancelableCaptureReader<R> {
    reader: R,
    wake_reader: UnixStream,
}

impl<R: Read + AsRawFd> Read for CancelableCaptureReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() { return Ok(0); }
        loop {
            let mut poll_fds = [
                libc::pollfd { fd: self.reader.as_raw_fd(), events: libc::POLLIN | libc::POLLHUP | libc::POLLERR, revents: 0 },
                libc::pollfd { fd: self.wake_reader.as_raw_fd(), events: libc::POLLIN | libc::POLLHUP | libc::POLLERR, revents: 0 },
            ];
            // SAFETY: both descriptors remain owned by this reader for the call.
            let polled = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, -1) };
            if polled < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted { continue; }
                return Err(error);
            }
            if poll_fds[1].revents != 0 {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "capture reader cancelled"));
            }
            if poll_fds[0].revents != 0 {
                match self.reader.read(buf) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    result => return result,
                }
            }
        }
    }
}

pub fn prepare_capture_reader<R>(
    reader: R,
    cancellation: &CaptureCancellation,
    stream: crate::platform::process::CaptureStream,
) -> io::Result<Box<dyn Read + Send>>
where R: Read + AsRawFd + Send + 'static {
    set_nonblocking(reader.as_raw_fd())?;
    let (wake_reader, wake_writer) = UnixStream::pair()?;
    wake_writer.set_nonblocking(true)?;
    let mut wakers = cancellation.wakers.lock().expect("capture wakers mutex poisoned");
    match stream {
        crate::platform::process::CaptureStream::Stdout => wakers.stdout = Some(wake_writer),
        crate::platform::process::CaptureStream::Stderr => wakers.stderr = Some(wake_writer),
    }
    Ok(Box::new(CancelableCaptureReader { reader, wake_reader }))
}

pub fn capture_reader_done(cancellation: &CaptureCancellation, stream: crate::platform::process::CaptureStream) {
    let mut wakers = cancellation.wakers.lock().expect("capture wakers mutex poisoned");
    match stream {
        crate::platform::process::CaptureStream::Stdout => wakers.stdout = None,
        crate::platform::process::CaptureStream::Stderr => wakers.stderr = None,
    }
}

pub fn cancel_capture_reader(cancellation: &CaptureCancellation) {
    let wakers = cancellation.wakers.lock().expect("capture wakers mutex poisoned");
    let byte = [1_u8; 1];
    for writer in [&wakers.stdout, &wakers.stderr].into_iter().flatten() {
        // SAFETY: the stored wake socket stays alive while the mutex is held.
        let _ = unsafe { libc::write(writer.as_raw_fd(), byte.as_ptr().cast(), byte.len()) };
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is borrowed from a live reader for both calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: `fd` is borrowed from a live reader for both calls.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[path = "platform_linux_file_handles.rs"]
mod file_handles;
pub use file_handles::read_process_file_handles;
#[path = "platform_linux_cmdline.rs"]
mod cmdline;
pub use cmdline::read_process_cmdline;

#[path = "platform/process_tree.rs"]
mod process_tree;

pub(crate) fn kill_tree_identity(
    identity: crate::platform::process::ProcessIdentity,
    timeout: std::time::Duration,
) -> Result<u32, crate::platform::process::ProcessIdentityActionError> {
    process_tree::kill_tree(identity, timeout, capture_process_identity, force_kill_identity)
}

pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| -status.signal().unwrap_or(1))
}

/// Rebuild an [`std::process::ExitStatus`] from the native wait-status word a
/// session recorded; the inverse of `ExitStatusExt::into_raw`.
pub(crate) fn process_exit_status_from_native(native_status: u32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(native_status as i32)
}

pub fn set_process_name(name: &str) {
    let truncated: String = name.chars().take(15).collect();
    let c_name = std::ffi::CString::new(truncated).unwrap_or_default();
    unsafe { libc::prctl(libc::PR_SET_NAME, c_name.as_ptr() as libc::c_ulong, 0, 0, 0); }
}

pub fn configure_trampoline_command(_command: &mut std::process::Command) {}

pub fn configure_process_command(
    command: &mut std::process::Command,
    config: crate::platform::process::ProcessCommandConfig,
) -> io::Result<()> {
    let create_process_group = config.create_process_group;
    let nice = config.nice;
    let address_space_limit_bytes = config.address_space_limit_bytes;
    if !(create_process_group || nice.is_some() || address_space_limit_bytes.is_some()) {
        return Ok(());
    }
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(move || {
            if create_process_group && libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if let Some(nice) = nice {
                if libc::setpriority(libc::PRIO_PROCESS, 0, nice) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            if let Some(limit) = address_space_limit_bytes {
                let rlim = libc::rlimit { rlim_cur: limit, rlim_max: limit };
                if libc::setrlimit(libc::RLIMIT_AS, &rlim) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

pub fn trampoline_exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map_or_else(|| status.code().unwrap_or(1), |signal| 128 + signal)
}

/// Return the GNU build ID of the running executable without reading the
/// executable from disk.
///
/// The dynamic loader has already mapped the main image's `PT_NOTE` segment,
/// so callers that only need an image-generation identity do not need to hash
/// a potentially large unoptimized binary. `None` preserves a clean fallback
/// for binaries linked without a GNU build ID.
pub fn current_executable_build_id() -> Option<Vec<u8>> {
    unsafe extern "C" fn visit(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        output: *mut libc::c_void,
    ) -> libc::c_int {
        const MAX_NOTE_BYTES: usize = 1024 * 1024;

        let info = unsafe { &*info };
        let is_main_executable = info.dlpi_name.is_null()
            || unsafe { std::ffi::CStr::from_ptr(info.dlpi_name) }
                .to_bytes()
                .is_empty();
        if !is_main_executable || info.dlpi_phdr.is_null() || info.dlpi_phnum == 0 {
            return 0;
        }
        let headers = unsafe {
            std::slice::from_raw_parts(info.dlpi_phdr, usize::from(info.dlpi_phnum))
        };
        #[allow(clippy::unnecessary_cast)]
        let load_bias = info.dlpi_addr as u64;
        for header in headers {
            if header.p_type != libc::PT_NOTE {
                continue;
            }
            let Ok(length) = usize::try_from(header.p_memsz) else {
                continue;
            };
            if length == 0 || length > MAX_NOTE_BYTES {
                continue;
            }
            let Some(address) = load_bias.checked_add(header.p_vaddr) else {
                continue;
            };
            let Some(note_end) = address.checked_add(length as u64) else {
                continue;
            };
            let mapped_read_only = headers.iter().any(|load| {
                if load.p_type != libc::PT_LOAD || load.p_flags & libc::PF_R == 0 {
                    return false;
                }
                let Some(start) = load_bias.checked_add(load.p_vaddr) else {
                    return false;
                };
                let Some(end) = start.checked_add(load.p_memsz) else {
                    return false;
                };
                address >= start && note_end <= end
            });
            if address == 0 || !mapped_read_only {
                continue;
            }
            let notes = unsafe { std::slice::from_raw_parts(address as *const u8, length) };
            if let Some(build_id) = gnu_build_id_from_notes(notes) {
                let output = unsafe { &mut *output.cast::<Option<Vec<u8>>>() };
                *output = Some(build_id.to_vec());
                return 1;
            }
        }
        0
    }

    let mut output = None;
    unsafe {
        libc::dl_iterate_phdr(
            Some(visit),
            (&mut output as *mut Option<Vec<u8>>).cast::<libc::c_void>(),
        );
    }
    output
}

fn gnu_build_id_from_notes(mut notes: &[u8]) -> Option<&[u8]> {
    fn aligned(value: usize) -> Option<usize> {
        value.checked_add(3).map(|value| value & !3)
    }

    while notes.len() >= 12 {
        let name_len = usize::try_from(u32::from_ne_bytes(notes[0..4].try_into().ok()?)).ok()?;
        let desc_len = usize::try_from(u32::from_ne_bytes(notes[4..8].try_into().ok()?)).ok()?;
        let kind = u32::from_ne_bytes(notes[8..12].try_into().ok()?);
        let name_end = 12usize.checked_add(name_len)?;
        let desc_start = 12usize.checked_add(aligned(name_len)?)?;
        let desc_end = desc_start.checked_add(desc_len)?;
        let next = desc_start.checked_add(aligned(desc_len)?)?;
        if next > notes.len() || name_end > notes.len() || desc_end > notes.len() {
            return None;
        }
        if kind == 3 && notes.get(12..name_end)?.starts_with(b"GNU") && desc_len > 0 {
            return notes.get(desc_start..desc_end);
        }
        notes = &notes[next..];
    }
    None
}

/// Request a graceful shutdown for a child-owned POSIX process group.
pub fn soft_terminate_process_group(pid: u32) -> io::Result<()> {
    // SAFETY: `kill` receives only the numeric child-owned group id; no Rust
    // references or borrowed state cross the OS boundary.
    let result = unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn process_snapshot() -> Vec<crate::platform::process::ProcessSnapshot> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
        .filter_map(process_snapshot_for_pid)
        .collect()
}

#[cfg(test)]
pub(crate) fn process_snapshot_for_pid(pid: u32) -> Option<crate::platform::process::ProcessSnapshot> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (parent_pid, start_ticks) = parse_proc_stat(&stat)?;
    Some(crate::platform::process::ProcessSnapshot {
        identity: crate::platform::process::ProcessIdentity::from_native(pid, [start_ticks, 0]),
        parent_pid,
    })
}

/// Capture the Linux boot-relative `/proc/<pid>/stat` start-tick key. It is
/// deliberately never converted into a wall-clock time or fabricated on a
/// partial read.
pub(crate) fn capture_process_identity(pid: u32) -> crate::platform::process::ProcessIdentityCapture {
    use crate::platform::process::{ProcessIdentityCapture as Capture, ProcessIdentityUnavailable};

    if pid == 0 {
        return Capture::Error(io::Error::new(io::ErrorKind::InvalidInput, "PID zero has no process identity"));
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => match parse_proc_stat(&stat) {
            Some((_parent, start_ticks)) => Capture::Found(
                crate::platform::process::ProcessIdentity::from_native(pid, [start_ticks, 0]),
            ),
            None => Capture::Error(io::Error::new(io::ErrorKind::InvalidData, "malformed /proc process stat")),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Capture::Exited,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            Capture::Unavailable(ProcessIdentityUnavailable::PermissionDenied)
        }
        Err(error) => Capture::Error(error),
    }
}

pub(crate) fn force_kill_identity(
    identity: crate::platform::process::ProcessIdentity,
) -> Result<crate::platform::process::ProcessIdentityAction, crate::platform::process::ProcessIdentityActionError> {
    signal_identity(identity, libc::SIGKILL)
}

pub(crate) fn signal_terminate_identity(
    identity: crate::platform::process::ProcessIdentity,
) -> Result<crate::platform::process::ProcessIdentityAction, crate::platform::process::ProcessIdentityActionError> {
    signal_identity(identity, libc::SIGTERM)
}

fn signal_identity(
    identity: crate::platform::process::ProcessIdentity,
    signal: libc::c_int,
) -> Result<crate::platform::process::ProcessIdentityAction, crate::platform::process::ProcessIdentityActionError> {
    use crate::platform::process::{
        ProcessIdentityAction as Action, ProcessIdentityActionError as Error,
        ProcessIdentityCapture as Capture, ProcessIdentityUnavailable,
    };
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    // pidfd pins one process instance. A second generation check after the
    // fd opens, followed by pidfd_send_signal, cannot hit a recycled PID.
    let raw_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid() as libc::pid_t, 0_u32) };
    if raw_fd < 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(Action::AlreadyExited),
            Some(libc::EPERM) | Some(libc::EACCES) => Err(Error::Unavailable(ProcessIdentityUnavailable::PermissionDenied)),
            Some(libc::ENOSYS) | Some(libc::EINVAL) => Err(Error::Unavailable(ProcessIdentityUnavailable::Unsupported)),
            _ => Err(Error::Host(error)),
        };
    }
    let pid_fd = unsafe { OwnedFd::from_raw_fd(raw_fd as i32) };
    match capture_process_identity(identity.pid()) {
        Capture::Found(current) if current == identity => {}
        Capture::Found(_) => return Err(Error::StaleIdentity),
        Capture::Exited => return Ok(Action::AlreadyExited),
        Capture::Unavailable(reason) => return Err(Error::Unavailable(reason)),
        Capture::Error(error) => return Err(Error::Host(error)),
    }
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pid_fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0_u32,
        )
    };
    if result == 0 {
        Ok(Action::Performed)
    } else if io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(Action::AlreadyExited)
    } else {
        Err(Error::Host(io::Error::last_os_error()))
    }
}

fn parse_proc_stat(stat: &str) -> Option<(u32, u64)> {
    // `comm` is parenthesized and may contain spaces or ')' characters; only
    // the final ')' starts the fixed-position fields after it.
    let suffix = stat.get(stat.rfind(')')? + 1..)?;
    let mut fields = suffix.split_ascii_whitespace();
    let _state = fields.next()?; // field 3
    let parent_pid = fields.next()?.parse().ok()?; // field 4
    let start_ticks = fields.nth(17)?.parse().ok()?; // field 22
    Some((parent_pid, start_ticks))
}

/// Mark inherited descriptors close-on-exec without breaking std's exec-error pipe.
///
/// # Safety
/// This must only be called from a post-fork `pre_exec` closure.
pub unsafe fn unix_mark_extra_fds_close_on_exec() {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "x86", target_arch = "arm", target_arch = "riscv64", target_arch = "powerpc64"))]
    {
        const SYS_CLOSE_RANGE: libc::c_long = 436;
        const CLOSE_RANGE_CLOEXEC: libc::c_uint = 4;
        if libc::syscall(SYS_CLOSE_RANGE, 3u32, libc::c_uint::MAX, CLOSE_RANGE_CLOEXEC) == 0 {
            return;
        }
    }
    mark_fds_from_directory_or_range();
}

pub fn configure_sync_daemon_command(command: &mut std::process::Command) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            let _ = libc::setsid();
            unix_mark_extra_fds_close_on_exec();
            Ok(())
        });
    }
    Ok(())
}

pub fn configure_sync_contained_command(command: &mut std::process::Command) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 { return Err(io::Error::last_os_error()); }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() == 1 { libc::_exit(1); }
            unix_mark_extra_fds_close_on_exec();
            Ok(())
        });
    }
    Ok(())
}

pub fn parent_has_console() -> bool { false }

pub fn sync_child_native_handle(_child: &std::process::Child) -> usize { 0 }

unsafe fn mark_fds_from_directory_or_range() {
    let dir = libc::opendir(c"/dev/fd".as_ptr());
    if !dir.is_null() {
        let dir_fd = libc::dirfd(dir);
        loop {
            let entry = libc::readdir(dir);
            if entry.is_null() { break; }
            let mut fd: libc::c_int = 0;
            let mut cursor = (*entry).d_name.as_ptr();
            let mut numeric = false;
            while *cursor != 0 {
                let byte = *cursor as u8;
                if !byte.is_ascii_digit() { numeric = false; break; }
                fd = fd * 10 + (byte - b'0') as libc::c_int;
                cursor = cursor.add(1);
                numeric = true;
            }
            if numeric && fd > 2 && fd != dir_fd { set_cloexec(fd); }
        }
        libc::closedir(dir);
        return;
    }
    let maximum = libc::sysconf(libc::_SC_OPEN_MAX);
    for fd in 3..if maximum < 0 { 4096 } else { maximum as libc::c_int } { set_cloexec(fd); }
}

unsafe fn set_cloexec(fd: libc::c_int) {
    let flags = libc::fcntl(fd, libc::F_GETFD);
    if flags != -1 { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC); }
}
pub fn observer_backend(scope: crate::platform::process::ObserverScope, category: crate::platform::process::ObserverCategory) -> crate::platform::process::ObserverBackend {
    use crate::platform::process::{ObserverBackend as B, ObserverCategory as C, ObserverScope as S, ObserverSupport as P};
    match (scope, category) {
        (S::SystemWide, C::File) => B { support:P::Unavailable, backend:"seccomp-user-notify", reason:"Phase 3: Linux seccomp user-notify file backend not yet implemented" },
        (S::SystemWide, C::Network) => B { support:P::Unavailable, backend:"ebpf", reason:"Phase 3: Linux eBPF network backend not yet implemented" },
        (S::SystemWide, C::Process) => B { support:P::Unavailable, backend:"seccomp-user-notify", reason:"Phase 3: Linux seccomp user-notify process backend not yet implemented" },
        (S::LaunchedProcessTree, C::File) => B { support:P::Partial, backend:"proc-fd-snapshot", reason:"Linux /proc/<pid>/fd/* snapshot via read_process_file_handles: every descriptor, readlink target verbatim, so socket:[..]/pipe:[..]/anon_inode:.. and '(deleted)' paths are reported alongside real ones (#539 slice 6 follow-up; no streaming file events)" },
        (S::LaunchedProcessTree, C::Network) => B { support:P::Unavailable, backend:"none", reason:"#539: no-admin per-child network backend deferred to a follow-up issue" },
        (S::LaunchedProcessTree, C::Process) => B { support:P::Supported, backend:"subreaper-proc-poll", reason:"Linux PR_SET_CHILD_SUBREAPER + /proc descendant polling (#539 slice 5)" },
    }
}

/// Reprioritise one process, with the same range check as signalling.
///
/// `setpriority` takes the `id` unsigned, so this one cannot become a
/// broadcast -- but a PID that could not name a process should not reach the
/// kernel at all, and one validation rule is easier to keep true than two.
pub fn unix_set_priority(pid: u32, nice: i32) -> io::Result<()> {
    let pid = crate::platform::process::ProcessId::new(pid)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, pid.get(), nice) } == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
/// Signal one process, never a process group.
///
/// The range check is not decoration. `pid_t` is signed, so an unchecked
/// `u32` above `i32::MAX` reaches `kill(2)` negative, where `-N` is process
/// group `N` and `-1` is every process this caller may signal -- a PID read
/// from a file or a state row could therefore end a user's whole session.
/// [`ProcessId`] is what makes that unrepresentable; this entry point keeps
/// its `u32` spelling for compatibility and validates before the cast.
///
/// [`ProcessId`]: crate::platform::process::ProcessId
pub fn unix_signal_process(pid: u32, signal: crate::platform::process::UnixSignalKind) -> io::Result<()> {
    let pid = crate::platform::process::ProcessId::new(pid)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if unsafe { libc::kill(pid.native_signed(), unix_signal_raw(signal)) } == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
pub fn unix_signal_process_group(pid: i32, signal: crate::platform::process::UnixSignalKind) -> io::Result<()> {
    if unsafe { libc::killpg(pid, unix_signal_raw(signal)) } == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
pub fn unix_signal_raw(signal: crate::platform::process::UnixSignalKind) -> i32 {
    match signal { crate::platform::process::UnixSignalKind::Interrupt => libc::SIGINT, crate::platform::process::UnixSignalKind::Terminate => libc::SIGTERM, crate::platform::process::UnixSignalKind::Kill => libc::SIGKILL }
}

pub(crate) fn shell_spec(command: &OsStr) -> SpawnSpec {
    SpawnSpec::new("/bin/sh").arg("-c").arg(command)
}

#[cfg(test)]
mod tests {
    #[test]
    fn shell_command_preserves_login_shell_contract_and_ignores_child_path() {
        use std::ffi::OsStr;

        let command_text = "printf '%s' 'alpha beta;\"gamma\"'";
        let mut command = super::shell_command(command_text);
        assert_eq!(command.get_program(), OsStr::new("/bin/sh"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [OsStr::new("-lc"), OsStr::new(command_text)]
        );
        command
            .env_clear()
            .env("PATH", "/caller-supplied-path-override");
        let output = command
            .output()
            .expect("absolute shell command should execute independently of child PATH");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"alpha beta;\"gamma\"");
    }

    #[test]
    #[cfg(not(target_env = "musl"))]
    fn current_executable_exposes_a_gnu_build_id() {
        let build_id = super::current_executable_build_id()
            .expect("Linux test executable should carry a GNU build ID");
        assert!(!build_id.is_empty());
    }
}
#[cfg(test)]
#[path = "tests/platform_linux_coverage.rs"]
mod coverage_tests;
#[path = "sync_spawn_group.rs"]
mod sync_spawn;
pub use sync_spawn::{spawn_sync, spawn_sync_daemon};

/// Launch the private Wasm worker with no inherited ambient stdio and an
/// explicit empty environment. Guest capabilities travel over the worker
/// protocol, never through inherited descriptors or environment variables.
#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
pub(crate) fn spawn_contained_worker(
    command: &mut std::process::Command,
    _limits: crate::platform::process::WorkerLimits,
) -> Result<crate::platform::process::WorkerChild, crate::platform::process::WorkerError> {
    use crate::platform::process::{SpawnStdio, StdioSource, SyncEnvironment, WorkerError, WorkerStage};
    let child = spawn_sync(
        command,
        SpawnStdio {
            stdin: StdioSource::Pipe,
            stdout: StdioSource::Pipe,
            stderr: StdioSource::Null,
            drain_timeout: None,
            show_console: false,
        },
        SyncEnvironment::Explicit(Vec::new()),
    )
    .map_err(|error| WorkerError::new(WorkerStage::Create, error))?;
    worker_from_spawned_child(child)
}

#[cfg(all(feature = "tauri-webview", feature = "wasm-sketch-worker"))]
pub(crate) fn configure_native_worker_environment(command: &mut std::process::Command) {
    // Host-selected display/session and native loader settings, never guest input.
    // Do not inherit HOME, credentials, or the full ambient environment.
    for key in ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS", "LD_LIBRARY_PATH", "GDK_BACKEND", "LIBGL_ALWAYS_SOFTWARE", "NO_AT_BRIDGE", "WEBKIT_DISABLE_COMPOSITING_MODE"] {
        if let Some(value) = std::env::var_os(key) { command.env(key, value); }
    }
}

#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
fn worker_from_spawned_child(
    child: crate::platform::process::SpawnedChild,
) -> Result<crate::platform::process::WorkerChild, crate::platform::process::WorkerError> {
    let (stdin, stdout, pid, inner) = child.into_worker_parts();
    Ok(crate::platform::process::WorkerChild::new(
        stdin,
        stdout,
        pid,
        Box::new(UnixWorkerControl { inner }),
    ))
}

#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
struct UnixWorkerControl {
    inner: Box<dyn crate::platform::process::SpawnedChildControl>,
}

impl crate::platform::process::WorkerChildControl for UnixWorkerControl {
    fn try_wait(&mut self) -> io::Result<Option<i32>> { self.inner.try_wait() }

    fn force_and_reap(&mut self, timeout: std::time::Duration) -> Result<(), crate::platform::process::WorkerError> {
        use crate::platform::process::{WorkerError, WorkerStage};
        self.inner.kill().map_err(|error| WorkerError::new(WorkerStage::Terminate, error))?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.inner.try_wait().map_err(|error| WorkerError::new(WorkerStage::Reap, error))?.is_some() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(WorkerError::new(WorkerStage::Reap, io::Error::new(io::ErrorKind::TimedOut, "contained worker did not exit")));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn shutdown(&mut self) { self.inner.shutdown(); }
}

#[cfg(all(test, feature = "ipc"))]
mod endpoint_naming_tests {
    use super::{ipc_broker_v1_endpoint_path, ipc_endpoint_name_limit, LINUX_SUN_PATH_MAX};

    #[test]
    fn the_v1_address_keeps_the_full_name_for_debuggability() {
        let address = ipc_broker_v1_endpoint_path("rpb-v1-abc-shared").expect("derive address");
        assert!(address.contains("rpb-v1-abc-shared"));
        assert!(address.ends_with("-shared.sock"));
        assert!(address.contains("/broker/"));
    }

    #[test]
    fn an_over_long_name_is_refused_against_sun_path() {
        let err = ipc_broker_v1_endpoint_path(&"a".repeat(LINUX_SUN_PATH_MAX))
            .expect_err("must exceed sun_path");
        assert_eq!(err.max, LINUX_SUN_PATH_MAX - 1);
        assert_eq!(err.limit_label, "Linux sun_path");
    }

    #[test]
    fn an_accepted_address_is_strictly_shorter_than_the_field() {
        // sockaddr_un is NUL-terminated, so equality with the field width
        // would truncate the terminator.
        let address = ipc_broker_v1_endpoint_path("rpb-v1-abc-shared").expect("derive address");
        assert!(address.len() < LINUX_SUN_PATH_MAX);
    }

    #[test]
    fn the_reported_budget_is_sun_path() {
        let limit = ipc_endpoint_name_limit();
        assert_eq!(limit.max_bytes, LINUX_SUN_PATH_MAX);
        assert_eq!(limit.label, "Linux sun_path");
    }

    #[test]
    fn the_scope_spelling_is_the_verbatim_path_bytes() {
        // Paths are opaque byte strings here: no spelling difference is
        // meaningless, and case is significant. This pins the spelling --
        // changing it re-scopes every deployed broker, and the stability
        // tests upstream would not notice.
        use super::ipc_endpoint_scope_bytes;

        let bytes = ipc_endpoint_scope_bytes(std::path::Path::new("/usr/local/bin/Broker"));
        assert_eq!(bytes, b"/usr/local/bin/Broker".to_vec());

        let lowered = ipc_endpoint_scope_bytes(std::path::Path::new("/usr/local/bin/broker"));
        assert_ne!(bytes, lowered, "case must remain significant");
    }

}

/// Replace this process's image with `command`.
///
/// Returns only on failure: on success `execve` has already replaced the
/// program and there is nothing left to return to. That is why the signature
/// yields `io::Error` rather than `io::Result<()>` -- an `Ok` would name a
/// state that cannot be observed.
pub fn process_replace_current_image(command: &mut std::process::Command) -> std::io::Error {
    use std::os::unix::process::CommandExt as _;
    command.exec()
}

/// This host replaces a running image in place; see the facade for what that
/// means for a caller that cannot accept a successor instead.
pub const fn process_can_replace_current_image() -> bool {
    true
}
#[path = "platform_linux/interrupt.rs"]
pub(crate) mod interrupt;
#[path = "platform_linux/termination.rs"]
pub(crate) mod termination;

#[cfg(feature = "fs-scan")]
#[path = "platform_linux/fs_scan.rs"]
pub(crate) mod fs_scan;
