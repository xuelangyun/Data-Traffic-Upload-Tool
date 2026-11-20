//! Pure in-memory application state, atomic metrics, ring-buffer logs, and target defaults.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::sys_stats::format_local_hms;

pub const DEFAULT_TCP_TARGETS: &str = "\
223.5.5.5:53, 223.5.5.5:443, 223.6.6.6:53, 223.6.6.6:443, \
119.29.29.29:53, 119.29.29.29:443, 180.76.76.76:53, 180.76.76.76:443, \
114.114.114.114:53, 114.114.115.115:53, 180.184.1.1:53, \
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

pub struct SharedStats {
    mode: AtomicU8,

    // Connection tester counters
    pub conn_current: AtomicUsize,
    pub conn_peak: AtomicUsize,
    pub conn_stable_max: AtomicUsize,
    pub conn_target_probe: AtomicUsize,
    pub conn_attempts: AtomicU64,
    pub conn_established_total: AtomicU64,
    pub conn_per_sec: AtomicU64,
    pub conn_rtt_ms: AtomicU64,
    pub conn_remote_closed: AtomicU64,
    pub conn_fail_timeout: AtomicU64,
    pub conn_fail_refused: AtomicU64,
    pub conn_fail_resource: AtomicU64,
    pub conn_fail_other: AtomicU64,
    pub conn_holding: AtomicBool,

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
            conn_target_probe: AtomicUsize::new(0),
            conn_attempts: AtomicU64::new(0),
            conn_established_total: AtomicU64::new(0),
            conn_per_sec: AtomicU64::new(0),
            conn_rtt_ms: AtomicU64::new(0),
            conn_remote_closed: AtomicU64::new(0),
            conn_fail_timeout: AtomicU64::new(0),
            conn_fail_refused: AtomicU64::new(0),
            conn_fail_resource: AtomicU64::new(0),
            conn_fail_other: AtomicU64::new(0),
            conn_holding: AtomicBool::new(false),
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
        self.conn_target_probe.store(0, Ordering::Relaxed);
        self.conn_attempts.store(0, Ordering::Relaxed);
        self.conn_established_total.store(0, Ordering::Relaxed);
        self.conn_per_sec.store(0, Ordering::Relaxed);
        self.conn_rtt_ms.store(0, Ordering::Relaxed);
        self.conn_remote_closed.store(0, Ordering::Relaxed);
        self.conn_fail_timeout.store(0, Ordering::Relaxed);
        self.conn_fail_refused.store(0, Ordering::Relaxed);
        self.conn_fail_resource.store(0, Ordering::Relaxed);
        self.conn_fail_other.store(0, Ordering::Relaxed);
        self.conn_holding.store(false, Ordering::Relaxed);
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
        log.push("提示：仅用于测试本机出口能力或明确授权目标，请勿滥用公网压测。");
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
        assert_eq!(format_bytes(1536), "1.50 KB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10.00 MB");
    }
}
