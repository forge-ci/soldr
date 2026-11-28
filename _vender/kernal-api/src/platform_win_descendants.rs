//! Windows Job Object containment and descendant lifecycle delivery.

use std::process::Child;

use crate::platform::process::DescendantEvent;

/// Owns the kill-on-close Job Object and its optional completion port.
pub struct WindowsJobHandle {
    job: usize,
    iocp: Option<usize>,
}

impl Drop for WindowsJobHandle {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.job as winapi::shared::ntdef::HANDLE);
            if let Some(port) = self.iocp.take() {
                winapi::um::handleapi::CloseHandle(port as winapi::shared::ntdef::HANDLE);
            }
        }
    }
}

/// Put `child` in a kill-on-close Job Object and, when requested, attach the
/// Job to an IOCP before assignment so no initial notification can race past.
pub fn assign_child_to_windows_job(
    child: &Child,
    direct_pid: u32,
    address_space_limit_bytes: Option<u64>,
    emit: Option<Box<dyn Fn(DescendantEvent) + Send>>,
) -> Result<WindowsJobHandle, std::io::Error> {
    use std::mem::zeroed;
    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::jobapi2::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    };
    use winapi::um::winnt::{
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };

    let handle = super::sync_child_native_handle(child);
    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() || job == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    let mut limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    if address_space_limit_bytes.is_some() {
        limit_flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    }
    info.BasicLimitInformation.LimitFlags = limit_flags;
    if let Some(limit) = address_space_limit_bytes {
        #[allow(clippy::cast_possible_truncation)]
        {
            info.ProcessMemoryLimit = limit as usize;
        }
    }
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw mut info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == FALSE {
        let error = std::io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(error);
    }

    let iocp = match emit {
        Some(emit) => match attach_iocp_pump(job, emit, direct_pid) {
            Ok(port) => Some(port),
            Err(error) => {
                unsafe { CloseHandle(job) };
                return Err(error);
            }
        },
        None => None,
    };

    if unsafe { AssignProcessToJobObject(job, handle as _) } == FALSE {
        let error = std::io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        if let Some(port) = iocp {
            unsafe { CloseHandle(port as winapi::shared::ntdef::HANDLE) };
        }
        return Err(error);
    }

    Ok(WindowsJobHandle {
        job: job as usize,
        iocp,
    })
}

fn attach_iocp_pump(
    job: winapi::shared::ntdef::HANDLE,
    emit: Box<dyn Fn(DescendantEvent) + Send>,
    direct_pid: u32,
) -> Result<usize, std::io::Error> {
    use std::mem::zeroed;
    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::ioapiset::CreateIoCompletionPort;
    use winapi::um::jobapi2::SetInformationJobObject;
    use winapi::um::winnt::{
        JobObjectAssociateCompletionPortInformation, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
    };

    let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
    if port.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let mut assoc: JOBOBJECT_ASSOCIATE_COMPLETION_PORT = unsafe { zeroed() };
    assoc.CompletionKey = job.cast();
    assoc.CompletionPort = port;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectAssociateCompletionPortInformation,
            (&raw mut assoc).cast(),
            std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
        )
    };
    if ok == FALSE {
        let error = std::io::Error::last_os_error();
        unsafe { winapi::um::handleapi::CloseHandle(port) };
        return Err(error);
    }

    let port_address = port as usize;
    std::thread::Builder::new()
        .name("rp-job-iocp-pump".to_owned())
        .spawn(move || iocp_pump_loop(port_address, emit, direct_pid))
        .map_err(|error| {
            unsafe { winapi::um::handleapi::CloseHandle(port) };
            std::io::Error::other(format!("spawn IOCP pump thread: {error}"))
        })?;
    Ok(port_address)
}

fn iocp_pump_loop(
    port_address: usize,
    emit: Box<dyn Fn(DescendantEvent) + Send>,
    direct_pid: u32,
) {
    use winapi::shared::minwindef::{DWORD, FALSE, LPDWORD};
    use winapi::um::ioapiset::GetQueuedCompletionStatus;
    use winapi::um::minwinbase::LPOVERLAPPED;

    const ACTIVE_PROCESS_ZERO: u32 = 4;
    const NEW_PROCESS: u32 = 6;
    const EXIT_PROCESS: u32 = 7;
    const ABNORMAL_EXIT_PROCESS: u32 = 8;
    let port = port_address as winapi::shared::ntdef::HANDLE;
    loop {
        let mut message: DWORD = 0;
        let mut completion_key: usize = 0;
        let mut overlapped: LPOVERLAPPED = std::ptr::null_mut();
        let ok = unsafe {
            GetQueuedCompletionStatus(
                port,
                &raw mut message as LPDWORD,
                &raw mut completion_key as *mut _,
                &raw mut overlapped,
                winapi::um::winbase::INFINITE,
            )
        };
        if ok == FALSE {
            emit(DescendantEvent::Completed);
            break;
        }
        let pid = overlapped as usize as u32;
        match message {
            // The job-object IOCP notification is PID-only; resolving the
            // parent would need a toolhelp scan per event, racy for
            // short-lived processes. Report unknown instead.
            NEW_PROCESS if pid != direct_pid => emit(DescendantEvent::Started {
                pid,
                parent_pid: None,
            }),
            EXIT_PROCESS | ABNORMAL_EXIT_PROCESS if pid != direct_pid => {
                emit(DescendantEvent::Exited(pid));
            }
            ACTIVE_PROCESS_ZERO => {
                emit(DescendantEvent::Completed);
                break;
            }
            _ => {}
        }
    }
}
