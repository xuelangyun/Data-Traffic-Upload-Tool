//! Background Tokio runtime & test lifecycle coordinator.
//! Initializes the Tokio thread pool lazily in the background after the Win32 UI paints
//! so application startup feels instantaneous. Supports duplex (up+down) simultaneous testing.

use std::sync::{Arc, OnceLock};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::runtime::Runtime;

use crate::conn_tester::{CancelToken, run_conn_test};
use crate::downloader::run_download_test;
use crate::state::{AppMode, AppState, SpeedSamplePoint, TestConfig};
use crate::sys_stats::query_sys_snapshot;
use crate::uploader::run_upload_test;

pub struct EngineHandle {
    runtime: Arc<OnceLock<Result<Runtime, String>>>,
    state: Arc<AppState>,
    active_cancel: Mutex<Option<Arc<CancelToken>>>,
}

impl EngineHandle {
    /// Returns immediately without spawning threads or touching Winsock/iphlpapi.
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            runtime: Arc::new(OnceLock::new()),
            state,
            active_cancel: Mutex::new(None),
        }
    }

    /// Spawns a background thread to warm up the Tokio runtime and OS telemetry sampler
    /// after the main window has rendered its first frame.
    pub fn warm_up(&self) {
        let rt_cell = self.runtime.clone();
        let state = self.state.clone();
        let _ = std::thread::Builder::new()
            .name("engine-init".to_string())
            .spawn(move || {
                let _ = init_runtime_once(&rt_cell, &state);
            });
    }

    fn runtime(&self) -> Option<&Runtime> {
        match init_runtime_once(&self.runtime, &self.state) {
            Ok(rt) => Some(rt),
            Err(err) => {
                self.state.log.push(err.clone());
                self.state.stats.set_mode(AppMode::Idle);
                None
            }
        }
    }

    pub fn start_conn_test(&self, config: TestConfig) -> bool {
        if !self.try_enter_mode(AppMode::TestingConn) {
            return false;
        }
        let Some(rt) = self.runtime() else {
            return false;
        };
        self.state.stats.reset_conn_run();
        self.state.stats.reset_speed_history();
        let cancel = self.install_cancel();
        let state = self.state.clone();

        Self::spawn_conn_rate_sampler(rt, &self.state, cancel.clone());
        Self::spawn_history_sampler(rt, &self.state, cancel.clone());
        rt.spawn(async move {
            run_conn_test(config, state.stats.clone(), state.log.clone(), cancel.clone()).await;
            cancel.cancel();
            state.stats.conn_per_sec.store(0, Ordering::Relaxed);
            state.stats.set_mode(AppMode::Idle);
        });
        true
    }

    pub fn start_upload_test(&self, config: TestConfig) -> bool {
        if !self.try_enter_mode(AppMode::Uploading) {
            return false;
        }
        let Some(rt) = self.runtime() else {
            return false;
        };
        self.state.stats.reset_upload_run();
        self.state.stats.reset_speed_history();
        let cancel = self.install_cancel();
        let state = self.state.clone();

        Self::spawn_rate_sampler(rt, &self.state, AppMode::Uploading, cancel.clone());
        Self::spawn_history_sampler(rt, &self.state, cancel.clone());
        rt.spawn(async move {
            run_upload_test(config, state.stats.clone(), state.log.clone(), cancel.clone()).await;
            cancel.cancel();
            state.stats.up_bps_current.store(0, Ordering::Relaxed);
            state.stats.set_mode(AppMode::Idle);
        });
        true
    }

    pub fn start_download_test(&self, config: TestConfig) -> bool {
        if !self.try_enter_mode(AppMode::Downloading) {
            return false;
        }
        let Some(rt) = self.runtime() else {
            return false;
        };
        self.state.stats.reset_download_run();
        self.state.stats.reset_speed_history();
        let cancel = self.install_cancel();
        let state = self.state.clone();

        Self::spawn_rate_sampler(rt, &self.state, AppMode::Downloading, cancel.clone());
        Self::spawn_history_sampler(rt, &self.state, cancel.clone());
        rt.spawn(async move {
            run_download_test(config, state.stats.clone(), state.log.clone(), cancel.clone()).await;
            cancel.cancel();
            state.stats.down_bps_current.store(0, Ordering::Relaxed);
            state.stats.set_mode(AppMode::Idle);
        });
        true
    }

    pub fn start_duplex_test(&self, config: TestConfig) -> bool {
        if !self.try_enter_mode(AppMode::DuplexTesting) {
            return false;
        }
        let Some(rt) = self.runtime() else {
            return false;
        };
        self.state.stats.reset_upload_run();
        self.state.stats.reset_download_run();
        self.state.stats.reset_speed_history();
        let cancel = self.install_cancel();
        let state = self.state.clone();

        // Independent concurrency samplers for up and down; they share the same cancel token
        // so a single Stop click cleans up both directions atomically.
        Self::spawn_rate_sampler(rt, &self.state, AppMode::Uploading, cancel.clone());
        Self::spawn_rate_sampler(rt, &self.state, AppMode::Downloading, cancel.clone());
        Self::spawn_history_sampler(rt, &self.state, cancel.clone());

        rt.spawn(async move {
            let cfg_up = config.clone();
            let cfg_down = config.clone();
            let stats_up = state.stats.clone();
            let stats_down = state.stats.clone();
            let log_up = state.log.clone();
            let log_down = state.log.clone();
            let cancel_up = cancel.clone();
            let cancel_down = cancel.clone();

            let up_handle = tokio::spawn(async move {
                run_upload_test(cfg_up, stats_up, log_up, cancel_up).await;
            });
            let down_handle = tokio::spawn(async move {
                run_download_test(cfg_down, stats_down, log_down, cancel_down).await;
            });

            // Both tasks respect the shared cancel token; joining waits for natural
            // completion or cancellation without double-moving JoinHandles.
            let _ = tokio::join!(up_handle, down_handle);

            // Ensure both counters are cleared after duplex
            state.stats.up_bps_current.store(0, Ordering::Relaxed);
            state.stats.down_bps_current.store(0, Ordering::Relaxed);
            state.stats.up_active_streams.store(0, Ordering::Relaxed);
            state.stats.down_active_streams.store(0, Ordering::Relaxed);
            state.stats.set_mode(AppMode::Idle);
        });
        true
    }

    pub fn stop(&self) {
        let guard = self.active_cancel.lock();
        if let Some(tok) = guard.as_ref()
            && !tok.is_cancelled()
        {
            self.state.log.push("收到停止指令，正在释放活动连接与任务...");
            tok.cancel();
        }
    }

    fn try_enter_mode(&self, next: AppMode) -> bool {
        if self.state.stats.mode() != AppMode::Idle {
            self.state.log.push("已有测试正在运行，请先点击“停止当前测试”。");
            return false;
        }
        self.state.stats.set_mode(next);
        true
    }

    fn install_cancel(&self) -> Arc<CancelToken> {
        let tok = Arc::new(CancelToken::new());
        let mut guard = self.active_cancel.lock();
        *guard = Some(tok.clone());
        tok
    }

    fn spawn_rate_sampler(
        rt: &Runtime,
        state: &Arc<AppState>,
        mode: AppMode,
        cancel: Arc<CancelToken>,
    ) {
        let stats = state.stats.clone();
        rt.spawn(async move {
            let mut last_time = Instant::now();
            let mut last_bytes: u64 = 0;
            let mut ema_bps: f64 = 0.0;

            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                }

                let now = Instant::now();
                let elapsed_ns = now.duration_since(last_time).as_nanos() as f64;
                if elapsed_ns <= 0.0 {
                    continue;
                }

                let total = match mode {
                    AppMode::Uploading => stats.up_bytes_total.load(Ordering::Relaxed),
                    AppMode::Downloading => stats.down_bytes_total.load(Ordering::Relaxed),
                    _ => break,
                };

                // For duplex mode we still sample individually; the duplex spawn creates two samplers.
                // If the current global mode is DuplexTesting, let both samplers run.
                let current_mode = stats.mode();
                if current_mode != mode && current_mode != AppMode::DuplexTesting {
                    // Mode changed away from what we sample; continue but don't break immediately
                    // to allow duplex coordination.
                    if current_mode == AppMode::Idle {
                        break;
                    }
                }

                let delta = total.saturating_sub(last_bytes);
                last_bytes = total;
                last_time = now;

                let inst_bps = (delta as f64) * 8.0 * 1_000_000_000.0 / elapsed_ns;
                ema_bps = if ema_bps <= 0.0 {
                    inst_bps
                } else {
                    ema_bps * 0.45 + inst_bps * 0.55
                };
                let smooth_bps = ema_bps.round().max(0.0) as u64;

                match mode {
                    AppMode::Uploading => {
                        stats.up_bps_current.store(smooth_bps, Ordering::Relaxed);
                        stats.update_up_peak(smooth_bps);
                    }
                    AppMode::Downloading => {
                        stats.down_bps_current.store(smooth_bps, Ordering::Relaxed);
                        stats.update_down_peak(smooth_bps);
                    }
                    _ => {}
                }
            }
        });
    }

    fn spawn_history_sampler(rt: &Runtime, state: &Arc<AppState>, cancel: Arc<CancelToken>) {
        let stats = state.stats.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                let pt = SpeedSamplePoint {
                    up_bps: stats.up_bps_current.load(Ordering::Relaxed),
                    down_bps: stats.down_bps_current.load(Ordering::Relaxed),
                    nic_up_bps: stats.nic_up_bps.load(Ordering::Relaxed),
                    nic_down_bps: stats.nic_down_bps.load(Ordering::Relaxed),
                };
                stats.push_speed_sample(pt);
            }
        });
    }

    fn spawn_conn_rate_sampler(rt: &Runtime, state: &Arc<AppState>, cancel: Arc<CancelToken>) {
        let stats = state.stats.clone();
        rt.spawn(async move {
            let mut last_time = Instant::now();
            let mut last_estab: u64 = 0;

            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                }

                let now = Instant::now();
                let secs = now.duration_since(last_time).as_secs_f64();
                if secs <= 0.0 {
                    continue;
                }
                let total = stats.conn_orig_success.load(Ordering::Relaxed)
                    + stats.conn_replenish_success.load(Ordering::Relaxed);
                let delta = total.saturating_sub(last_estab);
                last_estab = total;
                last_time = now;

                let cps = ((delta as f64) / secs).round().max(0.0) as u64;
                stats.conn_per_sec.store(cps, Ordering::Relaxed);
                stats.update_cps_peak(cps);
            }
        });
    }
}

fn init_runtime_once<'a>(
    cell: &'a OnceLock<Result<Runtime, String>>,
    state: &Arc<AppState>,
) -> Result<&'a Runtime, &'a str> {
    let res = cell.get_or_init(|| {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 8);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .thread_name("net-stress-worker")
            .build()
            .map_err(|e| format!("初始化 Tokio 运行时失败: {e}"))?;

        let stats_bg = state.stats.clone();
        runtime.spawn(async move {
            // Brief initial delay so OS telemetry never competes with first-frame GUI paint
            tokio::time::sleep(Duration::from_millis(250)).await;
            let mut ticker = tokio::time::interval(Duration::from_millis(500));
            loop {
                ticker.tick().await;
                let snap = query_sys_snapshot();
                stats_bg
                    .sys_tcp_estab
                    .store(snap.tcp_established, Ordering::Relaxed);
                stats_bg
                    .proc_handles
                    .store(snap.process_handles, Ordering::Relaxed);
                stats_bg
                    .tcp_retrans_pct_x100
                    .store(snap.tcp_retrans_pct_x100, Ordering::Relaxed);
                stats_bg.nic_up_bps.store(snap.nic_tx_bps, Ordering::Relaxed);
                stats_bg
                    .nic_down_bps
                    .store(snap.nic_rx_bps, Ordering::Relaxed);
                stats_bg
                    .nic_tx_total
                    .store(snap.nic_tx_bytes, Ordering::Relaxed);
                stats_bg
                    .nic_rx_total
                    .store(snap.nic_rx_bytes, Ordering::Relaxed);
                {
                    let mut guard = stats_bg.nic_summary.lock();
                    *guard = snap.nic_summary.clone();
                }
            }
        });

        // Sample the public egress IP independently against several providers. These
        // observations are not WAN-interface identities; each provider is a separate destination.
        let wan_stats = state.stats.clone();
        runtime.spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let client = match reqwest::Client::builder()
                .http1_only()
                .timeout(Duration::from_secs(4))
                .pool_max_idle_per_host(0)
                .user_agent("WinNetStress/0.5")
                .build()
            {
                Ok(c) => c,
                Err(_) => return,
            };
            let endpoints = [
                "https://api.ipify.org?format=text",
                "https://checkip.amazonaws.com",
                "https://icanhazip.com",
                "https://ifconfig.me/ip",
                "https://ipinfo.io/ip",
                "https://api64.ipify.org",
                "https://ipv4.icanhazip.com",
                "https://ipv6.icanhazip.com",
            ];
            loop {
                // Independent destinations can be routed through different WANs by
                // destination-based load balancing. Do not reuse keep-alive sockets.
                let mut probes = tokio::task::JoinSet::new();
                for url in endpoints {
                    let probe_client = client.clone();
                    probes.spawn(async move {
                        let request = probe_client
                            .get(url)
                            .header(reqwest::header::CONNECTION, "close");
                        let response = tokio::time::timeout(
                            Duration::from_secs(4),
                            request.send(),
                        )
                        .await
                        .ok()?
                        .ok()?;
                        let body = response.text().await.ok()?;
                        let ip = body.trim().parse::<std::net::IpAddr>().ok()?;
                        let provider = url.split('/').nth(2).unwrap_or(url);
                        Some(format!("{provider}={ip}"))
                    });
                }
                let mut observations = Vec::new();
                while let Some(result) = probes.join_next().await {
                    if let Ok(Some(observation)) = result {
                        observations.push(observation);
                    }
                }
                if !observations.is_empty() {
                    let summary = observations.join(" | ");
                    *wan_stats.egress_ip_observations.lock() = summary;
                    wan_stats.egress_ip_last_update_ms.store(
                        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
                        Ordering::Relaxed,
                    );
                }
                tokio::time::sleep(Duration::from_secs(35)).await;
            }
        });

        Ok(runtime)
    });

    res.as_ref().map_err(String::as_str)
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.stop();
    }
}
