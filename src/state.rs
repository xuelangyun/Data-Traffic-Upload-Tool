//! Pure in-memory application state, atomic metrics, per-target WAN/speed stats,
//! ring-buffer logs, speed history for charts, and target defaults.

use std::collections::VecDeque;
use std::net::SocketAddr;
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
https://test.nju.edu.cn/backend/empty.php, \
http://speed.neu.edu.cn/empty.php, \
https://iptv.tsinghua.edu.cn/st/empty.php, \
https://speed.nuaa.edu.cn/backend/empty.php, \
https://speed.cloudflare.com/__up";

pub const DEFAULT_STREAM_CONCURRENCY: usize = 16;
pub const DEFAULT_MAX_CONNS_CAP: usize = 30_000;
pub const DEFAULT_HOLD_SECS: u64 = 3;
pub const DEFAULT_TEST_DURATION_SECS: u64 = 0;
pub const DEFAULT_WARMUP_SECS: u64 = 5;
pub const DEFAULT_HTTP_POST_BYTES: usize = 4 * 1024 * 1024;
pub const CONCURRENCY_TIERS: [usize; 7] = [1, 2, 4, 8, 16, 32, 64];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AppMode {
    Idle = 0,
    TestingConn = 1,
    Uploading = 2,
    Downloading = 3,
    DuplexTesting = 4,
}

impl AppMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::TestingConn,
            2 => Self::Uploading,
            3 => Self::Downloading,
            4 => Self::DuplexTesting,
            _ => Self::Idle,
        }
    }

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Idle => "空闲 (就绪)",
            Self::TestingConn => "并发 TCP 连接数持续探测中...",
            Self::Uploading => "上行带宽压测中...",
            Self::Downloading => "下行带宽压测中...",
            Self::DuplexTesting => "双向并发带宽压测中...",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TestConfig {
    pub stream_concurrency: usize,
    pub up_concurrency: usize,
    pub down_concurrency: usize,
    pub auto_concurrency: bool,
    pub duration_secs: u64,
    pub warmup_secs: u64,
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
            up_concurrency: DEFAULT_STREAM_CONCURRENCY,
            down_concurrency: DEFAULT_STREAM_CONCURRENCY,
            auto_concurrency: true,
            duration_secs: DEFAULT_TEST_DURATION_SECS,
            warmup_secs: DEFAULT_WARMUP_SECS,
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

/// Per-target metrics for multi-target TCP connection testing.
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

/// Per-IP endpoint stats under a bandwidth target (for multi-IP / multi-WAN dispersion).
pub struct IpEndpointMetrics {
    pub addr: SocketAddr,
    pub active_conns: AtomicUsize,
    pub total_conns: AtomicU64,
    pub bytes_transferred: AtomicU64,
    pub current_bps: AtomicU64,
    pub errors: AtomicU64,
}

impl IpEndpointMetrics {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            active_conns: AtomicUsize::new(0),
            total_conns: AtomicU64::new(0),
            bytes_transferred: AtomicU64::new(0),
            current_bps: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }
}

/// Per-node bandwidth metrics for upload and download ranking, dynamic weighting,
/// timing breakdown, and per-IP tracking.
pub struct NodeSpeedMetrics {
    pub host: String,
    pub url: String,
    pub probe_bps: AtomicU64,
    pub ema_bps: AtomicU64,
    pub bytes_http_accepted: AtomicU64,
    pub requests_ok: AtomicU64,
    pub requests_fail: AtomicU64,
    pub consecutive_fails: AtomicU32,
    pub latency_sum_ms: AtomicU64,
    pub latency_cnt: AtomicU64,
    pub tcp_connect_ms: AtomicU64,
    pub tls_handshake_ms: AtomicU64,
    pub ttfb_ms: AtomicU64,
    pub transfer_ms_total: AtomicU64,
    pub active_workers: AtomicUsize,
    pub current_post_bytes: AtomicUsize,
    pub range_supported: AtomicBool,
    pub quarantined: AtomicBool,
    pub weight: AtomicU32,
    pub ip_cursor: AtomicUsize,
    pub ip_metrics: Vec<Arc<IpEndpointMetrics>>,
}

impl NodeSpeedMetrics {
    pub fn new(
        host: String,
        url: String,
        probe_bps: u64,
        tcp_ms: u64,
        tls_ms: u64,
        ttfb_ms: u64,
        range_supported: bool,
        addrs: &[SocketAddr],
    ) -> Self {
        let ip_metrics = addrs
            .iter()
            .copied()
            .map(|a| Arc::new(IpEndpointMetrics::new(a)))
            .collect();
        Self {
            host,
            url,
            probe_bps: AtomicU64::new(probe_bps),
            ema_bps: AtomicU64::new(probe_bps),
            bytes_http_accepted: AtomicU64::new(0),
            requests_ok: AtomicU64::new(0),
            requests_fail: AtomicU64::new(0),
            consecutive_fails: AtomicU32::new(0),
            latency_sum_ms: AtomicU64::new(0),
            latency_cnt: AtomicU64::new(0),
            tcp_connect_ms: AtomicU64::new(tcp_ms),
            tls_handshake_ms: AtomicU64::new(tls_ms),
            ttfb_ms: AtomicU64::new(ttfb_ms),
            transfer_ms_total: AtomicU64::new(0),
            active_workers: AtomicUsize::new(0),
            current_post_bytes: AtomicUsize::new(DEFAULT_HTTP_POST_BYTES),
            range_supported: AtomicBool::new(range_supported),
            quarantined: AtomicBool::new(false),
            weight: AtomicU32::new(10),
            ip_cursor: AtomicUsize::new(0),
            ip_metrics,
        }
    }

    pub fn record_success(&self, http_accepted_bytes: u64, elapsed_ms: u64) {
        self.bytes_http_accepted
            .fetch_add(http_accepted_bytes, Ordering::Relaxed);
        self.requests_ok.fetch_add(1, Ordering::Relaxed);
        self.consecutive_fails.store(0, Ordering::Relaxed);
        self.latency_sum_ms
            .fetch_add(elapsed_ms.max(1), Ordering::Relaxed);
        self.latency_cnt.fetch_add(1, Ordering::Relaxed);

        let bps = ((http_accepted_bytes as f64) * 8.0 * 1000.0 / (elapsed_ms.max(1) as f64)) as u64;
        let prev = self.ema_bps.load(Ordering::Relaxed);
        let next = if prev == 0 {
            bps
        } else {
            (prev * 6 + bps * 4) / 10
        };
        self.ema_bps.store(next, Ordering::Relaxed);
    }

    pub fn record_failure(&self) {
        self.requests_fail.fetch_add(1, Ordering::Relaxed);
        let streak = self.consecutive_fails.fetch_add(1, Ordering::Relaxed) + 1;
        if streak >= 6 {
            self.quarantined.store(true, Ordering::Relaxed);
        }
    }

    pub fn avg_latency_ms(&self) -> u64 {
        let cnt = self.latency_cnt.load(Ordering::Relaxed);
        if cnt == 0 {
            self.ttfb_ms.load(Ordering::Relaxed)
        } else {
            self.latency_sum_ms.load(Ordering::Relaxed) / cnt
        }
    }

    pub fn success_rate_pct(&self) -> f64 {
        let ok = self.requests_ok.load(Ordering::Relaxed);
        let fail = self.requests_fail.load(Ordering::Relaxed);
        let total = ok + fail;
        if total == 0 {
            100.0
        } else {
            (ok as f64) * 100.0 / (total as f64)
        }
    }

    /// Least-active-connections with atomic round-robin tie-breaker across resolved IPs.
    pub fn pick_ip(&self) -> Option<Arc<IpEndpointMetrics>> {
        if self.ip_metrics.is_empty() {
            return None;
        }
        let n = self.ip_metrics.len();
        let start = self.ip_cursor.fetch_add(1, Ordering::Relaxed) % n;
        let mut best_idx = start;
        let mut best_active = self.ip_metrics[start].active_conns.load(Ordering::Relaxed);
        for offset in 1..n {
            let idx = (start + offset) % n;
            let act = self.ip_metrics[idx].active_conns.load(Ordering::Relaxed);
            if act < best_active {
                best_active = act;
                best_idx = idx;
            }
        }
        Some(self.ip_metrics[best_idx].clone())
    }
}

#[derive(Debug, Clone)]
pub struct TargetDistributionRow {
    pub label: String,
    pub resolved: String,
    pub active: usize,
    pub peak: usize,
    pub pct: f64,
    pub rtt: u64,
    pub share: f64,
}

/// Rolling speed sample for Win32 GDI charts (500ms per point, up to 120 points = 60s).
#[derive(Debug, Clone, Copy, Default)]
pub struct SpeedSamplePoint {
    pub up_bps: u64,
    pub down_bps: u64,
    pub nic_up_bps: u64,
    pub nic_down_bps: u64,
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

    pub conn_orig_attempts: AtomicU64,
    pub conn_orig_success: AtomicU64,
    pub conn_replenish_attempts: AtomicU64,
    pub conn_replenish_success: AtomicU64,

    pub conn_per_sec: AtomicU64,
    pub conn_cps_peak: AtomicU64,
    pub conn_rtt_ms: AtomicU64,

    pub conn_fail_local: AtomicU64,
    pub conn_fail_refused: AtomicU64,
    pub conn_remote_closed: AtomicU64,
    pub conn_fail_timeout: AtomicU64,

    pub conn_holding: AtomicBool,
    pub conn_targets_total: AtomicUsize,
    pub conn_targets_healthy: AtomicUsize,
    pub target_metrics: Mutex<Vec<Arc<TargetMetrics>>>,

    // Upload accounting: body bytes yielded to the HTTP client and complete 2xx request bytes.
    // HTTP success is not a byte-level server receipt acknowledgement.
    pub up_bytes_total: AtomicU64,
    pub up_bytes_body_yielded: AtomicU64,
    pub up_active_streams: AtomicUsize,
    pub up_current_concurrency: AtomicUsize,
    pub up_best_concurrency: AtomicUsize,
    pub up_bps_current: AtomicU64,
    pub up_bps_avg: AtomicU64,
    pub up_bps_peak: AtomicU64,
    pub up_bps_p95: AtomicU64,
    pub up_requests_ok: AtomicU64,
    pub up_requests_fail: AtomicU64,
    pub up_errors: AtomicU64,
    pub up_latency_ms: AtomicU64,
    pub up_node_metrics: Mutex<Vec<Arc<NodeSpeedMetrics>>>,

    // Downloader counters & timing breakdown
    pub down_bytes_total: AtomicU64,
    pub down_active_streams: AtomicUsize,
    pub down_current_concurrency: AtomicUsize,
    pub down_best_concurrency: AtomicUsize,
    pub down_bps_current: AtomicU64,
    pub down_bps_avg: AtomicU64,
    pub down_bps_peak: AtomicU64,
    pub down_bps_p95: AtomicU64,
    pub down_requests_ok: AtomicU64,
    pub down_requests_fail: AtomicU64,
    pub down_errors: AtomicU64,
    pub down_tcp_ms: AtomicU64,
    pub down_tls_ms: AtomicU64,
    pub down_ttfb_ms: AtomicU64,
    pub down_transfer_ms: AtomicU64,
    pub down_node_metrics: Mutex<Vec<Arc<NodeSpeedMetrics>>>,

    // Windows OS & NIC telemetry
    pub sys_tcp_estab: AtomicU32,
    pub proc_handles: AtomicU32,
    pub tcp_retrans_pct_x100: AtomicU32,
    pub nic_up_bps: AtomicU64,
    pub nic_down_bps: AtomicU64,
    pub nic_tx_total: AtomicU64,
    pub nic_rx_total: AtomicU64,
    pub nic_summary: Mutex<String>,

    // Chart history (up to 120 points = 60s at 500ms)
    pub speed_history: Mutex<VecDeque<SpeedSamplePoint>>,

    // Public egress IP observations grouped by probe destination; not WAN identities.
    pub egress_ip_observations: Mutex<String>,
    pub egress_ip_last_update_ms: AtomicU64,
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
            up_bytes_body_yielded: AtomicU64::new(0),
            up_active_streams: AtomicUsize::new(0),
            up_current_concurrency: AtomicUsize::new(0),
            up_best_concurrency: AtomicUsize::new(0),
            up_bps_current: AtomicU64::new(0),
            up_bps_avg: AtomicU64::new(0),
            up_bps_peak: AtomicU64::new(0),
            up_bps_p95: AtomicU64::new(0),
            up_requests_ok: AtomicU64::new(0),
            up_requests_fail: AtomicU64::new(0),
            up_errors: AtomicU64::new(0),
            up_latency_ms: AtomicU64::new(0),
            up_node_metrics: Mutex::new(Vec::new()),
            down_bytes_total: AtomicU64::new(0),
            down_active_streams: AtomicUsize::new(0),
            down_current_concurrency: AtomicUsize::new(0),
            down_best_concurrency: AtomicUsize::new(0),
            down_bps_current: AtomicU64::new(0),
            down_bps_avg: AtomicU64::new(0),
            down_bps_peak: AtomicU64::new(0),
            down_bps_p95: AtomicU64::new(0),
            down_requests_ok: AtomicU64::new(0),
            down_requests_fail: AtomicU64::new(0),
            down_errors: AtomicU64::new(0),
            down_tcp_ms: AtomicU64::new(0),
            down_tls_ms: AtomicU64::new(0),
            down_ttfb_ms: AtomicU64::new(0),
            down_transfer_ms: AtomicU64::new(0),
            down_node_metrics: Mutex::new(Vec::new()),
            sys_tcp_estab: AtomicU32::new(0),
            proc_handles: AtomicU32::new(0),
            tcp_retrans_pct_x100: AtomicU32::new(0),
            nic_up_bps: AtomicU64::new(0),
            nic_down_bps: AtomicU64::new(0),
            nic_tx_total: AtomicU64::new(0),
            nic_rx_total: AtomicU64::new(0),
            nic_summary: Mutex::new("网卡检测中...".to_string()),
            speed_history: Mutex::new(VecDeque::with_capacity(120)),
            egress_ip_observations: Mutex::new("等待探测".to_string()),
            egress_ip_last_update_ms: AtomicU64::new(0),
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
        self.up_bytes_body_yielded.store(0, Ordering::Relaxed);
        self.up_active_streams.store(0, Ordering::Relaxed);
        self.up_current_concurrency.store(0, Ordering::Relaxed);
        self.up_best_concurrency.store(0, Ordering::Relaxed);
        self.up_bps_current.store(0, Ordering::Relaxed);
        self.up_bps_avg.store(0, Ordering::Relaxed);
        self.up_bps_peak.store(0, Ordering::Relaxed);
        self.up_bps_p95.store(0, Ordering::Relaxed);
        self.up_requests_ok.store(0, Ordering::Relaxed);
        self.up_requests_fail.store(0, Ordering::Relaxed);
        self.up_errors.store(0, Ordering::Relaxed);
        self.up_latency_ms.store(0, Ordering::Relaxed);
        self.up_node_metrics.lock().clear();
    }

    pub fn reset_download_run(&self) {
        self.down_bytes_total.store(0, Ordering::Relaxed);
        self.down_active_streams.store(0, Ordering::Relaxed);
        self.down_current_concurrency.store(0, Ordering::Relaxed);
        self.down_best_concurrency.store(0, Ordering::Relaxed);
        self.down_bps_current.store(0, Ordering::Relaxed);
        self.down_bps_avg.store(0, Ordering::Relaxed);
        self.down_bps_peak.store(0, Ordering::Relaxed);
        self.down_bps_p95.store(0, Ordering::Relaxed);
        self.down_requests_ok.store(0, Ordering::Relaxed);
        self.down_requests_fail.store(0, Ordering::Relaxed);
        self.down_errors.store(0, Ordering::Relaxed);
        self.down_tcp_ms.store(0, Ordering::Relaxed);
        self.down_tls_ms.store(0, Ordering::Relaxed);
        self.down_ttfb_ms.store(0, Ordering::Relaxed);
        self.down_transfer_ms.store(0, Ordering::Relaxed);
        self.down_node_metrics.lock().clear();
    }

    pub fn reset_speed_history(&self) {
        self.speed_history.lock().clear();
    }

    pub fn push_speed_sample(&self, pt: SpeedSamplePoint) {
        let mut hist = self.speed_history.lock();
        if hist.len() >= 120 {
            hist.pop_front();
        }
        hist.push_back(pt);
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

    pub fn up_fail_rate_pct(&self) -> f64 {
        let ok = self.up_requests_ok.load(Ordering::Relaxed);
        let fail = self.up_requests_fail.load(Ordering::Relaxed);
        let tot = ok + fail;
        if tot == 0 {
            0.0
        } else {
            (fail as f64) * 100.0 / (tot as f64)
        }
    }

    pub fn down_fail_rate_pct(&self) -> f64 {
        let ok = self.down_requests_ok.load(Ordering::Relaxed);
        let fail = self.down_requests_fail.load(Ordering::Relaxed);
        let tot = ok + fail;
        if tot == 0 {
            0.0
        } else {
            (fail as f64) * 100.0 / (tot as f64)
        }
    }

    pub fn target_distribution_summary(&self) -> String {
        let guard = self.target_metrics.lock();
        let healthy: Vec<&Arc<TargetMetrics>> = guard.iter().filter(|t| t.healthy).collect();
        if healthy.is_empty() {
            return "等待健康检查...".to_string();
        }
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
        let mut parts = Vec::new();
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

    /// Remote-target rows sorted by current active connections, with current active share %
    pub fn target_distribution_rows(&self) -> Vec<TargetDistributionRow> {
        let guard = self.target_metrics.lock();
        let healthy: Vec<&Arc<TargetMetrics>> = guard.iter().filter(|t| t.healthy).collect();
        if healthy.is_empty() {
            return Vec::new();
        }
        let total_active: usize = healthy.iter().map(|t| t.active_conns.load(Ordering::Relaxed)).sum();
        let mut sorted = healthy;
        sorted.sort_by(|a,b| b.active_conns.load(Ordering::Relaxed).cmp(&a.active_conns.load(Ordering::Relaxed)));
        sorted.iter().take(8).map(|t| {
            let act = t.active_conns.load(Ordering::Relaxed);
            let peak = t.peak_conns.load(Ordering::Relaxed);
            let pct = t.orig_success_rate_pct();
            let rtt = t.avg_rtt_ms();
            let share = if total_active == 0 { 0.0 } else { (act as f64) * 100.0 / total_active as f64 };
            TargetDistributionRow {
                label: t.label.clone(),
                resolved: t.resolved_addr.clone(),
                active: act,
                peak,
                pct,
                rtt,
                share,
            }
        }).collect()
    }

    pub fn node_ranking_summary(nodes: &Mutex<Vec<Arc<NodeSpeedMetrics>>>) -> String {
        let guard = nodes.lock();
        if guard.is_empty() {
            return "-".to_string();
        }
        let mut sorted: Vec<&Arc<NodeSpeedMetrics>> = guard.iter().collect();
        sorted.sort_by(|a, b| {
            b.ema_bps
                .load(Ordering::Relaxed)
                .cmp(&a.ema_bps.load(Ordering::Relaxed))
        });
        let parts: Vec<String> = sorted
            .iter()
            .take(3)
            .map(|n| {
                let bps = n.ema_bps.load(Ordering::Relaxed);
                let flag = if n.quarantined.load(Ordering::Relaxed) {
                    "[隔离]"
                } else {
                    ""
                };
                format!("{flag}{}({})", n.host, format_mbps(bps))
            })
            .collect();
        parts.join(" · ")
    }
}

pub fn compute_p95(samples: &[u64]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let idx = ((sorted.len() * 95).div_ceil(100)).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

pub fn compute_median(samples: &[u64]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2
    } else {
        sorted[mid]
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

    #[test]
    fn test_compute_p95_and_median() {
        let vals: Vec<u64> = (1..=20).collect();
        assert_eq!(compute_p95(&vals), 19);
        assert_eq!(compute_median(&[10, 50, 20]), 20);
        assert_eq!(compute_median(&[10, 40, 20, 30]), 25);
    }
}
