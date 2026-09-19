//! Liquid-glass overlay for incoming transfers (Windows only).
//! A borderless topmost, per-pixel-alpha layered window in the bottom-right:
//! folder icon slides in from the left -> file icon follows -> live %
//! -> at 100% the file slides into the folder -> both fade/slide away.
//! Rendered in software (premultiplied BGRA via UpdateLayeredWindow) with
//! Win32 GDI text on top — no external dependencies.
//! Non-Windows builds get silent no-ops.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

// ---- shared animation state ----
static BEGIN_MS: AtomicU64 = AtomicU64::new(0); // 0 = hidden/idle
static FINISH_MS: AtomicU64 = AtomicU64::new(0); // 0 = transfer not done yet
static PENDING: AtomicU64 = AtomicU64::new(0); // set on begin, cleared when shown
static SENT: AtomicU64 = AtomicU64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(1);
static FILENAME: Mutex<String> = Mutex::new(String::new());

pub fn begin(filename: &str, total: u64) {
    TOTAL.store(total.max(1), Ordering::Relaxed);
    SENT.store(0, Ordering::Relaxed);
    FINISH_MS.store(0, Ordering::Relaxed);
    *FILENAME.lock().unwrap() = filename.to_string();
    PENDING.store(1, Ordering::Relaxed);
    show();
}

pub fn progress(sent: u64, total: u64) {
    SENT.store(sent, Ordering::Relaxed);
    TOTAL.store(total.max(1), Ordering::Relaxed);
}

pub fn finish() {
    FINISH_MS.store(now_ms(), Ordering::Relaxed);
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(not(windows))]
pub fn show() {}

#[cfg(windows)]
pub fn show() {
    win::ensure_window();
}

#[cfg(windows)]
static SIDE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0); // 0=right 1=left

/// "left" | "right" — which screen edge the panel lives on / slides from
pub fn set_side(position: &str) {
    #[cfg(windows)]
    SIDE.store(if position == "left" { 1 } else { 0 }, Ordering::Relaxed);
    #[cfg(not(windows))]
    let _ = position;
}

// ============================ Windows implementation ============================
#[cfg(windows)]
mod win {
    use super::*;
    use std::sync::atomic::AtomicIsize;

    static HWND: AtomicIsize = AtomicIsize::new(0);
    static THREAD: std::sync::Once = std::sync::Once::new();
    static RENDER_LOG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    type HWND = isize;
    type HDC = isize;
    type HGDIOBJ = isize;
    type HFONT = isize;
    type HINSTANCE = isize;
    type BOOL = i32;
    type UINT = u32;
    type COLORREF = u32;
    type LPCWSTR = *const u16;
    type WNDPROC = Option<unsafe extern "system" fn(HWND, UINT, usize, isize) -> isize>;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct POINT {
        x: i32,
        y: i32,
    }
    #[repr(C)]
    struct SIZE {
        cx: i32,
        cy: i32,
    }
    #[repr(C)]
    struct RECT {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[repr(C)]
    struct BLENDFUNCTION {
        blend_op: u8,
        blend_flags: u8,
        source_constant_alpha: u8,
        alpha_format: u8,
    }
    #[repr(C)]
    struct MSG {
        hwnd: HWND,
        message: UINT,
        w_param: usize,
        l_param: isize,
        time: u32,
        pt: POINT,
    }
    #[repr(C)]
    struct BITMAPINFOHEADER {
        bi_size: u32,
        bi_width: i32,
        bi_height: i32,
        bi_planes: u16,
        bi_bit_count: u16,
        bi_compression: u32,
        bi_size_image: u32,
        bi_x_pels_per_meter: i32,
        bi_y_pels_per_meter: i32,
        bi_clr_used: u32,
        bi_clr_important: u32,
    }
    #[repr(C)]
    struct BITMAPINFO {
        bmi_header: BITMAPINFOHEADER,
        bmi_colors: [u32; 1],
    }
    #[repr(C)]
    struct WNDCLASSW {
        style: UINT,
        lpfn_wnd_proc: WNDPROC,
        cb_cls_extra: i32,
        cb_wnd_extra: i32,
        h_instance: HINSTANCE,
        h_icon: HINSTANCE,
        h_cursor: HCURSOR,
        h_br_background: HBRUSH,
        lpsz_menu_name: LPCWSTR,
        lpsz_class_name: LPCWSTR,
    }
    type HCURSOR = isize;
    type HBRUSH = isize;

    extern "system" {
        fn GetModuleHandleW(name: LPCWSTR) -> HINSTANCE;
        fn RegisterClassW(class: *const WNDCLASSW) -> u16;
        fn CreateWindowExW(
            ex_style: u32,
            class: LPCWSTR,
            title: LPCWSTR,
            style: u32,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            parent: HWND,
            menu: isize,
            instance: HINSTANCE,
            param: *mut std::ffi::c_void,
        ) -> HWND;
        fn DefWindowProcW(hwnd: HWND, msg: UINT, w: usize, l: isize) -> isize;
        fn ShowWindow(hwnd: HWND, cmd: i32) -> BOOL;
        fn SetWindowPos(
            hwnd: HWND,
            after: HWND,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            flags: UINT,
        ) -> BOOL;
        fn GetSystemMetrics(index: i32) -> i32;
        fn SetTimer(hwnd: HWND, id: usize, ms: u32, proc: isize) -> usize;
        fn PostMessageW(hwnd: HWND, msg: UINT, w: usize, l: isize) -> BOOL;
        fn GetMessageW(msg: *mut MSG, hwnd: HWND, min: UINT, max: UINT) -> i32;
        fn TranslateMessage(msg: *const MSG) -> BOOL;
        fn DispatchMessageW(msg: *const MSG) -> isize;
        fn PostQuitMessage(code: i32);
        fn UpdateLayeredWindow(
            hwnd: HWND,
            dst: HDC,
            dst_pos: *const POINT,
            size: *const SIZE,
            src: HDC,
            src_pos: *const POINT,
            key: COLORREF,
            blend: *const BLENDFUNCTION,
            flags: u32,
        ) -> BOOL;
        fn CreateDIBSection(
            dc: HDC,
            info: *const BITMAPINFO,
            usage: u32,
            bits: *mut *mut std::ffi::c_void,
            section: isize,
            offset: u32,
        ) -> HGDIOBJ;
        fn GetDC(hwnd: HWND) -> HDC;
        fn ReleaseDC(hwnd: HWND, dc: HDC) -> i32;
        fn CreateCompatibleDC(dc: HDC) -> HDC;
        fn SelectObject(dc: HDC, obj: HGDIOBJ) -> HGDIOBJ;
        fn DeleteObject(obj: HGDIOBJ) -> BOOL;
        fn DeleteDC(dc: HDC) -> BOOL;
        fn SetProcessDPIAware() -> BOOL;
        fn SetTextColor(dc: HDC, color: COLORREF) -> COLORREF;
        fn SetBkMode(dc: HDC, mode: i32) -> i32;
        fn CreateFontW(
            height: i32,
            width: i32,
            escapement: i32,
            orientation: i32,
            weight: i32,
            italic: u32,
            underline: u32,
            strikeout: u32,
            charset: u32,
            out_precision: u32,
            clip_precision: u32,
            quality: u32,
            pitch_family: u32,
            face: LPCWSTR,
        ) -> HFONT;
        fn DrawTextW(dc: HDC, text: LPCWSTR, len: i32, rect: *mut RECT, format: UINT) -> i32;
        fn PatBlt(dc: HDC, x: i32, y: i32, w: i32, h: i32, rop: u32) -> BOOL;
        fn GetLastError() -> u32;
        fn GetCurrentProcessId() -> u32;
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> BOOL;
    }

    const WS_POPUP: u32 = 0x8000_0000;
    const WS_EX_TOPMOST: u32 = 0x8;
    const WS_EX_TOOLWINDOW: u32 = 0x80;
    const WS_EX_LAYERED: u32 = 0x8_0000;
    const WS_EX_NOACTIVATE: u32 = 0x800_0000;
    const SW_HIDE: i32 = 0;
    const SW_SHOWNOACTIVATE: i32 = 4;
    const WM_CREATE: UINT = 1;
    const WM_TIMER: UINT = 0x113;
    const WM_DESTROY: UINT = 2;
    const WM_APP_SHOW: UINT = 0x8000;
    const ULW_ALPHA: u32 = 2;
    const AC_SRC_OVER: u8 = 0;
    const AC_SRC_ALPHA: u8 = 1;
    const BLACKNESS: u32 = 0x42;
    const TRANSPARENT: i32 = 1;
    const DT_CENTER: UINT = 1;
    const DT_VCENTER: UINT = 4;
    const DT_SINGLELINE: UINT = 0x20;
    const DT_END_ELLIPSIS: UINT = 0x8000;
    const SM_CXSCREEN: i32 = 0;
    const SM_CYSCREEN: i32 = 1;

    // ---- layout (physical px, DPI-aware) ----
    const MARGIN: i32 = 30; // room for shadow / glow / slide overshoot
    const PANEL_W: i32 = 250;
    const PANEL_H: i32 = 152;
    const WIN_W: i32 = PANEL_W + MARGIN * 2;
    const WIN_H: i32 = PANEL_H + MARGIN * 2;
    const RADIUS: f64 = 26.0;

    // timeline (ms)
    const FOLDER_IN: f64 = 450.0;
    const FILE_IN: f64 = 450.0; // starts at FOLDER_IN
    const SWALLOW: f64 = 350.0;
    const EXIT: f64 = 450.0;

    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: UINT, w: usize, l: isize) -> isize {
        match msg {
            WM_CREATE => {
                SetTimer(hwnd, 1, 16, 0);
                println!("[overlay] window ready");
                0
            }
            WM_TIMER => {
                // self-healing show: pick up a pending begin no matter how
                // the request arrived (no reliance on posted messages)
                if PENDING.swap(0, Ordering::Relaxed) == 1 {
                    FINISH_MS.store(0, Ordering::Relaxed);
                    BEGIN_MS.store(super::now_ms(), Ordering::Relaxed);
                    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    SetWindowPos(hwnd, -1, 0, 0, 0, 0, 0x1 | 0x2 | 0x10);
                    println!("[overlay] animating");
                }
                let begin = BEGIN_MS.load(Ordering::Relaxed);
                let fin = FINISH_MS.load(Ordering::Relaxed);
                if begin != 0 && fin != 0 {
                    let t = (super::now_ms().saturating_sub(begin)) as f64;
                    let s = (super::now_ms().saturating_sub(fin)) as f64;
                    if t >= FOLDER_IN + FILE_IN && s >= SWALLOW + EXIT {
                        BEGIN_MS.store(0, Ordering::Relaxed);
                        FINISH_MS.store(0, Ordering::Relaxed);
                        ShowWindow(hwnd, SW_HIDE);
                        println!("[overlay] hidden — ready for next transfer");
                    }
                }
                render(hwnd, super::now_ms());
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }

    fn ease_out(p: f64) -> f64 {
        let p = p.clamp(0.0, 1.0);
        1.0 - (1.0 - p).powi(3)
    }
    fn ease_back(p: f64) -> f64 {
        // slight overshoot — the "liquid" pop
        let p = p.clamp(0.0, 1.0);
        let c1 = 1.70158;
        let c3 = c1 + 1.0;
        1.0 + c3 * (p - 1.0).powi(3) + c1 * (p - 1.0).powi(2)
    }

    struct Frame {
        // premultiplied BGRA
        px: Vec<u32>,
    }

    impl Frame {
        fn new() -> Self {
            Frame {
                px: vec![0; (WIN_W * WIN_H) as usize],
            }
        }
        #[inline]
        fn blend(&mut self, x: i32, y: i32, r: u8, g: u8, b: u8, a: u8) {
            if x < 0 || y < 0 || x >= WIN_W || y >= WIN_H || a == 0 {
                return;
            }
            let i = (y as usize) * WIN_W as usize + x as usize;
            let p = &mut self.px[i];
            let old_b = (*p & 0xFF) as u32;
            let old_g = ((*p >> 8) & 0xFF) as u32;
            let old_r = ((*p >> 16) & 0xFF) as u32;
            let old_a = (*p >> 24) as u32;
            let na = a as u32;
            let nb = b as u32 * na / 255;
            let ng = g as u32 * na / 255;
            let nr = r as u32 * na / 255;
            let out_a = na + old_a * (255 - na) / 255;
            // premultiplied store: value already scaled by its own alpha
            let pb = nb + old_b * (255 - na) / 255;
            let pg = ng + old_g * (255 - na) / 255;
            let pr = nr + old_r * (255 - na) / 255;
            *p = (out_a << 24) | (pb.min(255) << 16) | (pg.min(255) << 8) | pr.min(255);
        }
    }

    /// Render one frame into the layered window.
    unsafe fn render(hwnd: HWND, now: u64) {
        let begin = BEGIN_MS.load(Ordering::Relaxed);
        if begin == 0 {
            return;
        }
        let fin = FINISH_MS.load(Ordering::Relaxed);
        let t = (now.saturating_sub(begin)) as f64;

        // ---- timeline (side-aware: enters from its screen edge) ----
        let dir = if SIDE.load(Ordering::Relaxed) == 1 {
            -1.0
        } else {
            1.0
        };
        let mut folder_off = if t < FOLDER_IN {
            dir * (1.0 - ease_back(t / FOLDER_IN)) * (WIN_W as f64)
        } else {
            0.0
        };
        let mut file_off = if t < FOLDER_IN {
            dir * (WIN_W as f64)
        } else if t < FOLDER_IN + FILE_IN {
            dir * (1.0 - ease_back((t - FOLDER_IN) / FILE_IN)) * (WIN_W as f64)
        } else {
            0.0
        };
        let mut file_scale = 1.0;
        let mut global_alpha = 1.0f64;
        if fin != 0 && t >= FOLDER_IN + FILE_IN {
            let s = (now.saturating_sub(fin)) as f64;
            if s < SWALLOW {
                let e = ease_out(s / SWALLOW);
                file_off = e * 96.0; // into the folder
                file_scale = 1.0 - 0.45 * e;
            } else {
                let e = ease_out(((s - SWALLOW) / EXIT).min(1.0));
                file_off = 96.0 + dir * e * 420.0;
                folder_off = dir * e * 420.0;
                global_alpha = 1.0 - e;
            }
        }
        let done = fin != 0 && t >= FOLDER_IN + FILE_IN;

        let sent = SENT.load(Ordering::Relaxed);
        let total = TOTAL.load(Ordering::Relaxed);
        let pct = ((sent as f64 / total as f64) * 100.0)
            .round()
            .clamp(0.0, 100.0);

        // ---- pixel buffer ----
        let mut frame = Frame::new();
        let px0 = MARGIN as f64 + folder_off;
        let py0 = MARGIN as f64;

        // soft drop shadow around the panel
        {
            let spread = 26.0;
            for y in (py0 as i32 - 30)..(py0 as i32 + PANEL_H + 30) {
                for x in (px0 as i32 - 30)..(px0 as i32 + PANEL_W + 30) {
                    let d = sdf_rr(
                        x as f64 + 0.5,
                        y as f64 + 0.5,
                        px0,
                        py0,
                        PANEL_W as f64,
                        PANEL_H as f64,
                        RADIUS,
                    );
                    if d > 0.0 && d < spread {
                        let a = (1.0 - d / spread) * 90.0 * global_alpha;
                        if a >= 1.0 {
                            frame.blend(x, y, 5, 8, 20, a as u8);
                        }
                    }
                }
            }
        }

        // glass panel: vertical gradient with alpha (the "liquid" body)
        {
            let top = (44.0, 52.0, 76.0, 186.0);
            let bot = (14.0, 16.0, 28.0, 208.0);
            for y in (py0 as i32)..(py0 as i32 + PANEL_H) {
                let ty = (y as f64 - py0) / PANEL_H as f64;
                let r = top.0 + (bot.0 - top.0) * ty;
                let g = top.1 + (bot.1 - top.1) * ty;
                let b = top.2 + (bot.2 - top.2) * ty;
                let a = top.3 + (bot.3 - top.3) * ty;
                let sheen = if ty < 0.30 {
                    (0.30 - ty) / 0.30 * 16.0
                } else {
                    0.0
                };
                for x in (px0 as i32)..(px0 as i32 + PANEL_W) {
                    let d = sdf_rr(
                        x as f64 + 0.5,
                        y as f64 + 0.5,
                        px0,
                        py0,
                        PANEL_W as f64,
                        PANEL_H as f64,
                        RADIUS,
                    );
                    if d <= 0.0 {
                        frame.blend(
                            x,
                            y,
                            (r + sheen) as u8,
                            (g + sheen) as u8,
                            (b + sheen + 8.0) as u8,
                            a as u8,
                        );
                    } else if d < 1.5 {
                        // AA edge
                        let cov = (1.0 - d / 1.5) * a;
                        frame.blend(x, y, r as u8, g as u8, b as u8, cov as u8);
                    }
                }
            }
            // border light
            for y in (py0 as i32)..(py0 as i32 + PANEL_H) {
                for x in (px0 as i32)..(px0 as i32 + PANEL_W) {
                    let d = (sdf_rr(
                        x as f64 + 0.5,
                        y as f64 + 0.5,
                        px0,
                        py0,
                        PANEL_W as f64,
                        PANEL_H as f64,
                        RADIUS,
                    ))
                    .abs();
                    if d < 1.0 {
                        frame.blend(x, y, 190, 205, 255, (120.0 * global_alpha) as u8);
                    }
                }
            }
        }

        // folder icon (amber, liquid gradient) — right side
        {
            let fx = px0 + PANEL_W as f64 - 116.0 + folder_off;
            let fy = py0 + 38.0;
            for y in (fy as i32 - 4)..(fy as i32 + 62) {
                for x in (fx as i32 - 4)..(fx as i32 + 92) {
                    let xf = x as f64 + 0.5 - fx;
                    let yf = y as f64 + 0.5 - fy;
                    // tab + body
                    let in_tab = xf >= 8.0 && xf <= 36.0 && yf >= 0.0 && yf <= 12.0;
                    let in_body = in_round(xf, yf, 0.0, 8.0, 86.0, 52.0, 10.0);
                    if in_tab || in_body {
                        let ty = ((yf - 8.0) / 52.0).clamp(0.0, 1.0);
                        let r = 255.0 - 18.0 * ty;
                        let g = 172.0 - 34.0 * ty;
                        let b = 30.0 - 10.0 * ty;
                        // glass highlight on the top half
                        let hl = if yf < 26.0 {
                            26.0 * (1.0 - yf / 26.0)
                        } else {
                            0.0
                        };
                        frame.blend(
                            x,
                            y,
                            (r + hl) as u8,
                            (g + hl) as u8,
                            (b + hl) as u8,
                            (255.0 * global_alpha) as u8,
                        );
                    }
                }
            }
        }

        // file icon (white sheet, folded corner) — left side, slides into folder
        {
            let icon_x = px0 + 30.0 + file_off;
            let icon_y = py0 + 32.0;
            let w0 = 56.0 * file_scale;
            let h0 = 70.0 * file_scale;
            let fold = 15.0 * file_scale;
            for y in (icon_y as i32 - 2)..((icon_y + h0) as i32 + 3) {
                for x in (icon_x as i32 - 2)..((icon_x + w0) as i32 + 3) {
                    let xf = x as f64 + 0.5 - icon_x;
                    let yf = y as f64 + 0.5 - icon_y;
                    let cut = w0 - fold;
                    let in_sheet =
                        xf >= 0.0 && xf <= w0 && yf >= 0.0 && yf <= h0 && !(xf > cut && yf < fold);
                    if in_sheet {
                        let shade = 246.0 - (yf / h0) * 10.0;
                        frame.blend(
                            x,
                            y,
                            shade as u8,
                            (shade + 2.0) as u8,
                            (shade + 8.0).min(255.0) as u8,
                            (240.0 * global_alpha) as u8,
                        );
                    }
                    let in_fold = xf > cut && yf < fold && (xf - cut) + (fold - yf) <= fold;
                    if in_fold {
                        frame.blend(x, y, 176, 182, 200, (235.0 * global_alpha) as u8);
                    }
                }
            }
        }

        // progress bar
        {
            let bx = px0 + 22.0;
            let by = py0 + PANEL_H as f64 - 40.0;
            let bw = (PANEL_W - 44) as f64;
            let bh = 9.0;
            for y in (by as i32 - 2)..(by as i32 + bh as i32 + 3) {
                for x in (bx as i32 - 2)..(bx as i32 + bw as i32 + 3) {
                    let xf = x as f64 + 0.5 - bx;
                    let yf = y as f64 + 0.5 - by;
                    if in_round(xf, yf, 0.0, 0.0, bw, bh, bh / 2.0) {
                        let fill_w = bw * (pct as f64 / 100.0);
                        if xf <= fill_w {
                            let tt = (xf / bw).clamp(0.0, 1.0);
                            if done {
                                frame.blend(x, y, 52, 219, 122, (255.0 * global_alpha) as u8);
                            } else {
                                let r = 10.0 + (100.0 - 10.0) * tt;
                                let g = 132.0 + (210.0 - 132.0) * tt;
                                let b = 255.0;
                                frame.blend(
                                    x,
                                    y,
                                    r as u8,
                                    g as u8,
                                    b as u8,
                                    (255.0 * global_alpha) as u8,
                                );
                            }
                        } else {
                            frame.blend(x, y, 255, 255, 255, (42.0 * global_alpha) as u8);
                        }
                    }
                }
            }
        }

        // ---- push pixels to the DIB ----
        let hdc_screen = GetDC(0);
        let mem = CreateCompatibleDC(hdc_screen);
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmi_header.bi_size = 40;
        bmi.bmi_header.bi_width = WIN_W;
        bmi.bmi_header.bi_height = -WIN_H; // top-down
        bmi.bmi_header.bi_planes = 1;
        bmi.bmi_header.bi_bit_count = 32;
        bmi.bmi_header.bi_compression = 0;
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(mem, &bmi, 0, &mut bits, 0, 0);
        let old_bmp = SelectObject(mem, bmp);
        let buf = std::slice::from_raw_parts_mut(bits as *mut u32, (WIN_W * WIN_H) as usize);
        buf.copy_from_slice(&frame.px);

        // ---- text via GDI on a scratch copy of the buffer, then alpha-fix ----
        draw_texts(buf);

        // push to screen FIRST — destroying the DC/bitmap before this was
        // exactly the earlier invisible-window bug
        // middle-left of the screen
        let dst = POINT {
            x: 20,
            y: (GetSystemMetrics(SM_CYSCREEN) - WIN_H) / 2,
        };
        let size = SIZE {
            cx: WIN_W,
            cy: WIN_H,
        };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            blend_op: AC_SRC_OVER,
            blend_flags: 0,
            source_constant_alpha: 255,
            alpha_format: AC_SRC_ALPHA,
        };
        let ok = UpdateLayeredWindow(
            hwnd, hdc_screen, &dst, &size, mem, &src, 0, &blend, ULW_ALPHA,
        );
        if !RENDER_LOG.swap(true, Ordering::Relaxed) {
            let gle = GetLastError();
            println!("[overlay] ulw ok={} gle={}", ok, gle);
        }
        SelectObject(mem, old_bmp);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(0, hdc_screen);
    }

    /// GDI text: rendered white-on-black on a scratch copy of the frame,
    /// then glyph coverage (luminance) is blended back as real alpha.
    unsafe fn draw_texts(buf: &mut [u32]) {
        let hdc_screen = GetDC(0);
        let mem = CreateCompatibleDC(hdc_screen);
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmi_header.bi_size = 40;
        bmi.bmi_header.bi_width = WIN_W;
        bmi.bmi_header.bi_height = -WIN_H;
        bmi.bmi_header.bi_planes = 1;
        bmi.bmi_header.bi_bit_count = 32;
        bmi.bmi_header.bi_compression = 0;
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(mem, &bmi, 0, &mut bits, 0, 0);
        let old_bmp = SelectObject(mem, bmp);
        // black background (luminance 0 = no glyph)
        PatBlt(mem, 0, 0, WIN_W, WIN_H, BLACKNESS);
        SetBkMode(mem, TRANSPARENT);
        SetTextColor(mem, 0x00FF_FF_FF);

        let name = FILENAME.lock().unwrap().clone();
        let title_font: HFONT = CreateFontW(
            -17,
            0,
            0,
            0,
            600,
            0,
            0,
            0,
            1,
            0,
            0,
            4,
            0,
            wide("Segoe UI").as_ptr(),
        );
        let old_font = SelectObject(mem, title_font);
        let wname = wide(&name);
        let mut tr = RECT {
            left: MARGIN + 10,
            top: MARGIN + 12,
            right: MARGIN + PANEL_W - 10,
            bottom: MARGIN + 40,
        };
        DrawTextW(
            mem,
            wname.as_ptr(),
            (wname.len() - 1) as i32,
            &mut tr,
            DT_CENTER | DT_END_ELLIPSIS,
        );

        let done = FINISH_MS.load(Ordering::Relaxed) != 0;
        let sent = SENT.load(Ordering::Relaxed);
        let total = TOTAL.load(Ordering::Relaxed);
        let pct = ((sent as f64 / total as f64) * 100.0)
            .round()
            .clamp(0.0, 100.0) as i64;
        let label = if done {
            "Done ✓".to_string()
        } else {
            format!("{}%", pct)
        };
        let pct_font: HFONT = CreateFontW(
            -34,
            0,
            0,
            0,
            700,
            0,
            0,
            0,
            1,
            0,
            0,
            4,
            0,
            wide("Segoe UI").as_ptr(),
        );
        SelectObject(mem, pct_font);
        let wl = wide(&label);
        let mut pr = RECT {
            left: MARGIN,
            top: MARGIN + PANEL_H - 40,
            right: MARGIN + PANEL_W,
            bottom: MARGIN + PANEL_H,
        };
        DrawTextW(
            mem,
            wl.as_ptr(),
            (wl.len() - 1) as i32,
            &mut pr,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );

        SelectObject(mem, old_font);
        DeleteObject(title_font);
        DeleteObject(pct_font);

        // blend glyph coverage back (white text, slight glow color)
        let scr = std::slice::from_raw_parts(bits as *const u32, (WIN_W * WIN_H) as usize);
        for i in 0..buf.len() {
            let s = scr[i];
            let lum = ((s & 0xFF) + ((s >> 8) & 0xFF) + ((s >> 16) & 0xFF)) / 3;
            if lum > 0 {
                let c = lum as u32; // coverage
                let b = buf[i] & 0xFF;
                let g = (buf[i] >> 8) & 0xFF;
                let r = (buf[i] >> 16) & 0xFF;
                let a = buf[i] >> 24;
                // "over": white text (premult = lum) onto existing premult pixel
                let nb = c + b * (255 - c) / 255;
                let ng = c + g * (255 - c) / 255;
                let nr = c + r * (255 - c) / 255;
                let na = c + a * (255 - c) / 255;
                buf[i] = (na << 24) | (nb << 16) | (ng << 8) | nr;
            }
        }

        SelectObject(mem, old_bmp);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(0, hdc_screen);
    }

    #[inline]
    fn in_round(px: f64, py: f64, x0: f64, y0: f64, w: f64, h: f64, rad: f64) -> bool {
        let cx0 = x0 + rad;
        let cx1 = x0 + w - rad;
        let cy0 = y0 + rad;
        let cy1 = y0 + h - rad;
        if px < x0 || px > x0 + w || py < y0 || py > y0 + h {
            return false;
        }
        let ncx = if px < cx0 {
            cx0
        } else if px > cx1 {
            cx1
        } else {
            px
        };
        let ncy = if py < cy0 {
            cy0
        } else if py > cy1 {
            cy1
        } else {
            py
        };
        (px - ncx).hypot(py - ncy) <= rad
    }

    fn sdf_rr(px: f64, py: f64, x0: f64, y0: f64, w: f64, h: f64, rad: f64) -> f64 {
        let cx = x0 + w / 2.0;
        let cy = y0 + h / 2.0;
        let dx = (px - cx).abs() - (w / 2.0 - rad);
        let dy = (py - cy).abs() - (h / 2.0 - rad);
        let ox = dx.max(0.0);
        let oy = dy.max(0.0);
        let outside = ox.hypot(oy);
        let inside = dx.max(dy).min(0.0);
        outside + inside
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Spawn the GUI thread once; window starts hidden.
    pub fn ensure_window() {
        THREAD.call_once(|| {
            std::thread::spawn(|| unsafe {
                SetProcessDPIAware();
                let class_name = wide("WhisperDropOverlay");
                let class = WNDCLASSW {
                    style: 0,
                    lpfn_wnd_proc: Some(wnd_proc),
                    cb_cls_extra: 0,
                    cb_wnd_extra: 0,
                    h_instance: GetModuleHandleW(std::ptr::null()),
                    h_icon: 0,
                    h_cursor: 0,
                    h_br_background: 0,
                    lpsz_menu_name: std::ptr::null(),
                    lpsz_class_name: class_name.as_ptr(),
                };
                RegisterClassW(&class);
                let sx = GetSystemMetrics(0);
                let sy = GetSystemMetrics(1);
                let hwnd = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
                    class_name.as_ptr(),
                    wide("").as_ptr(),
                    WS_POPUP,
                    20,
                    (sy - WIN_H) / 2,
                    WIN_W,
                    WIN_H,
                    0,
                    0,
                    GetModuleHandleW(std::ptr::null()),
                    std::ptr::null_mut(),
                );
                let pid = GetCurrentProcessId();
                let mut sid: u32 = 0;
                ProcessIdToSessionId(pid, &mut sid);
                println!(
                    "[overlay] ready hwnd={} screen={}x{} win={}x{} session={}",
                    hwnd, sx, sy, WIN_W, WIN_H, sid
                );
                ShowWindow(hwnd, SW_HIDE);

                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, 0, 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            });
        });
    }
}
