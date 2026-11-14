//! Spawn a service with its own console that stays hidden (no flashing windows).
//!
//! `CREATE_NEW_CONSOLE` is required so `CTRL_BREAK` / `SIGBREAK` still works.
//! `STARTF_USESHOWWINDOW` + `SW_HIDE` hides that console at creation time.

use std::collections::BTreeMap;
use std::ffi::{OsStr, c_void};
use std::fs::OpenOptions;
use std::io;
use std::iter::repeat_n;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::ptr::null;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, GetExitCodeProcess,
    INFINITE, PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTF_USESTDHANDLES, STARTUPINFOW,
    TerminateProcess, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

/// Child process started with a hidden console.
pub struct HiddenConsoleChild {
    pid: u32,
    handle: OwnedHandle,
}

impl HiddenConsoleChild {
    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn raw_handle(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }

    pub async fn wait(self) -> io::Result<ExitStatus> {
        let handle = self.handle.into_raw_handle() as usize;
        tokio::task::spawn_blocking(move || wait_handle(handle as HANDLE))
            .await
            .map_err(|error| io::Error::other(format!("wait task failed: {error}")))?
    }

    pub fn kill(&self) -> io::Result<()> {
        let ok = unsafe { TerminateProcess(self.handle.as_raw_handle() as HANDLE, 1) };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

fn wait_handle(handle: HANDLE) -> io::Result<ExitStatus> {
    // SAFETY: handle is an open process handle owned by us for this call.
    let waited = unsafe { WaitForSingleObject(handle, INFINITE) };
    if waited != WAIT_OBJECT_0 {
        let error = io::Error::last_os_error();
        unsafe {
            CloseHandle(handle);
        }
        return Err(error);
    }
    let mut code = 0u32;
    let ok = unsafe { GetExitCodeProcess(handle, &raw mut code) };
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ExitStatus::from_raw(code))
}

/// Starts `command` with `args` in `cwd`, stdout/stderr appended to `log_path`.
/// Extra env vars are merged over the current process environment.
pub fn spawn_hidden(
    command: &str,
    args: &[String],
    cwd: &Path,
    log_path: &Path,
    extra_env: &BTreeMap<String, String>,
) -> io::Result<HiddenConsoleChild> {
    let log = OpenOptions::new().create(true).append(true).open(log_path)?;
    let log_handle = log.as_raw_handle() as HANDLE;
    let stdin = OpenOptions::new().read(true).write(true).open("NUL")?;
    let stdin_handle = stdin.as_raw_handle() as HANDLE;

    for handle in [stdin_handle, log_handle] {
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }

    let mut command_line = wide(&std::iter::once(quote_arg(command))
        .chain(args.iter().map(|arg| quote_arg(arg)))
        .collect::<Vec<_>>()
        .join(" "));
    let current_dir = wide_os(cwd.as_os_str());
    let mut env = env_block(extra_env);

    let mut startup: STARTUPINFOW = unsafe { zeroed() };
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    startup.dwFlags = STARTF_USESHOWWINDOW | STARTF_USESTDHANDLES;
    startup.wShowWindow = SW_HIDE as u16;
    startup.hStdInput = stdin_handle;
    startup.hStdOutput = log_handle;
    startup.hStdError = log_handle;

    let mut process: PROCESS_INFORMATION = unsafe { zeroed() };
    let created = unsafe {
        CreateProcessW(
            null(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            1, // inherit the redirected std handles
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
            env.as_mut_ptr().cast::<c_void>(),
            current_dir.as_ptr(),
            &raw mut startup,
            &raw mut process,
        )
    };
    // Keep `log` / `stdin` alive until CreateProcess returns (handles duplicated into the child).
    drop(log);
    drop(stdin);

    if created == 0 {
        return Err(io::Error::last_os_error());
    }

    unsafe {
        CloseHandle(process.hThread);
    }
    // SAFETY: CreateProcess returned an owned process handle.
    let handle = unsafe { OwnedHandle::from_raw_handle(process.hProcess as RawHandle) };
    Ok(HiddenConsoleChild {
        pid: process.dwProcessId,
        handle,
    })
}

fn env_block(extra: &BTreeMap<String, String>) -> Vec<u16> {
    let mut map: BTreeMap<String, String> = std::env::vars().collect();
    for (key, value) in extra {
        map.insert(key.clone(), value.clone());
    }
    let mut block = Vec::new();
    for (key, value) in map {
        block.extend(OsStr::new(&key).encode_wide());
        block.push(u16::from(b'='));
        block.extend(OsStr::new(&value).encode_wide());
        block.push(0);
    }
    block.push(0);
    block
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
