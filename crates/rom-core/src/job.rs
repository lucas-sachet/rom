//! Process tree ownership: Job Object on Windows, process group on Unix.
//!
//! Stopping terminates the whole tree. Dropping the last handle also reaps leftovers
//! on Windows (`KILL_ON_JOB_CLOSE`); on Unix drop sends `SIGKILL` to the group.

#[cfg(windows)]
mod windows_tree {
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

    #[derive(Debug)]
    pub struct ProcessTree {
        handle: usize,
    }

    impl ProcessTree {
        pub fn new() -> io::Result<Self> {
            // SAFETY: null security attributes and name are valid for CreateJobObjectW.
            let handle = unsafe { CreateJobObjectW(null(), null()) };
            if handle as usize == 0 {
                return Err(io::Error::last_os_error());
            }
            let tree = Self {
                handle: handle as usize,
            };
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = unsafe {
                SetInformationJobObject(
                    tree.handle as HANDLE,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(tree)
        }

        pub fn assign(&self, process: RawHandle) -> io::Result<()> {
            let ok = unsafe { AssignProcessToJobObject(self.handle as HANDLE, process as HANDLE) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn terminate(&self) -> io::Result<()> {
            let ok = unsafe { TerminateJobObject(self.handle as HANDLE, 1) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for ProcessTree {
        fn drop(&mut self) {
            // SAFETY: handle created by CreateJobObjectW and closed only here.
            unsafe {
                CloseHandle(self.handle as HANDLE);
            }
        }
    }
}

#[cfg(unix)]
mod unix_tree {
    use std::io;
    use std::sync::Mutex;

    #[derive(Debug)]
    pub struct ProcessTree {
        pgid: Mutex<Option<i32>>,
    }

    impl ProcessTree {
        pub fn new() -> io::Result<Self> {
            Ok(Self {
                pgid: Mutex::new(None),
            })
        }

        pub fn assign_pgid(&self, pid: u32) -> io::Result<()> {
            let Ok(pid) = i32::try_from(pid) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "pid out of range",
                ));
            };
            *self.pgid.lock().unwrap_or_else(|e| e.into_inner()) = Some(pid);
            Ok(())
        }

        pub fn terminate(&self) -> io::Result<()> {
            let pgid = *self.pgid.lock().unwrap_or_else(|e| e.into_inner());
            let Some(pgid) = pgid else {
                return Ok(());
            };
            let result = unsafe { libc::kill(-pgid, libc::SIGKILL) };
            if result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }

    impl Drop for ProcessTree {
        fn drop(&mut self) {
            let _ = self.terminate();
        }
    }
}

#[cfg(unix)]
pub use unix_tree::ProcessTree;
#[cfg(windows)]
pub use windows_tree::ProcessTree;
