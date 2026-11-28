//! Windows ConPTY implementation.

#[cfg(feature = "pty")]
#[path = "terminal/conpty_passthrough/mod.rs"]
mod conpty_passthrough;

#[cfg(feature = "pty")]
use crate::platform::terminal::{
    PtyBackend, PtyChild, PtyMaster, PtySize, PtySlave,
};
#[cfg(feature = "pty")]
use std::ffi::OsString;
#[cfg(feature = "pty")]
use std::io::{self, Read, Write};
#[cfg(feature = "pty")]
use std::path::Path;

#[cfg(feature = "pty")]
pub use conpty_passthrough::conpty_api::{current_backend_kind, ConPtyBackendKind};

#[cfg(feature = "pty")]
pub struct ConPtyBackend;

#[cfg(feature = "pty")]
impl PtyBackend for ConPtyBackend {
    type Master = conpty_passthrough::ConPtyMaster;
    type Slave = conpty_passthrough::ConPtySlave;

    fn openpty(size: PtySize) -> io::Result<(Self::Master, Self::Slave)> {
        let pair = conpty_passthrough::openpty(conpty_passthrough::PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
        })?;
        Ok((pair.master, pair.slave))
    }
}

#[cfg(feature = "pty")]
impl PtyMaster for conpty_passthrough::ConPtyMaster {
    fn try_clone_reader(&mut self) -> io::Result<Box<dyn Read + Send>> {
        conpty_passthrough::ConPtyMaster::try_clone_reader(self)
    }

    fn take_writer(&mut self) -> io::Result<Box<dyn Write + Send>> {
        conpty_passthrough::ConPtyMaster::take_writer(self)
    }

    fn resize(&self, size: PtySize) -> io::Result<()> {
        conpty_passthrough::ConPtyMaster::resize(
            self,
            conpty_passthrough::PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
            },
        )
    }

    fn get_size(&self) -> io::Result<PtySize> {
        let size = conpty_passthrough::ConPtyMaster::get_size(self);
        Ok(PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
        })
    }

}

#[cfg(feature = "pty")]
impl PtySlave for conpty_passthrough::ConPtySlave {
    type Child = conpty_passthrough::child::ConPtyChild;

    fn spawn(
        self,
        argv: &[OsString],
        cwd: Option<&Path>,
        env: Option<&[(OsString, OsString)]>,
    ) -> io::Result<Self::Child> {
        conpty_passthrough::ConPtySlave::spawn(self, argv, cwd, env)
    }
}

#[cfg(feature = "pty")]
impl PtyChild for conpty_passthrough::child::ConPtyChild {
    fn pid(&self) -> u32 {
        conpty_passthrough::child::ConPtyChild::pid(self)
    }

    fn try_wait(&mut self) -> io::Result<Option<u32>> {
        conpty_passthrough::child::ConPtyChild::try_wait(self)
    }

    fn wait(&mut self) -> io::Result<u32> {
        conpty_passthrough::child::ConPtyChild::wait(self)
    }

    fn kill(&mut self) -> io::Result<()> {
        conpty_passthrough::child::ConPtyChild::kill(self)
    }

    fn as_raw_handle(&self) -> Option<*mut std::ffi::c_void> {
        Some(conpty_passthrough::child::ConPtyChild::as_raw_handle(self).cast())
    }

    fn prepare_process(
        &self,
        context: PtySpawnContext,
        nice: Option<i32>,
    ) -> std::io::Result<PtyProcessGuard> {
        prepare_conpty_child(
            context,
            conpty_passthrough::child::ConPtyChild::as_raw_handle(self).cast(),
            nice,
        )
    }
}

#[cfg(feature = "pty")]
pub type Backend = ConPtyBackend;

#[cfg(feature = "terminal-input")]
use crate::platform::terminal::PtyInputChunk;

#[cfg(feature = "pty")]
pub struct PtySpawnContext(Vec<u32>);

#[cfg(feature = "pty")]
pub struct PtyProcessGuard(usize);

#[cfg(feature = "pty")]
impl PtyProcessGuard {
    pub fn assign_pid(&self, pid: u32) -> std::io::Result<()> {
        use winapi::um::handleapi::CloseHandle;
        use winapi::um::processthreadsapi::OpenProcess;
        use winapi::um::winnt::{PROCESS_SET_QUOTA, PROCESS_TERMINATE};
        let handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if handle.is_null() { return Err(std::io::Error::last_os_error()); }
        let result = unsafe {
            winapi::um::jobapi2::AssignProcessToJobObject(
                self.0 as winapi::shared::ntdef::HANDLE,
                handle,
            )
        };
        unsafe { CloseHandle(handle) };
        if result == 0 { return Err(std::io::Error::last_os_error()); }
        Ok(())
    }
}

#[cfg(feature = "pty")]
impl Drop for PtyProcessGuard {
    fn drop(&mut self) {
        unsafe { winapi::um::handleapi::CloseHandle(self.0 as winapi::shared::ntdef::HANDLE) };
    }
}

#[cfg(feature = "pty")]
#[derive(Debug, Clone)]
pub struct ChildProcessInfo { pub pid: u32, pub name: String }

#[cfg(feature = "pty")]
#[derive(Debug, Clone)]
pub struct OrphanConhostInfo { pub pid: u32, pub parent_pid: u32, pub parent_name: String }

#[cfg(feature = "pty")]
pub fn find_child_processes(parent_pid: u32) -> Vec<ChildProcessInfo> {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::tlhelp32::{CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS};
    let mut children = Vec::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == winapi::um::handleapi::INVALID_HANDLE_VALUE { return children; }
    let mut entry: PROCESSENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;
    if unsafe { Process32First(snapshot, &mut entry) } != 0 {
        loop {
            if entry.th32ParentProcessID == parent_pid {
                let length = entry.szExeFile.iter().position(|&byte| byte == 0).unwrap_or(entry.szExeFile.len());
                let name = String::from_utf8_lossy(&entry.szExeFile[..length].iter().map(|&byte| byte as u8).collect::<Vec<_>>()).into_owned();
                children.push(ChildProcessInfo { pid: entry.th32ProcessID, name });
            }
            if unsafe { Process32Next(snapshot, &mut entry) } == 0 { break; }
        }
    }
    unsafe { CloseHandle(snapshot) };
    children
}

#[cfg(feature = "pty")]
fn conhost_children_of_current_process() -> Vec<u32> {
    find_child_processes(std::process::id()).into_iter()
        .filter(|child| child.name.eq_ignore_ascii_case("conhost.exe"))
        .map(|child| child.pid).collect()
}

#[cfg(feature = "pty")]
pub fn find_orphan_conhosts() -> Vec<OrphanConhostInfo> {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::tlhelp32::{CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS};
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == winapi::um::handleapi::INVALID_HANDLE_VALUE { return Vec::new(); }
    let mut entry: PROCESSENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;
    let mut all_pids = std::collections::HashSet::new();
    let mut conhosts = Vec::new();
    let mut names = std::collections::HashMap::new();
    if unsafe { Process32First(snapshot, &mut entry) } != 0 {
        loop {
            let length = entry.szExeFile.iter().position(|&byte| byte == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf8_lossy(&entry.szExeFile[..length].iter().map(|&byte| byte as u8).collect::<Vec<_>>()).into_owned();
            all_pids.insert(entry.th32ProcessID);
            names.insert(entry.th32ProcessID, name.clone());
            if name.eq_ignore_ascii_case("conhost.exe") { conhosts.push((entry.th32ProcessID, entry.th32ParentProcessID)); }
            if unsafe { Process32Next(snapshot, &mut entry) } == 0 { break; }
        }
    }
    unsafe { CloseHandle(snapshot) };
    conhosts.into_iter().filter(|(_, parent)| !all_pids.contains(parent)).map(|(pid, parent_pid)| OrphanConhostInfo {
        pid, parent_pid, parent_name: names.get(&parent_pid).cloned().unwrap_or_default(),
    }).collect()
}

#[cfg(feature = "pty")]
pub fn before_pty_spawn() -> PtySpawnContext {
    PtySpawnContext(conhost_children_of_current_process())
}

#[cfg(feature = "pty")]
fn apply_priority(handle: *mut std::ffi::c_void, nice: Option<i32>) -> std::io::Result<()> {
    use winapi::um::processthreadsapi::SetPriorityClass;
    use winapi::um::winbase::{ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS};
    let flags = match nice {
        Some(value) if value >= 15 => IDLE_PRIORITY_CLASS,
        Some(value) if value >= 1 => BELOW_NORMAL_PRIORITY_CLASS,
        Some(value) if value <= -15 => HIGH_PRIORITY_CLASS,
        Some(value) if value <= -1 => ABOVE_NORMAL_PRIORITY_CLASS,
        _ => 0,
    };
    if flags != 0 && unsafe { SetPriorityClass(handle.cast(), flags) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(feature = "pty")]
fn prepare_conpty_child(
    context: PtySpawnContext,
    handle: *mut std::ffi::c_void,
    nice: Option<i32>,
) -> std::io::Result<PtyProcessGuard> {
    use winapi::shared::minwindef::FALSE;
    use winapi::um::jobapi2::{AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject};
    use winapi::um::winnt::{JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE};
    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() || job == winapi::um::handleapi::INVALID_HANDLE_VALUE { return Err(std::io::Error::last_os_error()); }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    let configured = unsafe { SetInformationJobObject(job, JobObjectExtendedLimitInformation, (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) };
    if configured == FALSE {
        let error = std::io::Error::last_os_error(); unsafe { winapi::um::handleapi::CloseHandle(job) }; return Err(error);
    }
    if unsafe { AssignProcessToJobObject(job, handle.cast()) } == FALSE {
        let error = std::io::Error::last_os_error(); unsafe { winapi::um::handleapi::CloseHandle(job) }; return Err(error);
    }
    let guard = PtyProcessGuard(job as usize);
    for pid in conhost_children_of_current_process() {
        if !context.0.contains(&pid) { let _ = guard.assign_pid(pid); }
    }
    apply_priority(handle, nice)?;
    Ok(guard)
}

#[cfg(feature = "pty")]
pub fn prepare_unmanaged_pty_child(
    _context: PtySpawnContext,
    _nice: Option<i32>,
) -> std::io::Result<PtyProcessGuard> {
    Err(std::io::Error::other(
        "Pseudo-terminal child does not expose a Windows process handle",
    ))
}

#[cfg(feature = "pty")]
pub fn input_payload(data: &[u8]) -> Vec<u8> {
    let mut translated = Vec::with_capacity(data.len());
    let mut index = 0;
    while index < data.len() {
        match data[index] {
            b'\r' => {
                translated.push(b'\r');
                if data.get(index + 1) == Some(&b'\n') { translated.push(b'\n'); index += 2; } else { index += 1; }
            }
            b'\n' => { translated.push(b'\r'); index += 1; }
            byte => { translated.push(byte); index += 1; }
        }
    }
    translated
}

#[cfg(feature = "pty")]
pub fn query_responses(data: &[u8]) -> Vec<Vec<u8>> {
    let query = b"\x1b[6n";
    data.windows(query.len()).filter(|window| *window == query).map(|_| b"\x1b[1;1R".to_vec()).collect()
}

#[cfg(feature = "pty")]
pub fn shell_argv(command: &str) -> Vec<String> {
    vec!["cmd.exe".into(), "/C".into(), command.into()]
}

#[cfg(feature = "pty")]
pub fn wait_before_pty_close_supported() -> bool { false }

#[cfg(feature = "pty")]
pub fn is_ignorable_process_control_error(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput)
}

#[cfg(feature = "pty")]
pub fn terminate_pty_child(_pid: u32) -> std::io::Result<bool> { Ok(true) }

#[cfg(feature = "pty")]
pub fn signal_pty_tree(_pid: u32, _force: bool) -> std::io::Result<bool> { Ok(true) }

#[cfg(feature = "pty")]
pub fn resize_pty(
    _master: &dyn crate::platform::terminal::PtyMaster,
    _size: crate::platform::terminal::PtySize,
) -> std::io::Result<()> { Ok(()) }

#[cfg(feature = "terminal-input")]
pub struct TerminalInputSession(super::terminal_input::TerminalInputCore);

#[cfg(feature = "terminal-input")]
impl TerminalInputSession {
    pub(crate) fn new_for_keys() -> std::io::Result<Option<Self>> {
        let input = super::terminal_input::TerminalInputCore::new();
        input.start_for_keys()?;
        Ok(Some(Self(input)))
    }

    pub fn new() -> std::io::Result<Option<Self>> {
        let input = super::terminal_input::TerminalInputCore::new();
        input.start_impl()?;
        Ok(Some(Self(input)))
    }

    pub fn read_chunk(&self, timeout: std::time::Duration) -> std::io::Result<Option<PtyInputChunk>> {
        use super::terminal_input::{TerminalInputWaitOutcome, wait_for_terminal_input_event};
        match wait_for_terminal_input_event(&self.0.state, &self.0.condvar, Some(timeout)) {
            TerminalInputWaitOutcome::Failed(error) => Err(std::io::Error::other(error)),
            TerminalInputWaitOutcome::Event(event) => Ok(Some(PtyInputChunk { data: event.data, submit: event.submit })),
            TerminalInputWaitOutcome::Timeout => Ok(None),
            TerminalInputWaitOutcome::Closed => Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "native terminal input closed")),
        }
    }
}

#[cfg(feature = "terminal-input")]
impl Drop for TerminalInputSession {
    fn drop(&mut self) { let _ = self.0.stop_impl(); }
}

pub fn active_graphics_probe(
    _timeout: std::time::Duration,
) -> crate::platform::terminal::TerminalGraphicsProbe {
    crate::platform::terminal::TerminalGraphicsProbe::default()
}

#[cfg(all(test, feature = "pty"))]
mod tests {
    use super::*;

    #[test]
    fn assign_child_to_job_null_handle_errors() {
        assert!(prepare_unmanaged_pty_child(before_pty_spawn(), None).is_err());
    }

    #[test]
    fn apply_windows_pty_priority_zero_nice_noop() {
        let handle = std::ptr::NonNull::<std::ffi::c_void>::dangling().as_ptr();
        assert!(apply_priority(handle, Some(0)).is_ok());
        assert!(apply_priority(handle, None).is_ok());
    }
}
