use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, GetLastError};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

/// The processes that die together: closing the job, even by a crash of this process, ends
/// every process in it. What a process of the job starts leaves the job at once, so a
/// window program the user opens from the terminal is not ended with it; the console
/// programs are pulled back in by [`super::process_tree::ProcessTree::adopt`].
pub(crate) struct Job {
    handle: OwnedHandle,
}

impl Job {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: both pointers may be null, a non-null result is ours to own.
        let handle = unsafe { CreateJobObjectW(null(), null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the handle is valid and nothing else owns it.
        let job = Self {
            handle: unsafe { OwnedHandle::from_raw_handle(handle) },
        };
        job.set_limits()?;
        Ok(job)
    }

    fn set_limits(&self) -> io::Result<()> {
        // SAFETY: an all-zero struct is valid, it is plain data.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // Without the breakaway every child of the shell would be born in the job, a window
        // program too, and `TerminateJobObject` or the close would end it with the session.
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
        // SAFETY: the pointer and the size describe `limits`.
        let done = unsafe {
            SetInformationJobObject(
                self.handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        check(done)
    }

    /// Assign before the process starts running, or whatever it spawns first stays
    /// outside the job.
    pub(crate) fn assign(&self, process: impl AsHandle) -> io::Result<()> {
        let process = process.as_handle().as_raw_handle();
        // SAFETY: both handles are valid for the call.
        check(unsafe { AssignProcessToJobObject(self.handle.as_raw_handle(), process) })
    }

    /// Pulls a running process into the job, for a program whose children left it.
    pub(crate) fn assign_id(&self, pid: u32) -> io::Result<()> {
        // SAFETY: the handle is checked and closed below.
        let handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a non-null result is ours to own.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        // SAFETY: both handles are valid for the call.
        check(unsafe {
            AssignProcessToJobObject(self.handle.as_raw_handle(), handle.as_raw_handle())
        })
    }

    /// Ends every process in the job. Returns before they are gone.
    pub(crate) fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: the handle is valid.
        check(unsafe { TerminateJobObject(self.handle.as_raw_handle(), exit_code) })
    }

    /// Ids of the processes alive in the job. Only the tests ask, to see what the job holds.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn process_ids(&self) -> io::Result<Vec<u32>> {
        let header = offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList);
        let mut capacity = 64;
        loop {
            // A `usize` buffer has the alignment of the list.
            let bytes = header + capacity * size_of::<usize>();
            let mut buffer = vec![0usize; bytes.div_ceil(size_of::<usize>())];
            let list = buffer
                .as_mut_ptr()
                .cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            // SAFETY: the buffer holds `bytes` bytes and is aligned for the list.
            let done = unsafe {
                QueryInformationJobObject(
                    self.handle.as_raw_handle(),
                    JobObjectBasicProcessIdList,
                    list.cast(),
                    bytes as u32,
                    null_mut(),
                )
            };
            // SAFETY: a thread-local read right after the call.
            if done == 0 && unsafe { GetLastError() } != ERROR_MORE_DATA {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the call filled the header, which lies inside the buffer.
            let (assigned, listed) = unsafe {
                let list = &*list;
                (
                    list.NumberOfAssignedProcesses,
                    list.NumberOfProcessIdsInList,
                )
            };
            if assigned > listed {
                capacity = assigned as usize + 16;
                continue;
            }
            let ids = buffer[header / size_of::<usize>()..].iter();
            return Ok(ids.take(listed as usize).map(|&id| id as u32).collect());
        }
    }
}

fn check(done: i32) -> io::Result<()> {
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
