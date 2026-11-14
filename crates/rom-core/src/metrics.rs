//! Process metrics for the daemon and its services.
//!
//! Working set is reported as-is and must not be summed or labelled as exclusive
//! physical RAM — Windows overcommits and shares pages across processes.

use std::io;
use std::mem::{size_of, zeroed};
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// Snapshot of one Windows process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessMetrics {
    /// Working set size in bytes (not exclusive physical RAM).
    pub working_set_bytes: u64,
    /// Cumulative kernel + user CPU time.
    pub cpu_time: Duration,
}

/// Metrics for the current process (the rom daemon).
pub fn current_process_metrics() -> io::Result<ProcessMetrics> {
    // SAFETY: GetCurrentProcess returns a pseudo-handle that need not be closed.
    let handle = unsafe { GetCurrentProcess() };
    metrics_from_handle(handle)
}

/// Metrics for another process by PID.
pub fn process_metrics(pid: u32) -> io::Result<ProcessMetrics> {
    // SAFETY: OpenProcess with query-limited rights; handle closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let result = metrics_from_handle(handle);
    unsafe {
        CloseHandle(handle);
    }
    result
}

fn metrics_from_handle(handle: HANDLE) -> io::Result<ProcessMetrics> {
    // SAFETY: PROCESS_MEMORY_COUNTERS is plain data; size matches the API.
    let mut memory: PROCESS_MEMORY_COUNTERS = unsafe { zeroed() };
    memory.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let ok = unsafe {
        GetProcessMemoryInfo(
            handle,
            &raw mut memory,
            size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    // SAFETY: all FILETIME out-params are valid stack locals.
    let ok = unsafe { GetProcessTimes(handle, &raw mut creation, &raw mut exit, &raw mut kernel, &raw mut user) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(ProcessMetrics {
        working_set_bytes: memory.WorkingSetSize as u64,
        cpu_time: filetime_to_duration(kernel) + filetime_to_duration(user),
    })
}

fn filetime_to_duration(time: FILETIME) -> Duration {
    let ticks = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Duration::from_nanos(ticks.saturating_mul(100))
}

/// Formats a byte count for CLI display (binary units).
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.1}GiB", value / GIB)
    } else if value >= MIB {
        format!("{:.1}MiB", value / MIB)
    } else if value >= KIB {
        format!("{:.1}KiB", value / KIB)
    } else {
        format!("{bytes}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_reports_working_set() {
        let metrics = current_process_metrics().expect("current process metrics");
        assert!(metrics.working_set_bytes > 0);
    }

    #[test]
    fn format_bytes_uses_binary_units() {
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(2048), "2.0KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0MiB");
    }
}
