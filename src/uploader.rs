//! Multi-stream high-throughput upload bandwidth stress tester.
//! Generates incompressible random bytes purely in RAM and streams via independent
//! HTTP/1.1 TCP sockets (with explicit Content-Length and pre-flight node ranking) or raw TCP.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_core::Stream;
use socket2::SockRef;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, lookup_host};
use tokio::task::JoinSet;

use crate::conn_tester::CancelToken;
use crate::downloader::extract_host_port;
use crate::state::{MemoryLog, SharedStats, TestConfig, format_bytes, format_mbps};

const UPLOAD_BUF_SIZE: usize = 1024 * 1024; // 1 MiB reusable RAM buffer
const UPLOAD_SLICE_SIZE: usize = 256 * 1024; // 256 KiB smooth slice accounting
const DEFAULT_HTTP_POST_BYTES: usize = 4 * 1024 * 1024; // 4 MiB per HTTP/1.1 POST cycle
const PROBE_POST_BYTES: usize = 512 * 1024; // 512 KiB pre-flight probe
const STALL_TIMEOUT_MS: u64 = 8000;

#[derive(Clone)]
enum UploadTarget {
    Http {
        url: String,
        host: String,
        edge_addrs: Vec<SocketAddr>,
        post_bytes: usize,
        allow_full_drain_4xx: bool,
        probe_bps: u64,
    },
    RawTcp {
        host_port: String,
        probe_bps: u64,
    },
}

fn parse_raw_upload_targets(raw_targets: &[String]) -> Vec<(bool, String)> {
    raw_targets
        .iter()
        .filter_map(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                Some((true, trimmed.to_owned()))
            } else {
                let hp = trimmed.strip_prefix("tcp://").unwrap_or(trimmed);
                if hp.contains(':') {
                    Some((false, hp.to_owned()))
                } else {
                    None
                }
            }
        })
        .collect()
}

/// Generate an incompressible buffer in RAM using xorshift64*.
pub fn make_in_memory_random_bytes(size: usize) -> Bytes {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        ^ 0xA076_1D64_78BD_642F;
    let mut state = if seed == 0 { 0x1234_5678_9ABC_DEF0 } else { seed };
    let mut buf = vec![0u8; size];
    for chunk in buf.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bytes = state.to_le_bytes();
        let n = chunk.len();
        chunk.copy_from_slice(&bytes[..n]);
    }
    Bytes::from(buf)
}

async fn resolve_edge_ips(host: &str, port: u16) -> Vec<SocketAddr> {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    if let Ok(Ok(addrs)) = tokio::time::timeout(Duration::from_secs(3), lookup_host((host, port))).await
    {
        for addr in addrs {
            if addr.is_ipv4() {
                if v4.len() < 6 && !v4.contains(&addr) {
                    v4.push(addr);
                }
            } else if v6.len() < 4 && !v6.contains(&addr) {
                v6.push(addr);
            }
        }
    }
    if v4.is_empty() { v6 } else { v4 }
}

fn build_http1_client(pinned: Option<(&str, SocketAddr)>, connect_secs: u64) -> Option<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .http1_only()
        .connect_timeout(Duration::from_secs(connect_secs))
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(15))
        .pool_max_idle_per_host(1)
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) WinNetStress/0.2");

    if let Some((host, addr)) = pinned {
        builder = builder.resolve(host, addr);
    }

    builder.build().ok()
}

struct InMemoryUploadStream {
    chunk: Bytes,
    offset: usize,
    remaining: usize,
    stats: Option<Arc<SharedStats>>,
    pushed_bytes: Option<Arc<AtomicUsize>>,
    last_progress_ms: Option<Arc<AtomicU64>>,
    start_instant: tokio::time::Instant,
    cancel: Arc<CancelToken>,
}

impl Stream for InMemoryUploadStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.cancel.is_cancelled() || self.remaining == 0 {
            return Poll::Ready(None);
        }
        let avail_in_buf = self.chunk.len() - self.offset;
        let to_send = self.remaining.min(UPLOAD_SLICE_SIZE).min(avail_in_buf);
        let slice = self.chunk.slice(self.offset..(self.offset + to_send));
        self.offset = (self.offset + to_send) % self.chunk.len();
        self.remaining -= to_send;

        if let Some(stats) = &self.stats {
            stats
                .up_bytes_total
                .fetch_add(to_send as u64, Ordering::Relaxed);
        }
        if let Some(pushed) = &self.pushed_bytes {
            pushed.fetch_add(to_send, Ordering::Relaxed);
        }
        if let Some(lp) = &self.last_progress_ms {
            lp.store(
                self.start_instant.elapsed().as_millis() as u64,
                Ordering::Relaxed,
            );
        }

        Poll::Ready(Some(Ok(slice)))
    }
}

async fn probe_and_rank_upload_targets(
    raw: Vec<(bool, String)>,
    payload: Bytes,
    cancel: &Arc<CancelToken>,
    log: &MemoryLog,
) -> Vec<UploadTarget> {
    let mut set = JoinSet::new();

    for (is_http, target_str) in raw {
        let payload = payload.clone();
        let cancel = cancel.clone();

        set.spawn(async move {
            if !is_http {
                let t0 = Instant::now();
                let res = tokio::time::timeout(
                    Duration::from_millis(2000),
                    TcpStream::connect(&target_str),
                )
                .await;
                return match res {
                    Ok(Ok(s)) => {
                        drop(s);
                        let rtt_ms = t0.elapsed().as_millis().max(1) as u64;
                        Some((
                            true,
                            UploadTarget::RawTcp {
                                host_port: target_str,
                                probe_bps: 100_000_000_u64 / rtt_ms,
                            },
                        ))
                    }
                    _ => None,
                };
            }

            let Some((host, port)) = extract_host_port(&target_str) else {
                return None;
            };
            let edge_addrs = resolve_edge_ips(&host, port).await;
            let pinned = edge_addrs.first().copied().map(|a| (host.as_str(), a));
            let client = build_http1_client(pinned, 3)?;

            let mut probe_len = PROBE_POST_BYTES;
            for _ in 0..2 {
                let pushed = Arc::new(AtomicUsize::new(0));
                let stream = InMemoryUploadStream {
                    chunk: payload.clone(),
                    offset: 0,
                    remaining: probe_len,
                    stats: None,
                    pushed_bytes: Some(pushed.clone()),
                    last_progress_ms: None,
                    start_instant: tokio::time::Instant::now(),
                    cancel: cancel.clone(),
                };

                let t0 = Instant::now();
                let req = client
                    .post(&target_str)
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .header(reqwest::header::CONTENT_LENGTH, probe_len as u64)
                    .body(reqwest::Body::wrap_stream(stream))
                    .send();

                match tokio::time::timeout(Duration::from_secs(4), req).await {
                    Ok(Ok(resp)) => {
                        let status = resp.status();
                        let sent = pushed.load(Ordering::Relaxed);
                        let elapsed_ns = t0.elapsed().as_nanos().max(1) as f64;
                        let bps = ((sent as f64) * 8.0 * 1_000_000_000.0 / elapsed_ns) as u64;

                        if status.is_success() {
                            return Some((
                                true,
                                UploadTarget::Http {
                                    url: target_str,
                                    host,
                                    edge_addrs,
                                    post_bytes: DEFAULT_HTTP_POST_BYTES,
                                    allow_full_drain_4xx: false,
                                    probe_bps: bps,
                                },
                            ));
                        }
                        if status.as_u16() == 413 && probe_len > 128 * 1024 {
                            probe_len = 128 * 1024;
                            continue;
                        }
                        // Telemetry/CDN endpoints (like arms-retcode/snowflake) may drain 100% of
                        // the POST body over TLS/TCP and reply 400/404 if JSON format wasn't used.
                        if sent >= probe_len && !matches!(status.as_u16(), 403 | 405 | 429 | 500..=599) {
                            return Some((
                                false,
                                UploadTarget::Http {
                                    url: target_str,
                                    host,
                                    edge_addrs,
                                    post_bytes: 2 * 1024 * 1024,
                                    allow_full_drain_4xx: true,
                                    probe_bps: bps,
                                },
                            ));
                        }
                        return None;
                    }
                    _ => return None,
                }
            }
            None
        });
    }

    let mut strict_list = Vec::new();
    let mut drain_list = Vec::new();

    while let Some(res) = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            set.abort_all();
            return Vec::new();
        }
        r = set.join_next() => r,
    } {
        if let Ok(Some((is_strict, target))) = res {
            if is_strict {
                strict_list.push(target);
            } else {
                drain_list.push(target);
            }
        }
    }

    let mut chosen = if !strict_list.is_empty() {
        strict_list
    } else {
        drain_list
    };

    chosen.sort_by_key(|t| std::cmp::Reverse(target_probe_bps(t)));
    if chosen.is_empty() {
        return chosen;
    }

    let best_bps = target_probe_bps(&chosen[0]);
    let floor_bps = best_bps / 5;
    let mut selected: Vec<UploadTarget> = chosen
        .iter()
        .enumerate()
        .filter(|(i, t)| *i < 3 || target_probe_bps(t) >= floor_bps)
        .map(|(_, t)| t.clone())
        .collect();
    selected.truncate(6);

    let summary: Vec<String> = selected
        .iter()
        .map(|t| match t {
            UploadTarget::Http { host, probe_bps, .. } => {
                format!("{host} ({})", format_mbps(*probe_bps))
            }
            UploadTarget::RawTcp { host_port, .. } => format!("tcp://{host_port}"),
        })
        .collect();
    log.push(format!(
        "上行预探测优选 {} 个高速节点: {}",
        selected.len(),
        summary.join(", ")
    ));

    selected
}

fn target_probe_bps(t: &UploadTarget) -> u64 {
    match t {
        UploadTarget::Http { probe_bps, .. } | UploadTarget::RawTcp { probe_bps, .. } => {
            *probe_bps
        }
    }
}

pub async fn run_upload_test(
    config: TestConfig,
    stats: Arc<SharedStats>,
    log: Arc<MemoryLog>,
    cancel: Arc<CancelToken>,
) {
    let raw_targets = parse_raw_upload_targets(&config.upload_targets);
    if raw_targets.is_empty() {
        log.push("错误：没有有效的上传目标（支持 http(s)://... 或 tcp://ip:port）。");
        return;
    }

    log.push("正在内存中预生成 1 MiB 随机不可压缩测试载荷...");
    let payload = make_in_memory_random_bytes(UPLOAD_BUF_SIZE);

    log.push(format!(
        "正在对 {} 个上行候选节点执行测速预筛选（强制 HTTP/1.1 独立 TCP 多流）...",
        raw_targets.len()
    ));

    let targets = probe_and_rank_upload_targets(raw_targets, payload.clone(), &cancel, &log).await;
    if cancel.is_cancelled() {
        return;
    }
    if targets.is_empty() {
        log.push("错误：所有上行候选节点预探测均不可用，请检查网络或自定义上传目标。");
        return;
    }

    let concurrency = config.stream_concurrency.clamp(1, 256);
    log.push(format!(
        "开始持续上行带宽压测：{concurrency} 路独立 TCP 流（纯内存循环发送，零磁盘读写）"
    ));

    let targets = Arc::new(targets);
    let last_err_log_ms = Arc::new(AtomicU64::new(0));
    let start_instant = tokio::time::Instant::now();
    let mut workers = JoinSet::new();

    for worker_id in 0..concurrency {
        let targets = targets.clone();
        let payload = payload.clone();
        let stats = stats.clone();
        let log = log.clone();
        let cancel = cancel.clone();
        let last_err_log_ms = last_err_log_ms.clone();

        workers.spawn(async move {
            let mut idx = worker_id % targets.len();
            let mut post_sizes: Vec<usize> = targets
                .iter()
                .map(|t| match t {
                    UploadTarget::Http { post_bytes, .. } => *post_bytes,
                    UploadTarget::RawTcp { .. } => 0,
                })
                .collect();
            let mut clients: Vec<Option<reqwest::Client>> = targets
                .iter()
                .map(|t| match t {
                    UploadTarget::Http {
                        host, edge_addrs, ..
                    } => {
                        let pinned = if edge_addrs.is_empty() {
                            None
                        } else {
                            let addr = edge_addrs[worker_id % edge_addrs.len()];
                            Some((host.as_str(), addr))
                        };
                        build_http1_client(pinned, 5)
                    }
                    UploadTarget::RawTcp { .. } => None,
                })
                .collect();
            let mut cycle_nonce = 0usize;

            while !cancel.is_cancelled() {
                cycle_nonce = cycle_nonce.wrapping_add(1);
                let target = &targets[idx];

                match target {
                    UploadTarget::Http {
                        url,
                        host,
                        edge_addrs,
                        allow_full_drain_4xx,
                        ..
                    } => {
                        let Some(client) = clients[idx].as_ref() else {
                            idx = (idx + 1) % targets.len();
                            continue;
                        };
                        let http_post_size = post_sizes[idx];
                        let last_progress_ms = Arc::new(AtomicU64::new(
                            start_instant.elapsed().as_millis() as u64,
                        ));
                        let pushed = Arc::new(AtomicUsize::new(0));
                        let stream = InMemoryUploadStream {
                            chunk: payload.clone(),
                            offset: 0,
                            remaining: http_post_size,
                            stats: Some(stats.clone()),
                            pushed_bytes: Some(pushed.clone()),
                            last_progress_ms: Some(last_progress_ms.clone()),
                            start_instant,
                            cancel: cancel.clone(),
                        };
                        let body = reqwest::Body::wrap_stream(stream);

                        stats.up_active_streams.fetch_add(1, Ordering::Relaxed);
                        let req_fut = client
                            .post(url)
                            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                            .header(reqwest::header::CONTENT_LENGTH, http_post_size as u64)
                            .body(body)
                            .send();

                        let lp_watch = last_progress_ms.clone();
                        let stall_fut = async move {
                            loop {
                                tokio::time::sleep(Duration::from_secs(2)).await;
                                let now_ms = start_instant.elapsed().as_millis() as u64;
                                let prev = lp_watch.load(Ordering::Relaxed);
                                if now_ms.saturating_sub(prev) >= STALL_TIMEOUT_MS {
                                    break;
                                }
                            }
                        };

                        enum UploadOutcome {
                            Done(Result<reqwest::Response, reqwest::Error>),
                            Stalled,
                        }

                        let outcome = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {
                                stats.up_active_streams.fetch_sub(1, Ordering::Relaxed);
                                return;
                            }
                            res = req_fut => UploadOutcome::Done(res),
                            _ = stall_fut => UploadOutcome::Stalled,
                        };
                        stats.up_active_streams.fetch_sub(1, Ordering::Relaxed);

                        match outcome {
                            UploadOutcome::Done(Ok(resp)) => {
                                let status = resp.status();
                                let sent = pushed.load(Ordering::Relaxed);
                                if status.as_u16() == 413 && http_post_size > 256 * 1024 {
                                    let next_size = (http_post_size / 2).max(256 * 1024);
                                    post_sizes[idx] = next_size;
                                    log_throttled(
                                        &log,
                                        &last_err_log_ms,
                                        start_instant,
                                        format!(
                                            "上行节点 [{host}] 返回 413，自适应调整单次载荷为 {}",
                                            format_bytes(next_size as u64)
                                        ),
                                    );
                                } else if status.is_success()
                                    || (*allow_full_drain_4xx && sent >= http_post_size)
                                {
                                    // Keep-alive cycle succeeded cleanly; stay on fast node
                                } else {
                                    stats.up_errors.fetch_add(1, Ordering::Relaxed);
                                    log_throttled(
                                        &log,
                                        &last_err_log_ms,
                                        start_instant,
                                        format!("上行 HTTP 状态异常 [{status}] [{host}]"),
                                    );
                                    idx = (idx + 1) % targets.len();
                                    sleep_or_cancel(&cancel, Duration::from_millis(250)).await;
                                }
                            }
                            UploadOutcome::Done(Err(e)) => {
                                stats.up_errors.fetch_add(1, Ordering::Relaxed);
                                log_throttled(
                                    &log,
                                    &last_err_log_ms,
                                    start_instant,
                                    format!("上行 POST 失败 [{host}]: {e}"),
                                );
                                idx = (idx + 1) % targets.len();
                                sleep_or_cancel(&cancel, Duration::from_millis(250)).await;
                            }
                            UploadOutcome::Stalled => {
                                stats.up_errors.fetch_add(1, Ordering::Relaxed);
                                log_throttled(
                                    &log,
                                    &last_err_log_ms,
                                    start_instant,
                                    format!("上行流卡顿超时，自动重建连接并切换节点 [{host}]"),
                                );
                                let pinned = if edge_addrs.is_empty() {
                                    None
                                } else {
                                    let addr = edge_addrs[(worker_id + cycle_nonce) % edge_addrs.len()];
                                    Some((host.as_str(), addr))
                                };
                                clients[idx] = build_http1_client(pinned, 5);
                                idx = (idx + 1) % targets.len();
                            }
                        }
                    }
                    UploadTarget::RawTcp { host_port, .. } => {
                        let connect_res = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return,
                            res = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(host_port)) => res,
                        };

                        let stream = match connect_res {
                            Ok(Ok(s)) => s,
                            Ok(Err(e)) => {
                                stats.up_errors.fetch_add(1, Ordering::Relaxed);
                                log_throttled(
                                    &log,
                                    &last_err_log_ms,
                                    start_instant,
                                    format!("TCP 上行连接失败 [{host_port}]: {e}"),
                                );
                                idx = (idx + 1) % targets.len();
                                sleep_or_cancel(&cancel, Duration::from_millis(300)).await;
                                continue;
                            }
                            Err(_) => {
                                stats.up_errors.fetch_add(1, Ordering::Relaxed);
                                log_throttled(
                                    &log,
                                    &last_err_log_ms,
                                    start_instant,
                                    format!("TCP 上行连接超时 [{host_port}]"),
                                );
                                idx = (idx + 1) % targets.len();
                                sleep_or_cancel(&cancel, Duration::from_millis(300)).await;
                                continue;
                            }
                        };

                        let _ = stream.set_nodelay(true);
                        let _ = SockRef::from(&stream).set_send_buffer_size(512 * 1024);

                        let (mut reader, mut writer) = stream.into_split();
                        let drain_cancel = cancel.clone();
                        let drain_task = tokio::spawn(async move {
                            let mut sink = [0u8; 4096];
                            loop {
                                tokio::select! {
                                    biased;
                                    _ = drain_cancel.cancelled() => break,
                                    r = reader.read(&mut sink) => {
                                        match r {
                                            Ok(0) | Err(_) => break,
                                            Ok(_) => continue,
                                        }
                                    }
                                }
                            }
                        });

                        stats.up_active_streams.fetch_add(1, Ordering::Relaxed);
                        loop {
                            let write_res = tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    drain_task.abort();
                                    stats.up_active_streams.fetch_sub(1, Ordering::Relaxed);
                                    return;
                                }
                                r = tokio::time::timeout(Duration::from_secs(8), writer.write(&payload)) => r,
                            };

                            match write_res {
                                Ok(Ok(0)) => break,
                                Ok(Ok(n)) => {
                                    stats.up_bytes_total.fetch_add(n as u64, Ordering::Relaxed);
                                }
                                Ok(Err(e)) => {
                                    stats.up_errors.fetch_add(1, Ordering::Relaxed);
                                    log_throttled(
                                        &log,
                                        &last_err_log_ms,
                                        start_instant,
                                        format!("TCP 上行写入中断 [{host_port}]: {e}"),
                                    );
                                    break;
                                }
                                Err(_) => {
                                    stats.up_errors.fetch_add(1, Ordering::Relaxed);
                                    log_throttled(
                                        &log,
                                        &last_err_log_ms,
                                        start_instant,
                                        format!("TCP 上行发送卡顿超时 [{host_port}]"),
                                    );
                                    break;
                                }
                            }
                        }
                        drain_task.abort();
                        stats.up_active_streams.fetch_sub(1, Ordering::Relaxed);
                        idx = (idx + 1) % targets.len();
                    }
                }
            }
        });
    }

    while workers.join_next().await.is_some() {}
    stats.up_active_streams.store(0, Ordering::Relaxed);

    let total = stats.up_bytes_total.load(Ordering::Relaxed);
    let peak = stats.up_bps_peak.load(Ordering::Relaxed);
    let errs = stats.up_errors.load(Ordering::Relaxed);
    log.push(format!(
        "上行压测已停止：累计发送 {}，峰值速率 {}，错误次数 {errs}",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_random_bytes_in_memory() {
        let b1 = make_in_memory_random_bytes(1024);
        assert_eq!(b1.len(), 1024);
        assert!(b1.iter().any(|&x| x != 0));
    }

    #[test]
    fn test_parse_raw_upload_targets() {
        let targets = parse_raw_upload_targets(&[
            "https://speed.cloudflare.com/__up".to_string(),
            "tcp://127.0.0.1:9000".to_string(),
            "192.168.1.10:5201".to_string(),
            "invalid".to_string(),
        ]);
        assert_eq!(targets.len(), 3);
    }
}
