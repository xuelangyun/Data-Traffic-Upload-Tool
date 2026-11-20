//! Multi-stream continuous HTTP/HTTPS download bandwidth stress tester.
//! Discards all received bytes in memory without touching disk.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::task::JoinSet;

use crate::conn_tester::CancelToken;
use crate::state::{MemoryLog, SharedStats, TestConfig, format_bytes, format_mbps};

pub async fn run_download_test(
    config: TestConfig,
    stats: Arc<SharedStats>,
    log: Arc<MemoryLog>,
    cancel: Arc<CancelToken>,
) {
    let urls: Vec<String> = config
        .download_targets
        .into_iter()
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .collect();

    if urls.is_empty() {
        log.push("错误：没有有效的 HTTP/HTTPS 下载目标 URL。");
        return;
    }

    let concurrency = config.stream_concurrency.clamp(1, 128);
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .tcp_nodelay(true)
        .pool_max_idle_per_host(concurrency)
        .user_agent("WinNetStress/0.1 (Windows NT 10.0; Win64; x64)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            log.push(format!("初始化 HTTP 客户端失败: {e}"));
            return;
        }
    };

    log.push(format!(
        "开始持续下行带宽压测：并发流 {concurrency}，目标节点 {} 个（数据仅在内存丢弃，不写磁盘）",
        urls.len()
    ));

    let urls = Arc::new(urls);
    let last_err_log_ms = Arc::new(AtomicU64::new(0));
    let start_instant = tokio::time::Instant::now();
    let mut workers = JoinSet::new();

    for worker_id in 0..concurrency {
        let client = client.clone();
        let urls = urls.clone();
        let stats = stats.clone();
        let log = log.clone();
        let cancel = cancel.clone();
        let last_err_log_ms = last_err_log_ms.clone();

        workers.spawn(async move {
            let mut url_idx = worker_id % urls.len();
            while !cancel.is_cancelled() {
                let url = &urls[url_idx];
                url_idx = (url_idx + 1) % urls.len();

                let resp_res = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    res = client.get(url).send() => res,
                };

                let mut resp = match resp_res {
                    Ok(r) => r,
                    Err(e) => {
                        stats.down_errors.fetch_add(1, Ordering::Relaxed);
                        log_throttled(
                            &log,
                            &last_err_log_ms,
                            start_instant,
                            format!("下行请求失败 [{url}]: {e}"),
                        );
                        sleep_or_cancel(&cancel, Duration::from_millis(600)).await;
                        continue;
                    }
                };

                if !resp.status().is_success() {
                    let status = resp.status();
                    stats.down_errors.fetch_add(1, Ordering::Relaxed);
                    log_throttled(
                        &log,
                        &last_err_log_ms,
                        start_instant,
                        format!("下行 HTTP 状态异常 [{status}] [{url}]"),
                    );
                    sleep_or_cancel(&cancel, Duration::from_millis(600)).await;
                    continue;
                }

                stats.down_active_streams.fetch_add(1, Ordering::Relaxed);
                loop {
                    let chunk_res = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            stats.down_active_streams.fetch_sub(1, Ordering::Relaxed);
                            return;
                        }
                        chunk = resp.chunk() => chunk,
                    };

                    match chunk_res {
                        Ok(Some(bytes)) => {
                            // Discard chunk immediately; only accumulate byte count in atomic counter.
                            stats
                                .down_bytes_total
                                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        }
                        Ok(None) => {
                            // Stream finished normally; loop to start next download immediately.
                            break;
                        }
                        Err(e) => {
                            stats.down_errors.fetch_add(1, Ordering::Relaxed);
                            log_throttled(
                                &log,
                                &last_err_log_ms,
                                start_instant,
                                format!("下行读取中断 [{url}]: {e}"),
                            );
                            sleep_or_cancel(&cancel, Duration::from_millis(400)).await;
                            break;
                        }
                    }
                }
                stats.down_active_streams.fetch_sub(1, Ordering::Relaxed);
            }
        });
    }

    while workers.join_next().await.is_some() {}
    stats.down_active_streams.store(0, Ordering::Relaxed);

    let total = stats.down_bytes_total.load(Ordering::Relaxed);
    let peak = stats.down_bps_peak.load(Ordering::Relaxed);
    let errs = stats.down_errors.load(Ordering::Relaxed);
    log.push(format!(
        "下行压测已停止：累计下载 {}，峰值速率 {}，错误次数 {errs}",
        format_bytes(total),
        format_mbps(peak)
    ));
}

async fn sleep_or_cancel(cancel: &CancelToken, dur: Duration) {
    tokio::select! {
        _ = cancel.cancelled() => {},
        _ = tokio::time::sleep(dur) => {},
    }
}

fn log_throttled(
    log: &MemoryLog,
    last_ms: &AtomicU64,
    start: tokio::time::Instant,
    msg: String,
) {
    let now_ms = start.elapsed().as_millis() as u64;
    let prev = last_ms.load(Ordering::Relaxed);
    if now_ms.saturating_sub(prev) >= 2000
        && last_ms
            .compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        log.push(msg);
    }
}
