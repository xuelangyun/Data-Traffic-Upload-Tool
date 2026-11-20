//! Background Tokio runtime & test lifecycle coordinator.
//! Initializes the Tokio thread pool lazily in the background after the Win32 UI paints
//! so application startup feels instantaneous.

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
                let total = stats.conn_established_total.load(Ordering::Relaxed);
                let delta = total.saturating_sub(last_estab);
                last_estab = total;
                last_time = now;

                let cps = ((delta as f64) / secs).round().max(0.0) as u64;
                stats.conn_per_sec.store(cps, Ordering::Relaxed);
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
