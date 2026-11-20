//! Router-WAN-style concurrent TCP session prober (ConnTester).
//! Principles:
//! 1. Pre-flight WAN endpoint reachability probe (drops blocked/dead IPs & broken IPv6).
//! 2. Multi-IP/multi-port 4-tuple dispersion across verified public WAN endpoints.
//! 3. Paced handshake concurrency + RTT EWMA congestion sensing (prevents SYN burst collapse).
//! 4. Transparent slot auto-replenishment when remote servers evict idle sockets.
//! 5. Rapid adaptive ramp-up with continuous escalation & NAT-table hold mode.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use socket2::{SockRef, TcpKeepalive};
use tokio::net::{TcpSocket, TcpStream, lookup_host};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;

use crate::state::{MemoryLog, SharedStats, TestConfig};

const MAX_CONCURRENT_HANDSHAKES: usize = 256;
const UNLIMITED_SAFETY_CEILING: usize = 100_000;

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

fn is_resource_os_error(err: &std::io::Error) -> bool {
    err.raw_os_error()
        .is_some_and(|c| matches!(c, 10055 | 10048 | 10024 | 24 | 105 | 98))
}

fn classify_connect_error(err: &std::io::Error, stats: &SharedStats) {
    if is_resource_os_error(err) {
        stats.conn_fail_resource.fetch_add(1, Ordering::Relaxed);
        return;
    }
    match err.kind() {
        ErrorKind::TimedOut => {
            stats.conn_fail_timeout.fetch_add(1, Ordering::Relaxed);
        }
        ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => {
            stats.conn_fail_refused.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            stats.conn_fail_other.fetch_add(1, Ordering::Relaxed);
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

/// Resolve all configured targets and run a fast parallel WAN pre-flight TCP probe
/// (like a router probing WAN gateway/endpoint health) so unreachable ports or broken IPv6
/// addresses never pollute concurrency stability.
async fn probe_and_filter_targets(
    targets: &[String],
    log: &MemoryLog,
    cancel: &CancelToken,
) -> (Vec<SocketAddr>, u64) {
    let mut candidates = Vec::new();

    for raw in targets {
        if cancel.is_cancelled() {
            return (Vec::new(), 0);
        }
        let host_port = raw.trim().trim_start_matches("tcp://");
        if host_port.is_empty() {
            continue;
        }
        match tokio::time::timeout(Duration::from_secs(3), lookup_host(host_port)).await {
            Ok(Ok(addrs)) => {
                let mut v4_count = 0usize;
                let mut v6_count = 0usize;
                for addr in addrs {
                    if addr.is_ipv4() {
                        if v4_count < 8 && !candidates.contains(&addr) {
                            candidates.push(addr);
                            v4_count += 1;
                        }
                    } else if v6_count < 3 && !candidates.contains(&addr) {
                        candidates.push(addr);
                        v6_count += 1;
                    }
                }
            }
            Ok(Err(e)) => {
                log.push(format!("WAN 目标解析失败 [{host_port}]: {e}"));
            }
            Err(_) => {
                log.push(format!("WAN 目标解析超时 [{host_port}]"));
            }
        }
    }

    if candidates.is_empty() {
        return (Vec::new(), 0);
    }

    let mut set = JoinSet::new();
    for addr in candidates {
        set.spawn(async move {
            let t0 = Instant::now();
            let res = tokio::time::timeout(
                Duration::from_millis(1600),
                TcpStream::connect(addr),
            )
            .await;
            match res {
                Ok(Ok(stream)) => {
                    drop(stream);
                    Some((addr, t0.elapsed().as_millis() as u64))
                }
                _ => None,
            }
        });
    }

    let mut verified_v4 = Vec::new();
    let mut verified_v6 = Vec::new();
    let mut rtt_sum = 0u64;
    let mut rtt_cnt = 0u64;

    while let Some(res) = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            set.abort_all();
            return (Vec::new(), 0);
        }
        r = set.join_next() => r,
    } {
        if let Ok(Some((addr, rtt_ms))) = res {
            rtt_sum += rtt_ms.max(1);
            rtt_cnt += 1;
            if addr.is_ipv4() {
                verified_v4.push(addr);
            } else {
                verified_v6.push(addr);
            }
        }
    }

    // Put verified IPv4 first, followed by verified IPv6 (if reachable on this WAN)
    verified_v4.extend(verified_v6);
    let baseline_rtt = if rtt_cnt > 0 { rtt_sum / rtt_cnt } else { 0 };
    (verified_v4, baseline_rtt)
}

#[allow(clippy::too_many_arguments)]
fn spawn_slot(
    slot_idx: usize,
    addrs: Arc<Vec<SocketAddr>>,
    connect_timeout: Duration,
    limiter: Arc<Semaphore>,
    stats: Arc<SharedStats>,
    global_cancel: Arc<CancelToken>,
    slot_cancel: Arc<CancelToken>,
) {
    tokio::spawn(async move {
        let mut attempt = 0usize;
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

            // RTT-aware pacing (like router WAN probe queue discipline):
            // if WAN handshake RTT climbs sharply, pace slightly before dialing
            let cur_rtt = stats.conn_rtt_ms.load(Ordering::Relaxed);
            if cur_rtt >= 300 {
                let pace = ((cur_rtt - 250) / 15).clamp(10, 80);
                sleep_or_cancel2(&global_cancel, &slot_cancel, Duration::from_millis(pace)).await;
                if global_cancel.is_cancelled() || slot_cancel.is_cancelled() {
                    drop(permit);
                    break;
                }
            }

            let addr = addrs[(slot_idx.wrapping_add(attempt)) % addrs.len()];
            attempt = attempt.wrapping_add(1);
            stats.conn_attempts.fetch_add(1, Ordering::Relaxed);

            let socket = match if addr.is_ipv4() {
                TcpSocket::new_v4()
            } else {
                TcpSocket::new_v6()
            } {
                Ok(s) => s,
                Err(e) => {
                    drop(permit);
                    classify_connect_error(&e, &stats);
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
                    update_rtt_ewma(&stats, t0.elapsed().as_millis() as u64);
                    s
                }
                Ok(Err(e)) => {
                    let is_res = is_resource_os_error(&e);
                    classify_connect_error(&e, &stats);
                    let backoff = if is_res {
                        500 + stagger_ms
                    } else {
                        140 + stagger_ms
                    };
                    sleep_or_cancel2(&global_cancel, &slot_cancel, Duration::from_millis(backoff)).await;
                    continue;
                }
                Err(_) => {
                    stats.conn_fail_timeout.fetch_add(1, Ordering::Relaxed);
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

            stats.conn_established_total.fetch_add(1, Ordering::Relaxed);
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

            stats.conn_current.fetch_sub(1, Ordering::AcqRel);
            if remote_evicted && !global_cancel.is_cancelled() && !slot_cancel.is_cancelled() {
                stats.conn_remote_closed.fetch_add(1, Ordering::Relaxed);
                // Transparently replenish remote-closed idle socket across WAN pool
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
    addrs: &Arc<Vec<SocketAddr>>,
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
            addrs.clone(),
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
    Bottleneck { peak_live: usize, resource_delta: u64 },
    Cancelled,
}

async fn monitor_step(
    target: usize,
    hold_secs: u64,
    prev_resource_err: u64,
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

        let res_delta = stats
            .conn_fail_resource
            .load(Ordering::Relaxed)
            .saturating_sub(prev_resource_err);

        if res_delta >= 40 && live < (target * 80) / 100 {
            return StepVerdict::Bottleneck {
                peak_live: best_live,
                resource_delta: res_delta,
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
            if final_live >= ((target * 78) / 100).max(1) && res_delta < 40 {
                return StepVerdict::Passed { live: final_live };
            }
            return StepVerdict::Bottleneck {
                peak_live: best_live,
                resource_delta: res_delta,
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

pub async fn run_conn_test(
    config: TestConfig,
    stats: Arc<SharedStats>,
    log: Arc<MemoryLog>,
    cancel: Arc<CancelToken>,
) {
    log.push("正在执行 WAN 节点预探测与可达性筛选 (仅内存)...");
    let (addrs, baseline_rtt) = probe_and_filter_targets(&config.tcp_targets, &log, &cancel).await;
    if cancel.is_cancelled() {
        return;
    }
    if addrs.is_empty() {
        log.push("错误：预探测未发现可达的 TCP 测试节点，请检查网络或目标配置。");
        return;
    }
    if baseline_rtt > 0 {
        stats.conn_rtt_ms.store(baseline_rtt, Ordering::Relaxed);
    }

    let cap_desc = if config.max_conn_cap == 0 {
        "不限 (持续探测至系统/WAN 瓶颈)".to_string()
    } else if config.continuous_conn {
        format!(
            "参考上限 {}（已开启持续加压，到达后继续维持与递增）",
            config.max_conn_cap
        )
    } else {
        format!("{}（到达后停止）", config.max_conn_cap)
    };

    log.push(format!(
        "WAN 预探测完成：{} 个公网端点可用 (基准握手延迟: {} ms)；开始并发探测 [上限: {cap_desc}, 步进窗口: {}s]",
        addrs.len(),
        baseline_rtt.max(1),
        config.hold_secs
    ));

    let addrs = Arc::new(addrs);
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
        let prev_resource_err = stats.conn_fail_resource.load(Ordering::Relaxed);

        adjust_pool(
            step_target,
            &mut pool,
            &addrs,
            connect_timeout,
            &limiter,
            &stats,
            &cancel,
        );

        match monitor_step(
            step_target,
            config.hold_secs,
            prev_resource_err,
            &stats,
            &cancel,
        )
        .await
        {
            StepVerdict::Cancelled => break,
            StepVerdict::Passed { live } => {
                last_stable = live.max(last_stable);
                stats.conn_stable_max.store(last_stable, Ordering::Relaxed);
                log.push(format!(
                    "阶梯 {step_target} 通过 (当前存活: {live}, 峰值: {}, 握手延迟: {} ms, 远端补建: {})",
                    stats.conn_peak.load(Ordering::Relaxed),
                    stats.conn_rtt_ms.load(Ordering::Relaxed),
                    stats.conn_remote_closed.load(Ordering::Relaxed)
                ));

                if !config.continuous_conn
                    && config.max_conn_cap > 0
                    && step_target >= config.max_conn_cap
                {
                    log.push("已达到设定连接上限且未开启持续加压，结束连接探测。");
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
                    log.push(format!("已达到安全上限 {effective_cap}，进入持续维持阶段。"));
                    break;
                }

                step_target = (step_target + next_step_size(step_target)).min(effective_cap);
            }
            StepVerdict::Bottleneck {
                peak_live,
                resource_delta,
            } => {
                last_stable = last_stable.max(peak_live);
                stats.conn_stable_max.store(last_stable, Ordering::Relaxed);
                hit_bottleneck = true;
                log.push(format!(
                    "在目标 {step_target} 触及瓶颈 (窗口峰值存活: {peak_live}, 握手延迟: {} ms, 资源错误 +{resource_delta})",
                    stats.conn_rtt_ms.load(Ordering::Relaxed)
                ));
                break;
            }
        }
    }

    if config.continuous_conn && !cancel.is_cancelled() {
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
            &addrs,
            connect_timeout,
            &limiter,
            &stats,
            &cancel,
        );
        log.push(format!(
            "进入持续维持模式：稳定维持目标 {hold_target} 并发会话并自动补建，点击“停止当前测试”可随时终止。"
        ));

        let mut probe_ticks = 0u32;
        let mut current_hold = hold_target;
        while !cancel.is_cancelled() {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            probe_ticks += 1;
            let live = stats.conn_current.load(Ordering::Relaxed);
            if live > last_stable {
                last_stable = live;
                stats.conn_stable_max.store(last_stable, Ordering::Relaxed);
            }

            if probe_ticks.is_multiple_of(6)
                && current_hold < UNLIMITED_SAFETY_CEILING
                && live >= (current_hold * 88) / 100
            {
                let prev_res = stats.conn_fail_resource.load(Ordering::Relaxed);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
                let res_now = stats.conn_fail_resource.load(Ordering::Relaxed);
                if res_now == prev_res {
                    current_hold = (current_hold + 500).min(UNLIMITED_SAFETY_CEILING);
                    stats.conn_target_probe.store(current_hold, Ordering::Relaxed);
                    adjust_pool(
                        current_hold,
                        &mut pool,
                        &addrs,
                        connect_timeout,
                        &limiter,
                        &stats,
                        &cancel,
                    );
                    log.push(format!("持续加压：尝试提升维持目标至 {current_hold} 并发会话..."));
                }
            }
        }
        stats.conn_holding.store(false, Ordering::Relaxed);
    }

    for tok in pool.drain(..) {
        tok.cancel();
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    stats.conn_current.store(0, Ordering::Relaxed);
    stats.conn_per_sec.store(0, Ordering::Relaxed);

    let peak = stats.conn_peak.load(Ordering::Relaxed);
    let stable_max = stats.conn_stable_max.load(Ordering::Relaxed);
    let remote_closed = stats.conn_remote_closed.load(Ordering::Relaxed);
    let f_to = stats.conn_fail_timeout.load(Ordering::Relaxed);
    let f_ref = stats.conn_fail_refused.load(Ordering::Relaxed);
    let f_res = stats.conn_fail_resource.load(Ordering::Relaxed);
    let f_oth = stats.conn_fail_other.load(Ordering::Relaxed);

    log.push(format!(
        "连接测试结束：峰值会话 {peak}，稳定并发 {stable_max}；远端回收补建 {remote_closed}，失败 [超时:{f_to}, 拒绝:{f_ref}, 系统资源:{f_res}, 其他:{f_oth}]"
    ));

    if f_res > 0 {
        log.push("提示：检测到本机端口/缓冲区资源受限。如需更高并发，可在管理员 CMD 执行: netsh int ipv4 set dynamicport tcp start=10000 num=55000（本程序不修改注册表或磁盘）。");
    }
}
