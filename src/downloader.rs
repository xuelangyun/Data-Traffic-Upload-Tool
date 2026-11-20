//! Multi-stream high-throughput HTTP/HTTPS download bandwidth stress tester.
//! Uses pre-flight speed ranking, independent HTTP/1.1 TCP sockets per worker,
//! multi-IP edge dispersion, and zero-disk memory discard.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::net::lookup_host;
use tokio::task::JoinSet;

use crate::conn_tester::CancelToken;
use crate::state::{MemoryLog, SharedStats, TestConfig, format_bytes, format_mbps};

const PROBE_WINDOW: Duration = Duration::from_millis(1400);
const PROBE_BYTE_CAP: usize = 6 * 1024 * 1024;
const CHUNK_STALL_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone)]
struct RankedDownloadNode {
    url: String,
    host: String,
    edge_addrs: Vec<SocketAddr>,
    probe_bps: u64,
}

pub fn extract_host_port(url: &str) -> Option<(String, u16)> {
    let (default_port, rest) = if let Some(r) = url.strip_prefix("https://") {
        (443u16, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (80u16, r)
    } else {
        return None;
    };

    let authority = rest.split(['/', '?', '#']).next()?.trim();
    if authority.is_empty() {
        return None;
    }

    if let Some(bracket_end) = authority.strip_prefix('[').and_then(|s| s.find(']')) {
        let host = &authority[1..=bracket_end];
        let tail = &authority[bracket_end + 2..];
        let port = tail
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(default_port);
        return Some((host.to_string(), port));
    }

    if let Some((host, port_str)) = authority.rsplit_once(':')
        && !host.is_empty()
        && let Ok(port) = port_str.parse::<u16>()
    {
        return Some((host.to_string(), port));
    }

    Some((authority.to_string(), default_port))
}

fn cache_busted_url(base: &str, nonce: u64) -> String {
    if base.contains("garbage.php") || base.contains("fastgarbage.php") {
        let sep = if base.contains('?') { '&' } else { '?' };
        format!("{base}{sep}r={nonce}")
    } else {
        base.to_string()
    }
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

async fn probe_and_rank_urls(
    urls: Vec<String>,
    log: &MemoryLog,
    cancel: &CancelToken,
) -> Vec<RankedDownloadNode> {
    let mut set = JoinSet::new();

    for (idx, url) in urls.into_iter().enumerate() {
        set.spawn(async move {
            let Some((host, port)) = extract_host_port(&url) else {
                return None;
            };
            let edge_addrs = resolve_edge_ips(&host, port).await;
            let pinned = edge_addrs.first().copied().map(|a| (host.as_str(), a));
            let client = build_http1_client(pinned, 3)?;

            let probe_url = cache_busted_url(&url, 1000 + idx as u64);
            let t0 = Instant::now();
            let mut resp = match tokio::time::timeout(
                Duration::from_secs(4),
                client.get(&probe_url).send(),
            )
            .await
            {
                Ok(Ok(r)) if r.status().is_success() => r,
                _ => return None,
            };

            let mut bytes_read = 0usize;
            let deadline = Instant::now() + PROBE_WINDOW;
            while Instant::now() < deadline && bytes_read < PROBE_BYTE_CAP {
                let rem = deadline.saturating_duration_since(Instant::now());
                match tokio::time::timeout(rem, resp.chunk()).await {
                    Ok(Ok(Some(b))) => bytes_read += b.len(),
                    _ => break,
                }
            }

            if bytes_read == 0 {
                return None;
            }
            let elapsed_ns = t0.elapsed().as_nanos().max(1) as f64;
            let bps = ((bytes_read as f64) * 8.0 * 1_000_000_000.0 / elapsed_ns) as u64;
            Some(RankedDownloadNode {
                url,
                host,
                edge_addrs,
                probe_bps: bps,
            })
        });
    }

    let mut ranked = Vec::new();
    while let Some(res) = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            set.abort_all();
            return Vec::new();
        }
        r = set.join_next() => r,
    } {
        if let Ok(Some(node)) = res {
            ranked.push(node);
        }
    }

    ranked.sort_by(|a, b| b.probe_bps.cmp(&a.probe_bps));
    if ranked.is_empty() {
        return ranked;
    }

    let best_bps = ranked[0].probe_bps;
    let floor_bps = best_bps / 5; // Keep fast nodes within >=20% of top probe speed (at least top 3)
    let mut selected: Vec<RankedDownloadNode> = ranked
        .iter()
        .enumerate()
        .filter(|(i, n)| *i < 3 || n.probe_bps >= floor_bps)
        .map(|(_, n)| n.clone())
        .collect();
    selected.truncate(6);

    let summary: Vec<String> = selected
        .iter()
        .map(|n| format!("{} ({})", n.host, format_mbps(n.probe_bps)))
        .collect();
    log.push(format!(
        "下行预探测优选 {} 个高速节点: {}",
        selected.len(),
        summary.join(", ")
    ));

    selected
}

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

    log.push(format!(
        "正在对 {} 个下行候选节点执行并发测速预筛选（强制 HTTP/1.1 独立 TCP 多流）...",
        urls.len()
    ));

    let nodes = probe_and_rank_urls(urls, &log, &cancel).await;
    if cancel.is_cancelled() {
        return;
    }
    if nodes.is_empty() {
        log.push("错误：所有下行候选节点预探测均不可用，请检查网络或自定义测速 URL。");
        return;
    }

    let concurrency = config.stream_concurrency.clamp(1, 256);
    log.push(format!(
        "开始持续下行带宽压测：{concurrency} 路独立 HTTP/1.1 TCP 流（仅内存计数并立即丢弃数据）"
    ));

    let nodes = Arc::new(nodes);
    let last_err_log_ms = Arc::new(AtomicU64::new(0));
    let nonce_counter = Arc::new(AtomicU64::new(1));
    let start_instant = tokio::time::Instant::now();
    let mut workers = JoinSet::new();

    for worker_id in 0..concurrency {
        let nodes = nodes.clone();
        let stats = stats.clone();
        let log = log.clone();
        let cancel = cancel.clone();
        let last_err_log_ms = last_err_log_ms.clone();
        let nonce_counter = nonce_counter.clone();

        workers.spawn(async move {
            let mut node_idx = worker_id % nodes.len();
            // Cache one dedicated HTTP/1.1 client per (node_idx) in this worker so each worker
            // maintains persistent keep-alive TCP connections pinned to distinct edge IPs.
            let mut clients: Vec<Option<reqwest::Client>> = (0..nodes.len())
                .map(|i| {
                    let n = &nodes[i];
                    let pinned = if n.edge_addrs.is_empty() {
                        None
                    } else {
                        let addr = n.edge_addrs[worker_id % n.edge_addrs.len()];
                        Some((n.host.as_str(), addr))
                    };
                    build_http1_client(pinned, 5)
                })
                .collect();

            while !cancel.is_cancelled() {
                let node = &nodes[node_idx];
                let Some(client) = clients[node_idx].as_ref() else {
                    node_idx = (node_idx + 1) % nodes.len();
                    continue;
                };

                let nonce = nonce_counter.fetch_add(1, Ordering::Relaxed);
                let req_url = cache_busted_url(&node.url, nonce);

                let resp_res = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    res = client.get(&req_url).send() => res,
                };

                let mut resp = match resp_res {
                    Ok(r) => r,
                    Err(e) => {
                        stats.down_errors.fetch_add(1, Ordering::Relaxed);
                        log_throttled(
                            &log,
                            &last_err_log_ms,
                            start_instant,
                            format!("下行请求失败 [{}]: {e}", node.host),
                        );
                        node_idx = (node_idx + 1) % nodes.len();
                        sleep_or_cancel(&cancel, Duration::from_millis(250)).await;
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
                        format!("下行 HTTP 状态异常 [{status}] [{}]", node.host),
                    );
                    node_idx = (node_idx + 1) % nodes.len();
                    sleep_or_cancel(&cancel, Duration::from_millis(250)).await;
                    continue;
                }

                stats.down_active_streams.fetch_add(1, Ordering::Relaxed);
                let mut stalled = false;
                loop {
                    let chunk_res = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            stats.down_active_streams.fetch_sub(1, Ordering::Relaxed);
                            return;
                        }
                        chunk = tokio::time::timeout(CHUNK_STALL_TIMEOUT, resp.chunk()) => chunk,
                    };

                    match chunk_res {
                        Ok(Ok(Some(bytes))) => {
                            stats
                                .down_bytes_total
                                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        }
                        Ok(Ok(None)) => break,
                        Ok(Err(e)) => {
                            stats.down_errors.fetch_add(1, Ordering::Relaxed);
                            log_throttled(
                                &log,
                                &last_err_log_ms,
                                start_instant,
                                format!("下行读取中断 [{}]: {e}", node.host),
                            );
                            stalled = true;
                            break;
                        }
                        Err(_) => {
                            stats.down_errors.fetch_add(1, Ordering::Relaxed);
                            log_throttled(
                                &log,
                                &last_err_log_ms,
                                start_instant,
                                format!("下行流卡顿超时，自动切换节点 [{}]", node.host),
                            );
                            stalled = true;
                            break;
                        }
                    }
                }
                stats.down_active_streams.fetch_sub(1, Ordering::Relaxed);

                if stalled {
                    // Rebuild client to drop stalled socket and rotate to next fast node
                    let n = &nodes[node_idx];
                    let pinned = if n.edge_addrs.is_empty() {
                        None
                    } else {
                        let addr = n.edge_addrs[(worker_id + nonce as usize) % n.edge_addrs.len()];
                        Some((n.host.as_str(), addr))
                    };
                    clients[node_idx] = build_http1_client(pinned, 5);
                    node_idx = (node_idx + 1) % nodes.len();
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_host_port() {
        assert_eq!(
            extract_host_port("https://test.nju.edu.cn/backend/garbage.php?ckSize=100"),
            Some(("test.nju.edu.cn".to_string(), 443))
        );
        assert_eq!(
            extract_host_port("http://speed.neu.edu.cn:8080/fastgarbage.php"),
            Some(("speed.neu.edu.cn".to_string(), 8080))
        );
        assert_eq!(extract_host_port("ftp://invalid"), None);
    }

    #[test]
    fn test_cache_busted_url() {
        assert!(
            cache_busted_url("https://test.nju.edu.cn/backend/garbage.php?ckSize=100", 42)
                .ends_with("&r=42")
        );
        assert_eq!(
            cache_busted_url("https://mirrors.aliyun.com/archlinux/iso/latest/archlinux-x86_64.iso", 42),
            "https://mirrors.aliyun.com/archlinux/iso/latest/archlinux-x86_64.iso"
        );
    }
}
