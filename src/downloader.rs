//! High-accuracy, multi-WAN HTTP/HTTPS download bandwidth tester.
//! Key capabilities:
//! 1. Separates TCP connect, TLS handshake, TTFB, and pure data-transfer time.
//! 2. Multi-probe median node evaluation using pure transfer duration only.
//! 3. HTTP Range segmented download with automatic fallback to normal GET.
//! 4. Weighted dynamic worker allocation, faulty node quarantine, and client reuse.
//! 5. Auto concurrency optimization across [1, 2, 4, 8, 16, 32, 64] tiers & multi-IP dispersion.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::net::{TcpStream, lookup_host};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use crate::conn_tester::CancelToken;
use crate::state::{
    CONCURRENCY_TIERS, IpEndpointMetrics, MemoryLog, NodeSpeedMetrics, SharedStats, TestConfig,
    compute_median, compute_p95, format_bytes, format_mbps,
};

const PROBE_ROUNDS: usize = 3;
const PROBE_WINDOW: Duration = Duration::from_millis(1200);
const PROBE_BYTE_CAP: usize = 6 * 1024 * 1024;
const CHUNK_STALL_TIMEOUT: Duration = Duration::from_secs(8);
const RANGE_SEGMENT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone)]
struct DownloadNode {
    url: String,
    host: String,
    edge_addrs: Vec<SocketAddr>,
    total_size: u64,
    range_cursor: Arc<AtomicU64>,
    metrics: Arc<NodeSpeedMetrics>,
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

fn is_dynamic_stream_url(url: &str) -> bool {
    url.contains("garbage.php") || url.contains("fastgarbage.php")
}

/// Parse `bytes 0-65535/1234567890` from an HTTP Content-Range header.
pub fn parse_content_range_total(header_val: &str) -> Option<u64> {
    let (_, total_str) = header_val.trim().rsplit_once('/')?;
    if total_str == "*" {
        return None;
    }
    total_str.trim().parse::<u64>().ok()
}

async fn resolve_edge_ips(host: &str, port: u16) -> Vec<SocketAddr> {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    if let Ok(Ok(addrs)) =
        tokio::time::timeout(Duration::from_secs(3), lookup_host((host, port))).await
    {
        for addr in addrs {
            if addr.is_ipv4() {
                if v4.len() < 8 && !v4.contains(&addr) {
                    v4.push(addr);
                }
            } else if v6.len() < 4 && !v6.contains(&addr) {
                v6.push(addr);
            }
        }
    }
    if v4.is_empty() { v6 } else { v4 }
}

fn build_http1_client(
    pinned: Option<(&str, SocketAddr)>,
    connect_secs: u64,
) -> Option<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .http1_only()
        .connect_timeout(Duration::from_secs(connect_secs))
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(15))
        .pool_max_idle_per_host(2)
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) WinNetStress/0.4");

    if let Some((host, addr)) = pinned {
        builder = builder.resolve(host, addr);
    }

    builder.build().ok()
}

async fn probe_and_rank_urls(
    urls: Vec<String>,
    log: &MemoryLog,
    cancel: &CancelToken,
) -> Vec<DownloadNode> {
    let mut set = JoinSet::new();

    for (idx, url) in urls.into_iter().enumerate() {
        set.spawn(async move {
            let Some((host, port)) = extract_host_port(&url) else {
                return None;
            };
            let is_https = url.starts_with("https://");
            let edge_addrs = resolve_edge_ips(&host, port).await;
            if edge_addrs.is_empty() {
                return None;
            }

            // 1. Measure raw TCP handshake latency
            let t_tcp = Instant::now();
            let tcp_ok = tokio::time::timeout(
                Duration::from_millis(1800),
                TcpStream::connect(edge_addrs[0]),
            )
            .await;
            let tcp_ms = match tcp_ok {
                Ok(Ok(s)) => {
                    drop(s);
                    t_tcp.elapsed().as_millis().max(1) as u64
                }
                _ => return None,
            };

            let pinned = Some((host.as_str(), edge_addrs[0]));
            let client = build_http1_client(pinned, 3)?;

            let mut probe_bps_samples = Vec::with_capacity(PROBE_ROUNDS);
            let mut cold_hdr_ms = 0u64;
            let mut warm_ttfb_samples = Vec::with_capacity(PROBE_ROUNDS);
            let mut range_supported = false;
            let mut total_size = 0u64;

            // 2. Run PROBE_ROUNDS independent timing + transfer probes
            for round in 0..PROBE_ROUNDS {
                let probe_url = cache_busted_url(&url, 1000 + (idx as u64) * 10 + round as u64);
                let req_builder = if round == 0 && !is_dynamic_stream_url(&url) {
                    client
                        .get(&probe_url)
                        .header(reqwest::header::RANGE, "bytes=0-4194303")
                } else {
                    client.get(&probe_url)
                };

                let t_hdr = Instant::now();
                let mut resp =
                    match tokio::time::timeout(Duration::from_secs(4), req_builder.send()).await {
                        Ok(Ok(r)) if r.status().is_success() => r,
                        _ => continue,
                    };
                let hdr_ms = t_hdr.elapsed().as_millis().max(1) as u64;
                if round == 0 {
                    cold_hdr_ms = hdr_ms;
                    if resp.status().as_u16() == 206 {
                        if let Some(cr) = resp
                            .headers()
                            .get(reqwest::header::CONTENT_RANGE)
                            .and_then(|v| v.to_str().ok())
                            .and_then(parse_content_range_total)
                        {
                            if cr >= RANGE_SEGMENT_BYTES {
                                range_supported = true;
                                total_size = cr;
                            }
                        }
                    }
                } else {
                    warm_ttfb_samples.push(hdr_ms);
                }

                // Pure data transfer timer starts ONLY AFTER response headers arrive!
                let t_transfer = Instant::now();
                let mut bytes_read = 0usize;
                let deadline = t_transfer + PROBE_WINDOW;
                while Instant::now() < deadline && bytes_read < PROBE_BYTE_CAP {
                    let rem = deadline.saturating_duration_since(Instant::now());
                    match tokio::time::timeout(rem, resp.chunk()).await {
                        Ok(Ok(Some(b))) => bytes_read += b.len(),
                        _ => break,
                    }
                }

                let transfer_ns = t_transfer.elapsed().as_nanos().max(1) as f64;
                if bytes_read > 0 {
                    let bps = ((bytes_read as f64) * 8.0 * 1_000_000_000.0 / transfer_ns) as u64;
                    probe_bps_samples.push(bps);
                }
            }

            if probe_bps_samples.is_empty() {
                return None;
            }

            let median_bps = compute_median(&probe_bps_samples);
            let warm_ttfb = if warm_ttfb_samples.is_empty() {
                cold_hdr_ms.saturating_sub(tcp_ms)
            } else {
                compute_median(&warm_ttfb_samples)
            };
            let tls_ms = if is_https {
                cold_hdr_ms.saturating_sub(tcp_ms + warm_ttfb)
            } else {
                0
            };

            let metrics = Arc::new(NodeSpeedMetrics::new(
                host.clone(),
                url.clone(),
                median_bps,
                tcp_ms,
                tls_ms,
                warm_ttfb,
                range_supported,
                &edge_addrs,
            ));

            Some(DownloadNode {
                url,
                host,
                edge_addrs,
                total_size,
                range_cursor: Arc::new(AtomicU64::new(0)),
                metrics,
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

    ranked.sort_by_key(|n| std::cmp::Reverse(n.metrics.probe_bps.load(Ordering::Relaxed)));
    if ranked.is_empty() {
        return ranked;
    }

    let best_bps = ranked[0].metrics.probe_bps.load(Ordering::Relaxed);
    let floor_bps = best_bps / 5;
    let mut selected: Vec<DownloadNode> = ranked
        .iter()
        .enumerate()
        .filter(|(i, n)| *i < 4 || n.metrics.probe_bps.load(Ordering::Relaxed) >= floor_bps)
        .map(|(_, n)| n.clone())
        .collect();
    selected.truncate(6);

    let summary: Vec<String> = selected
        .iter()
        .map(|n| {
            let m = &n.metrics;
            let rng = if m.range_supported.load(Ordering::Relaxed) {
                "Range"
            } else {
                "GET"
            };
            format!(
                "{}({}·{rng}·TCP{}ms/TLS{}ms/TTFB{}ms)",
                n.host,
                format_mbps(m.probe_bps.load(Ordering::Relaxed)),
                m.tcp_connect_ms.load(Ordering::Relaxed),
                m.tls_handshake_ms.load(Ordering::Relaxed),
                m.ttfb_ms.load(Ordering::Relaxed),
            )
        })
        .collect();
    log.push(format!(
        "下行中位数预探测优选 {} 个节点: {}",
        selected.len(),
        summary.join(", ")
    ));

    selected
}

fn select_weighted_download_node(nodes: &[DownloadNode], cycle: usize) -> usize {
    let mut total_w = 0u64;
    let mut active_indices = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        if !n.metrics.quarantined.load(Ordering::Relaxed) {
            let w = n.metrics.weight.load(Ordering::Relaxed).max(1) as u64;
            total_w += w;
            active_indices.push((i, w));
        }
    }
    if active_indices.is_empty() {
        for n in nodes {
            n.metrics.quarantined.store(false, Ordering::Relaxed);
            n.metrics.consecutive_fails.store(0, Ordering::Relaxed);
        }
        return cycle % nodes.len();
    }
    let pick = ((cycle as u64).wrapping_mul(2_654_435_761) >> 16) % total_w;
    let mut acc = 0u64;
    for (idx, w) in active_indices {
        acc += w;
        if pick < acc {
            return idx;
        }
    }
    0
}

fn refresh_download_weights(nodes: &[DownloadNode]) {
    let best_ema = nodes
        .iter()
        .filter(|n| !n.metrics.quarantined.load(Ordering::Relaxed))
        .map(|n| n.metrics.ema_bps.load(Ordering::Relaxed))
        .max()
        .unwrap_or(1)
        .max(1);

    for n in nodes {
        let ema = n.metrics.ema_bps.load(Ordering::Relaxed);
        let pct = n.metrics.success_rate_pct();
        // Slow nodes (<15% of best) receive minimal weight 1; fast stable nodes get 10..100
        let rel = ((ema * 90) / best_ema).clamp(1, 90);
        let w = ((rel as f64) * (pct / 100.0)).round().clamp(1.0, 100.0) as u32;
        n.metrics.weight.store(w, Ordering::Relaxed);
    }
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
        "正在对 {} 个下行节点执行 3 轮中位数测速（分离 TCP / TLS / TTFB / 纯传输时间 + Range 检测）...",
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

    *stats.down_node_metrics.lock() = nodes.iter().map(|n| n.metrics.clone()).collect();
    stats.down_tcp_ms.store(
        nodes[0].metrics.tcp_connect_ms.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    stats.down_tls_ms.store(
        nodes[0].metrics.tls_handshake_ms.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    stats.down_ttfb_ms.store(
        nodes[0].metrics.ttfb_ms.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );

    let max_workers = 64usize;
    let initial_conc = if config.auto_concurrency {
        CONCURRENCY_TIERS[0]
    } else {
        config.down_concurrency.clamp(1, 64)
    };

    let target_conc = Arc::new(AtomicUsize::new(initial_conc));
    let conc_notify = Arc::new(Notify::new());
    stats
        .down_current_concurrency
        .store(initial_conc, Ordering::Relaxed);
    stats
        .down_best_concurrency
        .store(initial_conc, Ordering::Relaxed);

    let dur_desc = if config.duration_secs == 0 {
        "持续运行（手动停止）".to_string()
    } else {
        format!("{} 秒（前 {} 秒预热）", config.duration_secs, config.warmup_secs)
    };
    log.push(format!(
        "开始下行带宽压测：自动并发寻优={}，初始并发={initial_conc}，时长={dur_desc}（纯内存丢弃，不落盘）",
        if config.auto_concurrency { "开启(1~64)" } else { "关闭" }
    ));

    let nodes = Arc::new(nodes);
    let last_err_log_ms = Arc::new(AtomicU64::new(0));
    let nonce_counter = Arc::new(AtomicU64::new(1));
    let start_instant = tokio::time::Instant::now();
    let test_cancel = Arc::new(CancelToken::new());

    // Coordinator task: duration timer, adaptive concurrency search, dynamic node weighting, valid-window stats
    let coord_cancel = cancel.clone();
    let coord_test_cancel = test_cancel.clone();
    let coord_stats = stats.clone();
    let coord_nodes = nodes.clone();
    let coord_target_conc = target_conc.clone();
    let coord_notify = conc_notify.clone();
    let coord_log = log.clone();
    let auto_conc = config.auto_concurrency;
    let duration_secs = config.duration_secs;
    let warmup_secs = config.warmup_secs;

    let coord_task = tokio::spawn(async move {
        let mut tier_idx = 0usize;
        let mut tier_started_at = Instant::now();
        let mut tier_start_bytes = 0u64;
        let mut tier_start_ok = 0u64;
        let mut tier_start_fail = 0u64;
        let mut best_tier = initial_conc;
        let mut best_tier_bps = 0u64;
        let mut search_done = !auto_conc;

        let mut warmup_captured = warmup_secs == 0;
        let mut valid_start_bytes = 0u64;
        let mut valid_start_time = Instant::now();
        let mut valid_samples: Vec<u64> = Vec::new();
        let mut last_sample_bytes = 0u64;
        let mut last_sample_time = Instant::now();

        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                biased;
                _ = coord_cancel.cancelled() => {
                    coord_test_cancel.cancel();
                    break;
                }
                _ = tick.tick() => {}
            }

            let elapsed_s = start_instant.elapsed().as_secs();
            if duration_secs > 0 && elapsed_s >= duration_secs {
                coord_log.push(format!("下行测速达到设定时长 {duration_secs} 秒，正在汇总报告..."));
                coord_test_cancel.cancel();
                break;
            }

            let now_bytes = coord_stats.down_bytes_total.load(Ordering::Relaxed);
            let now_time = Instant::now();

            if !warmup_captured && elapsed_s >= warmup_secs {
                warmup_captured = true;
                valid_start_bytes = now_bytes;
                valid_start_time = now_time;
            }

            let dt = now_time.duration_since(last_sample_time).as_secs_f64();
            if dt > 0.05 {
                let delta_b = now_bytes.saturating_sub(last_sample_bytes);
                let inst_bps = ((delta_b as f64) * 8.0 / dt) as u64;
                last_sample_bytes = now_bytes;
                last_sample_time = now_time;
                if warmup_captured {
                    valid_samples.push(inst_bps);
                    let valid_dt = now_time.duration_since(valid_start_time).as_secs_f64();
                    if valid_dt > 0.2 {
                        let valid_bytes = now_bytes.saturating_sub(valid_start_bytes);
                        let avg_bps = ((valid_bytes as f64) * 8.0 / valid_dt) as u64;
                        coord_stats.down_bps_avg.store(avg_bps, Ordering::Relaxed);
                    }
                    coord_stats
                        .down_bps_p95
                        .store(compute_p95(&valid_samples), Ordering::Relaxed);
                }
            }

            refresh_download_weights(&coord_nodes);

            if !search_done {
                let tier_dur = tier_started_at.elapsed().as_secs_f64();
                if tier_dur >= 2.5 {
                    let cur_tier = CONCURRENCY_TIERS[tier_idx];
                    let tier_bytes = now_bytes.saturating_sub(tier_start_bytes);
                    let tier_bps = ((tier_bytes as f64) * 8.0 / tier_dur) as u64;

                    let ok_now = coord_stats.down_requests_ok.load(Ordering::Relaxed);
                    let fail_now = coord_stats.down_requests_fail.load(Ordering::Relaxed);
                    let d_ok = ok_now.saturating_sub(tier_start_ok);
                    let d_fail = fail_now.saturating_sub(tier_start_fail);
                    let fail_pct = if d_ok + d_fail > 0 {
                        (d_fail as f64) * 100.0 / ((d_ok + d_fail) as f64)
                    } else {
                        0.0
                    };

                    let improved = tier_bps > (best_tier_bps * 105) / 100 || best_tier_bps == 0;
                    if fail_pct <= 18.0 && (improved || tier_bps >= best_tier_bps) {
                        best_tier = cur_tier;
                        best_tier_bps = tier_bps;
                        coord_stats
                            .down_best_concurrency
                            .store(best_tier, Ordering::Relaxed);
                    }

                    let should_stop_climbing = fail_pct > 20.0
                        || (tier_idx >= 2 && !improved)
                        || tier_idx + 1 >= CONCURRENCY_TIERS.len();

                    if should_stop_climbing {
                        search_done = true;
                        coord_target_conc.store(best_tier, Ordering::Release);
                        coord_stats
                            .down_current_concurrency
                            .store(best_tier, Ordering::Relaxed);
                        coord_stats
                            .down_best_concurrency
                            .store(best_tier, Ordering::Relaxed);
                        coord_notify.notify_waiters();
                        coord_log.push(format!(
                            "下行自动寻优完成：最佳稳定并发={best_tier} (测得有效速率 {}, 失败率 {fail_pct:.1}%)",
                            format_mbps(best_tier_bps)
                        ));
                    } else {
                        tier_idx += 1;
                        let next_tier = CONCURRENCY_TIERS[tier_idx];
                        coord_target_conc.store(next_tier, Ordering::Release);
                        coord_stats
                            .down_current_concurrency
                            .store(next_tier, Ordering::Relaxed);
                        coord_notify.notify_waiters();
                        tier_started_at = Instant::now();
                        tier_start_bytes = now_bytes;
                        tier_start_ok = ok_now;
                        tier_start_fail = fail_now;
                        coord_log.push(format!(
                            "下行并发寻优：档位 {cur_tier}={} -> 尝试提升至 {next_tier}",
                            format_mbps(tier_bps)
                        ));
                    }
                }
            }
        }
    });

    let mut workers = JoinSet::new();
    for worker_id in 0..max_workers {
        let nodes = nodes.clone();
        let stats = stats.clone();
        let log = log.clone();
        let cancel = test_cancel.clone();
        let target_conc = target_conc.clone();
        let conc_notify = conc_notify.clone();
        let last_err_log_ms = last_err_log_ms.clone();
        let nonce_counter = nonce_counter.clone();

        workers.spawn(async move {
            let mut client_cache: HashMap<(usize, SocketAddr), reqwest::Client> = HashMap::new();
            let mut cycle = worker_id;

            while !cancel.is_cancelled() {
                while worker_id >= target_conc.load(Ordering::Acquire) {
                    if cancel.is_cancelled() {
                        return;
                    }
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return,
                        _ = conc_notify.notified() => {}
                    }
                }

                cycle = cycle.wrapping_add(1);
                let node_idx = select_weighted_download_node(&nodes, cycle);
                let node = &nodes[node_idx];

                let chosen_ip: Option<Arc<IpEndpointMetrics>> = node.metrics.pick_ip();
                let pinned_addr = chosen_ip.as_ref().map(|ip| ip.addr);

                let client_key = (
                    node_idx,
                    pinned_addr.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0))),
                );
                let client = if let Some(c) = client_cache.get(&client_key) {
                    c.clone()
                } else {
                    let Some(c) = build_http1_client(
                        pinned_addr.map(|a| (node.host.as_str(), a)),
                        5,
                    ) else {
                        sleep_or_cancel(&cancel, Duration::from_millis(200)).await;
                        continue;
                    };
                    client_cache.insert(client_key, c.clone());
                    c
                };

                let nonce = nonce_counter.fetch_add(1, Ordering::Relaxed);
                let req_url = cache_busted_url(&node.url, nonce);

                let use_range =
                    node.metrics.range_supported.load(Ordering::Relaxed) && node.total_size >= RANGE_SEGMENT_BYTES;
                let mut req = client.get(&req_url);
                if use_range {
                    let seg_start = node
                        .range_cursor
                        .fetch_add(RANGE_SEGMENT_BYTES, Ordering::Relaxed)
                        % node.total_size;
                    let seg_end = (seg_start + RANGE_SEGMENT_BYTES - 1).min(node.total_size - 1);
                    req = req.header(
                        reqwest::header::RANGE,
                        format!("bytes={seg_start}-{seg_end}"),
                    );
                }

                if let Some(ip) = &chosen_ip {
                    ip.active_conns.fetch_add(1, Ordering::Relaxed);
                    ip.total_conns.fetch_add(1, Ordering::Relaxed);
                }

                let t_req = Instant::now();
                let resp_res = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        if let Some(ip) = &chosen_ip {
                            ip.active_conns.fetch_sub(1, Ordering::Relaxed);
                        }
                        break;
                    }
                    res = req.send() => res,
                };

                let mut resp = match resp_res {
                    Ok(r) => r,
                    Err(e) => {
                        if let Some(ip) = &chosen_ip {
                            ip.active_conns.fetch_sub(1, Ordering::Relaxed);
                            ip.errors.fetch_add(1, Ordering::Relaxed);
                        }
                        stats.down_requests_fail.fetch_add(1, Ordering::Relaxed);
                        stats.down_errors.fetch_add(1, Ordering::Relaxed);
                        node.metrics.record_failure();
                        client_cache.remove(&client_key);
                        log_throttled(
                            &log,
                            &last_err_log_ms,
                            start_instant,
                            format!("下行请求失败 [{}]: {e}", node.host),
                        );
                        sleep_or_cancel(&cancel, Duration::from_millis(200)).await;
                        continue;
                    }
                };

                let ttfb = t_req.elapsed().as_millis().max(1) as u64;
                let status = resp.status();
                if use_range && status.as_u16() == 200 {
                    // Server ignored Range header; automatically fall back to normal GET
                    node.metrics.range_supported.store(false, Ordering::Relaxed);
                } else if use_range && status.as_u16() == 416 {
                    node.range_cursor.store(0, Ordering::Relaxed);
                    if let Some(ip) = &chosen_ip {
                        ip.active_conns.fetch_sub(1, Ordering::Relaxed);
                    }
                    continue;
                } else if !status.is_success() {
                    if let Some(ip) = &chosen_ip {
                        ip.active_conns.fetch_sub(1, Ordering::Relaxed);
                        ip.errors.fetch_add(1, Ordering::Relaxed);
                    }
                    stats.down_requests_fail.fetch_add(1, Ordering::Relaxed);
                    stats.down_errors.fetch_add(1, Ordering::Relaxed);
                    node.metrics.record_failure();
                    log_throttled(
                        &log,
                        &last_err_log_ms,
                        start_instant,
                        format!("下行 HTTP 状态异常 [{status}] [{}]", node.host),
                    );
                    sleep_or_cancel(&cancel, Duration::from_millis(200)).await;
                    continue;
                }

                stats.down_ttfb_ms.store(ttfb, Ordering::Relaxed);
                node.metrics.ttfb_ms.store(ttfb, Ordering::Relaxed);

                stats.down_active_streams.fetch_add(1, Ordering::Relaxed);
                node.metrics.active_workers.fetch_add(1, Ordering::Relaxed);

                // Measure pure data transfer duration separately from TCP/TLS/TTFB
                let t_transfer = Instant::now();
                let mut req_bytes = 0u64;
                let mut stalled = false;

                loop {
                    let chunk_res = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            stats.down_active_streams.fetch_sub(1, Ordering::Relaxed);
                            node.metrics.active_workers.fetch_sub(1, Ordering::Relaxed);
                            if let Some(ip) = &chosen_ip {
                                ip.active_conns.fetch_sub(1, Ordering::Relaxed);
                            }
                            return;
                        }
                        chunk = tokio::time::timeout(CHUNK_STALL_TIMEOUT, resp.chunk()) => chunk,
                    };

                    match chunk_res {
                        Ok(Ok(Some(bytes))) => {
                            let n = bytes.len() as u64;
                            req_bytes += n;
                            stats.down_bytes_total.fetch_add(n, Ordering::Relaxed);
                            if let Some(ip) = &chosen_ip {
                                ip.bytes_transferred.fetch_add(n, Ordering::Relaxed);
                            }
                        }
                        Ok(Ok(None)) => break,
                        Ok(Err(e)) => {
                            stats.down_requests_fail.fetch_add(1, Ordering::Relaxed);
                            stats.down_errors.fetch_add(1, Ordering::Relaxed);
                            node.metrics.record_failure();
                            if let Some(ip) = &chosen_ip {
                                ip.errors.fetch_add(1, Ordering::Relaxed);
                            }
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
                            stats.down_requests_fail.fetch_add(1, Ordering::Relaxed);
                            stats.down_errors.fetch_add(1, Ordering::Relaxed);
                            node.metrics.record_failure();
                            if let Some(ip) = &chosen_ip {
                                ip.errors.fetch_add(1, Ordering::Relaxed);
                            }
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

                let transfer_ms = t_transfer.elapsed().as_millis().max(1) as u64;
                stats.down_active_streams.fetch_sub(1, Ordering::Relaxed);
                node.metrics.active_workers.fetch_sub(1, Ordering::Relaxed);
                if let Some(ip) = &chosen_ip {
                    ip.active_conns.fetch_sub(1, Ordering::Relaxed);
                }

                if !stalled && req_bytes > 0 {
                    stats.down_requests_ok.fetch_add(1, Ordering::Relaxed);
                    stats
                        .down_transfer_ms
                        .fetch_add(transfer_ms, Ordering::Relaxed);
                    node.metrics
                        .transfer_ms_total
                        .fetch_add(transfer_ms, Ordering::Relaxed);
                    node.metrics.record_success(req_bytes, transfer_ms);
                    if let Some(ip) = &chosen_ip {
                        let ip_bps =
                            ((req_bytes as f64) * 8.0 * 1000.0 / (transfer_ms as f64)) as u64;
                        ip.current_bps.store(ip_bps, Ordering::Relaxed);
                    }
                } else if stalled {
                    client_cache.remove(&client_key);
                }
            }
        });
    }

    let _ = coord_task.await;
    while workers.join_next().await.is_some() {}
    stats.down_active_streams.store(0, Ordering::Relaxed);

    let total = stats.down_bytes_total.load(Ordering::Relaxed);
    let avg = stats.down_bps_avg.load(Ordering::Relaxed);
    let peak = stats.down_bps_peak.load(Ordering::Relaxed);
    let p95 = stats.down_bps_p95.load(Ordering::Relaxed);
    let best_c = stats.down_best_concurrency.load(Ordering::Relaxed);
    let fail_pct = stats.down_fail_rate_pct();

    log.push(format!(
        "下行测速结束：有效平均 {} | 峰值 {} | P95 {} | 累计下载 {} | 最佳并发 {best_c} | 失败率 {fail_pct:.1}%",
        format_mbps(avg),
        format_mbps(peak),
        format_mbps(p95),
        format_bytes(total),
    ));
    log_download_node_report(&nodes, &log);
}

fn log_download_node_report(nodes: &[DownloadNode], log: &MemoryLog) {
    log.push("[下行节点与多 IP 分布明细]:");
    for n in nodes {
        let m = &n.metrics;
        let ema = m.ema_bps.load(Ordering::Relaxed);
        let ok = m.requests_ok.load(Ordering::Relaxed);
        let fail = m.requests_fail.load(Ordering::Relaxed);
        let tcp = m.tcp_connect_ms.load(Ordering::Relaxed);
        let tls = m.tls_handshake_ms.load(Ordering::Relaxed);
        let ttfb = m.ttfb_ms.load(Ordering::Relaxed);
        let bytes = m.bytes_confirmed.load(Ordering::Relaxed);
        let mode = if m.range_supported.load(Ordering::Relaxed) {
            "Range分段"
        } else {
            "普通GET"
        };
        let q = if m.quarantined.load(Ordering::Relaxed) {
            " [已隔离]"
        } else {
            ""
        };
        log.push(format!(
            "  · {}{q} | 速率:{} | 已收:{} | {mode} | 成/败:{ok}/{fail} | TCP:{tcp}ms/TLS:{tls}ms/TTFB:{ttfb}ms",
            m.host,
            format_mbps(ema),
            format_bytes(bytes),
        ));
        for ip in &m.ip_metrics {
            let tc = ip.total_conns.load(Ordering::Relaxed);
            if tc > 0 {
                let ib = ip.bytes_transferred.load(Ordering::Relaxed);
                let ibps = ip.current_bps.load(Ordering::Relaxed);
                let ie = ip.errors.load(Ordering::Relaxed);
                log.push(format!(
                    "      IP {} -> 连接:{tc} | 已下载:{} | 速率:{} | 错误:{ie}",
                    ip.addr,
                    format_bytes(ib),
                    format_mbps(ibps)
                ));
            }
        }
    }
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
            cache_busted_url(
                "https://mirrors.aliyun.com/archlinux/iso/latest/archlinux-x86_64.iso",
                42
            ),
            "https://mirrors.aliyun.com/archlinux/iso/latest/archlinux-x86_64.iso"
        );
    }

    #[test]
    fn test_parse_content_range_total() {
        assert_eq!(
            parse_content_range_total("bytes 0-4194303/987654321"),
            Some(987_654_321)
        );
        assert_eq!(parse_content_range_total("bytes 0-1023/*"), None);
    }
}
