//! Native Windows GUI built directly with Win32 API (`windows` crate 0.100+).
//! Zero disk usage: all controls, metrics, and ring-buffer logs live strictly in RAM.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use windows::Win32::*;
use windows::core::{PCWSTR, PWSTR, w};

use crate::engine::EngineHandle;
use crate::state::{
    AppMode, AppState, DEFAULT_DOWNLOAD_TARGETS, DEFAULT_HOLD_SECS, DEFAULT_MAX_CONNS_CAP,
    DEFAULT_STREAM_CONCURRENCY, DEFAULT_TCP_TARGETS, DEFAULT_UPLOAD_TARGETS, TestConfig,
    format_bytes, format_mbps, parse_target_list,
};

const TIMER_REFRESH_ID: usize = 1;

// Classic Win32 Edit control messages (WinUser.h ABI constants)
const EM_SETSEL: u32 = 0x00B1;
const EM_SCROLLCARET: u32 = 0x00B7;
const EM_REPLACESEL: u32 = 0x00C2;
const EM_SETLIMITTEXT: u32 = 0x00C5;

const ID_BTN_CONN: u16 = 1001;
const ID_BTN_UP: u16 = 1002;
const ID_BTN_DOWN: u16 = 1003;
const ID_BTN_STOP: u16 = 1004;
const ID_BTN_RESET: u16 = 1005;
const ID_BTN_CLEAR_LOG: u16 = 1006;
const ID_BTN_HELP: u16 = 1007;

struct Controls {
    grp_stats: HWND,
    lbl_mode: HWND,
    lbl_conn: HWND,
    lbl_up: HWND,
    lbl_down: HWND,
    lbl_sys: HWND,

    grp_cfg: HWND,
    lbl_streams: HWND,
    edit_streams: HWND,
    lbl_max_conn: HWND,
    edit_max_conn: HWND,
    lbl_hold_secs: HWND,
    edit_hold_secs: HWND,

    lbl_tcp_targets: HWND,
    edit_tcp_targets: HWND,
    lbl_up_targets: HWND,
    edit_up_targets: HWND,
    lbl_down_targets: HWND,
    edit_down_targets: HWND,

    btn_conn: HWND,
    btn_up: HWND,
    btn_down: HWND,
    btn_stop: HWND,
    btn_reset: HWND,
    btn_clear_log: HWND,
    btn_help: HWND,

    grp_log: HWND,
    edit_log: HWND,
}

impl Default for Controls {
    fn default() -> Self {
        let z = HWND::default();
        Self {
            grp_stats: z,
            lbl_mode: z,
            lbl_conn: z,
            lbl_up: z,
            lbl_down: z,
            lbl_sys: z,
            grp_cfg: z,
            lbl_streams: z,
            edit_streams: z,
            lbl_max_conn: z,
            edit_max_conn: z,
            lbl_hold_secs: z,
            edit_hold_secs: z,
            lbl_tcp_targets: z,
            edit_tcp_targets: z,
            lbl_up_targets: z,
            edit_up_targets: z,
            lbl_down_targets: z,
            edit_down_targets: z,
            btn_conn: z,
            btn_up: z,
            btn_down: z,
            btn_stop: z,
            btn_reset: z,
            btn_clear_log: z,
            btn_help: z,
            grp_log: z,
            edit_log: z,
        }
    }
}

struct WindowData {
    state: Arc<AppState>,
    engine: EngineHandle,
    controls: Controls,
    font: HFONT,
    owns_font: bool,
    seen_log_gen: u64,
    seen_log_seq: u64,
    last_mode: Option<AppMode>,
}

impl Drop for WindowData {
    fn drop(&mut self) {
        if self.owns_font && !self.font.0.is_null() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn set_window_text(hwnd: HWND, text: &str) {
    let wide = to_wide(text);
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(wide.as_ptr()));
    }
}

fn get_window_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len as usize) + 2];
        let copied = GetWindowTextW(hwnd, PWSTR(buf.as_mut_ptr()), buf.len() as i32);
        if copied <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..copied as usize])
    }
}

fn create_ui_font() -> (HFONT, bool) {
    unsafe {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        if SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS as u32,
            ncm.cbSize,
            (&mut ncm as *mut NONCLIENTMETRICSW).cast::<c_void>(),
            0,
        )
        .as_bool()
        {
            let font = CreateFontIndirectW(&ncm.lfMessageFont);
            if !font.0.is_null() {
                return (font, true);
            }
        }
        (HFONT(GetStockObject(DEFAULT_GUI_FONT).0), false)
    }
}

unsafe fn spawn_child(
    parent: HWND,
    ex_style: u32,
    class_name: PCWSTR,
    text: &str,
    style: u32,
    id: u16,
    font: HFONT,
) -> HWND {
    let wtext = to_wide(text);
    let hmenu = if id == 0 {
        None
    } else {
        Some(HMENU(id as usize as *mut c_void))
    };
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            class_name,
            PCWSTR(wtext.as_ptr()),
            style,
            0,
            0,
            100,
            24,
            Some(parent),
            hmenu,
            None,
            None,
        )
    };

    if !hwnd.0.is_null() && !font.0.is_null() {
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETFONT as u32,
                WPARAM(font.0 as usize),
                LPARAM(0),
            );
        }
    }
    hwnd
}

fn create_controls(hwnd: HWND, data: &mut WindowData) {
    let (font, owns_font) = create_ui_font();
    data.font = font;
    data.owns_font = owns_font;

    let grp_style = (WS_CHILD | WS_VISIBLE | BS_GROUPBOX) as u32;
    let lbl_style = (WS_CHILD | WS_VISIBLE) as u32;
    let edit_single_style = (WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL) as u32;
    let edit_num_style = edit_single_style | (ES_NUMBER as u32);
    let edit_multi_style =
        (WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL | ES_MULTILINE | ES_AUTOVSCROLL | ES_AUTOHSCROLL)
            as u32;
    let edit_log_style =
        (WS_CHILD | WS_VISIBLE | WS_VSCROLL | ES_MULTILINE | ES_AUTOVSCROLL | ES_READONLY) as u32;
    let btn_style = (WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON) as u32;
    let ex_edge = WS_EX_CLIENTEDGE as u32;

    unsafe {
        let c = &mut data.controls;

        c.grp_stats = spawn_child(
            hwnd,
            0,
            w!("BUTTON"),
            "实时状态与监控 (纯内存)",
            grp_style,
            0,
            font,
        );
        c.lbl_mode = spawn_child(
            hwnd,
            0,
            w!("STATIC"),
            "当前状态: 空闲 (就绪)",
            lbl_style,
            0,
            font,
        );
        c.lbl_conn = spawn_child(
            hwnd,
            0,
            w!("STATIC"),
            "连接测试: 当前 0 | 峰值 0 | 最大稳定 0 | 失败(超时/拒绝/资源/其他) 0/0/0/0",
            lbl_style,
            0,
            font,
        );
        c.lbl_up = spawn_child(
            hwnd,
            0,
            w!("STATIC"),
            "上行压测: 实时 0.00 Mbps | 峰值 0.00 Mbps | 累计 0 B | 活跃流 0 | 错误 0",
            lbl_style,
            0,
            font,
        );
        c.lbl_down = spawn_child(
            hwnd,
            0,
            w!("STATIC"),
            "下行压测: 实时 0.00 Mbps | 峰值 0.00 Mbps | 累计 0 B | 活跃流 0 | 错误 0",
            lbl_style,
            0,
            font,
        );
        c.lbl_sys = spawn_child(
            hwnd,
            0,
            w!("STATIC"),
            "系统指标: 本机 ESTABLISHED TCP: 0 | 进程句柄: 0 | 磁盘读写: 0 (纯内存)",
            lbl_style,
            0,
            font,
        );

        c.grp_cfg = spawn_child(
            hwnd,
            0,
            w!("BUTTON"),
            "测试参数与公网目标节点 (仅内存生效，支持临时编辑)",
            grp_style,
            0,
            font,
        );
        c.lbl_streams = spawn_child(
            hwnd,
            0,
            w!("STATIC"),
            "并发流数 (1-128):",
            lbl_style,
            0,
            font,
        );
        c.edit_streams = spawn_child(
            hwnd,
            ex_edge,
            w!("EDIT"),
            &DEFAULT_STREAM_CONCURRENCY.to_string(),
            edit_num_style,
            0,
            font,
        );
        c.lbl_max_conn = spawn_child(
            hwnd,
            0,
            w!("STATIC"),
            "连接探测上限:",
            lbl_style,
            0,
            font,
        );
        c.edit_max_conn = spawn_child(
            hwnd,
            ex_edge,
            w!("EDIT"),
            &DEFAULT_MAX_CONNS_CAP.to_string(),
            edit_num_style,
            0,
            font,
        );
        c.lbl_hold_secs = spawn_child(
            hwnd,
            0,
            w!("STATIC"),
            "稳定观察(秒):",
            lbl_style,
            0,
            font,
        );
        c.edit_hold_secs = spawn_child(
            hwnd,
            ex_edge,
            w!("EDIT"),
            &DEFAULT_HOLD_SECS.to_string(),
            edit_num_style,
            0,
            font,
        );

        c.lbl_tcp_targets = spawn_child(
            hwnd,
            0,
            w!("STATIC"),
            "连接测试目标 (ip:port，逗号或换行分隔):",
            lbl_style,
            0,
            font,
        );
        c.edit_tcp_targets = spawn_child(
            hwnd,
            ex_edge,
            w!("EDIT"),
            DEFAULT_TCP_TARGETS,
            edit_single_style,
            0,
            font,
        );

        c.lbl_up_targets = spawn_child(
            hwnd,
            0,
            w!("STATIC"),
            "上行测试目标 (http(s):// POST 端点 或 tcp://ip:port):",
            lbl_style,
            0,
            font,
        );
        c.edit_up_targets = spawn_child(
            hwnd,
            ex_edge,
            w!("EDIT"),
            DEFAULT_UPLOAD_TARGETS,
            edit_single_style,
            0,
            font,
        );

        c.lbl_down_targets = spawn_child(
            hwnd,
            0,
            w!("STATIC"),
            "下行测试目标 (每行一个公网大文件 URL，优先国内镜像/CDN):",
            lbl_style,
            0,
            font,
        );
        c.edit_down_targets = spawn_child(
            hwnd,
            ex_edge,
            w!("EDIT"),
            DEFAULT_DOWNLOAD_TARGETS,
            edit_multi_style,
            0,
            font,
        );

        c.btn_conn = spawn_child(
            hwnd,
            0,
            w!("BUTTON"),
            "开始最大连接数测试",
            btn_style,
            ID_BTN_CONN,
            font,
        );
        c.btn_up = spawn_child(
            hwnd,
            0,
            w!("BUTTON"),
            "开始持续上传压测",
            btn_style,
            ID_BTN_UP,
            font,
        );
        c.btn_down = spawn_child(
            hwnd,
            0,
            w!("BUTTON"),
            "开始持续下载压测",
            btn_style,
            ID_BTN_DOWN,
            font,
        );
        c.btn_stop = spawn_child(
            hwnd,
            0,
            w!("BUTTON"),
            "停止当前测试",
            btn_style,
            ID_BTN_STOP,
            font,
        );
        c.btn_reset = spawn_child(
            hwnd,
            0,
            w!("BUTTON"),
            "重置默认节点",
            btn_style,
            ID_BTN_RESET,
            font,
        );
        c.btn_clear_log = spawn_child(
            hwnd,
            0,
            w!("BUTTON"),
            "清空内存日志",
            btn_style,
            ID_BTN_CLEAR_LOG,
            font,
        );
        c.btn_help = spawn_child(
            hwnd,
            0,
            w!("BUTTON"),
            "说明与声明",
            btn_style,
            ID_BTN_HELP,
            font,
        );

        c.grp_log = spawn_child(
            hwnd,
            0,
            w!("BUTTON"),
            "运行日志 (内存环形缓冲，最多保留 400 条，可 Ctrl+C 复制)",
            grp_style,
            0,
            font,
        );
        c.edit_log = spawn_child(hwnd, ex_edge, w!("EDIT"), "", edit_log_style, 0, font);
        SendMessageW(c.edit_log, EM_SETLIMITTEXT, WPARAM(0), LPARAM(0));
    }
}

fn place(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let _ = MoveWindow(hwnd, x, y, w.max(10), h.max(10), true);
    }
}

fn layout_controls(hwnd: HWND, data: &WindowData) {
    let mut rc = RECT::default();
    unsafe {
        if !GetClientRect(hwnd, &mut rc).as_bool() {
            return;
        }
    }

    let cw = rc.right - rc.left;
    let ch = rc.bottom - rc.top;
    let m = 12;
    let inner_w = (cw - m * 2).max(400);
    let c = &data.controls;

    let stats_h = 148;
    place(c.grp_stats, m, m, inner_w, stats_h);
    let sx = m + 14;
    let sw = inner_w - 28;
    place(c.lbl_mode, sx, m + 22, sw, 20);
    place(c.lbl_conn, sx, m + 46, sw, 20);
    place(c.lbl_up, sx, m + 70, sw, 20);
    place(c.lbl_down, sx, m + 94, sw, 20);
    place(c.lbl_sys, sx, m + 118, sw, 20);

    let cfg_y = m + stats_h + 10;
    let cfg_h = 264;
    place(c.grp_cfg, m, cfg_y, inner_w, cfg_h);

    let row1_y = cfg_y + 24;
    place(c.lbl_streams, sx, row1_y + 3, 120, 20);
    place(c.edit_streams, sx + 124, row1_y, 70, 24);
    place(c.lbl_max_conn, sx + 214, row1_y + 3, 100, 20);
    place(c.edit_max_conn, sx + 318, row1_y, 90, 24);
    place(c.lbl_hold_secs, sx + 428, row1_y + 3, 100, 20);
    place(c.edit_hold_secs, sx + 532, row1_y, 70, 24);

    let row2_y = row1_y + 32;
    place(c.lbl_tcp_targets, sx, row2_y, sw, 18);
    place(c.edit_tcp_targets, sx, row2_y + 20, sw, 24);

    let row3_y = row2_y + 50;
    place(c.lbl_up_targets, sx, row3_y, sw, 18);
    place(c.edit_up_targets, sx, row3_y + 20, sw, 24);

    let row4_y = row3_y + 50;
    place(c.lbl_down_targets, sx, row4_y, sw, 18);
    place(c.edit_down_targets, sx, row4_y + 20, sw, 80);

    let btn_y = cfg_y + cfg_h + 10;
    let btn_h = 32;
    let gap = 8;
    let mut bx = m;

    let btns = [
        (c.btn_conn, 148),
        (c.btn_up, 138),
        (c.btn_down, 138),
        (c.btn_stop, 114),
        (c.btn_reset, 104),
        (c.btn_clear_log, 104),
        (c.btn_help, 92),
    ];
    for (btn, bw) in btns {
        place(btn, bx, btn_y, bw, btn_h);
        bx += bw + gap;
    }

    let log_y = btn_y + btn_h + 10;
    let log_h = (ch - log_y - m).max(120);
    place(c.grp_log, m, log_y, inner_w, log_h);
    place(c.edit_log, sx, log_y + 22, sw, (log_h - 34).max(80));
}

fn read_config_from_ui(data: &WindowData) -> TestConfig {
    let c = &data.controls;
    let stream_concurrency = get_window_text(c.edit_streams)
        .trim()
        .parse::<usize>()
        .unwrap_or(DEFAULT_STREAM_CONCURRENCY)
        .clamp(1, 128);
    let max_conn_cap = get_window_text(c.edit_max_conn)
        .trim()
        .parse::<usize>()
        .unwrap_or(DEFAULT_MAX_CONNS_CAP)
        .clamp(10, 65_535);
    let hold_secs = get_window_text(c.edit_hold_secs)
        .trim()
        .parse::<u64>()
        .unwrap_or(DEFAULT_HOLD_SECS)
        .clamp(2, 60);

    TestConfig {
        stream_concurrency,
        max_conn_cap,
        hold_secs,
        tcp_targets: parse_target_list(&get_window_text(c.edit_tcp_targets)),
        upload_targets: parse_target_list(&get_window_text(c.edit_up_targets)),
        download_targets: parse_target_list(&get_window_text(c.edit_down_targets)),
    }
}

fn refresh_ui(data: &mut WindowData) {
    let stats = &data.state.stats;
    let mode = stats.mode();
    let c = &data.controls;

    set_window_text(c.lbl_mode, &format!("当前状态: {}", mode.label_zh()));

    let conn_cur = stats.conn_current.load(Ordering::Relaxed);
    let conn_peak = stats.conn_peak.load(Ordering::Relaxed);
    let conn_stable = stats.conn_stable_max.load(Ordering::Relaxed);
    let conn_probe = stats.conn_target_probe.load(Ordering::Relaxed);
    let f_to = stats.conn_fail_timeout.load(Ordering::Relaxed);
    let f_ref = stats.conn_fail_refused.load(Ordering::Relaxed);
    let f_res = stats.conn_fail_resource.load(Ordering::Relaxed);
    let f_oth = stats.conn_fail_other.load(Ordering::Relaxed);
    set_window_text(
        c.lbl_conn,
        &format!(
            "连接测试: 当前 {conn_cur} (探测目标 {conn_probe}) | 峰值 {conn_peak} | 最大稳定 {conn_stable} | 失败(超时/拒绝/资源/其他) {f_to}/{f_ref}/{f_res}/{f_oth}"
        ),
    );

    let up_cur = stats.up_bps_current.load(Ordering::Relaxed);
    let up_peak = stats.up_bps_peak.load(Ordering::Relaxed);
    let up_bytes = stats.up_bytes_total.load(Ordering::Relaxed);
    let up_streams = stats.up_active_streams.load(Ordering::Relaxed);
    let up_err = stats.up_errors.load(Ordering::Relaxed);
    set_window_text(
        c.lbl_up,
        &format!(
            "上行压测: 实时 {} | 峰值 {} | 累计 {} | 活跃流 {up_streams} | 错误 {up_err}",
            format_mbps(up_cur),
            format_mbps(up_peak),
            format_bytes(up_bytes)
        ),
    );

    let dn_cur = stats.down_bps_current.load(Ordering::Relaxed);
    let dn_peak = stats.down_bps_peak.load(Ordering::Relaxed);
    let dn_bytes = stats.down_bytes_total.load(Ordering::Relaxed);
    let dn_streams = stats.down_active_streams.load(Ordering::Relaxed);
    let dn_err = stats.down_errors.load(Ordering::Relaxed);
    set_window_text(
        c.lbl_down,
        &format!(
            "下行压测: 实时 {} | 峰值 {} | 累计 {} | 活跃流 {dn_streams} | 错误 {dn_err}",
            format_mbps(dn_cur),
            format_mbps(dn_peak),
            format_bytes(dn_bytes)
        ),
    );

    let tcp_estab = stats.sys_tcp_estab.load(Ordering::Relaxed);
    let proc_handles = stats.proc_handles.load(Ordering::Relaxed);
    set_window_text(
        c.lbl_sys,
        &format!(
            "系统指标: 本机 ESTABLISHED TCP: {tcp_estab} | 当前进程句柄: {proc_handles} | 磁盘读写: 0 (纯内存)"
        ),
    );

    if data.last_mode != Some(mode) {
        data.last_mode = Some(mode);
        let is_idle = mode == AppMode::Idle;
        unsafe {
            let _ = EnableWindow(c.btn_conn, is_idle);
            let _ = EnableWindow(c.btn_up, is_idle);
            let _ = EnableWindow(c.btn_down, is_idle);
            let _ = EnableWindow(c.btn_reset, is_idle);
            let _ = EnableWindow(c.btn_stop, !is_idle);
        }
    }

    let (new_gen, new_seq, lines, full_reset) = data
        .state
        .log
        .fetch_since(data.seen_log_gen, data.seen_log_seq);

    if full_reset {
        let mut joined = lines.join("\r\n");
        if !joined.is_empty() {
            joined.push_str("\r\n");
        }
        set_window_text(c.edit_log, &joined);
        unsafe {
            let len = GetWindowTextLengthW(c.edit_log).max(0);
            SendMessageW(
                c.edit_log,
                EM_SETSEL,
                WPARAM(len as usize),
                LPARAM(len as isize),
            );
            SendMessageW(c.edit_log, EM_SCROLLCARET, WPARAM(0), LPARAM(0));
        }
    } else if !lines.is_empty() {
        let mut chunk = lines.join("\r\n");
        chunk.push_str("\r\n");
        let w_chunk = to_wide(&chunk);
        unsafe {
            let len = GetWindowTextLengthW(c.edit_log).max(0);
            SendMessageW(
                c.edit_log,
                EM_SETSEL,
                WPARAM(len as usize),
                LPARAM(len as isize),
            );
            SendMessageW(
                c.edit_log,
                EM_REPLACESEL,
                WPARAM(0),
                LPARAM(w_chunk.as_ptr() as isize),
            );
            SendMessageW(c.edit_log, EM_SCROLLCARET, WPARAM(0), LPARAM(0));
        }
    }

    data.seen_log_gen = new_gen;
    data.seen_log_seq = new_seq;
}

fn handle_command(hwnd: HWND, id: u16, data: &mut WindowData) {
    match id {
        ID_BTN_CONN => {
            let cfg = read_config_from_ui(data);
            if cfg.tcp_targets.is_empty() {
                unsafe {
                    MessageBoxW(
                        Some(hwnd),
                        w!("请先填写至少一个有效的 TCP 测试目标 (ip:port)。"),
                        w!("参数提示"),
                        (MB_OK | MB_ICONWARNING) as u32,
                    );
                }
                return;
            }
            data.engine.start_conn_test(cfg);
            refresh_ui(data);
        }
        ID_BTN_UP => {
            let cfg = read_config_from_ui(data);
            if cfg.upload_targets.is_empty() {
                unsafe {
                    MessageBoxW(
                        Some(hwnd),
                        w!("请先填写至少一个上传目标 (http(s)://... 或 tcp://ip:port)。"),
                        w!("参数提示"),
                        (MB_OK | MB_ICONWARNING) as u32,
                    );
                }
                return;
            }
            data.engine.start_upload_test(cfg);
            refresh_ui(data);
        }
        ID_BTN_DOWN => {
            let cfg = read_config_from_ui(data);
            if cfg.download_targets.is_empty() {
                unsafe {
                    MessageBoxW(
                        Some(hwnd),
                        w!("请先填写至少一个下载目标 URL。"),
                        w!("参数提示"),
                        (MB_OK | MB_ICONWARNING) as u32,
                    );
                }
                return;
            }
            data.engine.start_download_test(cfg);
            refresh_ui(data);
        }
        ID_BTN_STOP => {
            data.engine.stop();
        }
        ID_BTN_RESET => {
            set_window_text(
                data.controls.edit_streams,
                &DEFAULT_STREAM_CONCURRENCY.to_string(),
            );
            set_window_text(
                data.controls.edit_max_conn,
                &DEFAULT_MAX_CONNS_CAP.to_string(),
            );
            set_window_text(data.controls.edit_hold_secs, &DEFAULT_HOLD_SECS.to_string());
            set_window_text(data.controls.edit_tcp_targets, DEFAULT_TCP_TARGETS);
            set_window_text(data.controls.edit_up_targets, DEFAULT_UPLOAD_TARGETS);
            set_window_text(data.controls.edit_down_targets, DEFAULT_DOWNLOAD_TARGETS);
            data.state.log.push("已恢复内存默认节点与参数配置。");
        }
        ID_BTN_CLEAR_LOG => {
            data.state.log.clear();
            refresh_ui(data);
        }
        ID_BTN_HELP => {
            let msg = w!(
                "【纯内存运行声明】\r\n\
                本工具全程不读写磁盘、不写注册表、不产生任何临时文件，所有配置与日志均保存在内存中，关闭后即释放。\r\n\r\n\
                【功能说明】\r\n\
                1. 最大连接数测试：采用阶梯递增 + 稳定窗口 + 二分收敛探测本机对公网节点的最大稳定并发 TCP 连接数。\r\n\
                2. 持续上传压测：在内存中生成 1 MiB 不可压缩随机载荷并多流并发发送，支持 HTTP POST 与 tcp://ip:port 原始流。\r\n\
                3. 持续下载压测：多流并发请求公网大文件，收到的数据块直接在内存中丢弃，仅统计实时与峰值速率。\r\n\r\n\
                【合规提示与注意事项】\r\n\
                - 仅限测试本机出口能力或已获明确授权的目标，持续打满公网带宽可能触发运营商 QoS/限速或目标端封禁。\r\n\
                - Windows 默认动态端口范围较小，如高连接数测试出现系统资源不足，可在管理员终端执行：\r\n\
                  netsh int ipv4 set dynamicport tcp start=10000 num=55000"
            );
            unsafe {
                MessageBoxW(
                    Some(hwnd),
                    msg,
                    w!("使用说明与合规声明"),
                    (MB_OK | MB_ICONINFORMATION) as u32,
                );
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE as u32 {
        let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        }
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowData;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let data = unsafe { &mut *ptr };

    match msg as i32 {
        WM_CREATE => {
            create_controls(hwnd, data);
            layout_controls(hwnd, data);
            refresh_ui(data);
            unsafe {
                SetTimer(Some(hwnd), TIMER_REFRESH_ID, 250, None);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            layout_controls(hwnd, data);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let mmi = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
            mmi.ptMinTrackSize.x = 840;
            mmi.ptMinTrackSize.y = 620;
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_REFRESH_ID {
                refresh_ui(data);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as u16;
            let code = ((wparam.0 >> 16) & 0xFFFF) as u16;
            if code == BN_CLICKED as u16 {
                handle_command(hwnd, id, data);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_REFRESH_ID);
            }
            data.engine.stop();
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

pub fn run_gui(state: Arc<AppState>, engine: EngineHandle) -> Result<(), String> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()) };
    if hinstance.0.is_null() {
        return Err("GetModuleHandleW failed".to_string());
    }

    let class_name = w!("WinNetStressMainWnd");
    let hcursor = unsafe { LoadCursorW(None, IDC_ARROW) };
    if hcursor.0.is_null() {
        return Err("LoadCursorW failed".to_string());
    }
    let hbrush = unsafe { GetSysColorBrush(COLOR_BTNFACE) };

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: (CS_HREDRAW | CS_VREDRAW) as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance,
        hCursor: hcursor,
        hbrBackground: hbrush,
        lpszClassName: class_name,
        ..Default::default()
    };

    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        return Err("RegisterClassExW failed".to_string());
    }

    let boxed = Box::new(WindowData {
        state,
        engine,
        controls: Controls::default(),
        font: HFONT::default(),
        owns_font: false,
        seen_log_gen: 0,
        seen_log_seq: 0,
        last_mode: None,
    });
    let raw_ptr = Box::into_raw(boxed);

    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name,
            w!("公网连接与带宽极限测试工具 (纯内存 / WinAPI)"),
            (WS_OVERLAPPEDWINDOW | WS_VISIBLE) as u32,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            960,
            740,
            None,
            None,
            Some(hinstance),
            Some(raw_ptr.cast::<c_void>()),
        )
    };

    if hwnd.0.is_null() {
        unsafe {
            drop(Box::from_raw(raw_ptr));
        }
        return Err("CreateWindowExW failed".to_string());
    }

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    Ok(())
}
