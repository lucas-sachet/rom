//! Windows Job Object wrapper. A job owns a whole process tree.

use std::ffi::c_void;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::RawHandle;
use std::ptr::null;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};

/// Every descendant of an assigned process joins the job. `terminate` kills the whole tree.
/// Closing the last handle (including when the daemon exits) kills whatever is left,
/// so nothing is orphaned.
#[derive(Debug)]
pub struct JobObject {
    handle: usize,
}

impl JobObject {
    pub fn new() -> io::Result<Self> {
        // SAFETY: null security attributes and name are valid arguments for CreateJobObjectW.
        let handle = unsafe { CreateJobObjectW(null(), null()) };
        if handle as usize == 0 {
            return Err(io::Error::last_os_error());
        }
        // Wrap first so the handle is closed if the configuration below fails.
        let job = Self {
            handle: handle as usize,
        };

        // SAFETY: JOBOBJECT_EXTENDED_LIMIT_INFORMATION is plain old data; zero is a valid value.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `info` matches the declared information class and size.
        let ok = unsafe {
            SetInformationJobObject(
                job.handle as HANDLE,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    pub fn assign(&self, process: RawHandle) -> io::Result<()> {
        // SAFETY: both handles are valid for the lifetime of this call.
        let ok = unsafe { AssignProcessToJobObject(self.handle as HANDLE, process as HANDLE) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn terminate(&self) -> io::Result<()> {
        // SAFETY: the handle is owned by `self` and still open.
        let ok = unsafe { TerminateJobObject(self.handle as HANDLE, 1) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // SAFETY: the handle was created by CreateJobObjectW and is closed only here.
        unsafe {
            CloseHandle(self.handle as HANDLE);
        }
    }
}
