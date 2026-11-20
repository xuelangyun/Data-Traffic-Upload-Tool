//! Native Win32 GUI with zero-flicker GDI rendering, Per-Monitor DPI scaling,
//! Windows system style & font adaptation, connection/bandwidth tables and
//! dynamically expanding public egress-IP columns.

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
const LVM_DELETEITEM_MSG: u32 = 0x1008; // LVM_FIRST + 8
const LVM_SETCOLUMN_MSG: u32 = 0x101A; // LVM_FIRST + 26
const LVM_DELETECOLUMN_MSG: u32 = 0x101C; // LVM_FIRST + 28
const LVM_SETCOLUMNWIDTH_MSG: u32 = 0x101E; // LVM_FIRST + 30

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(((b as u32) << 16) | ((g as u32) << 8) | (r as u32))
}

#[derive(Default)]
struct LabelCache {
    status: String,
    mem_badge: String,
    // Legacy kept for log etc, but metrics now via table
    conn_row1: String,
    conn_row2: String,
    conn_row3: String,
    up_row: String,
    up_detail: String,
    down_row: String,
    down_detail: String,
    duplex_row: String,
    rank_row: String,
    wan_ip: String,
    // Table caches: keep last rendered cell to avoid flicker
    metrics_cells: [[String; 6]; 8],
    bandwidth_cells: [[String; 6]; 4],
    wan_columns: Vec<String>,
    // Two fixed data rows; each row has one cache cell per visible column.
    wan_cells: Vec<Vec<String>>,
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
    let half = 0.90f32;
    let radius = 0.25f32;
    let qx = u.abs() - (half - radius);
    let qy = v.abs() - (half - radius);
    let sdf = (qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0)) - radius;
    if sdf > 0.035 {
        return [0.0, 0.0, 0.0, 0.0];
    }
    let alpha = ((-sdf + 0.035) / 0.07).clamp(0.0, 1.0);
    let t = ((u + v + 1.9) / 3.8).clamp(0.0, 1.0);
    let color = [
        0.015 * (1.0 - t) + 0.025 * t,
        0.28 * (1.0 - t) + 0.10 * t,
        0.68 * (1.0 - t) + 0.36 * t,
        alpha,
    ];

    let gauge_y = -0.08;
    let gauge_r = 0.66;
    let gauge_dist = (u.hypot(v - gauge_y) - gauge_r).abs();
    let gauge_angle = (v - gauge_y).atan2(u);
    if gauge_angle > -2.78 && gauge_angle < -0.36 && gauge_dist < 0.045 {
        return [0.40, 0.88, 1.0, alpha];
    }

    for angle in [-2.58f32, -2.08, -1.57, -1.06, -0.56] {
        let (sin_a, cos_a) = angle.sin_cos();
        let outer = gauge_r + 0.02;
        let inner = gauge_r - 0.09;
        if segment_distance(u, v, cos_a * inner, gauge_y + sin_a * inner,
                            cos_a * outer, gauge_y + sin_a * outer) < 0.022 {
            return [0.78, 0.94, 1.0, alpha];
        }
    }

    let shaft = segment_distance(u, v, 0.00, -0.05, 0.43, -0.49);
    let head_a = segment_distance(u, v, 0.43, -0.49, 0.26, -0.45);
    let head_b = segment_distance(u, v, 0.43, -0.49, 0.39, -0.31);
    if shaft < 0.052 || head_a < 0.052 || head_b < 0.052 {
        return [0.30, 0.94, 1.0, alpha];
    }

    let globe_y = 0.23;
    let globe_r = 0.52;
    let globe_d = u.hypot(v - globe_y);
    if (globe_d - globe_r).abs() < 0.040 {
        return [0.93, 0.98, 1.0, alpha];
    }

    let yn = ((v - globe_y) / globe_r).clamp(-1.0, 1.0);
    if yn.abs() < 0.96 && u.abs() < 0.030 {
        return [0.78, 0.92, 1.0, alpha];
    }
    let meridian_span = (1.0 - yn * yn).max(0.0).sqrt();
    for bend in [-0.48f32, 0.48] {
        let x = bend * globe_r * meridian_span;
        if yn.abs() < 0.90 && (u - x).abs() < 0.026 {
            return [0.78, 0.92, 1.0, alpha];
        }
    }

    for offset in [-0.22f32, 0.22] {
        let width = (globe_r * globe_r - offset * offset).sqrt();
        if (v - (globe_y + offset)).abs() < 0.025 && u.abs() < width {
            return [0.78, 0.92, 1.0, alpha];
        }
    }

    for (nx, ny) in [(-0.50f32, 0.23f32), (0.50, 0.23), (0.0, 0.73)] {
        if (u - nx).hypot(v - ny) < 0.055 {
            return [0.30, 0.94, 1.0, alpha];
        }
    }

    color
}

fn segment_distance(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let dx = bx - ax;
    let dy = by - ay;
    let length_sq = dx * dx + dy * dy;
    if length_sq <= f32::EPSILON {
        return (px - ax).hypot(py - ay);
    }
    let t = (((px - ax) * dx + (py - ay) * dy) / length_sq).clamp(0.0, 1.0);
    (px - (ax + t * dx)).hypot(py - (ay + t * dy))
}

struct UiState {
    app: Arc<AppState>,
    engine: EngineHandle,
    theme: ThemeResources,
    cache: LabelCache,
    last_mode_active: Option<bool>,
    log_gen: u64,
    log_seq: u64,

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

    // Table groups - ListView tables for aligned data
    grp_metrics: HWND,
    lv_metrics: HWND,
    grp_bandwidth: HWND,
    lv_bandwidth: HWND,
    grp_wan: HWND,
    lv_wan: HWND,
    lbl_wan_ip: HWND,

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

unsafe fn create_listview(parent: HWND, hinst: HINSTANCE, font: HFONT) -> HWND {
    let wide = to_wide_null("SysListView32");
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE as u32,
            PCWSTR(wide.as_ptr()),
            PCWSTR(std::ptr::null()),
            (WS_CHILD as u32 | WS_VISIBLE as u32 | WS_CLIPSIBLINGS as u32 | LVS_REPORT as u32 | LVS_SHOWSELALWAYS as u32 | LVS_SINGLESEL as u32) as u32,
            0,0,10,10,
            Some(parent),
            None,
            Some(hinst),
            None,
        )
    };
    if !hwnd.0.is_null() {
        unsafe { set_font(hwnd, font) };
        // Extended styles: gridlines + fullrowselect + doublebuffer (0x00010000)
        let ex = (0x00000001u32 | 0x00000020u32 | 0x00010000u32 | 0x00000010u32) as usize; // gridlines|fullrowselect|doublebuffer|headerdragdrop
        let _ = unsafe { SendMessageW(hwnd, LVM_SETEXTENDEDLISTVIEWSTYLE as u32, WPARAM(0), LPARAM(ex as isize)) };
    }
    hwnd
}

unsafe fn lv_insert_column(hwnd: HWND, idx: i32, title: &str, width: i32, fmt: i32) {
    let mut wide = to_wide_null(title);
    let mut col = LVCOLUMNW {
        mask: (LVCF_TEXT | LVCF_WIDTH | LVCF_FMT) as u32,
        fmt,
        cx: width,
        pszText: PWSTR(wide.as_mut_ptr()),
        cchTextMax: 0,
        iSubItem: 0,
        iImage: 0,
        iOrder: 0,
        ..Default::default()
    };
    let _ = unsafe { SendMessageW(hwnd, LVM_INSERTCOLUMNW as u32, WPARAM(idx as usize), LPARAM(&mut col as *mut _ as isize)) };
}

unsafe fn lv_set_column_title(hwnd: HWND, idx: i32, title: &str) {
    let mut wide = to_wide_null(title);
    let mut col = LVCOLUMNW {
        mask: LVCF_TEXT as u32,
        pszText: PWSTR(wide.as_mut_ptr()),
        cchTextMax: wide.len() as i32,
        ..Default::default()
    };
    let _ = unsafe {
        SendMessageW(
            hwnd,
            LVM_SETCOLUMN_MSG,
            WPARAM(idx as usize),
            LPARAM(&mut col as *mut _ as isize),
        )
    };
}

unsafe fn lv_delete_column(hwnd: HWND, idx: i32) {
    let _ = unsafe {
        SendMessageW(
            hwnd,
            LVM_DELETECOLUMN_MSG,
            WPARAM(idx as usize),
            LPARAM(0),
        )
    };
}

unsafe fn lv_set_column_width(hwnd: HWND, idx: i32, width: i32) {
    let _ = unsafe {
        SendMessageW(
            hwnd,
            LVM_SETCOLUMNWIDTH_MSG,
            WPARAM(idx as usize),
            LPARAM(width.max(24) as isize),
        )
    };
}

unsafe fn lv_insert_row(hwnd: HWND, row: i32) {
    let mut empty_text = to_wide_null("");
    let mut item = LVITEMW {
        mask: LVIF_TEXT as u32,
        iItem: row,
        iSubItem: 0,
        state: 0,
        stateMask: 0,
        pszText: PWSTR(empty_text.as_mut_ptr()),
        cchTextMax: 0,
        iImage: 0,
        lParam: LPARAM(0),
        iIndent: 0,
        iGroupId: 0,
        cColumns: 0,
        puColumns: std::ptr::null_mut(),
        piColFmt: std::ptr::null_mut(),
        iGroup: 0,
    };
    let _ = unsafe { SendMessageW(hwnd, LVM_INSERTITEMW as u32, WPARAM(0), LPARAM(&mut item as *mut _ as isize)) };
}

unsafe fn lv_delete_row(hwnd: HWND, row: i32) {
    let _ = unsafe {
        SendMessageW(
            hwnd,
            LVM_DELETEITEM_MSG,
            WPARAM(row as usize),
            LPARAM(0),
        )
    };
}

unsafe fn lv_set_cell(hwnd: HWND, row: i32, col: i32, text: &str) {
    let mut wide = to_wide_null(text);
    let mut item = LVITEMW {
        mask: LVIF_TEXT as u32,
        iItem: row,
        iSubItem: col,
        state: 0,
        stateMask: 0,
        pszText: PWSTR(wide.as_mut_ptr()),
        cchTextMax: 0,
        iImage: 0,
        lParam: LPARAM(0),
        iIndent: 0,
        iGroupId: 0,
        cColumns: 0,
        puColumns: std::ptr::null_mut(),
        piColFmt: std::ptr::null_mut(),
        iGroup: 0,
    };
    let _ = unsafe { SendMessageW(hwnd, LVM_SETITEMTEXTW as u32, WPARAM(row as usize), LPARAM(&mut item as *mut _ as isize)) };
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
            set_font(self.lv_metrics, t.font_small);
            set_font(self.grp_bandwidth, t.font_bold);
            set_font(self.lv_bandwidth, t.font_small);
            set_font(self.grp_wan, t.font_bold);
            set_font(self.lbl_wan_ip, t.font_small);
            set_font(self.lv_wan, t.font_small);
            set_font(self.grp_log, t.font_bold);
            set_font(self.edit_log, t.font_log);
        }
    }

    unsafe fn layout(&mut self, width: i32, height: i32) {
        let dpi = self.theme.dpi;
        let s = |px: i32| scale_px(px, dpi);
        let lh = self.theme.line_h;
        let th = self.theme.title_h;
        let margin = s(8);
        let inner_pad = s(8);
        let gap = s(6);
        let row_gap = s(4);
        let w = (width - margin * 2).max(s(860));
        let row_h = lh + s(4);
        let small_row_h = lh + s(2);
        let edit_h = lh + s(4);
        let multi_edit_h = lh * 2 + s(8);
        let btn_h = lh + s(8);
        let mut hdwp = unsafe { BeginDeferWindowPos(64) };
        let defer = |hdwp: &mut HDWP, hwnd: HWND, x: i32, y: i32, cx: i32, cy: i32| {
            if !hdwp.0.is_null() {
                *hdwp = unsafe { DeferWindowPos(*hdwp, hwnd, None, x, y, cx.max(1), cy.max(1), (SWP_NOZORDER | SWP_NOACTIVATE) as u32) };
            } else { let _ = unsafe { MoveWindow(hwnd, x, y, cx.max(1), cy.max(1), true) }; }
        };
        let defer_bottom = |hdwp: &mut HDWP, hwnd: HWND, x: i32, y: i32, cx: i32, cy: i32| {
            if !hdwp.0.is_null() {
                *hdwp = unsafe { DeferWindowPos(*hdwp, hwnd, Some(HWND_BOTTOM), x, y, cx.max(1), cy.max(1), SWP_NOACTIVATE as u32) };
            } else { let _ = unsafe { SetWindowPos(hwnd, Some(HWND_BOTTOM), x, y, cx.max(1), cy.max(1), SWP_NOACTIVATE as u32) }; }
        };
        let mut y = s(6);
        let title_w = (w * 50) / 100;
        let status_w = w - title_w - gap;
        let title_row_h = th + s(4);
        defer(&mut hdwp, self.lbl_title, margin, y, title_w, title_row_h);
        defer(&mut hdwp, self.lbl_status, margin + title_w + gap, y + s(1), status_w, title_row_h - s(1));
        y += title_row_h + s(1);
        defer(&mut hdwp, self.lbl_mem_badge, margin, y, w, lh + s(2));
        y += lh + s(6);
        // Config
        let cfg_top = y;
        let cfg_title_offset = lh + s(6);
        let cfg_inner_w = w - inner_pad * 2;
        let cfg_x = margin + inner_pad;
        let mut cy = cfg_top + cfg_title_offset;
        let lbl_s_w = s(124); let edt_s_w = s(52); let lbl_m_w = s(142); let edt_m_w = s(64); let lbl_h_w = s(92); let edt_h_w = s(42);
        let mut rx = cfg_x;
        defer(&mut hdwp, self.lbl_streams, rx, cy, lbl_s_w, edit_h); rx += lbl_s_w + s(3);
        defer(&mut hdwp, self.edit_streams, rx, cy, edt_s_w, edit_h); rx += edt_s_w + s(8);
        defer(&mut hdwp, self.lbl_max_conns, rx, cy, lbl_m_w, edit_h); rx += lbl_m_w + s(3);
        defer(&mut hdwp, self.edit_max_conns, rx, cy, edt_m_w, edit_h); rx += edt_m_w + s(8);
        defer(&mut hdwp, self.lbl_hold_secs, rx, cy, lbl_h_w, edit_h); rx += lbl_h_w + s(3);
        defer(&mut hdwp, self.edit_hold_secs, rx, cy, edt_h_w, edit_h); rx += edt_h_w + s(8);
        let chk_w = (cfg_x + cfg_inner_w - rx).max(s(132));
        defer(&mut hdwp, self.chk_continuous, rx, cy, chk_w, edit_h);
        cy += edit_h + row_gap;
        let target_lbl_w = s(176); let target_edt_w = (cfg_inner_w - target_lbl_w - gap).max(s(220)); let target_edt_x = cfg_x + target_lbl_w + gap;
        defer(&mut hdwp, self.lbl_tcp_targets, cfg_x, cy, target_lbl_w, edit_h);
        defer(&mut hdwp, self.edit_tcp_targets, target_edt_x, cy, target_edt_w, edit_h); cy += edit_h + row_gap;
        defer(&mut hdwp, self.lbl_up_targets, cfg_x, cy, target_lbl_w, edit_h);
        defer(&mut hdwp, self.edit_up_targets, target_edt_x, cy, target_edt_w, edit_h); cy += edit_h + row_gap;
        defer(&mut hdwp, self.lbl_down_targets, cfg_x, cy, target_lbl_w, edit_h);
        defer(&mut hdwp, self.edit_down_targets, target_edt_x, cy, target_edt_w, multi_edit_h); cy += multi_edit_h + s(8);
        let cfg_h = cy - cfg_top;
        defer_bottom(&mut hdwp, self.grp_config, margin, cfg_top, w, cfg_h);
        y = cfg_top + cfg_h + s(6);
        // Buttons
        let b_gap = s(4); let avail = w - b_gap * 7; let weights = [118,118,118,128,112,82,82,82]; let total_w: i32 = weights.iter().sum();
        let btns = [self.btn_conn,self.btn_up,self.btn_down,self.btn_duplex,self.btn_stop,self.btn_reset,self.btn_clear_log,self.btn_help];
        let mut bx = margin;
        for (i, &btn) in btns.iter().enumerate() {
            let bw = if i==btns.len()-1 { margin + w - bx } else { (avail*weights[i])/total_w };
            defer(&mut hdwp, btn, bx, y, bw, btn_h); bx += bw + b_gap;
        }
        y += btn_h + s(6);
        // Metrics table: 8 rows incl header? Actually ListView header counts, so inner height = header + rows*row_h
        let metrics_top = y;
        let metrics_title_off = lh + s(6);
        let metrics_rows = 7; // 7 data rows
        let metrics_h = metrics_title_off + s(22) + metrics_rows * small_row_h + s(8);
        // s(22) approx header height
        defer_bottom(&mut hdwp, self.grp_metrics, margin, metrics_top, w, metrics_h);
        defer(&mut hdwp, self.lv_metrics, cfg_x, metrics_top + metrics_title_off, cfg_inner_w, metrics_h - metrics_title_off - s(4));
        y = metrics_top + metrics_h + s(6);
        // Bandwidth table: upload, download, duplex and ranking
        let bw_top = y;
        let bw_rows = 4;
        let bw_h = metrics_title_off + s(22) + bw_rows * small_row_h + s(8);
        defer_bottom(&mut hdwp, self.grp_bandwidth, margin, bw_top, w, bw_h);
        defer(&mut hdwp, self.lv_bandwidth, cfg_x, bw_top + metrics_title_off, cfg_inner_w, bw_h - metrics_title_off - s(4));
        y = bw_top + bw_h + s(6);
        // WAN table keeps two data rows; each detected public IP expands horizontally as a column.
        let wan_top = y;
        let wan_data_rows = 2;
        let wan_h = metrics_title_off + small_row_h + s(22) + wan_data_rows * small_row_h + s(8);
        defer_bottom(&mut hdwp, self.grp_wan, margin, wan_top, w, wan_h);
        // Inside WAN: summary label first, then the IP-per-column ListView.
        let wan_inner_y = wan_top + metrics_title_off;
        defer(&mut hdwp, self.lbl_wan_ip, cfg_x, wan_inner_y, cfg_inner_w, small_row_h);
        defer(&mut hdwp, self.lv_wan, cfg_x, wan_inner_y + small_row_h + s(2), cfg_inner_w, wan_h - metrics_title_off - small_row_h - s(6));
        y = wan_top + wan_h + s(6);
        // Log
        let log_top = y;
        let log_h = (height - log_top - s(6)).max(s(90));
        defer_bottom(&mut hdwp, self.grp_log, margin, log_top, w, log_h);
        defer(&mut hdwp, self.edit_log, cfg_x, log_top + metrics_title_off, cfg_inner_w, (log_h - metrics_title_off - s(6)).max(s(60)));
        if !hdwp.0.is_null() { let _ = unsafe { EndDeferWindowPos(hdwp) }; }
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

        // === Table-based async-safe refresh with data alignment ===
        // Gather stats once to avoid jitter from racy reads
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
        let rep_ok = stats.conn_replenish_success.load(Ordering::Relaxed);
        let rep_att = stats.conn_replenish_attempts.load(Ordering::Relaxed);
        let f_loc = stats.conn_fail_local.load(Ordering::Relaxed);
        let f_ref = stats.conn_fail_refused.load(Ordering::Relaxed);
        let f_rem = stats.conn_remote_closed.load(Ordering::Relaxed);
        let f_to = stats.conn_fail_timeout.load(Ordering::Relaxed);
        let t_healthy = stats.conn_targets_healthy.load(Ordering::Relaxed);
        let t_total = stats.conn_targets_total.load(Ordering::Relaxed);
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
        let up_rank = crate::state::SharedStats::node_ranking_summary(&stats.up_node_metrics);
        let down_rank = crate::state::SharedStats::node_ranking_summary(&stats.down_node_metrics);

        // Helper to update ListView cell with cache check (prevent jitter)
        let mut set_lv_cell = |hwnd: HWND, row: i32, col: i32, cache: &mut String, next: String| {
            if *cache != next {
                unsafe { lv_set_cell(hwnd, row, col, &next) };
                *cache = next;
            }
        };
        // Metrics table: 7 rows, 6 cols
        // Row mapping:
        // 0: 分级 瞬时峰值
        // 1: 分级 稳定并发
        // 2: 分级 长时保持
        // 3: 归属 存活
        // 4: 归属 原始
        // 5: 上行 实时
        // 6: 下行 实时
        {
            let lv = self.lv_metrics;
            // Freeze updates to prevent flicker
            let _ = unsafe { SendMessageW(lv, WM_SETREDRAW as u32, WPARAM(0), LPARAM(0)) };
            // Row 0
            set_lv_cell(lv, 0, 0, &mut self.cache.metrics_cells[0][0], "分级".to_string());
            set_lv_cell(lv, 0, 1, &mut self.cache.metrics_cells[0][1], "瞬时峰值".to_string());
            set_lv_cell(lv, 0, 2, &mut self.cache.metrics_cells[0][2], format!("{c_peak}"));
            set_lv_cell(lv, 0, 3, &mut self.cache.metrics_cells[0][3], format!("{cps_peak}/s"));
            set_lv_cell(lv, 0, 4, &mut self.cache.metrics_cells[0][4], format!("{orig_pct:.1}%"));
            set_lv_cell(lv, 0, 5, &mut self.cache.metrics_cells[0][5], format!("建连峰值{:.1}%", orig_pct));
            // Row1
            set_lv_cell(lv, 1, 0, &mut self.cache.metrics_cells[1][0], "分级".to_string());
            set_lv_cell(lv, 1, 1, &mut self.cache.metrics_cells[1][1], "稳定并发".to_string());
            set_lv_cell(lv, 1, 2, &mut self.cache.metrics_cells[1][2], format!("{c_stable}"));
            set_lv_cell(lv, 1, 3, &mut self.cache.metrics_cells[1][3], "-".to_string());
            set_lv_cell(lv, 1, 4, &mut self.cache.metrics_cells[1][4], "-".to_string());
            set_lv_cell(lv, 1, 5, &mut self.cache.metrics_cells[1][5], format!("达标上限 {}", c_stable));
            // Row2
            set_lv_cell(lv, 2, 0, &mut self.cache.metrics_cells[2][0], "分级".to_string());
            set_lv_cell(lv, 2, 1, &mut self.cache.metrics_cells[2][1], "长时保持".to_string());
            set_lv_cell(lv, 2, 2, &mut self.cache.metrics_cells[2][2], format!("{c_hold}"));
            set_lv_cell(lv, 2, 3, &mut self.cache.metrics_cells[2][3], format!("持续{hold_secs}s"));
            set_lv_cell(lv, 2, 4, &mut self.cache.metrics_cells[2][4], if holding { "保持中" } else { "-" }.to_string());
            set_lv_cell(lv, 2, 5, &mut self.cache.metrics_cells[2][5], format!("保持均值{c_hold}"));
            // Row3
            set_lv_cell(lv, 3, 0, &mut self.cache.metrics_cells[3][0], "归属".to_string());
            set_lv_cell(lv, 3, 1, &mut self.cache.metrics_cells[3][1], "存活/目标".to_string());
            set_lv_cell(lv, 3, 2, &mut self.cache.metrics_cells[3][2], format!("{c_cur}"));
            set_lv_cell(lv, 3, 3, &mut self.cache.metrics_cells[3][3], format!("{c_tgt}"));
            set_lv_cell(lv, 3, 4, &mut self.cache.metrics_cells[3][4], format!("{c_cps}/s"));
            set_lv_cell(lv, 3, 5, &mut self.cache.metrics_cells[3][5], format!("健康{t_healthy}/{t_total} RTT{c_rtt}ms"));
            // Row4
            set_lv_cell(lv, 4, 0, &mut self.cache.metrics_cells[4][0], "归属".to_string());
            set_lv_cell(lv, 4, 1, &mut self.cache.metrics_cells[4][1], "原始成功".to_string());
            set_lv_cell(lv, 4, 2, &mut self.cache.metrics_cells[4][2], format!("{orig_ok}"));
            set_lv_cell(lv, 4, 3, &mut self.cache.metrics_cells[4][3], format!("{orig_att}"));
            set_lv_cell(lv, 4, 4, &mut self.cache.metrics_cells[4][4], format!("{orig_pct:.1}%"));
            set_lv_cell(lv, 4, 5, &mut self.cache.metrics_cells[4][5], format!("补建{rep_ok}/{rep_att} 失败本机{f_loc} 远拒{f_ref} 远闭{f_rem} 超时{f_to}"));
            // Row5 上行
            set_lv_cell(lv, 5, 0, &mut self.cache.metrics_cells[5][0], "上行".to_string());
            set_lv_cell(lv, 5, 1, &mut self.cache.metrics_cells[5][1], "近1秒".to_string());
            set_lv_cell(lv, 5, 2, &mut self.cache.metrics_cells[5][2], format_mbps(up_bps));
            set_lv_cell(lv, 5, 3, &mut self.cache.metrics_cells[5][3], format_mbps(up_peak));
            set_lv_cell(lv, 5, 4, &mut self.cache.metrics_cells[5][4], format!("{up_fail:.1}%/{}err", up_err));
            set_lv_cell(lv, 5, 5, &mut self.cache.metrics_cells[5][5], format!("并发{up_cur_c}(最佳{up_best_c}) 流{up_streams} 平均{} P95{}", format_mbps(up_avg), format_mbps(up_p95)));
            // Row6 下行
            set_lv_cell(lv, 6, 0, &mut self.cache.metrics_cells[6][0], "下行".to_string());
            set_lv_cell(lv, 6, 1, &mut self.cache.metrics_cells[6][1], "近1秒".to_string());
            set_lv_cell(lv, 6, 2, &mut self.cache.metrics_cells[6][2], format_mbps(down_bps));
            set_lv_cell(lv, 6, 3, &mut self.cache.metrics_cells[6][3], format_mbps(down_peak));
            set_lv_cell(lv, 6, 4, &mut self.cache.metrics_cells[6][4], format!("{down_fail:.1}%/{}err", down_err));
            set_lv_cell(lv, 6, 5, &mut self.cache.metrics_cells[6][5], format!("并发{down_cur_c}(最佳{down_best_c}) 流{down_streams} 平均{} P95{}", format_mbps(down_avg), format_mbps(down_p95)));
            let _ = unsafe { SendMessageW(lv, WM_SETREDRAW as u32, WPARAM(1), LPARAM(0)) };
        }
        // Bandwidth table: application-layer rolling rate and effective totals.
        {
            let lv = self.lv_bandwidth;
            let _ = unsafe { SendMessageW(lv, WM_SETREDRAW as u32, WPARAM(0), LPARAM(0)) };
            // Upload current: body bytes yielded to the HTTP client in the latest ~1s window.
            set_lv_cell(lv, 0, 0, &mut self.cache.bandwidth_cells[0][0], "上行".to_string());
            set_lv_cell(lv, 0, 1, &mut self.cache.bandwidth_cells[0][1], format_mbps(up_bps));
            set_lv_cell(lv, 0, 2, &mut self.cache.bandwidth_cells[0][2], format_mbps(up_peak));
            set_lv_cell(lv, 0, 3, &mut self.cache.bandwidth_cells[0][3], format!("{} / P95 {}", format_mbps(up_avg), format_mbps(up_p95)));
            set_lv_cell(lv, 0, 4, &mut self.cache.bandwidth_cells[0][4], format_bytes(up_total));
            set_lv_cell(lv, 0, 5, &mut self.cache.bandwidth_cells[0][5], format!("实时/峰值为请求体供给；均值/P95/累计仅计完整2xx · 延迟{up_lat}ms"));

            // Download current: actual response bytes read in the latest ~1s window.
            set_lv_cell(lv, 1, 0, &mut self.cache.bandwidth_cells[1][0], "下行".to_string());
            set_lv_cell(lv, 1, 1, &mut self.cache.bandwidth_cells[1][1], format_mbps(down_bps));
            set_lv_cell(lv, 1, 2, &mut self.cache.bandwidth_cells[1][2], format_mbps(down_peak));
            set_lv_cell(lv, 1, 3, &mut self.cache.bandwidth_cells[1][3], format!("{} / P95 {}", format_mbps(down_avg), format_mbps(down_p95)));
            set_lv_cell(lv, 1, 4, &mut self.cache.bandwidth_cells[1][4], format_bytes(down_total));
            set_lv_cell(lv, 1, 5, &mut self.cache.bandwidth_cells[1][5], format!("实际读取 · TCP{tcp_ms} TLS{tls_ms} TTFB{ttfb_ms}"));

            // Duplex summary.
            let duplex_val = if mode == AppMode::DuplexTesting {
                format_mbps(up_bps.saturating_add(down_bps))
            } else {
                "-".to_string()
            };
            set_lv_cell(lv, 2, 0, &mut self.cache.bandwidth_cells[2][0], "双向".to_string());
            set_lv_cell(lv, 2, 1, &mut self.cache.bandwidth_cells[2][1], duplex_val);
            set_lv_cell(lv, 2, 2, &mut self.cache.bandwidth_cells[2][2], format_mbps(up_peak.saturating_add(down_peak)));
            set_lv_cell(lv, 2, 3, &mut self.cache.bandwidth_cells[2][3], format_mbps(up_avg.saturating_add(down_avg)));
            set_lv_cell(lv, 2, 4, &mut self.cache.bandwidth_cells[2][4], format!("上{} 下{}", format_bytes(up_total), format_bytes(down_total)));
            set_lv_cell(lv, 2, 5, &mut self.cache.bandwidth_cells[2][5], if mode == AppMode::DuplexTesting { "并发压测中" } else { "空闲" }.to_string());

            // Best nodes.
            set_lv_cell(lv, 3, 0, &mut self.cache.bandwidth_cells[3][0], "节点".to_string());
            set_lv_cell(lv, 3, 1, &mut self.cache.bandwidth_cells[3][1], up_rank.clone());
            set_lv_cell(lv, 3, 2, &mut self.cache.bandwidth_cells[3][2], down_rank.clone());
            set_lv_cell(lv, 3, 3, &mut self.cache.bandwidth_cells[3][3], "-".to_string());
            set_lv_cell(lv, 3, 4, &mut self.cache.bandwidth_cells[3][4], "-".to_string());
            set_lv_cell(lv, 3, 5, &mut self.cache.bandwidth_cells[3][5], format!("上行 {} · 下行 {}", up_rank, down_rank));
            let _ = unsafe { SendMessageW(lv, WM_SETREDRAW as u32, WPARAM(1), LPARAM(0)) };
        }
        // Group external reflector responses by their observed public egress IP.
        let wan_age = {
            let last = stats.egress_ip_last_update_ms.load(Ordering::Relaxed);
            if last == 0 {
                "尚未成功探测".to_string()
            } else {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let secs = now_ms.saturating_sub(last) / 1000;
                if secs < 120 { format!("{secs}s 前更新") } else { format!("{}m 前更新", secs / 60) }
            }
        };

        let wan_rows = stats.egress_ip_rows();
        let probe_hits: usize = wan_rows.iter().map(|row| row.hits).sum();
        let wan_summary = format!(
            "发现 {} 个不同公网 IP · {} 次服务响应 · {}",
            wan_rows.len(),
            probe_hits,
            wan_age
        );
        unsafe { set_cached_text(self.lbl_wan_ip, &mut self.cache.wan_ip, wan_summary) };

        {
            let lv = self.lv_wan;
            let _ = unsafe { SendMessageW(lv, WM_SETREDRAW as u32, WPARAM(0), LPARAM(0)) };

            // The first column labels the metric. Every public IP gets its own dynamic column.
            let desired_columns = wan_rows.len().max(1) + 1;
            while self.cache.wan_columns.len() < desired_columns {
                let col_idx = self.cache.wan_columns.len();
                let title = if wan_rows.is_empty() && col_idx == 1 {
                    "等待探测"
                } else {
                    "公网 IP"
                };
                unsafe {
                    lv_insert_column(lv, col_idx as i32, title, scale_px(220, self.theme.dpi), LVCFMT_CENTER as i32)
                };
                self.cache.wan_columns.push(title.to_string());
            }
            while self.cache.wan_columns.len() > desired_columns {
                let col_idx = self.cache.wan_columns.len() - 1;
                unsafe { lv_delete_column(lv, col_idx as i32) };
                self.cache.wan_columns.pop();
            }

            let mut titles = Vec::with_capacity(desired_columns);
            titles.push("项目".to_string());
            if wan_rows.is_empty() {
                titles.push("等待探测".to_string());
            } else {
                titles.extend(wan_rows.iter().map(|row| row.ip.clone()));
            }
            for (col_idx, title) in titles.iter().enumerate() {
                if self.cache.wan_columns[col_idx] != *title {
                    unsafe { lv_set_column_title(lv, col_idx as i32, title) };
                    self.cache.wan_columns[col_idx] = title.clone();
                }
            }

            // Exactly two fixed metric rows: hit count and provider sources.
            while self.cache.wan_cells.len() < 2 {
                let row_idx = self.cache.wan_cells.len();
                unsafe { lv_insert_row(lv, row_idx as i32) };
                self.cache.wan_cells.push(vec![String::new(); desired_columns]);
            }
            while self.cache.wan_cells.len() > 2 {
                let row_idx = self.cache.wan_cells.len() - 1;
                unsafe { lv_delete_row(lv, row_idx as i32) };
                self.cache.wan_cells.pop();
            }
            for row_cache in &mut self.cache.wan_cells {
                row_cache.resize(desired_columns, String::new());
            }

            let labels = ["命中服务", "探测来源"];
            for (row_idx, label) in labels.iter().enumerate() {
                set_lv_cell(
                    lv,
                    row_idx as i32,
                    0,
                    &mut self.cache.wan_cells[row_idx][0],
                    (*label).to_string(),
                );
            }

            if wan_rows.is_empty() {
                set_lv_cell(lv, 0, 1, &mut self.cache.wan_cells[0][1], "-".to_string());
                set_lv_cell(
                    lv,
                    1,
                    1,
                    &mut self.cache.wan_cells[1][1],
                    "等待公网 IP 查询服务响应".to_string(),
                );
            } else {
                for (idx, row) in wan_rows.iter().enumerate() {
                    let col_idx = idx + 1;
                    set_lv_cell(
                        lv,
                        0,
                        col_idx as i32,
                        &mut self.cache.wan_cells[0][col_idx],
                        format!("{} 个服务", row.hits),
                    );
                    set_lv_cell(
                        lv,
                        1,
                        col_idx as i32,
                        &mut self.cache.wan_cells[1][col_idx],
                        row.providers.join(" · "),
                    );
                }
            }

            // Keep a useful minimum width per IP; extra columns become horizontally scrollable.
            let mut client_rect = RECT::default();
            let _ = unsafe { GetClientRect(lv, &mut client_rect) };
            let client_width = (client_rect.right - client_rect.left).max(0);
            let label_width = (client_width * 16 / 100)
                .clamp(scale_px(88, self.theme.dpi), scale_px(140, self.theme.dpi));
            let ip_column_count = desired_columns.saturating_sub(1).max(1) as i32;
            let ip_width = ((client_width - label_width) / ip_column_count)
                .max(scale_px(220, self.theme.dpi));
            unsafe {
                lv_set_column_width(lv, 0, label_width);
                for col_idx in 1..desired_columns {
                    lv_set_column_width(lv, col_idx as i32, ip_width);
                }
            }

            let _ = unsafe { SendMessageW(lv, WM_SETREDRAW as u32, WPARAM(1), LPARAM(0)) };
            let _ = unsafe { InvalidateRect(Some(lv), None, true) };
        }

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
        // Init ListView common controls for table alignment (prevent freeze via double buffer)
        unsafe {
            let mut icc = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES as u32 };
            InitCommonControlsEx(&icc);
        }

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

        let init_w = scale_px(980, sys_dpi).min((screen_w * 92) / 100);
        let init_h = scale_px(900, sys_dpi).min((screen_h * 92) / 100);

        let init_ctx = Box::new((state, engine));
        let raw_ctx = Box::into_raw(init_ctx);

        let hwnd = CreateWindowExW(
            0,
            class_name,
            w!("WinNetStress - 纯内存网络并发与带宽压力测试工具"),
            (WS_OVERLAPPEDWINDOW) as u32,
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

            // Metrics ListView (created before group for Z-order)
            let lv_metrics = unsafe { create_listview(hwnd, hinst, theme.font_small) };
            // Setup columns: 项目 | 当前 | 峰值/目标 | 成功率 | 详情 | 备注
            unsafe {
                let total_w = 860;
                lv_insert_column(lv_metrics, 0, "类别", total_w*12/100, LVCFMT_LEFT as i32);
                lv_insert_column(lv_metrics, 1, "指标", total_w*15/100, LVCFMT_LEFT as i32);
                lv_insert_column(lv_metrics, 2, "当前", total_w*13/100, LVCFMT_RIGHT as i32);
                lv_insert_column(lv_metrics, 3, "峰值/目标", total_w*16/100, LVCFMT_RIGHT as i32);
                lv_insert_column(lv_metrics, 4, "成功率/速率", total_w*14/100, LVCFMT_RIGHT as i32);
                lv_insert_column(lv_metrics, 5, "详情", total_w*30/100, LVCFMT_LEFT as i32);
                for r in 0..7 { lv_insert_row(lv_metrics, r); }
            }
            let grp_metrics = unsafe {
                create_ctrl(w!("BUTTON"), " 实时测试指标 (表格·自动对齐) ", grp_style, 0, hwnd, 0, hinst, theme.font_bold)
            };

            let lv_bandwidth = unsafe { create_listview(hwnd, hinst, theme.font_small) };
            unsafe {
                let total_w = 860;
                lv_insert_column(lv_bandwidth, 0, "类型", total_w*12/100, LVCFMT_LEFT as i32);
                lv_insert_column(lv_bandwidth, 1, "近1秒", total_w*15/100, LVCFMT_RIGHT as i32);
                lv_insert_column(lv_bandwidth, 2, "峰值", total_w*13/100, LVCFMT_RIGHT as i32);
                lv_insert_column(lv_bandwidth, 3, "平均/P95", total_w*16/100, LVCFMT_RIGHT as i32);
                lv_insert_column(lv_bandwidth, 4, "累计/流量", total_w*18/100, LVCFMT_LEFT as i32);
                lv_insert_column(lv_bandwidth, 5, "测量口径", total_w*26/100, LVCFMT_LEFT as i32);
                for r in 0..4 { lv_insert_row(lv_bandwidth, r); }
            }
            let grp_bandwidth = unsafe {
                create_ctrl(w!("BUTTON"), " 宽带详情（近1秒速率） ", grp_style, 0, hwnd, 0, hinst, theme.font_bold)
            };

            let lbl_wan_ip = unsafe { create_ctrl(w!("STATIC"), "公网出口 IP 探测中...", st_left, 0, hwnd, 0, hinst, theme.font_small) };
            let lv_wan = unsafe { create_listview(hwnd, hinst, theme.font_small) };
            unsafe {
                let total_w = 860;
                lv_insert_column(lv_wan, 0, "项目", total_w*16/100, LVCFMT_LEFT as i32);
                lv_insert_column(lv_wan, 1, "等待探测", total_w*84/100, LVCFMT_CENTER as i32);
                lv_insert_row(lv_wan, 0);
                lv_insert_row(lv_wan, 1);
            }
            let grp_wan = unsafe { create_ctrl(w!("BUTTON"), " 多 WAN 公网出口 IP（一个 IP 一列，可横向滚动） ", grp_style, 0, hwnd, 0, hinst, theme.font_bold) };

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
                app, engine, theme,
                cache: LabelCache {
                    wan_columns: vec!["项目".to_string(), "等待探测".to_string()],
                    wan_cells: vec![vec![String::new(); 2]; 2],
                    ..LabelCache::default()
                },
                last_mode_active: None, log_gen: 0, log_seq: 0,
                lbl_title, lbl_status, lbl_mem_badge,
                grp_config, lbl_streams, edit_streams, lbl_max_conns, edit_max_conns, lbl_hold_secs, edit_hold_secs, chk_continuous,
                lbl_tcp_targets, edit_tcp_targets, lbl_up_targets, edit_up_targets, lbl_down_targets, edit_down_targets,
                btn_conn, btn_up, btn_down, btn_duplex, btn_stop, btn_reset, btn_clear_log, btn_help,
                grp_metrics, lv_metrics,
                grp_bandwidth, lv_bandwidth,
                grp_wan, lv_wan, lbl_wan_ip,
                grp_log, edit_log,
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
                } else if ctrl == ui.lbl_mem_badge {
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
                    // WAN columns expand/collapse inside the ListView; panel layout stays fixed.
                    unsafe { ui.refresh_metrics_and_logs() };
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
                            "纯内存运行：测试配置、统计和日志仅保存在进程内存。\n\n",
                            "实时速率：按最近约 1 秒应用层字节增量计算。上传实时值是请求体交给 HTTP 客户端的速率，累计有效量只计完整请求返回 HTTP 2xx；下行按实际读取字节计算。\n\n",
                            "多 WAN：并行查询多个公网 IP 服务，每个不同公网出口 IP 单独占一列，动态增减列并显示命中服务；列数超出窗口宽度时可横向滚动。支持 IPv4/IPv6。探测结果用于识别出口 IP，不代表物理 WAN 口或带宽占比。\n\n",
                            "仅对自己拥有或已获授权的目标进行连接和带宽测试。"
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
