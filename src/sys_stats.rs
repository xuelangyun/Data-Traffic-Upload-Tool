//! System telemetry & local clock helpers using Win32 API (zero disk I/O).

#[derive(Debug, Clone, Copy, Default)]
pub struct SysSnapshot {
    pub tcp_established: u32,
    pub process_handles: u32,
}

#[cfg(windows)]
pub fn query_sys_snapshot() -> SysSnapshot {
    use windows::Win32::NetworkManagement::IpHelper::{GetTcpStatistics, MIB_TCPSTATS_LH};
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

    let mut tcp_stats = MIB_TCPSTATS_LH::default();
    let tcp_estab = unsafe {
        if GetTcpStatistics(&mut tcp_stats) == 0 {
            tcp_stats.dwCurrEstab
        } else {
            0
        }
    };

    let mut handles: u32 = 0;
    unsafe {
        let proc_handle = GetCurrentProcess();
        let _ = GetProcessHandleCount(proc_handle, &mut handles);
    }

    SysSnapshot {
        tcp_established: tcp_estab,
        process_handles: handles,
    }
}

#[cfg(not(windows))]
pub fn query_sys_snapshot() -> SysSnapshot {
    SysSnapshot::default()
}

/// Format current local time as `HH:MM:SS` without touching filesystem / tz files.
#[cfg(windows)]
pub fn format_local_hms() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
}

#[cfg(not(windows))]
pub fn format_local_hms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day_secs = secs % 86_400;
    let h = day_secs / 3_600;
    let m = (day_secs % 3_600) / 60;
    let s = day_secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}
