//! Launches a long-running process detached from the caller.
//!
//! On Windows, `CreateProcessW` inherits only the three standard handles so a capturing
//! shell is not blocked. On Unix, stdout/stderr go to the log and `setsid` detaches from
//! the controlling terminal.

use std::fs::File;
use std::io;
use std::path::Path;

/// Starts `exe` with `args` in `cwd`, with stdout and stderr going to `log`, and no stdin.
/// Returns the new process id. The child is not waited on and is not killed when the caller exits.
pub fn spawn_detached(exe: &Path, args: &[&str], cwd: &Path, log: &File) -> io::Result<u32> {
    #[cfg(windows)]
    {
        windows_detach::spawn_detached(exe, args, cwd, log)
    }
    #[cfg(unix)]
    {
        unix_detach::spawn_detached(exe, args, cwd, log)
    }
}

#[cfg(windows)]
mod windows_detach {
    use std::ffi::{OsStr, c_void};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::iter::repeat_n;
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, UpdateProcThreadAttribute,
    };

    pub fn spawn_detached(exe: &Path, args: &[&str], cwd: &Path, log: &File) -> io::Result<u32> {
        let stdin = OpenOptions::new().read(true).write(true).open("NUL")?;
        let stdin_handle = stdin.as_raw_handle() as HANDLE;
        let log_handle = log.as_raw_handle() as HANDLE;
        let mut handles: [HANDLE; 2] = [stdin_handle, log_handle];

        for handle in handles {
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
        }

        let mut command_line = wide(
            &std::iter::once(quote_arg(&exe.to_string_lossy()))
                .chain(args.iter().map(|arg| quote_arg(arg)))
                .collect::<Vec<_>>()
                .join(" "),
        );
        let current_dir = wide_os(cwd.as_os_str());

        let mut attribute_size = 0usize;
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attribute_size) };
        let mut attribute_storage = vec![0usize; attribute_size.div_ceil(size_of::<usize>())];
        let attribute_list =
            attribute_storage.as_mut_ptr().cast::<c_void>() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if unsafe { InitializeProcThreadAttributeList(attribute_list, 1, 0, &mut attribute_size) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        let updated = unsafe {
            UpdateProcThreadAttribute(
                attribute_list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_mut_ptr().cast::<c_void>(),
                size_of::<HANDLE>() * handles.len(),
                null_mut(),
                null_mut(),
            )
        };
        if updated == 0 {
            let error = io::Error::last_os_error();
            unsafe { DeleteProcThreadAttributeList(attribute_list) };
            return Err(error);
        }

        let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
        startup.StartupInfo = STARTUPINFOW {
            cb: size_of::<STARTUPINFOEXW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: stdin_handle,
            hStdOutput: log_handle,
            hStdError: log_handle,
            ..unsafe { zeroed() }
        };
        startup.lpAttributeList = attribute_list;
        let mut process: PROCESS_INFORMATION = unsafe { zeroed() };

        let created = unsafe {
            CreateProcessW(
                null(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                1,
                DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | EXTENDED_STARTUPINFO_PRESENT,
                null(),
                current_dir.as_ptr(),
                (&startup as *const STARTUPINFOEXW).cast::<STARTUPINFOW>(),
                &mut process,
            )
        };
        let error = (created == 0).then(io::Error::last_os_error);

        unsafe { DeleteProcThreadAttributeList(attribute_list) };
        if let Some(error) = error {
            return Err(error);
        }
        unsafe {
            CloseHandle(process.hThread);
            CloseHandle(process.hProcess);
        }
        Ok(process.dwProcessId)
    }

    fn quote_arg(arg: &str) -> String {
        if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
            return arg.to_owned();
        }
        let mut quoted = String::from("\"");
        let mut backslashes = 0usize;
        for ch in arg.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    quoted.extend(repeat_n('\\', backslashes * 2 + 1));
                    quoted.push('"');
                    backslashes = 0;
                }
                _ => {
                    quoted.extend(repeat_n('\\', backslashes));
                    quoted.push(ch);
                    backslashes = 0;
                }
            }
        }
        quoted.extend(repeat_n('\\', backslashes * 2));
        quoted.push('"');
        quoted
    }

    fn wide(text: &str) -> Vec<u16> {
        wide_os(OsStr::new(text))
    }

    fn wide_os(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain(std::iter::once(0)).collect()
    }

    #[cfg(test)]
    mod tests {
        use super::quote_arg;

        #[test]
        fn quotes_only_when_needed() {
            assert_eq!(quote_arg("server.js"), "server.js");
            assert_eq!(quote_arg("with space"), "\"with space\"");
            assert_eq!(quote_arg(""), "\"\"");
        }

        #[test]
        fn escapes_quotes_and_trailing_backslashes() {
            assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
            assert_eq!(quote_arg("C:\\dir with\\"), "\"C:\\dir with\\\\\"");
        }
    }
}

#[cfg(unix)]
mod unix_detach {
    use std::fs::File;
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    pub fn spawn_detached(exe: &Path, args: &[&str], cwd: &Path, log: &File) -> io::Result<u32> {
        let stdout = log.try_clone()?;
        let stderr = log.try_clone()?;
        let mut command = Command::new(exe);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        // SAFETY: setsid detaches from the controlling terminal before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let pid = child.id();
        // Dropping Child waits on Unix; forget so the CLI returns while the daemon runs.
        // When the CLI exits, init reaps the daemon if needed.
        std::mem::forget(child);
        Ok(pid)
    }
}
