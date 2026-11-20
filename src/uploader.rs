//! Multi-stream continuous upload bandwidth stress tester.
//! Generates incompressible random bytes purely in RAM and streams via HTTP POST or raw TCP.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_core::Stream;
use socket2::SockRef;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinSet;

use crate::conn_tester::CancelToken;
use crate::state::{MemoryLog, SharedStats, TestConfig, format_bytes, format_mbps};

const UPLOAD_BUF_SIZE: usize = 1024 * 1024; // 1 MiB reusable RAM buffer
const DEFAULT_HTTP_POST_BYTES: usize = 32 * 1024 * 1024; // 32 MiB per POST cycle

#[derive(Clone)]
enum UploadTarget {
    Http(String),
    RawTcp(String),
}

fn parse_upload_targets(raw_targets: &[String]) -> Vec<UploadTarget> {
    raw_targets
        .iter()
        .filter_map(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                Some(UploadTarget::Http(trimmed.to_owned()))
            } else {
                let hp = trimmed.strip_prefix("tcp://").unwrap_or(trimmed);
                if hp.contains(':') {
                    Some(UploadTarget::RawTcp(hp.to_owned()))
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

struct InMemoryUploadStream {
    chunk: Bytes,
    remaining: usize,
    stats: Arc<SharedStats>,
    cancel: Arc<CancelToken>,
}

impl Stream for InMemoryUploadStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.cancel.is_cancelled() || self.remaining == 0 {
            return Poll::Ready(None);
        }
        let to_send = self.remaining.min(self.chunk.len());
        let slice = if to_send == self.chunk.len() {
            self.chunk.clone() // O(1) Arc refcount clone, zero-copy
        } else {
            self.chunk.slice(..to_send)
        };
        self.remaining -= to_send;
        self.stats
            .up_bytes_total
            .fetch_add(to_send as u64, Ordering::Relaxed);
        Poll::Ready(Some(Ok(slice)))
    }
}

pub async fn run_upload_test(
    config: TestConfig,
    stats: Arc<SharedStats>,
    log: Arc<MemoryLog>,
    cancel: Arc<CancelToken>,
) {
    let targets = parse_upload_targets(&config.upload_targets);
    if targets.is_empty() {
        log.push("错误：没有有效的上传目标（支持 http(s)://... 或 tcp://ip:port）。");
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
            log.push(format!("初始化 HTTP 上传客户端失败: {e}"));
            return;
        }
    };

    log.push("正在内存中预生成 1 MiB 随机不可压缩测试载荷...");
    let payload = make_in_memory_random_bytes(UPLOAD_BUF_SIZE);
    log.push(format!(
        "开始持续上行带宽压测：并发流 {concurrency}，目标节点 {} 个（纯内存循环发送，零磁盘读写）",
        targets.len()
    ));

    let targets = Arc::new(targets);
    let last_err_log_ms = Arc::new(AtomicU64::new(0));
    let start_instant = tokio::time::Instant::now();
    let mut workers = JoinSet::new();

    for worker_id in 0..concurrency {
        let client = client.clone();
        let targets = targets.clone();
        let payload = payload.clone();
        let stats = stats.clone();
        let log = log.clone();
        let cancel = cancel.clone();
        let last_err_log_ms = last_err_log_ms.clone();

        workers.spawn(async move {
            let mut idx = worker_id % targets.len();
            let mut http_post_size = DEFAULT_HTTP_POST_BYTES;

            while !cancel.is_cancelled() {
                let target = &targets[idx];
                idx = (idx + 1) % targets.len();

                match target {
                    UploadTarget::Http(url) => {
                        let stream = InMemoryUploadStream {
                            chunk: payload.clone(),
                            remaining: http_post_size,
                            stats: stats.clone(),
                            cancel: cancel.clone(),
                        };
                        let body = reqwest::Body::wrap_stream(stream);

                        stats.up_active_streams.fetch_add(1, Ordering::Relaxed);
                        let req_fut = client
                            .post(url)
                            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                            .body(body)
                            .send();

                        let res = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {
                                stats.up_active_streams.fetch_sub(1, Ordering::Relaxed);
                                return;
                            }
                            res = req_fut => res,
                        };
                        stats.up_active_streams.fetch_sub(1, Ordering::Relaxed);

                        match res {
                            Ok(resp) => {
                                let status = resp.status();
                                if status.as_u16() == 413 && http_post_size > 512 * 1024 {
                                    http_post_size = (http_post_size / 4).max(512 * 1024);
                                    log_throttled(
                                        &log,
                                        &last_err_log_ms,
                                        start_instant,
                                        format!(
                                            "上行节点返回 413，自动缩小单次 POST 载荷为 {} [{url}]",
                                            format_bytes(http_post_size as u64)
                                        ),
                                    );
                                } else if !status.is_success() {
                                    stats.up_errors.fetch_add(1, Ordering::Relaxed);
                                    log_throttled(
                                        &log,
                                        &last_err_log_ms,
                                        start_instant,
                                        format!("上行 HTTP 状态异常 [{status}] [{url}]"),
                                    );
                                    sleep_or_cancel(&cancel, Duration::from_millis(600)).await;
                                }
                            }
                            Err(e) => {
                                stats.up_errors.fetch_add(1, Ordering::Relaxed);
                                log_throttled(
                                    &log,
                                    &last_err_log_ms,
                                    start_instant,
                                    format!("上行 POST 失败 [{url}]: {e}"),
                                );
                                sleep_or_cancel(&cancel, Duration::from_millis(600)).await;
                            }
                        }
                    }
                    UploadTarget::RawTcp(host_port) => {
                        let connect_res = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return,
                            res = tokio::time::timeout(Duration::from_secs(6), TcpStream::connect(host_port)) => res,
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
                                sleep_or_cancel(&cancel, Duration::from_millis(600)).await;
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
                                sleep_or_cancel(&cancel, Duration::from_millis(600)).await;
                                continue;
                            }
                        };

                        let _ = stream.set_nodelay(true);
                        let _ = SockRef::from(&stream).set_send_buffer_size(256 * 1024);

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
                                r = writer.write(&payload) => r,
                            };

                            match write_res {
                                Ok(0) => break,
                                Ok(n) => {
                                    stats.up_bytes_total.fetch_add(n as u64, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    stats.up_errors.fetch_add(1, Ordering::Relaxed);
                                    log_throttled(
                                        &log,
                                        &last_err_log_ms,
                                        start_instant,
                                        format!("TCP 上行写入中断 [{host_port}]: {e}"),
                                    );
                                    break;
                                }
                            }
                        }
                        drain_task.abort();
                        stats.up_active_streams.fetch_sub(1, Ordering::Relaxed);
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
    fn test_parse_upload_targets() {
        let targets = parse_upload_targets(&[
            "https://speed.cloudflare.com/__up".to_string(),
            "tcp://127.0.0.1:9000".to_string(),
            "192.168.1.10:5201".to_string(),
            "invalid".to_string(),
        ]);
        assert_eq!(targets.len(), 3);
    }
}
