//! Process metrics for the daemon and its services.
//!
//! Working set / RSS is reported as-is and must not be summed or labelled as exclusive
//! physical RAM — the OS overcommits and shares pages across processes.

use std::io;
use std::time::Duration;

/// Snapshot of one process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessMetrics {
    /// Working set (Windows) or RSS (Unix) in bytes — not exclusive physical RAM.
    pub working_set_bytes: u64,
    /// Cumulative kernel + user CPU time.
    pub cpu_time: Duration,
}

/// Metrics for the current process (the rom daemon).
pub fn current_process_metrics() -> io::Result<ProcessMetrics> {
    #[cfg(windows)]
    {
        windows::current_process_metrics()
    }
    #[cfg(unix)]
    {
        unix::current_process_metrics()
    }
}

/// Metrics for another process by PID.
pub fn process_metrics(pid: u32) -> io::Result<ProcessMetrics> {
    #[cfg(windows)]
    {
        windows::process_metrics(pid)
    }
    #[cfg(unix)]
    {
        unix::process_metrics(pid)
    }
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

#[cfg(windows)]
mod windows {
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

    use super::ProcessMetrics;

    pub fn current_process_metrics() -> io::Result<ProcessMetrics> {
        let handle = unsafe { GetCurrentProcess() };
        metrics_from_handle(handle)
    }

    pub fn process_metrics(pid: u32) -> io::Result<ProcessMetrics> {
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
        let ok = unsafe {
            GetProcessTimes(
                handle,
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        };
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
}

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io;
    use std::time::Duration;

    use super::ProcessMetrics;

    pub fn current_process_metrics() -> io::Result<ProcessMetrics> {
        process_metrics(std::process::id())
    }

    pub fn process_metrics(pid: u32) -> io::Result<ProcessMetrics> {
        #[cfg(target_os = "linux")]
        {
            linux_metrics(pid)
        }
        #[cfg(not(target_os = "linux"))]
        {
            macos_or_bsd_metrics(pid)
        }
    }

    #[cfg(target_os = "linux")]
    fn linux_metrics(pid: u32) -> io::Result<ProcessMetrics> {
        let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
        let mut rss_kb = None;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest
                    .split_whitespace()
                    .next()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "VmRSS missing value")
                    })?
                    .parse()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                rss_kb = Some(kb);
                break;
            }
        }
        let rss_kb =
            rss_kb.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "VmRSS not found"))?;

        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let cpu_time = parse_linux_cpu(&stat)?;
        Ok(ProcessMetrics {
            working_set_bytes: rss_kb.saturating_mul(1024),
            cpu_time,
        })
    }

    #[cfg(target_os = "linux")]
    fn parse_linux_cpu(stat: &str) -> io::Result<Duration> {
        // comm may contain spaces/parens; fields after the last ')' are numbered from 1 = state.
        let after_comm = stat
            .rsplit_once(')')
            .map(|(_, rest)| rest)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "stat missing comm"))?;
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        // utime=14, stime=15 in /proc/pid/stat → indices 11 and 12 after the ')'.
        let utime: u64 = fields
            .get(11)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "stat missing utime"))?
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let stime: u64 = fields
            .get(12)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "stat missing stime"))?
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let ticks = utime.saturating_add(stime);
        let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if ticks_per_sec <= 0 {
            return Err(io::Error::other("sysconf(_SC_CLK_TCK) failed"));
        }
        let nanos = ticks.saturating_mul(1_000_000_000) / ticks_per_sec as u64;
        Ok(Duration::from_nanos(nanos))
    }

    #[cfg(not(target_os = "linux"))]
    fn macos_or_bsd_metrics(pid: u32) -> io::Result<ProcessMetrics> {
        // Self: getrusage. Other PIDs: best-effort via kill(0) existence check + zeros for memory
        // is too weak; use libc::getrusage only for self, and for others read what macOS allows.
        if pid == std::process::id() {
            let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
            let ok = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
            if ok != 0 {
                return Err(io::Error::last_os_error());
            }
            let cpu = timeval_to_duration(usage.ru_utime) + timeval_to_duration(usage.ru_stime);
            // ru_maxrss is bytes on macOS and kilobytes on other BSDs.
            let rss = if cfg!(target_os = "macos") {
                usage.ru_maxrss as u64
            } else {
                (usage.ru_maxrss as u64).saturating_mul(1024)
            };
            return Ok(ProcessMetrics {
                working_set_bytes: rss,
                cpu_time: cpu,
            });
        }
        // Other processes: confirm alive, report CPU/RSS as unavailable → error so status shows "-".
        let result = unsafe { libc::kill(pid as i32, 0) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "per-pid RSS/CPU is only implemented for the current process on this OS",
        ))
    }

    #[cfg(not(target_os = "linux"))]
    fn timeval_to_duration(tv: libc::timeval) -> Duration {
        Duration::from_secs(tv.tv_sec as u64) + Duration::from_micros(tv.tv_usec as u64)
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
