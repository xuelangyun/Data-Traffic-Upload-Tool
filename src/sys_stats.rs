//! Windows TCP/process telemetry. Network-adapter byte counters are intentionally not collected.

#[derive(Debug, Clone, Default)]
pub struct SysSnapshot {
    pub tcp_established: u32,
    pub process_handles: u32,
    pub tcp_retrans_pct_x100: u32,
}

/// Return the sampled IPv4 TCP retransmission percentage multiplied by 100.
/// For example, 1.25% is represented as 125.
fn retransmission_pct_x100(delta_out_segments: u64, delta_retrans_segments: u64) -> u32 {
    if delta_out_segments == 0 {
        0
    } else {
        ((delta_retrans_segments as f64 * 10_000.0 / delta_out_segments as f64)
            .round()
            .clamp(0.0, u32::MAX as f64)) as u32
    }
}

#[cfg(windows)]
mod win_imp {
    use super::{retransmission_pct_x100, SysSnapshot};
    use std::sync::Mutex;
    use std::time::Instant;
    use windows::Win32::iphlpapi::GetTcpStatistics;
    use windows::Win32::processthreadsapi::{GetCurrentProcess, GetProcessHandleCount};
    use windows::Win32::tcpmib::MIB_TCPSTATS_LH;

    struct TcpCounterState {
        last_sample: Instant,
        out_segments: u32,
        retrans_segments: u32,
    }

    static TCP_COUNTER_STATE: Mutex<Option<TcpCounterState>> = Mutex::new(None);

    pub fn query_sys_snapshot() -> SysSnapshot {
        let mut tcp_stats = MIB_TCPSTATS_LH::default();
        let tcp_ok = unsafe { GetTcpStatistics(&mut tcp_stats) == 0 };
        let tcp_established = if tcp_ok { tcp_stats.dwCurrEstab } else { 0 };

        let mut process_handles = 0u32;
        unsafe {
            let process = GetCurrentProcess();
            let _ = GetProcessHandleCount(process, &mut process_handles);
        }

        let tcp_retrans_pct_x100 = if tcp_ok {
            sample_tcp_retransmission_pct(tcp_stats.dwOutSegs, tcp_stats.dwRetransSegs)
        } else {
            0
        };

        SysSnapshot {
            tcp_established,
            process_handles,
            tcp_retrans_pct_x100,
        }
    }

    fn sample_tcp_retransmission_pct(out_segments: u32, retrans_segments: u32) -> u32 {
        let now = Instant::now();
        let mut guard = TCP_COUNTER_STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let result = if let Some(previous) = guard.as_ref() {
            let elapsed = now.duration_since(previous.last_sample);
            if elapsed >= std::time::Duration::from_millis(200) {
                let delta_out = out_segments.wrapping_sub(previous.out_segments) as u64;
                let delta_retrans =
                    retrans_segments.wrapping_sub(previous.retrans_segments) as u64;
                retransmission_pct_x100(delta_out, delta_retrans)
            } else {
                0
            }
        } else {
            0
        };

        *guard = Some(TcpCounterState {
            last_sample: now,
            out_segments,
            retrans_segments,
        });
        result
    }
}

#[cfg(windows)]
pub use win_imp::query_sys_snapshot;

#[cfg(test)]
mod tests {
    use super::retransmission_pct_x100;

    #[test]
    fn retransmission_rate_uses_interval_deltas() {
        assert_eq!(retransmission_pct_x100(10_000, 125), 125);
        assert_eq!(retransmission_pct_x100(0, 100), 0);
        assert_eq!(retransmission_pct_x100(100, 0), 0);
    }
}

#[cfg(not(windows))]
pub fn query_sys_snapshot() -> SysSnapshot {
    SysSnapshot::default()
}

/// Format current local time as `HH:MM:SS` without touching filesystem / tz files.
#[cfg(windows)]
pub fn format_local_hms() -> String {
    use windows::Win32::sysinfoapi::GetLocalTime;
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
