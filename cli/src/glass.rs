//! Software "glass" renderer for the Windows drop-zone UI.
//!
//! Draws into a premultiplied-BGRA buffer that is pushed to a per-pixel
//! alpha layered window with `UpdateLayeredWindow`. Text is rasterised by
//! GDI (white on black) and its luminance used as coverage, so glyphs are
//! anti-aliased over transparency without any external dependency.
//! Colours and metrics mirror `src/edge.html` so the widget looks the same
//! on macOS and Windows.

#![cfg(windows)]
#![allow(non_snake_case, clippy::too_many_arguments)]

pub type Rgba = (u8, u8, u8, u8);

// ---- design tokens (edge.html :root) ----
pub const ACCENT_A: Rgba = (59, 130, 246, 255);
pub const ACCENT_B: Rgba = (139, 92, 246, 255);
pub const GLASS: Rgba = (16, 18, 26, 214); // frost is drawn in; ~.84 alpha
pub const GLASS_SOLID: Rgba = (14, 16, 24, 230);
pub const LINE: Rgba = (255, 255, 255, 26);
pub const TEXT: Rgba = (242, 244, 248, 255);
pub const MUTED: Rgba = (255, 255, 255, 140);
pub const OK: Rgba = (52, 211, 153, 255);
pub const BAD: Rgba = (251, 113, 133, 255);
pub const RADIUS: f64 = 20.0;

/// Avatar hues shared with edge.html (`HUES`).
pub const HUES: [(u8, u8, u8); 8] = [
    (59, 130, 246),
    (139, 92, 246),
    (236, 72, 153),
    (245, 158, 11),
    (16, 185, 129),
    (6, 182, 212),
    (249, 115, 22),
    (99, 102, 241),
];

pub fn hue_for(name: &str) -> (u8, u8, u8) {
    let mut h: u32 = 0;
    for c in name.chars() {
        h = h.wrapping_mul(31).wrapping_add(c as u32);
    }
    HUES[(h % HUES.len() as u32) as usize]
}

pub fn monogram(name: &str) -> String {
    let parts: Vec<&str> = name
        .split(|c: char| c.is_whitespace() || c == '-' || c == '_' || c == '.')
        .filter(|p| !p.is_empty())
        .collect();
    let s: String = if parts.len() >= 2 {
        parts[0].chars().take(1).chain(parts[1].chars().take(1)).collect()
    } else {
        name.chars().take(2).collect()
    };
    s.to_uppercase()
}

pub fn lerp(a: Rgba, b: Rgba, t: f64) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2), m(a.3, b.3))
}

pub fn with_alpha(c: Rgba, a: u8) -> Rgba {
    (c.0, c.1, c.2, a)
}

/// Signed distance to a rounded rectangle (negative inside).
pub fn sdf_rr(px: f64, py: f64, x0: f64, y0: f64, w: f64, h: f64, rad: f64) -> f64 {
    let rad = rad.min(w / 2.0).min(h / 2.0);
    let cx = x0 + w / 2.0;
    let cy = y0 + h / 2.0;
    let dx = (px - cx).abs() - (w / 2.0 - rad);
    let dy = (py - cy).abs() - (h / 2.0 - rad);
    let ox = dx.max(0.0);
    let oy = dy.max(0.0);
    ox.hypot(oy) + dx.max(dy).min(0.0)
}

pub struct Canvas {
    pub w: i32,
    pub h: i32,
    /// premultiplied BGRA, top-down
    pub px: Vec<u32>,
}

impl Canvas {
    pub fn new(w: i32, h: i32) -> Self {
        Canvas {
            w,
            h,
            px: vec![0; (w.max(1) * h.max(1)) as usize],
        }
    }

    #[inline]
    pub fn blend(&mut self, x: i32, y: i32, c: Rgba) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h || c.3 == 0 {
            return;
        }
        let i = (y as usize) * self.w as usize + x as usize;
        let p = &mut self.px[i];
        let ob = *p & 0xFF;
        let og = (*p >> 8) & 0xFF;
        let or = (*p >> 16) & 0xFF;
        let oa = *p >> 24;
        let na = c.3 as u32;
        let inv = 255 - na;
        let nb = c.2 as u32 * na / 255 + ob * inv / 255;
        let ng = c.1 as u32 * na / 255 + og * inv / 255;
        let nr = c.0 as u32 * na / 255 + or * inv / 255;
        let a = na + oa * inv / 255;
        *p = (a.min(255) << 24) | (nr.min(255) << 16) | (ng.min(255) << 8) | nb.min(255);
    }

    /// Cheapest possible "hit-test only" fill: alpha 1 keeps the layered
    /// window clickable/droppable without showing anything.
    pub fn hit_area(&mut self, x: i32, y: i32, w: i32, h: i32) {
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                let i = (yy as usize) * self.w as usize + xx as usize;
                if self.px[i] >> 24 == 0 {
                    self.px[i] = 1 << 24;
                }
            }
        }
    }

    /// Anti-aliased rounded rectangle, vertical gradient `top`→`bottom`,
    /// alpha scaled by `opacity`.
    pub fn rrect(&mut self, x: f64, y: f64, w: f64, h: f64, rad: f64, top: Rgba, bottom: Rgba, opacity: f64) {
        let (x0, y0) = ((x - 1.0).floor() as i32, (y - 1.0).floor() as i32);
        let (x1, y1) = ((x + w + 1.0).ceil() as i32, (y + h + 1.0).ceil() as i32);
        for yy in y0.max(0)..y1.min(self.h) {
            let t = ((yy as f64 + 0.5 - y) / h).clamp(0.0, 1.0);
            let c = lerp(top, bottom, t);
            for xx in x0.max(0)..x1.min(self.w) {
                let d = sdf_rr(xx as f64 + 0.5, yy as f64 + 0.5, x, y, w, h, rad);
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let a = (c.3 as f64 * cov * opacity).round() as u8;
                    self.blend(xx, yy, with_alpha(c, a));
                }
            }
        }
    }

    /// 1px inner stroke of a rounded rectangle.
    pub fn rrect_stroke(&mut self, x: f64, y: f64, w: f64, h: f64, rad: f64, c: Rgba, opacity: f64) {
        let (x0, y0) = ((x - 1.0).floor() as i32, (y - 1.0).floor() as i32);
        let (x1, y1) = ((x + w + 1.0).ceil() as i32, (y + h + 1.0).ceil() as i32);
        for yy in y0.max(0)..y1.min(self.h) {
            for xx in x0.max(0)..x1.min(self.w) {
                let d = sdf_rr(xx as f64 + 0.5, yy as f64 + 0.5, x, y, w, h, rad);
                let cov = (1.0 - (d + 0.5).abs()).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let a = (c.3 as f64 * cov * opacity).round() as u8;
                    self.blend(xx, yy, with_alpha(c, a));
                }
            }
        }
    }

    /// Soft outer shadow / glow around a rounded rectangle.
    pub fn glow(&mut self, x: f64, y: f64, w: f64, h: f64, rad: f64, spread: f64, c: Rgba, opacity: f64) {
        let s = spread.ceil() as i32 + 1;
        let (x0, y0) = (x.floor() as i32 - s, y.floor() as i32 - s);
        let (x1, y1) = ((x + w).ceil() as i32 + s, (y + h).ceil() as i32 + s);
        for yy in y0.max(0)..y1.min(self.h) {
            for xx in x0.max(0)..x1.min(self.w) {
                let d = sdf_rr(xx as f64 + 0.5, yy as f64 + 0.5, x, y, w, h, rad);
                if d > 0.0 && d < spread {
                    let f = 1.0 - d / spread;
                    let a = (c.3 as f64 * f * f * opacity).round() as u8;
                    self.blend(xx, yy, with_alpha(c, a));
                }
            }
        }
    }

    /// Anti-aliased circle outline (for the "drop" ring).
    pub fn ring(&mut self, cx: f64, cy: f64, r: f64, thick: f64, c: Rgba, dash: Option<(f64, f64, f64)>) {
        let s = (r + thick + 1.0).ceil() as i32;
        for yy in (cy as i32 - s).max(0)..(cy as i32 + s).min(self.h) {
            for xx in (cx as i32 - s).max(0)..(cx as i32 + s).min(self.w) {
                let (dx, dy) = (xx as f64 + 0.5 - cx, yy as f64 + 0.5 - cy);
                let d = (dx.hypot(dy) - r).abs() - thick / 2.0;
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                if let Some((on, period, offset)) = dash {
                    let ang = dy.atan2(dx).rem_euclid(std::f64::consts::TAU);
                    if (ang * r + offset).rem_euclid(period) > on {
                        continue;
                    }
                }
                self.blend(xx, yy, with_alpha(c, (c.3 as f64 * cov) as u8));
            }
        }
    }

    /// Anti-aliased line segment with round caps.
    pub fn line(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, thick: f64, c: Rgba) {
        let (minx, maxx) = (x0.min(x1) - thick, x0.max(x1) + thick);
        let (miny, maxy) = (y0.min(y1) - thick, y0.max(y1) + thick);
        let (vx, vy) = (x1 - x0, y1 - y0);
        let len2 = vx * vx + vy * vy;
        for yy in (miny.floor() as i32).max(0)..(maxy.ceil() as i32).min(self.h) {
            for xx in (minx.floor() as i32).max(0)..(maxx.ceil() as i32).min(self.w) {
                let (px, py) = (xx as f64 + 0.5, yy as f64 + 0.5);
                let t = if len2 > 0.0 { (((px - x0) * vx + (py - y0) * vy) / len2).clamp(0.0, 1.0) } else { 0.0 };
                let d = (px - (x0 + vx * t)).hypot(py - (y0 + vy * t)) - thick / 2.0;
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(xx, yy, with_alpha(c, (c.3 as f64 * cov) as u8));
                }
            }
        }
    }

    /// Horizontal progress bar; `frac` in 0..=1, `None` = indeterminate.
    pub fn bar(&mut self, x: f64, y: f64, w: f64, h: f64, frac: Option<f64>, fill_a: Rgba, fill_b: Rgba, phase: f64) {
        self.rrect(x, y, w, h, h / 2.0, (255, 255, 255, 30), (255, 255, 255, 30), 1.0);
        match frac {
            Some(f) => {
                let fw = (w * f.clamp(0.0, 1.0)).max(if f > 0.0 { h } else { 0.0 });
                if fw > 0.0 {
                    self.rrect(x, y, fw, h, h / 2.0, fill_a, fill_b, 1.0);
                }
            }
            None => {
                let seg = w * 0.4;
                let off = (phase % 1.0) * (w + seg) - seg;
                let (sx, ex) = ((x + off).max(x), (x + off + seg).min(x + w));
                if ex > sx {
                    self.rrect(sx, y, ex - sx, h, h / 2.0, fill_a, fill_b, 1.0);
                }
            }
        }
    }

    /// Composite `text` (GDI-rasterised) into the canvas.
    pub fn text(&mut self, t: &TextRun) {
        unsafe { gdi::draw_text(self, t) }
    }
}

pub const ALIGN_LEFT: u32 = 0;
pub const ALIGN_CENTER: u32 = 1;
pub const ALIGN_RIGHT: u32 = 2;

pub struct TextRun<'a> {
    pub text: &'a str,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub size: i32,
    pub weight: i32,
    pub color: Rgba,
    pub align: u32,
    pub ellipsis: bool,
}

impl<'a> TextRun<'a> {
    pub fn new(text: &'a str, x: i32, y: i32, w: i32, h: i32, size: i32) -> Self {
        TextRun { text, x, y, w, h, size, weight: 400, color: TEXT, align: ALIGN_LEFT, ellipsis: true }
    }
    pub fn bold(mut self, weight: i32) -> Self { self.weight = weight; self }
    pub fn color(mut self, c: Rgba) -> Self { self.color = c; self }
    pub fn align(mut self, a: u32) -> Self { self.align = a; self }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Win32 plumbing: DIB creation, GDI text, layered-window presentation.
pub mod gdi {
    use super::*;

    pub type HWND = isize;
    pub type HDC = isize;
    pub type HGDIOBJ = isize;
    pub type BOOL = i32;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct POINT { pub x: i32, pub y: i32 }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct SIZE { pub cx: i32, pub cy: i32 }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct RECT { pub left: i32, pub top: i32, pub right: i32, pub bottom: i32 }
    #[repr(C)]
    struct BLENDFUNCTION { op: u8, flags: u8, alpha: u8, format: u8 }
    #[repr(C)]
    struct BITMAPINFOHEADER {
        size: u32, width: i32, height: i32, planes: u16, bit_count: u16, compression: u32,
        size_image: u32, xppm: i32, yppm: i32, clr_used: u32, clr_important: u32,
    }
    #[repr(C)]
    struct BITMAPINFO { header: BITMAPINFOHEADER, colors: [u32; 1] }

    #[link(name = "user32")]
    extern "system" {
        fn GetDC(hwnd: HWND) -> HDC;
        fn ReleaseDC(hwnd: HWND, dc: HDC) -> i32;
        fn UpdateLayeredWindow(hwnd: HWND, dst: HDC, dst_pos: *const POINT, size: *const SIZE, src: HDC,
            src_pos: *const POINT, key: u32, blend: *const BLENDFUNCTION, flags: u32) -> BOOL;
        fn DrawTextW(dc: HDC, text: *const u16, len: i32, rect: *mut RECT, format: u32) -> i32;
    }
    #[link(name = "gdi32")]
    extern "system" {
        fn CreateCompatibleDC(dc: HDC) -> HDC;
        fn CreateDIBSection(dc: HDC, info: *const BITMAPINFO, usage: u32, bits: *mut *mut std::ffi::c_void, section: isize, offset: u32) -> HGDIOBJ;
        fn SelectObject(dc: HDC, obj: HGDIOBJ) -> HGDIOBJ;
        fn DeleteObject(obj: HGDIOBJ) -> BOOL;
        fn DeleteDC(dc: HDC) -> BOOL;
        fn SetTextColor(dc: HDC, color: u32) -> u32;
        fn SetBkMode(dc: HDC, mode: i32) -> i32;
        fn PatBlt(dc: HDC, x: i32, y: i32, w: i32, h: i32, rop: u32) -> BOOL;
        fn CreateFontW(height: i32, width: i32, esc: i32, orient: i32, weight: i32, italic: u32, underline: u32,
            strike: u32, charset: u32, out_prec: u32, clip_prec: u32, quality: u32, pitch: u32, face: *const u16) -> HGDIOBJ;
    }

    const DT_LEFT: u32 = 0;
    const DT_CENTER: u32 = 1;
    const DT_RIGHT: u32 = 2;
    const DT_VCENTER: u32 = 4;
    const DT_SINGLELINE: u32 = 0x20;
    const DT_NOPREFIX: u32 = 0x800;
    const DT_END_ELLIPSIS: u32 = 0x8000;
    const BLACKNESS: u32 = 0x42;
    const CLEARTYPE_QUALITY: u32 = 5;

    struct Dib { dc: HDC, bmp: HGDIOBJ, old: HGDIOBJ, bits: *mut u32, screen: HDC }

    unsafe fn dib(w: i32, h: i32) -> Dib {
        let screen = GetDC(0);
        let dc = CreateCompatibleDC(screen);
        let info = BITMAPINFO {
            header: BITMAPINFOHEADER {
                size: 40, width: w, height: -h, planes: 1, bit_count: 32, compression: 0,
                size_image: 0, xppm: 0, yppm: 0, clr_used: 0, clr_important: 0,
            },
            colors: [0],
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(dc, &info, 0, &mut bits, 0, 0);
        let old = SelectObject(dc, bmp);
        Dib { dc, bmp, old, bits: bits as *mut u32, screen }
    }

    unsafe fn free(d: Dib) {
        SelectObject(d.dc, d.old);
        DeleteObject(d.bmp);
        DeleteDC(d.dc);
        ReleaseDC(0, d.screen);
    }

    /// Rasterise one text run and blend it into the canvas.
    pub unsafe fn draw_text(canvas: &mut Canvas, t: &TextRun) {
        if t.text.is_empty() || t.w <= 0 || t.h <= 0 {
            return;
        }
        let (w, h) = (canvas.w, canvas.h);
        let d = dib(w, h);
        if d.bits.is_null() {
            free(d);
            return;
        }
        PatBlt(d.dc, 0, 0, w, h, BLACKNESS);
        SetBkMode(d.dc, 1);
        SetTextColor(d.dc, 0x00FF_FFFF);
        let font = CreateFontW(-t.size, 0, 0, 0, t.weight, 0, 0, 0, 1, 0, 0, CLEARTYPE_QUALITY, 0, wide("Segoe UI").as_ptr());
        let old_font = SelectObject(d.dc, font);
        let mut rc = RECT { left: t.x, top: t.y, right: t.x + t.w, bottom: t.y + t.h };
        let mut fmt = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
        fmt |= match t.align { ALIGN_CENTER => DT_CENTER, ALIGN_RIGHT => DT_RIGHT, _ => DT_LEFT };
        if t.ellipsis {
            fmt |= DT_END_ELLIPSIS;
        }
        let wt = wide(t.text);
        DrawTextW(d.dc, wt.as_ptr(), -1, &mut rc, fmt);
        SelectObject(d.dc, old_font);
        DeleteObject(font);

        let src = std::slice::from_raw_parts(d.bits, (w * h) as usize);
        let (y0, y1) = (t.y.max(0), (t.y + t.h).min(h));
        let (x0, x1) = (t.x.max(0), (t.x + t.w).min(w));
        for y in y0..y1 {
            for x in x0..x1 {
                let p = src[(y * w + x) as usize];
                // grayscale coverage (ClearType colour fringes averaged out)
                let lum = ((p & 0xFF) + ((p >> 8) & 0xFF) + ((p >> 16) & 0xFF)) / 3;
                if lum > 0 {
                    let a = (lum * t.color.3 as u32 / 255) as u8;
                    canvas.blend(x, y, with_alpha(t.color, a));
                }
            }
        }
        free(d);
    }

    /// Push a canvas to a layered window at screen position (x, y).
    pub unsafe fn present(hwnd: HWND, canvas: &Canvas, x: i32, y: i32) -> bool {
        let d = dib(canvas.w, canvas.h);
        if d.bits.is_null() {
            free(d);
            return false;
        }
        std::slice::from_raw_parts_mut(d.bits, canvas.px.len()).copy_from_slice(&canvas.px);
        let dst = POINT { x, y };
        let size = SIZE { cx: canvas.w, cy: canvas.h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION { op: 0, flags: 0, alpha: 255, format: 1 };
        let ok = UpdateLayeredWindow(hwnd, d.screen, &dst, &size, d.dc, &src, 0, &blend, 2);
        free(d);
        ok != 0
    }
}
