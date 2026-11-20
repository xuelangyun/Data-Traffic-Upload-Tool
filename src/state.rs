//! Pure in-memory application state, atomic metrics, per-target WAN stats,
//! ring-buffer logs, and target defaults.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::sys_stats::format_local_hms;

pub const DEFAULT_TCP_TARGETS: &str = "\
223.5.5.5:443, 223.6.6.6:443, \
119.29.29.29:443, 180.76.76.76:443, \
1.12.12.12:443, 223.5.5.5:53, \
www.baidu.com:80, www.baidu.com:443, www.qq.com:80, www.qq.com:443, \
www.aliyun.com:443, www.taobao.com:443, www.163.com:80, www.jd.com:443, www.bilibili.com:443";

pub const DEFAULT_DOWNLOAD_TARGETS: &str = "\
https://test.nju.edu.cn/backend/garbage.php?ckSize=100\r\n\
http://speed.neu.edu.cn/fastgarbage.php?ckSize=100\r\n\
https://iptv.tsinghua.edu.cn/st/garbage.php?ckSize=100\r\n\
https://speed.nuaa.edu.cn/backend/garbage.php?ckSize=100\r\n\
https://mirrors.aliyun.com/archlinux/iso/latest/archlinux-x86_64.iso\r\n\
https://mirrors.cloud.tencent.com/archlinux/iso/latest/archlinux-x86_64.iso\r\n\
https://mirrors.bfsu.edu.cn/archlinux/iso/latest/archlinux-x86_64.iso\r\n\
https://mirrors.tuna.tsinghua.edu.cn/archlinux/iso/latest/archlinux-x86_64.iso\r\n\
https://mirrors.ustc.edu.cn/archlinux/iso/latest/archlinux-x86_64.iso";

pub const DEFAULT_UPLOAD_TARGETS: &str = "\
https://arms-retcode.aliyuncs.com/r.png, \
https://snowflake.qq.com/, \
https://gatherer.m.qq.com/, \
https://galileotelemetry.tencent.com/collect, \
https://test.nju.edu.cn/backend/empty.php, \
http://speed.neu.edu.cn/empty.php, \
https://iptv.tsinghua.edu.cn/st/empty.php, \
https://speed.nuaa.edu.cn/backend/empty.php, \
https://speed.cloudflare.com/__up";

pub const DEFAULT_STREAM_CONCURRENCY: usize = 16;
pub const DEFAULT_MAX_CONNS_CAP: usize = 30_000;
pub const DEFAULT_HOLD_SECS: u64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AppMode {
    Idle = 0,
    TestingConn = 1,
    Uploading = 2,
    Downloading = 3,
}

impl AppMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::TestingConn,
            2 => Self::Uploading,
            3 => Self::Downloading,
            _ => Self::Idle,
        }
    }

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Idle => "空闲 (就绪)",
            Self::TestingConn => "并发 TCP 连接数持续探测中...",
            Self::Uploading => "持续上行带宽压测中...",
            Self::Downloading => "持续下行带宽压测中...",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TestConfig {
    pub stream_concurrency: usize,
    pub max_conn_cap: usize,
    pub hold_secs: u64,
    pub continuous_conn: bool,
    pub tcp_targets: Vec<String>,
    pub upload_targets: Vec<String>,
    pub download_targets: Vec<String>,
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            stream_concurrency: DEFAULT_STREAM_CONCURRENCY,
            max_conn_cap: DEFAULT_MAX_CONNS_CAP,
            hold_secs: DEFAULT_HOLD_SECS,
            continuous_conn: true,
            tcp_targets: parse_target_list(DEFAULT_TCP_TARGETS),
            upload_targets: parse_target_list(DEFAULT_UPLOAD_TARGETS),
            download_targets: parse_target_list(DEFAULT_DOWNLOAD_TARGETS),
        }
    }
}

pub fn parse_target_list(raw: &str) -> Vec<String> {
    raw.split([',', ';', '\n', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Per-target metrics for multi-target testing, health verification,
/// connection attribution, and multi-WAN distribution observation.
pub struct TargetMetrics {
    pub label: String,
    pub resolved_addr: String,
    pub healthy: bool,
    pub health_note: String,
    pub probe_rtt_ms: u64,

    pub active_conns: AtomicUsize,
    pub peak_conns: AtomicUsize,
    pub orig_attempts: AtomicU64,
    pub orig_success: AtomicU64,
    pub replenish_attempts: AtomicU64,
    pub replenish_success: AtomicU64,

    pub fail_local: AtomicU64,
    pub fail_refused: AtomicU64,
    pub remote_closed: AtomicU64,
    pub fail_timeout: AtomicU64,

    pub rtt_sum_ms: AtomicU64,
    pub rtt_cnt: AtomicU64,
}

impl TargetMetrics {
    pub fn healthy(label: String, resolved_addr: String, probe_rtt_ms: u64) -> Self {
        Self {
            label,
            resolved_addr,
            healthy: true,
            health_note: format!("TCP 可达 ({probe_rtt_ms} ms)"),
            probe_rtt_ms,
            active_conns: AtomicUsize::new(0),
            peak_conns: AtomicUsize::new(0),
            orig_attempts: AtomicU64::new(0),
            orig_success: AtomicU64::new(0),
            replenish_attempts: AtomicU64::new(0),
            replenish_success: AtomicU64::new(0),
            fail_local: AtomicU64::new(0),
            fail_refused: AtomicU64::new(0),
            remote_closed: AtomicU64::new(0),
            fail_timeout: AtomicU64::new(0),
            rtt_sum_ms: AtomicU64::new(0),
            rtt_cnt: AtomicU64::new(0),
        }
    }

    pub fn unhealthy(label: String, reason: String) -> Self {
        Self {
            label,
            resolved_addr: "-".to_string(),
            healthy: false,
            health_note: reason,
            probe_rtt_ms: 0,
            active_conns: AtomicUsize::new(0),
            peak_conns: AtomicUsize::new(0),
            orig_attempts: AtomicU64::new(0),
            orig_success: AtomicU64::new(0),
            replenish_attempts: AtomicU64::new(0),
            replenish_success: AtomicU64::new(0),
            fail_local: AtomicU64::new(0),
            fail_refused: AtomicU64::new(0),
            remote_closed: AtomicU64::new(0),
            fail_timeout: AtomicU64::new(0),
            rtt_sum_ms: AtomicU64::new(0),
            rtt_cnt: AtomicU64::new(0),
        }
    }

    pub fn update_peak(&self, cur: usize) {
        let mut prev = self.peak_conns.load(Ordering::Relaxed);
        while cur > prev {
            match self.peak_conns.compare_exchange_weak(
                prev,
                cur,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => prev = actual,
            }
        }
    }

    pub fn record_rtt(&self, ms: u64) {
        self.rtt_sum_ms.fetch_add(ms.max(1), Ordering::Relaxed);
        self.rtt_cnt.fetch_add(1, Ordering::Relaxed);
    }

    pub fn avg_rtt_ms(&self) -> u64 {
        let cnt = self.rtt_cnt.load(Ordering::Relaxed);
        if cnt == 0 {
            self.probe_rtt_ms
        } else {
            self.rtt_sum_ms.load(Ordering::Relaxed) / cnt
        }
    }

    pub fn orig_success_rate_pct(&self) -> f64 {
        let att = self.orig_attempts.load(Ordering::Relaxed);
        if att == 0 {
            0.0
        } else {
            (self.orig_success.load(Ordering::Relaxed) as f64) * 100.0 / (att as f64)
        }
    }
}

pub struct SharedStats {
    mode: AtomicU8,

    // Connection tester live & tiered counters
    pub conn_current: AtomicUsize,
    pub conn_peak: AtomicUsize,
    pub conn_stable_max: AtomicUsize,
    pub conn_long_hold_max: AtomicUsize,
    pub conn_hold_elapsed_secs: AtomicU64,
    pub conn_target_probe: AtomicUsize,

    // Strict separation: original attempts/success vs. replenishment attempts/success
    pub conn_orig_attempts: AtomicU64,
    pub conn_orig_success: AtomicU64,
    pub conn_replenish_attempts: AtomicU64,
    pub conn_replenish_success: AtomicU64,

    // Instant CPS & peak CPS + RTT
    pub conn_per_sec: AtomicU64,
    pub conn_cps_peak: AtomicU64,
    pub conn_rtt_ms: AtomicU64,

    // Connection failure/closure attribution (4 distinct buckets)
    pub conn_fail_local: AtomicU64,
    pub conn_fail_refused: AtomicU64,
    pub conn_remote_closed: AtomicU64,
    pub conn_fail_timeout: AtomicU64,

    pub conn_holding: AtomicBool,
    pub conn_targets_total: AtomicUsize,
    pub conn_targets_healthy: AtomicUsize,
    pub target_metrics: Mutex<Vec<Arc<TargetMetrics>>>,

    // Uploader counters
    pub up_bytes_total: AtomicU64,
    pub up_active_streams: AtomicUsize,
    pub up_bps_current: AtomicU64,
    pub up_bps_peak: AtomicU64,
    pub up_errors: AtomicU64,

    // Downloader counters
    pub down_bytes_total: AtomicU64,
    pub down_active_streams: AtomicUsize,
    pub down_bps_current: AtomicU64,
    pub down_bps_peak: AtomicU64,
    pub down_errors: AtomicU64,

    // Windows OS counters
    pub sys_tcp_estab: AtomicU32,
    pub proc_handles: AtomicU32,
}

impl Default for SharedStats {
    fn default() -> Self {
        Self {
            mode: AtomicU8::new(AppMode::Idle as u8),
            conn_current: AtomicUsize::new(0),
            conn_peak: AtomicUsize::new(0),
            conn_stable_max: AtomicUsize::new(0),
            conn_long_hold_max: AtomicUsize::new(0),
            conn_hold_elapsed_secs: AtomicU64::new(0),
            conn_target_probe: AtomicUsize::new(0),
            conn_orig_attempts: AtomicU64::new(0),
            conn_orig_success: AtomicU64::new(0),
            conn_replenish_attempts: AtomicU64::new(0),
            conn_replenish_success: AtomicU64::new(0),
            conn_per_sec: AtomicU64::new(0),
            conn_cps_peak: AtomicU64::new(0),
            conn_rtt_ms: AtomicU64::new(0),
            conn_fail_local: AtomicU64::new(0),
            conn_fail_refused: AtomicU64::new(0),
            conn_remote_closed: AtomicU64::new(0),
            conn_fail_timeout: AtomicU64::new(0),
            conn_holding: AtomicBool::new(false),
            conn_targets_total: AtomicUsize::new(0),
            conn_targets_healthy: AtomicUsize::new(0),
            target_metrics: Mutex::new(Vec::new()),
            up_bytes_total: AtomicU64::new(0),
            up_active_streams: AtomicUsize::new(0),
            up_bps_current: AtomicU64::new(0),
            up_bps_peak: AtomicU64::new(0),
            up_errors: AtomicU64::new(0),
            down_bytes_total: AtomicU64::new(0),
            down_active_streams: AtomicUsize::new(0),
            down_bps_current: AtomicU64::new(0),
            down_bps_peak: AtomicU64::new(0),
            down_errors: AtomicU64::new(0),
            sys_tcp_estab: AtomicU32::new(0),
            proc_handles: AtomicU32::new(0),
        }
    }
}

impl SharedStats {
    pub fn mode(&self) -> AppMode {
        AppMode::from_u8(self.mode.load(Ordering::Acquire))
    }

    pub fn set_mode(&self, mode: AppMode) {
        self.mode.store(mode as u8, Ordering::Release);
    }

    pub fn reset_conn_run(&self) {
        self.conn_current.store(0, Ordering::Relaxed);
        self.conn_peak.store(0, Ordering::Relaxed);
        self.conn_stable_max.store(0, Ordering::Relaxed);
        self.conn_long_hold_max.store(0, Ordering::Relaxed);
        self.conn_hold_elapsed_secs.store(0, Ordering::Relaxed);
        self.conn_target_probe.store(0, Ordering::Relaxed);
        self.conn_orig_attempts.store(0, Ordering::Relaxed);
        self.conn_orig_success.store(0, Ordering::Relaxed);
        self.conn_replenish_attempts.store(0, Ordering::Relaxed);
        self.conn_replenish_success.store(0, Ordering::Relaxed);
        self.conn_per_sec.store(0, Ordering::Relaxed);
        self.conn_cps_peak.store(0, Ordering::Relaxed);
        self.conn_rtt_ms.store(0, Ordering::Relaxed);
        self.conn_fail_local.store(0, Ordering::Relaxed);
        self.conn_fail_refused.store(0, Ordering::Relaxed);
        self.conn_remote_closed.store(0, Ordering::Relaxed);
        self.conn_fail_timeout.store(0, Ordering::Relaxed);
        self.conn_holding.store(false, Ordering::Relaxed);
        self.conn_targets_total.store(0, Ordering::Relaxed);
        self.conn_targets_healthy.store(0, Ordering::Relaxed);
        self.target_metrics.lock().clear();
    }

    pub fn reset_upload_run(&self) {
        self.up_bytes_total.store(0, Ordering::Relaxed);
        self.up_active_streams.store(0, Ordering::Relaxed);
        self.up_bps_current.store(0, Ordering::Relaxed);
        self.up_bps_peak.store(0, Ordering::Relaxed);
        self.up_errors.store(0, Ordering::Relaxed);
    }

    pub fn reset_download_run(&self) {
        self.down_bytes_total.store(0, Ordering::Relaxed);
        self.down_active_streams.store(0, Ordering::Relaxed);
        self.down_bps_current.store(0, Ordering::Relaxed);
        self.down_bps_peak.store(0, Ordering::Relaxed);
        self.down_errors.store(0, Ordering::Relaxed);
    }

    pub fn update_conn_peak(&self, current: usize) {
        let mut prev = self.conn_peak.load(Ordering::Relaxed);
        while current > prev {
            match self.conn_peak.compare_exchange_weak(
                prev,
                current,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => prev = actual,
            }
        }
    }

    pub fn update_cps_peak(&self, cps: u64) {
        let mut prev = self.conn_cps_peak.load(Ordering::Relaxed);
        while cps > prev {
            match self.conn_cps_peak.compare_exchange_weak(
                prev,
                cps,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => prev = actual,
            }
        }
    }

    pub fn update_up_peak(&self, bps: u64) {
        let mut prev = self.up_bps_peak.load(Ordering::Relaxed);
        while bps > prev {
            match self.up_bps_peak.compare_exchange_weak(
                prev,
                bps,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => prev = actual,
            }
        }
    }

    pub fn update_down_peak(&self, bps: u64) {
        let mut prev = self.down_bps_peak.load(Ordering::Relaxed);
        while bps > prev {
            match self.down_bps_peak.compare_exchange_weak(
                prev,
                bps,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => prev = actual,
            }
        }
    }

    pub fn orig_success_rate_pct(&self) -> f64 {
        let att = self.conn_orig_attempts.load(Ordering::Relaxed);
        if att == 0 {
            0.0
        } else {
            (self.conn_orig_success.load(Ordering::Relaxed) as f64) * 100.0 / (att as f64)
        }
    }

    /// Concise summary of live connection dispersion across top healthy WAN targets.
    pub fn wan_distribution_summary(&self) -> String {
        let guard = self.target_metrics.lock();
        let healthy: Vec<&Arc<TargetMetrics>> = guard.iter().filter(|t| t.healthy).collect();
        if healthy.is_empty() {
            return "等待健康检查...".to_string();
        }
        let mut parts: Vec<String> = Vec::new();
        let mut sorted = healthy;
        sorted.sort_by(|a, b| {
            b.active_conns
                .load(Ordering::Relaxed)
                .cmp(&a.active_conns.load(Ordering::Relaxed))
                .then_with(|| {
                    b.peak_conns
                        .load(Ordering::Relaxed)
                        .cmp(&a.peak_conns.load(Ordering::Relaxed))
                })
        });
        for t in sorted.iter().take(4) {
            let act = t.active_conns.load(Ordering::Relaxed);
            let rtt = t.avg_rtt_ms();
            let pct = t.orig_success_rate_pct();
            parts.push(format!("{}({act}条/{pct:.0}%/{rtt}ms)", t.label));
        }
        if sorted.len() > 4 {
            parts.push(format!("等{}个节点", sorted.len()));
        }
        parts.join(" · ")
    }
}

pub struct MemoryLog {
    max_lines: usize,
    next_seq: AtomicU64,
    generation: AtomicU64,
    lines: Mutex<VecDeque<(u64, String)>>,
}

impl MemoryLog {
    pub fn new(max_lines: usize) -> Self {
        Self {
            max_lines: max_lines.max(32),
            next_seq: AtomicU64::new(1),
            generation: AtomicU64::new(1),
            lines: Mutex::new(VecDeque::with_capacity(max_lines.max(32))),
        }
    }

    pub fn push(&self, msg: impl AsRef<str>) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let ts = format_local_hms();
        let entry = format!("[{ts}] {}", msg.as_ref());
        let mut guard = self.lines.lock();
        if guard.len() >= self.max_lines {
            guard.pop_front();
        }
        guard.push_back((seq, entry));
    }

    pub fn clear(&self) {
        let mut guard = self.lines.lock();
        guard.clear();
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Returns `(current_generation, latest_seq, lines_to_append, full_reset_needed)`.
    pub fn fetch_since(&self, seen_gen: u64, seen_seq: u64) -> (u64, u64, Vec<String>, bool) {
        let cur_gen = self.generation.load(Ordering::Acquire);
        let guard = self.lines.lock();
        let latest_seq = guard.back().map(|(s, _)| *s).unwrap_or(seen_seq);

        if cur_gen != seen_gen {
            let all = guard.iter().map(|(_, s)| s.clone()).collect();
            return (cur_gen, latest_seq, all, true);
        }

        if let Some((first_seq, _)) = guard.front()
            && seen_seq > 0
            && *first_seq > seen_seq + 1
        {
            let all = guard.iter().map(|(_, s)| s.clone()).collect();
            return (cur_gen, latest_seq, all, true);
        }

        let delta = guard
            .iter()
            .filter(|(seq, _)| *seq > seen_seq)
            .map(|(_, s)| s.clone())
            .collect();
        (cur_gen, latest_seq, delta, false)
    }
}

pub struct AppState {
    pub stats: Arc<SharedStats>,
    pub log: Arc<MemoryLog>,
}

impl Default for AppState {
    fn default() -> Self {
        let log = Arc::new(MemoryLog::new(400));
        log.push("程序已启动（纯内存模式：无配置文件、无日志落盘、无临时文件）。");
        log.push("多 WAN 提示：单条 TCP 连接通常仅走单条出口，多目标并发连接才会分散至多条线路。");
        Self {
            stats: Arc::new(SharedStats::default()),
            log,
        }
    }
}

pub fn format_mbps(bps: u64) -> String {
    let mbps = (bps as f64) / 1_000_000.0;
    format!("{mbps:.2} Mbps")
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.2} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_target_list() {
        let parsed = parse_target_list(" 1.1.1.1:80, 223.5.5.5:443;\nhttps://example.com \r\n ");
        assert_eq!(
            parsed,
            vec!["1.1.1.1:80", "223.5.5.5:443", "https://example.com"]
        );
    }

    #[test]
    fn test_memory_log_ring_and_delta() {
        let log = MemoryLog::new(32);
        log.push("one");
        log.push("two");
        let (g1, s1, lines1, reset1) = log.fetch_since(0, 0);
        assert!(reset1);
        assert_eq!(lines1.len(), 2);

        log.push("three");
        let (g2, s2, lines2, reset2) = log.fetch_since(g1, s1);
        assert!(!reset2);
        assert_eq!(g2, g1);
        assert!(s2 > s1);
        assert_eq!(lines2.len(), 1);
        assert!(lines2[0].contains("three"));
    }

    #[test]
    fn test_format_helpers() {
        assert_eq!(format_mbps(125_000_000), "125.00 Mbps");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.00 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.00 MB");
    }

    #[test]
    fn test_orig_success_rate_not_masked_by_replenish() {
        let stats = SharedStats::default();
        stats.conn_orig_attempts.store(100, Ordering::Relaxed);
        stats.conn_orig_success.store(75, Ordering::Relaxed);
        stats.conn_replenish_attempts.store(50, Ordering::Relaxed);
        stats.conn_replenish_success.store(50, Ordering::Relaxed);
        assert!((stats.orig_success_rate_pct() - 75.0).abs() < 0.001);
    }
}
