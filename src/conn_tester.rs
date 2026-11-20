//! Router-WAN-style multi-target concurrent TCP session prober (ConnTester).
//! Principles:
//! 1. Pre-flight TCP health check per configured target before entering active pool.
//! 2. Per-target independent metrics & multi-WAN dispersion across verified IP:port endpoints.
//! 3. Strict 4-way connection attribution: local failure, remote refused, remote closed, timeout.
//! 4. Strict separation of original attempts/success vs. replenishment attempts/success
//!    (replenishment never masks original failure rates).
//! 5. 3-tier result grading: instant burst, stable step concurrency, and long-duration hold.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use socket2::{SockRef, TcpKeepalive};
use tokio::net::{TcpSocket, TcpStream, lookup_host};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;

use crate::state::{MemoryLog, SharedStats, TargetMetrics, TestConfig};

const MAX_CONCURRENT_HANDSHAKES: usize = 256;
const UNLIMITED_SAFETY_CEILING: usize = 100_000;
const FINITE_LONG_HOLD_SECS: u64 = 12;

pub struct CancelToken {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    pub fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::Release) {
            self.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectErrorBucket {
    LocalFailure,
    RemoteRefused,
    Timeout,
}

pub fn is_local_os_error(err: &std::io::Error) -> bool {
    if let Some(code) = err.raw_os_error() {
        // Winsock2 + POSIX local resource / routing / socket permission errors
        if matches!(
            code,
            10013 | 10022 | 10024 | 10048 | 10049 | 10050 | 10051 | 10055 | 13 | 24 | 98 | 99 | 101
                | 105
        ) {
            return true;
        }
    }
    matches!(
        err.kind(),
        ErrorKind::AddrInUse
            | ErrorKind::AddrNotAvailable
            | ErrorKind::PermissionDenied
            | ErrorKind::NetworkUnreachable
            | ErrorKind::NetworkDown
    )
}

pub fn classify_connect_error(err: &std::io::Error) -> ConnectErrorBucket {
    if is_local_os_error(err) {
        return ConnectErrorBucket::LocalFailure;
    }
    if err.raw_os_error().is_some_and(|c| matches!(c, 10060 | 110))
        || err.kind() == ErrorKind::TimedOut
    {
        return ConnectErrorBucket::Timeout;
    }
    if err
        .raw_os_error()
        .is_some_and(|c| matches!(c, 10053 | 10054 | 10061 | 10064 | 10065 | 111 | 104 | 113))
        || matches!(
            err.kind(),
            ErrorKind::ConnectionRefused
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::HostUnreachable
        )
    {
        return ConnectErrorBucket::RemoteRefused;
    }
    ConnectErrorBucket::LocalFailure
}

fn record_connect_error(err: &std::io::Error, stats: &SharedStats, target: &TargetMetrics) {
    match classify_connect_error(err) {
        ConnectErrorBucket::LocalFailure => {
            stats.conn_fail_local.fetch_add(1, Ordering::Relaxed);
            target.fail_local.fetch_add(1, Ordering::Relaxed);
        }
        ConnectErrorBucket::RemoteRefused => {
            stats.conn_fail_refused.fetch_add(1, Ordering::Relaxed);
            target.fail_refused.fetch_add(1, Ordering::Relaxed);
        }
        ConnectErrorBucket::Timeout => {
            stats.conn_fail_timeout.fetch_add(1, Ordering::Relaxed);
            target.fail_timeout.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn update_rtt_ewma(stats: &SharedStats, sample_ms: u64) {
    let clamped = sample_ms.clamp(1, 10_000);
    let prev = stats.conn_rtt_ms.load(Ordering::Relaxed);
    let next = if prev == 0 {
        clamped
    } else {
        (prev * 7 + clamped) / 8
    };
    stats.conn_rtt_ms.store(next, Ordering::Relaxed);
}

#[derive(Clone)]
struct VerifiedTarget {
    addrs: Vec<SocketAddr>,
    metrics: Arc<TargetMetrics>,
}

/// Resolves each configured target independently, performs a TCP handshake +
/// brief stability check (confirms the peer actually supports TCP and doesn't
/// immediately RST), and returns verified healthy targets plus full target list.
async fn probe_and_verify_targets(
    targets: &[String],
    log: &MemoryLog,
    cancel: &CancelToken,
) -> (Vec<VerifiedTarget>, Vec<Arc<TargetMetrics>>, u64) {
    let mut unique_labels: Vec<String> = Vec::new();
    for raw in targets {
        let trimmed = raw.trim().trim_start_matches("tcp://").to_string();
        if !trimmed.is_empty() && !unique_labels.contains(&trimmed) {
            unique_labels.push(trimmed);
        }
    }

    let mut resolve_set = JoinSet::new();
    for (idx, label) in unique_labels.iter().cloned().enumerate() {
        resolve_set.spawn(async move {
            let res = tokio::time::timeout(Duration::from_secs(3), lookup_host(label.as_str()))
                .await
                .map(|r| r.map(|iter| iter.collect::<Vec<_>>()));
            (idx, label, res)
        });
    }

    let mut resolved_groups: Vec<Option<(String, Vec<SocketAddr>, Option<String>)>> =
        vec![None; unique_labels.len()];

    while let Some(joined) = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            resolve_set.abort_all();
            return (Vec::new(), Vec::new(), 0);
        }
        r = resolve_set.join_next() => r,
    } {
        if let Ok((idx, label, res)) = joined {
            match res {
                Ok(Ok(addrs)) => {
                    let mut v4 = Vec::new();
                    let mut v6 = Vec::new();
                    for addr in addrs {
                        if addr.is_ipv4() {
                            if v4.len() < 4 && !v4.contains(&addr) {
                                v4.push(addr);
                            }
                        } else if v6.len() < 2 && !v6.contains(&addr) {
                            v6.push(addr);
                        }
                    }
                    v4.extend(v6);
                    if v4.is_empty() {
                        resolved_groups[idx] =
                            Some((label, Vec::new(), Some("DNS 未返回可用 IP".to_string())));
                    } else {
                        resolved_groups[idx] = Some((label, v4, None));
                    }
                }
                Ok(Err(e)) => {
                    resolved_groups[idx] = Some((label, Vec::new(), Some(format!("DNS 解析失败: {e}"))));
                }
                Err(_) => {
                    resolved_groups[idx] = Some((label, Vec::new(), Some("DNS 解析超时".to_string())));
                }
            }
        }
    }

    // Probe TCP support for every resolved target in parallel
    let mut probe_set = JoinSet::new();
    let mut all_metrics: Vec<Option<Arc<TargetMetrics>>> = vec![None; unique_labels.len()];

    for (idx, group) in resolved_groups.iter().enumerate() {
        let Some((label, addrs, err_opt)) = group else {
            continue;
        };
        if let Some(reason) = err_opt {
            all_metrics[idx] = Some(Arc::new(TargetMetrics::unhealthy(
                label.clone(),
                reason.clone(),
            )));
            continue;
        }
        for &addr in addrs {
            probe_set.spawn(async move {
                let t0 = Instant::now();
                let res =
                    tokio::time::timeout(Duration::from_millis(1600), TcpStream::connect(addr))
                        .await;
                match res {
                    Ok(Ok(stream)) => {
                        let rtt = t0.elapsed().as_millis().max(1) as u64;
                        // Verify peer holds TCP open briefly without immediate RST
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        let mut buf = [0u8; 8];
                        let stable = match stream.try_read(&mut buf) {
                            Ok(0) => false,
                            Ok(_) => true,
                            Err(e) if e.kind() == ErrorKind::WouldBlock => true,
                            Err(_) => false,
                        };
                        drop(stream);
                        if stable {
                            (idx, addr, Ok(rtt))
                        } else {
                            (idx, addr, Err("握手后被远端立即重置/关闭"))
                        }
                    }
                    Ok(Err(e)) => {
                        let reason = match classify_connect_error(&e) {
                            ConnectErrorBucket::LocalFailure => "本机网络/路由不可达",
                            ConnectErrorBucket::RemoteRefused => "远端拒绝 TCP 连接",
                            ConnectErrorBucket::Timeout => "TCP 连接超时",
                        };
                        (idx, addr, Err(reason))
                    }
                    Err(_) => (idx, addr, Err("TCP 握手超时")),
                }
            });
        }
    }

    let mut verified_by_idx: Vec<Vec<(SocketAddr, u64)>> = vec![Vec::new(); unique_labels.len()];
    let mut last_err_by_idx: Vec<Option<&'static str>> = vec![None; unique_labels.len()];

    while let Some(joined) = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            probe_set.abort_all();
            return (Vec::new(), Vec::new(), 0);
        }
        r = probe_set.join_next() => r,
    } {
        if let Ok((idx, addr, outcome)) = joined {
            match outcome {
                Ok(rtt) => verified_by_idx[idx].push((addr, rtt)),
                Err(reason) => {
                    if last_err_by_idx[idx].is_none() {
                        last_err_by_idx[idx] = Some(reason);
                    }
                }
            }
        }
    }

    let mut verified_targets = Vec::new();
    let mut ordered_metrics = Vec::new();
    let mut rtt_sum = 0u64;
    let mut rtt_cnt = 0u64;

    for idx in 0..unique_labels.len() {
        if let Some(m) = &all_metrics[idx] {
            log.push(format!("[健康检查] 剔除 {} -> {}", m.label, m.health_note));
            ordered_metrics.push(m.clone());
            continue;
        }
        let label = unique_labels[idx].clone();
        let mut ok_addrs = std::mem::take(&mut verified_by_idx[idx]);
        if ok_addrs.is_empty() {
            let reason = last_err_by_idx[idx].unwrap_or("不支持 TCP 或端口不可达");
            let m = Arc::new(TargetMetrics::unhealthy(label.clone(), reason.to_string()));
            log.push(format!("[健康检查] 剔除 {label} -> {reason}"));
            ordered_metrics.push(m);
            continue;
        }

        // Prefer IPv4 first, sorted by probe RTT
        ok_addrs.sort_by_key(|(a, rtt)| (u8::from(!a.is_ipv4()), *rtt));
        let primary_addr = ok_addrs[0].0;
        let primary_rtt = ok_addrs[0].1;
        let addrs: Vec<SocketAddr> = ok_addrs.into_iter().map(|(a, _)| a).collect();

        rtt_sum += primary_rtt;
        rtt_cnt += 1;

        let m = Arc::new(TargetMetrics::healthy(
            label.clone(),
            primary_addr.to_string(),
            primary_rtt,
        ));
        log.push(format!(
            "[健康检查] 通过 {label} -> IP {primary_addr} (可用节点 {}, 基准延迟 {primary_rtt} ms)",
            addrs.len()
        ));
        ordered_metrics.push(m.clone());
        verified_targets.push(VerifiedTarget { addrs, metrics: m });
    }

    let baseline_rtt = if rtt_cnt > 0 { rtt_sum / rtt_cnt } else { 0 };
    (verified_targets, ordered_metrics, baseline_rtt)
}

#[allow(clippy::too_many_arguments)]
fn spawn_slot(
    slot_idx: usize,
    targets: Arc<Vec<VerifiedTarget>>,
    connect_timeout: Duration,
    limiter: Arc<Semaphore>,
    stats: Arc<SharedStats>,
    global_cancel: Arc<CancelToken>,
    slot_cancel: Arc<CancelToken>,
) {
    tokio::spawn(async move {
        let target_idx = slot_idx % targets.len();
        let target = &targets[target_idx];
        let mut attempt = 0usize;
        let mut is_replenish = false;
        let stagger_ms = ((slot_idx % 11) as u64) * 7;

        while !global_cancel.is_cancelled() && !slot_cancel.is_cancelled() {
            let permit = tokio::select! {
                biased;
                _ = global_cancel.cancelled() => break,
                _ = slot_cancel.cancelled() => break,
                p = limiter.acquire() => match p {
                    Ok(p) => p,
                    Err(_) => break,
                }
            };

            let cur_rtt = stats.conn_rtt_ms.load(Ordering::Relaxed);
            if cur_rtt >= 300 {
                let pace = ((cur_rtt - 250) / 15).clamp(10, 80);
                sleep_or_cancel2(&global_cancel, &slot_cancel, Duration::from_millis(pace)).await;
                if global_cancel.is_cancelled() || slot_cancel.is_cancelled() {
                    drop(permit);
                    break;
                }
            }

            let addr_idx = (slot_idx / targets.len()).wrapping_add(attempt) % target.addrs.len();
            let addr = target.addrs[addr_idx];
            attempt = attempt.wrapping_add(1);

            let dial_is_replenish = is_replenish;
            is_replenish = true;

            if dial_is_replenish {
                stats.conn_replenish_attempts.fetch_add(1, Ordering::Relaxed);
                target
                    .metrics
                    .replenish_attempts
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                stats.conn_orig_attempts.fetch_add(1, Ordering::Relaxed);
                target.metrics.orig_attempts.fetch_add(1, Ordering::Relaxed);
            }

            let socket = match if addr.is_ipv4() {
                TcpSocket::new_v4()
            } else {
                TcpSocket::new_v6()
            } {
                Ok(s) => s,
                Err(e) => {
                    drop(permit);
                    record_connect_error(&e, &stats, &target.metrics);
                    sleep_or_cancel2(
                        &global_cancel,
                        &slot_cancel,
                        Duration::from_millis(350 + stagger_ms),
                    )
                    .await;
                    continue;
                }
            };

            let _ = socket.set_recv_buffer_size(2048);
            let _ = socket.set_send_buffer_size(2048);

            let t0 = Instant::now();
            let connect_res = tokio::select! {
                biased;
                _ = global_cancel.cancelled() => {
                    drop(permit);
                    break;
                }
                _ = slot_cancel.cancelled() => {
                    drop(permit);
                    break;
                }
                res = tokio::time::timeout(connect_timeout, socket.connect(addr)) => res,
            };
            drop(permit);

            let stream = match connect_res {
                Ok(Ok(s)) => {
                    let rtt = t0.elapsed().as_millis().max(1) as u64;
                    update_rtt_ewma(&stats, rtt);
                    target.metrics.record_rtt(rtt);
                    s
                }
                Ok(Err(e)) => {
                    let local = classify_connect_error(&e) == ConnectErrorBucket::LocalFailure;
                    record_connect_error(&e, &stats, &target.metrics);
                    let backoff = if local {
                        500 + stagger_ms
                    } else {
                        140 + stagger_ms
                    };
                    sleep_or_cancel2(&global_cancel, &slot_cancel, Duration::from_millis(backoff))
                        .await;
                    continue;
                }
                Err(_) => {
                    stats.conn_fail_timeout.fetch_add(1, Ordering::Relaxed);
                    target.metrics.fail_timeout.fetch_add(1, Ordering::Relaxed);
                    sleep_or_cancel2(
                        &global_cancel,
                        &slot_cancel,
                        Duration::from_millis(180 + stagger_ms),
                    )
                    .await;
                    continue;
                }
            };

            let keepalive = TcpKeepalive::new()
                .with_time(Duration::from_secs(10))
                .with_interval(Duration::from_secs(3));
            let _ = SockRef::from(&stream).set_tcp_keepalive(&keepalive);

            if dial_is_replenish {
                stats.conn_replenish_success.fetch_add(1, Ordering::Relaxed);
                target
                    .metrics
                    .replenish_success
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                stats.conn_orig_success.fetch_add(1, Ordering::Relaxed);
                target.metrics.orig_success.fetch_add(1, Ordering::Relaxed);
            }

            let t_active = target.metrics.active_conns.fetch_add(1, Ordering::AcqRel) + 1;
            target.metrics.update_peak(t_active);

            let current = stats.conn_current.fetch_add(1, Ordering::AcqRel) + 1;
            stats.update_conn_peak(current);

            let mut scratch = [0u8; 256];
            let mut remote_evicted = false;
            loop {
                tokio::select! {
                    biased;
                    _ = global_cancel.cancelled() => break,
                    _ = slot_cancel.cancelled() => break,
                    ready = stream.readable() => {
                        match ready {
                            Ok(()) => match stream.try_read(&mut scratch) {
                                Ok(0) => {
                                    remote_evicted = true;
                                    break;
                                }
                                Ok(_) => continue,
                                Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                                Err(_) => {
                                    remote_evicted = true;
                                    break;
                                }
                            },
                            Err(_) => {
                                remote_evicted = true;
                                break;
                            }
                        }
                    }
                }
            }

            target.metrics.active_conns.fetch_sub(1, Ordering::AcqRel);
            stats.conn_current.fetch_sub(1, Ordering::AcqRel);

            if remote_evicted && !global_cancel.is_cancelled() && !slot_cancel.is_cancelled() {
                stats.conn_remote_closed.fetch_add(1, Ordering::Relaxed);
                target.metrics.remote_closed.fetch_add(1, Ordering::Relaxed);
                // Replenishment keeps target concurrency alive, counted strictly in replenish_*
                sleep_or_cancel2(
                    &global_cancel,
                    &slot_cancel,
                    Duration::from_millis(20 + stagger_ms),
                )
                .await;
            }
        }
    });
}

async fn sleep_or_cancel2(c1: &CancelToken, c2: &CancelToken, dur: Duration) {
    tokio::select! {
        biased;
        _ = c1.cancelled() => {},
        _ = c2.cancelled() => {},
        _ = tokio::time::sleep(dur) => {},
    }
}

#[allow(clippy::too_many_arguments)]
fn adjust_pool(
    desired: usize,
    pool: &mut Vec<Arc<CancelToken>>,
    targets: &Arc<Vec<VerifiedTarget>>,
    connect_timeout: Duration,
    limiter: &Arc<Semaphore>,
    stats: &Arc<SharedStats>,
    global_cancel: &Arc<CancelToken>,
) {
    if pool.len() > desired {
        while pool.len() > desired {
            if let Some(tok) = pool.pop() {
                tok.cancel();
            }
        }
        return;
    }

    let start_idx = pool.len();
    for slot_idx in start_idx..desired {
        if global_cancel.is_cancelled() {
            break;
        }
        let tok = Arc::new(CancelToken::new());
        pool.push(tok.clone());
        spawn_slot(
            slot_idx,
            targets.clone(),
            connect_timeout,
            limiter.clone(),
            stats.clone(),
            global_cancel.clone(),
            tok,
        );
    }
}

enum StepVerdict {
    Passed { live: usize },
    Bottleneck { peak_live: usize, local_delta: u64 },
    Cancelled,
}

async fn monitor_step(
    target: usize,
    hold_secs: u64,
    prev_local_err: u64,
    stats: &SharedStats,
    cancel: &CancelToken,
) -> StepVerdict {
    let max_wait = Duration::from_secs(hold_secs.max(2) + 2);
    let min_fast_advance = Duration::from_millis(1200);
    let started = Instant::now();

    let mut healthy_streak = 0u32;
    let mut best_live = 0usize;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return StepVerdict::Cancelled,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }

        let live = stats.conn_current.load(Ordering::Acquire);
        if live > best_live {
            best_live = live;
        }

        let local_delta = stats
            .conn_fail_local
            .load(Ordering::Relaxed)
            .saturating_sub(prev_local_err);

        if local_delta >= 40 && live < (target * 80) / 100 {
            return StepVerdict::Bottleneck {
                peak_live: best_live,
                local_delta,
            };
        }

        let pass_line = ((target * 85) / 100).max(1);
        if live >= pass_line {
            healthy_streak += 1;
        } else {
            healthy_streak = 0;
        }

        if started.elapsed() >= min_fast_advance && healthy_streak >= 4 {
            return StepVerdict::Passed { live };
        }

        if started.elapsed() >= max_wait {
            let final_live = stats.conn_current.load(Ordering::Acquire);
            if final_live >= ((target * 78) / 100).max(1) && local_delta < 40 {
                return StepVerdict::Passed { live: final_live };
            }
            return StepVerdict::Bottleneck {
                peak_live: best_live,
                local_delta,
            };
        }
    }
}

fn next_step_size(current: usize) -> usize {
    if current < 1_000 {
        300
    } else if current < 4_000 {
        700
    } else if current < 10_000 {
        1_200
    } else {
        2_000
    }
}

fn log_target_distribution_table(stats: &SharedStats, log: &MemoryLog, header: &str) {
    let snapshot: Vec<Arc<TargetMetrics>> = stats.target_metrics.lock().clone();
    if snapshot.is_empty() {
        return;
    }
    log.push(header);
    let total_active: usize = snapshot
        .iter()
        .filter(|t| t.healthy)
        .map(|t| t.active_conns.load(Ordering::Relaxed))
        .sum();

    for t in &snapshot {
        if !t.healthy {
            continue;
        }
        let act = t.active_conns.load(Ordering::Relaxed);
        let pk = t.peak_conns.load(Ordering::Relaxed);
        let share = if total_active == 0 { 0.0 } else { (act as f64) * 100.0 / total_active as f64 };
        let orig_ok = t.orig_success.load(Ordering::Relaxed);
        let orig_att = t.orig_attempts.load(Ordering::Relaxed);
        let orig_pct = t.orig_success_rate_pct();
        let rep_ok = t.replenish_success.load(Ordering::Relaxed);
        let rep_att = t.replenish_attempts.load(Ordering::Relaxed);
        let f_loc = t.fail_local.load(Ordering::Relaxed);
        let f_ref = t.fail_refused.load(Ordering::Relaxed);
        let f_rem = t.remote_closed.load(Ordering::Relaxed);
        let f_to = t.fail_timeout.load(Ordering::Relaxed);
        let avg_rtt = t.avg_rtt_ms();
        log.push(format!(
            "  · {} [{}] | 存活/峰值:{act}/{pk} (当前存活占比{share:.1}%) | 原始:{orig_ok}/{orig_att}({orig_pct:.1}%) | 补建:{rep_ok}/{rep_att} | 归属(本机:{f_loc} 拒:{f_ref} 远闭:{f_rem} 超时:{f_to}) | RTT:{avg_rtt}ms",
            t.label, t.resolved_addr
        ));
    }
}

pub async fn run_conn_test(
    config: TestConfig,
    stats: Arc<SharedStats>,
    log: Arc<MemoryLog>,
    cancel: Arc<CancelToken>,
) {
    log.push("第1步 [目标健康检查]：正在验证各配置目标的 DNS 解析与 TCP 握手稳定性...");
    let (verified_targets, all_metrics, baseline_rtt) =
        probe_and_verify_targets(&config.tcp_targets, &log, &cancel).await;
    if cancel.is_cancelled() {
        return;
    }

    let total_targets = all_metrics.len();
    let healthy_targets = verified_targets.len();
    stats
        .conn_targets_total
        .store(total_targets, Ordering::Relaxed);
    stats
        .conn_targets_healthy
        .store(healthy_targets, Ordering::Relaxed);
    *stats.target_metrics.lock() = all_metrics;

    if verified_targets.is_empty() {
        log.push("错误：所有目标均未通过 TCP 健康检查，请检查网络或目标配置。");
        return;
    }
    if baseline_rtt > 0 {
        stats.conn_rtt_ms.store(baseline_rtt, Ordering::Relaxed);
    }

    let cap_desc = if config.max_conn_cap == 0 {
        "不限 (持续探测至瓶颈)".to_string()
    } else if config.continuous_conn {
        format!(
            "参考上限 {}（已开启持续加压，到达后进入长时保持并尝试递增）",
            config.max_conn_cap
        )
    } else {
        format!("{}（到达后进入长时保持验证）", config.max_conn_cap)
    };

    log.push(format!(
        "健康检查完成：{healthy_targets}/{total_targets} 个目标支持 TCP 并加入测试 (基准 RTT: {} ms)；开始第2步 [瞬时与稳定并发探测] [上限: {cap_desc}, 步进窗口: {}s]",
        baseline_rtt.max(1),
        config.hold_secs
    ));

    let targets = Arc::new(verified_targets);
    let limiter = Arc::new(Semaphore::new(MAX_CONCURRENT_HANDSHAKES));
    let connect_timeout = Duration::from_secs(4);
    let mut pool: Vec<Arc<CancelToken>> = Vec::new();

    let effective_cap = if config.max_conn_cap == 0 || config.continuous_conn {
        UNLIMITED_SAFETY_CEILING
    } else {
        config.max_conn_cap
    };

    let mut step_target: usize = 200.min(effective_cap).max(1);
    let mut last_stable: usize = 0;
    let mut crossed_ref_cap = false;
    let mut hit_bottleneck = false;

    while !cancel.is_cancelled() {
        stats.conn_target_probe.store(step_target, Ordering::Relaxed);
        let prev_local_err = stats.conn_fail_local.load(Ordering::Relaxed);

        adjust_pool(
            step_target,
            &mut pool,
            &targets,
            connect_timeout,
            &limiter,
            &stats,
            &cancel,
        );

        match monitor_step(
            step_target,
            config.hold_secs,
            prev_local_err,
            &stats,
            &cancel,
        )
        .await
        {
            StepVerdict::Cancelled => break,
            StepVerdict::Passed { live } => {
                last_stable = live.max(last_stable);
                stats.conn_stable_max.store(last_stable, Ordering::Relaxed);
                let orig_ok = stats.conn_orig_success.load(Ordering::Relaxed);
                let orig_att = stats.conn_orig_attempts.load(Ordering::Relaxed);
                let orig_pct = stats.orig_success_rate_pct();
                let rep_ok = stats.conn_replenish_success.load(Ordering::Relaxed);
                log.push(format!(
                    "阶梯 {step_target} 通过 (存活: {live}, 峰值: {}, 原始: {orig_ok}/{orig_att}={orig_pct:.1}%, 补建: {rep_ok}, RTT: {} ms)",
                    stats.conn_peak.load(Ordering::Relaxed),
                    stats.conn_rtt_ms.load(Ordering::Relaxed),
                ));

                if !config.continuous_conn
                    && config.max_conn_cap > 0
                    && step_target >= config.max_conn_cap
                {
                    log.push("已达到设定连接上限，即将进入第3步 [长时间保持验证]。");
                    break;
                }

                if config.continuous_conn
                    && config.max_conn_cap > 0
                    && !crossed_ref_cap
                    && step_target >= config.max_conn_cap
                {
                    crossed_ref_cap = true;
                    log.push(format!(
                        "已达到参考上限 {}，继续维持并向上递增探测（可随时点击“停止当前测试”）...",
                        config.max_conn_cap
                    ));
                }

                if step_target >= effective_cap {
                    log.push(format!(
                        "已达到安全上限 {effective_cap}，进入第3步 [长时间保持验证]。"
                    ));
                    break;
                }

                step_target = (step_target + next_step_size(step_target)).min(effective_cap);
            }
            StepVerdict::Bottleneck {
                peak_live,
                local_delta,
            } => {
                last_stable = last_stable.max(peak_live);
                stats.conn_stable_max.store(last_stable, Ordering::Relaxed);
                hit_bottleneck = true;
                log.push(format!(
                    "在目标 {step_target} 触及瓶颈 (窗口峰值存活: {peak_live}, RTT: {} ms, 本机失败 +{local_delta})，转入长时间保持验证",
                    stats.conn_rtt_ms.load(Ordering::Relaxed)
                ));
                break;
            }
        }
    }

    // Stage 3: Long-duration hold verification (runs both in finite mode and continuous mode)
    if !cancel.is_cancelled() {
        let hold_target = if hit_bottleneck {
            last_stable.max(200)
        } else {
            step_target
        };
        stats.conn_target_probe.store(hold_target, Ordering::Relaxed);
        stats.conn_holding.store(true, Ordering::Relaxed);
        adjust_pool(
            hold_target,
            &mut pool,
            &targets,
            connect_timeout,
            &limiter,
            &stats,
            &cancel,
        );

        let hold_limit = if config.continuous_conn {
            None
        } else {
            Some(FINITE_LONG_HOLD_SECS)
        };
        log.push(match hold_limit {
            Some(secs) => format!(
                "第3步 [长时间保持验证]：维持 {hold_target} 并发会话 {secs} 秒（不将补建计入原始成功率）..."
            ),
            None => format!(
                "第3步 [长时间持续保持]：持续维持 {hold_target} 并发会话并按 10s 周期采样 WAN 分流，点击“停止当前测试”可终止。"
            ),
        });

        let mut hold_elapsed = 0u64;
        let mut hold_sum_live = 0u64;
        let mut hold_samples = 0u64;
        let mut current_hold = hold_target;

        while !cancel.is_cancelled() {
            if let Some(limit) = hold_limit
                && hold_elapsed >= limit
            {
                break;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            hold_elapsed += 1;
            stats
                .conn_hold_elapsed_secs
                .store(hold_elapsed, Ordering::Relaxed);

            let live = stats.conn_current.load(Ordering::Relaxed);
            if hold_elapsed >= 2 {
                hold_sum_live += live as u64;
                hold_samples += 1;
                let avg_hold = (hold_sum_live / hold_samples) as usize;
                stats.conn_long_hold_max.store(avg_hold, Ordering::Relaxed);
            } else {
                stats.conn_long_hold_max.store(live, Ordering::Relaxed);
            }

            if hold_elapsed.is_multiple_of(10) {
                log_target_distribution_table(
                    &stats,
                    &log,
                    &format!("[远端目标分布周期采样 @ 保持 {hold_elapsed}s] 各健康远端目标实况:"),
                );
            }

            if config.continuous_conn
                && hold_elapsed.is_multiple_of(10)
                && current_hold < UNLIMITED_SAFETY_CEILING
                && live >= (current_hold * 88) / 100
            {
                let prev_local = stats.conn_fail_local.load(Ordering::Relaxed);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
                let local_now = stats.conn_fail_local.load(Ordering::Relaxed);
                if local_now == prev_local {
                    current_hold = (current_hold + 500).min(UNLIMITED_SAFETY_CEILING);
                    stats.conn_target_probe.store(current_hold, Ordering::Relaxed);
                    adjust_pool(
                        current_hold,
                        &mut pool,
                        &targets,
                        connect_timeout,
                        &limiter,
                        &stats,
                        &cancel,
                    );
                    log.push(format!("持续加压：尝试提升保持目标至 {current_hold} 并发会话..."));
                }
            }
        }
        stats.conn_holding.store(false, Ordering::Relaxed);
    }

    // Final remote-target snapshot while sockets or peaks are intact before teardown
    log_target_distribution_table(
        &stats,
        &log,
        "[远端目标连接分布明细]（注：此处统计远端目标连接分布，不代表物理 WAN 口分布）",
    );

    for tok in pool.drain(..) {
        tok.cancel();
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    stats.conn_current.store(0, Ordering::Relaxed);
    stats.conn_per_sec.store(0, Ordering::Relaxed);

    let peak = stats.conn_peak.load(Ordering::Relaxed);
    let cps_peak = stats.conn_cps_peak.load(Ordering::Relaxed);
    let stable_max = stats.conn_stable_max.load(Ordering::Relaxed);
    let long_hold = stats.conn_long_hold_max.load(Ordering::Relaxed);
    let hold_secs = stats.conn_hold_elapsed_secs.load(Ordering::Relaxed);

    let orig_ok = stats.conn_orig_success.load(Ordering::Relaxed);
    let orig_att = stats.conn_orig_attempts.load(Ordering::Relaxed);
    let orig_pct = stats.orig_success_rate_pct();
    let rep_ok = stats.conn_replenish_success.load(Ordering::Relaxed);
    let rep_att = stats.conn_replenish_attempts.load(Ordering::Relaxed);

    let f_loc = stats.conn_fail_local.load(Ordering::Relaxed);
    let f_ref = stats.conn_fail_refused.load(Ordering::Relaxed);
    let f_rem = stats.conn_remote_closed.load(Ordering::Relaxed);
    let f_to = stats.conn_fail_timeout.load(Ordering::Relaxed);

    log.push(format!(
        "=== 测试分级汇总 === [1.瞬时建连] 峰值并发:{peak} / 建连峰值:{cps_peak}/s / 原始成功率:{orig_ok}/{orig_att}({orig_pct:.1}%) | [2.稳定并发] 稳定上限:{stable_max} | [3.长时间保持] 平均保持:{long_hold} (持续 {hold_secs}s)"
    ));
    log.push(format!(
        "=== 归属与补建汇总 === 补建(不计入原始): {rep_ok}/{rep_att} | 连接归属失败 -> 本机失败:{f_loc}, 远端拒绝:{f_ref}, 远端关闭:{f_rem}, 连接超时:{f_to}"
    ));

    if f_loc > 0 {
        log.push("提示：检测到本机失败（端口耗尽/缓冲区不足或本地路由异常）。如需更大本地端口池，可在管理员 CMD 执行: netsh int ipv4 set dynamicport tcp start=10000 num=55000（本程序不修改注册表或磁盘）。");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn test_classify_connect_error_attribution() {
        let local_err = std::io::Error::from(ErrorKind::AddrInUse);
        assert_eq!(classify_connect_error(&local_err), ConnectErrorBucket::LocalFailure);

        let refused_err = std::io::Error::from(ErrorKind::ConnectionRefused);
        assert_eq!(
            classify_connect_error(&refused_err),
            ConnectErrorBucket::RemoteRefused
        );

        let timeout_err = std::io::Error::from(ErrorKind::TimedOut);
        assert_eq!(classify_connect_error(&timeout_err), ConnectErrorBucket::Timeout);
    }

    #[tokio::test]
    async fn test_health_check_filters_dead_port_and_keeps_live_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = listener.local_addr().unwrap();

        // Reserve and close a port so it refuses TCP
        let temp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = temp.local_addr().unwrap();
        drop(temp);

        let accept_task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });

        let log = MemoryLog::new(64);
        let cancel = CancelToken::new();
        let targets = vec![live_addr.to_string(), dead_addr.to_string()];
        let (verified, all, _rtt) = probe_and_verify_targets(&targets, &log, &cancel).await;

        assert_eq!(all.len(), 2);
        assert_eq!(verified.len(), 1);
        assert_eq!(verified[0].addrs[0], live_addr);
        assert!(all[0].healthy);
        assert!(!all[1].healthy);

        accept_task.abort();
    }

    #[tokio::test]
    async fn test_replenish_does_not_mask_original_attempts_or_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Accept and immediately close the first connection to force remote eviction + replenish
        let server_task = tokio::spawn(async move {
            if let Ok((s1, _)) = listener.accept().await {
                drop(s1);
            }
            if let Ok((s2, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_millis(300)).await;
                drop(s2);
            }
        });

        let metrics = Arc::new(TargetMetrics::healthy(addr.to_string(), addr.to_string(), 1));
        let targets = Arc::new(vec![VerifiedTarget {
            addrs: vec![addr],
            metrics: metrics.clone(),
        }]);
        let stats = Arc::new(SharedStats::default());
        let limiter = Arc::new(Semaphore::new(8));
        let global_cancel = Arc::new(CancelToken::new());
        let slot_cancel = Arc::new(CancelToken::new());

        spawn_slot(
            0,
            targets,
            Duration::from_secs(2),
            limiter,
            stats.clone(),
            global_cancel.clone(),
            slot_cancel,
        );

        tokio::time::sleep(Duration::from_millis(220)).await;
        global_cancel.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(stats.conn_orig_attempts.load(Ordering::Relaxed), 1);
        assert_eq!(stats.conn_orig_success.load(Ordering::Relaxed), 1);
        assert!(stats.conn_remote_closed.load(Ordering::Relaxed) >= 1);
        assert!(stats.conn_replenish_attempts.load(Ordering::Relaxed) >= 1);

        server_task.abort();
    }
}
