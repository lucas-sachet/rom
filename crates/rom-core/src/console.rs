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

/// Enables ANSI colors on stdout. Unix terminals already honor them. On Windows
/// this sets `ENABLE_VIRTUAL_TERMINAL_PROCESSING` and does nothing when stdout
/// is not a console.
pub fn enable_ansi_stdout() {
    #[cfg(windows)]
    windows::enable_virtual_terminal_processing();
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

    use windows_sys::Win32::Foundation::TRUE;
    use windows_sys::Win32::System::Console::{
        AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent,
        SetConsoleCtrlHandler,
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

    pub fn enable_virtual_terminal_processing() {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE,
            SetConsoleMode,
        };

        unsafe {
            let handle = GetStdHandle(STD_OUTPUT_HANDLE);
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return;
            }
            let mut mode = 0u32;
            if GetConsoleMode(handle, &mut mode) == 0 {
                return;
            }
            let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
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
        use std::os::windows::process::CommandExt;

        let (exe, prefix) = ctrl_break_command()?;
        let mut command = Command::new(&exe);
        for arg in &prefix {
            command.arg(arg);
        }
        // Avoid a visible console flash from the helper itself.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let status = command
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
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
}

#[cfg(unix)]
mod unix {
    use std::io;

    pub fn send_sigterm(pid: u32) -> io::Result<()> {
        let Ok(pid) = i32::try_from(pid) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pid out of range",
            ));
        };
        let result = unsafe { libc::kill(pid, libc::SIGTERM) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
