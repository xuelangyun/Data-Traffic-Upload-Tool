//! Background Tokio runtime & test lifecycle coordinator.
//! Initializes the Tokio thread pool lazily in the background after the Win32 UI paints
//! so application startup feels instantaneous. Supports duplex (up+down) simultaneous testing.

use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::runtime::Runtime;

use crate::conn_tester::{CancelToken, run_conn_test};
use crate::downloader::run_download_test;
use crate::state::{AppMode, AppState, TestConfig};
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
        let cancel = self.install_cancel();
        let state = self.state.clone();

        Self::spawn_conn_rate_sampler(rt, &self.state, cancel.clone());
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
        let cancel = self.install_cancel();
        let state = self.state.clone();

        Self::spawn_rate_sampler(rt, &self.state, AppMode::Uploading, cancel.clone());
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
        let cancel = self.install_cancel();
        let state = self.state.clone();

        Self::spawn_rate_sampler(rt, &self.state, AppMode::Downloading, cancel.clone());
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
        let cancel = self.install_cancel();
        let state = self.state.clone();

        // Independent concurrency samplers for up and down; they share the same cancel token
        // so a single Stop click cleans up both directions atomically.
        Self::spawn_rate_sampler(rt, &self.state, AppMode::Uploading, cancel.clone());
        Self::spawn_rate_sampler(rt, &self.state, AppMode::Downloading, cancel.clone());

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
            const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
            const RATE_WINDOW: Duration = Duration::from_secs(1);
            const MIN_WINDOW: Duration = Duration::from_millis(750);

            let mut samples: VecDeque<(Instant, u64)> = VecDeque::with_capacity(8);
            let initial_bytes = match mode {
                AppMode::Uploading => stats.up_bytes_body_yielded.load(Ordering::Relaxed),
                AppMode::Downloading => stats.down_bytes_total.load(Ordering::Relaxed),
                _ => return,
            };
            samples.push_back((Instant::now(), initial_bytes));

            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(SAMPLE_INTERVAL) => {}
                }

                let now = Instant::now();
                let current_mode = stats.mode();
                if current_mode != mode && current_mode != AppMode::DuplexTesting {
                    break;
                }

                let total = match mode {
                    // This measures bytes yielded by the request-body stream to the HTTP client.
                    // It is not a NIC counter or a server-side byte acknowledgement.
                    AppMode::Uploading => stats.up_bytes_body_yielded.load(Ordering::Relaxed),
                    AppMode::Downloading => stats.down_bytes_total.load(Ordering::Relaxed),
                    _ => break,
                };

                if samples.back().is_some_and(|(_, previous)| total < *previous) {
                    samples.clear();
                    samples.push_back((now, total));
                    continue;
                }

                samples.push_back((now, total));
                while samples.len() > 2
                    && now.duration_since(samples[1].0) >= RATE_WINDOW
                {
                    samples.pop_front();
                }

                if let Some((started, baseline)) = samples.front().copied() {
                    let elapsed = now.saturating_duration_since(started);
                    if elapsed >= MIN_WINDOW {
                        let rate_bps = calculate_rate_bps(total.saturating_sub(baseline), elapsed);
                        match mode {
                            AppMode::Uploading => {
                                stats.up_bps_current.store(rate_bps, Ordering::Relaxed);
                                stats.update_up_peak(rate_bps);
                            }
                            AppMode::Downloading => {
                                stats.down_bps_current.store(rate_bps, Ordering::Relaxed);
                                stats.update_down_peak(rate_bps);
                            }
                            _ => {}
                        }
                    }
                }
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

fn calculate_rate_bps(delta_bytes: u64, elapsed: Duration) -> u64 {
    let nanos = elapsed.as_nanos();
    if nanos == 0 {
        return 0;
    }

    let bits_per_second = (delta_bytes as u128)
        .saturating_mul(8_000_000_000)
        .checked_div(nanos)
        .unwrap_or(0);
    bits_per_second.min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod rate_tests {
    use super::calculate_rate_bps;
    use std::time::Duration;

    #[test]
    fn rate_uses_elapsed_time_and_bytes_without_ema_lag() {
        assert_eq!(calculate_rate_bps(1_250_000, Duration::from_secs(1)), 10_000_000);
        assert_eq!(calculate_rate_bps(625_000, Duration::from_millis(500)), 10_000_000);
        assert_eq!(calculate_rate_bps(u64::MAX, Duration::ZERO), 0);
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.stop();
    }
}
