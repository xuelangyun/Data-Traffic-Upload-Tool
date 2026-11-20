//! Windows OS & NIC telemetry via native Win32 API (no disk I/O).
//! Samples every 500–1000 ms via GetIfTable2 / MIB_IF_ROW2 and GetTcpStatistics.

#[derive(Debug, Clone, Default)]
pub struct SysSnapshot {
    pub tcp_established: u32,
    pub process_handles: u32,
    pub tcp_retrans_pct_x100: u32, // retrans *100 (e.g., 125 = 1.25%)
    pub nic_tx_bytes: u64,
    pub nic_rx_bytes: u64,
    pub nic_tx_bps: u64,
    pub nic_rx_bps: u64,
    pub nic_summary: String,
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
    use super::SysSnapshot;
    use std::sync::Mutex;
    use std::time::Instant;
    // In windows-rs 0.100.0 (tag 74) these live directly under Win32::iphlpapi / sysinfoapi
    use windows::Win32::iphlpapi::GetTcpStatistics;
    use windows::Win32::netioapi::{FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2};
    use windows::Win32::processthreadsapi::{GetCurrentProcess, GetProcessHandleCount};
    use windows::Win32::sysinfoapi::GetLocalTime;
    use windows::Win32::tcpmib::MIB_TCPSTATS_LH;

    struct TcpCounterState {
        last_sample: Instant,
        out_segments: u32,
        retrans_segments: u32,
    }

    static TCP_COUNTER_STATE: Mutex<Option<TcpCounterState>> = Mutex::new(None);

    struct NicState {
        last_sample: Instant,
        tx_bps_ema: f64,
        rx_bps_ema: f64,
        nic_tx_bytes: u64,
        nic_rx_bytes: u64,
        nic_tx_bps: u64,
        nic_rx_bps: u64,
        summary: String,
    }

    static NIC_STATE: Mutex<Option<NicState>> = Mutex::new(None);

    fn wide_to_string(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    pub fn query_sys_snapshot() -> SysSnapshot {
        let mut tcp_stats = MIB_TCPSTATS_LH::default();
        let tcp_ok = unsafe { GetTcpStatistics(&mut tcp_stats) == 0 };
        let tcp_estab = if tcp_ok { tcp_stats.dwCurrEstab } else { 0 };

        let mut handles: u32 = 0;
        unsafe {
            let proc_handle = GetCurrentProcess();
            let _ = GetProcessHandleCount(proc_handle, &mut handles);
        }

        // GetTcpStatistics counters are cumulative. Calculate the ratio from the
        // delta between samples, not from the lifetime totals.
        let retrans_pct_x100 = if tcp_ok {
            sample_tcp_retransmission_pct(tcp_stats.dwOutSegs, tcp_stats.dwRetransSegs)
        } else {
            0
        };

        // Sample NICs via GetIfTable2
        let (tx_total, rx_total, summary, tx_bps, rx_bps) = sample_nics();

        SysSnapshot {
            tcp_established: tcp_estab,
            process_handles: handles,
            tcp_retrans_pct_x100: retrans_pct_x100,
            nic_tx_bytes: tx_total,
            nic_rx_bytes: rx_total,
            nic_tx_bps: tx_bps,
            nic_rx_bps: rx_bps,
            nic_summary: summary,
        }
    }

    fn sample_tcp_retransmission_pct(out_segments: u32, retrans_segments: u32) -> u32 {
        let now = Instant::now();
        let mut guard = TCP_COUNTER_STATE.lock().unwrap();
        let result = if let Some(previous) = guard.as_ref() {
            let elapsed = now.duration_since(previous.last_sample).as_secs_f64();
            if elapsed >= 0.2 {
                let delta_out = out_segments.wrapping_sub(previous.out_segments) as u64;
                let delta_retrans = retrans_segments.wrapping_sub(previous.retrans_segments) as u64;
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

    fn sample_nics() -> (u64, u64, String, u64, u64) {
        let mut table_ptr: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        let rc = unsafe { GetIfTable2(&mut table_ptr) };
        if rc.0 != 0 || table_ptr.is_null() {
            return (0, 0, "网卡检测中...".to_string(), 0, 0);
        }

        let table = unsafe { &*table_ptr };
        let num = table.NumEntries as usize;

        let mut tx_total: u64 = 0;
        let mut rx_total: u64 = 0;
        let mut rows: Vec<(String, u64, u64, u64)> = Vec::new(); // physical adapters only: (name, tx, rx, linkSpeed)
        let mut virtual_adapters = 0usize;

        for i in 0..num {
            let row: &MIB_IF_ROW2 = unsafe { &*table.Table.as_ptr().add(i) };
            let is_loopback = row.Type.0 == 24;
            let has_mac = row.PhysicalAddressLength > 0;
            let is_hardware = (row.InterfaceAndOperStatusFlags & 0x01) != 0;
            if is_loopback || !has_mac {
                continue;
            }
            if !is_hardware {
                virtual_adapters += 1;
                continue;
            }

            let tx = row.OutOctets;
            let rx = row.InOctets;
            let link = row.TransmitLinkSpeed.max(row.ReceiveLinkSpeed);
            let alias = wide_to_string(&row.Alias);
            let name = if alias.is_empty() {
                wide_to_string(&row.Description)
            } else {
                alias
            };

            tx_total = tx_total.wrapping_add(tx);
            rx_total = rx_total.wrapping_add(rx);
            rows.push((name, tx, rx, link));
        }

        unsafe { FreeMibTable(table_ptr as *const _) };

        if rows.is_empty() {
            return (tx_total, rx_total, "未检测到物理网卡接口".to_string(), 0, 0);
        }

        // Compute BPS via delta with EMA smoothing
        let now = Instant::now();
        let mut guard = NIC_STATE.lock().unwrap();
        let (tx_bps, rx_bps) = if let Some(st) = guard.as_mut() {
            let elapsed = now.duration_since(st.last_sample).as_secs_f64();
            if elapsed >= 0.2 {
                let d_tx = tx_total.saturating_sub(st.nic_tx_bytes);
                let d_rx = rx_total.saturating_sub(st.nic_rx_bytes);
                let inst_tx = (d_tx as f64) * 8.0 / elapsed;
                let inst_rx = (d_rx as f64) * 8.0 / elapsed;
                st.tx_bps_ema = if st.tx_bps_ema <= 0.0 {
                    inst_tx
                } else {
                    st.tx_bps_ema * 0.4 + inst_tx * 0.6
                };
                st.rx_bps_ema = if st.rx_bps_ema <= 0.0 {
                    inst_rx
                } else {
                    st.rx_bps_ema * 0.4 + inst_rx * 0.6
                };
                st.last_sample = now;
                st.nic_tx_bytes = tx_total;
                st.nic_rx_bytes = rx_total;
                (st.tx_bps_ema.round() as u64, st.rx_bps_ema.round() as u64)
            } else {
                (st.tx_bps_ema.round() as u64, st.rx_bps_ema.round() as u64)
            }
        } else {
            *guard = Some(NicState {
                last_sample: now,
                tx_bps_ema: 0.0,
                rx_bps_ema: 0.0,
                nic_tx_bytes: tx_total,
                nic_rx_bytes: rx_total,
                nic_tx_bps: 0,
                nic_rx_bps: 0,
                summary: String::new(),
            });
            (0, 0)
        };

        // Build human-readable summary
        rows.sort_by(|a, b| b.1.saturating_add(b.2).cmp(&a.1.saturating_add(&a.2)));
        let mut parts: Vec<String> = Vec::new();
        for (name, _tx, _rx, link) in rows.iter().take(3) {
            let link_str = if *link >= 1_000_000 {
                format!("{:.0}Mbps", (*link as f64) / 1_000_000.0)
            } else if *link > 0 {
                format!("{}bps", link)
            } else {
                "未知速率".to_string()
            };
            parts.push(format!("{name}({link_str})"));
        }
        if rows.len() > 3 {
            parts.push(format!("等{}块网卡", rows.len()));
        }
        let mut summary = format!("物理网卡汇总（不含虚拟接口）: {}", parts.join(" · "));
        if virtual_adapters > 0 {
            summary.push_str(&format!(" · 已排除 {virtual_adapters} 个虚拟/软件接口，避免重复计数"));
        }

        // Store updated BPS back
        if let Some(st) = guard.as_mut() {
            st.nic_tx_bps = tx_bps;
            st.nic_rx_bps = rx_bps;
            st.summary = summary.clone();
        }

        (tx_total, rx_total, summary, tx_bps, rx_bps)
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
    SysSnapshot {
        tcp_established: 0,
        process_handles: 0,
        tcp_retrans_pct_x100: 0,
        nic_tx_bytes: 0,
        nic_rx_bytes: 0,
        nic_tx_bps: 0,
        nic_rx_bps: 0,
        nic_summary: "非 Windows 环境".to_string(),
    }
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
