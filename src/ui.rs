//! Native Win32 GUI with zero-flicker GDI rendering, Per-Monitor DPI scaling,
//! Windows system style & font adaptation, custom vector window icon, real-time
//! speed curves (upload/download/duplex), NIC monitoring, node ranking and
//! pure in-memory state updates.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use windows::Win32::*;
use windows::core::{BOOL, PCWSTR, PWSTR, w};

use crate::engine::EngineHandle;
use crate::state::{
    AppMode, AppState, DEFAULT_DOWNLOAD_TARGETS, DEFAULT_HOLD_SECS, DEFAULT_MAX_CONNS_CAP,
    DEFAULT_STREAM_CONCURRENCY, DEFAULT_TCP_TARGETS, DEFAULT_UPLOAD_TARGETS, TestConfig,
    format_bytes, format_mbps, parse_target_list,
};

const ID_BTN_CONN: usize = 1001;
const ID_BTN_UP: usize = 1002;
const ID_BTN_DOWN: usize = 1003;
const ID_BTN_DUPLEX: usize = 1004;
const ID_BTN_STOP: usize = 1005;
const ID_BTN_RESET: usize = 1006;
const ID_BTN_CLEAR_LOG: usize = 1007;
const ID_BTN_HELP: usize = 1008;
const ID_CHK_CONTINUOUS: usize = 1010;
const ID_TIMER_REFRESH: usize = 2001;
const WM_APP_WARMUP: u32 = (WM_APP as u32) + 1;

// Classic Win32 Edit control messages omitted from windows crate UI bindings.
const EM_SETSEL: u32 = 0x00B1;
const EM_SCROLLCARET: u32 = 0x00B7;
const EM_REPLACESEL: u32 = 0x00C2;
const EM_SETLIMITTEXT: u32 = 0x00C5;

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(((b as u32) << 16) | ((g as u32) << 8) | (r as u32))
}

#[derive(Default)]
struct LabelCache {
    status: String,
    mem_badge: String,
    conn_row1: String,
    conn_row2: String,
    conn_row3: String,
    up_row: String,
    up_detail: String,
    down_row: String,
    down_detail: String,
    duplex_row: String,
    nic_row: String,
    rank_row: String,
}

struct SystemColors {
    bg: COLORREF,
    card: COLORREF,
    text_primary: COLORREF,
    text_muted: COLORREF,
    brand: COLORREF,
    active: COLORREF,
    idle: COLORREF,
}

impl SystemColors {
    unsafe fn query() -> Self {
        let bg = COLORREF(unsafe { GetSysColor(COLOR_BTNFACE) });
        let card = COLORREF(unsafe { GetSysColor(COLOR_WINDOW) });
        let text_primary = COLORREF(unsafe { GetSysColor(COLOR_WINDOWTEXT) });
        let text_muted = COLORREF(unsafe { GetSysColor(COLOR_GRAYTEXT) });
        let hotlight = COLORREF(unsafe { GetSysColor(COLOR_HOTLIGHT) });

        let bg_luma = color_luma(bg);
        let brand = if hotlight.0 != 0 {
            hotlight
        } else if bg_luma < 128 {
            rgb(96, 165, 250)
        } else {
            rgb(15, 76, 129)
        };
        let active = if bg_luma < 128 {
            rgb(52, 211, 153)
        } else {
            rgb(5, 122, 85)
        };
        let idle = if bg_luma < 128 {
            rgb(56, 189, 248)
        } else {
            rgb(3, 105, 161)
        };

        Self {
            bg,
            card,
            text_primary,
            text_muted,
            brand,
            active,
            idle,
        }
    }
}

fn color_luma(c: COLORREF) -> u32 {
    let r = c.0 & 0xFF;
    let g = (c.0 >> 8) & 0xFF;
    let b = (c.0 >> 16) & 0xFF;
    (r * 299 + g * 587 + b * 114) / 1000
}

struct ThemeResources {
    dpi: u32,
    line_h: i32,
    title_h: i32,
    font_title: HFONT,
    font_ui: HFONT,
    font_bold: HFONT,
    font_log: HFONT,
    font_small: HFONT,
    colors: SystemColors,
    brush_bg: HBRUSH,
    brush_card: HBRUSH,
}

impl ThemeResources {
    unsafe fn create(dpi: u32) -> Self {
        let safe_dpi = dpi.max(96);
        let (font_title, font_ui, font_bold, font_log, font_small, line_h, title_h) =
            unsafe { create_font_bundle(safe_dpi) };
        let colors = unsafe { SystemColors::query() };
        let brush_bg = unsafe { CreateSolidBrush(colors.bg) };
        let brush_card = unsafe { CreateSolidBrush(colors.card) };

        Self {
            dpi: safe_dpi,
            line_h,
            title_h,
            font_title,
            font_ui,
            font_bold,
            font_log,
            font_small,
            colors,
            brush_bg,
            brush_card,
        }
    }

    unsafe fn rebuild_fonts(&mut self, dpi: u32) {
        let safe_dpi = dpi.max(96);
        unsafe {
            delete_font(&mut self.font_title);
            delete_font(&mut self.font_ui);
            delete_font(&mut self.font_bold);
            delete_font(&mut self.font_log);
            delete_font(&mut self.font_small);
        }
        let (font_title, font_ui, font_bold, font_log, font_small, line_h, title_h) =
            unsafe { create_font_bundle(safe_dpi) };
        self.dpi = safe_dpi;
        self.font_title = font_title;
        self.font_ui = font_ui;
        self.font_bold = font_bold;
        self.font_log = font_log;
        self.font_small = font_small;
        self.line_h = line_h;
        self.title_h = title_h;
    }

    unsafe fn refresh_colors(&mut self) {
        self.colors = unsafe { SystemColors::query() };
        if !self.brush_bg.0.is_null() {
            let _ = unsafe { DeleteObject(HGDIOBJ(self.brush_bg.0)) };
        }
        if !self.brush_card.0.is_null() {
            let _ = unsafe { DeleteObject(HGDIOBJ(self.brush_card.0)) };
        }
        self.brush_bg = unsafe { CreateSolidBrush(self.colors.bg) };
        self.brush_card = unsafe { CreateSolidBrush(self.colors.card) };
    }

    unsafe fn destroy(&mut self) {
        unsafe {
            delete_font(&mut self.font_title);
            delete_font(&mut self.font_ui);
            delete_font(&mut self.font_bold);
            delete_font(&mut self.font_log);
            delete_font(&mut self.font_small);
            if !self.brush_bg.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.brush_bg.0));
                self.brush_bg = HBRUSH(std::ptr::null_mut());
            }
            if !self.brush_card.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.brush_card.0));
                self.brush_card = HBRUSH(std::ptr::null_mut());
            }
        }
    }
}

unsafe fn delete_font(font: &mut HFONT) {
    if !font.0.is_null() {
        let _ = unsafe { DeleteObject(HGDIOBJ(font.0)) };
        *font = HFONT(std::ptr::null_mut());
    }
}

unsafe fn create_font_bundle(dpi: u32) -> (HFONT, HFONT, HFONT, HFONT, HFONT, i32, i32) {
    let hdc = unsafe { GetDC(None) };
    let sys_dpi = if !hdc.0.is_null() {
        let y = unsafe { GetDeviceCaps(Some(hdc), LOGPIXELSY) };
        if y > 0 { y as u32 } else { 96 }
    } else {
        96
    };

    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let has_ncm = unsafe {
        SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS as u32,
            ncm.cbSize,
            (&mut ncm as *mut NONCLIENTMETRICSW).cast::<c_void>(),
            0,
        )
    }
    .as_bool();

    let mut base_lf = if has_ncm && ncm.lfMessageFont.lfFaceName[0] != 0 {
        ncm.lfMessageFont
    } else {
        make_logfont("Microsoft YaHei UI", 9, FW_NORMAL, dpi as i32)
    };

    let raw_pt = if base_lf.lfHeight != 0 {
        ((base_lf.lfHeight.unsigned_abs() * 72) + (sys_dpi / 2)) / sys_dpi
    } else {
        9
    };
    let base_pt = raw_pt.clamp(9, 12) as i32;

    base_lf.lfQuality = CLEARTYPE_QUALITY as u8;
    base_lf.lfHeight = -((base_pt * dpi as i32) / 72);
    base_lf.lfWeight = FW_NORMAL;
    let font_ui = unsafe { CreateFontIndirectW(&base_lf) };

    let mut bold_lf = base_lf;
    bold_lf.lfWeight = FW_SEMIBOLD;
    let font_bold = unsafe { CreateFontIndirectW(&bold_lf) };

    let mut title_lf = base_lf;
    title_lf.lfHeight = -(((base_pt + 2) * dpi as i32) / 72);
    title_lf.lfWeight = FW_BOLD;
    let font_title = unsafe { CreateFontIndirectW(&title_lf) };

    let font_log = unsafe { CreateFontIndirectW(&base_lf) };

    let mut small_lf = base_lf;
    small_lf.lfHeight = -(((base_pt - 1).max(8) * dpi as i32) / 72);
    let font_small = unsafe { CreateFontIndirectW(&small_lf) };

    let line_h = unsafe { measure_font_height_dc(hdc, font_ui, dpi) };
    let title_h = unsafe { measure_font_height_dc(hdc, font_title, dpi) };

    if !hdc.0.is_null() {
        let _ = unsafe { ReleaseDC(None, hdc) };
    }

    (font_title, font_ui, font_bold, font_log, font_small, line_h, title_h)
}

fn make_logfont(face: &str, pt_size: i32, weight: i32, effective_dpi: i32) -> LOGFONTW {
    let mut lf = LOGFONTW {
        lfHeight: -((pt_size * effective_dpi) / 72),
        lfWeight: weight,
        lfCharSet: DEFAULT_CHARSET as u8,
        lfQuality: CLEARTYPE_QUALITY as u8,
        ..Default::default()
    };

    let wide: Vec<u16> = face.encode_utf16().collect();
    let copy_len = wide.len().min(lf.lfFaceName.len() - 1);
    lf.lfFaceName[..copy_len].copy_from_slice(&wide[..copy_len]);
    lf.lfFaceName[copy_len] = 0;
    lf
}

unsafe fn measure_font_height_dc(hdc: HDC, font: HFONT, dpi: u32) -> i32 {
    let fallback = scale_px(18, dpi);
    if hdc.0.is_null() {
        return fallback;
    }
    let prev = unsafe { SelectObject(hdc, HGDIOBJ(font.0)) };
    let mut tm = TEXTMETRICW::default();
    let ok = unsafe { GetTextMetricsW(hdc, &mut tm) }.as_bool();
    let _ = unsafe { SelectObject(hdc, prev) };
    if ok && tm.tmHeight > 0 {
        tm.tmHeight
    } else {
        fallback
    }
}

fn scale_px(px_at_96: i32, dpi: u32) -> i32 {
    ((px_at_96 as i64 * dpi.max(96) as i64) + 48) as i32 / 96
}

/// Generates a custom 32-bit ARGB vector icon in memory (azure rounded badge +
/// speed-gauge arc + dual upload/download arrows) for the titlebar & taskbar.
unsafe fn create_app_icon(size: i32) -> HICON {
    let sz = size.clamp(16, 128);
    let mut biv5 = BITMAPV5HEADER {
        bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: sz,
        bV5Height: -sz, // top-down ARGB DIB
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS as u32,
        bV5RedMask: 0x00FF_0000,
        bV5GreenMask: 0x0000_FF00,
        bV5BlueMask: 0x0000_00FF,
        bV5AlphaMask: 0xFF00_0000,
        ..Default::default()
    };

    let mut bits_ptr: *mut c_void = std::ptr::null_mut();
    let hbm_color = unsafe {
        CreateDIBSection(
            None,
            (&mut biv5 as *mut BITMAPV5HEADER).cast::<BITMAPINFO>(),
            DIB_RGB_COLORS as u32,
            &mut bits_ptr,
            None,
            0,
        )
    };
    if hbm_color.0.is_null() || bits_ptr.is_null() {
        if !hbm_color.0.is_null() {
            let _ = unsafe { DeleteObject(HGDIOBJ(hbm_color.0)) };
        }
        return HICON(std::ptr::null_mut());
    }

    let pixel_count = (sz * sz) as usize;
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits_ptr.cast::<u32>(), pixel_count) };
    let inv = 1.0f32 / (sz as f32);

    for py in 0..sz {
        for px in 0..sz {
            let mut acc_r = 0.0f32;
            let mut acc_g = 0.0f32;
            let mut acc_b = 0.0f32;
            let mut acc_a = 0.0f32;

            for sy in 0..2 {
                for sx in 0..2 {
                    let u = (px as f32 + (sx as f32 + 0.5) * 0.5) * inv * 2.0 - 1.0;
                    let v = (py as f32 + (sy as f32 + 0.5) * 0.5) * inv * 2.0 - 1.0;
                    let [r, g, b, a] = sample_icon_pixel(u, v);
                    acc_r += r * a;
                    acc_g += g * a;
                    acc_b += b * a;
                    acc_a += a;
                }
            }

            let a = (acc_a * 0.25).clamp(0.0, 1.0);
            let (r, g, b) = if acc_a > 0.001 {
                (
                    (acc_r / acc_a).clamp(0.0, 1.0),
                    (acc_g / acc_a).clamp(0.0, 1.0),
                    (acc_b / acc_a).clamp(0.0, 1.0),
                )
            } else {
                (0.0, 0.0, 0.0)
            };

            let aa = (a * 255.0).round() as u32;
            let rr = (r * 255.0).round() as u32;
            let gg = (g * 255.0).round() as u32;
            let bb = (b * 255.0).round() as u32;
            pixels[(py * sz + px) as usize] = (aa << 24) | (rr << 16) | (gg << 8) | bb;
        }
    }

    let hbm_mask = unsafe { CreateBitmap(sz, sz, 1, 1, None) };
    if hbm_mask.0.is_null() {
        let _ = unsafe { DeleteObject(HGDIOBJ(hbm_color.0)) };
        return HICON(std::ptr::null_mut());
    }

    let ii = ICONINFO {
        fIcon: BOOL(1),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: hbm_mask,
        hbmColor: hbm_color,
    };
    let hicon = unsafe { CreateIconIndirect(&ii) };
    let _ = unsafe { DeleteObject(HGDIOBJ(hbm_color.0)) };
    let _ = unsafe { DeleteObject(HGDIOBJ(hbm_mask.0)) };
    hicon
}

fn sample_icon_pixel(u: f32, v: f32) -> [f32; 4] {
    let half = 0.88f32;
    let rad = 0.26f32;
    let qx = u.abs() - (half - rad);
    let qy = v.abs() - (half - rad);
    let sdf = (qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0)) - rad;
    if sdf > 0.03 {
        return [0.0, 0.0, 0.0, 0.0];
    }
    let badge_alpha = ((-sdf + 0.03) / 0.06).clamp(0.0, 1.0);

    let t = ((u + v + 1.8) / 3.6).clamp(0.0, 1.0);
    let mut r = 0.02 * (1.0 - t) + 0.06 * t;
    let mut g = 0.52 * (1.0 - t) + 0.24 * t;
    let mut b = 0.82 * (1.0 - t) + 0.52 * t;

    if sdf > -0.055 {
        r = r * 0.72 + 0.55 * 0.28;
        g = g * 0.72 + 0.85 * 0.28;
        b = b * 0.72 + 1.00 * 0.28;
    }

    let dist = u.hypot(v);
    let angle = v.atan2(u);
    let in_arc = !(angle > 0.72 && angle < 2.42);
    if in_arc && (dist - 0.62).abs() < 0.055 {
        return [0.85, 0.95, 1.0, badge_alpha];
    }

    // Left upward arrow (Upload)
    let up_shaft = (u + 0.19).abs() < 0.075 && v > -0.20 && v < 0.36;
    let up_head = v >= -0.44 && v <= -0.10 && (u + 0.19).abs() <= (v + 0.44) * 0.78 + 0.02;
    if up_shaft || up_head {
        return [1.0, 1.0, 1.0, badge_alpha];
    }

    // Right downward arrow (Download)
    let dn_shaft = (u - 0.19).abs() < 0.075 && v > -0.36 && v < 0.20;
    let dn_head = v >= 0.10 && v <= 0.44 && (u - 0.19).abs() <= (0.44 - v) * 0.78 + 0.02;
    if dn_shaft || dn_head {
        return [0.49, 0.89, 1.0, badge_alpha];
    }

    [r, g, b, badge_alpha]
}

struct UiState {
    app: Arc<AppState>,
    engine: EngineHandle,
    theme: ThemeResources,
    cache: LabelCache,
    last_mode_active: Option<bool>,
    log_gen: u64,
    log_seq: u64,
    chart_rect: RECT,

    // Header
    lbl_title: HWND,
    lbl_status: HWND,
    lbl_mem_badge: HWND,

    // Config group
    grp_config: HWND,
    lbl_streams: HWND,
    edit_streams: HWND,
    lbl_max_conns: HWND,
    edit_max_conns: HWND,
    lbl_hold_secs: HWND,
    edit_hold_secs: HWND,
    chk_continuous: HWND,
    lbl_tcp_targets: HWND,
    edit_tcp_targets: HWND,
    lbl_up_targets: HWND,
    edit_up_targets: HWND,
    lbl_down_targets: HWND,
    edit_down_targets: HWND,

    // Action buttons
    btn_conn: HWND,
    btn_up: HWND,
    btn_down: HWND,
    btn_duplex: HWND,
    btn_stop: HWND,
    btn_reset: HWND,
    btn_clear_log: HWND,
    btn_help: HWND,

    // Live metrics group
    grp_metrics: HWND,
    lbl_conn_row1: HWND,
    lbl_conn_row2: HWND,
    lbl_conn_row3: HWND,
    lbl_up_row: HWND,
    lbl_up_detail: HWND,
    lbl_down_row: HWND,
    lbl_down_detail: HWND,

    // Extended metrics
    grp_bandwidth: HWND,
    lbl_duplex_row: HWND,
    lbl_nic_row: HWND,
    lbl_rank_row: HWND,

    // Chart
    grp_chart: HWND,

    // Log group
    grp_log: HWND,
    edit_log: HWND,
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn set_text(hwnd: HWND, text: &str) {
    let wide = to_wide_null(text);
    let _ = unsafe { SetWindowTextW(hwnd, PCWSTR(wide.as_ptr())) };
}

unsafe fn set_cached_text(hwnd: HWND, cached: &mut String, next: String) {
    if *cached != next {
        unsafe { set_text(hwnd, &next) };
        *cached = next;
    }
}

unsafe fn get_text(hwnd: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len as usize) + 2];
    let copied = unsafe { GetWindowTextW(hwnd, PWSTR(buf.as_mut_ptr()), buf.len() as i32) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..copied as usize])
}

unsafe fn set_font(hwnd: HWND, font: HFONT) {
    let _ = unsafe { SendMessageW(hwnd, WM_SETFONT as u32, WPARAM(font.0 as usize), LPARAM(1)) };
}

#[allow(clippy::too_many_arguments)]
unsafe fn create_ctrl(
    class_name: PCWSTR,
    text: &str,
    style: u32,
    ex_style: u32,
    parent: HWND,
    id: usize,
    hinst: HINSTANCE,
    font: HFONT,
) -> HWND {
    let wide = to_wide_null(text);
    let hmenu = if id == 0 {
        None
    } else {
        Some(HMENU(id as *mut c_void))
    };
    let is_groupbox = (style & (BS_GROUPBOX as u32)) == (BS_GROUPBOX as u32);
    let final_style = if is_groupbox {
        style
    } else {
        style | (WS_CLIPSIBLINGS as u32)
    };
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            class_name,
            PCWSTR(wide.as_ptr()),
            final_style,
            0,
            0,
            10,
            10,
            Some(parent),
            hmenu,
            Some(hinst),
            None,
        )
    };
    unsafe { set_font(hwnd, font) };
    hwnd
}

impl UiState {
    unsafe fn apply_fonts(&self) {
        let t = &self.theme;
        unsafe {
            set_font(self.lbl_title, t.font_title);
            set_font(self.lbl_status, t.font_bold);
            set_font(self.lbl_mem_badge, t.font_small);

            set_font(self.grp_config, t.font_bold);
            set_font(self.lbl_streams, t.font_ui);
            set_font(self.edit_streams, t.font_ui);
            set_font(self.lbl_max_conns, t.font_ui);
            set_font(self.edit_max_conns, t.font_ui);
            set_font(self.lbl_hold_secs, t.font_ui);
            set_font(self.edit_hold_secs, t.font_ui);
            set_font(self.chk_continuous, t.font_ui);
            set_font(self.lbl_tcp_targets, t.font_ui);
            set_font(self.edit_tcp_targets, t.font_ui);
            set_font(self.lbl_up_targets, t.font_ui);
            set_font(self.edit_up_targets, t.font_ui);
            set_font(self.lbl_down_targets, t.font_ui);
            set_font(self.edit_down_targets, t.font_ui);

            set_font(self.btn_conn, t.font_bold);
            set_font(self.btn_up, t.font_bold);
            set_font(self.btn_down, t.font_bold);
            set_font(self.btn_duplex, t.font_bold);
            set_font(self.btn_stop, t.font_bold);
            set_font(self.btn_reset, t.font_ui);
            set_font(self.btn_clear_log, t.font_ui);
            set_font(self.btn_help, t.font_ui);

            set_font(self.grp_metrics, t.font_bold);
            set_font(self.lbl_conn_row1, t.font_bold);
            set_font(self.lbl_conn_row2, t.font_small);
            set_font(self.lbl_conn_row3, t.font_small);
            set_font(self.lbl_up_row, t.font_ui);
            set_font(self.lbl_up_detail, t.font_small);
            set_font(self.lbl_down_row, t.font_ui);
            set_font(self.lbl_down_detail, t.font_small);

            set_font(self.grp_bandwidth, t.font_bold);
            set_font(self.lbl_duplex_row, t.font_bold);
            set_font(self.lbl_nic_row, t.font_small);
            set_font(self.lbl_rank_row, t.font_small);

            set_font(self.grp_chart, t.font_bold);
            set_font(self.grp_log, t.font_bold);
            set_font(self.edit_log, t.font_log);
        }
    }

    unsafe fn layout(&mut self, width: i32, height: i32) {
        let dpi = self.theme.dpi;
        let s = |px: i32| scale_px(px, dpi);
        let lh = self.theme.line_h;
        let th = self.theme.title_h;

        let margin = s(14);
        let inner_pad = s(12);
        let gap = s(8);
        let row_gap = s(6);
        let w = (width - margin * 2).max(s(760));

        let row_h = lh + s(10);
        let small_row_h = lh + s(6);
        let edit_h = lh + s(8);
        let multi_edit_h = lh * 3 + s(16);
        let btn_h = lh + s(16);

        let mut hdwp = unsafe { BeginDeferWindowPos(50) };
        let defer = |hdwp: &mut HDWP, hwnd: HWND, x: i32, y: i32, cx: i32, cy: i32| {
            if !hdwp.0.is_null() {
                *hdwp = unsafe {
                    DeferWindowPos(
                        *hdwp,
                        hwnd,
                        None,
                        x,
                        y,
                        cx.max(1),
                        cy.max(1),
                        (SWP_NOZORDER | SWP_NOACTIVATE) as u32,
                    )
                };
            } else {
                let _ = unsafe { MoveWindow(hwnd, x, y, cx.max(1), cy.max(1), true) };
            }
        };
        let defer_bottom = |hdwp: &mut HDWP, hwnd: HWND, x: i32, y: i32, cx: i32, cy: i32| {
            if !hdwp.0.is_null() {
                *hdwp = unsafe {
                    DeferWindowPos(
                        *hdwp,
                        hwnd,
                        Some(HWND_BOTTOM),
                        x,
                        y,
                        cx.max(1),
                        cy.max(1),
                        SWP_NOACTIVATE as u32,
                    )
                };
            } else {
                let _ = unsafe {
                    SetWindowPos(
                        hwnd,
                        Some(HWND_BOTTOM),
                        x,
                        y,
                        cx.max(1),
                        cy.max(1),
                        SWP_NOACTIVATE as u32,
                    )
                };
            }
        };

        let mut y = s(10);

        // Top header bar
        let title_w = (w * 52) / 100;
        let status_w = w - title_w - gap;
        let title_row_h = th + s(6);
        defer(&mut hdwp, self.lbl_title, margin, y, title_w, title_row_h);
        defer(
            &mut hdwp,
            self.lbl_status,
            margin + title_w + gap,
            y + s(2),
            status_w,
            title_row_h - s(2),
        );
        y += title_row_h + s(2);

        defer(&mut hdwp, self.lbl_mem_badge, margin, y, w, lh + s(4));
        y += lh + s(10);

        // Group 1: Config
        let cfg_top = y;
        let cfg_title_offset = lh + s(10);
        let cfg_inner_w = w - inner_pad * 2;
        let cfg_x = margin + inner_pad;
        let mut cy = cfg_top + cfg_title_offset;

        let lbl_s_w = s(136);
        let edt_s_w = s(56);
        let lbl_m_w = s(158);
        let edt_m_w = s(74);
        let lbl_h_w = s(104);
        let edt_h_w = s(48);

        let mut rx = cfg_x;
        defer(&mut hdwp, self.lbl_streams, rx, cy, lbl_s_w, edit_h);
        rx += lbl_s_w + s(4);
        defer(&mut hdwp, self.edit_streams, rx, cy, edt_s_w, edit_h);
        rx += edt_s_w + s(12);

        defer(&mut hdwp, self.lbl_max_conns, rx, cy, lbl_m_w, edit_h);
        rx += lbl_m_w + s(4);
        defer(&mut hdwp, self.edit_max_conns, rx, cy, edt_m_w, edit_h);
        rx += edt_m_w + s(12);

        defer(&mut hdwp, self.lbl_hold_secs, rx, cy, lbl_h_w, edit_h);
        rx += lbl_h_w + s(4);
        defer(&mut hdwp, self.edit_hold_secs, rx, cy, edt_h_w, edit_h);
        rx += edt_h_w + s(12);

        let chk_w = (cfg_x + cfg_inner_w - rx).max(s(146));
        defer(&mut hdwp, self.chk_continuous, rx, cy, chk_w, edit_h);
        cy += edit_h + row_gap;

        let target_lbl_w = s(188);
        let target_edt_w = (cfg_inner_w - target_lbl_w - gap).max(s(220));
        let target_edt_x = cfg_x + target_lbl_w + gap;

        defer(&mut hdwp, self.lbl_tcp_targets, cfg_x, cy, target_lbl_w, edit_h);
        defer(
            &mut hdwp,
            self.edit_tcp_targets,
            target_edt_x,
            cy,
            target_edt_w,
            edit_h,
        );
        cy += edit_h + row_gap;

        defer(&mut hdwp, self.lbl_up_targets, cfg_x, cy, target_lbl_w, edit_h);
        defer(
            &mut hdwp,
            self.edit_up_targets,
            target_edt_x,
            cy,
            target_edt_w,
            edit_h,
        );
        cy += edit_h + row_gap;

        defer(
            &mut hdwp,
            self.lbl_down_targets,
            cfg_x,
            cy,
            target_lbl_w,
            edit_h,
        );
        defer(
            &mut hdwp,
            self.edit_down_targets,
            target_edt_x,
            cy,
            target_edt_w,
            multi_edit_h,
        );
        cy += multi_edit_h + s(12);

        let cfg_h = cy - cfg_top;
        defer_bottom(&mut hdwp, self.grp_config, margin, cfg_top, w, cfg_h);
        y = cfg_top + cfg_h + s(10);

        // Action buttons row (8 buttons)
        let b_gap = s(6);
        let avail = w - b_gap * 7;
        let weights = [118, 118, 118, 128, 112, 82, 82, 82];
        let total_w: i32 = weights.iter().sum();
        let btns = [
            self.btn_conn,
            self.btn_up,
            self.btn_down,
            self.btn_duplex,
            self.btn_stop,
            self.btn_reset,
            self.btn_clear_log,
            self.btn_help,
        ];
        let mut bx = margin;
        for (i, &btn) in btns.iter().enumerate() {
            let bw = if i == btns.len() - 1 {
                margin + w - bx
            } else {
                (avail * weights[i]) / total_w
            };
            defer(&mut hdwp, btn, bx, y, bw, btn_h);
            bx += bw + b_gap;
        }
        y += btn_h + s(10);

        // Group 2: Metrics (5 rows: conn1,2,3, up, down)
        let met_top = y;
        let met_h = cfg_title_offset + row_h + small_row_h * 2 + row_h * 2 + s(10);
        defer_bottom(&mut hdwp, self.grp_metrics, margin, met_top, w, met_h);

        let mut my = met_top + cfg_title_offset;
        defer(&mut hdwp, self.lbl_conn_row1, cfg_x, my, cfg_inner_w, row_h);
        my += row_h;
        defer(&mut hdwp, self.lbl_conn_row2, cfg_x, my, cfg_inner_w, small_row_h);
        my += small_row_h;
        defer(&mut hdwp, self.lbl_conn_row3, cfg_x, my, cfg_inner_w, small_row_h);
        my += small_row_h;
        defer(&mut hdwp, self.lbl_up_row, cfg_x, my, cfg_inner_w, row_h);
        my += row_h;
        defer(&mut hdwp, self.lbl_down_row, cfg_x, my, cfg_inner_w, row_h);
        y = met_top + met_h + s(10);

        // Group 3: Bandwidth detail (up_detail, down_detail, duplex, nic, rank)
        let det_top = y;
        let det_h = cfg_title_offset + row_h + small_row_h * 4 + s(10);
        // Actually we have: up_detail (small), down_detail (small), duplex (bold), nic (small), rank (small) => 5 rows
        defer_bottom(&mut hdwp, self.grp_bandwidth, margin, det_top, w, det_h);
        let mut dy = det_top + cfg_title_offset;
        defer(&mut hdwp, self.lbl_up_detail, cfg_x, dy, cfg_inner_w, small_row_h);
        dy += small_row_h;
        defer(&mut hdwp, self.lbl_down_detail, cfg_x, dy, cfg_inner_w, small_row_h);
        dy += small_row_h;
        defer(&mut hdwp, self.lbl_duplex_row, cfg_x, dy, cfg_inner_w, row_h);
        dy += row_h;
        defer(&mut hdwp, self.lbl_nic_row, cfg_x, dy, cfg_inner_w, small_row_h);
        dy += small_row_h;
        defer(&mut hdwp, self.lbl_rank_row, cfg_x, dy, cfg_inner_w, small_row_h);
        y = det_top + det_h + s(10);

        // Group 4: Chart
        let chart_top = y;
        let chart_h = s(160).max(lh * 8 + s(20));
        defer_bottom(&mut hdwp, self.grp_chart, margin, chart_top, w, chart_h);
        // Inner chart rect for GDI drawing
        self.chart_rect = RECT {
            left: cfg_x,
            top: chart_top + cfg_title_offset,
            right: cfg_x + cfg_inner_w,
            bottom: chart_top + chart_h - s(8),
        };
        y = chart_top + chart_h + s(10);

        // Group 5: Log
        let log_top = y;
        let log_h = (height - log_top - s(12)).max(s(120));
        defer_bottom(&mut hdwp, self.grp_log, margin, log_top, w, log_h);
        defer(
            &mut hdwp,
            self.edit_log,
            cfg_x,
            log_top + cfg_title_offset,
            cfg_inner_w,
            (log_h - cfg_title_offset - s(10)).max(s(80)),
        );

        if !hdwp.0.is_null() {
            let _ = unsafe { EndDeferWindowPos(hdwp) };
        }
    }

    unsafe fn reset_defaults(&self) {
        unsafe {
            set_text(self.edit_streams, &DEFAULT_STREAM_CONCURRENCY.to_string());
            set_text(self.edit_max_conns, &DEFAULT_MAX_CONNS_CAP.to_string());
            set_text(self.edit_hold_secs, &DEFAULT_HOLD_SECS.to_string());
            set_text(self.edit_tcp_targets, DEFAULT_TCP_TARGETS);
            set_text(self.edit_up_targets, DEFAULT_UPLOAD_TARGETS);
            set_text(self.edit_down_targets, DEFAULT_DOWNLOAD_TARGETS);
            let _ = SendMessageW(
                self.chk_continuous,
                BM_SETCHECK as u32,
                WPARAM(BST_CHECKED as usize),
                LPARAM(0),
            );
        }
        self.app
            .log
            .push("已恢复默认配置参数与国内公网节点列表（仅内存）。");
    }

    unsafe fn read_config(&self) -> TestConfig {
        let streams = unsafe { get_text(self.edit_streams) }
            .trim()
            .parse::<usize>()
            .unwrap_or(DEFAULT_STREAM_CONCURRENCY)
            .clamp(1, 256);

        let max_conn_cap = unsafe { get_text(self.edit_max_conns) }
            .trim()
            .parse::<usize>()
            .unwrap_or(DEFAULT_MAX_CONNS_CAP)
            .min(100_000);

        let hold_secs = unsafe { get_text(self.edit_hold_secs) }
            .trim()
            .parse::<u64>()
            .unwrap_or(DEFAULT_HOLD_SECS)
            .clamp(2, 60);

        let checked = unsafe {
            SendMessageW(self.chk_continuous, BM_GETCHECK as u32, WPARAM(0), LPARAM(0))
        };
        let continuous_conn = checked.0 == BST_CHECKED as isize;

        let tcp_targets = parse_target_list(&unsafe { get_text(self.edit_tcp_targets) });
        let upload_targets = parse_target_list(&unsafe { get_text(self.edit_up_targets) });
        let download_targets = parse_target_list(&unsafe { get_text(self.edit_down_targets) });

        TestConfig {
            stream_concurrency: streams,
            up_concurrency: streams,
            down_concurrency: streams,
            max_conn_cap,
            hold_secs,
            continuous_conn,
            tcp_targets,
            upload_targets,
            download_targets,
            ..TestConfig::default()
        }
    }

    unsafe fn refresh_metrics_and_logs(&mut self) {
        let stats = &self.app.stats;
        let mode = stats.mode();
        let active = mode != AppMode::Idle;

        if self.last_mode_active != Some(active) {
            self.last_mode_active = Some(active);
            let _ = unsafe { EnableWindow(self.btn_conn, !active) };
            let _ = unsafe { EnableWindow(self.btn_up, !active) };
            let _ = unsafe { EnableWindow(self.btn_down, !active) };
            let _ = unsafe { EnableWindow(self.btn_duplex, !active) };
            let _ = unsafe { EnableWindow(self.btn_reset, !active) };
            let _ = unsafe { EnableWindow(self.btn_stop, active) };
            let _ = unsafe { InvalidateRect(Some(self.lbl_status), None, false) };
        }

        let holding = stats.conn_holding.load(Ordering::Relaxed);
        let status_text = if mode == AppMode::TestingConn && holding {
            "当前状态：并发会话持续维持与自动补建中...".to_string()
        } else {
            format!("当前状态：{}", mode.label_zh())
        };
        unsafe { set_cached_text(self.lbl_status, &mut self.cache.status, status_text) };

        let sys_tcp = stats.sys_tcp_estab.load(Ordering::Relaxed);
        let handles = stats.proc_handles.load(Ordering::Relaxed);
        let retrans_x100 = stats.tcp_retrans_pct_x100.load(Ordering::Relaxed);
        let retrans_pct = (retrans_x100 as f64) / 100.0;
        let mem_badge = format!(
            "纯内存运行保障：零配置落盘 · 零临时文件 · 零注册表写入  |  系统 TCP ESTABLISHED: {sys_tcp}  |  本进程句柄: {handles}  |  TCP重传率: {retrans_pct:.2}%"
        );
        unsafe { set_cached_text(self.lbl_mem_badge, &mut self.cache.mem_badge, mem_badge) };

        let c_cur = stats.conn_current.load(Ordering::Relaxed);
        let c_tgt = stats.conn_target_probe.load(Ordering::Relaxed);
        let c_peak = stats.conn_peak.load(Ordering::Relaxed);
        let c_stable = stats.conn_stable_max.load(Ordering::Relaxed);
        let c_hold = stats.conn_long_hold_max.load(Ordering::Relaxed);
        let hold_secs = stats.conn_hold_elapsed_secs.load(Ordering::Relaxed);
        let c_cps = stats.conn_per_sec.load(Ordering::Relaxed);
        let cps_peak = stats.conn_cps_peak.load(Ordering::Relaxed);
        let c_rtt = stats.conn_rtt_ms.load(Ordering::Relaxed);
        let orig_ok = stats.conn_orig_success.load(Ordering::Relaxed);
        let orig_att = stats.conn_orig_attempts.load(Ordering::Relaxed);
        let orig_pct = stats.orig_success_rate_pct();

        let conn_row1 = format!(
            "[分级结果] 瞬时峰值: {c_peak} (建连峰值 {cps_peak}/s, 原始成功率 {orig_pct:.1}%)  |  稳定并发: {c_stable}  |  长时间保持: {c_hold} (持续 {hold_secs}s)"
        );
        unsafe { set_cached_text(self.lbl_conn_row1, &mut self.cache.conn_row1, conn_row1) };

        let rep_ok = stats.conn_replenish_success.load(Ordering::Relaxed);
        let rep_att = stats.conn_replenish_attempts.load(Ordering::Relaxed);
        let f_loc = stats.conn_fail_local.load(Ordering::Relaxed);
        let f_ref = stats.conn_fail_refused.load(Ordering::Relaxed);
        let f_rem = stats.conn_remote_closed.load(Ordering::Relaxed);
        let f_to = stats.conn_fail_timeout.load(Ordering::Relaxed);
        let conn_row2 = format!(
            "[连接归属] 存活: {c_cur} (目标:{c_tgt})  |  原始: {orig_ok}/{orig_att}  |  补建(不计入原始): {rep_ok}/{rep_att}  |  本机:{f_loc}  远拒:{f_ref}  远闭:{f_rem}  超时:{f_to}"
        );
        unsafe { set_cached_text(self.lbl_conn_row2, &mut self.cache.conn_row2, conn_row2) };

        let t_healthy = stats.conn_targets_healthy.load(Ordering::Relaxed);
        let t_total = stats.conn_targets_total.load(Ordering::Relaxed);
        let wan_summary = stats.wan_distribution_summary();
        let conn_row3 = format!(
            "[WAN 分布] 健康目标: {t_healthy}/{t_total}  |  建连速率: {c_cps}/s  |  RTT: {c_rtt} ms  |  分流: {wan_summary}"
        );
        unsafe { set_cached_text(self.lbl_conn_row3, &mut self.cache.conn_row3, conn_row3) };

        // Upload realtime
        let up_bps = stats.up_bps_current.load(Ordering::Relaxed);
        let up_peak = stats.up_bps_peak.load(Ordering::Relaxed);
        let up_avg = stats.up_bps_avg.load(Ordering::Relaxed);
        let up_p95 = stats.up_bps_p95.load(Ordering::Relaxed);
        let up_total = stats.up_bytes_total.load(Ordering::Relaxed);
        let up_streams = stats.up_active_streams.load(Ordering::Relaxed);
        let up_cur_c = stats.up_current_concurrency.load(Ordering::Relaxed);
        let up_best_c = stats.up_best_concurrency.load(Ordering::Relaxed);
        let up_err = stats.up_errors.load(Ordering::Relaxed);
        let up_fail = stats.up_fail_rate_pct();
        let up_lat = stats.up_latency_ms.load(Ordering::Relaxed);
        let up_row = format!(
            "[上行] 实时:{} 峰值:{} 平均:{} P95:{} | 并发:{up_cur_c}(最佳{up_best_c}) 活跃流:{up_streams} 失败率:{up_fail:.1}% 错误:{up_err}",
            format_mbps(up_bps),
            format_mbps(up_peak),
            format_mbps(up_avg),
            format_mbps(up_p95),
        );
        unsafe { set_cached_text(self.lbl_up_row, &mut self.cache.up_row, up_row) };

        let up_detail = format!(
            "  └ 累计确认:{} | 延迟:{up_lat}ms | 重传:{retrans_pct:.2}% | NIC上行:{}",
            format_bytes(up_total),
            format_mbps(stats.nic_up_bps.load(Ordering::Relaxed)),
        );
        unsafe { set_cached_text(self.lbl_up_detail, &mut self.cache.up_detail, up_detail) };

        // Download realtime
        let down_bps = stats.down_bps_current.load(Ordering::Relaxed);
        let down_peak = stats.down_bps_peak.load(Ordering::Relaxed);
        let down_avg = stats.down_bps_avg.load(Ordering::Relaxed);
        let down_p95 = stats.down_bps_p95.load(Ordering::Relaxed);
        let down_total = stats.down_bytes_total.load(Ordering::Relaxed);
        let down_streams = stats.down_active_streams.load(Ordering::Relaxed);
        let down_cur_c = stats.down_current_concurrency.load(Ordering::Relaxed);
        let down_best_c = stats.down_best_concurrency.load(Ordering::Relaxed);
        let down_err = stats.down_errors.load(Ordering::Relaxed);
        let down_fail = stats.down_fail_rate_pct();
        let tcp_ms = stats.down_tcp_ms.load(Ordering::Relaxed);
        let tls_ms = stats.down_tls_ms.load(Ordering::Relaxed);
        let ttfb_ms = stats.down_ttfb_ms.load(Ordering::Relaxed);
        let down_row = format!(
            "[下行] 实时:{} 峰值:{} 平均:{} P95:{} | 并发:{down_cur_c}(最佳{down_best_c}) 活跃流:{down_streams} 失败率:{down_fail:.1}% 错误:{down_err}",
            format_mbps(down_bps),
            format_mbps(down_peak),
            format_mbps(down_avg),
            format_mbps(down_p95),
        );
        unsafe { set_cached_text(self.lbl_down_row, &mut self.cache.down_row, down_row) };

        let down_detail = format!(
            "  └ 累计接收:{} | TCP:{tcp_ms}ms TLS:{tls_ms}ms TTFB:{ttfb_ms}ms | NIC下行:{}",
            format_bytes(down_total),
            format_mbps(stats.nic_down_bps.load(Ordering::Relaxed)),
        );
        unsafe {
            set_cached_text(
                self.lbl_down_detail,
                &mut self.cache.down_detail,
                down_detail,
            )
        };

        // Duplex combined
        let duplex_text = if mode == AppMode::DuplexTesting {
            let combined = up_bps + down_bps;
            let combined_avg = stats.up_bps_avg.load(Ordering::Relaxed)
                + stats.down_bps_avg.load(Ordering::Relaxed);
            format!(
                "[双向] 实时合计:{} (上{}+下{}) | 有效平均合计:{} | 应用↔网卡对比: 上行网卡{} 下行网卡{}",
                format_mbps(combined),
                format_mbps(up_bps),
                format_mbps(down_bps),
                format_mbps(combined_avg),
                format_mbps(stats.nic_up_bps.load(Ordering::Relaxed)),
                format_mbps(stats.nic_down_bps.load(Ordering::Relaxed)),
            )
        } else {
            format!(
                "[双向] 空闲 — 点击“双向同时压测”可同时跑满上行+下行 | 合计峰值: {}",
                format_mbps(up_peak + down_peak)
            )
        };
        unsafe { set_cached_text(self.lbl_duplex_row, &mut self.cache.duplex_row, duplex_text) };

        // NIC
        let nic_tx = stats.nic_tx_total.load(Ordering::Relaxed);
        let nic_rx = stats.nic_rx_total.load(Ordering::Relaxed);
        let nic_summary = stats.nic_summary.lock().clone();
        let nic_row = format!(
            "[网卡] 实时上行:{} 下行:{} | 累计发送:{} 接收:{} | 网卡:{}",
            format_mbps(stats.nic_up_bps.load(Ordering::Relaxed)),
            format_mbps(stats.nic_down_bps.load(Ordering::Relaxed)),
            format_bytes(nic_tx),
            format_bytes(nic_rx),
            if nic_summary.is_empty() {
                "检测中..."
            } else {
                &nic_summary
            }
        );
        unsafe { set_cached_text(self.lbl_nic_row, &mut self.cache.nic_row, nic_row) };

        // Node ranking
        let up_rank = crate::state::SharedStats::node_ranking_summary(&stats.up_node_metrics);
        let down_rank = crate::state::SharedStats::node_ranking_summary(&stats.down_node_metrics);
        let rank_text = format!("[节点排名] 上行: {up_rank}  |  下行: {down_rank}");
        unsafe { set_cached_text(self.lbl_rank_row, &mut self.cache.rank_row, rank_text) };

        // Delta-append memory ring buffer log
        let (new_gen, new_seq, lines, reset) = self.app.log.fetch_since(self.log_gen, self.log_seq);
        if reset {
            let full_text = if lines.is_empty() {
                String::new()
            } else {
                format!("{}\r\n", lines.join("\r\n"))
            };
            unsafe { set_text(self.edit_log, &full_text) };
            let end_pos = unsafe { GetWindowTextLengthW(self.edit_log) } as usize;
            let _ = unsafe {
                SendMessageW(
                    self.edit_log,
                    EM_SETSEL,
                    WPARAM(end_pos),
                    LPARAM(end_pos as isize),
                )
            };
            let _ = unsafe { SendMessageW(self.edit_log, EM_SCROLLCARET, WPARAM(0), LPARAM(0)) };
        } else if !lines.is_empty() {
            let append_text = format!("{}\r\n", lines.join("\r\n"));
            let wide = to_wide_null(&append_text);
            let end_pos = unsafe { GetWindowTextLengthW(self.edit_log) } as usize;
            let _ = unsafe {
                SendMessageW(
                    self.edit_log,
                    EM_SETSEL,
                    WPARAM(end_pos),
                    LPARAM(end_pos as isize),
                )
            };
            let _ = unsafe {
                SendMessageW(
                    self.edit_log,
                    EM_REPLACESEL,
                    WPARAM(0),
                    LPARAM(wide.as_ptr() as isize),
                )
            };
            let _ = unsafe { SendMessageW(self.edit_log, EM_SCROLLCARET, WPARAM(0), LPARAM(0)) };
        }
        self.log_gen = new_gen;
        self.log_seq = new_seq;
    }
}

/// Draw bandwidth chart inside given rect using GDI (caller provides HDC from BeginPaint)
unsafe fn paint_speed_chart_on_hdc(hdc: HDC, chart_rect: RECT, stats: &crate::state::SharedStats, theme: &ThemeResources) {
    let rc = chart_rect;
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    if w <= 20 || h <= 20 {
        return;
    }

    // Create memory DC for double-buffer to avoid flicker
    let mem_dc = unsafe { CreateCompatibleDC(Some(hdc)) };
    let mem_bmp = unsafe { CreateCompatibleBitmap(hdc, w, h) };
    if mem_dc.0.is_null() || mem_bmp.0.is_null() {
        if !mem_dc.0.is_null() { let _ = unsafe { DeleteDC(mem_dc) }; }
        if !mem_bmp.0.is_null() { let _ = unsafe { DeleteObject(HGDIOBJ(mem_bmp.0)) }; }
        return;
    }
    let old_bmp = unsafe { SelectObject(mem_dc, HGDIOBJ(mem_bmp.0)) };

    // Fill background with card color
    let mut fill_rc = RECT { left: 0, top: 0, right: w, bottom: h };
    let _ = unsafe { FillRect(mem_dc, &fill_rc, theme.brush_card) };

    // Border
    let pen_border = unsafe { CreatePen(PS_SOLID as u32, 1, rgb(200, 200, 200)) };
    let old_pen = unsafe { SelectObject(mem_dc, HGDIOBJ(pen_border.0)) };
    let old_brush = unsafe { SelectObject(mem_dc, HGDIOBJ(GetStockObject(NULL_BRUSH).0)) };
    let _ = unsafe { Rectangle(mem_dc, 0, 0, w, h) };

    // Chart inner area with padding for labels
    let pad_l = scale_px(48, theme.dpi);
    let pad_r = scale_px(10, theme.dpi);
    let pad_t = scale_px(12, theme.dpi);
    let pad_b = scale_px(18, theme.dpi);
    let chart_l = pad_l;
    let chart_t = pad_t;
    let chart_r = w - pad_r;
    let chart_b = h - pad_b;
    let chart_w = (chart_r - chart_l).max(1);
    let chart_h = (chart_b - chart_t).max(1);

    // Determine max Y from history
    let hist = stats.speed_history.lock().clone();
    let mut max_bps: u64 = 10_000_000; // at least 10 Mbps visible
    for pt in hist.iter() {
        max_bps = max_bps.max(pt.up_bps).max(pt.down_bps).max(pt.nic_up_bps).max(pt.nic_down_bps);
    }
    max_bps = max_bps.max(stats.up_bps_peak.load(Ordering::Relaxed)).max(stats.down_bps_peak.load(Ordering::Relaxed));
    max_bps = ((max_bps as f64) * 1.2) as u64;
    if max_bps < 5_000_000 { max_bps = 5_000_000; }
    let max_mbps = (max_bps as f64) / 1_000_000.0;

    // Grid lines (4 horizontal)
    let pen_grid = unsafe { CreatePen(PS_DOT as u32, 1, rgb(220, 220, 220)) };
    let _ = unsafe { SelectObject(mem_dc, HGDIOBJ(pen_grid.0)) };
    for i in 1..4 {
        let y = chart_t + (chart_h * i) / 4;
        let _ = unsafe { MoveToEx(mem_dc, chart_l, y, None) };
        let _ = unsafe { LineTo(mem_dc, chart_r, y) };
    }

    // Y-axis labels
    let _ = unsafe { SelectObject(mem_dc, HGDIOBJ(theme.font_small.0)) };
    let _ = unsafe { SetBkMode(mem_dc, TRANSPARENT) };
    let _ = unsafe { SetTextColor(mem_dc, theme.colors.text_muted) };
    for i in 0..=4 {
        let frac = (4 - i) as f64 / 4.0;
        let mbps = max_mbps * frac;
        let label = if mbps >= 100.0 { format!("{mbps:.0}") } else { format!("{mbps:.1}") };
        let y = chart_t + (chart_h * i) / 4 - scale_px(6, theme.dpi);
        let mut tr = RECT { left: scale_px(2, theme.dpi), top: y, right: pad_l - scale_px(2, theme.dpi), bottom: y + scale_px(14, theme.dpi) };
        let wide = to_wide_null(&label);
        let _ = unsafe { DrawTextW(mem_dc, PCWSTR(wide.as_ptr()), -1, &mut tr, DT_RIGHT.0 | DT_SINGLELINE.0) };
    }
    // X-axis time labels (0s .. 60s)
    for i in 0..=3 {
        let x = chart_l + (chart_w * i) / 3;
        let secs = (60 * i) / 3;
        let label = format!("{secs}s");
        let wide = to_wide_null(&label);
        let mut tr = RECT { left: x - scale_px(14, theme.dpi), top: chart_b + scale_px(2, theme.dpi), right: x + scale_px(14, theme.dpi), bottom: chart_b + scale_px(14, theme.dpi) };
        let _ = unsafe { DrawTextW(mem_dc, PCWSTR(wide.as_ptr()), -1, &mut tr, DT_CENTER.0 | DT_SINGLELINE.0) };
    }

    // Helper to plot a series
    let plot_series = |mem_dc: HDC, points: Vec<(f64,f64)>, color: COLORREF, width: i32, style: u32| {
        if points.len() < 2 { return; }
        let pen = unsafe { CreatePen(style, width, color) };
        let old = unsafe { SelectObject(mem_dc, HGDIOBJ(pen.0)) };
        let mut first = true;
        for (x_frac, y_frac) in points {
            let x = chart_l + (chart_w as f64 * x_frac) as i32;
            let y = chart_b - (chart_h as f64 * y_frac) as i32;
            if first {
                let _ = unsafe { MoveToEx(mem_dc, x, y, None) };
                first = false;
            } else {
                let _ = unsafe { LineTo(mem_dc, x, y) };
            }
        }
        let _ = unsafe { SelectObject(mem_dc, old) };
        let _ = unsafe { DeleteObject(HGDIOBJ(pen.0)) };
    };

    if !hist.is_empty() {
        let max_f = max_bps as f64;

        let up_pts: Vec<(f64,f64)> = hist.iter().enumerate().map(|(i, pt)| {
            let xf = (i as f64) / 119.0; // 120 points span
            let yf = (pt.up_bps as f64) / max_f;
            (xf, yf.clamp(0.0, 1.0))
        }).collect();
        let down_pts: Vec<(f64,f64)> = hist.iter().enumerate().map(|(i, pt)| {
            let xf = (i as f64) / 119.0;
            let yf = (pt.down_bps as f64) / max_f;
            (xf, yf.clamp(0.0, 1.0))
        }).collect();
        let nic_up_pts: Vec<(f64,f64)> = hist.iter().enumerate().map(|(i, pt)| {
            let xf = (i as f64) / 119.0;
            let yf = (pt.nic_up_bps as f64) / max_f;
            (xf, yf.clamp(0.0, 1.0))
        }).collect();
        let nic_down_pts: Vec<(f64,f64)> = hist.iter().enumerate().map(|(i, pt)| {
            let xf = (i as f64) / 119.0;
            let yf = (pt.nic_down_bps as f64) / max_f;
            (xf, yf.clamp(0.0, 1.0))
        }).collect();

        // Up: orange solid 2px
        plot_series(mem_dc, up_pts, rgb(234, 88, 12), 2, PS_SOLID as u32);
        // Down: blue solid 2px
        plot_series(mem_dc, down_pts, rgb(37, 99, 235), 2, PS_SOLID as u32);
        // Duplex combined: green if active (sum)
        if stats.mode() == AppMode::DuplexTesting {
            let duplex_pts: Vec<(f64,f64)> = hist.iter().enumerate().map(|(i, pt)| {
                let xf = (i as f64) / 119.0;
                let yf = ((pt.up_bps + pt.down_bps) as f64 / max_f).clamp(0.0, 1.0);
                (xf, yf)
            }).collect();
            plot_series(mem_dc, duplex_pts, rgb(5, 122, 85), 2, PS_SOLID as u32);
        }
        // NIC up: orange dashed
        plot_series(mem_dc, nic_up_pts, rgb(180, 120, 40), 1, PS_DOT as u32);
        // NIC down: blue dashed
        plot_series(mem_dc, nic_down_pts, rgb(100, 130, 180), 1, PS_DOT as u32);
    }

    // Legend at top-right inside chart
    let legend_items: Vec<(&str, COLORREF)> = vec![
        ("上行", rgb(234,88,12)),
        ("下行", rgb(37,99,235)),
        ("双向合计", rgb(5,122,85)),
        ("网卡上行(虚线)", rgb(180,120,40)),
        ("网卡下行(虚线)", rgb(100,130,180)),
    ];
    let mut lx = chart_r - scale_px(260, theme.dpi);
    let ly = chart_t + scale_px(4, theme.dpi);
    for (label, color) in legend_items {
        // color box
        let pen = unsafe { CreatePen(PS_SOLID as u32, 2, color) };
        let old = unsafe { SelectObject(mem_dc, HGDIOBJ(pen.0)) };
        let bx1 = lx;
        let by1 = ly;
        let bx2 = lx + scale_px(12, theme.dpi);
        let by2 = ly + scale_px(4, theme.dpi);
        let _ = unsafe { MoveToEx(mem_dc, bx1, (by1+by2)/2, None) };
        let _ = unsafe { LineTo(mem_dc, bx2, (by1+by2)/2) };
        let _ = unsafe { SelectObject(mem_dc, old) };
        let _ = unsafe { DeleteObject(HGDIOBJ(pen.0)) };
        // text
        let wide = to_wide_null(label);
        let mut tr = RECT { left: bx2 + scale_px(4, theme.dpi), top: ly - scale_px(6, theme.dpi), right: bx2 + scale_px(60, theme.dpi), bottom: ly + scale_px(8, theme.dpi) };
        let _ = unsafe { DrawTextW(mem_dc, PCWSTR(wide.as_ptr()), -1, &mut tr, DT_LEFT.0 | DT_SINGLELINE.0) };
        lx += scale_px(52, theme.dpi);
        if label.len() > 8 { lx += scale_px(20, theme.dpi); }
    }

    // Restore and blit
    let _ = unsafe { SelectObject(mem_dc, old_pen) };
    let _ = unsafe { SelectObject(mem_dc, old_brush) };
    let _ = unsafe { DeleteObject(HGDIOBJ(pen_border.0)) };
    let _ = unsafe { DeleteObject(HGDIOBJ(pen_grid.0)) };

    // Blit to screen at chart_rect origin
    let _ = unsafe { BitBlt(hdc, rc.left, rc.top, w, h, Some(mem_dc), 0, 0, SRCCOPY) };

    let _ = unsafe { SelectObject(mem_dc, old_bmp) };
    let _ = unsafe { DeleteObject(HGDIOBJ(mem_bmp.0)) };
    let _ = unsafe { DeleteDC(mem_dc) };
}

pub fn run_gui(state: Arc<AppState>, engine: EngineHandle) -> Result<(), String> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let hmodule = GetModuleHandleW(PCWSTR::null());
        if hmodule.0.is_null() {
            return Err("获取进程模块句柄失败".to_string());
        }
        let hinstance = HINSTANCE(hmodule.0);
        let class_name = w!("WinNetStressMainWindow");

        let big_icon_sz = GetSystemMetrics(SM_CXICON).max(32);
        let sm_icon_sz = GetSystemMetrics(SM_CXSMICON).max(16);
        let hicon_big = create_app_icon(big_icon_sz);
        let hicon_sm = create_app_icon(sm_icon_sz);

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            hIcon: hicon_big,
            hIconSm: hicon_sm,
            hCursor: LoadCursorW(None, IDC_ARROW),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: class_name,
            ..Default::default()
        };

        if RegisterClassExW(&wc).0 == 0 {
            if !hicon_big.0.is_null() {
                let _ = DestroyIcon(hicon_big);
            }
            if !hicon_sm.0.is_null() {
                let _ = DestroyIcon(hicon_sm);
            }
            return Err("注册 Win32 窗口类失败".to_string());
        }

        let screen_w = GetSystemMetrics(SM_CXSCREEN).max(1024);
        let screen_h = GetSystemMetrics(SM_CYSCREEN).max(720);

        let hdc = GetDC(None);
        let sys_dpi = if !hdc.0.is_null() {
            let d = GetDeviceCaps(Some(hdc), LOGPIXELSY);
            let _ = ReleaseDC(None, hdc);
            d.max(96) as u32
        } else {
            96
        };

        let init_w = scale_px(1080, sys_dpi).min((screen_w * 92) / 100);
        let init_h = scale_px(960, sys_dpi).min((screen_h * 92) / 100);

        let init_ctx = Box::new((state, engine));
        let raw_ctx = Box::into_raw(init_ctx);

        let hwnd = CreateWindowExW(
            0,
            class_name,
            w!("WinNetStress - 纯内存网络并发与带宽压力测试工具"),
            (WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN) as u32,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            init_w,
            init_h,
            None,
            None,
            Some(hinstance),
            Some(raw_ctx as *const c_void),
        );

        if hwnd.0.is_null() {
            drop(Box::from_raw(raw_ctx));
            if !hicon_big.0.is_null() {
                let _ = DestroyIcon(hicon_big);
            }
            if !hicon_sm.0.is_null() {
                let _ = DestroyIcon(hicon_sm);
            }
            return Err("创建 Win32 主窗口失败".to_string());
        }

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        let _ = PostMessageW(Some(hwnd), WM_APP_WARMUP, WPARAM(0), LPARAM(0));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        if !hicon_big.0.is_null() {
            let _ = DestroyIcon(hicon_big);
        }
        if !hicon_sm.0.is_null() {
            let _ = DestroyIcon(hicon_sm);
        }
    }

    Ok(())
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        m if m == WM_CREATE as u32 => {
            let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
            let ctx = unsafe { Box::from_raw(cs.lpCreateParams as *mut (Arc<AppState>, EngineHandle)) };
            let (app, engine) = *ctx;

            let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
            let theme = unsafe { ThemeResources::create(dpi) };
            let hinst = cs.hInstance;

            let st_left = (WS_CHILD | WS_VISIBLE | SS_LEFT | SS_NOPREFIX | SS_CENTERIMAGE) as u32;
            let st_right =
                (WS_CHILD | WS_VISIBLE | SS_RIGHT | SS_NOPREFIX | SS_CENTERIMAGE) as u32;
            let grp_style = (WS_CHILD | WS_VISIBLE | BS_GROUPBOX) as u32;
            let edt_num = (WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL | ES_NUMBER) as u32;
            let edt_single = (WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL) as u32;
            let edt_multi = (WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | ES_MULTILINE
                | ES_AUTOVSCROLL
                | WS_VSCROLL) as u32;
            let btn_style = (WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON) as u32;
            let chk_style = (WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX) as u32;
            let log_style = (WS_CHILD
                | WS_VISIBLE
                | ES_MULTILINE
                | ES_READONLY
                | WS_VSCROLL
                | ES_AUTOVSCROLL) as u32;

            // Header controls
            let lbl_title = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "WinNetStress · 纯内存网络并发与带宽压力测试工具",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_title,
                )
            };
            let lbl_status = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "当前状态：空闲 (就绪)",
                    st_right,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_bold,
                )
            };
            let lbl_mem_badge = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "纯内存运行保障：零配置落盘 · 零临时文件 · 零注册表写入",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_small,
                )
            };

            // Inner controls for Config (created BEFORE grp_config so they sit above it in Z-order)
            let lbl_streams = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "并发流数 (上/下行):",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let edit_streams = unsafe {
                create_ctrl(
                    w!("EDIT"),
                    &DEFAULT_STREAM_CONCURRENCY.to_string(),
                    edt_num,
                    WS_EX_CLIENTEDGE as u32,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let lbl_max_conns = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "连接参考上限 (0=不限):",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let edit_max_conns = unsafe {
                create_ctrl(
                    w!("EDIT"),
                    &DEFAULT_MAX_CONNS_CAP.to_string(),
                    edt_num,
                    WS_EX_CLIENTEDGE as u32,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let lbl_hold_secs = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "阶梯窗口 (秒):",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let edit_hold_secs = unsafe {
                create_ctrl(
                    w!("EDIT"),
                    &DEFAULT_HOLD_SECS.to_string(),
                    edt_num,
                    WS_EX_CLIENTEDGE as u32,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let chk_continuous = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    "持续加压 / 维持连接",
                    chk_style,
                    0,
                    hwnd,
                    ID_CHK_CONTINUOUS,
                    hinst,
                    theme.font_ui,
                )
            };
            let _ = unsafe {
                SendMessageW(
                    chk_continuous,
                    BM_SETCHECK as u32,
                    WPARAM(BST_CHECKED as usize),
                    LPARAM(0),
                )
            };

            let lbl_tcp_targets = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "TCP 探测节点 (host:port):",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let edit_tcp_targets = unsafe {
                create_ctrl(
                    w!("EDIT"),
                    DEFAULT_TCP_TARGETS,
                    edt_single,
                    WS_EX_CLIENTEDGE as u32,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let lbl_up_targets = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "上行压测目标 (HTTP / tcp://):",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let edit_up_targets = unsafe {
                create_ctrl(
                    w!("EDIT"),
                    DEFAULT_UPLOAD_TARGETS,
                    edt_single,
                    WS_EX_CLIENTEDGE as u32,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let lbl_down_targets = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "下行文件节点 (每行一个):",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let edit_down_targets = unsafe {
                create_ctrl(
                    w!("EDIT"),
                    DEFAULT_DOWNLOAD_TARGETS,
                    edt_multi,
                    WS_EX_CLIENTEDGE as u32,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let grp_config = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    " 测试参数与目标配置 (仅内存生效，支持实时编辑) ",
                    grp_style,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_bold,
                )
            };

            // Action buttons
            let btn_conn = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    "开始并发连接测试",
                    btn_style,
                    0,
                    hwnd,
                    ID_BTN_CONN,
                    hinst,
                    theme.font_bold,
                )
            };
            let btn_up = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    "开始上行带宽压测",
                    btn_style,
                    0,
                    hwnd,
                    ID_BTN_UP,
                    hinst,
                    theme.font_bold,
                )
            };
            let btn_down = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    "开始下行带宽压测",
                    btn_style,
                    0,
                    hwnd,
                    ID_BTN_DOWN,
                    hinst,
                    theme.font_bold,
                )
            };
            let btn_duplex = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    "双向同时压测",
                    btn_style,
                    0,
                    hwnd,
                    ID_BTN_DUPLEX,
                    hinst,
                    theme.font_bold,
                )
            };
            let btn_stop = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    "停止当前测试",
                    btn_style,
                    0,
                    hwnd,
                    ID_BTN_STOP,
                    hinst,
                    theme.font_bold,
                )
            };
            let btn_reset = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    "恢复默认",
                    btn_style,
                    0,
                    hwnd,
                    ID_BTN_RESET,
                    hinst,
                    theme.font_ui,
                )
            };
            let btn_clear_log = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    "清空日志",
                    btn_style,
                    0,
                    hwnd,
                    ID_BTN_CLEAR_LOG,
                    hinst,
                    theme.font_ui,
                )
            };
            let btn_help = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    "使用说明",
                    btn_style,
                    0,
                    hwnd,
                    ID_BTN_HELP,
                    hinst,
                    theme.font_ui,
                )
            };

            // Inner controls for Metrics (created BEFORE grp_metrics)
            let lbl_conn_row1 = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_bold,
                )
            };
            let lbl_conn_row2 = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_small,
                )
            };
            let lbl_conn_row3 = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_small,
                )
            };
            let lbl_up_row = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let lbl_up_detail = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_small,
                )
            };
            let lbl_down_row = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_ui,
                )
            };
            let lbl_down_detail = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_small,
                )
            };
            let grp_metrics = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    " 实时测试指标监控 ",
                    grp_style,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_bold,
                )
            };

            // Bandwidth detail inner before group
            let lbl_duplex_row = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_bold,
                )
            };
            let lbl_nic_row = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_small,
                )
            };
            let lbl_rank_row = unsafe {
                create_ctrl(
                    w!("STATIC"),
                    "",
                    st_left,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_small,
                )
            };
            let grp_bandwidth = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    " 带宽详情 · 网卡对比 · 节点排名 ",
                    grp_style,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_bold,
                )
            };

            // Chart group (no inner text controls, painted via GDI)
            let grp_chart = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    " 实时速度曲线 (上行·下行·双向合计 vs 网卡实际流量) ",
                    grp_style,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_bold,
                )
            };

            // Inner control for Log (created BEFORE grp_log)
            let edit_log = unsafe {
                create_ctrl(
                    w!("EDIT"),
                    "",
                    log_style,
                    WS_EX_CLIENTEDGE as u32,
                    hwnd,
                    0,
                    hinst,
                    theme.font_log,
                )
            };
            let _ = unsafe { SendMessageW(edit_log, EM_SETLIMITTEXT, WPARAM(1_048_576), LPARAM(0)) };
            let grp_log = unsafe {
                create_ctrl(
                    w!("BUTTON"),
                    " 运行日志 (纯内存环形缓冲，最多保留 400 行) ",
                    grp_style,
                    0,
                    hwnd,
                    0,
                    hinst,
                    theme.font_bold,
                )
            };

            let mut ui = Box::new(UiState {
                app,
                engine,
                theme,
                cache: LabelCache::default(),
                last_mode_active: None,
                log_gen: 0,
                log_seq: 0,
                chart_rect: RECT::default(),
                lbl_title,
                lbl_status,
                lbl_mem_badge,
                grp_config,
                lbl_streams,
                edit_streams,
                lbl_max_conns,
                edit_max_conns,
                lbl_hold_secs,
                edit_hold_secs,
                chk_continuous,
                lbl_tcp_targets,
                edit_tcp_targets,
                lbl_up_targets,
                edit_up_targets,
                lbl_down_targets,
                edit_down_targets,
                btn_conn,
                btn_up,
                btn_down,
                btn_duplex,
                btn_stop,
                btn_reset,
                btn_clear_log,
                btn_help,
                grp_metrics,
                lbl_conn_row1,
                lbl_conn_row2,
                lbl_conn_row3,
                lbl_up_row,
                lbl_up_detail,
                lbl_down_row,
                lbl_down_detail,
                grp_bandwidth,
                lbl_duplex_row,
                lbl_nic_row,
                lbl_rank_row,
                grp_chart,
                grp_log,
                edit_log,
            });

            let mut rc = RECT::default();
            let _ = unsafe { GetClientRect(hwnd, &mut rc) };
            unsafe { ui.layout(rc.right - rc.left, rc.bottom - rc.top) };
            unsafe { ui.refresh_metrics_and_logs() };

            let _ = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(ui) as isize) };
            let _ = unsafe { SetTimer(Some(hwnd), ID_TIMER_REFRESH, 250, None) };
            LRESULT(0)
        }
        m if m == WM_GETMINMAXINFO as u32 => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
            let dpi = if !ptr.is_null() {
                unsafe { (*ptr).theme.dpi }
            } else {
                96
            };
            let mmi = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
            mmi.ptMinTrackSize.x = scale_px(960, dpi);
            mmi.ptMinTrackSize.y = scale_px(960, dpi);
            LRESULT(0)
        }
        m if m == WM_SYSCOLORCHANGE as u32 => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
            if !ptr.is_null() {
                let ui = unsafe { &mut *ptr };
                unsafe { ui.theme.refresh_colors() };
                let _ = unsafe { InvalidateRect(Some(hwnd), None, true) };
            }
            LRESULT(0)
        }
        m if m == WM_ERASEBKGND as u32 => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
            if !ptr.is_null() {
                let ui = unsafe { &*ptr };
                let hdc = HDC(wparam.0 as *mut _);
                let mut rc = RECT::default();
                let _ = unsafe { GetClientRect(hwnd, &mut rc) };
                let _ = unsafe { FillRect(hdc, &rc, ui.theme.brush_bg) };
                return LRESULT(1);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        m if m == WM_CTLCOLORBTN as u32 => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
            if !ptr.is_null() {
                let ui = unsafe { &*ptr };
                let hdc = HDC(wparam.0 as *mut _);
                let _ = unsafe { SetBkMode(hdc, OPAQUE) };
                let _ = unsafe { SetBkColor(hdc, ui.theme.colors.bg) };
                let _ = unsafe { SetTextColor(hdc, ui.theme.colors.text_primary) };
                return LRESULT(ui.theme.brush_bg.0 as isize);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        m if m == WM_CTLCOLORSTATIC as u32 => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
            if !ptr.is_null() {
                let ui = unsafe { &*ptr };
                let hdc = HDC(wparam.0 as *mut _);
                let ctrl = HWND(lparam.0 as *mut _);
                let _ = unsafe { SetBkMode(hdc, OPAQUE) };

                if ctrl == ui.edit_log {
                    let _ = unsafe { SetBkColor(hdc, ui.theme.colors.card) };
                    let _ = unsafe { SetTextColor(hdc, ui.theme.colors.text_primary) };
                    return LRESULT(ui.theme.brush_card.0 as isize);
                }

                let _ = unsafe { SetBkColor(hdc, ui.theme.colors.bg) };
                let color = if ctrl == ui.lbl_title {
                    ui.theme.colors.brand
                } else if ctrl == ui.lbl_status {
                    if ui.app.stats.mode() == AppMode::Idle {
                        ui.theme.colors.idle
                    } else {
                        ui.theme.colors.active
                    }
                } else if ctrl == ui.lbl_mem_badge || ctrl == ui.lbl_conn_row3 || ctrl == ui.lbl_nic_row || ctrl == ui.lbl_rank_row {
                    ui.theme.colors.text_muted
                } else {
                    ui.theme.colors.text_primary
                };
                let _ = unsafe { SetTextColor(hdc, color) };
                return LRESULT(ui.theme.brush_bg.0 as isize);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        m if m == WM_CTLCOLOREDIT as u32 => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
            if !ptr.is_null() {
                let ui = unsafe { &*ptr };
                let hdc = HDC(wparam.0 as *mut _);
                let _ = unsafe { SetBkMode(hdc, OPAQUE) };
                let _ = unsafe { SetBkColor(hdc, ui.theme.colors.card) };
                let _ = unsafe { SetTextColor(hdc, ui.theme.colors.text_primary) };
                return LRESULT(ui.theme.brush_card.0 as isize);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        m if m == WM_PAINT as u32 => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
            let mut ps = PAINTSTRUCT::default();
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            if !ptr.is_null() {
                let ui = unsafe { &*ptr };
                if ui.chart_rect.right > ui.chart_rect.left && ui.chart_rect.bottom > ui.chart_rect.top {
                    unsafe { paint_speed_chart_on_hdc(hdc, ui.chart_rect, &ui.app.stats, &ui.theme) };
                }
            }
            let _ = unsafe { EndPaint(hwnd, &ps) };
            // Let child controls paint via DefWindowProc after we validated
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        m if m == WM_DPICHANGED as u32 => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
            if !ptr.is_null() {
                let ui = unsafe { &mut *ptr };
                let new_dpi = (wparam.0 & 0xFFFF) as u32;
                unsafe { ui.theme.rebuild_fonts(new_dpi) };
                unsafe { ui.apply_fonts() };
                let suggested = unsafe { &*(lparam.0 as *const RECT) };
                let _ = unsafe {
                    MoveWindow(
                        hwnd,
                        suggested.left,
                        suggested.top,
                        suggested.right - suggested.left,
                        suggested.bottom - suggested.top,
                        true,
                    )
                };
                let mut rc = RECT::default();
                let _ = unsafe { GetClientRect(hwnd, &mut rc) };
                unsafe { ui.layout(rc.right - rc.left, rc.bottom - rc.top) };
                let _ = unsafe { InvalidateRect(Some(hwnd), None, true) };
            }
            LRESULT(0)
        }
        m if m == WM_SIZE as u32 => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
            if !ptr.is_null() {
                let ui = unsafe { &mut *ptr };
                let w = (lparam.0 & 0xFFFF) as i32;
                let h = ((lparam.0 >> 16) & 0xFFFF) as i32;
                unsafe { ui.layout(w, h) };
                let _ = unsafe { InvalidateRect(Some(hwnd), None, true) };
            }
            LRESULT(0)
        }
        m if m == WM_APP_WARMUP => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
            if !ptr.is_null() {
                let ui = unsafe { &*ptr };
                ui.engine.warm_up();
            }
            LRESULT(0)
        }
        m if m == WM_TIMER as u32 => {
            if wparam.0 == ID_TIMER_REFRESH {
                let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
                if !ptr.is_null() {
                    let ui = unsafe { &mut *ptr };
                    unsafe { ui.refresh_metrics_and_logs() };
                    // Invalidate chart area to repaint curves each tick
                    if ui.chart_rect.right > ui.chart_rect.left {
                        let _ = unsafe { InvalidateRect(Some(hwnd), Some(&ui.chart_rect), false) };
                    }
                }
            }
            LRESULT(0)
        }
        m if m == WM_COMMAND as u32 => {
            let id = wparam.0 & 0xFFFF;
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
            if !ptr.is_null() {
                let ui = unsafe { &mut *ptr };
                match id {
                    ID_BTN_CONN => {
                        let cfg = unsafe { ui.read_config() };
                        ui.engine.start_conn_test(cfg);
                    }
                    ID_BTN_UP => {
                        let cfg = unsafe { ui.read_config() };
                        ui.engine.start_upload_test(cfg);
                    }
                    ID_BTN_DOWN => {
                        let cfg = unsafe { ui.read_config() };
                        ui.engine.start_download_test(cfg);
                    }
                    ID_BTN_DUPLEX => {
                        let cfg = unsafe { ui.read_config() };
                        ui.engine.start_duplex_test(cfg);
                    }
                    ID_BTN_STOP => {
                        ui.engine.stop();
                    }
                    ID_BTN_RESET => {
                        if ui.app.stats.mode() == AppMode::Idle {
                            unsafe { ui.reset_defaults() };
                            unsafe { ui.refresh_metrics_and_logs() };
                        }
                    }
                    ID_BTN_CLEAR_LOG => {
                        ui.app.log.clear();
                        unsafe { ui.refresh_metrics_and_logs() };
                    }
                    ID_BTN_HELP => {
                        let help_text = concat!(
                            "1. 纯内存原则：配置、统计和日志全部保存在进程内存中，退出即销毁，不读写本地磁盘或注册表。\n\n",
                            "2. 测速准确性：\n",
                            "  · 上行严格区分【生成数据量/客户端发送量/服务端确认接收量】，仅服务端确认量计入有效速率；\n",
                            "  · 下行分离 TCP建连/TLS握手/TTFB/纯传输时间，预测仅基于纯传输阶段，多轮中位数选优；\n",
                            "  · 失败请求独立统计，不计入成功率分母。\n\n",
                            "3. 自动并发寻优：档位 1/2/4/8/16/32/64 基于速率、失败率、延迟自动搜索最佳稳定并发，非固定 16。\n\n",
                            "4. 多 WAN 说明：单条 TCP 连接仅走单条出口，多目标并发才会分散至多条线路；本工具按独立 IP 列表原子轮询分散连接，并记录各 IP 连接数与速率，但不假设多 IP=多 WAN，需对照路由器/网卡实际流量验证。\n\n",
                            "5. 双向压测：上传与下载可同时满载，独立并发与统计，实时合计吞吐直接可见。\n\n",
                            "6. 网卡监控：每 500ms 通过 GetIfTable2/MIB_IF_ROW2 采样网卡收发字节、名称、状态、链路速率，对比应用流量与网卡总流量，并显示 TCP 重传率与 RTT/失败率。\n\n",
                            "7. 报告以有效窗口平均吞吐为主，峰值/P95/累计流量/并发/失败率等为辅；速度曲线实时展示 60 秒历史。"
                        );
                        let wide = to_wide_null(help_text);
                        unsafe {
                            MessageBoxW(
                                Some(hwnd),
                                PCWSTR(wide.as_ptr()),
                                w!("使用说明 - WinNetStress"),
                                (MB_OK | MB_ICONINFORMATION) as u32,
                            );
                        }
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        m if m == WM_CLOSE as u32 => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
            if !ptr.is_null() {
                let ui = unsafe { &*ptr };
                ui.engine.stop();
            }
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        m if m == WM_DESTROY as u32 => {
            let _ = unsafe { KillTimer(Some(hwnd), ID_TIMER_REFRESH) };
            let ptr = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) } as *mut UiState;
            if !ptr.is_null() {
                let mut ui = unsafe { Box::from_raw(ptr) };
                ui.engine.stop();
                unsafe { ui.theme.destroy() };
            }
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
