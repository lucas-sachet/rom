//! Windows console helpers for graceful process shutdown.
//!
//! Each service is started with its own hidden console. Stopping re-invokes the
//! current binary (or `rom-ctrl-break` in tests) to attach and send `CTRL_BREAK`
//! (Node: `SIGBREAK`). Attaching in-process would also signal the daemon. If the
//! process ignores the signal, the Job Object force-kills after `shutdown_timeout`.

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

/// Windows status when a process is ended by Ctrl+C / Ctrl+Break.
const STATUS_CONTROL_C_EXIT: i32 = -1073741510; // 0xC000013A as i32

static IGNORING_CTRL: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn swallow_ctrl(_ctrl_type: u32) -> i32 {
    if IGNORING_CTRL.load(Ordering::SeqCst) {
        TRUE
    } else {
        0
    }
}

/// Internal argv token handled by `rom` and `rom-ctrl-break`.
pub const CTRL_BREAK_ARG: &str = "__ctrl_break";

/// If this process was launched as a ctrl-break helper, deliver the signal and exit.
/// Returns `true` when the helper path was taken.
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

/// Spawns a helper process to deliver `CTRL_BREAK` to `pid`'s console.
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

/// Attach to `pid`'s console and raise `CTRL_BREAK`. Used by the helper process.
pub fn deliver_ctrl_break(pid: u32) -> io::Result<()> {
    IGNORING_CTRL.store(true, Ordering::SeqCst);
    // SAFETY: swallow handler is a valid static callback for this process.
    if unsafe { SetConsoleCtrlHandler(Some(swallow_ctrl), TRUE) } == 0 {
        IGNORING_CTRL.store(false, Ordering::SeqCst);
        return Err(io::Error::last_os_error());
    }
    // Also ignore CTRL+C via the special NULL handler (does not cover BREAK).
    let _ = unsafe { SetConsoleCtrlHandler(None, TRUE) };

    // Detach from any existing console so AttachConsole can succeed.
    unsafe {
        FreeConsole();
    }

    let mut last_error = io::Error::other("AttachConsole did not run");
    let mut attached = false;
    for _ in 0..20 {
        // SAFETY: attaches this process to the target's console.
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

    // SAFETY: 0 = every process on this console (CREATE_NEW_CONSOLE drops
    // CREATE_NEW_PROCESS_GROUP, so a group id is not available).
    let signaled = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, 0) };
    let signal_error = if signaled == 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };

    // Give the target a moment to receive the event before we detach.
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

/// Best-effort hide of console windows owned by `pid` (avoids a visible flash).
pub fn hide_console_windows(pid: u32) {
    let mut target = pid;
    // SAFETY: EnumWindows callback receives each top-level HWND; we only hide
    // windows whose process id matches `pid`.
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
