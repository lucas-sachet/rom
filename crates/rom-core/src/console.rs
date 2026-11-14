//! Graceful stop signalling: `CTRL_BREAK` on Windows, `SIGTERM` on Unix.

use std::io;

/// Asks the service to exit gracefully. Force-kill is handled by [`crate::job::ProcessTree`].
pub fn send_graceful_stop(pid: u32) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows::send_ctrl_break(pid)
    }
    #[cfg(unix)]
    {
        unix::send_sigterm(pid)
    }
}

/// Best-effort hide of console windows owned by `pid` (Windows only; no-op elsewhere).
pub fn hide_console_windows(pid: u32) {
    #[cfg(windows)]
    windows::hide_console_windows(pid);
    #[cfg(unix)]
    let _ = pid;
}

/// If this process was launched as a Windows ctrl-break helper, deliver and exit.
/// Always returns `false` on Unix.
pub fn maybe_run_as_helper() -> bool {
    #[cfg(windows)]
    {
        windows::maybe_run_as_helper()
    }
    #[cfg(unix)]
    {
        false
    }
}

#[cfg(windows)]
pub use windows::{CTRL_BREAK_ARG, deliver_ctrl_break};

#[cfg(windows)]
mod windows {
    use std::io;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{HWND, TRUE};
    use windows_sys::Win32::System::Console::{
        AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, SW_HIDE, ShowWindow,
    };

    const STATUS_CONTROL_C_EXIT: i32 = -1073741510; // 0xC000013A as i32

    static IGNORING_CTRL: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn swallow_ctrl(_ctrl_type: u32) -> i32 {
        if IGNORING_CTRL.load(Ordering::SeqCst) {
            TRUE
        } else {
            0
        }
    }

    pub const CTRL_BREAK_ARG: &str = "__ctrl_break";

    pub fn maybe_run_as_helper() -> bool {
        let mut args = std::env::args();
        let _exe = args.next();
        if args.next().as_deref() != Some(CTRL_BREAK_ARG) {
            return false;
        }
        let pid: u32 = match args.next().and_then(|value| value.parse().ok()) {
            Some(pid) => pid,
            None => {
                eprintln!("usage: <exe> {CTRL_BREAK_ARG} <pid>");
                std::process::exit(2);
            }
        };
        match deliver_ctrl_break(pid) {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("ctrl-break helper: {error}");
                std::process::exit(1);
            }
        }
    }

    pub fn send_ctrl_break(pid: u32) -> io::Result<()> {
        let (exe, prefix) = ctrl_break_command()?;
        let mut command = Command::new(&exe);
        for arg in &prefix {
            command.arg(arg);
        }
        let status = command
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() || status.code() == Some(STATUS_CONTROL_C_EXIT) {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "ctrl-break helper exited with {status}"
        )))
    }

    fn ctrl_break_command() -> io::Result<(PathBuf, Vec<String>)> {
        if let Ok(path) = std::env::var("CARGO_BIN_EXE_rom-ctrl-break") {
            return Ok((PathBuf::from(path), Vec::new()));
        }
        let exe = std::env::current_exe()?;
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("rom-ctrl-break.exe");
            if sibling.is_file() {
                return Ok((sibling, Vec::new()));
            }
        }
        Ok((exe, vec![CTRL_BREAK_ARG.to_owned()]))
    }

    pub fn deliver_ctrl_break(pid: u32) -> io::Result<()> {
        IGNORING_CTRL.store(true, Ordering::SeqCst);
        if unsafe { SetConsoleCtrlHandler(Some(swallow_ctrl), TRUE) } == 0 {
            IGNORING_CTRL.store(false, Ordering::SeqCst);
            return Err(io::Error::last_os_error());
        }
        let _ = unsafe { SetConsoleCtrlHandler(None, TRUE) };

        unsafe {
            FreeConsole();
        }

        let mut last_error = io::Error::other("AttachConsole did not run");
        let mut attached = false;
        for _ in 0..20 {
            if unsafe { AttachConsole(pid) } != 0 {
                attached = true;
                break;
            }
            last_error = io::Error::last_os_error();
            thread::sleep(Duration::from_millis(25));
        }
        if !attached {
            IGNORING_CTRL.store(false, Ordering::SeqCst);
            return Err(last_error);
        }

        let signaled = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, 0) };
        let signal_error = if signaled == 0 {
            Some(io::Error::last_os_error())
        } else {
            None
        };

        thread::sleep(Duration::from_millis(50));

        unsafe {
            FreeConsole();
            SetConsoleCtrlHandler(None, 0);
            SetConsoleCtrlHandler(Some(swallow_ctrl), 0);
        }
        IGNORING_CTRL.store(false, Ordering::SeqCst);

        match signal_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn hide_console_windows(pid: u32) {
        let mut target = pid;
        unsafe {
            let _ = EnumWindows(Some(hide_if_pid), &raw mut target as *mut _ as isize);
        }
    }

    unsafe extern "system" fn hide_if_pid(hwnd: HWND, lparam: isize) -> i32 {
        let target = unsafe { *(lparam as *mut u32) };
        let mut window_pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, &raw mut window_pid);
            if window_pid == target {
                ShowWindow(hwnd, SW_HIDE);
            }
        }
        TRUE
    }
}

#[cfg(unix)]
mod unix {
    use std::io;

    pub fn send_sigterm(pid: u32) -> io::Result<()> {
        let Ok(pid) = i32::try_from(pid) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"));
        };
        let result = unsafe { libc::kill(pid, libc::SIGTERM) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
