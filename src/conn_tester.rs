//! Maximum concurrent TCP connection tester (ConnTester).
//! Uses incremental ramp-up + stability hold + binary search refinement in memory.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use socket2::{SockRef, TcpKeepalive};
use tokio::net::{TcpSocket, lookup_host};
use tokio::sync::Notify;

use crate::state::{MemoryLog, SharedStats, TestConfig};

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

fn classify_io_error(err: &std::io::Error, stats: &SharedStats) {
    if let Some(code) = err.raw_os_error() {
        // Windows Winsock resource/ephemeral-port exhaustion codes:
        // 10055 = WSAENOBUFS, 10048 = WSAEADDRINUSE, 10024 = WSAEMFILE
        // Linux equivalents: 24 = EMFILE, 105 = ENOBUFS, 98 = EADDRINUSE
        if matches!(code, 10055 | 10048 | 10024 | 24 | 105 | 98) {
            stats.conn_fail_resource.fetch_add(1, Ordering::Relaxed);
            return;
        }
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

async fn resolve_targets(targets: &[String], log: &MemoryLog) -> Vec<SocketAddr> {
    let mut resolved = Vec::new();
    for raw in targets {
        let host_port = raw.trim().trim_start_matches("tcp://");
        if host_port.is_empty() {
            continue;
        }
        match tokio::time::timeout(Duration::from_secs(5), lookup_host(host_port)).await {
            Ok(Ok(addrs)) => {
                let mut count = 0usize;
                for addr in addrs {
                    if !resolved.contains(&addr) {
                        resolved.push(addr);
                        count += 1;
                    }
                    if count >= 4 {
                        break;
                    }
                }
            }
            Ok(Err(e)) => {
                log.push(format!("目标解析失败 [{host_port}]: {e}"));
            }
            Err(_) => {
                log.push(format!("目标解析超时 [{host_port}]"));
            }
        }
    }
    resolved
}

fn spawn_single_conn(
    addr: SocketAddr,
    connect_timeout: Duration,
    stats: Arc<SharedStats>,
    global_cancel: Arc<CancelToken>,
    conn_cancel: Arc<CancelToken>,
) {
    tokio::spawn(async move {
        if global_cancel.is_cancelled() || conn_cancel.is_cancelled() {
            return;
        }
        stats.conn_attempts.fetch_add(1, Ordering::Relaxed);

        let socket = match if addr.is_ipv4() {
            TcpSocket::new_v4()
        } else {
            TcpSocket::new_v6()
        } {
            Ok(s) => s,
            Err(e) => {
                classify_io_error(&e, &stats);
                return;
            }
        };

        // Keep per-socket kernel buffers small so high connection counts use minimal RAM.
        let _ = socket.set_recv_buffer_size(4096);
        let _ = socket.set_send_buffer_size(4096);

        let connect_res = tokio::select! {
            biased;
            _ = global_cancel.cancelled() => return,
            _ = conn_cancel.cancelled() => return,
            res = tokio::time::timeout(connect_timeout, socket.connect(addr)) => res,
        };

        let stream = match connect_res {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                classify_io_error(&e, &stats);
                return;
            }
            Err(_) => {
                stats.conn_fail_timeout.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };

        let keepalive = TcpKeepalive::new()
            .with_time(Duration::from_secs(15))
            .with_interval(Duration::from_secs(5));
        let _ = SockRef::from(&stream).set_tcp_keepalive(&keepalive);

        let current = stats.conn_current.fetch_add(1, Ordering::AcqRel) + 1;
        stats.update_conn_peak(current);

        let mut scratch = [0u8; 256];
        loop {
            tokio::select! {
                biased;
                _ = global_cancel.cancelled() => break,
                _ = conn_cancel.cancelled() => break,
                ready = stream.readable() => {
                    match ready {
                        Ok(()) => {
                            match stream.try_read(&mut scratch) {
                                Ok(0) => break, // remote FIN
                                Ok(_) => continue,
                                Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                                Err(e) => {
                                    classify_io_error(&e, &stats);
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            classify_io_error(&e, &stats);
                            break;
                        }
                    }
                }
            }
        }

        stats.conn_current.fetch_sub(1, Ordering::AcqRel);
    });
}

fn adjust_pool(
    desired: usize,
    pool: &mut Vec<Arc<CancelToken>>,
    addrs: &[SocketAddr],
    rr_idx: &mut usize,
    connect_timeout: Duration,
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

    let to_add = desired - pool.len();
    for _ in 0..to_add {
        if global_cancel.is_cancelled() {
            break;
        }
        let addr = addrs[*rr_idx % addrs.len()];
        *rr_idx = rr_idx.wrapping_add(1);
        let tok = Arc::new(CancelToken::new());
        pool.push(tok.clone());
        spawn_single_conn(
            addr,
            connect_timeout,
            stats.clone(),
            global_cancel.clone(),
            tok,
        );
    }
}

async fn hold_and_check(
    target: usize,
    hold_secs: u64,
    stats: &SharedStats,
    cancel: &CancelToken,
) -> (bool, usize) {
    // Allow brief ramp settle, then monitor stability over hold_secs
    let settle_ms = 1200u64;
    let steps = (hold_secs.max(2) * 1000) / 500;

    tokio::select! {
        _ = cancel.cancelled() => return (false, stats.conn_current.load(Ordering::Acquire)),
        _ = tokio::time::sleep(Duration::from_millis(settle_ms)) => {}
    }

    let mut min_observed = usize::MAX;
    for _ in 0..steps {
        tokio::select! {
            _ = cancel.cancelled() => return (false, stats.conn_current.load(Ordering::Acquire)),
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
        let live = stats.conn_current.load(Ordering::Acquire);
        if live < min_observed {
            min_observed = live;
        }
    }

    if min_observed == usize::MAX {
        min_observed = stats.conn_current.load(Ordering::Acquire);
    }

    // Consider stable if at least 88% of target stayed alive throughout hold window
    let threshold = (target * 88) / 100;
    let stable = min_observed >= threshold.max(1);
    (stable, min_observed)
}

pub async fn run_conn_test(
    config: TestConfig,
    stats: Arc<SharedStats>,
    log: Arc<MemoryLog>,
    cancel: Arc<CancelToken>,
) {
    log.push("正在解析连接测试目标 (仅内存)...".to_string());
    let addrs = resolve_targets(&config.tcp_targets, &log).await;
    if addrs.is_empty() {
        log.push("错误：没有可用的 TCP 测试目标地址，请检查配置。");
        return;
    }

    log.push(format!(
        "已解析 {} 个公网节点，开始最大并发 TCP 连接探测 (上限: {}, 稳定窗口: {}s)",
        addrs.len(),
        config.max_conn_cap,
        config.hold_secs
    ));

    let connect_timeout = Duration::from_secs(5);
    let mut pool: Vec<Arc<CancelToken>> = Vec::new();
    let mut rr_idx: usize = 0;

    let mut last_stable: usize = 0;
    let mut failed_upper: Option<usize> = None;

    let mut step_target: usize = 100.min(config.max_conn_cap).max(1);

    // Phase 1: Incremental ramp-up until cap or instability
    while !cancel.is_cancelled() {
        stats.conn_target_probe.store(step_target, Ordering::Relaxed);
        log.push(format!("探测阶梯目标: {step_target} 并发连接..."));

        let prev_resource_err = stats.conn_fail_resource.load(Ordering::Relaxed);
        adjust_pool(
            step_target,
            &mut pool,
            &addrs,
            &mut rr_idx,
            connect_timeout,
            &stats,
            &cancel,
        );

        let (stable, min_live) =
            hold_and_check(step_target, config.hold_secs, &stats, &cancel).await;
        if cancel.is_cancelled() {
            break;
        }

        let new_resource_err = stats.conn_fail_resource.load(Ordering::Relaxed);
        let hit_resource_limit = new_resource_err > prev_resource_err + 10;

        if stable && !hit_resource_limit {
            last_stable = min_live.max(last_stable);
            stats.conn_stable_max.store(last_stable, Ordering::Relaxed);
            log.push(format!(
                "阶梯 {step_target} 稳定通过 (窗口最低存活: {min_live}, 峰值: {})",
                stats.conn_peak.load(Ordering::Relaxed)
            ));

            if step_target >= config.max_conn_cap {
                log.push("已达到配置的连接数上限，停止继续递增。");
                break;
            }

            let next_delta = if step_target < 500 {
                200
            } else if step_target < 2000 {
                500
            } else {
                1000
            };
            step_target = (step_target + next_delta).min(config.max_conn_cap);
        } else {
            failed_upper = Some(step_target);
            log.push(format!(
                "阶梯 {step_target} 未能稳定维持 (最低存活: {min_live}, 资源错误新增: {})，进入二分收敛...",
                new_resource_err.saturating_sub(prev_resource_err)
            ));
            break;
        }
    }

    // Phase 2: Binary search refinement between last_stable and failed_upper
    if !cancel.is_cancelled()
        && let Some(mut high) = failed_upper
    {
        let mut low = last_stable;
        for round in 1..=4 {
            if cancel.is_cancelled() || high <= low + 50 {
                break;
            }
            let mid = low + (high - low) / 2;
            stats.conn_target_probe.store(mid, Ordering::Relaxed);
            log.push(format!("二分第 {round} 轮探测: {mid} 并发连接..."));

            adjust_pool(
                mid,
                &mut pool,
                &addrs,
                &mut rr_idx,
                connect_timeout,
                &stats,
                &cancel,
            );
            let (stable, min_live) =
                hold_and_check(mid, config.hold_secs, &stats, &cancel).await;
            if cancel.is_cancelled() {
                break;
            }

            if stable {
                low = min_live.max(low);
                stats.conn_stable_max.store(low, Ordering::Relaxed);
                log.push(format!("二分探测 {mid} 稳定 (最低存活: {min_live})"));
            } else {
                high = mid;
                log.push(format!("二分探测 {mid} 不稳定 (最低存活: {min_live})"));
            }
        }
    }

    // Clean up all open sockets in memory
    for tok in pool.drain(..) {
        tok.cancel();
    }

    // Wait briefly for socket tasks to release handles
    tokio::time::sleep(Duration::from_millis(300)).await;
    stats.conn_current.store(0, Ordering::Relaxed);

    let peak = stats.conn_peak.load(Ordering::Relaxed);
    let stable_max = stats.conn_stable_max.load(Ordering::Relaxed);
    let f_to = stats.conn_fail_timeout.load(Ordering::Relaxed);
    let f_ref = stats.conn_fail_refused.load(Ordering::Relaxed);
    let f_res = stats.conn_fail_resource.load(Ordering::Relaxed);
    let f_oth = stats.conn_fail_other.load(Ordering::Relaxed);

    log.push(format!(
        "连接测试结束：峰值连接 {peak}，最大稳定并发连接 {stable_max}；失败统计 [超时:{f_to}, 拒绝/重置:{f_ref}, 系统资源:{f_res}, 其他:{f_oth}]"
    ));

    if f_res > 0 {
        log.push("提示：检测到本机端口/缓冲区资源受限。如需更高并发，可在管理员 CMD 中调整动态端口范围 (netsh int ipv4 set dynamicport tcp start=10000 num=55000)，本程序不修改注册表或磁盘。");
    }
}
